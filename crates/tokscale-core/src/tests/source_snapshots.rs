use super::*;
use crate::{
    bucket_tz::BucketTimezone,
    source_snapshot::{source_event_id, source_scope_id, SourceReadEvidence},
};
use crate::{SourceEventUsage, SourceUsage};
use std::time::Instant;

fn message(
    client: &str,
    session: &str,
    date: &str,
    model: &str,
    input: i64,
    key: Option<&str>,
) -> UnifiedMessage {
    let timestamp = chrono::DateTime::parse_from_rfc3339(&format!("{date}T12:00:00Z"))
        .unwrap()
        .timestamp_millis();
    let mut message = UnifiedMessage::new(
        client,
        model,
        "provider",
        session,
        timestamp,
        TokenBreakdown {
            input,
            ..Default::default()
        },
        input as f64 / 100.0,
    );
    message.date = date.into();
    message.dedup_key = key.map(str::to_string);
    message
}

fn snapshot(
    options: &ReportOptions,
    messages: Vec<UnifiedMessage>,
    evidence: Option<SourceReadEvidence>,
) -> crate::GraphResult {
    let mut sink = GraphSink::new(
        Some(options),
        None,
        GraphPricingRequirement::LenientSourceSnapshots,
    );
    sink.enable_source_snapshots(SourceUsage {
        version: 2,
        event_identity_version: 2,
        captured_at: "2026-03-03T00:00:00Z".into(),
        full_history: options.since.is_none() && options.until.is_none() && options.year.is_none(),
        replacement_clients: crate::source_replacement_clients(options.clients.as_deref()),
        partial_clients: Vec::new(),
        bucket_timezone: "Etc/UTC".into(),
        day_key: "2026-03-03".into(),
        scan_roots: Default::default(),
        root_usage: Vec::new(),
        sessions: Vec::new(),
        scopes: Vec::new(),
        rows_without_identity: Default::default(),
    });
    if let Some(evidence) = evidence {
        sink.observe_source(evidence);
    }
    for message in messages {
        sink.accept(message);
    }
    sink.finish(
        Instant::now(),
        &BucketTimezone::from_pinned_name(Some("Etc/UTC")),
    )
    .unwrap()
}

fn tokens(event: &SourceEventUsage) -> i64 {
    event
        .contributions
        .iter()
        .map(|c| {
            c.tokens.input
                + c.tokens.output
                + c.tokens.cache_read
                + c.tokens.cache_write
                + c.tokens.reasoning
        })
        .sum()
}
fn selected(clients: &[&str]) -> ReportOptions {
    ReportOptions {
        clients: Some(clients.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    }
}

#[test]
fn source_snapshots_v2_timestamped_accounting_matches_graph_without_duplicate_days() {
    let mut detailed = message("micode", "same", "2026-03-01", "a", 7, Some("event-a"));
    detailed.tokens.output = 3;
    detailed.tokens.cache_read = 5;
    detailed.tokens.cache_write = 11;
    detailed.tokens.cache_write_1h = 2;
    detailed.tokens.reasoning = 13;
    detailed.message_count = 2;
    let graph = snapshot(
        &selected(&["micode", "micode-desktop"]),
        vec![
            detailed,
            message("micode", "same", "2026-03-02", "b", 2, Some("event-a")),
            message("micode", "same", "2026-03-01", "c", 4, Some("event-b")),
            message("micode", "same", "2026-03-02", "", 0, None),
            message("micode", "same", "2026-03-02", "b", 6, Some("  ")),
        ],
        Some(SourceReadEvidence::new("micode", true, true)),
    );
    let source = graph.source_usage.unwrap();
    let session = &source.sessions[0];
    assert_eq!(session.events.len(), 3);
    assert_eq!(
        session.events.iter().map(tokens).sum::<i64>(),
        graph.summary.total_tokens
    );
    assert_eq!(
        session
            .events
            .iter()
            .flat_map(|e| &e.contributions)
            .map(|c| c.messages)
            .sum::<i32>(),
        6
    );
    assert!(
        (session
            .events
            .iter()
            .flat_map(|e| &e.contributions)
            .map(|c| c.cost)
            .sum::<f64>()
            - graph.summary.total_cost)
            .abs()
            < 1e-12
    );
    assert_eq!(source.scopes[0].identity_status, "unresolved");
    let wire = serde_json::to_value(&source).unwrap();
    assert_eq!(wire["version"], 2);
    assert_eq!(wire["eventIdentityVersion"], 2);
    assert!(wire["sessions"][0].get("days").is_none());
    assert!(wire["sessions"][0]["events"][0].get("days").is_none());
    let c = &wire["sessions"][0]["events"][1]["contributions"][0];
    assert!(c["timestamp"].as_i64().unwrap() > 0);
    assert!(c["tokens"].get("cacheRead").is_some());
    assert!(c["tokens"].get("cache_write_1h").is_none());
}

#[test]
fn source_snapshots_identities_follow_ownership_correction_and_timezone() {
    for (family, aliases) in [
        ("micode", vec!["micode-desktop"]),
        ("claude", vec!["cc-mirror/test"]),
        ("gjc", vec!["9router", "9Router"]),
        (
            "antigravity",
            vec!["antigravity-cli", "antigravity-extension"],
        ),
    ] {
        let old = message(family, "A", "2026-03-01", "old", 100, Some("response"));
        for alias in aliases {
            let mut corrected = message(alias, "B", "2026-03-03", "new", 20, Some("response"));
            corrected.provider_id = "new-provider".into();
            corrected.rebucket_date(&BucketTimezone::from_pinned_name(Some(
                "Pacific/Kiritimati",
            )));
            assert_eq!(source_event_id(&old), source_event_id(&corrected));
            assert_eq!(
                source_scope_id(&old.client),
                source_scope_id(&corrected.client)
            );
        }
        let independent = message(
            "independent",
            "A",
            "2026-03-01",
            "old",
            100,
            Some("response"),
        );
        assert_ne!(source_event_id(&old), source_event_id(&independent));
        let id = source_event_id(&old).unwrap();
        assert_eq!(id.len(), 71);
        assert!(id.starts_with("sha256:"));
    }
}

#[test]
fn source_snapshots_codex_turn_group_survives_mutable_native_keys() {
    let mut a = message(
        "codex",
        "A",
        "2026-03-01",
        "old",
        30,
        Some("mutable-total-30"),
    );
    a.source_group_key = Some(crate::source_snapshot::codex_group_key("upstream", "turn"));
    let mut b = message(
        "codex",
        "A",
        "2026-03-02",
        "other",
        15,
        Some("mutable-total-45"),
    );
    b.session_id = "B-fragment".into();
    b.source_group_key = a.source_group_key.clone();
    let old = snapshot(
        &selected(&["codex"]),
        vec![a.clone(), b],
        Some(SourceReadEvidence::new("codex", true, true)),
    )
    .source_usage
    .unwrap();
    a.session_id = "B".into();
    a.model_id = "corrected".into();
    a.provider_id = "corrected-provider".into();
    a.tokens.input = 15;
    a.dedup_key = Some("mutable-total-15".into());
    let new = snapshot(
        &selected(&["codex"]),
        vec![a],
        Some(SourceReadEvidence::new("codex", true, true)),
    )
    .source_usage
    .unwrap();
    assert_eq!(old.sessions.len(), 2);
    assert!(old.sessions[1].events.is_empty());
    assert_eq!(old.sessions[0].events.len(), 1);
    assert_eq!(old.sessions[0].events[0].contributions.len(), 2);
    assert_eq!(tokens(&old.sessions[0].events[0]), 45);
    assert_eq!(tokens(&new.sessions[0].events[0]), 15);
    assert_eq!(old.sessions[0].events[0].id, new.sessions[0].events[0].id);
    assert_eq!(new.scopes[0].identity_status, "exact");
    let no_turn = message("codex", "C", "2026-03-02", "model", 15, Some("native-key"));
    let unknown = snapshot(
        &selected(&["codex"]),
        vec![no_turn],
        Some(SourceReadEvidence::new("codex", true, true)),
    )
    .source_usage
    .unwrap();
    assert_eq!(unknown.scopes[0].identity_status, "unresolved");
    assert!(unknown.sessions[0].events[0].id.is_none());
}

#[test]
fn source_snapshots_health_selection_and_date_coverage_are_independent() {
    for (clients, complete, expected) in [
        (vec!["micode", "micode-desktop"], true, "complete"),
        (vec!["micode", "micode-desktop"], false, "partial"),
        (vec!["micode"], true, "partial"),
        (vec!["micode-desktop"], true, "partial"),
    ] {
        let source = snapshot(
            &selected(&clients),
            vec![],
            Some(SourceReadEvidence::new("micode", complete, true)),
        )
        .source_usage
        .unwrap();
        assert!(source.full_history);
        assert_eq!(source.scopes[0].read_status, expected);
        assert_eq!(source.scopes[0].clients, ["micode", "micode-desktop"]);
        if clients.len() == 1 {
            assert_eq!(source.partial_clients, ["micode", "micode-desktop"]);
        }
    }
    let unknown = snapshot(&selected(&["opencode"]), vec![], None)
        .source_usage
        .unwrap();
    assert_eq!(unknown.scopes[0].read_status, "unknown");
    assert_eq!(unknown.scopes[0].identity_status, "unresolved");
    let mut window = selected(&["micode", "micode-desktop"]);
    window.since = Some("2026-03-02".into());
    let source = snapshot(
        &window,
        vec![message(
            "micode",
            "outside",
            "2026-03-01",
            "model",
            15,
            Some("id"),
        )],
        Some(SourceReadEvidence::new("micode", true, true)),
    )
    .source_usage
    .unwrap();
    assert!(!source.full_history);
    assert_eq!(source.scopes[0].read_status, "complete");
    assert!(source.sessions[0].events.is_empty());
    let synthetic = snapshot(
        &selected(&["synthetic"]),
        vec![message(
            "opencode",
            "sid",
            "2026-03-01",
            "model",
            15,
            Some("id"),
        )],
        None,
    )
    .source_usage
    .unwrap();
    assert!(synthetic.replacement_clients.is_empty());
    assert_eq!(synthetic.partial_clients, ["opencode"]);
}

#[test]
fn source_snapshots_inventory_zero_ids_gaps_batches_and_owner_partitions() {
    let mut evidence = SourceReadEvidence::new("micode", true, true);
    evidence.sessions.push(("micode".into(), "empty".into()));
    evidence
        .sessions
        .push(("micode-desktop".into(), "empty-desktop".into()));
    let graph = snapshot(
        &selected(&["micode", "micode-desktop"]),
        vec![
            message("micode", "zero", "2026-03-02", "", 0, Some("zero")),
            message(
                "micode",
                "unknown",
                "2026-03-02",
                "model",
                3,
                Some("missing"),
            ),
        ],
        Some(evidence),
    );
    let source = graph.source_usage.unwrap();
    assert_eq!(source.sessions.len(), 3);
    assert_eq!(source.rows_without_identity["micode"], 1);
    assert_eq!(
        source
            .sessions
            .iter()
            .filter(|s| s.events.is_empty())
            .count(),
        2
    );
    assert_eq!(
        source
            .sessions
            .iter()
            .find(|s| s.session_id == "zero")
            .unwrap()
            .events[0]
            .contributions[0]
            .messages,
        1
    );
    let rows = (0..GRAPH_SINK_BATCH + 1)
        .map(|_| message("micode", "same", "2026-03-01", "model", 1, Some("same")))
        .collect();
    let graph = snapshot(
        &selected(&["micode", "micode-desktop"]),
        rows,
        Some(SourceReadEvidence::new("micode", true, true)),
    );
    assert_eq!(
        tokens(&graph.source_usage.unwrap().sessions[0].events[0]),
        (GRAPH_SINK_BATCH + 1) as i64
    );
    let graph = snapshot(
        &ReportOptions::default(),
        vec![
            message("opencode", "shared", "2026-03-01", "model", 5, Some("same")),
            message("micode", "shared", "2026-03-01", "model", 7, Some("same")),
            message(
                "copilot",
                "unknown-session",
                "2026-03-01",
                "model",
                11,
                Some("span"),
            ),
        ],
        None,
    );
    let source = graph.source_usage.unwrap();
    assert_eq!(source.sessions.len(), 2);
    assert_eq!(source.rows_without_identity["copilot"], 1);
    assert_eq!(graph.summary.total_tokens, 23);
    assert_ne!(
        source.sessions[0].events[0].id,
        source.sessions[1].events[0].id
    );
    for client in crate::token_monitor::client_ids() {
        assert!(!crate::source_replacement_clients(None).contains(&client.to_string()));
        assert_eq!(
            crate::source_replacement_clients(Some(&[client.into()])),
            [client]
        );
    }
}

#[test]
fn source_snapshots_are_absent_without_opt_in() {
    for requirement in [
        GraphPricingRequirement::Lenient,
        GraphPricingRequirement::LenientSourceOnly,
    ] {
        let graph = build_graph_from_messages(
            vec![message(
                "micode",
                "same",
                "2026-03-01",
                "model",
                1,
                Some("id"),
            )],
            None,
            requirement,
            Instant::now(),
            &BucketTimezone::from_pinned_name(Some("Etc/UTC")),
        )
        .unwrap();
        assert!(graph.source_usage.is_none());
    }
}

#[test]
fn source_snapshots_performance_and_unpriced_are_additive_native_token_buckets() {
    let mut row = message(
        "claude",
        "session",
        "2026-03-02",
        "unpriced-model",
        10,
        Some("message:key"),
    );
    row.tokens.output = 5;
    row.tokens.reasoning = 3;
    row.tokens.cache_write = 4;
    row.tokens.cache_write_1h = 2;
    row.duration_ms = Some(1000);
    let graph = snapshot(
        &selected(&["claude"]),
        vec![row.clone(), row],
        Some(SourceReadEvidence::new("claude", true, true)),
    );
    let wire = serde_json::to_value(graph.source_usage.unwrap()).unwrap();
    let c = &wire["sessions"][0]["events"][0]["contributions"][0];
    assert_eq!(c["tokens"]["output"], 10);
    assert_eq!(c["tokens"]["reasoning"], 6);
    assert_eq!(c["tokens"]["cacheWrite"], 8);
    assert_eq!(c["tokens"]["cacheWrite1h"], 4);
    assert_eq!(c["pricing"]["unpricedTokens"], 44);
    assert_eq!(c["pricing"]["unpricedMessages"], 2);
    assert_eq!(c["performance"]["timedTokens"], 44);
    assert_eq!(c["performance"]["timedOutputTokens"], 16);
    assert_eq!(c["performance"]["totalDurationMs"], 2000);
    assert_eq!(c["performance"]["sampleCount"], 2);
}

#[test]
fn source_snapshots_claude_mirror_requires_project_session_relationship() {
    use crate::sessions::claudecode::is_ssh_mirror;
    use std::path::Path;
    assert!(is_ssh_mirror(Path::new(
        "/home/.claude/projects/ssh-session/session.jsonl"
    )));
    assert!(is_ssh_mirror(Path::new(
        "/home/.claude/projects/ssh-session/session/subagents/agent-a.jsonl"
    )));
    assert!(!is_ssh_mirror(Path::new(
        "/home/.claude/projects/ssh-session/unrelated.jsonl"
    )));
    assert!(!is_ssh_mirror(Path::new("/home/ssh-session/session.jsonl")));
    assert!(!is_ssh_mirror(Path::new(
        "/home/.claude/projects/ssh-/session.jsonl"
    )));
}

#[test]
fn source_snapshots_model_performance_matches_timed_output_and_reasoning() {
    let mut timed = message(
        "claude",
        "session",
        "2026-03-02",
        "model",
        10,
        Some("message:timed"),
    );
    timed.tokens.output = 5;
    timed.tokens.reasoning = 3;
    timed.duration_ms = Some(1000);
    let mut untimed = timed.clone();
    untimed.dedup_key = Some("message:untimed".into());
    untimed.tokens.output = 100;
    untimed.tokens.reasoning = 50;
    untimed.duration_ms = None;
    let rows = vec![timed, untimed];
    let entries = crate::aggregate_model_usage_entries_with_source_metadata(
        rows.clone(),
        &GroupBy::Model,
        WorktreeRollup::default(),
        &mut crate::WorkspaceLabeler::default(),
        true,
        None,
    );
    assert_eq!(entries[0].performance.timed_output_tokens, Some(8));
    assert_eq!(entries[0].performance.timed_tokens, 18);
    let ordinary = crate::aggregate_model_usage_entries_with_labeler(
        rows,
        &GroupBy::Model,
        WorktreeRollup::default(),
        &mut crate::WorkspaceLabeler::default(),
    );
    assert!(ordinary[0].performance.timed_output_tokens.is_none());
}

#[test]
fn source_snapshots_unpriced_report_counters_preserve_nonzero_fallback_costs_and_authority() {
    let options = ReportOptions::default();
    let mut unknown = message(
        "claude",
        "session",
        "2026-03-02",
        "unknown",
        10,
        Some("first"),
    );
    unknown.tokens.output = 5;
    unknown.tokens.reasoning = 3;
    unknown.tokens.cache_read = 7;
    unknown.tokens.cache_write = 5;
    unknown.tokens.cache_write_1h = 2; // Subset already counted in cache_write.
    unknown.cost = 2.5;
    unknown.mark_estimated_cost();
    let mut second = message(
        "claude",
        "session",
        "2026-03-02",
        "unknown",
        50,
        Some("second"),
    );
    second.cost = 1.5;
    second.mark_estimated_cost();
    let mut authoritative = message(
        "claude",
        "session",
        "2026-03-02",
        "unknown",
        20,
        Some("reported"),
    );
    authoritative.cost = 0.0;
    authoritative.mark_provider_reported_cost();
    let rows = vec![unknown, second, authoritative];
    let graph = snapshot(&options, rows.clone(), None);
    assert_eq!(graph.contributions[0].clients[0].unpriced_tokens, Some(80));
    assert_eq!(graph.summary.total_cost, 4.0);
    let source_total: i64 = graph
        .source_usage
        .as_ref()
        .unwrap()
        .sessions
        .iter()
        .flat_map(|session| &session.events)
        .flat_map(|event| &event.contributions)
        .map(|row| row.pricing.as_ref().unwrap().unpriced_tokens)
        .sum();
    assert_eq!(source_total, 80);
    let entries = crate::aggregate_model_usage_entries_with_source_metadata(
        rows.clone(),
        &GroupBy::Model,
        WorktreeRollup::default(),
        &mut crate::WorkspaceLabeler::default(),
        true,
        None,
    );
    assert_eq!(entries[0].unpriced_tokens, Some(80));
    assert_eq!(entries[0].cost, 4.0);
    let ordinary = crate::aggregate_model_usage_entries_with_labeler(
        rows.clone(),
        &GroupBy::Model,
        WorktreeRollup::default(),
        &mut crate::WorkspaceLabeler::default(),
    );
    assert!(ordinary[0].unpriced_tokens.is_none());
    let legacy = serde_json::to_value(crate::aggregator::aggregate_by_date(rows)).unwrap();
    assert!(legacy[0]["clients"][0].get("unpricedTokens").is_none());
    let mut legacy_row = serde_json::to_value(&graph.contributions[0].clients[0]).unwrap();
    legacy_row.as_object_mut().unwrap().remove("unpricedTokens");
    let decoded: crate::ClientContribution = serde_json::from_value(legacy_row).unwrap();
    assert!(decoded.unpriced_tokens.is_none());
}

#[test]
fn source_snapshots_claude_declared_null_accounting_is_partial_but_missing_optional_is_supported() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("session.jsonl");
    let legacy = serde_json::json!({
        "type": "assistant", "timestamp": "2026-03-02T00:00:00Z", "requestId": "request",
        "message": {"id": "same", "model": "unpriced-fixture-model", "usage": {
            "input_tokens": 100, "output_tokens": 0,
        }},
    });
    std::fs::write(&path, legacy.to_string()).unwrap();
    let (rows, complete) =
        crate::sessions::claudecode::parse_claude_file_for_source(&path, Some(home.path()));
    assert!(complete);
    let id = source_event_id(&rows[0]);
    for pointer in [
        "/message/usage/input_tokens",
        "/message/usage/output_tokens",
        "/message/usage/cache_read_input_tokens",
        "/message/usage/cache_creation_input_tokens",
        "/message/usage/cache_creation",
        "/message/usage/cache_creation/ephemeral_1h_input_tokens",
        "/message/usage/cache_creation/ephemeral_5m_input_tokens",
        "/message/usage/cached_input_tokens",
        "/message/usage/reasoning_output_tokens",
        "/message/usage/inputTokens",
        "/message/usage/outputTokens",
        "/message/usage/total_tokens",
        "/message/usage/output_tokens_details",
        "/message/usage/performance",
        "/message/performance",
        "/performance",
        "/duration_ms",
        "/message/durationMs",
    ] {
        let mut row = legacy.clone();
        let keys: Vec<_> = pointer.trim_start_matches('/').split('/').collect();
        let mut value = &mut row;
        for key in &keys[..keys.len() - 1] {
            if !value[*key].is_object() {
                value[*key] = serde_json::json!({});
            }
            value = &mut value[*key];
        }
        value[keys[keys.len() - 1]] = serde_json::Value::Null;
        std::fs::write(&path, row.to_string()).unwrap();
        let (rows, complete) =
            crate::sessions::claudecode::parse_claude_file_for_source(&path, Some(home.path()));
        assert!(!complete, "{pointer}");
        if let Some(row) = rows.first() {
            assert_eq!(source_event_id(row), id, "{pointer}");
        }
    }
    // A null request-start timestamp must not certify a lost timed sample.
    std::fs::write(
        &path,
        format!(
            "{}\n{}",
            serde_json::json!({"type":"user", "timestamp":null}),
            legacy
        ),
    )
    .unwrap();
    assert!(!crate::sessions::claudecode::parse_claude_file_for_source(&path, Some(home.path())).1);
}
