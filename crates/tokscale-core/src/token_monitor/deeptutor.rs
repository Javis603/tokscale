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
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
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

pub fn parse(scope: &Scope) -> Vec<UnifiedMessage> {
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
    parse_with(&db, &cache)
}

fn stat(path: &Path) -> io::Result<(u64, u128)> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(io::Error::other("ledger path is not a file"));
    }
    Ok((
        metadata.len(),
        metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos(),
    ))
}

fn fingerprint(db: &Path) -> io::Result<Fingerprint> {
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    let wal = match stat(Path::new(&wal)) {
        Ok(stamp) => Some(stamp),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    Ok(Fingerprint {
        database: stat(db)?,
        wal,
    })
}

fn bounded_read(reader: impl Read, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(max_bytes as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(io::Error::other("cache byte budget exceeded"));
    }
    Ok(bytes)
}

fn lock_cache(cache: &Path) -> io::Result<File> {
    if let Some(parent) = cache.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(cache.with_extension("lock"))?;
    // Contention must not delay collection. An unlocked reader may use an
    // atomically published cache, but must not overwrite another scan's result.
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(file)
}

fn parse_with(db: &Path, cache: &Path) -> Vec<UnifiedMessage> {
    let cache_lock = match lock_cache(cache) {
        Ok(file) => Some(file),
        Err(error) => {
            warn!(%error, "DeepTutor cache lock unavailable; reading without cache writes");
            None
        }
    };
    let saved = File::open(cache)
        .and_then(|file| bounded_read(file, MAX_BYTES))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Snapshot>(&bytes).ok())
        .filter(|snapshot| snapshot.version == CACHE_VERSION && snapshot.rows.len() <= MAX_ROWS);

    let fresh = (|| {
        let current = fingerprint(db).map_err(|e| e.to_string())?;
        if saved.as_ref().is_some_and(|s| s.fingerprint == current) {
            return Ok(None);
        }
        let rows = read_db(db, MAX_ROWS, MAX_BYTES)?;
        let snapshot = Snapshot {
            version: CACHE_VERSION,
            fingerprint: current.clone(),
            rows,
        };
        // A commit during the scan must be picked up on the next scan.
        if cache_lock.is_some() && fingerprint(db).ok().as_ref() == Some(&current) {
            if let Err(error) = save(cache, &snapshot) {
                warn!(%error, "DeepTutor cache write failed");
            }
        }
        Ok::<_, String>(Some(snapshot))
    })();
    let snapshot = match fresh {
        Ok(Some(snapshot)) => Some(snapshot),
        Ok(None) => saved,
        Err(error) => {
            warn!(%error, "DeepTutor usage read failed; retaining last complete snapshot");
            saved
        }
    };
    let namespace = js::path_namespace(db);
    snapshot
        .into_iter()
        .flat_map(|s| s.rows)
        .map(|row| to_message(row, &namespace))
        .collect()
}

fn save(path: &Path, snapshot: &Snapshot) -> io::Result<()> {
    let bytes = serde_json::to_vec(snapshot)?;
    if bytes.len() > MAX_BYTES {
        return Err(io::Error::other("cache byte budget exceeded"));
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = std::fs::write(&temporary, bytes)
        .and_then(|()| crate::fs_atomic::replace_file(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
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
    // Bound the scan before sorting. ORDER BY over an unindexed ledger could
    // materialize the entire database before the first row reaches our budget.
    let mut statement = conn
        .prepare(
            "SELECT call_id, started_at, session_id, turn_id, usage_json FROM llm_calls LIMIT ?",
        )
        .map_err(|e| e.to_string())?;
    let limit = max_rows.saturating_add(1).min(i64::MAX as usize) as i64;
    let mut cursor = statement.query([limit]).map_err(|e| e.to_string())?;
    let mut output = Vec::new();
    let mut ids = HashSet::new();
    let mut rows = 0usize;
    let mut bytes = 0usize;
    let mut skipped = 0usize;
    while let Some(row) = cursor.next().map_err(|e| e.to_string())? {
        rows += 1;
        if rows > max_rows {
            return Err("row budget exceeded".into());
        }
        let text = |index| {
            row.get_ref(index)
                .map_err(|e| e.to_string())?
                .as_str()
                .map_err(|e| e.to_string())
        };
        // Check borrowed SQLite values before allocating strings or parsing
        // JSON; one oversized cell must not bypass the byte budget.
        let id = text(0)?;
        let stamp: f64 = row.get(1).map_err(|e| e.to_string())?;
        let session = text(2)?;
        let turn = text(3)?;
        let raw = text(4)?;
        bytes = bytes.saturating_add(raw.len() + id.len() + session.len() + turn.len());
        if bytes > max_bytes || raw.len() > MAX_JSON_BYTES {
            return Err("byte budget exceeded".into());
        }
        if let Some(normalized) = normalize(id.into(), stamp, session.into(), turn.into(), raw) {
            if ids.insert(normalized.id.clone()) {
                output.push(normalized);
            }
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        warn!(
            skipped,
            "DeepTutor rows omitted: invalid counters or unknown precision"
        );
    }
    output.sort_unstable_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
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
        let messages = parse_with(&db, &cache);
        let (exact, estimated): (Vec<_>, Vec<_>) = messages
            .iter()
            .partition(|message| message.client == CLIENT_ID);
        assert_eq!((exact.len(), estimated.len()), (1, 1));
        assert_ne!(exact[0].dedup_key, estimated[0].dedup_key);
        assert_eq!(std::fs::read(&db).unwrap(), before);
        let cache_before = std::fs::metadata(&cache).unwrap().modified().unwrap();
        assert_eq!(parse_with(&db, &cache), messages);
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
        assert_eq!(parse_with(&db, &cache).len(), 1);
        insert(&conn, "b", false);
        let full = parse_with(&db, &cache);
        assert_eq!(full.len(), 2);
        drop(conn);
        std::fs::write(&db, b"damaged database fixture").unwrap();
        assert_eq!(parse_with(&db, &cache), full);
    }
    #[test]
    fn read_budgets_never_publish_partial_scans() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("usage.sqlite3");
        let conn = fixture(&db);
        insert(&conn, "a", false);
        insert(&conn, "b", false);
        assert_eq!(read_db(&db, 2, MAX_BYTES).unwrap().len(), 2);
        assert!(read_db(&db, 1, MAX_BYTES)
            .unwrap_err()
            .contains("row budget"));
        assert!(read_db(&db, MAX_ROWS, 1)
            .unwrap_err()
            .contains("byte budget"));
    }

    #[test]
    fn temporarily_missing_ledger_retains_last_complete_read() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("usage.sqlite3");
        let cache = temp.path().join("cache.json");
        let conn = fixture(&db);
        insert(&conn, "a", false);
        drop(conn);
        let complete = parse_with(&db, &cache);
        assert_eq!(complete.len(), 1);
        std::fs::rename(&db, temp.path().join("offline.sqlite3")).unwrap();
        assert_eq!(parse_with(&db, &cache), complete);
    }

    #[test]
    fn reported_replacement_moves_one_call_without_duplicate_or_stale_partition() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("usage.sqlite3");
        let cache = temp.path().join("cache.json");
        let conn = fixture(&db);
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        insert(&conn, "a", true);
        let estimated = parse_with(&db, &cache);
        assert_eq!(estimated.len(), 1);
        assert_eq!(estimated[0].client, ESTIMATED_CLIENT_ID);
        conn.execute(
            "UPDATE llm_calls SET usage_json=? WHERE call_id='a'",
            [usage(false).to_string()],
        )
        .unwrap();
        let reported = parse_with(&db, &cache);
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].client, CLIENT_ID);
        assert_eq!(reported[0].dedup_key, estimated[0].dedup_key);
        assert_eq!(reported[0].tokens, estimated[0].tokens);
        assert_eq!(parse_with(&db, &cache), reported);
    }

    #[test]
    fn valid_empty_ledger_clears_old_usage_but_invalid_schema_does_not() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("usage.sqlite3");
        let cache = temp.path().join("cache.json");
        let conn = fixture(&db);
        insert(&conn, "a", false);
        let complete = parse_with(&db, &cache);
        conn.execute_batch("ALTER TABLE llm_calls RENAME TO unavailable;")
            .unwrap();
        assert_eq!(parse_with(&db, &cache), complete);
        conn.execute_batch("ALTER TABLE unavailable RENAME TO llm_calls; DELETE FROM llm_calls;")
            .unwrap();
        assert!(parse_with(&db, &cache).is_empty());
        assert!(parse_with(&db, &cache).is_empty());
    }

    #[test]
    fn oversized_cell_retains_complete_cache_instead_of_truncating() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("usage.sqlite3");
        let cache = temp.path().join("cache.json");
        let conn = fixture(&db);
        insert(&conn, "a", false);
        let complete = parse_with(&db, &cache);
        let cached_bytes = std::fs::read(&cache).unwrap();
        insert(&conn, "b", false);
        conn.execute(
            "UPDATE llm_calls SET usage_json=? WHERE call_id='b'",
            ["x".repeat(MAX_JSON_BYTES + 1)],
        )
        .unwrap();
        assert_eq!(parse_with(&db, &cache), complete);
        assert_eq!(std::fs::read(&cache).unwrap(), cached_bytes);
    }

    #[test]
    fn cache_reads_stop_at_budget_and_invalid_cache_is_rebuilt() {
        let mut reader = io::Cursor::new(vec![b'x'; 1024]);
        assert!(bounded_read(&mut reader, 4).is_err());
        assert_eq!(
            reader.position(),
            5,
            "must not read an oversized cache to its end"
        );
        assert_eq!(bounded_read(io::Cursor::new(b"1234"), 4).unwrap(), b"1234");

        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("usage.sqlite3");
        let cache = temp.path().join("cache.json");
        let conn = fixture(&db);
        insert(&conn, "a", false);
        let complete = parse_with(&db, &cache);
        std::fs::write(&cache, b"{truncated cache").unwrap();
        assert_eq!(parse_with(&db, &cache), complete);
        let restored: Snapshot = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
        assert_eq!(restored.rows.len(), 1);
    }

    #[test]
    fn busy_or_unavailable_cache_lock_still_reads_without_cache_writes() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("usage.sqlite3");
        let cache = temp.path().join("cache.json");
        let conn = fixture(&db);
        insert(&conn, "a", false);
        assert_eq!(parse_with(&db, &cache).len(), 1);
        let cached_bytes = std::fs::read(&cache).unwrap();
        let _lock = lock_cache(&cache).unwrap();
        insert(&conn, "b", true);
        assert_eq!(parse_with(&db, &cache).len(), 2);
        assert_eq!(std::fs::read(&cache).unwrap(), cached_bytes);

        let invalid_parent = temp.path().join("not-a-directory");
        std::fs::write(&invalid_parent, b"fixture").unwrap();
        assert_eq!(parse_with(&db, &invalid_parent.join("cache.json")).len(), 2);
        assert_eq!(std::fs::read(&invalid_parent).unwrap(), b"fixture");
    }
}
