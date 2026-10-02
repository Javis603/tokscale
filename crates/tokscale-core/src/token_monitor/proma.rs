//! Proma agent sessions: `~/.proma/agent-sessions/*.jsonl`.
//!
//! Port of Token Monitor's `src/shared/providers/proma/usage.js`. Assistant
//! lines carry Anthropic-style `usage`; several streamed chunks share one
//! message id (thinking / tool_use / text), so each id keeps its largest chunk
//! and the latest timestamp among its chunks.

use crate::sessions::UnifiedMessage;
use crate::TokenBreakdown;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const CLIENT_ID: &str = "proma";

pub fn root(home_dir: &str) -> PathBuf {
    Path::new(home_dir).join(".proma").join("agent-sessions")
}

pub fn parse(home_dir: &str) -> Vec<UnifiedMessage> {
    parse_root(&root(home_dir))
}

/// Session ids carry a namespace derived from the root so the same file name
/// under two roots (host and WSL) stays two sessions, matching the JS adapter.
fn source_namespace(root: &Path) -> String {
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    digest
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn parse_root(root: &Path) -> Vec<UnifiedMessage> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let namespace = source_namespace(root);
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

fn number(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()).unwrap_or(0.0) as i64,
        Some(Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .unwrap_or(0.0) as i64,
        _ => 0,
    }
}

fn first_number(usage: &Value, keys: &[&str]) -> i64 {
    keys.iter()
        .map(|key| number(usage.get(*key)))
        .find(|value| *value != 0)
        .unwrap_or(0)
}

fn timestamp_ms(value: Option<&Value>) -> i64 {
    let from_number = |n: f64| -> i64 {
        if n > 0.0 && n < 1e12 {
            (n * 1000.0) as i64
        } else {
            n as i64
        }
    };
    match value {
        Some(Value::Number(n)) => n
            .as_f64()
            .filter(|f| f.is_finite())
            .map(from_number)
            .unwrap_or(0),
        Some(Value::String(s)) if !s.trim().is_empty() => {
            if let Ok(n) = s.trim().parse::<f64>() {
                return from_number(n);
            }
            chrono::DateTime::parse_from_rfc3339(s.trim())
                .map(|dt| dt.timestamp_millis())
                .unwrap_or(0)
        }
        _ => 0,
    }
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
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
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
        let Some(message) = obj.get("message") else {
            continue;
        };
        let Some(usage) = message.get("usage").filter(|u| !u.is_null()) else {
            continue;
        };
        let id = message
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                obj.get("uuid")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            });
        let Some(id) = id else { continue };
        let model = message
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                obj.get("_channelModelId")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or("unknown")
            .to_string();
        let tokens = TokenBreakdown {
            input: first_number(usage, &["input_tokens", "inputTokens"]),
            output: first_number(usage, &["output_tokens", "outputTokens"]),
            cache_read: first_number(usage, &["cache_read_input_tokens", "cacheReadInputTokens"]),
            cache_write: first_number(
                usage,
                &["cache_creation_input_tokens", "cacheCreationInputTokens"],
            ),
            reasoning: 0,
        };
        let created_at = ["_createdAt", "createdAt", "created_at", "timestamp"]
            .iter()
            .map(|key| obj.get(*key))
            .find(|value| value.is_some_and(|v| !v.is_null() && v != &Value::String(String::new())))
            .map(timestamp_ms)
            .unwrap_or(0);
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
    fn missing_root_is_empty() {
        assert!(parse_root(Path::new("/definitely/not/here")).is_empty());
    }
}
