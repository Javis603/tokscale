//! Proma agent sessions: `~/.proma/agent-sessions/*.jsonl`.
//!
//! Port of Token Monitor's `src/shared/providers/proma/usage.js`. Assistant
//! lines carry Anthropic-style `usage`; several streamed chunks share one
//! message id (thinking / tool_use / text), so each id keeps its largest chunk
//! and the latest timestamp among its chunks.
//!
//! One deliberate difference: the JS adapter reported every row under the
//! provider `proma`. Here the provider is inferred from the model, because
//! tokscale prices with it as a hint; `proma` matches no catalog provider and
//! lost the cache-read rate for some models.

use super::js;
use crate::sessions::UnifiedMessage;
use crate::TokenBreakdown;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const CLIENT_ID: &str = "proma";

pub fn root(home_dir: &str) -> PathBuf {
    Path::new(home_dir).join(".proma").join("agent-sessions")
}

pub fn parse(home_dir: &str) -> Vec<UnifiedMessage> {
    parse_root(&root(home_dir))
}

pub fn parse_root(root: &Path) -> Vec<UnifiedMessage> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    // Session ids carry a namespace derived from the root, so the same file
    // name under two roots (host and WSL) stays two sessions.
    let namespace = js::path_namespace(root);
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    files
        .iter()
        .flat_map(|path| parse_file(path, &namespace))
        .collect()
}

/// `numberValue(a || b)`: the first truthy field, as a finite count.
fn count(usage: &Value, keys: &[&str]) -> i64 {
    js::safe_count(js::js_number(js::first_truthy(usage, keys))).unwrap_or(0)
}

struct Chunk {
    model: String,
    tokens: TokenBreakdown,
    created_at: i64,
}

fn total(tokens: &TokenBreakdown) -> i64 {
    tokens.input + tokens.output + tokens.cache_read + tokens.cache_write
}

fn parse_file(path: &Path, namespace: &str) -> Vec<UnifiedMessage> {
    // Like Node's utf8 decoding, an invalid byte costs only the line it is on.
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let content = String::from_utf8_lossy(&bytes);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let session_id = format!("{stem}@{namespace}");

    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<Chunk>> = HashMap::new();
    for line in content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(message) = obj.get("message").filter(|m| js::truthy(m)) else {
            continue;
        };
        let Some(usage) = message.get("usage").filter(|u| js::truthy(u)) else {
            continue;
        };
        // Message ID: some tools set message.id, Proma uses the line's uuid.
        let Some(id) = message
            .get("id")
            .filter(|v| js::truthy(v))
            .or_else(|| obj.get("uuid").filter(|v| js::truthy(v)))
            .map(js::js_string)
        else {
            continue;
        };
        let id = id.as_str();
        let model = message
            .get("model")
            .filter(|v| js::truthy(v))
            .or_else(|| obj.get("_channelModelId").filter(|v| js::truthy(v)))
            .map(js::js_string)
            .unwrap_or_else(|| "unknown".to_string());
        let tokens = TokenBreakdown {
            input: count(usage, &["input_tokens", "inputTokens"]),
            output: count(usage, &["output_tokens", "outputTokens"]),
            cache_read: count(usage, &["cache_read_input_tokens", "cacheReadInputTokens"]),
            cache_write: count(
                usage,
                &["cache_creation_input_tokens", "cacheCreationInputTokens"],
            ),
            cache_write_1h: 0,
            reasoning: 0,
        };
        let created_at = js::timestamp_ms(js::first_truthy(
            &obj,
            &["_createdAt", "createdAt", "created_at", "timestamp"],
        ));
        if !groups.contains_key(id) {
            order.push(id.to_string());
        }
        groups.entry(id.to_string()).or_default().push(Chunk {
            model,
            tokens,
            created_at,
        });
    }

    order
        .into_iter()
        .filter_map(|id| {
            let chunks = groups.remove(&id)?;
            let created_at = chunks
                .iter()
                .map(|c| c.created_at)
                .max()
                .unwrap_or(0)
                .max(0);
            // Largest chunk wins; on a tie the first one seen, like the JS stable sort.
            let best = chunks.into_iter().reduce(|best, next| {
                if total(&next.tokens) > total(&best.tokens) {
                    next
                } else {
                    best
                }
            })?;
            let provider = crate::provider_identity::inferred_provider_from_model(&best.model)
                .unwrap_or("proma")
                .to_string();
            Some(UnifiedMessage::new_with_dedup(
                CLIENT_ID,
                best.model,
                provider,
                session_id.clone(),
                created_at,
                best.tokens,
                0.0,
                Some(format!("proma:{session_id}:{id}")),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, lines: &[Value]) {
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(dir.join(name), body.join("\n") + "\n").unwrap();
    }

    fn assistant(id: &str, model: &str, input: i64, output: i64, created_at: &str) -> Value {
        serde_json::json!({
            "type": "assistant",
            "_createdAt": created_at,
            "message": { "id": id, "model": model, "usage": { "input_tokens": input, "output_tokens": output } }
        })
    }

    #[test]
    fn streamed_chunks_keep_the_largest_usage_and_the_latest_time() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "s1.jsonl",
            &[
                assistant("m1", "claude-sonnet", 10, 5, "2026-10-01T10:00:00Z"),
                assistant("m1", "claude-sonnet", 60, 40, "2026-10-01T10:00:01Z"),
                assistant("m1", "claude-sonnet", 1, 1, "2026-10-01T10:00:09Z"),
                serde_json::json!({ "type": "user", "message": { "content": "hi" } }),
            ],
        );
        let messages = parse_root(dir.path());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input + messages[0].tokens.output, 100);
        assert_eq!(messages[0].timestamp, 1_790_848_809_000);
        assert_eq!(messages[0].client, "proma");
    }

    #[test]
    fn skips_falsy_usage_and_keeps_the_first_truthy_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "s1.jsonl",
            &[
                serde_json::json!({ "type": "assistant", "message": { "id": "a", "usage": false } }),
                serde_json::json!({ "type": "assistant", "message": { "id": "b", "usage": 0 } }),
                serde_json::json!({ "type": "assistant", "message": { "id": "c", "usage": "" } }),
                serde_json::json!({
                    "type": "assistant",
                    "_createdAt": 0,
                    "createdAt": "2026-10-01T10:00:00Z",
                    "message": { "id": 7, "model": "m", "usage": { "input_tokens": 0, "inputTokens": "5", "output_tokens": "Infinity" } }
                }),
            ],
        );
        let messages = parse_root(dir.path());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].timestamp, 1_790_848_800_000);
        assert_eq!(messages[0].tokens.input, 5);
        assert_eq!(messages[0].tokens.output, 0);
        assert_eq!(
            messages[0].dedup_key.as_deref(),
            Some(format!("proma:{}:7", messages[0].session_id).as_str())
        );
    }

    #[test]
    fn an_invalid_utf8_byte_costs_only_its_line() {
        let dir = tempfile::tempdir().unwrap();
        let good = assistant("m1", "claude-sonnet", 10, 5, "2026-10-01T10:00:00Z").to_string();
        let mut body = b"{\"type\":\"assistant\",\"message\":\"\xff\"}\n".to_vec();
        body.extend_from_slice(good.as_bytes());
        body.push(b'\n');
        std::fs::write(dir.path().join("s1.jsonl"), body).unwrap();
        let messages = parse_root(dir.path());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 10);
    }

    #[test]
    fn missing_root_is_empty() {
        assert!(parse_root(Path::new("/definitely/not/here")).is_empty());
    }
}
