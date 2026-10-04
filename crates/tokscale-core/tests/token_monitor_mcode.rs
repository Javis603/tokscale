//! The Token Monitor `mcode` supplement in both parse lanes: the runtime store
//! adds Sessions upstream does not read, a turn already counted from a
//! headless capture is not counted again, and the result is the same whether
//! or not the scan names `mcode`.

use std::path::Path;
use tokscale_core::{
    parse_local_clients, parse_local_unified_messages_with_pricing, ClientId, LocalParseOptions,
};

mod common;
use common::EnvGuard;

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
    let session = home.join(".minimax/v2/sessions/2026/05/28/20-26-40-000-session_c2Vzc2lvbg");
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
    let headless = home.join(".config/tokscale/headless/mcode");
    std::fs::create_dir_all(&headless).unwrap();
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
