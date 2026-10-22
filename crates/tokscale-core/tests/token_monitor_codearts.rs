//! CodeArts follows the caller's source-root policy in both parser lanes.

use std::path::Path;
use tokscale_core::{
    parse_local_clients, parse_local_unified_messages_with_pricing, LocalParseOptions,
};

mod common;
use common::EnvGuard;

fn write_db(path: &Path, session: &str, input: i64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT,
             time_created INTEGER, time_updated INTEGER);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, data TEXT,
             time_created INTEGER, time_updated INTEGER);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session VALUES (?1, '/work/codearts', 'CodeArts fixture', 1790871400715, 1790871400715)",
        [session],
    )
    .unwrap();
    let payload = serde_json::json!({
        "role": "assistant",
        "modelID": "glm-5.3-flash",
        "providerID": "inferhub-provider",
        "tokens": {"input": input, "output": 3},
        "time": {"created": 1790871400715_i64}
    });
    conn.execute(
        "INSERT INTO message VALUES ('msg-1', ?1, ?2, 1790871400715, 1790871400715)",
        rusqlite::params![session, payload.to_string()],
    )
    .unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn source_root_policy_is_respected_in_both_lanes() {
    let home = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let external_db = external.path().join("override.db");
    write_db(
        &home.path().join(".codeartsdoer/codearts-data/opencode.db"),
        "home-session",
        11,
    );
    write_db(&external_db, "external-session", 99);
    let _env = EnvGuard::set(&[
        ("TOKEN_MONITOR_CODEARTS_DB_PATH", external_db.as_os_str()),
        ("TOKSCALE_CONFIG_DIR", cache.path().as_os_str()),
    ]);

    for (use_env_roots, session, input) in
        [(false, "home-session", 11), (true, "external-session", 99)]
    {
        let options = LocalParseOptions {
            home_dir: Some(home.path().to_string_lossy().into_owned()),
            use_env_roots,
            clients: Some(vec!["codearts".to_owned()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: Default::default(),
        };
        let local = parse_local_clients(options.clone()).unwrap();
        assert_eq!(local.messages.len(), 1);
        assert_eq!(local.messages[0].session_id, session);
        assert_eq!(local.messages[0].input, input);

        let streaming = parse_local_unified_messages_with_pricing(options, None)
            .await
            .unwrap();
        assert_eq!(streaming.len(), 1);
        assert_eq!(streaming[0].client, "codearts");
        assert_eq!(streaming[0].session_id, session);
        assert_eq!(streaming[0].tokens.input, input);
    }
}
