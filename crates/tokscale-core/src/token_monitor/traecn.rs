//! TRAE CN (Trae Code) session parser.
//!
//! TRAE CN persists one append-only JSONL event stream per task under
//! `~/.trae-cn/hub_event_cache/TraeCode/<install-id>/<task-id>.jsonl`.
//! Each line is `{ task_id, seq, type, data: <json-string> }`.
//!
//! Key design points:
//!
//! - `token_usage` carries authoritative per-turn token counts but no
//!   timestamp. Nearby events do (`metadata.created_at / chat_start_time`,
//!   `plan_item.timing.*_at_ms`, `timing_events.data.data`,
//!   `context_usage.updated_at`). Events are appended in order, so every
//!   `token_usage` inherits the clock of the nearest timestamped event
//!   before it; leading rows are back-filled from the first timestamped
//!   event seen. Fallback to file mtime only when the whole file carries
//!   no timestamps at all.
//! - Model identity is the **user's selection**, not the routed backend:
//!   `metadata.model_smart_selection_meta.mode == "auto"` → bucket under
//!   `"auto"`; otherwise prefer
//!   `metadata.user_message_context.model_info.display_model_name`. The
//!   routed backend (`model_config.model_name`, e.g.
//!   `deepseek-v4.1-flash__dev`) is ignored so an auto session mid-swap
//!   cannot silently change its model attribution.
//! - TRAE CN bills in credits, not dollars, so every message carries
//!   `cost = 0.0`.

use super::UnifiedMessage;
use crate::TokenBreakdown;
use serde_json::Value;
use std::io::Read;
use std::path::Path;

/// Maximum bytes read off disk for one task event file.
const MAX_EVENT_FILE_BYTES: usize = 64 * 1024 * 1024;

/// Epoch seconds threshold: values below this are seconds; above are ms.
const SECOND_MS_SPLIT: i64 = 1_000_000_000_000;

fn normalize_epoch_ms(value: &Value) -> Option<i64> {
    let n = value.as_i64()?;
    if n <= 0 {
        return None;
    }
    if n >= SECOND_MS_SPLIT {
        Some(n)
    } else if n >= 1_000_000_000 {
        Some(n * 1000)
    } else {
        None
    }
}

fn max_positive(values: impl Iterator<Item = Option<i64>>) -> i64 {
    values.flatten().max().unwrap_or(0)
}

/// Extract timestamp (epoch ms) from a single event's parsed `data`.
/// Returns 0 when the event carries no clock.
fn event_timestamp_ms(event_type: &str, data: &Value) -> i64 {
    match event_type {
        "metadata" => max_positive(
            [
                data.pointer("/created_at").and_then(normalize_epoch_ms),
                data.pointer("/chat_start_time")
                    .and_then(normalize_epoch_ms),
            ]
            .into_iter(),
        ),
        "context_usage" => data
            .pointer("/updated_at")
            .and_then(normalize_epoch_ms)
            .unwrap_or(0),
        "plan_item" => {
            let Some(timing) = data.pointer("/timing") else {
                return 0;
            };
            let mut best = 0i64;
            if let Some(v) = timing
                .pointer("/generated_at_ms")
                .and_then(normalize_epoch_ms)
            {
                best = v;
            }
            if let Some(v) = timing
                .pointer("/tool_call_started_at_ms")
                .and_then(normalize_epoch_ms)
            {
                best = best.max(v);
            }
            best
        }
        "timing_events" => {
            let Some(stages) = data.pointer("/data") else {
                return 0;
            };
            let mut best = 0i64;
            if let Some(obj) = stages.as_object() {
                for v in obj.values() {
                    if let Some(ts) = normalize_epoch_ms(v) {
                        if ts > best {
                            best = ts;
                        }
                    }
                }
            }
            best
        }
        _ => 0,
    }
}

fn read_file_head(path: &Path) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let len = metadata.len();
    if len > MAX_EVENT_FILE_BYTES as u64 {
        return None;
    }
    let mut buf = Vec::with_capacity(len as usize);
    file.read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn parse_event(line: &str) -> Option<(String, String, Value)> {
    let outer: Value = serde_json::from_str(line).ok()?;
    let event_type = outer
        .pointer("/type")
        .and_then(Value::as_str)
        .map(String::from)?;
    let data_raw = outer.pointer("/data")?;
    let data = match data_raw {
        Value::String(s) => serde_json::from_str(s).ok()?,
        other => other.clone(),
    };
    let task_id = outer
        .pointer("/task_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some((event_type, task_id, data))
}

pub fn parse_traecn_file(path: &Path) -> Vec<UnifiedMessage> {
    let Some(bytes) = read_file_head(path) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);

    // Phase 1: parse every event and collect the file-wide first clock. We
    // can't forward-fill on the first pass because an early `token_usage`
    // may arrive before any timestamped event, and its resolved clock
    // would be 0 with no way to retroactively backfill it. Two passes
    // cost nothing on a file that tops out at ~64 MiB.
    let mut events: Vec<(String, String, Value)> = Vec::new();
    let mut first_clock: i64 = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Some((event_type, task_id, data)) = parse_event(line) else {
            continue;
        };
        let event_ts = event_timestamp_ms(&event_type, &data);
        if event_ts > 0 && first_clock == 0 {
            first_clock = event_ts;
        }
        events.push((event_type, task_id, data));
    }

    if events.is_empty() {
        return Vec::new();
    }

    // Phase 2: second pass — forward-fill timestamps with a known
    // `first_clock` so leading token_usage rows inherit it.
    let mut events_with_ts: Vec<(String, String, Value, i64)> = Vec::with_capacity(events.len());
    let mut clock = first_clock;
    for (event_type, task_id, data) in events {
        let event_ts = event_timestamp_ms(&event_type, &data);
        if event_ts > 0 {
            clock = clock.max(event_ts);
        }
        events_with_ts.push((event_type, task_id, data, clock));
    }
    let events = events_with_ts;

    // Phase 2: derive session-level state from metadata, then emit per-turn
    // messages for token_usage events using the (possibly forward-filled)
    // clock attached to each row.
    let mut smart_mode = String::new();
    let mut selected_model = String::new();
    let mut agent_name: Option<String> = None;
    let mut session_title: Option<String> = None;

    for (event_type, _, data, _) in &events {
        if event_type != "metadata" {
            continue;
        }
        if smart_mode.is_empty() {
            smart_mode = data
                .pointer("/model_smart_selection_meta/mode")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if smart_mode.is_empty() {
                smart_mode = data
                    .pointer(
                        "/user_message_context/model_info/persist_meta/smart_selection/strategy",
                    )
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase();
            }
        }
        if selected_model.is_empty() {
            let info = data.pointer("/user_message_context/model_info");
            if let Some(info) = info {
                let picked = info
                    .pointer("/display_model_name")
                    .and_then(Value::as_str)
                    .or_else(|| info.pointer("/model_name").and_then(Value::as_str))
                    .or_else(|| info.pointer("/config_name").and_then(Value::as_str))
                    .unwrap_or("")
                    .trim();
                if !picked.is_empty() {
                    selected_model = picked.to_string();
                }
            }
        }
        if agent_name.is_none() {
            agent_name = data
                .pointer("/agent_name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(String::from);
        }
        if session_title.is_none() {
            if let Some(query) = data.pointer("/parsed_query") {
                let title = match query {
                    Value::Array(arr) => arr.first().and_then(Value::as_str),
                    Value::String(s) => Some(s.as_str()),
                    _ => None,
                }
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().take(96).collect::<String>());
                session_title = title;
            }
        }
    }

    let task_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let model_id = if smart_mode == "auto" {
        "auto".to_string()
    } else if !selected_model.is_empty() {
        selected_model.clone()
    } else {
        // Fallback: no user selection on file, walk remaining authoritative
        // sources in priority order.
        //
        // 1. `timing_events.model_name` — the routed backend's real name.
        // 2. `model_config.config_name` — the TRAE CN product id (e.g.
        //    "Doubao-Seed-Code", no `__dev`).
        // 3. `model_config.model_name` stripped of a trailing `__dev` suffix
        //    — final fallback for files that never wrote config_name.
        let mut resolved = String::new();
        for (event_type, _, data, _) in &events {
            if event_type == "timing_events" {
                if let Some(name) = data
                    .pointer("/model_name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    resolved = name.trim().to_string();
                    break;
                }
            } else if event_type == "model_config" {
                let name = data
                    .pointer("/config_name")
                    .and_then(Value::as_str)
                    .or_else(|| data.pointer("/model_name").and_then(Value::as_str))
                    .filter(|s| !s.is_empty())
                    .map(|s| s.trim_end_matches("__dev").to_string());
                if let Some(n) = name {
                    resolved = n;
                    break;
                }
            }
        }
        if resolved.is_empty() {
            "unknown".to_string()
        } else {
            resolved
        }
    };

    let provider_id = "traecn";

    // Phase 3: emit one UnifiedMessage per `token_usage` event.
    let mut messages = Vec::new();
    for (idx, (event_type, _, data, event_clock)) in events.iter().enumerate() {
        if event_type != "token_usage" {
            continue;
        }
        let total = data
            .pointer("/total_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if total <= 0 {
            continue;
        }
        let prompt = data
            .pointer("/prompt_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let output = data
            .pointer("/completion_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let cache_read = data
            .pointer("/cache_read_input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let cache_write = data
            .pointer("/cache_creation_input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        // TRAE CN bills with cache-inclusive prompt tokens: `prompt_tokens`
        // already contains `cache_read + cache_write`. tokscale's
        // `TokenBreakdown::total()` sums `input + output + cache_read +
        // cache_write + reasoning`, so storing the raw prompt in `input`
        // would double-count the cache. Split the fresh (uncached) input out
        // so the components reconstruct the billed total exactly.
        let input = (prompt - cache_read - cache_write).max(0);
        // `reasoning_tokens` is a subset of `completion_tokens` (TRAE CN's
        // `total_tokens == prompt_tokens + completion_tokens` holds strictly,
        // with reasoning already folded into completion). It is therefore not
        // emitted as a separate component — doing so would double-count it in
        // `total()` — and is left inside `output`, matching the upstream trae
        // parser which also reports reasoning as 0.

        // Fall back to file mtime when the stream was entirely clockless.
        let mtime_ms = std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let timestamp = if *event_clock > 0 {
            *event_clock
        } else if mtime_ms > 0 {
            mtime_ms
        } else {
            continue;
        };

        let tokens = TokenBreakdown {
            input,
            output,
            cache_read,
            cache_write,
            cache_write_1h: 0,
            reasoning: 0,
        };
        if tokens.total() <= 0 {
            continue;
        }

        let dedup_key = format!("traecn:{}:usage-{}:{}-{}", task_id, idx, total, timestamp);

        let mut message = UnifiedMessage::new_with_dedup(
            "traecn",
            model_id.clone(),
            provider_id,
            task_id.clone(),
            timestamp,
            tokens,
            0.0,
            Some(dedup_key),
        );
        message.agent = agent_name.clone();
        message.session_title = session_title.clone();
        // Every `token_usage` row is one user turn start, so the first row of
        // a session anchors the turn-scoped metrics lane.
        message.is_turn_start = idx == 0;
        messages.push(message);
    }

    messages.sort_by_key(|m| (m.session_id.clone(), m.timestamp));
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_lines(path: &Path, lines: &[&str]) {
        let mut f = std::fs::File::create(path).unwrap();
        for line in lines {
            f.write_all(line.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
    }

    fn ev(t: &str, d: &str) -> String {
        format!(r#"{{"task_id":"t","seq":1,"type":"{}","data":{}}}"#, t, d)
    }

    #[test]
    fn parse_auto_mode_emits_model_auto() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task-auto.jsonl");
        write_lines(
            &path,
            &[
                &ev(
                    "metadata",
                    r#"{"created_at":1765400000,"chat_start_time":1765400000000,"parsed_query":["auto-test"],"model_smart_selection_meta":{"mode":"auto","config_name":""},"user_message_context":{"model_info":{"model_name":"","config_name":"","display_model_name":null}}}"#,
                ),
                &ev(
                    "model_config",
                    r#"{"model_name":"deepseek-v4.1-flash__dev","config_name":"deepseek-v4.1-flash"}"#,
                ),
                &ev(
                    "token_usage",
                    r#"{"total_tokens":3000,"prompt_tokens":2000,"completion_tokens":1000,"cache_read_input_tokens":1500,"cache_creation_input_tokens":0,"reasoning_tokens":50}"#,
                ),
            ],
        );

        let msgs = parse_traecn_file(&path);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].model_id, "auto");
        // TRAE CN's prompt_tokens (2000) is cache-inclusive: it already
        // contains cache_read (1500). tokscale's total() sums
        // input+output+cache_read+cache_write+reasoning, so the fresh input
        // is split out (2000-1500=500) and total() reconstructs the billed
        // total_tokens (3000) exactly.
        assert_eq!(msgs[0].tokens.input, 500);
        assert_eq!(msgs[0].tokens.output, 1000);
        assert_eq!(msgs[0].tokens.cache_read, 1500);
        assert_eq!(msgs[0].tokens.reasoning, 0);
        assert_eq!(msgs[0].tokens.total(), 3000);
        assert_eq!(msgs[0].timestamp, 1_765_400_000_000);
        assert_eq!(msgs[0].session_title.as_deref(), Some("auto-test"));
    }

    #[test]
    fn parse_manual_select_prefers_display_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task-manual.jsonl");
        write_lines(
            &path,
            &[
                &ev(
                    "metadata",
                    r#"{"created_at":1765400000,"parsed_query":["manual-test"],"model_smart_selection_meta":{"mode":"manual","config_name":"glm-4.6"},"user_message_context":{"model_info":{"model_name":"glm-4.6","config_name":"glm-4.6","display_model_name":"GLM-4.6"}}}"#,
                ),
                &ev(
                    "model_config",
                    r#"{"model_name":"glm-4.6__dev","config_name":"glm-4.6"}"#,
                ),
                &ev(
                    "token_usage",
                    r#"{"total_tokens":5000,"prompt_tokens":4000,"completion_tokens":1000}"#,
                ),
            ],
        );

        let msgs = parse_traecn_file(&path);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].model_id, "GLM-4.6");
        assert_eq!(msgs[0].tokens.total(), 5000);
    }

    #[test]
    fn token_usage_before_clock_gets_first_metadata_clock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task-leading-usage.jsonl");
        write_lines(
            &path,
            &[
                &ev(
                    "token_usage",
                    r#"{"total_tokens":1000,"prompt_tokens":800,"completion_tokens":200}"#,
                ),
                &ev(
                    "metadata",
                    r#"{"created_at":1765400000,"model_smart_selection_meta":{"mode":"auto","config_name":""},"user_message_context":{"model_info":{"model_name":"","config_name":"","display_model_name":null}}}"#,
                ),
            ],
        );
        let msgs = parse_traecn_file(&path);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].timestamp, 1_765_400_000_000);
    }

    #[test]
    fn plan_item_timing_supplies_clock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task-plan-item.jsonl");
        write_lines(
            &path,
            &[
                &ev(
                    "plan_item",
                    r#"{"timing":{"generated_at_ms":1765400000500,"tool_call_started_at_ms":1765400000510},"model_smart_selection_meta":{"mode":"auto","config_name":""},"user_message_context":{"model_info":{"model_name":"","config_name":"","display_model_name":null}}}"#,
                ),
                &ev(
                    "token_usage",
                    r#"{"total_tokens":1200,"prompt_tokens":900,"completion_tokens":300,"cache_read_input_tokens":100}"#,
                ),
            ],
        );
        let msgs = parse_traecn_file(&path);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].timestamp, 1_765_400_000_510);
        assert_eq!(msgs[0].tokens.cache_read, 100);
        // prompt_tokens is cache-inclusive, so total() must still reconstruct
        // the billed total (1200), not 1300 from double-counting the cache.
        assert_eq!(msgs[0].tokens.input, 800);
        assert_eq!(msgs[0].tokens.total(), 1200);
    }

    #[test]
    fn cache_inclusive_prompt_is_split_into_fresh_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task-cache.jsonl");
        write_lines(
            &path,
            &[
                &ev(
                    "metadata",
                    r#"{"created_at":1765400000,"model_smart_selection_meta":{"mode":"auto","config_name":""},"user_message_context":{"model_info":{"model_name":"","config_name":"","display_model_name":null}}}"#,
                ),
                // prompt=42462 already contains cache_read=5504. Fresh input
                // is 42462-5504 = 36958, and total() == total_tokens == 42983.
                &ev(
                    "token_usage",
                    r#"{"total_tokens":42983,"prompt_tokens":42462,"completion_tokens":521,"cache_read_input_tokens":5504,"cache_creation_input_tokens":0,"reasoning_tokens":389}"#,
                ),
            ],
        );
        let msgs = parse_traecn_file(&path);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].tokens.input, 36958);
        assert_eq!(msgs[0].tokens.cache_read, 5504);
        // reasoning stays inside completion and is reported as 0 so total()
        // reconstructs the billed total_tokens exactly.
        assert_eq!(msgs[0].tokens.reasoning, 0);
        assert_eq!(msgs[0].tokens.output, 521);
        assert_eq!(msgs[0].tokens.total(), 42983);
    }

    #[test]
    fn skips_zero_turn_token_usage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task-empty.jsonl");
        write_lines(
            &path,
            &[
                &ev(
                    "metadata",
                    r#"{"created_at":1765400000,"model_smart_selection_meta":{"mode":"auto","config_name":""},"user_message_context":{"model_info":{"model_name":"","config_name":"","display_model_name":null}}}"#,
                ),
                &ev(
                    "token_usage",
                    r#"{"total_tokens":0,"prompt_tokens":0,"completion_tokens":0}"#,
                ),
            ],
        );
        let msgs = parse_traecn_file(&path);
        assert!(msgs.is_empty());
    }

    #[test]
    fn handles_torn_trailing_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task-torn.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(ev(
            "metadata",
            r#"{"created_at":1765400000,"model_smart_selection_meta":{"mode":"auto","config_name":""},"user_message_context":{"model_info":{"model_name":"","config_name":"","display_model_name":null}}}"#,
        ).as_bytes()).unwrap();
        f.write_all(b"\n").unwrap();
        // Incomplete line — should be silently skipped
        f.write_all(
            br#"{"task_id":"t","seq":1,"type":"token_usage","data":"{\"total_tokens\":100,"#,
        )
        .unwrap();
        let msgs = parse_traecn_file(&path);
        assert!(msgs.is_empty());
    }
}
