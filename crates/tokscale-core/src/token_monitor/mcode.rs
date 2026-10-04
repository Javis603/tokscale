//! MiniMax Code session history, supplementing upstream's `mcode` client.
//!
//! Upstream reads only the streams Tokscale captures from
//! `tokscale headless mcode exec`. The MiniMax Code CLI and desktop app both
//! write every Session through the shared local runtime instead, to
//! `<data-dir>/v2/sessions/<yyyy>/<mm>/<dd>/<dir>/`:
//!
//! - `manifest.json` names the Session.
//! - `messages.jsonl` is the active history: one envelope per message, whose
//!   assistant messages carry Pi `usage` and the model that answered.
//! - `snapshots/g<generation>--<id>.jsonl` is the whole active history as it
//!   was before each compaction. Compaction keeps the identity of every message
//!   it retains, so a message id is counted once across the chain.
//!
//! The runtime also projects usage into the SQLite `local_runtime_token_usage`
//! table, but that projection is best effort (write failures are swallowed)
//! and usually leaves the model empty, so it is read only for Session metadata.
//!
//! A turn that upstream already counted from a headless capture is skipped
//! here: the capture and the store carry the same Session and turn ids. What
//! upstream counted is taken from its lane's own output, recorded right after
//! it parsed, never by reading the captures again.
//!
//! History files hold whole conversations, tool output included, and Token
//! Monitor scans several times a minute, so the usage rows of each file are
//! cached by length and mtime, per scanned home, and a file is reparsed only
//! when it changes. A read that fails or
//! races a write is never cached; the last complete read is served instead.

use super::{js, Scope};
use crate::sessions::utils::open_readonly_sqlite_opt;
use crate::sessions::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::TokenBreakdown;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub const CLIENT_ID: &str = "mcode";
const CACHE_VERSION: u32 = 3;

const ENV_OVERRIDES: [&str; 2] = ["MINIMAX_DATA_DIR", "MAVIS_DATA_DIR"];
/// The current default data directory and the one earlier releases used. A
/// selected profile appends `-<profile>` to either.
const DATA_DIR_BASENAMES: [&str; 2] = [".minimax", ".mavis"];

/// Data directories to read, in the order MiniMax Code would choose them.
///
/// A non-empty override is the only directory, as it is for MiniMax Code
/// itself; it is ignored when the scan targets another home (`--home`), whose
/// environment it does not describe. Otherwise every default and profile
/// directory under the home is read, since the desktop app and each profile
/// can write to a different one.
pub fn data_dirs(home_dir: &str, use_env_roots: bool) -> Vec<PathBuf> {
    if use_env_roots {
        for name in ENV_OVERRIDES {
            if let Some(value) = std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
            {
                return vec![PathBuf::from(value)];
            }
        }
    }
    let home = Path::new(home_dir);
    let mut dirs: Vec<PathBuf> = DATA_DIR_BASENAMES
        .iter()
        .map(|name| home.join(name))
        .collect();
    let mut profiles: Vec<PathBuf> = std::fs::read_dir(home)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            DATA_DIR_BASENAMES.iter().any(|base| {
                name.strip_prefix(base)
                    .and_then(|rest| rest.strip_prefix('-'))
                    .is_some_and(|profile| !profile.is_empty())
            })
        })
        .map(|entry| entry.path())
        .collect();
    profiles.sort();
    dirs.extend(profiles);
    dirs
}

/// One assistant message with usage, as extracted from a history file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Row {
    message_id: String,
    turn_id: String,
    provider: String,
    model: String,
    timestamp: i64,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
}

/// File length and mtime.
type Fingerprint = (u64, u128);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CachedFile {
    fingerprint: Fingerprint,
    rows: Vec<Row>,
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Cache {
    version: u32,
    /// Keyed by history file path.
    files: BTreeMap<String, CachedFile>,
}

pub fn parse(scope: &Scope) -> Vec<UnifiedMessage> {
    // One cache per scanned home: a host scan and a `--home` (WSL) scan visit
    // different files, and sharing one cache would evict each other's rows.
    let namespace = js::path_namespace(Path::new(scope.home_dir));
    let cache_path = crate::paths::get_cache_dir()
        .join("token-monitor")
        .join(format!("mcode-{namespace}.json"));
    parse_with(scope, &cache_path)
}

fn parse_with(scope: &Scope, cache_path: &Path) -> Vec<UnifiedMessage> {
    let loaded = load_cache(cache_path);
    let mut next = Cache {
        version: CACHE_VERSION,
        ..Cache::default()
    };
    let captured: HashSet<(String, String)> = scope
        .counted
        .dedup_keys(CLIENT_ID)
        .iter()
        .filter_map(|key| headless_turn(key))
        .collect();
    let mut seen_roots = HashSet::new();
    let mut seen_messages = HashSet::new();
    let mut messages = Vec::new();
    for data_dir in data_dirs(scope.home_dir, scope.use_env_roots) {
        let sessions_root = data_dir.join("v2").join("sessions");
        // `.mavis` is often a link to `.minimax`; read each store once.
        let Ok(canonical) = std::fs::canonicalize(&sessions_root) else {
            continue;
        };
        if !seen_roots.insert(canonical) {
            continue;
        }
        let metadata = session_metadata(&data_dir);
        for session_dir in session_dirs(&sessions_root) {
            let Some(session_id) = manifest_session_id(&session_dir) else {
                continue;
            };
            let session = metadata.get(&session_id).cloned().unwrap_or_default();
            let rows = history_files(&session_dir)
                .into_iter()
                .flat_map(|path| cached_rows(&path, &loaded, &mut next));
            for message in session_messages(&session_id, &session, rows, &captured) {
                let key = message.dedup_key.clone().unwrap_or_default();
                if seen_messages.insert(key) {
                    messages.push(message);
                }
            }
        }
    }
    // Rewrite only on a change; most scans find every file unchanged.
    if next != loaded {
        save_cache(cache_path, &next);
    }
    messages
}

/// Upstream keys a headless row
/// `mcode:<session>:<turn>:<index>:<input>:<output>:<cache_read>:<cache_write>`
/// without escaping. Turn ids can contain `:` (`mavis-internal:<scope>:<id>`,
/// `plan-review:…`) while minted session ids (`mvs_<hex>`) do not, so the turn
/// is everything between the session and the five trailing numbers.
fn headless_turn(key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix("mcode:")?;
    let mut fields = rest.rsplitn(6, ':');
    for _ in 0..5 {
        fields.next()?.parse::<i64>().ok()?;
    }
    let (session, turn) = fields.next()?.split_once(':')?;
    Some((session.to_string(), turn.to_string()))
}

/// `<sessions>/<yyyy>/<mm>/<dd>/<dir>`, sorted so output order is stable.
fn session_dirs(sessions_root: &Path) -> Vec<PathBuf> {
    let mut level = vec![sessions_root.to_path_buf()];
    for _ in 0..4 {
        level = level
            .iter()
            .flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten())
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect();
    }
    level.sort();
    level
}

#[derive(Debug, Default, Clone)]
struct SessionMetadata {
    title: Option<String>,
    workspace: Option<String>,
}

/// Session titles and workspaces from the runtime database. Usage never comes
/// from here; a missing or unreadable database only loses these labels.
fn session_metadata(data_dir: &Path) -> HashMap<String, SessionMetadata> {
    let path = data_dir
        .join("v2")
        .join("sqlite")
        .join("runtime-state.sqlite");
    let mut metadata = HashMap::new();
    let Some(conn) = open_readonly_sqlite_opt(&path) else {
        return metadata;
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT session_id, title, \
         CASE WHEN is_default_workspace = 1 THEN NULL \
              ELSE COALESCE(NULLIF(project_workspace_dir, ''), workspace_dir) END \
         FROM local_runtime_sessions",
    ) else {
        return metadata;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    }) else {
        return metadata;
    };
    for (session_id, title, workspace) in rows.flatten() {
        let title = title
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty());
        metadata.insert(session_id, SessionMetadata { title, workspace });
    }
    metadata
}

#[derive(Deserialize)]
struct Manifest {
    #[serde(rename = "sessionId")]
    session_id: String,
}

#[derive(Deserialize)]
struct Envelope {
    message_id: String,
    turn_id: String,
    message: HistoryMessage,
}

#[derive(Deserialize)]
struct HistoryMessage {
    role: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<serde_json::Value>,
    #[serde(default)]
    timestamp: Option<serde_json::Value>,
}

fn manifest_session_id(session_dir: &Path) -> Option<String> {
    std::fs::read(session_dir.join("manifest.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok())
        .map(|manifest| manifest.session_id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// Snapshots oldest generation first, then the active history, so a message
/// keeps the place it was first committed in when it reappears later.
fn history_files(session_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(session_dir.join("snapshots"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    files.push(session_dir.join("messages.jsonl"));
    files
}

/// Rows of one history file: cached while its fingerprint holds. A failed
/// read serves the last complete read instead, and a read that raced a write
/// is used once without being cached, so neither can stick.
fn cached_rows(path: &Path, loaded: &Cache, next: &mut Cache) -> Vec<Row> {
    let key = path.to_string_lossy().into_owned();
    let previous = loaded.files.get(&key);
    let Some(before) = fingerprint(path) else {
        return Vec::new();
    };
    if let Some(entry) = previous.filter(|entry| entry.fingerprint == before) {
        next.files.insert(key, entry.clone());
        return entry.rows.clone();
    }
    match read_rows(path) {
        Ok(rows) if fingerprint(path) == Some(before) => {
            next.files.insert(
                key,
                CachedFile {
                    fingerprint: before,
                    rows: rows.clone(),
                },
            );
            rows
        }
        Ok(rows) => {
            if let Some(entry) = previous {
                next.files.insert(key, entry.clone());
            }
            rows
        }
        Err(_) => match previous {
            Some(entry) => {
                next.files.insert(key, entry.clone());
                entry.rows.clone()
            }
            None => Vec::new(),
        },
    }
}

fn read_rows(path: &Path) -> std::io::Result<Vec<Row>> {
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut buf = Vec::new();
    let mut rows = Vec::new();
    loop {
        buf.clear();
        // Unlike a `lines()` iterator, a read error here fails the whole read
        // instead of looking like the end of the file.
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        // An invalid byte costs only its own line.
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_start_matches('\u{feff}');
        // Only assistant messages carry usage; skip parsing the rest.
        if !line.contains("\"assistant\"") {
            continue;
        }
        let Ok(envelope) = serde_json::from_str::<Envelope>(line) else {
            continue;
        };
        let message = envelope.message;
        if message.role != "assistant" {
            continue;
        }
        let Some(tokens) = message.usage.as_ref().map(tokens_from_usage) else {
            continue;
        };
        let timestamp = normalize_timestamp(
            message
                .timestamp
                .as_ref()
                .and_then(number)
                .map(|value| value as i64)
                .unwrap_or(0),
        );
        if tokens.total() == 0 || timestamp <= 0 {
            continue;
        }
        rows.push(Row {
            message_id: envelope.message_id,
            turn_id: envelope.turn_id,
            provider: non_empty(message.provider).unwrap_or_default(),
            model: non_empty(message.model).unwrap_or_else(|| "unknown".into()),
            timestamp,
            input: tokens.input,
            output: tokens.output,
            cache_read: tokens.cache_read,
            cache_write: tokens.cache_write,
            reasoning: tokens.reasoning,
        });
    }
    Ok(rows)
}

/// Seconds to milliseconds, as upstream's headless parser does.
fn normalize_timestamp(timestamp: i64) -> i64 {
    if timestamp > 0 && timestamp < 10_000_000_000 {
        timestamp.saturating_mul(1_000)
    } else {
        timestamp
    }
}

fn session_messages(
    session_id: &str,
    session: &SessionMetadata,
    rows: impl Iterator<Item = Row>,
    captured: &HashSet<(String, String)>,
) -> Vec<UnifiedMessage> {
    let workspace_key = session
        .workspace
        .as_deref()
        .and_then(normalize_workspace_key);
    let workspace_label = workspace_key.as_deref().and_then(workspace_label_from_key);
    let mut seen = HashSet::new();
    let mut started_turns = HashSet::new();
    let mut messages = Vec::new();
    for row in rows {
        if !seen.insert(row.message_id.clone())
            || captured.contains(&(session_id.to_string(), row.turn_id.clone()))
        {
            continue;
        }
        let tokens = TokenBreakdown {
            input: row.input,
            output: row.output,
            cache_read: row.cache_read,
            cache_write: row.cache_write,
            cache_write_1h: 0,
            reasoning: row.reasoning,
        };
        let mut message = UnifiedMessage::new_with_dedup(
            CLIENT_ID,
            row.model,
            row.provider,
            session_id.to_string(),
            row.timestamp,
            tokens,
            0.0,
            Some(format!("mcode:store:{session_id}:{}", row.message_id)),
        );
        message.is_turn_start = started_turns.insert(row.turn_id);
        message.session_title = session.title.clone();
        if workspace_key.is_some() {
            message.set_workspace(workspace_key.clone(), workspace_label.clone());
        }
        messages.push(message);
    }
    messages
}

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

fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let meta = std::fs::metadata(path).ok()?;
    let modified_ns = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    Some((meta.len(), modified_ns))
}

/// Pi usage as the runtime's own usage projection reads it
/// (`normalizePiUsage` in local-runtime-v2): `input` excludes cache reads, and
/// a usage with neither input nor output falls back to `total_tokens`.
///
/// Reasoning is the exception. Tokscale's `reasoning` bucket is added to the
/// total and priced on top of `output`, but MiniMax Code's providers report
/// reasoning inside `output`: in 0.6.2 none of them writes a `reasoning` field,
/// and headless captures that carry `reasoningTokens` have a `totalTokens` of
/// input plus output, which upstream's parser follows by leaving it out. So a
/// reported `reasoning` is added only when it cannot be part of `output`:
/// when it is larger, or when `totalTokens` counts it on top of the others.
fn tokens_from_usage(usage: &serde_json::Value) -> TokenBreakdown {
    let field = |key: &str| usage.get(key).and_then(number).unwrap_or(0.0);
    let nested = |outer: &str, inner: &str| {
        usage
            .get(outer)
            .and_then(|value| value.get(inner))
            .and_then(number)
    };
    let input = field("input");
    let output = field("output");
    let (input, output) = if input > 0.0 || output > 0.0 {
        (input, output)
    } else {
        (field("total_tokens"), 0.0)
    };
    let cache_read = usage
        .get("cacheRead")
        .and_then(number)
        .or_else(|| nested("cache", "read"))
        .or_else(|| usage.get("cache_read").and_then(number))
        .unwrap_or(0.0);
    let cache_write = usage
        .get("cacheWrite")
        .and_then(number)
        .or_else(|| nested("cache", "write"))
        .or_else(|| usage.get("cache_write").and_then(number))
        .unwrap_or(0.0);
    let reasoning = field("reasoning");
    let total = field("totalTokens");
    let separate = reasoning > output
        || (total > 0.0 && total == input + output + cache_read + cache_write + reasoning);
    TokenBreakdown {
        input: count(input),
        output: count(output),
        cache_read: count(cache_read),
        cache_write: count(cache_write),
        cache_write_1h: 0,
        reasoning: if separate { count(reasoning) } else { 0 },
    }
}

/// A finite number, or a string holding one (the runtime accepts both).
fn number(value: &serde_json::Value) -> Option<f64> {
    let parsed = match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    parsed.is_finite().then_some(parsed)
}

fn count(value: f64) -> i64 {
    if value > 0.0 {
        value as i64
    } else {
        0
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::test_env::EnvGuard;
    use crate::token_monitor::Counted;
    use serial_test::serial;
    use std::fs;

    const SESSION: &str = "mvs_0123456789abcdef0123456789abcdef";

    fn assistant(message_id: &str, turn_id: &str, timestamp: i64, input: i64) -> String {
        format!(
            r#"{{"message_id":"{message_id}","turn_id":"{turn_id}","message":{{"role":"assistant","content":[{{"type":"text","text":"ok"}}],"api":"openai-completions","provider":"minimax","model":"MiniMax-M2.5","usage":{{"input":{input},"output":20,"cacheRead":300,"cacheWrite":5,"totalTokens":0,"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}}},"stopReason":"stop","timestamp":{timestamp}}}}}"#
        )
    }

    fn user(message_id: &str, turn_id: &str) -> String {
        format!(
            r#"{{"message_id":"{message_id}","turn_id":"{turn_id}","message":{{"role":"user","content":"hi assistant","timestamp":1791050552261}}}}"#
        )
    }

    fn session_dir(data_dir: &Path) -> PathBuf {
        let dir = data_dir
            .join("v2/sessions/2026/10/03")
            .join("18-02-32-130-session_bXZzXzAxMjM");
        fs::create_dir_all(dir.join("snapshots")).unwrap();
        fs::write(
            dir.join("manifest.json"),
            format!(r#"{{"schemaVersion":1,"sessionId":"{SESSION}","createdAtMs":1791050552130}}"#),
        )
        .unwrap();
        dir
    }

    /// What the upstream lane would count from the captures right now.
    fn upstream_counted(home: &Path, use_env_roots: bool) -> Counted {
        let result = crate::scanner::scan_all_clients_with_env_strategy(
            home.to_str().unwrap(),
            &[CLIENT_ID.to_string()],
            use_env_roots,
        );
        let messages: Vec<UnifiedMessage> = result
            .get(crate::ClientId::Mcode)
            .iter()
            .flat_map(|path| crate::sessions::mcode::parse_mcode_file(path))
            .collect();
        Counted::from_messages(&messages)
    }

    fn scan_with(home: &Path, use_env_roots: bool, counted: &Counted) -> Vec<UnifiedMessage> {
        let scope = Scope {
            home_dir: home.to_str().unwrap(),
            use_env_roots,
            counted,
        };
        parse_with(&scope, &home.join("tm-cache/mcode.json"))
    }

    fn scan(home: &Path, use_env_roots: bool) -> Vec<UnifiedMessage> {
        scan_with(home, use_env_roots, &upstream_counted(home, use_env_roots))
    }

    fn write_capture(home: &Path, turn_id: &str) {
        let headless = home.join(".config/tokscale/headless/mcode");
        fs::create_dir_all(&headless).unwrap();
        fs::write(
            headless.join("capture.jsonl"),
            serde_json::json!({
                "schemaVersion": 1, "type": "exec.completed", "timestampMs": 1791050552292i64,
                "sessionId": SESSION, "turnId": turn_id,
                "result": {
                    "type": "exec.result", "sessionId": SESSION, "turnId": turn_id, "status": "succeeded",
                    "model": {"providerId": "minimax", "modelId": "MiniMax-M2.5"},
                    "usage": {"inputTokens": 704, "outputTokens": 20, "cacheReadTokens": 300}
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    const ENV_KEYS: [&str; 4] = [
        "MINIMAX_DATA_DIR",
        "MAVIS_DATA_DIR",
        "TOKSCALE_HEADLESS_DIR",
        "TOKSCALE_EXTRA_DIRS",
    ];

    fn clear_env() -> EnvGuard {
        let mut guard = EnvGuard::capture(&ENV_KEYS);
        for name in ENV_KEYS {
            guard.remove(name);
        }
        guard
    }

    #[test]
    #[serial]
    fn reads_assistant_usage_once_across_the_compaction_chain() {
        let _env = clear_env();
        let home = tempfile::tempdir().unwrap();
        let dir = session_dir(&home.path().join(".minimax"));
        // The snapshot is the active history before compaction; the retained
        // `msg-b` keeps its identity in the new active file.
        fs::write(
            dir.join("snapshots/g000000000000--compact-1.jsonl"),
            [
                user("msg-user-1", "turn-1"),
                assistant("msg-a", "turn-1", 1791050552292, 704),
                assistant("msg-b", "turn-1", 1791050553292, 800),
            ]
            .join("\n"),
        )
        .unwrap();
        fs::write(
            dir.join("messages.jsonl"),
            [
                r#"{"message_id":"msg-summary","turn_id":"turn-2:compaction","message":{"role":"compactionSummary","summary":"…"}}"#.to_string(),
                assistant("msg-b", "turn-1", 1791050553292, 800),
                user("msg-user-2", "turn-2"),
                assistant("msg-c", "turn-2", 1791050554292, 900),
            ]
            .join("\n"),
        )
        .unwrap();

        let messages = scan(home.path(), true);

        let inputs: Vec<i64> = messages.iter().map(|m| m.tokens.input).collect();
        assert_eq!(inputs, vec![704, 800, 900]);
        let first = &messages[0];
        assert_eq!(first.client, "mcode");
        assert_eq!(first.session_id, SESSION);
        assert_eq!(first.model_id, "MiniMax-M2.5");
        assert_eq!(first.provider_id, "minimax");
        assert_eq!(first.tokens.output, 20);
        assert_eq!(first.tokens.cache_read, 300);
        assert_eq!(first.tokens.cache_write, 5);
        assert_eq!(first.timestamp, 1791050552292);
        let turn_starts: Vec<bool> = messages.iter().map(|m| m.is_turn_start).collect();
        assert_eq!(turn_starts, vec![true, false, true]);
    }

    #[test]
    #[serial]
    fn skips_turns_upstream_counted_from_a_headless_capture() {
        let _env = clear_env();
        let home = tempfile::tempdir().unwrap();
        let dir = session_dir(&home.path().join(".minimax"));
        fs::write(
            dir.join("messages.jsonl"),
            [
                assistant("msg-a", "turn-1", 1791050552292, 704),
                assistant("msg-b", "turn-2", 1791050553292, 800),
            ]
            .join("\n"),
        )
        .unwrap();
        write_capture(home.path(), "turn-1");

        let messages = scan(home.path(), true);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 800);
        // Only an exact turn is skipped, so a capture never hides the rest of
        // a Session continued in the TUI or the desktop app.
        assert!(messages[0].is_turn_start);
    }

    #[test]
    #[serial]
    fn an_override_replaces_the_default_directories_unless_scanning_another_home() {
        let mut env = clear_env();
        let home = tempfile::tempdir().unwrap();
        let custom = tempfile::tempdir().unwrap();
        for data_dir in [home.path().join(".minimax"), custom.path().to_path_buf()] {
            let dir = session_dir(&data_dir);
            fs::write(
                dir.join("messages.jsonl"),
                assistant("msg-a", "turn-1", 1791050552292, 704),
            )
            .unwrap();
        }
        env.set("MAVIS_DATA_DIR", custom.path().to_str().unwrap());
        let home_dir = home.path().to_str().unwrap();

        assert_eq!(data_dirs(home_dir, true), vec![custom.path().to_path_buf()]);
        assert_eq!(scan(home.path(), true).len(), 1);
        assert!(data_dirs(home_dir, false)
            .iter()
            .all(|dir| dir.starts_with(home.path())));
        assert_eq!(scan(home.path(), false).len(), 1);
    }

    #[test]
    #[serial]
    fn reads_profiles_and_the_legacy_directory_once_each() {
        let _env = clear_env();
        let home = tempfile::tempdir().unwrap();
        for (name, message_id) in [
            (".minimax", "msg-a"),
            (".mavis-work", "msg-b"),
            (".minimax-code", "msg-c"),
        ] {
            let dir = session_dir(&home.path().join(name));
            fs::write(
                dir.join("messages.jsonl"),
                assistant(message_id, "turn-1", 1791050552292, 704),
            )
            .unwrap();
        }
        // `.mavis` linked to `.minimax` is the same store.
        #[cfg(unix)]
        std::os::unix::fs::symlink(home.path().join(".minimax"), home.path().join(".mavis"))
            .unwrap();

        let messages = scan(home.path(), true);

        assert_eq!(messages.len(), 3);
    }

    #[test]
    #[serial]
    fn labels_sessions_from_the_runtime_database() {
        let _env = clear_env();
        let home = tempfile::tempdir().unwrap();
        let data_dir = home.path().join(".minimax");
        let dir = session_dir(&data_dir);
        fs::write(
            dir.join("messages.jsonl"),
            assistant("msg-a", "turn-1", 1791050552292, 704),
        )
        .unwrap();
        let workspace = home.path().join("project");
        fs::create_dir_all(&workspace).unwrap();
        let db_dir = data_dir.join("v2/sqlite");
        fs::create_dir_all(&db_dir).unwrap();
        let conn = rusqlite::Connection::open(db_dir.join("runtime-state.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE local_runtime_sessions (session_id TEXT PRIMARY KEY, title TEXT, \
             workspace_dir TEXT, project_workspace_dir TEXT, \
             is_default_workspace INTEGER NOT NULL DEFAULT 0)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO local_runtime_sessions VALUES (?1, 'Fix the build', ?2, ?2, 0)",
            rusqlite::params![SESSION, workspace.to_str().unwrap()],
        )
        .unwrap();
        drop(conn);

        let messages = scan(home.path(), true);

        assert_eq!(messages[0].session_title.as_deref(), Some("Fix the build"));
        assert!(messages[0].workspace_key.is_some());
        assert_eq!(messages[0].workspace_label.as_deref(), Some("project"));
    }

    #[test]
    #[serial]
    fn unchanged_history_files_are_served_from_the_cache() {
        let _env = clear_env();
        let home = tempfile::tempdir().unwrap();
        let dir = session_dir(&home.path().join(".minimax"));
        let active = dir.join("messages.jsonl");
        let snapshot = dir.join("snapshots/g000000000000--compact-1.jsonl");
        fs::write(&snapshot, assistant("msg-a", "turn-1", 1791050552292, 704)).unwrap();
        fs::write(&active, assistant("msg-b", "turn-2", 1791050553292, 800)).unwrap();
        assert_eq!(scan(home.path(), true).len(), 2);

        // Same length and mtime: the snapshot is not read again.
        let modified = fs::metadata(&snapshot).unwrap().modified().unwrap();
        let original = fs::read(&snapshot).unwrap();
        fs::write(&snapshot, vec![b' '; original.len()]).unwrap();
        fs::File::options()
            .write(true)
            .open(&snapshot)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        // A new turn in the active file is.
        fs::write(
            &active,
            [
                assistant("msg-b", "turn-2", 1791050553292, 800),
                assistant("msg-c", "turn-3", 1791050554292, 900),
            ]
            .join("\n"),
        )
        .unwrap();

        let inputs: Vec<i64> = scan(home.path(), true)
            .iter()
            .map(|m| m.tokens.input)
            .collect();
        assert_eq!(inputs, vec![704, 800, 900]);
    }

    #[test]
    #[serial]
    fn a_captured_turn_id_containing_colons_is_skipped_whole() {
        let _env = clear_env();
        let home = tempfile::tempdir().unwrap();
        let dir = session_dir(&home.path().join(".minimax"));
        let internal = "mavis-internal:scope:id";
        fs::write(
            dir.join("messages.jsonl"),
            [
                assistant("msg-a", internal, 1791050552292, 704),
                assistant("msg-b", "mavis-internal", 1791050553292, 800),
            ]
            .join("\n"),
        )
        .unwrap();
        write_capture(home.path(), internal);

        let inputs: Vec<i64> = scan(home.path(), true)
            .iter()
            .map(|m| m.tokens.input)
            .collect();

        // The captured turn is skipped, and its first segment is not mistaken
        // for a different turn that happens to share it.
        assert_eq!(inputs, vec![800]);
    }

    #[test]
    #[serial]
    fn overlap_follows_what_upstream_counted_not_the_capture_now() {
        let _env = clear_env();
        let home = tempfile::tempdir().unwrap();
        let dir = session_dir(&home.path().join(".minimax"));
        fs::write(
            dir.join("messages.jsonl"),
            assistant("msg-a", "turn-1", 1791050552292, 704),
        )
        .unwrap();

        // Upstream read the capture before its final result was written, so
        // it counted nothing; the capture completes before the supplement runs.
        let headless = home.path().join(".config/tokscale/headless/mcode");
        fs::create_dir_all(&headless).unwrap();
        fs::write(headless.join("capture.jsonl"), "").unwrap();
        let before = upstream_counted(home.path(), true);
        write_capture(home.path(), "turn-1");
        assert_eq!(scan_with(home.path(), true, &before).len(), 1);

        // Upstream counted the turn; the capture disappears before the
        // supplement runs.
        let counted = upstream_counted(home.path(), true);
        fs::remove_file(headless.join("capture.jsonl")).unwrap();
        assert!(scan_with(home.path(), true, &counted).is_empty());
    }

    #[test]
    #[serial]
    fn a_failed_read_serves_the_last_complete_read() {
        let _env = clear_env();
        let home = tempfile::tempdir().unwrap();
        let dir = session_dir(&home.path().join(".minimax"));
        let active = dir.join("messages.jsonl");
        fs::write(&active, assistant("msg-a", "turn-1", 1791050552292, 704)).unwrap();
        assert_eq!(scan(home.path(), true).len(), 1);

        // A changed fingerprint that cannot be read keeps the cached rows,
        // and the failure is not cached under the new fingerprint.
        fs::write(&active, assistant("msg-a", "turn-1", 1791050552292, 7040)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&active, fs::Permissions::from_mode(0o000)).unwrap();
            if fs::File::open(&active).is_ok() {
                // Running as root: permissions cannot make the read fail.
                return;
            }
            let inputs: Vec<i64> = scan(home.path(), true)
                .iter()
                .map(|m| m.tokens.input)
                .collect();
            assert_eq!(inputs, vec![704]);
            fs::set_permissions(&active, fs::Permissions::from_mode(0o644)).unwrap();
            let inputs: Vec<i64> = scan(home.path(), true)
                .iter()
                .map(|m| m.tokens.input)
                .collect();
            assert_eq!(inputs, vec![7040]);
        }
    }

    #[test]
    fn reasoning_is_added_only_when_output_cannot_hold_it() {
        // Inside output: what MiniMax Code's headless captures report.
        let inside = tokens_from_usage(&serde_json::json!({
            "input": 100, "output": 24, "reasoning": 9, "totalTokens": 124
        }));
        assert_eq!((inside.output, inside.reasoning), (24, 0));
        // No total to tell: follow upstream and keep it in output.
        let unknown =
            tokens_from_usage(&serde_json::json!({"input": 7, "output": 2, "reasoning": 1}));
        assert_eq!((unknown.output, unknown.reasoning), (2, 0));
        // The total counts it on top of the other buckets.
        let separate = tokens_from_usage(&serde_json::json!({
            "input": 100, "output": 24, "cacheRead": 10, "reasoning": 9, "totalTokens": 143
        }));
        assert_eq!((separate.output, separate.reasoning), (24, 9));
        // Larger than output, so it cannot be inside it.
        let only =
            tokens_from_usage(&serde_json::json!({"input": 0, "output": 0, "reasoning": 12}));
        assert_eq!((only.output, only.reasoning), (0, 12));
        assert_eq!(only.total(), 12);
    }

    #[test]
    fn headless_keys_split_at_the_trailing_numbers() {
        assert_eq!(
            headless_turn("mcode:mvs_1:mavis-internal:scope:id:0:10:1:0:0"),
            Some(("mvs_1".to_string(), "mavis-internal:scope:id".to_string()))
        );
        assert_eq!(
            headless_turn("mcode:mvs_1:turn-1:2:10:1:0:0"),
            Some(("mvs_1".to_string(), "turn-1".to_string()))
        );
        assert_eq!(headless_turn("mcode:store:mvs_1:msg"), None);
        assert_eq!(headless_turn("codex:mvs_1:turn-1:0:1:1:0:0"), None);
    }

    #[test]
    fn seconds_timestamps_become_milliseconds() {
        assert_eq!(normalize_timestamp(1_791_050_552), 1_791_050_552_000);
        assert_eq!(normalize_timestamp(1_791_050_552_292), 1_791_050_552_292);
        assert_eq!(normalize_timestamp(0), 0);
    }

    #[test]
    fn usage_follows_the_runtime_normalization() {
        let tokens = tokens_from_usage(&serde_json::json!({
            "input": "0", "output": 0, "total_tokens": 42,
            "cache": {"read": 7, "write": 3}
        }));
        assert_eq!(
            (
                tokens.input,
                tokens.output,
                tokens.cache_read,
                tokens.cache_write
            ),
            (42, 0, 7, 3)
        );
        let tokens = tokens_from_usage(&serde_json::json!({
            "input": -5, "output": 9, "cache_read": 2, "cache_write": 1
        }));
        assert_eq!(
            (
                tokens.input,
                tokens.output,
                tokens.cache_read,
                tokens.cache_write
            ),
            (0, 9, 2, 1)
        );
    }
}
