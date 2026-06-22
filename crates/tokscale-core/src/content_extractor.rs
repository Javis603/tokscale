use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::Path;

use super::sessions::utils::open_readonly_sqlite;

const MAX_CONTENT_CHARS: usize = 1000;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionContent {
    pub session_id: String,
    pub first_user_message: Option<String>,
    pub client: String,
}

pub fn extract_opencode_content(db_path: &Path, session_id: &str) -> Option<SessionContent> {
    let conn = open_readonly_sqlite(db_path)?;

    let sql = r#"
        SELECT json_extract(data, '$.parts') as parts
        FROM message
        WHERE session_id = ?1
        AND json_extract(data, '$.role') = 'user'
        ORDER BY CAST(json_extract(data, '$.time.created') AS REAL) ASC
        LIMIT 1
    "#;

    let first_user: Option<String> = conn
        .query_row(sql, [session_id], |row| row.get::<_, Option<String>>(0))
        .ok()
        .flatten();

    let text = first_user.and_then(|parts_json| {
        let parts: Vec<Value> = serde_json::from_str(&parts_json).ok()?;
        let mut combined = String::new();
        for part in &parts {
            if let Some(t) = part.get("content").and_then(|c| c.as_str()) {
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str(t);
            } else if let Some(t) = part.as_str() {
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str(t);
            }
        }
        if combined.is_empty() {
            None
        } else {
            Some(truncate(&combined, MAX_CONTENT_CHARS))
        }
    });

    let text = text.or_else(|| {
        let sql_fallback = r#"
            SELECT json_extract(data, '$.content') as content
            FROM message
            WHERE session_id = ?1
            AND json_extract(data, '$.role') = 'user'
            ORDER BY CAST(json_extract(data, '$.time.created') AS REAL) ASC
            LIMIT 1
        "#;
        conn.query_row(sql_fallback, [session_id], |row| {
            row.get::<_, Option<String>>(0)
        })
        .ok()
        .flatten()
        .map(|s| truncate(&s, MAX_CONTENT_CHARS))
    });

    Some(SessionContent {
        session_id: session_id.to_string(),
        first_user_message: text,
        client: "opencode".to_string(),
    })
}

pub fn extract_claudecode_content(jsonl_path: &Path, session_id: &str) -> Option<SessionContent> {
    let file = std::fs::File::open(jsonl_path).ok()?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if !trimmed.contains("\"human\"") && !trimmed.contains("\"user\"") {
            continue;
        }

        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if entry_type != "human" && entry_type != "user" {
            continue;
        }

        let text = extract_text_from_claude_message(&value);
        if let Some(t) = text {
            if !t.trim().is_empty() {
                return Some(SessionContent {
                    session_id: session_id.to_string(),
                    first_user_message: Some(truncate(&t, MAX_CONTENT_CHARS)),
                    client: "claude".to_string(),
                });
            }
        }
    }

    Some(SessionContent {
        session_id: session_id.to_string(),
        first_user_message: None,
        client: "claude".to_string(),
    })
}

pub fn extract_codex_content(jsonl_path: &Path, session_id: &str) -> Option<SessionContent> {
    let file = std::fs::File::open(jsonl_path).ok()?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let msg_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("");

        if msg_type == "user_message" || msg_type == "input" {
            let text = value
                .get("content")
                .or_else(|| value.get("text"))
                .or_else(|| value.get("payload").and_then(|p| p.get("content")))
                .and_then(|v| v.as_str())
                .map(|s| truncate(s, MAX_CONTENT_CHARS));

            if text.is_some() {
                return Some(SessionContent {
                    session_id: session_id.to_string(),
                    first_user_message: text,
                    client: "codex".to_string(),
                });
            }
        }
    }

    Some(SessionContent {
        session_id: session_id.to_string(),
        first_user_message: None,
        client: "codex".to_string(),
    })
}

pub fn extract_gemini_content(json_path: &Path, session_id: &str) -> Option<SessionContent> {
    let content = std::fs::read_to_string(json_path).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;

    let messages = value.get("messages").and_then(|m| m.as_array())?;

    for msg in messages {
        let msg_type = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if msg_type == "user" || msg_type == "human" {
            let text = msg
                .get("text")
                .or_else(|| msg.get("content"))
                .and_then(|v| v.as_str());

            if let Some(t) = text {
                if !t.trim().is_empty() {
                    return Some(SessionContent {
                        session_id: session_id.to_string(),
                        first_user_message: Some(truncate(t, MAX_CONTENT_CHARS)),
                        client: "gemini".to_string(),
                    });
                }
            }
        }
    }

    Some(SessionContent {
        session_id: session_id.to_string(),
        first_user_message: None,
        client: "gemini".to_string(),
    })
}

pub fn metadata_only_content(session_id: &str, client: &str) -> SessionContent {
    SessionContent {
        session_id: session_id.to_string(),
        first_user_message: None,
        client: client.to_string(),
    }
}

/// Dispatch to the correct per-client extractor for `client`, reading the
/// session's actual file(s) from `candidate_paths`, and return the first result
/// that yields a real `first_user_message`.
///
/// Falls back to [`metadata_only_content`] — never an error or panic — when:
/// - the client has no dedicated extractor,
/// - `candidate_paths` is empty,
/// - every candidate file is missing/unreadable/unparseable, or
/// - no candidate produced a non-empty first user message.
///
/// For file-keyed clients (claude, codex, gemini) each candidate is the
/// session's own transcript file. For opencode the session lives as rows inside
/// a shared SQLite database, so `candidate_paths` should carry the opencode
/// database(s) and the extractor selects the session by `session_id` internally.
pub fn extract_session_content(
    client: &str,
    session_id: &str,
    candidate_paths: &[std::path::PathBuf],
) -> SessionContent {
    let extractor: fn(&Path, &str) -> Option<SessionContent> = match client {
        "opencode" => extract_opencode_content,
        "claude" => extract_claudecode_content,
        "codex" => extract_codex_content,
        "gemini" => extract_gemini_content,
        // Unknown/unsupported client: no dedicated extractor.
        _ => return metadata_only_content(session_id, client),
    };

    for path in candidate_paths {
        if let Some(content) = extractor(path, session_id) {
            // A real extractor can still return `Some` with `first_user_message:
            // None` (e.g. the file parsed but held no user message); keep
            // scanning the remaining candidates for one that does.
            if content.first_user_message.is_some() {
                return content;
            }
        }
    }

    metadata_only_content(session_id, client)
}

fn extract_text_from_claude_message(value: &Value) -> Option<String> {
    if let Some(content) = value.get("message").and_then(|m| m.get("content")) {
        if let Some(arr) = content.as_array() {
            let mut combined = String::new();
            for block in arr {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str(t);
                }
            }
            if !combined.is_empty() {
                return Some(combined);
            }
        }
        if let Some(s) = content.as_str() {
            return Some(s.to_string());
        }
    }

    if let Some(content) = value.get("content") {
        if let Some(s) = content.as_str() {
            return Some(s.to_string());
        }
        if let Some(arr) = content.as_array() {
            let mut combined = String::new();
            for block in arr {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str(t);
                }
            }
            if !combined.is_empty() {
                return Some(combined);
            }
        }
    }

    None
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let boundary = s
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}...", &s[..boundary])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extract_session_content_dispatches_to_real_claude_extractor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"Add a CLI flag"}]}}
"#,
        )
        .unwrap();

        let content = extract_session_content("claude", "sess", &[path]);
        assert_eq!(content.first_user_message.as_deref(), Some("Add a CLI flag"));
        assert_eq!(content.client, "claude");
    }

    #[test]
    fn extract_session_content_unknown_client_is_metadata_only() {
        let content = extract_session_content("totally-unknown", "sess", &[PathBuf::from("/nope")]);
        assert!(content.first_user_message.is_none());
        assert_eq!(content.client, "totally-unknown");
    }

    #[test]
    fn extract_session_content_no_candidates_is_metadata_only() {
        let content = extract_session_content("claude", "sess", &[]);
        assert!(content.first_user_message.is_none());
        assert_eq!(content.client, "claude");
    }

    #[test]
    fn extract_session_content_unreadable_file_does_not_panic() {
        // Missing/unparseable candidate must degrade gracefully, never panic.
        let content =
            extract_session_content("codex", "sess", &[PathBuf::from("/definitely/missing.jsonl")]);
        assert!(content.first_user_message.is_none());
        assert_eq!(content.client, "codex");
    }
}
