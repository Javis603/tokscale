//! Best-effort request timing for Codex performance, independent of activity spans.
//!
//! token_count can be persisted after local tools finish. Prefer a matching
//! token_usage_record; on older rollouts, use the final model item when a tool
//! result and token_count were flushed together. Never subtract tool durations:
//! parallel tools and model output can overlap.

use super::codex::{CodexPayload, CodexTotals};
use super::PerformanceTiming;

const SAME_FLUSH_MS: i64 = 100;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct CodexRequestTiming {
    boundary_ms: Option<i64>,
    last_count_ms: Option<i64>,
    request_start_ms: Option<i64>,
    model_end_ms: Option<i64>,
    tool_result_after_output_ms: Option<i64>,
    usage_record: Option<(i64, CodexTotals)>,
}

impl CodexRequestTiming {
    pub(super) fn observe(
        &mut self,
        entry_type: &str,
        timestamp: Option<&str>,
        payload: &CodexPayload,
    ) {
        let Some(at) = super::codex::parse_codex_entry_timestamp(timestamp) else {
            return;
        };
        let kind = payload.payload_type.as_deref().unwrap_or("");
        match entry_type {
            "turn_context" => self.reset(Some(at)),
            "token_usage_record" => {
                self.usage_record = payload
                    .usage
                    .as_ref()
                    .map(|usage| (at, CodexTotals::from_usage(usage)));
            }
            "event_msg" if kind == "task_started" || kind == "turn_aborted" => {
                self.reset(Some(at));
            }
            "event_msg"
                if kind == "user_message"
                    && super::codex::codex_message_is_human_turn(payload.message.as_deref()) =>
            {
                self.reset(Some(at));
            }
            "event_msg" if kind == "task_complete" => self.reset(None),
            "response_item" if kind.ends_with("_output") => {
                self.boundary_ms = self.boundary_ms.max(Some(at));
                if self.model_end_ms.is_some() {
                    self.tool_result_after_output_ms = Some(at);
                }
            }
            "response_item"
                if kind == "reasoning"
                    || kind.ends_with("_call")
                    || kind == "message"
                        && payload.role.as_ref().and_then(serde_json::Value::as_str)
                            == Some("assistant") =>
            {
                // Some old versions flush the previous response's items just
                // after token_count. They cannot anchor the next request.
                if self.request_start_ms.is_none()
                    && self.last_count_ms.is_some_and(|last| {
                        at.checked_sub(last)
                            .is_some_and(|gap| (0..=SAME_FLUSH_MS).contains(&gap))
                    })
                {
                    return;
                }
                if self.request_start_ms.is_none() {
                    self.request_start_ms = self.boundary_ms;
                }
                self.model_end_ms = Some(at);
                self.tool_result_after_output_ms = None;
            }
            "response_item" if kind == "message" => {
                self.boundary_ms = self.boundary_ms.max(Some(at));
            }
            _ => {}
        }
    }

    fn reset(&mut self, boundary_ms: Option<i64>) {
        *self = Self {
            boundary_ms,
            last_count_ms: self.last_count_ms,
            ..Self::default()
        };
    }

    pub(super) fn finish(
        &mut self,
        count_ms: Option<i64>,
        last_usage: Option<CodexTotals>,
    ) -> PerformanceTiming {
        let start = self.request_start_ms.or(self.boundary_ms);
        let end = match self.usage_record {
            // A usage record must identify the same response, including all
            // token buckets; an unpaired record is not evidence for timing.
            Some((at, usage))
                if Some(usage) == last_usage
                    && self.model_end_ms.is_none_or(|model_end| at >= model_end)
                    && count_ms.is_none_or(|count| at <= count) =>
            {
                Some(at)
            }
            Some(_) => None,
            None => {
                let delayed_by_tools = match (self.tool_result_after_output_ms, count_ms) {
                    (Some(tool), Some(count)) => count
                        .checked_sub(tool)
                        .is_some_and(|gap| (0..=SAME_FLUSH_MS).contains(&gap)),
                    (Some(_), None) => true,
                    _ => false,
                };
                if delayed_by_tools {
                    self.model_end_ms
                } else {
                    count_ms
                }
            }
        };
        let next_boundary = self.boundary_ms.max(count_ms);
        self.reset(next_boundary);
        self.last_count_ms = self.last_count_ms.max(count_ms);
        match start
            .zip(end)
            .and_then(|(start, end)| end.checked_sub(start))
        {
            Some(duration) if duration > 0 => PerformanceTiming::Duration(duration),
            _ => PerformanceTiming::Unavailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::codex::{parse_codex_file, parse_codex_file_incremental, CodexParseState};
    use crate::{aggregate_model_usage_entries, GroupBy};
    use serde_json::{json, Value};
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn row(seconds: i64, kind: &str, payload: Value) -> String {
        json!({"timestamp": format!("2040-01-01T00:00:{seconds:02}Z"), "type":kind, "payload":payload}).to_string()
    }

    fn usage(output: i64) -> Value {
        json!({"input_tokens":100,"cached_input_tokens":0,"output_tokens":output,"reasoning_output_tokens":0})
    }

    fn count(seconds: i64, total: i64, last: i64) -> String {
        row(
            seconds,
            "event_msg",
            json!({"type":"token_count","info":{
                "total_token_usage":{"input_tokens":total,"output_tokens":total},
                "last_token_usage":usage(last)
            }}),
        )
    }

    fn call(seconds: i64, id: &str) -> String {
        row(
            seconds,
            "response_item",
            json!({"type":"function_call","call_id":id}),
        )
    }

    fn result(seconds: i64, id: &str) -> String {
        row(
            seconds,
            "response_item",
            json!({"type":"function_call_output","call_id":id}),
        )
    }

    fn fixture(wait: i64, records: bool) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        let mut rows = vec![
            row(0, "turn_context", json!({"model":"gpt-5.4"})),
            call(10, "a"),
        ];
        if records {
            rows.push(row(10, "token_usage_record", json!({"usage":usage(100)})));
        }
        rows.extend([
            result(10 + wait, "a"),
            count(10 + wait, 100, 100),
            call(20 + wait, "b"),
        ]);
        if records {
            rows.push(row(
                20 + wait,
                "token_usage_record",
                json!({"usage":usage(100)}),
            ));
        }
        rows.extend([result(20 + wait * 2, "b"), count(20 + wait * 2, 200, 100)]);
        writeln!(file, "{}", rows.join("\n")).unwrap();
        file
    }

    #[test]
    fn tool_wait_changes_activity_but_not_matched_request_performance() {
        for records in [true, false] {
            for wait in [0, 15] {
                let file = fixture(wait, records);
                let messages = parse_codex_file(file.path());
                assert_eq!(messages.len(), 2);
                assert_eq!(
                    messages.iter().map(|m| m.duration_ms.unwrap()).sum::<i64>(),
                    20_000 + wait * 2_000
                );
                assert!(messages
                    .iter()
                    .all(|m| m.performance_duration_ms() == Some(10_000)));
                let entries = aggregate_model_usage_entries(messages, &GroupBy::ClientModel);
                assert_eq!(entries[0].performance.total_duration_ms, 20_000);
                assert_eq!(entries[0].performance.timed_output_tokens, 200);
            }
        }
    }

    #[test]
    fn parallel_tool_results_do_not_get_subtracted_from_generation() {
        let mut file = NamedTempFile::new().unwrap();
        let rows = [
            row(0, "turn_context", json!({"model":"gpt-5.4"})),
            call(5, "a"),
            call(6, "b"),
            result(7, "a"),
            row(
                8,
                "response_item",
                json!({"type":"message","role":"assistant"}),
            ),
            row(10, "token_usage_record", json!({"usage":usage(100)})),
            result(30, "b"),
            count(30, 100, 100),
        ];
        writeln!(file, "{}", rows.join("\n")).unwrap();
        let messages = parse_codex_file(file.path());
        assert_eq!(messages[0].performance_duration_ms(), Some(10_000));
        assert_eq!(messages[0].duration_ms, Some(30_000));
    }

    #[test]
    fn incremental_timing_survives_serialized_state_before_tool_completion() {
        let file = fixture(15, true);
        let raw = std::fs::read_to_string(file.path()).unwrap();
        for split in 1..raw.lines().count() {
            let mut partial = NamedTempFile::new().unwrap();
            let first = raw.lines().take(split).collect::<Vec<_>>().join("\n") + "\n";
            write!(partial, "{first}").unwrap();
            let initial =
                parse_codex_file_incremental(partial.path(), 0, CodexParseState::default());
            let state = bincode::deserialize(&bincode::serialize(&initial.state).unwrap()).unwrap();
            writeln!(
                partial,
                "{}",
                raw.lines().skip(split).collect::<Vec<_>>().join("\n")
            )
            .unwrap();
            let appended = parse_codex_file_incremental(partial.path(), first.len() as u64, state);
            let mut combined = initial.messages;
            combined.extend(appended.messages);
            assert_eq!(
                combined,
                parse_codex_file(partial.path()),
                "split at line {split}"
            );
        }
    }

    #[test]
    fn mismatched_usage_record_is_untimed_without_losing_tokens() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "{}",
            [
                row(0, "turn_context", json!({"model":"gpt-5.4"})),
                call(5, "a"),
                row(5, "token_usage_record", json!({"usage":usage(99)})),
                result(20, "a"),
                count(20, 100, 100)
            ]
            .join("\n")
        )
        .unwrap();
        let messages = parse_codex_file(file.path());
        assert_eq!(messages[0].tokens.output, 100);
        assert_eq!(
            messages[0].performance_timing,
            Some(PerformanceTiming::Unavailable)
        );
        assert_eq!(messages[0].duration_ms, Some(20_000));
        let entry = &aggregate_model_usage_entries(messages, &GroupBy::ClientModel)[0];
        assert_eq!(entry.output, 100);
        assert_eq!(entry.performance.timed_output_tokens, 0);
        assert_eq!(entry.performance.total_duration_ms, 0);
    }

    #[test]
    fn interrupted_request_does_not_lend_its_start_to_a_later_turn() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "{}",
            [
                row(0, "turn_context", json!({"model":"gpt-5.4"})),
                call(5, "a"),
                row(10, "event_msg", json!({"type":"turn_aborted"})),
                row(30, "event_msg", json!({"type":"task_started"})),
                call(35, "b"),
                row(35, "token_usage_record", json!({"usage":usage(100)})),
                count(35, 100, 100)
            ]
            .join("\n")
        )
        .unwrap();
        assert_eq!(
            parse_codex_file(file.path())[0].performance_duration_ms(),
            Some(5_000)
        );
    }

    #[test]
    fn injected_user_event_does_not_restart_request_timing() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "{}", [
            row(0, "turn_context", json!({"model":"gpt-5.4"})),
            row(10, "event_msg", json!({"type":"user_message","message":"<system-reminder>context</system-reminder>"})),
            call(15, "a"), count(15, 100, 100)
        ].join("\n")).unwrap();
        assert_eq!(
            parse_codex_file(file.path())[0].performance_duration_ms(),
            Some(15_000)
        );
    }

    #[test]
    fn zero_or_backward_timing_keeps_usage_without_performance() {
        for end in [0, -1] {
            let mut timing = CodexRequestTiming {
                boundary_ms: Some(1000),
                ..Default::default()
            };
            assert_eq!(
                timing.finish(Some(1000 + end), None),
                PerformanceTiming::Unavailable
            );
        }
    }

    #[test]
    fn usage_record_outside_its_response_window_is_not_a_timing_sample() {
        let usage = CodexTotals::default();
        for record_at in [1500, 4000] {
            let mut timing = CodexRequestTiming {
                boundary_ms: Some(1000),
                model_end_ms: Some(2000),
                usage_record: Some((record_at, usage)),
                ..Default::default()
            };
            assert_eq!(
                timing.finish(Some(3000), Some(usage)),
                PerformanceTiming::Unavailable
            );
        }
    }
}
