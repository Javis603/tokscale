//! Differential tests use the unsieved pipeline as an oracle, including the
//! public report's totals. Fixtures never depend on the wall clock or network.
use super::*;
use crate::{
    aggregate_model_usage_entries_with_labeler, aggregate_session_metadata, get_model_report,
    workspace_metadata_for_entries, ModelReport, WorkspaceLabeler,
};
use std::path::{Path, PathBuf};

const DAY: &str = "2026-05-02";
const OLD_MS: i64 = 1_777_636_800_000; // 2026-05-01 12:00 UTC
const DAY_MS: i64 = 1_777_723_200_000; // 2026-05-02 12:00 UTC

fn catalog(multiplier: f64) -> HashMap<String, pricing::ModelPricing> {
    [
        "gpt-5.4",
        "claude-3-5-sonnet",
        "Composer 1.5",
        "claude-sonnet-4",
        "claude-opus-4-6",
    ]
    .into_iter()
    .map(|model| {
        (
            model.into(),
            pricing::ModelPricing {
                // Binary fractions make sums exactly comparable across hash-map
                // iteration orders, without concealing errors behind a tolerance.
                input_cost_per_token: Some(multiplier / 1024.0),
                output_cost_per_token: Some(multiplier / 512.0),
                cache_read_input_token_cost: Some(multiplier / 2048.0),
                cache_creation_input_token_cost: Some(multiplier / 1024.0),
                ..Default::default()
            },
        )
    })
    .collect()
}

fn install_catalog(multiplier: f64) -> pricing::PricingService {
    let data = catalog(multiplier);
    pricing::cache::save_cache("pricing-litellm.json", &data).unwrap();
    pricing::PricingService::new(data, HashMap::new())
}

fn options(home: &Path, clients: &[&str]) -> ReportOptions {
    ReportOptions {
        home_dir: Some(home.to_str().unwrap().into()),
        use_env_roots: false,
        clients: Some(clients.iter().map(|client| (*client).into()).collect()),
        since: Some(DAY.into()),
        until: Some(DAY.into()),
        scanner_settings: scanner::ScannerSettings {
            bucket_timezone: Some("UTC".into()),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn full_report(options: &ReportOptions, pricing: &pricing::PricingService) -> ModelReport {
    let messages = parse_all_messages_with_pricing_with_env_strategy_and_window(
        options.home_dir.as_deref().unwrap(),
        options.clients.as_deref().unwrap(),
        Some(pricing),
        options.use_env_roots,
        &options.scanner_settings,
        None,
    );
    let filtered = filter_messages_for_report(messages, options);
    let sessions = if matches!(
        options.group_by,
        GroupBy::Session | GroupBy::ClientSession | GroupBy::ClientWorkspaceSession
    ) {
        aggregate_session_metadata(&filtered)
    } else {
        Vec::new()
    };
    let mut labeler = WorkspaceLabeler::default();
    let entries = aggregate_model_usage_entries_with_labeler(
        filtered,
        &options.group_by,
        options.worktree_rollup,
        &mut labeler,
    );
    let workspaces = workspace_metadata_for_entries(&entries, &mut labeler);
    ModelReport {
        total_input: entries.iter().map(|entry| entry.input).sum(),
        total_output: entries.iter().map(|entry| entry.output).sum(),
        total_cache_read: entries.iter().map(|entry| entry.cache_read).sum(),
        total_cache_write: entries.iter().map(|entry| entry.cache_write).sum(),
        total_messages: entries.iter().map(|entry| entry.message_count).sum(),
        total_cost: entries.iter().map(|entry| entry.cost).sum::<f64>() + 0.0,
        processing_time_ms: 0,
        entries,
        sessions,
        workspaces,
    }
}

fn normalized(report: &ModelReport) -> serde_json::Value {
    let mut value = serde_json::to_value(report).unwrap();
    value.as_object_mut().unwrap().remove("processing_time_ms");
    for array in ["entries", "sessions", "workspaces"] {
        value[array]
            .as_array_mut()
            .unwrap()
            .sort_by_key(|entry| entry.to_string());
    }
    value
}

async fn assert_public_matches_full(
    options: &ReportOptions,
    pricing: &pricing::PricingService,
    context: &str,
) -> ModelReport {
    // Public first: a reference run must not prime the cold path under test.
    let actual = get_model_report(options.clone()).await.unwrap();
    let expected = full_report(options, pricing);
    assert_eq!(normalized(&actual), normalized(&expected), "{context}");
    let repeated = get_model_report(options.clone()).await.unwrap();
    assert_eq!(
        normalized(&actual),
        normalized(&repeated),
        "{context}: repeat is idempotent"
    );
    actual
}

fn raw_cache_bytes() -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    let root = pricing::cache::get_cache_dir().join("source-message-cache-v2");
    walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().strip_prefix(&root).unwrap().to_path_buf(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

fn write(home: &Path, relative: &str, content: &str) -> PathBuf {
    let path = home.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    path
}

fn claude_row(id: &str, date: &str, input: i64) -> String {
    format!(
        r#"{{"type":"assistant","timestamp":"{date}T12:00:00.000Z","requestId":"req_{id}","message":{{"id":"msg_{id}","model":"claude-3-5-sonnet","usage":{{"input_tokens":{input},"output_tokens":16,"cache_read_input_tokens":8,"cache_creation_input_tokens":4}}}}}}"#
    ) + "\n"
}

fn codex_body() -> String {
    let mut body =
        String::from("{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.4\"}}\n");
    // Codex assigns each usage event the previous accepted event timestamp.
    for (n, date) in ["2026-05-01", DAY, DAY, "2026-05-03"]
        .into_iter()
        .enumerate()
    {
        body.push_str(&format!(r#"{{"timestamp":"{date}T12:00:0{n}.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{},"output_tokens":{}}},"last_token_usage":{{"input_tokens":32,"output_tokens":16,"cached_input_tokens":8}}}}}}}}"#, (n + 1) * 32, (n + 1) * 16));
        body.push('\n');
    }
    body
}

const CSV_HEADER: &str = "Date,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost\n";

fn cursor_row(timestamp: &str) -> String {
    format!("\"{timestamp}\",\"Included\",\"Composer 1.5\",\"No\",\"36\",\"32\",\"8\",\"16\",\"60\",\"Included\"\n")
}

fn seed_file_clients(home: &Path) -> (PathBuf, PathBuf) {
    write(home, ".codex/sessions/rollout.jsonl", &codex_body());
    let claude = write(
        home,
        ".claude/projects/proj/session.jsonl",
        &[
            claude_row("old", "2026-05-01", 32),
            claude_row("selected", DAY, 64),
            claude_row("future", "2026-05-03", 128),
        ]
        .concat(),
    );
    let cursor = write(
        home,
        ".config/tokscale/cursor-cache/usage.csv",
        &[
            CSV_HEADER.into(),
            cursor_row("2026-05-01T12:00:00.000Z"),
            cursor_row("2026-05-02T12:00:00.000Z"),
            cursor_row("2026-05-03T12:00:00.000Z"),
        ]
        .concat(),
    );
    (claude, cursor)
}

fn seed_sqlite_and_openclaw(home: &Path) -> PathBuf {
    let db = home.join(".local/share/opencode/opencode.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = create_opencode_sqlite_db(&db);
    for (id, timestamp, cost) in [
        ("old", OLD_MS, 0.0),
        ("selected", DAY_MS, 0.0),
        ("authoritative", DAY_MS + 1000, 0.375),
    ] {
        conn.execute(
            "INSERT INTO message VALUES (?1, 'session', ?2)",
            rusqlite::params![
                id,
                build_opencode_sqlite_payload(
                    timestamp as f64,
                    (timestamp + 500) as f64,
                    32,
                    16,
                    4,
                    8,
                    4,
                    cost
                )
            ],
        )
        .unwrap();
    }
    drop(conn);

    let hermes = home.join(".hermes/state.db");
    std::fs::create_dir_all(hermes.parent().unwrap()).unwrap();
    let conn = create_hermes_sqlite_db(&hermes);
    insert_hermes_session(&conn, "selected", "claude-sonnet-4", 2, 32, 16, 1.25);
    conn.execute(
        "UPDATE sessions SET started_at = ?1",
        [DAY_MS as f64 / 1000.0],
    )
    .unwrap();
    drop(conn);

    // Both stores hold identical events: this catches window filtering that
    // accidentally removes dedup participants before the OpenClaw reducer.
    let events = vec![
        sessions::openclaw::test_fixtures::header_event("claw"),
        openclaw_assistant_event("old", 32, 16, OLD_MS),
        openclaw_assistant_event("selected", 64, 32, DAY_MS),
    ];
    write(
        home,
        ".openclaw/agents/main/sessions/claw.jsonl",
        &(events.join("\n") + "\n"),
    );
    seed_openclaw_agent_db(home, "main", "claw", None, &events);
    db
}

#[tokio::test]
#[serial_test::serial]
async fn public_reports_match_full_pricing_for_each_lane_and_every_grouping() {
    let cache_home = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let _cache_env = redirect_cache_home(cache_home.path());
    let mut network = paths::test_env::EnvGuard::capture(&["TOKSCALE_PRICING_CACHE_ONLY"]);
    network.set("TOKSCALE_PRICING_CACHE_ONLY", "1");
    seed_file_clients(home.path());
    seed_sqlite_and_openclaw(home.path());
    let pricing = install_catalog(1.0);
    let clients = [
        "codex", "claude", "cursor", "opencode", "hermes", "openclaw",
    ];
    for client in clients {
        let report =
            assert_public_matches_full(&options(home.path(), &[client]), &pricing, client).await;
        assert!(
            report.total_messages > 0,
            "{client}: fixture must exercise the selected date"
        );
        assert!(
            report.total_cost > 0.0,
            "{client}: selected usage must be priced"
        );
        if client == "openclaw" {
            assert_eq!(
                report.total_messages, 1,
                "SQLite/JSON migration overlap counted once"
            );
        }
        if client == "hermes" {
            assert_eq!(
                report.total_cost, 1.25,
                "authoritative SQLite cost stays authoritative"
            );
        }
    }
    for grouping in [
        GroupBy::Model,
        GroupBy::ClientModel,
        GroupBy::ClientProviderModel,
        GroupBy::WorkspaceModel,
        GroupBy::Session,
        GroupBy::ClientSession,
        GroupBy::ClientWorkspaceSession,
    ] {
        let mut selected = options(home.path(), &clients);
        selected.group_by = grouping;
        assert_public_matches_full(&selected, &pricing, "all-client grouped report").await;
    }
    let selected = options(home.path(), &clients);
    let before = get_model_report(selected.clone()).await.unwrap();
    let raw_before = raw_cache_bytes();
    assert!(!raw_before.is_empty(), "exercise persisted cache shards");
    let new_pricing = install_catalog(2.0);
    let after = assert_public_matches_full(
        &selected,
        &new_pricing,
        "pricing catalog changed on warm cache",
    )
    .await;
    assert_eq!(after.total_input, before.total_input);
    assert_eq!(after.total_messages, before.total_messages);
    assert!(
        after.total_cost > before.total_cost,
        "warm estimates use the new catalog"
    );
    assert_eq!(
        get_model_report(options(home.path(), &["hermes"]))
            .await
            .unwrap()
            .total_cost,
        1.25
    );
    let opencode = get_model_report(options(home.path(), &["opencode"]))
        .await
        .unwrap();
    assert_eq!(opencode.total_messages, 2);
    assert!(
        opencode.total_cost >= 0.375,
        "embedded charge remains present"
    );
    assert_eq!(
        raw_cache_bytes(),
        raw_before,
        "warm catalog changes cannot rewrite raw source cache"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn public_window_sequence_survives_append_compaction_rewrite_and_delete() {
    let cache_home = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let _cache_env = redirect_cache_home(cache_home.path());
    let mut network = paths::test_env::EnvGuard::capture(&["TOKSCALE_PRICING_CACHE_ONLY"]);
    network.set("TOKSCALE_PRICING_CACHE_ONLY", "1");
    let (claude, cursor) = seed_file_clients(home.path());
    let pricing = install_catalog(1.0);
    let selected = options(home.path(), &["codex", "claude", "cursor"]);
    let narrow = assert_public_matches_full(&selected, &pricing, "cold narrow").await;
    let mut wide = selected.clone();
    wide.since = None;
    wide.until = None;
    wide.year = Some("2026".into());
    let full = assert_public_matches_full(&wide, &pricing, "wide after narrow").await;
    assert!(full.total_messages > narrow.total_messages);
    let again = assert_public_matches_full(&selected, &pricing, "narrow after wide").await;
    assert_eq!(normalized(&narrow), normalized(&again));

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&claude)
        .unwrap();
    file.write_all(claude_row("appended", DAY, 256).as_bytes())
        .unwrap();
    drop(file);
    let appended = assert_public_matches_full(&selected, &pricing, "append").await;
    assert_eq!(appended.total_messages, narrow.total_messages + 1);

    std::fs::write(&claude, claude_row("appended", DAY, 256)).unwrap();
    let compacted =
        assert_public_matches_full(&selected, &pricing, "Claude compacted transcript").await;
    assert_eq!(
        normalized(&appended),
        normalized(&compacted),
        "stable historical turns retained after compaction"
    );
    assert_public_matches_full(&wide, &pricing, "compacted wide reload").await;
    assert_eq!(
        normalized(&compacted),
        normalized(
            &assert_public_matches_full(&selected, &pricing, "compacted narrow reload").await
        )
    );

    std::fs::write(&cursor, CSV_HEADER).unwrap();
    let rewritten =
        assert_public_matches_full(&selected, &pricing, "CSV replaced by empty export").await;
    assert_eq!(
        rewritten.total_messages,
        compacted.total_messages - 1,
        "CSV loader uses the live export"
    );
    std::fs::remove_file(&claude).unwrap();
    std::fs::remove_file(&cursor).unwrap();
    let deleted =
        assert_public_matches_full(&selected, &pricing, "deleted transcript and export").await;
    assert_eq!(
        deleted.total_messages,
        rewritten.total_messages - 2,
        "deleted Claude file cannot resurrect retained turns"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn missing_catalog_recovers_on_warm_cache_and_authoritative_zero_stays_zero() {
    let cache_home = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let _cache_env = redirect_cache_home(cache_home.path());
    let mut network = paths::test_env::EnvGuard::capture(&["TOKSCALE_PRICING_CACHE_ONLY"]);
    network.set("TOKSCALE_PRICING_CACHE_ONLY", "1");
    seed_file_clients(home.path());
    write(
        home.path(),
        ".gjc/agent/sessions/project/session.jsonl",
        &format!(
            "{{\"type\":\"session\",\"id\":\"gjc-window\",\"cwd\":\"/work/project\"}}\n{}\n{}\n",
            format_args!(
                r#"{{"type":"message","id":"zero","message":{{"role":"assistant","model":"gpt-5.4","provider":"openai","timestamp":{DAY_MS},"usage":{{"input":32,"output":16,"cost":{{"total":0.0}}}}}}}}"#
            ),
            format_args!(
                r#"{{"type":"message","id":"missing","message":{{"role":"assistant","model":"gpt-5.4","provider":"openai","timestamp":{},"usage":{{"input":32,"output":16}}}}}}"#,
                DAY_MS + 1000
            ),
        ),
    );
    let selected = options(home.path(), &["cursor", "gjc"]);
    let missing = get_model_report(selected.clone()).await.unwrap();
    assert_eq!(missing.total_messages, 3);
    assert_eq!(missing.total_cost, 0.0);
    let raw_before = raw_cache_bytes();
    let pricing = install_catalog(1.0);
    let recovered =
        assert_public_matches_full(&selected, &pricing, "offline catalog becomes available").await;
    assert_eq!(recovered.total_messages, missing.total_messages);
    assert_eq!(recovered.total_input, missing.total_input);
    assert!(recovered.total_cost > 0.0);
    let raw = parse_all_messages_with_pricing_with_env_strategy_and_window(
        selected.home_dir.as_deref().unwrap(),
        selected.clients.as_deref().unwrap(),
        Some(&pricing),
        false,
        &selected.scanner_settings,
        Some(&selected),
    );
    let zero = raw
        .iter()
        .find(|message| {
            message.client == "gjc" && message.cost_source == sessions::CostSource::ProviderReported
        })
        .unwrap();
    assert_eq!(
        zero.cost, 0.0,
        "reported free usage never acquires an estimate"
    );
    assert_eq!(
        raw_cache_bytes(),
        raw_before,
        "catalog availability cannot change persisted raw messages"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn sqlite_updates_and_deletions_preserve_window_report_consistency() {
    let cache_home = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let _cache_env = redirect_cache_home(cache_home.path());
    let mut network = paths::test_env::EnvGuard::capture(&["TOKSCALE_PRICING_CACHE_ONLY"]);
    network.set("TOKSCALE_PRICING_CACHE_ONLY", "1");
    let opencode_db = seed_sqlite_and_openclaw(home.path());
    // No JSON original can hide a SQLite deletion by supplying the same row.
    std::fs::remove_file(
        home.path()
            .join(".openclaw/agents/main/sessions/claw.jsonl"),
    )
    .unwrap();
    let pricing = install_catalog(1.0);
    let selected = options(home.path(), &["opencode", "hermes", "openclaw"]);
    let before = assert_public_matches_full(&selected, &pricing, "SQLite cold and warm").await;

    let conn = rusqlite::Connection::open(&opencode_db).unwrap();
    conn.execute(
        "UPDATE message SET data = ?1 WHERE id = 'selected'",
        [build_opencode_sqlite_payload(
            DAY_MS as f64,
            (DAY_MS + 500) as f64,
            128,
            16,
            4,
            8,
            4,
            0.0,
        )],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message VALUES ('append', 'session', ?1)",
        [build_opencode_sqlite_payload(
            (DAY_MS + 2000) as f64,
            (DAY_MS + 2500) as f64,
            32,
            16,
            4,
            8,
            4,
            0.0,
        )],
    )
    .unwrap();
    drop(conn);

    let claw = home
        .path()
        .join(".openclaw/agents/main/agent/openclaw-agent.sqlite");
    let conn = rusqlite::Connection::open(&claw).unwrap();
    sessions::openclaw::test_fixtures::insert_event(
        &conn,
        "claw",
        3,
        &openclaw_assistant_event("appended", 32, 16, DAY_MS + 2000),
        DAY_MS + 2000,
    );
    drop(conn);

    let hermes = home.path().join(".hermes/state.db");
    let conn = rusqlite::Connection::open(&hermes).unwrap();
    conn.execute(
        "UPDATE sessions SET input_tokens = 64, actual_cost_usd = 2.5",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE session_model_usage SET input_tokens = 64, actual_cost_usd = 2.5",
        [],
    )
    .unwrap();
    drop(conn);

    let changed =
        assert_public_matches_full(&selected, &pricing, "SQLite append and in-place update").await;
    assert_eq!(changed.total_messages, before.total_messages + 2);
    assert!(changed.total_input > before.total_input);
    assert!(changed.total_cost > before.total_cost);
    assert_eq!(
        get_model_report(options(home.path(), &["hermes"]))
            .await
            .unwrap()
            .total_cost,
        2.5
    );

    let conn = rusqlite::Connection::open(&opencode_db).unwrap();
    conn.execute("DELETE FROM message WHERE id = 'selected'", [])
        .unwrap();
    drop(conn);
    let conn = rusqlite::Connection::open(&claw).unwrap();
    conn.execute(
        "DELETE FROM transcript_events WHERE session_id = 'claw' AND seq = 2",
        [],
    )
    .unwrap();
    drop(conn);
    let deleted = assert_public_matches_full(&selected, &pricing, "SQLite row deletion").await;
    assert_eq!(
        deleted.total_messages,
        changed.total_messages - 2,
        "SQLite caches cannot resurrect deleted rows"
    );
    let mut wide = selected.clone();
    wide.since = None;
    wide.until = None;
    assert_public_matches_full(&wide, &pricing, "SQLite wide after mutation").await;
    assert_eq!(
        normalized(&deleted),
        normalized(
            &assert_public_matches_full(&selected, &pricing, "SQLite narrow after wide").await
        )
    );
}

#[test]
#[serial_test::serial]
fn effective_scan_timezone_controls_window_estimation_at_date_boundaries() {
    let cache_home = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let _cache_env = redirect_cache_home(cache_home.path());
    write(
        home.path(),
        ".config/tokscale/cursor-cache/usage.csv",
        &[
            CSV_HEADER.into(),
            cursor_row("2026-05-01T23:59:59.000Z"),
            cursor_row("2026-05-02T00:00:00.000Z"),
            cursor_row("2026-05-02T23:59:59.000Z"),
        ]
        .concat(),
    );
    let pricing = install_catalog(1.0);
    let mut selected = options(home.path(), &["cursor"]);
    full_report(&selected, &pricing); // seed raw cache
    for (timezone, expected_count) in [("UTC", 2), ("Asia/Shanghai", 2), ("America/Los_Angeles", 1)]
    {
        selected.scanner_settings.bucket_timezone = Some(timezone.into());
        let messages = parse_all_messages_with_pricing_with_env_strategy_and_window(
            selected.home_dir.as_deref().unwrap(),
            selected.clients.as_deref().unwrap(),
            Some(&pricing),
            false,
            &selected.scanner_settings,
            Some(&selected),
        );
        let filtered = filter_messages_for_report(messages, &selected);
        assert_eq!(
            filtered.len(),
            expected_count,
            "{timezone}: final date boundary"
        );
        assert!(filtered.iter().all(|message| message.cost > 0.0));
        let actual = aggregate_model_usage_entries_with_rollup(
            filtered,
            &selected.group_by,
            selected.worktree_rollup,
        );
        assert_report_equivalence(&actual, &full_report(&selected, &pricing).entries, timezone);
    }

    let authoritative_settings = scanner::ScannerSettings {
        bucket_timezone: Some("UTC".into()),
        ..Default::default()
    };
    selected.scanner_settings.bucket_timezone = Some("Asia/Shanghai".into());
    let mut reference_options = selected.clone();
    reference_options.scanner_settings = authoritative_settings.clone();
    let actual = parse_all_messages_with_pricing_with_env_strategy_and_window(
        selected.home_dir.as_deref().unwrap(),
        selected.clients.as_deref().unwrap(),
        Some(&pricing),
        false,
        &authoritative_settings,
        Some(&selected),
    );
    let actual = aggregate_model_usage_entries_with_rollup(
        filter_messages_for_report(actual, &reference_options),
        &selected.group_by,
        selected.worktree_rollup,
    );
    assert_report_equivalence(
        &actual,
        &full_report(&reference_options, &pricing).entries,
        "scanner settings take precedence over report metadata",
    );
}

#[tokio::test]
#[serial_test::serial]
async fn codex_priority_pricing_survives_cold_warm_and_window_sequences() {
    let cache_home = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let _cache_env = redirect_cache_home(cache_home.path());
    let mut network = paths::test_env::EnvGuard::capture(&["TOKSCALE_PRICING_CACHE_ONLY"]);
    network.set("TOKSCALE_PRICING_CACHE_ONLY", "1");
    let standard = codex_body();
    let priority = standard.replace(
        "\"model\":\"gpt-5.4\"",
        "\"model\":\"gpt-5.4\",\"service_tier\":\"priority\"",
    );
    write(home.path(), ".codex/sessions/standard.jsonl", &standard);
    write(home.path(), ".codex/sessions/priority.jsonl", &priority);
    let pricing = install_catalog(1.0);
    let mut selected = options(home.path(), &["codex"]);
    selected.group_by = GroupBy::ClientWorkspaceSession;
    let report =
        assert_public_matches_full(&selected, &pricing, "Codex priority cold and warm").await;
    assert_eq!(report.entries.len(), 2);
    let mut costs: Vec<f64> = report.entries.iter().map(|entry| entry.cost).collect();
    costs.sort_by(f64::total_cmp);
    assert!(costs[0] > 0.0);
    assert_eq!(
        costs[1],
        costs[0] * 2.0,
        "OpenAI priority cost premium preserved"
    );
    assert_eq!(
        report.sessions.len(),
        2,
        "fork session metadata remains available"
    );
    let raw_before = raw_cache_bytes();
    let mut wide = selected.clone();
    wide.since = None;
    wide.until = None;
    assert_public_matches_full(&wide, &pricing, "priority wide after narrow").await;
    assert_eq!(
        normalized(&report),
        normalized(
            &assert_public_matches_full(&selected, &pricing, "priority narrow after wide").await
        )
    );
    assert_eq!(
        raw_cache_bytes(),
        raw_before,
        "priority tier remains in raw cache without query mutation"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn recovery_archive_and_daily_floors_are_consistent_across_windows_and_catalogs() {
    let cache_home = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let _cache_env = redirect_cache_home(cache_home.path());
    let mut network = paths::test_env::EnvGuard::capture(&[
        "TOKSCALE_PRICING_CACHE_ONLY",
        "TOKSCALE_RECOVERY_DISABLE",
    ]);
    network.set("TOKSCALE_PRICING_CACHE_ONLY", "1");
    network.remove("TOKSCALE_RECOVERY_DISABLE");
    write(home.path(), ".codex/sessions/rollout.jsonl", &codex_body());
    let mut archive = UnifiedMessage::new(
        "codex",
        "gpt-5.4",
        "openai",
        "missing-session",
        DAY_MS,
        TokenBreakdown {
            input: 16,
            ..Default::default()
        },
        0.25,
    );
    archive.dedup_key = Some("archived-request".into());
    archive.cost_source = sessions::CostSource::ProviderReported;
    let floor = |timestamp, input, cost, message_count| {
        let mut row = UnifiedMessage::new(
            "codex",
            "gpt-5.4",
            "openai",
            "floor",
            timestamp,
            TokenBreakdown {
                input,
                ..Default::default()
            },
            cost,
        );
        row.message_count = message_count;
        row
    };
    let ledger = serde_json::json!({
        "version": 1,
        "home": home.path().to_str().unwrap(),
        "messages": [archive],
        "daily_floors": [floor(OLD_MS, 1024, 8.0, 8), floor(DAY_MS, 512, 4.0, 4)],
    });
    std::fs::create_dir_all(crate::recovery::path().parent().unwrap()).unwrap();
    std::fs::write(
        crate::recovery::path(),
        serde_json::to_vec(&ledger).unwrap(),
    )
    .unwrap();
    let pricing = install_catalog(1.0);
    let mut selected = options(home.path(), &["codex"]);
    selected.group_by = GroupBy::ClientWorkspaceSession;
    let report = assert_public_matches_full(&selected, &pricing, "recovery selected date").await;
    let tokens: i64 = report
        .entries
        .iter()
        .map(|entry| {
            entry.input + entry.output + entry.cache_read + entry.cache_write + entry.reasoning
        })
        .sum();
    assert_eq!(
        tokens, 512,
        "native + archive + daily gap totals match selected floor"
    );
    assert_eq!(report.total_messages, 4);
    assert_eq!(report.total_cost, 4.0);
    assert!(report
        .sessions
        .iter()
        .any(|session| session.session_id == "missing-session"));
    let raw_before = raw_cache_bytes();
    let mut wide = selected.clone();
    wide.since = None;
    wide.until = None;
    let wide_report =
        assert_public_matches_full(&wide, &pricing, "recovery historical + selected dates").await;
    assert_eq!(wide_report.total_cost, 12.0);
    assert_eq!(wide_report.total_messages, 12);
    assert_eq!(
        normalized(&report),
        normalized(
            &assert_public_matches_full(&selected, &pricing, "recovery narrow after wide").await
        )
    );
    let changed_pricing = install_catalog(2.0);
    let changed = assert_public_matches_full(
        &selected,
        &changed_pricing,
        "recovery floor after catalog update",
    )
    .await;
    assert_eq!(
        changed.total_cost, 4.0,
        "larger native estimate reduces recovery gap cost"
    );
    assert_eq!(changed.total_messages, report.total_messages);
    assert_eq!(
        raw_cache_bytes(),
        raw_before,
        "overlay cannot persist into source cache"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn codex_fallback_mtime_repair_precedes_warm_window_pricing() {
    let cache_home = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let _cache_env = redirect_cache_home(cache_home.path());
    let mut network = paths::test_env::EnvGuard::capture(&["TOKSCALE_PRICING_CACHE_ONLY"]);
    network.set("TOKSCALE_PRICING_CACHE_ONLY", "1");
    let source = write(
        home.path(),
        ".codex/sessions/fallback.jsonl",
        concat!(
            r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":32,"cached_input_tokens":8,"output_tokens":16},"last_token_usage":{"input_tokens":32,"cached_input_tokens":8,"output_tokens":16}}}}"#,
            "\n",
        ),
    );
    let set_mtime = |milliseconds: i64| {
        std::fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(milliseconds as u64),
            ))
            .unwrap();
    };
    set_mtime(OLD_MS);
    let pricing = install_catalog(1.0);
    let mut selected = options(home.path(), &["codex"]);
    selected.group_by = GroupBy::ClientSession;
    let cold = assert_public_matches_full(
        &selected,
        &pricing,
        "timestamp fallback outside selected date",
    )
    .await;
    assert_eq!(cold.total_messages, 0);

    // Bytes are identical and the cached row's date is still yesterday. Only
    // the source mtime moved: the repair must happen before window pricing.
    set_mtime(DAY_MS);
    let warm = assert_public_matches_full(
        &selected,
        &pricing,
        "fallback mtime moves into selected date",
    )
    .await;
    assert_eq!(warm.total_messages, 1);
    assert!(
        warm.total_cost > 0.0,
        "fallback row entering window must receive an estimate"
    );
    assert_eq!(warm.sessions[0].first_active_ms, DAY_MS);
    set_mtime(OLD_MS);
    let moved_out =
        assert_public_matches_full(&selected, &pricing, "fallback mtime moves out again").await;
    assert_eq!(moved_out.total_messages, 0);
    assert_eq!(moved_out.total_cost, 0.0);
}

#[tokio::test]
#[serial_test::serial]
async fn codex_incremental_append_and_truncation_match_full_window_reports() {
    let cache_home = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let _cache_env = redirect_cache_home(cache_home.path());
    let mut network = paths::test_env::EnvGuard::capture(&["TOKSCALE_PRICING_CACHE_ONLY"]);
    network.set("TOKSCALE_PRICING_CACHE_ONLY", "1");
    // Append events move forward in the session's timestamp order. The shared
    // fixture's final next-day event is omitted before appending today's rows.
    let body: String = codex_body()
        .lines()
        .take(4)
        .map(|line| format!("{line}\n"))
        .collect();
    let source = write(home.path(), ".codex/sessions/rollout.jsonl", &body);
    let pricing = install_catalog(1.0);
    let selected = options(home.path(), &["codex"]);
    let first =
        assert_public_matches_full(&selected, &pricing, "Codex initial selected window").await;
    assert_eq!(first.total_messages, 1);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap();
    for n in 3..=4 {
        writeln!(file, r#"{{"timestamp":"{DAY}T12:00:0{n}.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{},"output_tokens":{}}},"last_token_usage":{{"input_tokens":32,"output_tokens":16,"cached_input_tokens":8}}}}}}}}"#, (n + 1) * 32, (n + 1) * 16).unwrap();
    }
    drop(file);
    let appended =
        assert_public_matches_full(&selected, &pricing, "Codex incremental append under window")
            .await;
    assert_eq!(appended.total_messages, first.total_messages + 2);
    let mut wide = selected.clone();
    wide.since = None;
    wide.until = None;
    let all = assert_public_matches_full(&wide, &pricing, "Codex appended wide").await;
    assert_eq!(all.total_messages, 5);
    assert_eq!(
        normalized(&appended),
        normalized(
            &assert_public_matches_full(&selected, &pricing, "Codex appended narrow after wide")
                .await
        )
    );
    let prefix: String = body
        .lines()
        .take(2)
        .map(|line| format!("{line}\n"))
        .collect();
    std::fs::write(&source, prefix).unwrap();
    let truncated =
        assert_public_matches_full(&selected, &pricing, "Codex source truncated under window")
            .await;
    assert_eq!(
        truncated.total_messages, 0,
        "truncated Codex rows must not survive in source cache"
    );
    assert_eq!(
        assert_public_matches_full(&wide, &pricing, "Codex truncated wide")
            .await
            .total_messages,
        1
    );
}
