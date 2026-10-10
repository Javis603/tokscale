mod common;

use std::fs;
use std::path::Path;

use tokscale_core::scanner::ScannerSettings;
use tokscale_core::{parse_local_unified_messages_with_pricing_uncached, LocalParseOptions};

const PARENT_THREAD: &str = "01a00000-0000-7000-8000-00000000aaaa";
const ORIGINAL: &str = "rollout-2026-01-15T12-00-00-01a00000-0000-7000-8000-00000000aaaa";
const CONTINUATION: &str = "rollout-2026-10-09T12-00-00-01a00000-0000-7000-8000-00000000aaaa_01a12000-0000-7000-8000-00000000bbbb";
const CHILD: &str = "rollout-2026-10-10T12-00-00-01a12100-0000-7000-8000-00000000cccc";

/// One call per file with distinct cumulative totals, so the replay dedup that
/// Codex scopes to the parent thread keeps every call.
fn write_rollout(
    home: &Path,
    day: &str,
    stem: &str,
    meta: &str,
    at: &str,
    total: (i64, i64),
    last: (i64, i64),
) {
    let dir = home.join(".codex/sessions").join(day);
    fs::create_dir_all(&dir).unwrap();
    let turn = format!(
        r#"{{"timestamp":"{at}:01Z","type":"turn_context","payload":{{"model":"gpt-5.2","cwd":"/repo"}}}}"#
    );
    let usage = format!(
        r#"{{"timestamp":"{at}:02Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{},"output_tokens":{}}},"last_token_usage":{{"input_tokens":{},"output_tokens":{}}}}}}}}}"#,
        total.0, total.1, last.0, last.1
    );
    fs::write(
        dir.join(format!("{stem}.jsonl")),
        format!("{meta}\n{turn}\n{usage}\n"),
    )
    .unwrap();
}

fn write_fixture(home: &Path) {
    let parent_meta = |at: &str| {
        format!(
            r#"{{"timestamp":"{at}:00Z","type":"session_meta","payload":{{"id":"{PARENT_THREAD}","source":"vscode","model_provider":"openai","cwd":"/repo"}}}}"#
        )
    };
    write_rollout(
        home,
        "2026/01/15",
        ORIGINAL,
        &parent_meta("2026-01-15T12:00"),
        "2026-01-15T12:00",
        (10, 4),
        (10, 4),
    );
    write_rollout(
        home,
        "2026/10/09",
        CONTINUATION,
        &parent_meta("2026-10-09T12:00"),
        "2026-10-09T12:00",
        (30, 12),
        (20, 8),
    );
    let child_meta = format!(
        r#"{{"timestamp":"2026-10-10T12:00:00Z","type":"session_meta","payload":{{"id":"01a12100-0000-7000-8000-00000000cccc","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{PARENT_THREAD}","depth":1}}}}}},"model_provider":"openai","cwd":"/repo"}}}}"#
    );
    write_rollout(
        home,
        "2026/10/10",
        CHILD,
        &child_meta,
        "2026-10-10T12:00",
        (7, 3),
        (7, 3),
    );
}

fn options(home: &Path, since: Option<&str>) -> LocalParseOptions {
    LocalParseOptions {
        home_dir: Some(home.to_str().unwrap().to_string()),
        use_env_roots: false,
        clients: Some(vec!["codex".to_string()]),
        since: since.map(str::to_string),
        until: None,
        year: None,
        scanner_settings: ScannerSettings::default(),
    }
}

/// The TUI reads messages through this entry point, so its subagent roll-up
/// needs the same guarantee as the report: a parent link names a parent row the
/// date filter kept.
#[tokio::test]
async fn test_codex_parent_link_names_a_parent_row_the_date_filter_kept() {
    let home_dir = common::temp_home();
    let home = home_dir.path();
    write_fixture(home);

    let filtered =
        parse_local_unified_messages_with_pricing_uncached(options(home, Some("2026-10-09")), None)
            .await
            .unwrap();
    assert!(filtered
        .iter()
        .all(|message| message.session_id != ORIGINAL));
    let child = filtered
        .iter()
        .find(|message| message.session_id == CHILD)
        .unwrap();
    assert_eq!(child.parent_session_id.as_deref(), Some(CONTINUATION));
    assert_eq!(
        filtered
            .iter()
            .map(|message| message.tokens.input)
            .sum::<i64>(),
        27
    );

    let all = parse_local_unified_messages_with_pricing_uncached(options(home, None), None)
        .await
        .unwrap();
    let child = all
        .iter()
        .find(|message| message.session_id == CHILD)
        .unwrap();
    assert_eq!(child.parent_session_id.as_deref(), Some(ORIGINAL));
    assert_eq!(
        all.iter().map(|message| message.tokens.input).sum::<i64>(),
        37
    );
}
