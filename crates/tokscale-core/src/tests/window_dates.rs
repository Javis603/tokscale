use crate::{
    message_passes_report_filter, scanner, PricingWindow, ReportOptions, TokenBreakdown,
    UnifiedMessage,
};

#[test]
fn window_prediction_matches_final_date_filter_without_mutating_messages() {
    let timestamps = [
        0,
        -1,
        i64::MAX,
        chrono::DateTime::parse_from_rfc3339("2025-12-31T23:59:59Z")
            .unwrap()
            .timestamp_millis(),
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .timestamp_millis(),
        // Both sides of the US spring-forward and fall-back transitions.
        chrono::DateTime::parse_from_rfc3339("2026-03-08T09:59:59Z")
            .unwrap()
            .timestamp_millis(),
        chrono::DateTime::parse_from_rfc3339("2026-03-08T10:00:00Z")
            .unwrap()
            .timestamp_millis(),
        chrono::DateTime::parse_from_rfc3339("2026-11-01T08:59:59Z")
            .unwrap()
            .timestamp_millis(),
        chrono::DateTime::parse_from_rfc3339("2026-11-01T09:00:00Z")
            .unwrap()
            .timestamp_millis(),
    ];
    for zone in [
        None,
        Some("UTC"),
        Some("Asia/Shanghai"),
        Some("America/Los_Angeles"),
        Some("invalid-zone"),
    ] {
        let settings = scanner::ScannerSettings {
            bucket_timezone: zone.map(str::to_owned),
            ..Default::default()
        };
        let timezone = crate::BucketTimezone::from_scanner_settings(&settings);
        for timestamp in timestamps {
            let mut message = UnifiedMessage::new(
                "codex",
                "gpt-5.4",
                "openai",
                "session",
                timestamp,
                TokenBreakdown::default(),
                0.0,
            );
            message.refresh_derived_fields();
            let original_date = message.date.clone();
            let mut final_message = message.clone();
            if timezone.is_pinned() {
                final_message.rebucket_date(&timezone);
            }
            for options in [
                ReportOptions {
                    year: Some("2026".into()),
                    ..Default::default()
                },
                ReportOptions {
                    since: Some("2026-01-01".into()),
                    until: Some("2026-01-01".into()),
                    ..Default::default()
                },
                ReportOptions {
                    since: Some("2026-03-08".into()),
                    until: Some("2026-11-01".into()),
                    ..Default::default()
                },
                ReportOptions {
                    year: Some("2025".into()),
                    since: Some("2026-01-01".into()),
                    ..Default::default()
                },
                ReportOptions {
                    since: Some("2026-01-02".into()),
                    until: Some("2026-01-01".into()),
                    ..Default::default()
                },
            ] {
                let window = PricingWindow::new(&options, &settings).unwrap();
                assert_eq!(
                    window.matches(&message),
                    message_passes_report_filter(&final_message, &options),
                    "zone {zone:?}, timestamp {timestamp}, options {options:?}"
                );
                assert_eq!(
                    message.date, original_date,
                    "pricing must not rebucket reducer inputs"
                );
            }
        }
    }
    assert!(
        PricingWindow::new(
            &ReportOptions::default(),
            &scanner::ScannerSettings::default()
        )
        .is_none(),
        "unbounded reports do not need a per-message date gate"
    );
}
