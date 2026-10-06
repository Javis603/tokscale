//! CatPaw assistant local usage: the domestic `catpaw-moon` and overseas
//! `catpaw-overseas` SQLite stores under the platform app-data root
//! (`~/Library/Application Support` on macOS, the native roaming app-data
//! directory on Windows). Explicit homes and a redirected Windows `HOME`
//! resolve the root under that home instead of the host's live profile.
//!
//! Usage is projected from `ui_sdk_messages` and model selection from
//! `sessions`; no message body, title or credential is read. The usage buckets
//! are disjoint: `promptTokens` is uncached input, `cacheReadTokens` and
//! `cacheWriteTokens` are separate additive buckets, and their checked sum
//! must match one of the recorded totals: `usage.totalTokens`, or the sibling
//! `contextInfo.totalUsageTokens` on rows whose stored total predates
//! cache-read accounting. Reasoning and 1-hour cache-write tokens are not
//! independently persisted and stay zero instead of being inferred.
//!
//! Model ids are edition-specific. Auto, routing, unknown and conflicting ids
//! are never mapped to a catalog model and stay out of automatic pricing.
//!
//! A changed source is re-read in one read-only transaction and replaces its
//! whole previous snapshot; each source is fingerprinted by database and WAL
//! size/mtime, so unchanged ones skip the re-read, and any failed read
//! (schema, counters, budget, I/O) retains that source's last complete
//! snapshot. A stable `catpaw.lock` sidecar serializes cache load, source
//! scans and publication, so a concurrent scan cannot overwrite a newer
//! result; without the lock, sources are still read but no cache is written.

use super::{js, Scope};
use crate::sessions::utils::open_readonly_sqlite;
use crate::sessions::UnifiedMessage;
use crate::TokenBreakdown;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::warn;

pub const CLIENT_ID: &str = "catpaw";
const MAX_DBS: usize = 256;
const MAX_ROWS: usize = 100_000;
const MAX_BYTES: usize = 50 * 1024 * 1024;
const CACHE_VERSION: u32 = 5;

#[derive(Clone, Copy)]
enum Edition {
    Domestic,
    Overseas,
}

impl Edition {
    fn directory(self) -> &'static str {
        match self {
            Self::Domestic => "catpaw-moon",
            Self::Overseas => "catpaw-overseas",
        }
    }

    // App model-types snapshots: domestic 2026-10-04, overseas 2026-10-06.
    // Integers are edition-specific; rateMultiplier is not a token price.
    fn model(self, id: i64) -> Option<&'static str> {
        match (self, id) {
            (_, 0) | (Self::Overseas, 10000003) => Some("auto"),
            (Self::Domestic, 63) => Some("deepseek-v4-flash"),
            (Self::Domestic, 64) => Some("deepseek-v4-pro"),
            (Self::Domestic, 70) => Some("MiniMax-M3"),
            (Self::Domestic, 77) | (Self::Overseas, 10000002) => Some("LongCat-2.0"),
            (Self::Domestic, 83) | (Self::Overseas, 10000007) => Some("kimi-k3"),
            (Self::Domestic, 89) | (Self::Overseas, 10000005) => Some("glm-5.3"),
            (Self::Domestic, 91) | (Self::Overseas, 10000006) => Some("glm-5.3-flash"),
            (Self::Domestic, 98) => Some("glm-5.3-flashx"),
            (Self::Overseas, 10000001) => Some("gpt-5.6-terra"),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Fingerprint {
    db: (u64, u128),
    wal: Option<(u64, u128)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Source {
    fingerprint: Fingerprint,
    messages: Vec<UnifiedMessage>,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct Cache {
    version: u32,
    sources: BTreeMap<String, Source>,
}

pub fn parse(scope: &Scope) -> Vec<UnifiedMessage> {
    if !cfg!(any(target_os = "macos", windows)) {
        // No Linux product root is read; fixtures still exercise the reader there.
        return Vec::new();
    }
    let support = PathBuf::from(
        crate::clients::PathRoot::AppData
            .resolve_with_env_strategy(scope.home_dir, scope.use_env_roots),
    );
    parse_support(
        &support,
        &crate::paths::get_cache_dir().join("token-monitor/catpaw.json"),
    )
}

fn database_name(name: &str) -> bool {
    name.strip_prefix("catpaw-memory-")
        .and_then(|name| name.strip_suffix(".db"))
        .is_some_and(|scope| {
            !scope.is_empty()
                && scope != "anon"
                && scope
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        })
}

fn databases(root: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_str().is_some_and(database_name) {
            paths.push(entry.path());
            if paths.len() > MAX_DBS {
                return Err(io::Error::other("database discovery budget exceeded"));
            }
        }
    }
    paths.sort();
    Ok(paths)
}

fn file_stamp(path: &Path) -> io::Result<(u64, u128)> {
    let meta = std::fs::metadata(path)?;
    let modified = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    Ok((meta.len(), modified))
}

fn fingerprint(path: &Path) -> io::Result<Fingerprint> {
    let mut sidecar = path.as_os_str().to_owned();
    sidecar.push("-wal");
    let wal = match file_stamp(Path::new(&sidecar)) {
        Ok(stamp) => Some(stamp),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    Ok(Fingerprint {
        db: file_stamp(path)?,
        wal,
    })
}

fn lock_cache(cache_path: &Path) -> io::Result<File> {
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(cache_path.with_extension("lock"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn parse_support(support: &Path, cache_path: &Path) -> Vec<UnifiedMessage> {
    // Lock a stable sidecar, not the JSON inode that atomic replacement removes.
    // Keep the guard through loading, source scans and publication so an older
    // concurrent scan cannot overwrite another scan's last complete read.
    let cache_lock = match lock_cache(cache_path) {
        Ok(file) => Some(file),
        Err(error) => {
            warn!(%error, "CatPaw cache lock unavailable; reading without cache writes");
            None
        }
    };
    let scope = format!("{}:", js::path_namespace(support));
    let loaded = std::fs::read(cache_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Cache>(&bytes).ok())
        .filter(|cache| cache.version == CACHE_VERSION)
        .unwrap_or_default();
    let mut next = loaded.clone();
    next.version = CACHE_VERSION;
    for edition in [Edition::Domestic, Edition::Overseas] {
        let prefix = format!("{scope}{}:", edition.directory());
        let root = support.join(edition.directory());
        let paths = match databases(&root) {
            Ok(paths) => paths,
            Err(_) => {
                warn!(
                    edition = edition.directory(),
                    "CatPaw discovery failed; retaining last complete read"
                );
                continue;
            }
        };
        let sources: BTreeMap<_, _> = paths
            .into_iter()
            .map(|path| (format!("{prefix}{}", js::path_namespace(&path)), path))
            .collect();
        next.sources
            .retain(|key, _| !key.starts_with(&prefix) || sources.contains_key(key));
        for (key, path) in sources {
            let fresh = (|| {
                let current =
                    fingerprint(&path).map_err(|_| "source metadata unavailable".to_string())?;
                if next
                    .sources
                    .get(&key)
                    .is_some_and(|cached| cached.fingerprint == current)
                {
                    return Ok(None);
                }
                let messages = read_db(&path, edition, &key)?;
                Ok::<_, String>(Some(Source {
                    fingerprint: current,
                    messages,
                }))
            })();
            match fresh {
                Ok(Some(source)) => {
                    next.sources.insert(key, source);
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(edition = edition.directory(), %error, "CatPaw read failed; retaining last complete read")
                }
            }
        }
    }
    if cache_lock.is_some() && next != loaded {
        if let (Some(parent), Ok(bytes)) = (cache_path.parent(), serde_json::to_vec(&next)) {
            if std::fs::create_dir_all(parent).is_ok() {
                let temp = cache_path.with_extension(format!("json.{}.tmp", std::process::id()));
                if std::fs::write(&temp, bytes).is_ok() {
                    let _ = crate::fs_atomic::replace_file(&temp, cache_path);
                }
            }
        }
    }
    next.sources
        .into_iter()
        .filter(|(key, _)| key.starts_with(&scope))
        .flat_map(|(_, source)| source.messages)
        .collect()
}

fn count(usage: &Value, key: &str) -> Result<i64, String> {
    usage
        .get(key)
        .and_then(Value::as_i64)
        .filter(|n| *n >= 0 && *n <= js::MAX_SAFE_INTEGER as i64)
        .ok_or_else(|| format!("invalid token field {key}"))
}

fn tokens(usage: &Value, recorded_total: Option<i64>) -> Result<TokenBreakdown, String> {
    let tokens = TokenBreakdown {
        input: count(usage, "promptTokens")?,
        output: count(usage, "completionTokens")?,
        cache_read: count(usage, "cacheReadTokens")?,
        cache_write: count(usage, "cacheWriteTokens")?,
        ..Default::default()
    };
    let total = [
        tokens.input,
        tokens.output,
        tokens.cache_read,
        tokens.cache_write,
    ]
    .into_iter()
    .try_fold(0i64, |sum, n| sum.checked_add(n))
    .ok_or("token sum overflow")?;
    if total != count(usage, "totalTokens")? && Some(total) != recorded_total {
        return Err("token sum differs from the recorded totals".into());
    }
    Ok(tokens)
}

// Older builds stored usage.totalTokens without cache reads; the sibling
// contextInfo.totalUsageTokens carries the all-bucket total. Anything that is
// not a nonnegative integral JS-safe number leaves the anchor unavailable
// rather than failing the row.
fn recorded_total(value: Option<rusqlite::types::Value>) -> Option<i64> {
    let (n, integral) = match value? {
        rusqlite::types::Value::Integer(n) => (n as f64, true),
        rusqlite::types::Value::Real(n) => (n, n.fract() == 0.0),
        _ => return None,
    };
    (integral && n.is_finite() && (0.0..=js::MAX_SAFE_INTEGER).contains(&n)).then_some(n as i64)
}

fn model(edition: Edition, extra: &Value) -> (String, bool) {
    let current = [
        "persistedModelId",
        "persistedModelMode",
        "persistedModelSelection",
    ]
    .iter()
    .any(|key| extra.get(key).is_some_and(|v| !v.is_null()));
    let (id_key, mode_key, selection_key) = if current {
        (
            "persistedModelId",
            "persistedModelMode",
            "persistedModelSelection",
        )
    } else {
        (
            "initialSelectedModelId",
            "initialModelMode",
            "initialModelSelection",
        )
    };
    let selection = &extra[selection_key];
    let mode = extra[mode_key].as_str();
    let persisted = extra[id_key].as_i64();
    let safe_model_id = |value: &Value| {
        value
            .as_i64()
            .filter(|id| id.unsigned_abs() <= js::MAX_SAFE_INTEGER as u64)
    };
    let selected = safe_model_id(&selection["modelId"]);
    // The app rejects partial selections and invalid lastSelectedModelId values.
    let selection_auto = selection["isAuto"].as_bool().filter(|_| {
        selected.is_some()
            && selection
                .get("lastSelectedModelId")
                .is_none_or(|value| safe_model_id(value).is_some())
    });
    if selection_auto == Some(true) || (selection_auto.is_none() && mode == Some("auto")) {
        return ("auto".into(), false);
    }
    if matches!((persisted, selected), (Some(a), Some(b)) if a != b) {
        return (format!("{}-model-conflict", edition.directory()), false);
    }
    let id = selected
        .filter(|_| selection_auto.is_some())
        .or(persisted)
        .or(match mode {
            Some("lite") => Some(10001),
            Some("pro") => Some(10002),
            Some("max") => Some(10003),
            _ => None,
        });
    match id {
        Some(id) => match edition.model(id) {
            Some("auto") => ("auto".into(), false),
            Some(name) => (name.into(), true),
            None => (format!("{}-model-{id}", edition.directory()), false),
        },
        None => (format!("{}-model-unknown", edition.directory()), false),
    }
}

// SQLite projects only numeric usage, the recorded totals and model
// selection, never body/title. The row limit is one past the read budget: an
// over-budget source must fail the row-budget check instead of arriving
// truncated as a valid snapshot.
const USAGE_SQL: &str = "
SELECT m.conversation_id, m.message_id, m.created_at_ms, m.updated_at_ms,
       m.schema_version, json_extract(m.payload, '$.extra.contextInfo.usage'),
       json_extract(m.payload, '$.extra.contextInfo.totalUsageTokens'),
       (SELECT json_object(
          'persistedModelId', json_extract(s.extra, '$.persistedModelId'),
          'persistedModelMode', json_extract(s.extra, '$.persistedModelMode'),
          'persistedModelSelection', json_extract(s.extra, '$.persistedModelSelection'),
          'initialSelectedModelId', json_extract(s.extra, '$.initialSelectedModelId'),
          'initialModelMode', json_extract(s.extra, '$.initialModelMode'),
          'initialModelSelection', json_extract(s.extra, '$.initialModelSelection'))
        FROM sessions s WHERE s.conversation_id = m.conversation_id LIMIT 1)
FROM ui_sdk_messages m
WHERE m.role = 'assistant' AND json_type(m.payload, '$.extra.contextInfo.usage') IS NOT NULL
ORDER BY m.updated_at_ms, m.seq LIMIT ?1";

const USAGE_ROW_LIMIT: i64 = (MAX_ROWS + 1) as i64;

fn read_db(path: &Path, edition: Edition, namespace: &str) -> Result<Vec<UnifiedMessage>, String> {
    let mut conn = open_readonly_sqlite(path).map_err(|_| "database open failed")?;
    conn.busy_timeout(Duration::from_secs(3))
        .map_err(|_| "busy timeout setup failed")?;
    conn.execute_batch("PRAGMA query_only=ON")
        .map_err(|_| "query-only setup failed")?;
    let tx = conn.transaction().map_err(|_| "read transaction failed")?;
    let mut statement = tx
        .prepare(USAGE_SQL)
        .map_err(|_| "unsupported database schema")?;
    let mut rows = statement
        .query(rusqlite::params![USAGE_ROW_LIMIT])
        .map_err(|_| "usage query failed")?;
    let mut messages = BTreeMap::new();
    let mut bytes = 0usize;
    let mut count = 0usize;
    while let Some(row) = rows.next().map_err(|_| "usage read incomplete")? {
        count += 1;
        if count > MAX_ROWS {
            return Err("row budget exceeded".into());
        }
        let decode = |_| "usage row decode failed".to_string();
        let conversation: String = row.get(0).map_err(decode)?;
        let id: String = row.get(1).map_err(decode)?;
        let created: Option<i64> = row.get(2).map_err(decode)?;
        let updated: i64 = row.get(3).map_err(decode)?;
        let schema: i64 = row.get(4).map_err(decode)?;
        let usage: String = row.get(5).map_err(decode)?;
        let recorded: Option<rusqlite::types::Value> = row.get(6).map_err(decode)?;
        let selection: Option<String> = row.get(7).map_err(decode)?;
        bytes +=
            conversation.len() + id.len() + usage.len() + selection.as_ref().map_or(0, String::len);
        if bytes > MAX_BYTES {
            return Err("byte budget exceeded".into());
        }
        if schema != 1 || conversation.is_empty() || id.is_empty() {
            return Err("unsupported message schema or missing identity".into());
        }
        let usage: Value = serde_json::from_str(&usage).map_err(|_| "invalid usage JSON")?;
        let tokens = tokens(&usage, recorded_total(recorded))?;
        let selection: Value = serde_json::from_str(selection.as_deref().unwrap_or("{}"))
            .map_err(|_| "invalid model selection JSON")?;
        let (model, known) = model(edition, &selection);
        let provider = if known {
            crate::provider_identity::inferred_provider_from_model(&model).unwrap_or(CLIENT_ID)
        } else {
            "unpriced:catpaw"
        };
        let valid_time = |n: &i64| *n > 0 && *n <= 8_640_000_000_000_000;
        let timestamp = created
            .filter(valid_time)
            .or_else(|| Some(updated).filter(valid_time))
            .unwrap_or(0);
        let session = format!("catpaw:{namespace}:{conversation}");
        let dedup = format!("{session}:{id}");
        // Same SDK id can move between seq slots after a session rewrite.
        // SQL orders by update time so the latest copy replaces earlier ones.
        messages.insert(
            dedup.clone(),
            UnifiedMessage::new_with_dedup(
                CLIENT_ID,
                model,
                provider,
                session,
                timestamp,
                tokens,
                0.0,
                Some(dedup),
            ),
        );
    }
    Ok(messages.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::CostSource;
    use serde_json::json;

    const TIME: i64 = 1_790_848_800_000;

    fn database(
        support: &Path,
        edition: Edition,
        scope: &str,
        id: i64,
    ) -> (PathBuf, rusqlite::Connection) {
        let root = support.join(edition.directory());
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(format!("catpaw-memory-{scope}.db"));
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE sessions (id TEXT, extra TEXT,
          conversation_id TEXT GENERATED ALWAYS AS (json_extract(extra, '$.conversationId')) STORED);
          CREATE TABLE ui_sdk_messages (conversation_id TEXT, seq INTEGER, message_id TEXT, role TEXT,
          payload TEXT, created_at_ms INTEGER, updated_at_ms INTEGER, schema_version INTEGER,
          PRIMARY KEY(conversation_id, seq));").unwrap();
        let extra = json!({"conversationId":"c", "initialModelMode":"auto", "persistedModelId":id,
            "persistedModelSelection":{"isAuto":false,"modelId":id},"title":"PRIVATE TITLE"});
        conn.execute(
            "INSERT INTO sessions(id,extra) VALUES('s',?1)",
            [extra.to_string()],
        )
        .unwrap();
        (path, conn)
    }

    fn usage(input: i64, cached: i64, write: i64, output: i64) -> Value {
        json!({"promptTokens":input,"cacheReadTokens":cached,"cacheWriteTokens":write,
            "completionTokens":output,"totalTokens":input+cached+write+output})
    }

    fn insert(conn: &rusqlite::Connection, seq: i64, id: &str, usage: Value, created: Option<i64>) {
        let payload = json!({"content":"PRIVATE BODY", "extra":{"contextInfo":{"usage":usage}}});
        conn.execute(
            "INSERT OR REPLACE INTO ui_sdk_messages VALUES('c',?1,?2,'assistant',?3,?4,?5,1)",
            rusqlite::params![seq, id, payload.to_string(), created, TIME + seq],
        )
        .unwrap();
    }

    #[tokio::test]
    #[serial_test::serial]
    #[cfg(any(target_os = "macos", windows))]
    async fn explicit_homes_do_not_read_the_hosts_app_data_in_either_lane() {
        use crate::{
            parse_local_clients, parse_local_unified_messages_with_pricing, LocalParseOptions,
        };
        let config = tempfile::tempdir().unwrap();
        let mut env =
            crate::paths::test_env::EnvGuard::capture(&["TOKSCALE_CONFIG_DIR", "APPDATA"]);
        env.set("TOKSCALE_CONFIG_DIR", config.path().to_str().unwrap());
        // Poison the ambient Windows root: neither explicit scan may read it.
        env.set("APPDATA", config.path().to_str().unwrap());
        let (_, host) = database(config.path(), Edition::Domestic, "host", 91);
        insert(&host, 2, "id", usage(9999, 0, 0, 1), Some(TIME));
        for input in [10, 20] {
            let home = tempfile::tempdir().unwrap();
            let support = home.path().join(if cfg!(windows) {
                "AppData/Roaming"
            } else {
                "Library/Application Support"
            });
            let (_, conn) = database(&support, Edition::Domestic, "account", 91);
            insert(&conn, 2, "id", usage(input, 0, 0, 1), Some(TIME));
            let options = LocalParseOptions {
                home_dir: Some(home.path().to_string_lossy().into_owned()),
                use_env_roots: false,
                clients: Some(vec![CLIENT_ID.into()]),
                ..Default::default()
            };
            let local = parse_local_clients(options.clone()).unwrap();
            assert_eq!(local.messages.len(), 1);
            assert_eq!(local.messages[0].input, input);
            let unified = parse_local_unified_messages_with_pricing(options, None)
                .await
                .unwrap();
            assert_eq!(unified.len(), 1);
            assert_eq!(unified[0].tokens.input, input);
        }
    }

    #[test]
    fn token_sums_anchor_to_either_recorded_total_across_writer_eras() {
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = database(dir.path(), Edition::Domestic, "eras", 91);
        insert(&conn, 1, "era-b", usage(100, 20, 0, 30), Some(TIME));
        // Older writers kept usage.totalTokens at prompt + completion, with
        // the sibling totalUsageTokens carrying all four buckets.
        let older = json!({"promptTokens":177,"cacheReadTokens":98944,"cacheWriteTokens":0,
            "completionTokens":38,"totalTokens":215});
        let payload = json!({"content":"x","extra":{"contextInfo":{
            "usage":older,"totalUsageTokens":99159}}});
        conn.execute(
            "INSERT OR REPLACE INTO ui_sdk_messages VALUES('c',2,'era-a','assistant',?1,?2,?3,1)",
            rusqlite::params![payload.to_string(), TIME, TIME + 2],
        )
        .unwrap();
        let messages = read_db(&path, Edition::Domestic, "eras").unwrap();
        assert_eq!(messages.len(), 2);
        let older = &messages[0];
        assert_eq!(older.dedup_key.as_deref(), Some("catpaw:eras:c:era-a"));
        assert_eq!(
            (
                older.tokens.input,
                older.tokens.output,
                older.tokens.cache_read,
                older.tokens.cache_write
            ),
            (177, 38, 98944, 0)
        );
    }

    #[test]
    fn a_sum_matching_neither_recorded_total_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = database(dir.path(), Edition::Domestic, "mismatch", 91);
        let bad = json!({"promptTokens":177,"cacheReadTokens":98944,"cacheWriteTokens":0,
            "completionTokens":38,"totalTokens":999});
        let payload = json!({"content":"x","extra":{"contextInfo":{
            "usage":bad,"totalUsageTokens":1.5}}});
        conn.execute(
            "INSERT OR REPLACE INTO ui_sdk_messages VALUES('c',1,'bad','assistant',?1,?2,?3,1)",
            rusqlite::params![payload.to_string(), TIME, TIME + 1],
        )
        .unwrap();
        assert!(read_db(&path, Edition::Domestic, "mismatch").is_err());
    }

    #[test]
    fn row_budget_rejects_an_over_limit_source_and_keeps_the_boundary_read() {
        let bulk = |scope: &str, rows: usize| {
            let dir = tempfile::tempdir().unwrap();
            let (path, conn) = database(dir.path(), Edition::Domestic, scope, 91);
            let payload =
                json!({"content":"x","extra":{"contextInfo":{"usage":usage(1,0,0,1)}}}).to_string();
            let mut stmt = conn
                .prepare(
                    "INSERT OR REPLACE INTO ui_sdk_messages VALUES('c',?1,?2,'assistant',?3,?4,?5,1)",
                )
                .unwrap();
            conn.execute_batch("BEGIN").unwrap();
            for seq in 0..rows as i64 {
                stmt.execute(rusqlite::params![
                    seq,
                    format!("m{seq}"),
                    payload,
                    TIME,
                    TIME + seq
                ])
                .unwrap();
            }
            conn.execute_batch("COMMIT").unwrap();
            (path, dir)
        };
        let (boundary, _keep) = bulk("boundary", MAX_ROWS);
        assert_eq!(
            read_db(&boundary, Edition::Domestic, "boundary")
                .unwrap()
                .len(),
            MAX_ROWS
        );
        let (over, _keep) = bulk("over", MAX_ROWS + 1);
        assert_eq!(
            read_db(&over, Edition::Domestic, "over"),
            Err("row budget exceeded".into())
        );
    }

    #[test]
    fn both_editions_and_accounts_keep_disjoint_cache_buckets_and_identities() {
        let dir = tempfile::tempdir().unwrap();
        let (_, cn) = database(dir.path(), Edition::Domestic, "alpha-1_2", 91);
        let (_, global) = database(dir.path(), Edition::Overseas, "beta", 10000001);
        let (_, other) = database(dir.path(), Edition::Domestic, "other", 91);
        insert(&cn, 2, "same-id", usage(1200, 500, 80, 340), Some(TIME));
        insert(&global, 2, "same-id", usage(1200, 500, 80, 340), None);
        insert(&other, 2, "same-id", usage(1, 0, 0, 1), Some(TIME));
        let (_, anon) = database(dir.path(), Edition::Domestic, "anon", 91);
        insert(&anon, 2, "same-id", usage(99, 0, 0, 1), Some(TIME));
        let cache = dir.path().join("cache.json");
        let messages = parse_support(dir.path(), &cache);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages.iter().map(|m| m.tokens.total()).sum::<i64>(), 4242);
        let global = messages
            .iter()
            .find(|m| m.model_id == "gpt-5.6-terra")
            .unwrap();
        assert_eq!(
            (
                global.tokens.input,
                global.tokens.output,
                global.tokens.cache_read,
                global.tokens.cache_write
            ),
            (1200, 340, 500, 80)
        );
        assert_eq!(global.timestamp, TIME + 2);
        assert_eq!(global.tokens.reasoning, 0);
        assert_eq!(global.cost_source, CostSource::Unknown);
        assert_eq!(
            messages
                .iter()
                .map(|m| m.dedup_key.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );
        let stored = std::fs::read_to_string(&cache).unwrap();
        assert!(!stored.contains("PRIVATE"));
        let written = std::fs::metadata(&cache).unwrap().modified().unwrap();
        assert_eq!(parse_support(dir.path(), &cache), messages);
        assert_eq!(
            std::fs::metadata(cache).unwrap().modified().unwrap(),
            written
        );
    }

    #[test]
    fn the_seven_observed_rounds_keep_their_original_totals() {
        let dir = tempfile::tempdir().unwrap();
        let (_, cn) = database(dir.path(), Edition::Domestic, "sample-cn", 91);
        let (_, overseas) = database(dir.path(), Edition::Overseas, "sample-global", 10000001);
        // Numeric-only transcription of the 2026-10-06 rounds; no chat content.
        for (conn, rows, expected) in [
            (
                &cn,
                &[
                    (5513, 15232, 83),
                    (10942, 9408, 45),
                    (27470, 0, 20),
                    (18381, 9216, 41),
                ][..],
                96351,
            ),
            (
                &overseas,
                &[(11734, 10624, 110), (22269, 0, 91), (20388, 0, 13)][..],
                65229,
            ),
        ] {
            for (index, &(input, cached, output)) in rows.iter().enumerate() {
                insert(
                    conn,
                    index as i64,
                    &format!("round-{index}"),
                    usage(input, cached, 0, output),
                    Some(TIME),
                );
            }
            let total: i64 = conn.query_row("SELECT sum(json_extract(payload,'$.extra.contextInfo.usage.totalTokens')) FROM ui_sdk_messages",[],|row| row.get(0)).unwrap();
            assert_eq!(total, expected);
        }
        cn.execute(
            "INSERT INTO ui_sdk_messages VALUES('c',20,'metadata','assistant','{}',NULL,?1,1)",
            [TIME],
        )
        .unwrap();
        cn.execute(
            "INSERT INTO ui_sdk_messages VALUES('c',21,'user','user','{}',NULL,?1,1)",
            [TIME],
        )
        .unwrap();
        let messages = parse_support(dir.path(), &dir.path().join("cache.json"));
        assert_eq!(messages.len(), 7);
        assert_eq!(
            messages.iter().map(|m| m.tokens.total()).sum::<i64>(),
            161580
        );
        assert_eq!(
            messages
                .iter()
                .filter(|m| m.model_id == "glm-5.3-flash")
                .map(|m| m.tokens.total())
                .sum::<i64>(),
            96351
        );
    }

    #[test]
    fn automatic_prices_refuse_routes_but_explicit_custom_prices_still_work() {
        use crate::pricing::{custom::CustomPricing, litellm::ModelPricing, PricingService};
        use std::collections::HashMap;
        let rates = ModelPricing {
            input_cost_per_token: Some(0.000001),
            output_cost_per_token: Some(0.000002),
            cache_read_input_token_cost: Some(0.0000001),
            cache_creation_input_token_cost: Some(0.000001),
            ..Default::default()
        };
        let catalog = HashMap::from([
            ("auto".to_string(), rates.clone()),
            ("gpt-5.6-terra".to_string(), rates.clone()),
        ]);
        let pricing = PricingService::new(catalog.clone(), HashMap::new());
        let usage = tokens(&usage(1200, 500, 80, 340), None).unwrap();
        assert_eq!(
            pricing.calculate_cost_with_provider("auto", Some("unpriced:catpaw"), &usage),
            0.0
        );
        assert!(
            pricing.calculate_cost_with_provider("gpt-5.6-terra", Some("openai"), &usage) > 0.0
        );
        let custom = CustomPricing::from_models(HashMap::from([("auto".to_string(), rates)]));
        let pricing = PricingService::new_with_custom(custom, catalog, HashMap::new());
        assert!(
            pricing.calculate_cost_with_provider("auto", Some("unpriced:catpaw"), &usage) > 0.0
        );
    }

    #[test]
    fn current_selection_wins_and_routes_unknowns_and_conflicts_are_unpriced() {
        assert_eq!(
            model(
                Edition::Domestic,
                &json!({"persistedModelMode":"auto", "persistedModelId":91,
                "persistedModelSelection":{"isAuto":false,"modelId":91}})
            ),
            ("glm-5.3-flash".into(), true)
        );
        for invalid in [
            json!({"modelId":91}),
            json!({"isAuto":false,"modelId":91,"lastSelectedModelId":"91"}),
            json!({"isAuto":false,"modelId":91,"lastSelectedModelId":null}),
            json!({"isAuto":false,"modelId":91,"lastSelectedModelId":9_007_199_254_740_992i64}),
        ] {
            assert_eq!(
                model(
                    Edition::Domestic,
                    &json!({
                        "persistedModelMode":"pro", "persistedModelSelection":invalid
                    })
                ),
                ("catpaw-moon-model-10002".into(), false)
            );
        }
        for (edition, id, name) in [
            (Edition::Domestic, 91, "glm-5.3-flash"),
            (Edition::Overseas, 10000006, "glm-5.3-flash"),
        ] {
            assert_eq!(
                model(
                    edition,
                    &json!({"initialModelMode":"auto","persistedModelId":id})
                ),
                (name.into(), true)
            );
            assert!(
                !model(
                    edition,
                    &json!({"persistedModelId":id,"persistedModelSelection":{"modelId":-1}})
                )
                .1
            );
        }
        for id in [0, 10000, 10001, 10002, 10003, 10000003, -1, 987654] {
            assert!(!model(Edition::Overseas, &json!({"persistedModelId":id})).1);
        }
        assert_eq!(
            model(
                Edition::Domestic,
                &json!({"persistedModelMode":"auto","persistedModelSelection":{"isAuto":true,"modelId":91}})
            ),
            ("auto".into(), false)
        );
        assert_eq!(
            model(Edition::Domestic, &json!({"initialSelectedModelId":91})),
            ("glm-5.3-flash".into(), true)
        );
        assert!(!model(Edition::Overseas, &json!({"persistedModelId":91})).1);
    }

    #[test]
    fn invalid_usage_rejects_the_whole_source() {
        let mut valid = usage(1, 2, 3, 4);
        for invalid in [
            json!(-1),
            json!(1.5),
            json!("2"),
            json!(null),
            json!(i64::MAX),
        ] {
            valid["promptTokens"] = invalid;
            assert!(tokens(&valid, None).is_err());
        }
        assert!(tokens(&json!({}), None).is_err());
        let mut bad = usage(1, 2, 3, 4);
        bad["totalTokens"] = json!(9);
        assert!(tokens(&bad, None).is_err());
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = database(dir.path(), Edition::Domestic, "account", 91);
        insert(&conn, 2, "good", usage(1, 0, 0, 1), Some(TIME));
        insert(&conn, 3, "bad", bad, Some(TIME));
        assert!(read_db(&path, Edition::Domestic, "test").is_err());
    }

    #[test]
    fn wal_updates_rewrites_deletes_and_failures_replace_or_retain_complete_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = database(dir.path(), Edition::Domestic, "account", 91);
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        let cache = dir.path().join("cache.json");
        insert(&conn, 2, "id", usage(10, 0, 0, 5), Some(TIME));
        assert_eq!(parse_support(dir.path(), &cache)[0].tokens.total(), 15);
        insert(&conn, 7, "id", usage(20, 0, 0, 5), Some(TIME));
        let fresh = parse_support(dir.path(), &cache);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].tokens.total(), 25);
        insert(&conn, 8, "bad", json!({"promptTokens":-1}), Some(TIME));
        assert_eq!(parse_support(dir.path(), &cache), fresh);
        conn.execute("DELETE FROM ui_sdk_messages WHERE message_id='bad'", [])
            .unwrap();
        conn.execute("DELETE FROM ui_sdk_messages", []).unwrap();
        assert!(parse_support(dir.path(), &cache).is_empty());
        insert(&conn, 2, "id", usage(30, 0, 0, 5), Some(TIME));
        let good = parse_support(dir.path(), &cache);
        drop(conn);
        std::fs::write(&path, b"corrupt db with different size").unwrap();
        assert_eq!(parse_support(dir.path(), &cache), good);
        std::fs::remove_file(path).unwrap();
        assert!(parse_support(dir.path(), &cache).is_empty());
    }

    #[test]
    fn a_failed_scan_of_another_home_cannot_reuse_this_homes_cache() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("home-a");
        let second = dir.path().join("home-b");
        let cache = dir.path().join("shared-cache.json");
        let (_, conn) = database(&first, Edition::Domestic, "account", 91);
        insert(&conn, 2, "id", usage(1, 0, 0, 1), Some(TIME));
        assert_eq!(parse_support(&first, &cache).len(), 1);
        drop(conn);
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(
            second.join(Edition::Domestic.directory()),
            b"not a directory",
        )
        .unwrap();
        assert!(parse_support(&second, &cache).is_empty());
        // Scanning a failed B must not discard A's last successful read.
        let root = first.join(Edition::Domestic.directory());
        std::fs::rename(&root, first.join("unavailable")).unwrap();
        std::fs::write(&root, b"not a directory").unwrap();
        assert_eq!(parse_support(&first, &cache).len(), 1);
    }

    #[test]
    fn missing_roots_are_empty_but_discovery_failures_keep_previous_sources() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache.json");
        assert!(parse_support(dir.path(), &cache).is_empty());
        let (_, conn) = database(dir.path(), Edition::Domestic, "account", 91);
        insert(&conn, 2, "id", usage(1, 0, 0, 1), Some(TIME));
        drop(conn);
        let good = parse_support(dir.path(), &cache);
        let root = dir.path().join(Edition::Domestic.directory());
        let moved = dir.path().join("unavailable");
        std::fs::rename(&root, &moved).unwrap();
        std::fs::write(&root, b"not a directory").unwrap();
        assert_eq!(parse_support(dir.path(), &cache), good);
    }

    #[test]
    fn a_scan_waits_for_the_cache_lock_before_loading_its_last_good_snapshot() {
        use std::fs::OpenOptions;
        use std::sync::mpsc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache.json");
        let (path, conn) = database(dir.path(), Edition::Domestic, "account", 91);
        insert(&conn, 2, "id", usage(20, 0, 0, 0), Some(TIME));
        parse_support(dir.path(), &cache);
        drop(conn);
        std::fs::write(path, b"corrupt after the previous complete read").unwrap();
        let guard = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(cache.with_extension("lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&guard).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let support = dir.path().to_owned();
        let scan_cache = cache.clone();
        let scan = thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx
                .send(parse_support(&support, &scan_cache))
                .unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_secs(1)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        // Simulate another scanner publishing its newer complete read while it
        // owns the same stable sidecar. The waiting scan must load this version.
        let mut latest: Cache = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
        for source in latest.sources.values_mut() {
            for message in &mut source.messages {
                message.tokens.input = 80;
                message.refresh_derived_fields();
            }
        }
        std::fs::write(&cache, serde_json::to_vec(&latest).unwrap()).unwrap();
        drop(guard);
        let messages = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        scan.join().unwrap();
        assert_eq!(messages[0].tokens.total(), 80);
    }

    #[test]
    fn unavailable_cache_lock_still_reads_sources_without_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache.json");
        let (_, conn) = database(dir.path(), Edition::Domestic, "account", 91);
        insert(&conn, 2, "id", usage(20, 0, 0, 0), Some(TIME));
        parse_support(dir.path(), &cache);
        let saved = std::fs::read(&cache).unwrap();
        let lock = cache.with_extension("lock");
        if lock.exists() {
            std::fs::remove_file(&lock).unwrap();
        }
        std::fs::create_dir(&lock).unwrap();
        insert(&conn, 2, "id", usage(80, 0, 0, 0), Some(TIME));
        assert_eq!(parse_support(dir.path(), &cache)[0].tokens.total(), 80);
        assert_eq!(std::fs::read(cache).unwrap(), saved);
    }
}
