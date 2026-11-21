//! The Token Monitor `mcode` supplement in both parse lanes: the runtime store
//! adds Sessions upstream does not read, a turn already counted from a
//! headless capture is not counted again, and the result is the same whether
//! or not the scan names `mcode`.

use std::path::{Path, PathBuf};
use tokscale_core::scanner::scan_all_clients_with_env_strategy;
use tokscale_core::{
    parse_local_clients, parse_local_unified_messages_with_pricing, ClientId, LocalParseOptions,
};

mod common;
use common::EnvGuard;

const SESSION_PATH: &str = "v2/sessions/2026/05/28/20-26-40-000-session_c2Vzc2lvbg";
const SNAPSHOT_PATH: &str = "snapshots/g000000000000--compact.jsonl";

fn options(home: &Path, clients: Option<Vec<String>>) -> LocalParseOptions {
    LocalParseOptions {
        home_dir: Some(home.to_string_lossy().into_owned()),
        use_env_roots: false,
        clients,
        since: None,
        until: None,
        year: None,
        scanner_settings: Default::default(),
    }
}

fn assistant(message_id: &str, turn_id: &str, input: i64) -> String {
    serde_json::json!({
        "message_id": message_id,
        "turn_id": turn_id,
        "message": {
            "role": "assistant",
            "provider": "minimax",
            "model": "MiniMax-M2.7",
            "usage": {"input": input, "output": 250, "cacheRead": 400, "cacheWrite": 50},
            "timestamp": 1780000000000i64
        }
    })
    .to_string()
}

fn write_home(home: &Path) {
    write_home_with_captures(home, &home.join(".config/tokscale/headless/mcode"));
}

fn write_home_with_captures(home: &Path, headless: &Path) {
    let session = home.join(".minimax").join(SESSION_PATH);
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(
        session.join("manifest.json"),
        r#"{"schemaVersion":1,"sessionId":"session","createdAtMs":1780000000000}"#,
    )
    .unwrap();
    std::fs::write(
        session.join("messages.jsonl"),
        [
            assistant("msg-1", "turn-1", 1000),
            assistant("msg-2", "turn-2", 2000),
        ]
        .join("\n"),
    )
    .unwrap();
    // turn-1 was also run through `tokscale headless mcode exec`.
    std::fs::create_dir_all(headless).unwrap();
    std::fs::write(
        headless.join("capture.jsonl"),
        serde_json::json!({
            "type": "exec.completed",
            "sessionId": "session",
            "turnId": "turn-1",
            "timestampMs": 1780000000000i64,
            "result": {
                "model": {"providerId": "minimax", "modelId": "MiniMax-M2.7"},
                "usage": {"inputTokens": 1000, "outputTokens": 250, "cacheReadTokens": 400, "cacheWriteTokens": 50}
            }
        })
        .to_string(),
    )
    .unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn the_store_supplements_headless_captures_once_in_both_lanes() {
    let cache = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&[("TOKSCALE_CONFIG_DIR", cache.path().as_os_str())]);
    let home = tempfile::tempdir().unwrap();
    write_home(home.path());
    let mcode = Some(vec!["mcode".to_string()]);

    let local = parse_local_clients(options(home.path(), mcode.clone())).unwrap();
    assert_eq!(local.messages.len(), 2);
    assert_eq!(local.counts.get(ClientId::Mcode), 2);
    assert_eq!(local.messages.iter().map(|m| m.input).sum::<i64>(), 3000);

    for _ in 0..2 {
        let messages =
            parse_local_unified_messages_with_pricing(options(home.path(), mcode.clone()), None)
                .await
                .unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages.iter().map(|m| m.tokens.input).sum::<i64>(), 3000);
        assert_eq!(messages.iter().map(|m| m.tokens.output).sum::<i64>(), 500);
        assert!(messages.iter().all(|m| m.client == "mcode"));
    }
}

#[tokio::test]
#[serial_test::serial]
async fn an_unfiltered_scan_reads_the_store_too() {
    let cache = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&[("TOKSCALE_CONFIG_DIR", cache.path().as_os_str())]);
    let home = tempfile::tempdir().unwrap();
    write_home(home.path());

    let local = parse_local_clients(options(home.path(), None)).unwrap();
    let mcode: Vec<_> = local
        .messages
        .iter()
        .filter(|m| m.client == "mcode")
        .collect();
    assert_eq!(mcode.len(), 2);
    assert_eq!(local.counts.get(ClientId::Mcode), 2);

    let messages = parse_local_unified_messages_with_pricing(options(home.path(), None), None)
        .await
        .unwrap();
    assert_eq!(messages.iter().filter(|m| m.client == "mcode").count(), 2);
}

#[tokio::test]
#[serial_test::serial]
async fn captures_under_an_extra_scan_path_are_not_counted_again() {
    let cache = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set(&[("TOKSCALE_CONFIG_DIR", cache.path().as_os_str())]);
    let home = tempfile::tempdir().unwrap();
    let captures = home.path().join("elsewhere/mcode-captures");
    write_home_with_captures(home.path(), &captures);
    let mut options = options(home.path(), Some(vec!["mcode".to_string()]));
    options
        .scanner_settings
        .extra_scan_paths
        .insert("mcode".to_string(), vec![captures]);

    let local = parse_local_clients(options.clone()).unwrap();
    assert_eq!(local.messages.len(), 2);
    assert_eq!(local.messages.iter().map(|m| m.input).sum::<i64>(), 3000);

    let messages = parse_local_unified_messages_with_pricing(options, None)
        .await
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages.iter().map(|m| m.tokens.input).sum::<i64>(), 3000);
}

fn write_relocated_store(home: &Path, store: &Path) {
    let session = store.join(SESSION_PATH);
    std::fs::create_dir_all(session.join("snapshots")).unwrap();
    std::fs::copy(
        home.join(".minimax")
            .join(SESSION_PATH)
            .join("manifest.json"),
        session.join("manifest.json"),
    )
    .unwrap();
    std::fs::write(
        session.join(SNAPSHOT_PATH),
        assistant("msg-2", "turn-2", 2000),
    )
    .unwrap();
    std::fs::write(
        session.join("messages.jsonl"),
        [
            assistant("msg-2", "turn-2", 2000),
            assistant("msg-3", "turn-3", 4000),
        ]
        .join("\n"),
    )
    .unwrap();
}

#[tokio::test]
#[serial_test::serial]
async fn relocated_stores_append_usage_once_in_both_parse_lanes() {
    let cache = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let relocated = tempfile::tempdir().unwrap();
    let headless = home.path().join(".config/tokscale/headless");
    let store = relocated.path().join(".minimax");
    write_home(home.path());
    write_relocated_store(home.path(), &store);
    let extra = format!("mcode:{}", store.display());
    let _env = EnvGuard::set(&[
        ("TOKSCALE_CONFIG_DIR", cache.path().as_os_str()),
        ("TOKSCALE_HEADLESS_DIR", headless.as_os_str()),
        ("MINIMAX_DATA_DIR", std::ffi::OsStr::new("")),
        ("MAVIS_DATA_DIR", std::ffi::OsStr::new("")),
        ("TOKSCALE_EXTRA_DIRS", std::ffi::OsStr::new(&extra)),
    ]);
    let mut scan_options = options(home.path(), Some(vec!["mcode".into()]));
    scan_options.use_env_roots = true;

    // Both the home default and an explicit override retain the original
    // store and count the relocated copy of the second turn only once.
    for override_dir in [PathBuf::new(), home.path().join(".minimax")] {
        std::env::set_var("MINIMAX_DATA_DIR", override_dir);
        let local = parse_local_clients(scan_options.clone()).unwrap();
        assert_eq!(local.counts.get(ClientId::Mcode), 3);
        assert_eq!(local.messages.iter().map(|m| m.input).sum::<i64>(), 7000);

        let messages = parse_local_unified_messages_with_pricing(scan_options.clone(), None)
            .await
            .unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages.iter().map(|m| m.tokens.input).sum::<i64>(), 7000);
    }

    // An explicit --home reads only that home's store and captures.
    scan_options.use_env_roots = false;
    let local = parse_local_clients(scan_options.clone()).unwrap();
    assert_eq!(local.messages.iter().map(|m| m.input).sum::<i64>(), 3000);
    let messages = parse_local_unified_messages_with_pricing(scan_options, None)
        .await
        .unwrap();
    assert_eq!(messages.iter().map(|m| m.tokens.input).sum::<i64>(), 3000);
}

#[test]
#[serial_test::serial]
fn extra_stores_keep_runtime_history_out_of_the_headless_scan() {
    let home = tempfile::tempdir().unwrap();
    let relocated = tempfile::tempdir().unwrap();
    let headless = home.path().join(".config/tokscale/headless");
    let store = relocated.path().join(".minimax");
    write_home(home.path());
    write_relocated_store(home.path(), &store);
    let capture = headless.join("mcode/capture.jsonl");
    let session = store.join(SESSION_PATH);
    let extra_captures = relocated.path().join("captures");
    std::fs::create_dir_all(extra_captures.join("snapshots")).unwrap();
    let mut expected = vec![
        store.join("messages.jsonl"),
        session.join("capture.jsonl"),
        extra_captures.join("messages.jsonl"),
        extra_captures.join(SNAPSHOT_PATH),
    ];
    for path in &expected {
        std::fs::copy(&capture, path).unwrap();
    }
    expected.push(capture);
    expected.sort();
    let extra = format!(
        "mcode:{},mcode:{}",
        store.display(),
        extra_captures.display()
    );
    let _env = EnvGuard::set(&[
        ("TOKSCALE_HEADLESS_DIR", headless.as_os_str()),
        ("TOKSCALE_EXTRA_DIRS", std::ffi::OsStr::new(&extra)),
    ]);

    // Both parse lanes consume this scanner result. Runtime history must
    // never enter it, even when repeated scans would return no headless rows.
    for _ in 0..2 {
        let scanned = scan_all_clients_with_env_strategy(
            home.path().to_str().unwrap(),
            &["mcode".into()],
            true,
        );
        let mut files = scanned.get(ClientId::Mcode).clone();
        files.sort();
        assert_eq!(files, expected);
    }
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn symlinked_runtime_history_stays_in_the_store_lane() {
    let cache = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let relocated = tempfile::tempdir().unwrap();
    let headless = home.path().join(".config/tokscale/headless");
    let store = relocated.path().join(".minimax");
    write_home(home.path());
    write_relocated_store(home.path(), &store);
    let active = store.join(SESSION_PATH).join("messages.jsonl");
    let external_history = relocated.path().join("external-history.jsonl");
    std::fs::rename(&active, &external_history).unwrap();
    std::os::unix::fs::symlink(&external_history, &active).unwrap();
    let extra = format!("mcode:{}", store.display());
    let _env = EnvGuard::set(&[
        ("TOKSCALE_CONFIG_DIR", cache.path().as_os_str()),
        ("TOKSCALE_HEADLESS_DIR", headless.as_os_str()),
        ("MINIMAX_DATA_DIR", std::ffi::OsStr::new("")),
        ("MAVIS_DATA_DIR", std::ffi::OsStr::new("")),
        ("TOKSCALE_EXTRA_DIRS", std::ffi::OsStr::new(&extra)),
    ]);
    let clients = vec!["mcode".into()];
    let result = scan_all_clients_with_env_strategy(home.path().to_str().unwrap(), &clients, true);
    assert_eq!(
        result.get(ClientId::Mcode),
        &[headless.join("mcode/capture.jsonl")]
    );
    let mut scan_options = options(home.path(), Some(clients));
    scan_options.use_env_roots = true;
    let messages = parse_local_unified_messages_with_pricing(scan_options, None)
        .await
        .unwrap();
    assert_eq!(messages.iter().map(|m| m.tokens.input).sum::<i64>(), 7000);
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn explicitly_scanned_symlinked_session_keeps_its_capture() {
    let home = tempfile::tempdir().unwrap();
    write_home(home.path());
    let store = home.path().join(".minimax");
    let linked_session = store.join(SESSION_PATH).with_file_name("linked-session");
    let capture_dir = home.path().join("captures");
    std::fs::create_dir(&capture_dir).unwrap();
    let capture = std::fs::read_to_string(
        home.path()
            .join(".config/tokscale/headless/mcode/capture.jsonl"),
    )
    .unwrap();
    std::fs::write(
        capture_dir.join("messages.jsonl"),
        capture.replace("turn-1", "linked-turn"),
    )
    .unwrap();
    std::os::unix::fs::symlink(&capture_dir, &linked_session).unwrap();
    let extra = format!("mcode:{}", linked_session.display());
    let _env = EnvGuard::set(&[
        (
            "TOKSCALE_CONFIG_DIR",
            home.path().join("config").as_os_str(),
        ),
        (
            "TOKSCALE_HEADLESS_DIR",
            home.path().join("empty-headless").as_os_str(),
        ),
        ("MINIMAX_DATA_DIR", store.as_os_str()),
        ("TOKSCALE_EXTRA_DIRS", std::ffi::OsStr::new(&extra)),
    ]);

    let mut scan_options = options(home.path(), Some(vec!["mcode".into()]));
    scan_options.use_env_roots = true;
    let local = parse_local_clients(scan_options).unwrap();
    assert_eq!(local.messages.iter().map(|m| m.input).sum::<i64>(), 4000);
}
