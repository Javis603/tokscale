//! Qoder CN local usage: the legacy `SharedClientCache/cache/db/local.db`
//! SQLite store and the Claude-compatible JSONL transcripts that 2026-09+
//! builds write under `~/.qoder-cn/projects`.
//!
//! Port of Token Monitor's `src/shared/providers/qodercn/usage.js`, keeping
//! its session and message ids so Token Monitor's keys do not change.
//!
//! Failure semantics differ from upstream's SQLite lanes on purpose. Those
//! return no rows when a changed database cannot be read, which a caller that
//! applies today's totals as a delta sees as usage disappearing. Here every
//! source keeps its last complete read in a small cache: a source that fails
//! to read (locked database, I/O error, read budget exceeded) is served from
//! that snapshot instead of from a partial or empty read. The same cache lets
//! unchanged sources skip re-reading entirely.

use super::js;
use crate::sessions::utils::open_readonly_sqlite;
use crate::sessions::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::TokenBreakdown;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::warn;

pub const CLIENT_ID: &str = "qodercn";
const PROVIDER_ID: &str = "qodercn";
const UNPRICED_PROVIDER_ID: &str = "unpriced:qodercn";
const FALLBACK_MODEL: &str = "qoder-agent";

const DB_SUFFIX: [&str; 4] = ["SharedClientCache", "cache", "db", "local.db"];
const DB_MAX_ROWS: usize = 100_000;
const DB_MAX_BYTES: usize = 50 * 1024 * 1024;
const DB_BUSY_TIMEOUT: Duration = Duration::from_secs(3);
const JSONL_MAX_DEPTH: usize = 6;
const JSONL_MAX_FILES: usize = 5000;
const JSONL_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
const JSONL_MAX_ROWS: usize = 100_000;
const JSONL_MAX_LINE_BYTES: usize = 32 * 1024 * 1024;
const CACHE_VERSION: u32 = 1;

/// Qoder CN stores internal model codes; these are the app's own display
/// names. Unmapped codes (custom models) pass through unchanged.
const MODEL_DISPLAY_NAMES: &[(&str, &str)] = &[
    ("auto", "Auto"),
    ("dashscope_qmodel", "Qwen3.7-Plus"),
    ("dashscope_qwen3_coder", "Qwen3-Coder-Plus"),
    ("dashscope_qwen_max_latest", "Qwen3-Max"),
    ("dfmodel", "DeepSeek-V4-Flash"),
    ("dmodel", "DeepSeek-V4-Pro"),
    ("efficient", "Efficient"),
    ("gm51model", "GLM-5.2"),
    ("gmodel", "GLM-5"),
    ("kmodel", "Kimi-K2.7-Code"),
    ("lite", "Lite"),
    ("mmodel", "MiniMax-M3"),
    ("performance", "Performance"),
    ("q35model", "Qwen3.5-Plus"),
    ("q35model_preview", "Qwen3.8-Max-Preview"),
    ("q36fmodel", "Qwen3.6-Flash"),
    ("qmodel", "Qwen3.7-Plus"),
    ("qmodel_latest", "Qwen3.7-Max"),
    ("qmodel_preview", "Qwen3.8-Max-Preview"),
    ("ultimate", "Ultimate"),
];

/// Routing tiers name a tier, not the model behind it, so no catalog price
/// may be attached to them.
const ROUTING_TIERS: &[&str] = &["auto", "ultimate", "performance", "efficient", "lite"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Row {
    session_id: String,
    message_id: String,
    model: String,
    /// Workspace key: the transcript's `cwd` (a real path the report can
    /// resolve to a project), or the legacy store's bare `project_name`.
    workspace: String,
    input: i64,
    output: i64,
    cache_read: i64,
    created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Fingerprint {
    len: u64,
    modified_ns: u128,
    /// Length and mtime of the `-wal` sidecar; SQLite commits land there
    /// first, so the main file alone does not change on every write.
    wal: Option<(u64, u128)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CachedSource {
    fingerprint: Fingerprint,
    rows: Vec<Row>,
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Cache {
    version: u32,
    /// Keyed by source path: the database file, or one JSONL transcript.
    sources: BTreeMap<String, CachedSource>,
}

struct Paths {
    db: PathBuf,
    projects: PathBuf,
}

pub fn parse(home_dir: &str) -> Vec<UnifiedMessage> {
    let cache_path = crate::paths::get_cache_dir()
        .join("token-monitor")
        .join("qodercn.json");
    parse_with(&data_paths(Path::new(home_dir)), &cache_path)
}

fn env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    std::path::absolute(trimmed).ok()
}

fn data_paths(home: &Path) -> Paths {
    let app_support = if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Roaming"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| home.join(".config"))
    };
    let db = env_path("TOKEN_MONITOR_QODER_CN_DB_PATH").unwrap_or_else(|| {
        DB_SUFFIX
            .iter()
            .fold(app_support.join("QoderCN"), |path, part| path.join(part))
    });
    // The explicit Token Monitor override wins over Qoder's own config root.
    let projects = env_path("TOKEN_MONITOR_QODER_CN_PROJECTS_PATH")
        .or_else(|| env_path("QODERCN_CONFIG_DIR").map(|dir| dir.join("projects")))
        .unwrap_or_else(|| home.join(".qoder-cn").join("projects"));
    Paths { db, projects }
}

fn parse_with(paths: &Paths, cache_path: &Path) -> Vec<UnifiedMessage> {
    let mut cache = load_cache(cache_path);
    let loaded = Cache {
        version: cache.version,
        sources: cache.sources.clone(),
    };
    let mut next = Cache {
        version: CACHE_VERSION,
        sources: BTreeMap::new(),
    };
    collect_db(&paths.db, &mut cache, &mut next);
    collect_jsonl(&paths.projects, &mut cache, &mut next);

    let mut unique: HashMap<String, Row> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for row in next.sources.values().flat_map(|source| source.rows.iter()) {
        if !unique.contains_key(&row.message_id) {
            order.push(row.message_id.clone());
        }
        unique.insert(row.message_id.clone(), row.clone());
    }
    // Token Monitor scans several times a minute; rewrite only on a change.
    if next != loaded {
        save_cache(cache_path, &next);
    }
    order
        .into_iter()
        .filter_map(|id| unique.remove(&id))
        .map(to_message)
        .collect()
}

fn to_message(row: Row) -> UnifiedMessage {
    // Routing tiers go through the `unpriced:` provider convention (as Unsloth
    // does) rather than a provider-reported $0: the row stays cost-unknown
    // instead of claiming an authoritative zero, and the lookup still refuses
    // to price it. Without it, `efficient` resolves to Kilo's `kilo-auto/efficient`.
    let provider = if ROUTING_TIERS.contains(&row.model.trim().to_ascii_lowercase().as_str()) {
        UNPRICED_PROVIDER_ID
    } else {
        PROVIDER_ID
    };
    let mut message = UnifiedMessage::new_with_dedup(
        CLIENT_ID,
        row.model.clone(),
        provider,
        row.session_id,
        row.created_at,
        TokenBreakdown {
            input: row.input,
            output: row.output,
            cache_read: row.cache_read,
            cache_write: 0,
            reasoning: 0,
        },
        0.0,
        Some(row.message_id),
    );
    if let Some(key) = normalize_workspace_key(&row.workspace) {
        let label = workspace_label_from_key(&key);
        message.set_workspace(Some(key), label);
    }
    message
}

// --- source bookkeeping ---

fn load_cache(path: &Path) -> Cache {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Cache>(&bytes).ok())
        .filter(|cache| cache.version == CACHE_VERSION)
        .unwrap_or_default()
}

fn save_cache(path: &Path, cache: &Cache) {
    let Some(dir) = path.parent() else { return };
    let Ok(bytes) = serde_json::to_vec(cache) else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = crate::fs_atomic::replace_file(&tmp, path);
    }
}

fn modified_ns(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn fingerprint(path: &Path, with_wal: bool) -> Option<Fingerprint> {
    let meta = std::fs::metadata(path).ok()?;
    let wal = if with_wal {
        let mut wal_path = path.as_os_str().to_owned();
        wal_path.push("-wal");
        std::fs::metadata(PathBuf::from(wal_path))
            .ok()
            .map(|wal| (wal.len(), modified_ns(&wal)))
    } else {
        None
    };
    Some(Fingerprint {
        len: meta.len(),
        modified_ns: modified_ns(&meta),
        wal,
    })
}

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// --- value coercion, mirroring the JS adapter ---

/// `Number(value)` truncated, rejecting negatives and non-finite values.
/// `None` is JS `undefined` (absent); JSON `null` coerces to 0.
fn numeric(value: Option<&Value>) -> Option<i64> {
    let number = match value? {
        Value::Null => 0.0,
        Value::Bool(flag) => f64::from(u8::from(*flag)),
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse::<f64>().ok()?
            }
        }
        _ => return None,
    };
    (number.is_finite() && (0.0..=js::MAX_SAFE_INTEGER).contains(&number))
        .then(|| number.trunc() as i64)
}

/// `value ?? 0` before [`numeric`].
fn numeric_or_zero(value: Option<&Value>) -> Option<i64> {
    match value {
        None | Some(Value::Null) => Some(0),
        some => numeric(some),
    }
}

fn display_model(code: &str) -> String {
    MODEL_DISPLAY_NAMES
        .iter()
        .find(|(key, _)| *key == code)
        .map(|(_, name)| (*name).to_string())
        .unwrap_or_else(|| code.to_string())
}

fn project_label(value: &str) -> String {
    // '.' is Qoder CN's "no project" sentinel.
    let label = value.trim();
    if label == "." {
        String::new()
    } else {
        label.to_string()
    }
}

// --- legacy SQLite store ---

const USAGE_SQL: &str = "
SELECT rowid AS row_id, id, session_id, request_id, token_info, model_info, gmt_create,
  (SELECT cs.project_name FROM chat_session cs WHERE cs.session_id = chat_message.session_id LIMIT 1) AS project_name
FROM chat_message
WHERE role = 'assistant'
  AND token_info IS NOT NULL
  AND trim(token_info) NOT IN ('', '{}')
ORDER BY gmt_create, rowid";

const USAGE_SQL_NO_PROJECT: &str = "
SELECT rowid AS row_id, id, session_id, request_id, token_info, model_info, gmt_create,
  NULL AS project_name
FROM chat_message
WHERE role = 'assistant'
  AND token_info IS NOT NULL
  AND trim(token_info) NOT IN ('', '{}')
ORDER BY gmt_create, rowid";

const CHAT_SESSION_PROBE_SQL: &str = "SELECT 1 FROM sqlite_master
WHERE type = 'table' AND name = 'chat_session'
  AND EXISTS (SELECT 1 FROM pragma_table_info('chat_session') WHERE name = 'project_name')
LIMIT 1";

/// A SQLite column as the JS `String(a || b || ...)` chain sees it: the
/// textual form, or `None` when the value is falsy (NULL, '', 0).
fn truthy_text(value: rusqlite::types::Value) -> Option<String> {
    use rusqlite::types::Value as Sql;
    match value {
        Sql::Null => None,
        Sql::Integer(0) => None,
        Sql::Integer(number) => Some(number.to_string()),
        Sql::Real(number) if number == 0.0 || number.is_nan() => None,
        Sql::Real(number) => Some(js_number_text(number)),
        Sql::Text(text) if text.is_empty() => None,
        Sql::Text(text) => Some(text),
        Sql::Blob(bytes) if bytes.is_empty() => None,
        Sql::Blob(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
    }
}

fn js_number_text(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e21 {
        format!("{}", number as i64)
    } else {
        number.to_string()
    }
}

fn sql_json(value: &rusqlite::types::Value) -> Option<Value> {
    use rusqlite::types::Value as Sql;
    let text = match value {
        Sql::Text(text) => text.clone(),
        Sql::Blob(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => return None,
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .filter(|parsed| parsed.is_object() || parsed.is_array())
}

fn sql_time(value: &rusqlite::types::Value) -> i64 {
    use rusqlite::types::Value as Sql;
    match value {
        Sql::Integer(number) => js::epoch_number_ms(*number as f64),
        Sql::Real(number) if number.is_finite() => js::epoch_number_ms(*number),
        Sql::Text(text) => js::parse_time_text(text),
        _ => 0,
    }
}

struct DbRow {
    row_id: rusqlite::types::Value,
    id: rusqlite::types::Value,
    session_id: rusqlite::types::Value,
    request_id: rusqlite::types::Value,
    token_info: rusqlite::types::Value,
    model_info: rusqlite::types::Value,
    gmt_create: rusqlite::types::Value,
    project_name: rusqlite::types::Value,
}

fn normalize_db_row(row: DbRow, source: &str) -> Option<Row> {
    let usage = sql_json(&row.token_info)?;
    let prompt = numeric(usage.get("prompt_tokens"))?;
    let cached = numeric_or_zero(usage.get("cached_tokens"))?;
    let output = numeric(usage.get("completion_tokens"))?;
    if prompt + output == 0 {
        return None;
    }
    let time = sql_time(&row.gmt_create);
    let session = truthy_text(row.session_id.clone())
        .or_else(|| truthy_text(row.request_id.clone()))
        .or_else(|| truthy_text(row.id.clone()))
        .or_else(|| truthy_text(row.row_id.clone()))
        .unwrap_or_else(|| "unknown".to_string());
    let gmt_text = truthy_text(row.gmt_create.clone()).unwrap_or_else(|| "0".to_string());
    let message = truthy_text(row.id)
        .or_else(|| truthy_text(row.request_id))
        .or_else(|| truthy_text(row.row_id))
        .unwrap_or(gmt_text);
    let model_info = sql_json(&row.model_info);
    let model_key = model_info
        .as_ref()
        .and_then(|info| {
            ["model_key", "modelKey"].iter().find_map(|field| {
                info.get(*field)
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
            })
        })
        .unwrap_or_else(|| FALLBACK_MODEL.to_string());
    let project = match &row.project_name {
        rusqlite::types::Value::Text(text) => project_label(text),
        _ => String::new(),
    };
    Some(Row {
        session_id: format!("qodercn:{source}:{session}"),
        message_id: format!("qodercn:{source}:{session}:{message}"),
        model: display_model(&model_key),
        workspace: project,
        input: (prompt - cached).max(0),
        output,
        cache_read: prompt.min(cached),
        created_at: time,
    })
}

fn read_db(path: &Path) -> Result<Vec<Row>, String> {
    let conn = open_readonly_sqlite(path).map_err(|err| format!("open failed: {err}"))?;
    let _ = conn.busy_timeout(DB_BUSY_TIMEOUT);
    let with_project = conn
        .query_row(CHAT_SESSION_PROBE_SQL, [], |_| Ok(()))
        .is_ok();
    let sql = if with_project {
        USAGE_SQL
    } else {
        USAGE_SQL_NO_PROJECT
    };
    let source = js::path_namespace(path);
    let mut statement = conn
        .prepare(sql)
        .map_err(|err| format!("prepare failed: {err}"))?;
    let mut cursor = statement
        .query([])
        .map_err(|err| format!("query failed: {err}"))?;
    let mut rows = Vec::new();
    let mut count = 0usize;
    let mut bytes = 0usize;
    // Stop stepping at the budget: the point of it is to bound the read.
    while let Some(row) = cursor
        .next()
        .map_err(|err| format!("query did not complete: {err}"))?
    {
        count += 1;
        if count > DB_MAX_ROWS {
            return Err(format!("read budget exceeded (rows limit {DB_MAX_ROWS})"));
        }
        let db_row = DbRow {
            row_id: row
                .get(0)
                .map_err(|err| format!("row decode failed: {err}"))?,
            id: row
                .get(1)
                .map_err(|err| format!("row decode failed: {err}"))?,
            session_id: row
                .get(2)
                .map_err(|err| format!("row decode failed: {err}"))?,
            request_id: row
                .get(3)
                .map_err(|err| format!("row decode failed: {err}"))?,
            token_info: row
                .get(4)
                .map_err(|err| format!("row decode failed: {err}"))?,
            model_info: row
                .get(5)
                .map_err(|err| format!("row decode failed: {err}"))?,
            gmt_create: row
                .get(6)
                .map_err(|err| format!("row decode failed: {err}"))?,
            project_name: row
                .get(7)
                .map_err(|err| format!("row decode failed: {err}"))?,
        };
        for value in [&db_row.token_info, &db_row.model_info] {
            if let rusqlite::types::Value::Text(text) = value {
                bytes += text.len();
            }
        }
        if bytes > DB_MAX_BYTES {
            return Err(format!("read budget exceeded (bytes limit {DB_MAX_BYTES})"));
        }
        if let Some(normalized) = normalize_db_row(db_row, &source) {
            rows.push(normalized);
        }
    }
    Ok(rows)
}

fn collect_db(path: &Path, cache: &mut Cache, next: &mut Cache) {
    let source_key = key(path);
    let cached = cache.sources.remove(&source_key);
    // An absent database is a valid empty source (JSONL-only installs).
    let Some(current) = fingerprint(path, true) else {
        return;
    };
    if let Some(entry) = cached.as_ref().filter(|entry| entry.fingerprint == current) {
        next.sources.insert(source_key, entry.clone());
        return;
    }
    match read_db(path) {
        Ok(rows) => {
            next.sources.insert(
                source_key,
                CachedSource {
                    fingerprint: current,
                    rows,
                },
            );
        }
        Err(error) => {
            warn!(db_path = %path.display(), %error, "Qoder CN database read failed; serving the last complete read");
            if let Some(entry) = cached {
                next.sources.insert(source_key, entry);
            }
        }
    }
}

// --- JSONL transcripts ---

fn jsonl_model_name(raw: &str) -> String {
    let model = raw.trim();
    if model.is_empty() {
        return String::new();
    }
    // `qoder-custom-<profile>/<real model>`: strip only the profile prefix,
    // the real model may itself be provider-qualified.
    let key = match model.find('/') {
        Some(slash) if model.starts_with("qoder-custom-") && slash > 0 => &model[slash + 1..],
        _ => model,
    };
    display_model(key)
}

fn jsonl_project_label(cwd: &str) -> String {
    let dir = cwd.trim_end_matches(['/', '\\']);
    if dir.is_empty() {
        return String::new();
    }
    // Remote-control sessions run inside the app data dir; the basename is a
    // session hash, not a project.
    if dir.contains("/remote-control/")
        || dir.contains("\\remote-control\\")
        || dir.contains("/remote-control\\")
        || dir.contains("\\remote-control/")
        || dir.contains("com.qodercn.app.stable")
    {
        return String::new();
    }
    let start = dir.rfind(['/', '\\']).map(|index| index + 1).unwrap_or(0);
    project_label(&dir[start..])
}

fn js_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) if number.as_f64().is_some_and(|n| n != 0.0) => {
            Some(number.as_f64().map(js_number_text).unwrap_or_default())
        }
        Value::Bool(true) => Some("true".to_string()),
        _ => None,
    }
}

fn normalize_jsonl_row(obj: &Value, source: &str) -> Option<Row> {
    if obj.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let message = obj.get("message");
    let usage = message.and_then(|m| m.get("usage")).filter(|u| match u {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        _ => true,
    })?;
    let prompt = numeric(usage.get("input_tokens"))?;
    let cached = numeric_or_zero(usage.get("cache_read_input_tokens"))?;
    let output = numeric(usage.get("output_tokens"))?;
    // Plan-billed first-party rows record credits and a context ratio but no
    // tokens; they are not measured zero usage, so they stay out.
    if prompt + output == 0 {
        return None;
    }
    let session = js_text(obj.get("sessionId")).unwrap_or_else(|| "unknown".to_string());
    let message_key = js_text(message.and_then(|m| m.get("id")))
        .or_else(|| js_text(obj.get("uuid")))
        .unwrap_or_else(|| js_text(obj.get("timestamp")).unwrap_or_else(|| "0".to_string()));
    let model = message
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
        .map(jsonl_model_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| FALLBACK_MODEL.to_string());
    // Keep the full cwd so the report can resolve it to a project path; the
    // JS adapter's label rules decide whether there is a project at all.
    let workspace = obj
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !jsonl_project_label(cwd).is_empty())
        .map(|cwd| cwd.trim_end_matches(['/', '\\']).to_string())
        .unwrap_or_default();
    Some(Row {
        session_id: format!("qodercn:jsonl:{source}:{session}"),
        message_id: format!("qodercn:jsonl:{source}:{session}:{message_key}"),
        model,
        workspace,
        input: (prompt - cached).max(0),
        output,
        cache_read: prompt.min(cached),
        created_at: js::timestamp_ms(obj.get("timestamp")),
    })
}

/// Enumerates every transcript, failing instead of returning a partial list:
/// a missing root is a valid empty source, but any error once traversal has
/// started (or a depth/file budget) aborts the whole JSONL source.
fn list_jsonl(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) -> Result<(), String> {
    if depth > JSONL_MAX_DEPTH {
        return Err(format!(
            "read budget exceeded (depth limit {JSONL_MAX_DEPTH})"
        ));
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if depth == 0 && err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("{}: {err}", dir.display())),
    };
    let mut entries = entries
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("{}: {err}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry
            .file_type()
            .map_err(|err| format!("{}: {err}", entry.path().display()))?;
        let path = entry.path();
        if file_type.is_dir() {
            list_jsonl(&path, depth + 1, found)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            found.push(path);
            if found.len() > JSONL_MAX_FILES {
                return Err(format!(
                    "read budget exceeded (files limit {JSONL_MAX_FILES})"
                ));
            }
        }
    }
    Ok(())
}

fn read_jsonl_file(path: &Path, bytes_read: &mut u64) -> Result<Vec<Row>, String> {
    let file = std::fs::File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let mut reader = BufReader::new(file);
    let source = js::path_namespace(path);
    let mut rows = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        // Bound a single line before it is buffered whole.
        let read = (&mut reader)
            .take(JSONL_MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        if read == 0 {
            break;
        }
        *bytes_read += read as u64;
        if *bytes_read > JSONL_MAX_TOTAL_BYTES {
            return Err(format!(
                "read budget exceeded (bytes limit {JSONL_MAX_TOTAL_BYTES})"
            ));
        }
        let content = line.strip_suffix(b"\n").unwrap_or(&line);
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        if content.len() > JSONL_MAX_LINE_BYTES {
            return Err(format!(
                "read budget exceeded (line limit {JSONL_MAX_LINE_BYTES})"
            ));
        }
        if content.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_slice::<Value>(content) else {
            continue;
        };
        if let Some(row) = normalize_jsonl_row(&obj, &source) {
            rows.push(row);
        }
    }
    Ok(rows)
}

fn collect_jsonl(root: &Path, cache: &mut Cache, next: &mut Cache) {
    let root_key = key(root);
    let is_under_root =
        |source: &String| Path::new(source).starts_with(root) && source != &root_key;
    let previous: BTreeMap<String, CachedSource> = cache
        .sources
        .iter()
        .filter(|(source, _)| is_under_root(source))
        .map(|(source, entry)| (source.clone(), entry.clone()))
        .collect();

    let fresh = (|| -> Result<BTreeMap<String, CachedSource>, String> {
        let mut files = Vec::new();
        list_jsonl(root, 0, &mut files)?;
        files.sort();
        let mut bytes_read = 0u64;
        let mut row_count = 0usize;
        let mut sources = BTreeMap::new();
        for path in files {
            let source_key = key(&path);
            let current = fingerprint(&path, false)
                .ok_or_else(|| format!("{}: stat failed", path.display()))?;
            let entry = match previous.get(&source_key) {
                Some(entry) if entry.fingerprint == current => entry.clone(),
                _ => CachedSource {
                    fingerprint: current,
                    rows: read_jsonl_file(&path, &mut bytes_read)?,
                },
            };
            row_count += entry.rows.len();
            if row_count > JSONL_MAX_ROWS {
                return Err(format!(
                    "read budget exceeded (rows limit {JSONL_MAX_ROWS})"
                ));
            }
            sources.insert(source_key, entry);
        }
        Ok(sources)
    })();

    match fresh {
        Ok(sources) => next.sources.extend(sources),
        Err(error) => {
            warn!(projects_dir = %root.display(), %error, "Qoder CN transcript read failed; serving the last complete read");
            next.sources.extend(previous);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::CostSource;
    use serde_json::json;

    fn paths(dir: &Path) -> Paths {
        Paths {
            db: dir.join("local.db"),
            projects: dir.join("projects"),
        }
    }

    fn write_jsonl(path: &Path, lines: &[Value]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(path, body.join("\n") + "\n").unwrap();
    }

    fn assistant(id: &str, model: &str, input: i64, cached: i64, output: i64, cwd: &str) -> Value {
        json!({
            "type": "assistant",
            "sessionId": "s-1",
            "cwd": cwd,
            "timestamp": "2026-10-01T10:00:00Z",
            "message": { "id": id, "model": model, "usage": {
                "input_tokens": input, "cache_read_input_tokens": cached, "output_tokens": output
            } }
        })
    }

    fn create_db(path: &Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE chat_message (id TEXT, session_id TEXT, request_id TEXT, role TEXT,
               token_info TEXT, model_info TEXT, gmt_create INTEGER);
             CREATE TABLE chat_session (session_id TEXT, project_name TEXT);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn jsonl_splits_the_cached_prefix_out_of_input_and_maps_models() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        write_jsonl(
            &p.projects.join("ws").join("s-1.jsonl"),
            &[
                assistant(
                    "m1",
                    "qoder-custom-abc/openai/gpt-5",
                    100,
                    60,
                    10,
                    "/Users/me/code/app",
                ),
                assistant("m2", "qmodel", 0, 0, 0, "/Users/me/code/app"),
                assistant("m3", "dfmodel", 50, 0, 5, "/x/remote-control/abc123"),
            ],
        );
        let messages = parse_with(&p, &dir.path().join("cache.json"));
        assert_eq!(messages.len(), 2);
        let gpt = messages
            .iter()
            .find(|m| m.model_id == "openai/gpt-5")
            .unwrap();
        assert_eq!(
            (gpt.tokens.input, gpt.tokens.cache_read, gpt.tokens.output),
            (40, 60, 10)
        );
        assert_eq!(gpt.workspace_label.as_deref(), Some("app"));
        let flash = messages
            .iter()
            .find(|m| m.model_id == "DeepSeek-V4-Flash")
            .unwrap();
        assert_eq!(flash.workspace_label, None);
    }

    #[test]
    fn db_rows_use_display_names_projects_and_routing_tiers_stay_unpriced() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let conn = create_db(&p.db);
        conn.execute_batch(
            r#"INSERT INTO chat_session VALUES ('sess', 'my-app'), ('dot', '.');
               INSERT INTO chat_message VALUES ('a', 'sess', 'r1', 'assistant',
                 '{"prompt_tokens":120,"cached_tokens":20,"completion_tokens":30}', '{"model_key":"kmodel"}', 1790848800000);
               INSERT INTO chat_message VALUES ('b', 'dot', 'r2', 'assistant',
                 '{"prompt_tokens":10,"completion_tokens":5}', '{"model_key":"auto"}', 1790848800);
               INSERT INTO chat_message VALUES ('c', 'sess', 'r3', 'user',
                 '{"prompt_tokens":10,"completion_tokens":5}', '{}', 1790848800000);"#,
        )
        .unwrap();
        drop(conn);
        let messages = parse_with(&p, &dir.path().join("cache.json"));
        assert_eq!(messages.len(), 2);
        let kimi = messages
            .iter()
            .find(|m| m.model_id == "Kimi-K2.7-Code")
            .unwrap();
        assert_eq!(
            (
                kimi.tokens.input,
                kimi.tokens.cache_read,
                kimi.tokens.output
            ),
            (100, 20, 30)
        );
        assert_eq!(kimi.workspace_label.as_deref(), Some("my-app"));
        assert!(kimi.session_id.starts_with("qodercn:") && kimi.session_id.ends_with(":sess"));
        let auto = messages.iter().find(|m| m.model_id == "Auto").unwrap();
        assert_eq!(auto.timestamp, 1_790_848_800_000);
        assert_eq!(auto.provider_id, "unpriced:qodercn");
        assert_eq!(auto.cost_source, CostSource::Unknown);
        assert_eq!(auto.workspace_label, None);
    }

    #[test]
    fn a_failed_read_serves_the_last_complete_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let cache = dir.path().join("cache.json");
        let conn = create_db(&p.db);
        conn.execute_batch(
            r#"INSERT INTO chat_message VALUES ('a', 's', 'r', 'assistant',
                 '{"prompt_tokens":10,"completion_tokens":5}', '{"model_key":"gmodel"}', 1790848800000);"#,
        )
        .unwrap();
        drop(conn);
        write_jsonl(
            &p.projects.join("s-1.jsonl"),
            &[assistant("m1", "gmodel", 7, 0, 3, "")],
        );
        assert_eq!(parse_with(&p, &cache).len(), 2);

        // Corrupt the database (changed fingerprint, unreadable) and push the
        // transcript tree past the depth budget: both fall back.
        std::fs::write(&p.db, b"not a database, and longer than before").unwrap();
        let mut deep = p.projects.clone();
        for level in 0..=JSONL_MAX_DEPTH {
            deep = deep.join(format!("d{level}"));
        }
        write_jsonl(
            &deep.join("late.jsonl"),
            &[assistant("m9", "gmodel", 1, 0, 1, "")],
        );
        let messages = parse_with(&p, &cache);
        let total: i64 = messages
            .iter()
            .map(|m| m.tokens.input + m.tokens.output)
            .sum();
        assert_eq!(messages.len(), 2);
        assert_eq!(total, 25);
    }

    #[test]
    fn unchanged_sources_are_served_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let cache = dir.path().join("cache.json");
        let file = p.projects.join("s-1.jsonl");
        write_jsonl(&file, &[assistant("m1", "gmodel", 7, 0, 3, "")]);
        assert_eq!(parse_with(&p, &cache).len(), 1);
        // Same length and mtime: a cache hit never reopens the file, so
        // making it unreadable changes nothing.
        let meta = std::fs::metadata(&file).unwrap();
        let mut perms = meta.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o000);
            std::fs::set_permissions(&file, perms).unwrap();
        }
        assert_eq!(parse_with(&p, &cache).len(), 1);
    }

    #[test]
    fn an_unchanged_scan_does_not_rewrite_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let cache = dir.path().join("cache.json");
        write_jsonl(
            &p.projects.join("s-1.jsonl"),
            &[assistant("m1", "gmodel", 7, 0, 3, "")],
        );
        parse_with(&p, &cache);
        let written = std::fs::metadata(&cache).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(parse_with(&p, &cache).len(), 1);
        assert_eq!(
            std::fs::metadata(&cache).unwrap().modified().unwrap(),
            written
        );
    }

    #[test]
    fn token_counts_beyond_the_safe_integer_range_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let conn = create_db(&p.db);
        conn.execute_batch(
            r#"INSERT INTO chat_message VALUES ('a', 's', 'r', 'assistant',
                 '{"prompt_tokens":1e30,"completion_tokens":9223372036854775807}', '{}', 1790848800000);
               INSERT INTO chat_message VALUES ('b', 's', 'r2', 'assistant',
                 '{"prompt_tokens":10,"completion_tokens":5}', '{}', 1790848800000);"#,
        )
        .unwrap();
        drop(conn);
        write_jsonl(
            &p.projects.join("s-1.jsonl"),
            &[assistant("m1", "gmodel", 7, 0, i64::MAX, "")],
        );
        let messages = parse_with(&p, &dir.path().join("cache.json"));
        let total: i64 = messages
            .iter()
            .map(|m| m.tokens.input + m.tokens.output)
            .sum();
        assert_eq!(messages.len(), 1);
        assert_eq!(total, 15);
    }

    #[test]
    fn timestamps_follow_the_js_rules() {
        assert_eq!(
            js::timestamp_ms(Some(&json!(1_790_848_800))),
            1_790_848_800_000
        );
        assert_eq!(
            js::timestamp_ms(Some(&json!(1_790_848_800_000_i64))),
            1_790_848_800_000
        );
        assert_eq!(
            js::timestamp_ms(Some(&json!("1790848800"))),
            1_790_848_800_000
        );
        assert_eq!(
            js::timestamp_ms(Some(&json!("2026-10-01T10:00:00Z"))),
            1_790_848_800_000
        );
        assert_eq!(
            js::timestamp_ms(Some(&json!("2026-10-01"))),
            1_790_812_800_000
        );
        assert_eq!(js::timestamp_ms(Some(&json!(""))), 0);
    }
}
