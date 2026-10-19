//! DeepTutor's content-free per-account `llm_calls` ledger (v1.6.14).
//!
//! Reported and estimated calls are disjoint, opt-in clients. DeepTutor's
//! OAuth adapter can omit usage: its character-based estimates must never
//! silently enter the provider-reported partition. Conversation snapshots
//! are deliberately not read, avoiding duplicates and prompt access.
//!
//! DeepTutor defaults to its launch directory, not a fixed user directory.
//! Require an explicit DB path or runtime home; never search projects/users.

use super::{js, Scope};
use crate::sessions::utils::open_readonly_sqlite;
use crate::sessions::{BillingIdentityOrigin, PerformanceTiming, UnifiedMessage};
use crate::TokenBreakdown;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};
use tracing::warn;

pub const CLIENT_ID: &str = "deeptutor";
pub const ESTIMATED_CLIENT_ID: &str = "deeptutor-estimated";
const MAX_ROWS: usize = 100_000;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_JSON_BYTES: usize = 64 * 1024;
const CACHE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Row {
    id: String,
    session: String,
    model: String,
    provider: String,
    timestamp: i64,
    estimated: bool,
    tokens: TokenBreakdown,
    generation_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Fingerprint {
    database: (u64, u128),
    wal: Option<(u64, u128)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    fingerprint: Fingerprint,
    rows: Vec<Row>,
}

fn configured_path(enabled: bool, db: Option<String>, home: Option<String>) -> Option<PathBuf> {
    if !enabled {
        return None;
    }
    // Invalid explicit configuration must not silently select another account.
    if let Some(db) = db.filter(|s| !s.trim().is_empty()) {
        let path = PathBuf::from(db.trim());
        return path.is_absolute().then_some(path);
    }
    let home = PathBuf::from(home?.trim());
    home.is_absolute()
        .then(|| home.join("data").join("user").join("usage.sqlite3"))
}

pub fn parse(scope: &Scope, estimated: bool) -> Vec<UnifiedMessage> {
    let Some(db) = configured_path(
        scope.use_env_roots,
        std::env::var("TOKEN_MONITOR_DEEPTUTOR_DB_PATH").ok(),
        std::env::var("DEEPTUTOR_HOME").ok(),
    ) else {
        return Vec::new();
    };
    let cache = crate::paths::get_cache_dir()
        .join("token-monitor")
        .join(format!("deeptutor-{}.json", js::path_namespace(&db)));
    parse_with(&db, &cache, estimated)
}

fn stat(path: &Path) -> Option<(u64, u128)> {
    let metadata = std::fs::metadata(path).ok()?;
    metadata.is_file().then_some((
        metadata.len(),
        metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos(),
    ))
}

fn fingerprint(db: &Path) -> Option<Fingerprint> {
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    Some(Fingerprint {
        database: stat(db)?,
        wal: stat(Path::new(&wal)),
    })
}

fn parse_with(db: &Path, cache: &Path, estimated: bool) -> Vec<UnifiedMessage> {
    let saved = std::fs::read(cache)
        .ok()
        .filter(|bytes| bytes.len() <= MAX_BYTES)
        .and_then(|bytes| serde_json::from_slice::<Snapshot>(&bytes).ok())
        .filter(|snapshot| snapshot.version == CACHE_VERSION);
    let Some(current) = fingerprint(db) else {
        return Vec::new();
    };
    let snapshot = if let Some(saved) = saved.as_ref().filter(|s| s.fingerprint == current) {
        Some(saved.clone())
    } else {
        match read_db(db, MAX_ROWS, MAX_BYTES) {
            Ok(rows) => {
                let snapshot = Snapshot {
                    version: CACHE_VERSION,
                    fingerprint: current.clone(),
                    rows,
                };
                // A commit during the scan must be picked up on the next scan.
                if fingerprint(db).as_ref() == Some(&current) {
                    save(cache, &snapshot);
                }
                Some(snapshot)
            }
            Err(error) => {
                warn!("DeepTutor usage read failed; retaining last complete snapshot: {error}");
                saved
            }
        }
    };
    let namespace = js::path_namespace(db);
    snapshot
        .into_iter()
        .flat_map(|s| s.rows)
        .filter(|row| row.estimated == estimated)
        .map(|row| to_message(row, &namespace))
        .collect()
}

fn save(path: &Path, snapshot: &Snapshot) {
    let Some(parent) = path.parent() else { return };
    let Ok(bytes) = serde_json::to_vec(snapshot) else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&temporary, bytes).is_ok() {
        let _ = crate::fs_atomic::replace_file(&temporary, path);
    }
}

fn count(usage: &Value, key: &str, required: bool) -> Option<i64> {
    match usage.get(key) {
        None | Some(Value::Null) if !required => Some(0),
        Some(value) => value
            .as_i64()
            .filter(|n| *n >= 0 && *n <= 9_007_199_254_740_991),
        _ => None,
    }
}

fn normalize(id: String, stamp: f64, session: String, turn: String, raw: &str) -> Option<Row> {
    if id.is_empty()
        || id.len() > 512
        || !stamp.is_finite()
        || !(0.0..253_402_300_799.0).contains(&stamp)
    {
        return None;
    }
    let usage: Value = serde_json::from_str(raw).ok()?;
    // Unknown precision is not provider-reported usage.
    let estimated = usage.get("estimated")?.as_bool()?;
    let model = usage.get("model")?.as_str()?.trim();
    let provider = usage.get("provider")?.as_str()?.trim();
    if model.is_empty() || provider.is_empty() || model.len() > 256 || provider.len() > 256 {
        return None;
    }
    let prompt = count(&usage, "prompt_tokens", true)?;
    let completion = count(&usage, "completion_tokens", true)?;
    let cached = count(&usage, "cache_read_input_tokens", false)?;
    let created = count(&usage, "cache_creation_input_tokens", false)?;
    let reasoning = count(&usage, "reasoning_tokens", false)?;
    if cached + created > prompt || reasoning > completion || prompt + completion == 0 {
        return None;
    }
    if let Some(total) = usage.get("total_tokens").filter(|v| !v.is_null()) {
        if total.as_i64()? != prompt.checked_add(completion)? {
            return None;
        }
    }
    let generation_ms = if estimated {
        None
    } else {
        usage
            .get("generation_seconds")
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && *n > 0.0 && *n < 86_400.0)
            .map(|n| (n * 1000.0).round() as i64)
            .filter(|n| *n > 0)
    };
    Some(Row {
        session: if !session.is_empty() {
            session
        } else if !turn.is_empty() {
            turn
        } else {
            id.clone()
        },
        id,
        model: model.to_owned(),
        provider: provider.to_owned(),
        timestamp: (stamp * 1000.0).round() as i64,
        estimated,
        tokens: TokenBreakdown {
            input: prompt - cached - created,
            output: completion - reasoning,
            cache_read: cached,
            cache_write: created,
            cache_write_1h: 0,
            reasoning,
        },
        generation_ms,
    })
}

fn read_db(db: &Path, max_rows: usize, max_bytes: usize) -> Result<Vec<Row>, String> {
    let conn = open_readonly_sqlite(db).map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_millis(250))
        .map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA query_only=ON")
        .map_err(|e| e.to_string())?;
    let mut statement = conn.prepare("SELECT call_id, started_at, session_id, turn_id, usage_json FROM llm_calls ORDER BY started_at, call_id")
        .map_err(|e| e.to_string())?;
    let mut cursor = statement.query([]).map_err(|e| e.to_string())?;
    let mut output = Vec::new();
    let mut ids = HashSet::new();
    let mut rows = 0usize;
    let mut bytes = 0usize;
    while let Some(row) = cursor.next().map_err(|e| e.to_string())? {
        rows += 1;
        if rows > max_rows {
            return Err("row budget exceeded".into());
        }
        let id: String = row.get(0).map_err(|e| e.to_string())?;
        let stamp: f64 = row.get(1).map_err(|e| e.to_string())?;
        let session: String = row.get(2).map_err(|e| e.to_string())?;
        let turn: String = row.get(3).map_err(|e| e.to_string())?;
        let raw: String = row.get(4).map_err(|e| e.to_string())?;
        bytes = bytes.saturating_add(raw.len() + id.len() + session.len() + turn.len());
        if bytes > max_bytes || raw.len() > MAX_JSON_BYTES {
            return Err("byte budget exceeded".into());
        }
        if let Some(normalized) = normalize(id, stamp, session, turn, &raw) {
            if ids.insert(normalized.id.clone()) {
                output.push(normalized);
            }
        }
    }
    Ok(output)
}

fn to_message(row: Row, namespace: &str) -> UnifiedMessage {
    // Estimates and subscription/OAuth counters have no reported API charge.
    // Existing unpriced-provider semantics prevent model-name tariff guessing.
    let provider = if row.estimated
        || matches!(
            row.provider.to_ascii_lowercase().as_str(),
            "openai_codex" | "openaicodexprovider"
        ) {
        format!("unpriced:{}", row.provider)
    } else {
        row.provider.clone()
    };
    let mut message = UnifiedMessage::new_with_dedup(
        if row.estimated {
            ESTIMATED_CLIENT_ID
        } else {
            CLIENT_ID
        },
        row.model,
        provider,
        format!("{}@{}", row.session, namespace),
        row.timestamp,
        row.tokens,
        0.0,
        Some(format!("deeptutor:{namespace}:{}", row.id)),
    )
    .with_billing(Some(&row.provider), BillingIdentityOrigin::ExplicitRecord);
    message.performance_timing = Some(
        row.generation_ms
            .map_or(PerformanceTiming::Unavailable, PerformanceTiming::Duration),
    );
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};
    use serde_json::json;

    fn usage(estimated: bool) -> Value {
        json!({"model":"fixture-model","provider":"openai_codex","estimated":estimated,
            "prompt_tokens":120,"completion_tokens":30,"total_tokens":150,
            "cache_read_input_tokens":40,"cache_creation_input_tokens":10,"reasoning_tokens":5,
            "generation_seconds":2.5})
    }
    fn row(value: Value) -> Option<Row> {
        normalize(
            "call-1".into(),
            1_792_368_000.125,
            "session-1".into(),
            "turn-1".into(),
            &value.to_string(),
        )
    }
    fn fixture(db: &Path) -> Connection {
        let conn = Connection::open(db).unwrap();
        conn.execute_batch("CREATE TABLE llm_calls (call_id TEXT PRIMARY KEY, started_at REAL, session_id TEXT, turn_id TEXT, source TEXT, usage_json TEXT);").unwrap();
        conn
    }
    fn insert(conn: &Connection, id: &str, estimated: bool) {
        conn.execute(
            "INSERT INTO llm_calls VALUES (?,1792368000,'session-1','turn-1','fixture',?)",
            params![id, usage(estimated).to_string()],
        )
        .unwrap();
    }

    #[test]
    fn splits_cache_and_reasoning_once_and_preserves_identity() {
        let message = to_message(row(usage(false)).unwrap(), "fixture");
        assert_eq!(message.client, CLIENT_ID);
        assert_eq!(message.timestamp, 1_792_368_000_125);
        assert_eq!(message.session_id, "session-1@fixture");
        assert_eq!(
            message.dedup_key.as_deref(),
            Some("deeptutor:fixture:call-1")
        );
        assert_eq!(
            (
                message.tokens.input,
                message.tokens.output,
                message.tokens.cache_read,
                message.tokens.cache_write,
                message.tokens.reasoning
            ),
            (70, 25, 40, 10, 5)
        );
        assert_eq!(message.performance_duration_ms(), Some(2500));
        assert!(message.provider_id.starts_with("unpriced:"));
    }
    #[test]
    fn estimates_have_a_separate_partition_and_no_measured_throughput() {
        let message = to_message(row(usage(true)).unwrap(), "fixture");
        assert_eq!(message.client, ESTIMATED_CLIENT_ID);
        assert_eq!(message.performance_duration_ms(), None);
        assert!(message.provider_id.starts_with("unpriced:"));
    }
    #[test]
    fn rejects_unknown_precision_and_inconsistent_counters() {
        for (key, value) in [
            ("estimated", Value::Null),
            ("prompt_tokens", json!(-1)),
            ("completion_tokens", json!(1.5)),
            ("total_tokens", json!(999)),
            ("cache_read_input_tokens", json!(130)),
            ("reasoning_tokens", json!(31)),
        ] {
            let mut input = usage(false);
            input[key] = value;
            assert!(row(input).is_none(), "{key}");
        }
        assert!(normalize(
            "x".into(),
            f64::INFINITY,
            "".into(),
            "".into(),
            &usage(false).to_string()
        )
        .is_none());
    }
    #[test]
    fn empty_sessions_use_distinct_native_call_ids() {
        let normalized = normalize(
            "call-2".into(),
            1234.0,
            "".into(),
            "".into(),
            &usage(false).to_string(),
        )
        .unwrap();
        assert_eq!(normalized.session, "call-2");
    }
    #[test]
    fn explicit_paths_only_and_foreign_home_scans_ignore_host_env() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_string_lossy().to_string();
        let db = temp
            .path()
            .join("custom.sqlite3")
            .to_string_lossy()
            .to_string();
        assert_eq!(
            configured_path(true, Some(db.clone()), Some(home.clone())),
            Some(PathBuf::from(&db))
        );
        assert_eq!(
            configured_path(true, None, Some(home.clone())),
            Some(temp.path().join("data/user/usage.sqlite3"))
        );
        assert!(configured_path(false, Some(db), Some(home)).is_none());
        assert!(configured_path(
            true,
            Some("relative.db".into()),
            Some(temp.path().to_string_lossy().into())
        )
        .is_none());
        assert!(configured_path(true, None, None).is_none());
    }
    #[test]
    fn ledger_scan_is_readonly_and_reported_estimated_are_disjoint() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("usage.sqlite3");
        let cache = temp.path().join("cache.json");
        let conn = fixture(&db);
        insert(&conn, "a", false);
        insert(&conn, "b", true);
        drop(conn);
        let before = std::fs::read(&db).unwrap();
        let exact = parse_with(&db, &cache, false);
        let estimated = parse_with(&db, &cache, true);
        assert_eq!((exact.len(), estimated.len()), (1, 1));
        assert_ne!(exact[0].dedup_key, estimated[0].dedup_key);
        assert_eq!(std::fs::read(&db).unwrap(), before);
        let cache_before = std::fs::metadata(&cache).unwrap().modified().unwrap();
        assert_eq!(parse_with(&db, &cache, false), exact);
        assert_eq!(
            std::fs::metadata(&cache).unwrap().modified().unwrap(),
            cache_before
        );
    }
    #[test]
    fn wal_commits_refresh_and_corruption_retains_last_complete_read() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("usage.sqlite3");
        let cache = temp.path().join("cache.json");
        let conn = fixture(&db);
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        insert(&conn, "a", false);
        assert_eq!(parse_with(&db, &cache, false).len(), 1);
        insert(&conn, "b", false);
        let full = parse_with(&db, &cache, false);
        assert_eq!(full.len(), 2);
        drop(conn);
        std::fs::write(&db, b"damaged database fixture").unwrap();
        assert_eq!(parse_with(&db, &cache, false), full);
    }
    #[test]
    fn read_budgets_never_publish_partial_scans() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("usage.sqlite3");
        let conn = fixture(&db);
        insert(&conn, "a", false);
        insert(&conn, "b", false);
        assert!(read_db(&db, 1, MAX_BYTES)
            .unwrap_err()
            .contains("row budget"));
        assert!(read_db(&db, MAX_ROWS, 1)
            .unwrap_err()
            .contains("byte budget"));
    }
}
