//! CodeArts CLI (Huawei Cloud) local usage:
//! `~/.codeartsdoer/codearts-data/opencode.db`.
//!
//! CodeArts is built on OpenCode's storage: assistant turns live in the same
//! `message` rows (`$.role`, `$.modelID`, `$.tokens`, `$.time` in
//! milliseconds) with the same `session` join (`directory`, `title`), so the
//! parse itself lives in [`crate::sessions::opencode_schema`]; only the places
//! where CodeArts's behaviour departs from OpenCode's are declared there, as
//! `OpenCodeSchemaConfig::codearts`. Like every owned client, the database is
//! re-read on each scan — the message-cache lane is keyed on `ClientId`, which
//! owned ids are not — and the store is small enough that this is cheap.

use crate::sessions::opencode_schema::{parse_opencode_schema_sqlite, OpenCodeSchemaConfig};
use crate::sessions::UnifiedMessage;
use std::path::{Path, PathBuf};

pub const CLIENT_ID: &str = "codearts";

const DB_SUFFIX: [&str; 2] = ["codearts-data", "opencode.db"];

pub fn parse(home_dir: &str) -> Vec<UnifiedMessage> {
    parse_db(&db_path(Path::new(home_dir)))
}

fn db_path(home: &Path) -> PathBuf {
    env_path("TOKEN_MONITOR_CODEARTS_DB_PATH").unwrap_or_else(|| {
        DB_SUFFIX
            .iter()
            .fold(home.join(".codeartsdoer"), |path, part| path.join(part))
    })
}

fn env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    std::path::absolute(trimmed).ok()
}

fn parse_db(db_path: &Path) -> Vec<UnifiedMessage> {
    parse_opencode_schema_sqlite(db_path, OpenCodeSchemaConfig::codearts())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};
    use tempfile::TempDir;

    fn create_codearts_db(dir: &TempDir) -> PathBuf {
        let db_path = dir.path().join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                directory TEXT,
                title TEXT,
                time_created INTEGER,
                time_updated INTEGER
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                data TEXT NOT NULL,
                time_created INTEGER,
                time_updated INTEGER
            );
            INSERT INTO session (id, directory, title, time_created, time_updated)
                VALUES ('ses_1', 'D:\work\app', 'Fix the login flow', 1790871300000, 1790871409590);
            "#,
        )
        .unwrap();
        db_path
    }

    fn insert_message(conn: &Connection, row_id: &str, session_id: &str, data_json: &str) {
        conn.execute(
            "INSERT INTO message (id, session_id, data, time_created, time_updated)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![row_id, session_id, data_json, 1790871400715_i64, 1790871409590_i64],
        )
        .unwrap();
    }

    fn assistant_payload(tokens_json: &str) -> String {
        format!(
            r#"{{
                "parentID": "msg_parent",
                "role": "assistant",
                "mode": "build",
                "agent": "build",
                "path": {{"cwd": "D:\\work\\app", "root": "/"}},
                "cost": 0,
                "tokens": {tokens_json},
                "modelID": "glm-5.3-flash",
                "providerID": "inferhub-provider",
                "time": {{"created": 1790871400715.0, "completed": 1790871409590.0}}
            }}"#
        )
    }

    #[test]
    fn parses_assistant_rows_with_the_full_breakdown() {
        let dir = TempDir::new().unwrap();
        let db_path = create_codearts_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        insert_message(
            &conn,
            "msg_1",
            "ses_1",
            &assistant_payload(
                r#"{"total": 156556, "input": 1908, "output": 174, "reasoning": 106,
                    "cache": {"write": 0, "read": 154368},
                    "context": {"tools": 139810, "mcp": 0, "messages": 2185, "skills": 0,
                                "system_prompts": 14561}}"#,
            ),
        );
        drop(conn);

        let messages = parse_db(&db_path);
        assert_eq!(messages.len(), 1);

        let msg = &messages[0];
        assert_eq!(msg.client, "codearts");
        assert_eq!(msg.session_id, "ses_1");
        assert_eq!(msg.model_id, "glm-5.3-flash");
        // canonical_provider spells provider ids with underscores, as it
        // does for every OpenCode-schema client.
        assert_eq!(msg.provider_id, "inferhub_provider");
        assert_eq!(msg.timestamp, 1_790_871_400_715);
        assert_eq!(msg.tokens.input, 1908);
        assert_eq!(msg.tokens.output, 174);
        assert_eq!(msg.tokens.reasoning, 106);
        assert_eq!(msg.tokens.cache_read, 154368);
        assert_eq!(msg.tokens.cache_write, 0);
        assert_eq!(msg.dedup_key.as_deref(), Some("msg_1"));
        // The session join's `directory` wins; the payload's `path.root` of
        // "/" is only a fallback and must not clear the workspace.
        assert_eq!(msg.workspace_key.as_deref(), Some("D:/work/app"));
    }

    #[test]
    fn a_payload_without_cache_or_reasoning_is_still_accepted() {
        let dir = TempDir::new().unwrap();
        let db_path = create_codearts_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        insert_message(
            &conn,
            "msg_2",
            "ses_1",
            &assistant_payload(r#"{"input": 10, "output": 5}"#),
        );
        drop(conn);

        let messages = parse_db(&db_path);
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(
            (msg.tokens.input, msg.tokens.output),
            (10, 5)
        );
        assert_eq!(msg.tokens.cache_read, 0);
        assert_eq!(msg.tokens.cache_write, 0);
    }

    #[test]
    fn zero_cost_stays_unpriced_and_user_rows_are_skipped() {
        let dir = TempDir::new().unwrap();
        let db_path = create_codearts_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        insert_message(&conn, "msg_3", "ses_1", &assistant_payload(
            r#"{"input": 7, "output": 3, "cache": {"write": 0, "read": 0}}"#,
        ));
        insert_message(
            &conn,
            "msg_user",
            "ses_1",
            r#"{"role": "user", "tokens": {"input": 100, "output": 0,
                "cache": {"write": 0, "read": 0}},
                "modelID": "glm-5.3-flash", "providerID": "inferhub-provider",
                "time": {"created": 1790871400715.0}}"#,
        );
        drop(conn);

        let messages = parse_db(&db_path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].cost, 0.0);
    }

    #[test]
    fn returns_empty_for_missing_db() {
        assert!(parse_db(Path::new("/nonexistent/codearts/opencode.db")).is_empty());
    }
}
