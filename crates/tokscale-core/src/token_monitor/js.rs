//! Value coercion shared by the ports of Token Monitor's JavaScript adapters.
//!
//! The adapters read loosely typed JSON with JavaScript's `||`, `Number()`
//! and `Date.parse`, and session ids hash `path.normalize`d paths. Keeping
//! those rules in one place keeps the ports from drifting apart, and from the
//! session ids Token Monitor already stored.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

/// `Number.MAX_SAFE_INTEGER`. A token count above it cannot be represented by
/// the JavaScript adapters either, and summing it would overflow `i64`.
pub(super) const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// The largest `Date` value, in milliseconds either side of the epoch.
const MAX_DATE_MS: f64 = 8.64e15;

/// JavaScript truthiness.
pub(super) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `a || b || …` over `keys` of `object`: the first truthy value.
pub(super) fn first_truthy<'a>(object: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .filter_map(|key| object.get(*key))
        .find(|value| truthy(value))
}

/// `String(value)` for a value used as a key or label.
pub(super) fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `Number(value)` for JSON scalars; `NaN` for anything it cannot convert.
pub(super) fn js_number(value: Option<&Value>) -> f64 {
    match value {
        None | Some(Value::Null) => 0.0,
        Some(Value::Bool(flag)) => f64::from(u8::from(*flag)),
        Some(Value::Number(number)) => number.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Some(Value::Array(_) | Value::Object(_)) => f64::NAN,
    }
}

/// A token count: a finite number within the safe-integer range, truncated.
pub(super) fn safe_count(number: f64) -> Option<i64> {
    (number.is_finite() && number.abs() <= MAX_SAFE_INTEGER).then(|| number.trunc() as i64)
}

pub(super) fn epoch_number_ms(number: f64) -> i64 {
    let ms = if number > 0.0 && number < 1e12 {
        number * 1000.0
    } else {
        number
    };
    if ms.is_finite() && ms.abs() <= MAX_DATE_MS {
        ms as i64
    } else {
        0
    }
}

/// The adapters' `timestampMs`: epoch seconds or milliseconds by magnitude,
/// numeric strings, then `Date.parse`. Anything unparseable is 0.
pub(super) fn timestamp_ms(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|n| n.is_finite())
            .map(epoch_number_ms)
            .unwrap_or(0),
        Some(Value::String(text)) => parse_time_text(text),
        _ => 0,
    }
}

/// `Date.parse` for the formats these clients write: RFC 3339, ISO date-times
/// without an offset (local time) and ISO dates (UTC).
pub(super) fn parse_time_text(text: &str) -> i64 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return 0;
    }
    if let Ok(number) = trimmed.parse::<f64>() {
        return if number.is_finite() {
            epoch_number_ms(number)
        } else {
            0
        };
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return parsed.timestamp_millis();
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, format) {
            return naive
                .and_local_timezone(chrono::Local)
                .earliest()
                .map(|local| local.timestamp_millis())
                .unwrap_or(0);
        }
    }
    chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|naive| naive.and_utc().timestamp_millis())
        .unwrap_or(0)
}

/// `path.normalize`: drops `.` components and repeated separators and folds
/// `..` lexically, without touching the filesystem.
pub(super) fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let parent_is_normal =
                    matches!(out.components().next_back(), Some(Component::Normal(_)));
                if parent_is_normal {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// `sha256(path.normalize(path)).slice(0, 12)`, the namespace the adapters
/// put into session ids.
pub(super) fn path_namespace(path: &Path) -> String {
    let normalized = normalize_path(path);
    Sha256::digest(normalized.to_string_lossy().as_bytes())
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truthiness_and_first_truthy_follow_javascript() {
        for falsy in [json!(null), json!(false), json!(0), json!(0.0), json!("")] {
            assert!(!truthy(&falsy), "{falsy}");
        }
        for value in [json!(true), json!(1), json!("0"), json!([]), json!({})] {
            assert!(truthy(&value), "{value}");
        }
        let object = json!({ "a": 0, "b": "", "c": "x" });
        assert_eq!(first_truthy(&object, &["a", "b", "c"]), Some(&json!("x")));
        assert_eq!(first_truthy(&object, &["a", "b"]), None);
    }

    #[test]
    fn numbers_reject_non_finite_and_unsafe_values() {
        assert_eq!(js_number(Some(&json!(true))), 1.0);
        assert!(js_number(Some(&json!("Infinity"))).is_infinite());
        assert_eq!(safe_count(js_number(Some(&json!("Infinity")))), None);
        assert_eq!(safe_count(1e30), None);
        assert_eq!(safe_count(12.9), Some(12));
    }

    #[test]
    fn timestamps_reject_values_outside_the_date_range() {
        assert_eq!(timestamp_ms(Some(&json!("Infinity"))), 0);
        assert_eq!(timestamp_ms(Some(&json!(1e300))), 0);
        assert_eq!(timestamp_ms(Some(&json!(1_790_848_800))), 1_790_848_800_000);
        assert_eq!(
            timestamp_ms(Some(&json!("2026-10-01T10:00:00Z"))),
            1_790_848_800_000
        );
    }

    #[test]
    fn normalize_path_matches_node() {
        let cases = [
            ("/a/./b//c/../d", "/a/b/d"),
            ("/a/b/..", "/a"),
            ("/..", "/"),
            ("a/../../b", "../b"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalize_path(Path::new(input)),
                PathBuf::from(expected),
                "{input}"
            );
        }
        assert_eq!(
            path_namespace(Path::new("/home/u/./.proma//agent-sessions")),
            path_namespace(Path::new("/home/u/.proma/agent-sessions"))
        );
    }
}
