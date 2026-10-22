//! CodeArts (Huawei Cloud) local usage, both stores:
//! `~/.codeartsdoer/cli-data/opencode.db` (the standalone CLI) and
//! `~/.codeartsdoer/codearts-data/opencode.db` (the IDE extension and the
//! desktop agent, which share one store). Both carry real usage, so the
//! default scans both and merges them under the `codearts` id; a missing
//! store is a valid empty source. `TOKEN_MONITOR_CODEARTS_DB_PATH` overrides
//! both defaults with one explicit database — but only when the scan allows
//! environment roots; an explicit `--home` stays under that home.
//!
//! CodeArts is built on OpenCode's storage: assistant turns live in the same
//! `message` rows (`$.role`, `$.modelID`, `$.tokens`, `$.time` in
//! milliseconds) with the same `session` join (`directory`, `title`), so the
//! parse itself lives in [`crate::sessions::opencode_schema`]; the config
//! below states only where CodeArts departs from OpenCode's own policy. Like
//! every owned client, the database is re-read on each scan — the
//! message-cache lane is keyed on `ClientId`, which owned ids are not — and
//! the store is small enough that this is cheap.

use super::Scope;
use crate::sessions::opencode_schema::{
    parse_opencode_schema_sqlite, OpenCodeSchemaConfig, OPENCODE_QUERY_GROUPS,
};
use crate::sessions::UnifiedMessage;
use std::path::{Path, PathBuf};

pub const CLIENT_ID: &str = "codearts";

/// The stores scanned by default, relative to `~/.codeartsdoer`. The CLI
/// writes `cli-data`; the IDE extension and the desktop agent share
/// `codearts-data`.
const DEFAULT_STORES: [&str; 2] = ["cli-data", "codearts-data"];

pub(crate) const fn config() -> OpenCodeSchemaConfig {
    OpenCodeSchemaConfig {
        query_groups: OPENCODE_QUERY_GROUPS,
        // Both OpenCode query groups are probed so a future CodeArts build
        // that moves to v2 tables keeps working without a parser change.
        dual_schema: true,
        // An assistant payload may omit `cache` (or its read/write); a strict
        // requirement would drop the message.
        strict_cache: false,
        // Epoch seconds are scaled like MiMo's, because the store is young
        // enough that no build has committed to one unit.
        normalize_epoch_seconds: true,
        // The stores are re-scanned in full on every tick, so no incremental
        // state is carried across scans.
        ..OpenCodeSchemaConfig::base("codearts")
    }
}

pub fn parse(scope: &Scope) -> Vec<UnifiedMessage> {
    // An explicit override replaces both defaults entirely — it names one
    // database, not one more store to add. The shared environment-root
    // policy applies: an explicit `--home` never collects another profile.
    if scope.use_env_roots {
        if let Some(overridden) = env_path("TOKEN_MONITOR_CODEARTS_DB_PATH") {
            return parse_db(&overridden);
        }
    }
    let home = Path::new(scope.home_dir);
    let mut messages = Vec::new();
    for store in DEFAULT_STORES {
        messages.extend(parse_db(
            &home.join(".codeartsdoer").join(store).join("opencode.db"),
        ));
    }
    messages
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
    parse_opencode_schema_sqlite(db_path, config())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_monitor::Counted;
    use rusqlite::{params, Connection};
    use serial_test::serial;
    use tempfile::TempDir;

    const CREATE_SCHEMA: &str = r#"
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
    "#;

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
            params![
                row_id,
                session_id,
                data_json,
                1790871400715_i64,
                1790871409590_i64
            ],
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
        assert_eq!((msg.tokens.input, msg.tokens.output), (10, 5));
        assert_eq!(msg.tokens.cache_read, 0);
        assert_eq!(msg.tokens.cache_write, 0);
    }

    #[test]
    fn zero_cost_stays_unpriced_and_user_rows_are_skipped() {
        let dir = TempDir::new().unwrap();
        let db_path = create_codearts_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        insert_message(
            &conn,
            "msg_3",
            "ses_1",
            &assistant_payload(r#"{"input": 7, "output": 3, "cache": {"write": 0, "read": 0}}"#),
        );
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

    fn scope<'a>(home_dir: &'a str, use_env_roots: bool, counted: &'a Counted) -> Scope<'a> {
        Scope {
            home_dir,
            use_env_roots,
            counted,
        }
    }

    // The default scans BOTH stores the way a real scan does — home in, no
    // DB override: the CLI's cli-data and the IDE/desktop-agent's shared
    // codearts-data. Running parse() itself (not parse_db) keeps a path
    // regression from hiding behind the parse_db() tests that bypass
    // discovery entirely.
    #[test]
    #[serial]
    fn scans_both_default_stores_without_an_override() {
        const VAR: &str = "TOKEN_MONITOR_CODEARTS_DB_PATH";
        let saved = std::env::var(VAR).ok();
        unsafe { std::env::remove_var(VAR) };

        let dir = TempDir::new().unwrap();
        let root = dir.path().join(".codeartsdoer");
        for (store, session, input) in
            [("cli-data", "ses_cli", 12), ("codearts-data", "ses_ide", 7)]
        {
            let store_dir = root.join(store);
            std::fs::create_dir_all(&store_dir).unwrap();
            let conn = Connection::open(store_dir.join("opencode.db")).unwrap();
            conn.execute_batch(CREATE_SCHEMA).unwrap();
            conn.execute_batch(&format!(
                "INSERT INTO session (id, directory, title, time_created, time_updated)
                 VALUES ('{session}', 'D:\\work\\app', 'A session', 1790871300000, 1790871409590);"
            ))
            .unwrap();
            insert_message(
                &conn,
                &format!("msg_{store}"),
                session,
                &assistant_payload(&format!(
                    r#"{{"input": {input}, "output": 4, "cache": {{"write": 0, "read": 0}}}}"#
                )),
            );
        }

        let counted = Counted::default();
        let messages = parse(&scope(dir.path().to_str().unwrap(), false, &counted));

        match saved {
            Some(value) => unsafe { std::env::set_var(VAR, value) },
            None => unsafe { std::env::remove_var(VAR) },
        }

        assert_eq!(messages.len(), 2, "both default stores must contribute");
        assert!(messages.iter().all(|m| m.client == "codearts"));
        let by_session = |sid: &str| {
            messages
                .iter()
                .find(|m| m.session_id == sid)
                .unwrap_or_else(|| panic!("missing session {sid}"))
        };
        assert_eq!(by_session("ses_cli").tokens.input, 12);
        assert_eq!(by_session("ses_ide").tokens.input, 7);
    }

    // The override names ONE database and replaces both defaults entirely —
    // it is for relocated homes and tests, not a third store to add. An
    // explicit home (use_env_roots: false) must ignore the override too,
    // which is the shared environment-root policy this branch enforces.
    #[test]
    #[serial]
    fn the_db_override_replaces_both_default_stores() {
        const VAR: &str = "TOKEN_MONITOR_CODEARTS_DB_PATH";
        let saved = std::env::var(VAR).ok();
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("relocated").join("opencode.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(CREATE_SCHEMA).unwrap();
            conn.execute_batch(
                r#"
                INSERT INTO session (id, directory, title, time_created, time_updated)
                    VALUES ('ses_relocated', 'D:\work\relocated', 'Relocated', 1790871300000, 1790871409590);
                "#,
            )
            .unwrap();
            insert_message(
                &conn,
                "msg_relocated",
                "ses_relocated",
                &assistant_payload(
                    r#"{"input": 5, "output": 2, "cache": {"write": 0, "read": 0}}"#,
                ),
            );
        }
        // Default stores exist too — if the override only extended the
        // default scan instead of replacing it, they would leak in.
        for store in DEFAULT_STORES {
            let store_dir = dir.path().join(".codeartsdoer").join(store);
            std::fs::create_dir_all(&store_dir).unwrap();
            let conn = Connection::open(store_dir.join("opencode.db")).unwrap();
            conn.execute_batch(CREATE_SCHEMA).unwrap();
        }

        unsafe { std::env::set_var(VAR, &db_path) };
        let counted = Counted::default();
        let messages = parse(&scope(dir.path().to_str().unwrap(), true, &counted));
        match saved {
            Some(value) => unsafe { std::env::set_var(VAR, value) },
            None => unsafe { std::env::remove_var(VAR) },
        }

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].session_id, "ses_relocated");
        assert_eq!(messages[0].tokens.input, 5);
    }
}
