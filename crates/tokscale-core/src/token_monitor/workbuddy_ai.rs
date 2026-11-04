//! WorkBuddy AI — the international build — as its own Token Monitor client.
//!
//! Upstream folds every WorkBuddy surface into one client
//! (`ClientId::WorkBuddy`), because #1334 equated "WorkBuddy 5.5" with
//! "WorkBuddy AI" back when the CN build briefly moved its home to
//! `~/.workbuddy-ai`. Current builds keep separate homes — CN WorkBuddy in
//! `~/.workbuddy`, the international WorkBuddy AI in `~/.workbuddy-ai` — so
//! the shared root lands both editions' usage on one client (Token Monitor
//! issue #1109). Both builds write byte-identical Tencent buddy transcripts,
//! so the detailed reader is shared and only the home root and client id
//! differ; the scanner no longer points the upstream WorkBuddy lane at the AI
//! root, which this module owns instead.
//!
//! The SQLite fallback and its session-level merge mirror
//! `sessions/workbuddy.rs` with the client id swapped; a session present in
//! both homes stays counted once per edition, because every dedup key is
//! client-prefixed.

use crate::sessions::tencent_buddy::{
    is_extension_log_source, is_jsonl_source, parse_extension_log_file, parse_jsonl_file,
};
use crate::sessions::utils::sqlite_for_each_row;
use crate::sessions::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::TokenBreakdown;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const CLIENT_ID: &str = "workbuddy-ai";
const HOME_DIR: &str = ".workbuddy-ai";
/// Matches the SQLite fallback model in `sessions/workbuddy.rs` and the
/// default model Token Tracker documents for both builds; records seen in the
/// wild always carry `providerData.model`.
const DEFAULT_MODEL: &str = "auto";

pub fn parse(home_dir: &str) -> Vec<UnifiedMessage> {
    let home = Path::new(home_dir).join(HOME_DIR);
    let detailed = collect_project_files(&home.join("projects"))
        .into_iter()
        .flat_map(|path| parse_workbuddy_ai_file(&path))
        .collect::<Vec<_>>();
    let fallback = parse_workbuddy_ai_sqlite(&home.join("workbuddy.db"));
    merge_detailed_and_fallback(detailed, fallback)
}

fn parse_workbuddy_ai_file(path: &Path) -> Vec<UnifiedMessage> {
    if is_jsonl_source(path) {
        return parse_jsonl_file(CLIENT_ID, DEFAULT_MODEL, path);
    }
    if is_extension_log_source(path) {
        return parse_extension_log_file(CLIENT_ID, DEFAULT_MODEL, path);
    }
    Vec::new()
}

/// Every `*.jsonl` under `projects/`, including nested
/// `<sessionId>/subagents/agent-*.jsonl` files. Mirrors the upstream scanner's
/// pattern walk for the WorkBuddy roots this client took over.
fn collect_project_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
    }
    files
}

/// Mirror of `merge_workbuddy_messages` in `lib.rs`: a SQLite fallback row is
/// redundant exactly when its session already has detailed JSONL coverage,
/// independent of calendar day. Both parsers derive `session_id` from the same
/// WorkBuddy session identifier, so the keys are directly comparable.
fn merge_detailed_and_fallback(
    detailed: Vec<UnifiedMessage>,
    fallback: Vec<UnifiedMessage>,
) -> Vec<UnifiedMessage> {
    let detailed_sessions: HashSet<String> = detailed
        .iter()
        .filter(|message| !message.session_id.is_empty())
        .map(|message| message.session_id.clone())
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut merged: Vec<UnifiedMessage> = detailed
        .into_iter()
        .filter(|message| keep_deduped(&mut seen, message))
        .collect();
    merged.extend(fallback.into_iter().filter(|message| {
        !detailed_sessions.contains(&message.session_id) && keep_deduped(&mut seen, message)
    }));
    merged
}

fn keep_deduped(seen_keys: &mut HashSet<String>, message: &UnifiedMessage) -> bool {
    message
        .dedup_key
        .as_ref()
        .is_none_or(|key| seen_keys.insert(key.clone()))
}

#[derive(Debug)]
struct WorkBuddyAiUsageRow {
    session_id: String,
    used: i64,
    updated_at: i64,
    model: Option<String>,
    cwd: Option<String>,
}

/// Mirror of `parse_workbuddy_sqlite` in `sessions/workbuddy.rs` reporting
/// under this client's id. Copied rather than parameterized so upstream syncs
/// never touch that file.
fn parse_workbuddy_ai_sqlite(db_path: &Path) -> Vec<UnifiedMessage> {
    let query = r#"
        SELECT
            su.session_id,
            su.used,
            su.updated_at,
            s.model,
            s.cwd
        FROM session_usage su
        LEFT JOIN sessions s ON s.id = su.session_id
        WHERE su.used IS NOT NULL
          AND su.used > 0
          AND su.updated_at IS NOT NULL
          AND su.updated_at > 0
        "#;

    let mut messages = Vec::new();
    sqlite_for_each_row(db_path, query, Some("WorkBuddy AI usage"), &mut |row| {
        messages.push(usage_row_to_message(WorkBuddyAiUsageRow {
            session_id: row.get(0)?,
            used: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
            updated_at: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            model: row.get(3)?,
            cwd: row.get(4)?,
        }));
        Ok(())
    });

    messages
}

fn usage_row_to_message(row: WorkBuddyAiUsageRow) -> UnifiedMessage {
    let model_id = row
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or(DEFAULT_MODEL)
        .to_string();
    let provider_id = crate::provider_identity::inferred_provider_from_model(&model_id)
        .unwrap_or(CLIENT_ID)
        .to_string();

    let mut message = UnifiedMessage::new(
        CLIENT_ID,
        model_id,
        provider_id,
        row.session_id.clone(),
        normalize_timestamp_ms(row.updated_at),
        TokenBreakdown {
            input: row.used.max(0),
            output: 0,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: 0,
            reasoning: 0,
        },
        0.0,
    );
    // Include `updated_at` so distinct usage rows for the same session (e.g.
    // per-date or incremental writes) are not collapsed by the dedup key.
    message.dedup_key = Some(format!(
        "{}:{}:{}",
        CLIENT_ID, row.session_id, row.updated_at
    ));

    if let Some(workspace_key) = row.cwd.as_deref().and_then(normalize_workspace_key) {
        let workspace_label = workspace_label_from_key(&workspace_key);
        message.set_workspace(Some(workspace_key), workspace_label);
    }

    message
}

fn normalize_timestamp_ms(timestamp: i64) -> i64 {
    if timestamp > 10_000_000_000 {
        timestamp
    } else {
        timestamp.saturating_mul(1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    const USAGE_LINE: &str = r#"{"id":"assistant-1","timestamp":1780000000100,"type":"message","role":"assistant","status":"completed","sessionId":"session-1","cwd":"/Users/alice/repo","providerData":{"model":"gpt-5.6","messageId":"msg-1"},"message":{"usage":{"prompt_tokens":24486,"completion_tokens":3}}}"#;

    fn write_session_jsonl(home: &Path, session_file: &str) -> PathBuf {
        let project_dir = home.join(HOME_DIR).join("projects").join("c-Users-alice-repo");
        std::fs::create_dir_all(&project_dir).unwrap();
        let path = project_dir.join(session_file);
        std::fs::write(&path, USAGE_LINE).unwrap();
        path
    }

    fn create_ai_db(home: &Path) -> PathBuf {
        let db_path = home.join(HOME_DIR).join("workbuddy.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                cwd TEXT,
                model TEXT
            );
            CREATE TABLE session_usage (
                session_id TEXT PRIMARY KEY,
                used INTEGER,
                size INTEGER,
                updated_at INTEGER,
                credit_json TEXT
            );
            "#,
        )
        .unwrap();
        drop(conn);
        db_path
    }

    #[test]
    fn parse_reads_jsonl_under_the_ai_client() {
        let dir = tempfile::tempdir().unwrap();
        write_session_jsonl(dir.path(), "session-1.jsonl");

        let messages = parse(dir.path().to_str().unwrap());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].client, "workbuddy-ai");
        assert_eq!(messages[0].model_id, "gpt-5.6");
        assert_eq!(messages[0].session_id, "session-1");
        assert_eq!(messages[0].dedup_key.as_deref(), Some("workbuddy-ai:session-1:msg-1"));
    }

    #[test]
    fn parse_reads_a_nested_subagent_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join(HOME_DIR).join("projects").join("c-Users-alice-repo");
        let subagent_dir = project_dir.join("session-1").join("subagents");
        std::fs::create_dir_all(&subagent_dir).unwrap();
        std::fs::write(subagent_dir.join("agent-1.jsonl"), USAGE_LINE).unwrap();

        let messages = parse(dir.path().to_str().unwrap());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].client, "workbuddy-ai");
    }

    #[test]
    fn sqlite_fallback_covers_only_sessions_without_detailed_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        write_session_jsonl(dir.path(), "session-1.jsonl");
        let db_path = create_ai_db(dir.path());
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO sessions (id, cwd, model) VALUES (?1, ?2, ?3)",
            params!["session-1", "/Users/alice/repo", "gpt-5.6"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, cwd, model) VALUES (?1, ?2, ?3)",
            params!["session-2", "/Users/alice/repo", "gpt-6"],
        )
        .unwrap();
        for (session, used) in [("session-1", 24486_i64), ("session-2", 500_i64)] {
            conn.execute(
                "INSERT INTO session_usage (session_id, used, size, updated_at, credit_json) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session, used, 1_000_000_i64, 1_780_000_000_000_i64, "{}"],
            )
            .unwrap();
        }
        drop(conn);

        let messages = parse(dir.path().to_str().unwrap());

        // session-1 keeps its detailed JSONL row; session-2 is fallback-only.
        assert_eq!(messages.len(), 2);
        let detailed = messages
            .iter()
            .find(|message| message.session_id == "session-1")
            .unwrap();
        assert_eq!(detailed.dedup_key.as_deref(), Some("workbuddy-ai:session-1:msg-1"));
        let fallback = messages
            .iter()
            .find(|message| message.session_id == "session-2")
            .unwrap();
        assert_eq!(fallback.model_id, "gpt-6");
        assert_eq!(fallback.tokens.input, 500);
    }

    #[test]
    fn parse_returns_empty_for_a_missing_home() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("empty-home");
        assert!(parse(missing.to_str().unwrap()).is_empty());
    }
}
