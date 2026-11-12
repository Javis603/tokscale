//! Qoder CN JSONL usage, parsed by the shared Qoder parser.
//! Keeps released source identities and last-complete-read fallback.

use super::js;
use crate::sessions::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::TokenBreakdown;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use tracing::warn;

pub const CLIENT_ID: &str = "qodercn";
const PROVIDER_ID: &str = "qodercn";
const UNPRICED_PROVIDER_ID: &str = "unpriced:qodercn";
const JSONL_MAX_DEPTH: usize = 6;
const JSONL_MAX_FILES: usize = 5000;
const JSONL_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
const JSONL_MAX_ROWS: usize = 100_000;
const JSONL_MAX_LINE_BYTES: usize = 32 * 1024 * 1024;
const CACHE_VERSION: u32 = 4;
const ROUTING_TIERS: &[&str] = &["auto", "ultimate", "performance", "efficient", "lite"];

// Decode previous JSONL cache snapshots once; new caches store native messages.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct LegacyRow {
    session_id: String,
    message_id: String,
    model: String,
    workspace: String,
    input: i64,
    output: i64,
    cache_read: i64,
    created_at: i64,
    #[serde(default)]
    native: Option<UnifiedMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Fingerprint {
    len: u64,
    modified_ns: u128,
    #[serde(default)]
    agent_meta: Option<(u64, u128)>,
    #[serde(default)]
    parser_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CachedSource {
    fingerprint: Fingerprint,
    #[serde(default)]
    messages: Vec<UnifiedMessage>,
    #[serde(default, rename = "rows", skip_serializing)]
    legacy_rows: Vec<LegacyRow>,
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Cache {
    version: u32,
    sources: BTreeMap<String, CachedSource>,
}

pub fn parse(home_dir: &str, use_env_roots: bool) -> Vec<UnifiedMessage> {
    let root = projects_dir(Path::new(home_dir), use_env_roots);
    let cache_path = crate::paths::get_cache_dir()
        .join("token-monitor")
        .join(format!("qodercn-{}.json", js::path_namespace(&root)));
    parse_with(&root, &cache_path)
}

fn env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    std::path::absolute(trimmed).ok()
}

fn projects_dir(home: &Path, use_env_roots: bool) -> PathBuf {
    let configured = use_env_roots
        .then(|| {
            env_path("TOKEN_MONITOR_QODER_CN_PROJECTS_PATH")
                .or_else(|| env_path("QODERCN_CONFIG_DIR").map(|root| root.join("projects")))
        })
        .flatten();
    configured.unwrap_or_else(|| home.join(".qoder-cn").join("projects"))
}

fn parse_with(root: &Path, cache_path: &Path) -> Vec<UnifiedMessage> {
    let loaded = load_cache(cache_path, root);
    let mut next = Cache {
        version: CACHE_VERSION,
        sources: BTreeMap::new(),
    };
    collect_jsonl(root, &loaded, &mut next);
    if next != loaded {
        save_cache(cache_path, &next);
    }
    let messages = next
        .sources
        .into_values()
        .flat_map(|entry| entry.messages)
        .map(|mut message| {
            message.qoder_prompt_tokens = Some(message.tokens.input + message.tokens.cache_read);
            message
        })
        .collect();
    let mut messages = crate::sessions::qoder::reconcile_request_copies(messages);
    for message in &mut messages {
        message.refresh_derived_fields();
    }
    messages
}

fn load_cache(path: &Path, root: &Path) -> Cache {
    let Some(mut cache) = std::fs::read(path)
        .or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                std::fs::read(path.with_file_name("qodercn.json"))
            } else {
                Err(error)
            }
        })
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Cache>(&bytes).ok())
        .filter(|cache| matches!(cache.version, 1..=CACHE_VERSION))
    else {
        return Cache::default();
    };
    cache.sources.retain(|source, _| {
        Path::new(source).starts_with(root)
            && Path::new(source)
                .extension()
                .is_some_and(|ext| ext == "jsonl")
    });
    for entry in cache.sources.values_mut() {
        let legacy = std::mem::take(&mut entry.legacy_rows);
        if cache.version < CACHE_VERSION {
            entry.messages = legacy.into_iter().map(legacy_message).collect();
        }
    }
    cache
}

fn legacy_message(row: LegacyRow) -> UnifiedMessage {
    if let Some(message) = row.native {
        return message;
    }
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
            cache_write_1h: 0,
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

fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Fingerprint {
        len: meta.len(),
        modified_ns: modified_ns(&meta),
        agent_meta: std::fs::metadata(path.with_extension("meta.json"))
            .ok()
            .map(|meta| (meta.len(), modified_ns(&meta))),
        parser_version: CACHE_VERSION,
    })
}

fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
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

fn read_jsonl_file(path: &Path, bytes_read: &mut u64) -> Result<Vec<UnifiedMessage>, String> {
    let source = js::path_namespace(path);
    let mut read = Ok(());
    let messages = crate::sessions::qoder::parse_qoder_measured_source(path, |sink| {
        read = read_jsonl_source(path, bytes_read, sink);
    });
    read?;
    Ok(messages
        .into_iter()
        .filter(|message| {
            [
                message.tokens.input,
                message.tokens.output,
                message.tokens.cache_read,
                message.tokens.cache_write,
                message.tokens.reasoning,
            ]
            .iter()
            .all(|&n| n as f64 <= js::MAX_SAFE_INTEGER)
        })
        .map(|mut message| {
            let native_session = message
                .parent_session_id
                .as_deref()
                .unwrap_or(&message.session_id);
            let session = format!("qodercn:jsonl:{source}:{native_session}");
            message.client = CLIENT_ID.into();
            if let Some(model) = message
                .pricing_model_id
                .as_deref()
                .or(Some(message.model_id.as_str()))
                .filter(|raw| raw.starts_with("qoder-custom-"))
                .and_then(|raw| raw.split_once('/').map(|(_, model)| model.to_string()))
            {
                // The profile is a client namespace, not a billing provider.
                message.provider_id = crate::provider_identity::canonical_provider(&model)
                    .unwrap_or_else(|| PROVIDER_ID.to_string())
                    .into();
                message.model_id = model.into();
                message.pricing_model_id = None;
            }
            if message.workspace_key.as_deref().is_some_and(|cwd| {
                cwd.contains("/remote-control/")
                    || cwd.contains("\\remote-control\\")
                    || cwd.contains("com.qodercn.app.stable")
            }) {
                message.workspace_key = None;
                message.workspace_label = None;
            }
            if ROUTING_TIERS.contains(&message.model_id.to_ascii_lowercase().as_str()) {
                message.provider_id = UNPRICED_PROVIDER_ID.into();
            }
            message.session_id = session.clone().into();
            // Preserve cross-file request dedup while keeping the released
            // path-qualified session identity for archived usage.
            let id = message
                .dedup_key
                .clone()
                .unwrap_or_default()
                .replacen("qoder:", "qodercn:", 1);
            message.dedup_key = Some(id.clone());
            // Gemini is normalized by the shared parser; its reported prompt
            // is retained for reconciliation. CN input_tokens also includes
            // cache reads, so both formats split the inclusive prompt here.
            let prompt = message.qoder_prompt_tokens.unwrap_or(message.tokens.input);
            message.tokens.cache_read = message.tokens.cache_read.min(prompt);
            message.tokens.input = prompt - message.tokens.cache_read;
            message.refresh_derived_fields();
            if let Some(parent) = message.parent_session_id.as_ref() {
                let parent_path = path
                    .parent()
                    .and_then(Path::parent)
                    .map(|dir| dir.with_extension("jsonl"));
                if let Some(parent_path) = parent_path {
                    message.parent_session_id = Some(
                        format!(
                            "qodercn:jsonl:{}:{parent}",
                            js::path_namespace(&parent_path)
                        )
                        .into(),
                    );
                }
            }
            message
        })
        .collect())
}

fn read_jsonl_source(
    path: &Path,
    bytes_read: &mut u64,
    sink: &mut dyn FnMut(usize, &str),
) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let mut reader = BufReader::new(file);
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
        let text = String::from_utf8_lossy(content);
        sink(0, text.trim_start_matches('\u{feff}').trim());
    }
    Ok(())
}

fn collect_jsonl(root: &Path, cache: &Cache, next: &mut Cache) {
    let root_key = key(root);
    let is_under_root = |source: &String| {
        Path::new(source).starts_with(root)
            && source != &root_key
            && Path::new(source)
                .extension()
                .is_some_and(|ext| ext == "jsonl")
    };
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
            let current =
                fingerprint(&path).ok_or_else(|| format!("{}: stat failed", path.display()))?;
            let entry = match previous.get(&source_key) {
                Some(entry) if entry.fingerprint == current => entry.clone(),
                _ => CachedSource {
                    fingerprint: current,
                    messages: read_jsonl_file(&path, &mut bytes_read)?,
                    legacy_rows: Vec::new(),
                },
            };
            row_count += entry.messages.len();
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
    use serde_json::{json, Value};
    use std::time::Duration;

    fn paths(dir: &Path) -> PathBuf {
        dir.join("projects")
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

    #[test]
    fn jsonl_splits_the_cached_prefix_out_of_input_and_maps_models() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        write_jsonl(
            &p.join("ws").join("s-1.jsonl"),
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
        assert_eq!(gpt.provider_id, "openai");
        assert_eq!(gpt.pricing_model_id, None);
        let flash = messages.iter().find(|m| m.model_id == "dfmodel").unwrap();
        assert_eq!(flash.workspace_label, None);
    }

    #[test]
    fn a_failed_jsonl_read_serves_the_last_complete_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let cache = dir.path().join("cache.json");
        write_jsonl(
            &p.join("s-1.jsonl"),
            &[assistant("m1", "gmodel", 7, 0, 3, "")],
        );
        assert_eq!(parse_with(&p, &cache).len(), 1);
        let mut deep = p.clone();
        for level in 0..=JSONL_MAX_DEPTH {
            deep = deep.join(format!("d{level}"));
        }
        write_jsonl(
            &deep.join("late.jsonl"),
            &[assistant("m9", "gmodel", 1, 0, 1, "")],
        );
        let messages = parse_with(&p, &cache);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.total(), 10);
    }

    #[test]
    fn unchanged_sources_are_served_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let cache = dir.path().join("cache.json");
        let file = p.join("s-1.jsonl");
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
            &p.join("s-1.jsonl"),
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
        write_jsonl(
            &p.join("s-1.jsonl"),
            &[
                assistant("oversized", "gmodel", 7, 0, i64::MAX, ""),
                assistant("valid", "gmodel", 10, 0, 5, ""),
            ],
        );
        let messages = parse_with(&p, &dir.path().join("cache.json"));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.total(), 15);
    }

    #[test]
    fn divergent_fork_snapshots_keep_complete_counters_in_either_path_order() {
        for (main, fork) in [("main", "z-fork"), ("z-main", "a-fork")] {
            let dir = tempfile::tempdir().unwrap();
            let p = paths(dir.path());
            let cache = dir.path().join("cache.json");
            let main_path = p.join(format!("ws/{main}.jsonl"));
            let fork_path = p.join(format!("ws/{fork}.jsonl"));
            let mut complete = assistant("shared", "gpt-5", 100, 60, 10, "/work/app");
            complete["message"]["usage"]["cache_creation_input_tokens"] = json!(20);
            complete["message"]["usage"]["thinking_tokens"] = json!(3);
            write_jsonl(&main_path, &[complete.clone()]);
            write_jsonl(
                &fork_path,
                &[assistant("shared", "gpt-5", 100, 0, 2, "/work/app")],
            );
            for output in [10, 30] {
                if output == 30 {
                    let mut final_fragment = complete.clone();
                    final_fragment["message"]["usage"]["output_tokens"] = json!(30);
                    write_jsonl(&main_path, &[complete.clone(), final_fragment]);
                }
                for _ in 0..2 {
                    let messages = parse_with(&p, &cache);
                    assert_eq!(messages.len(), 1);
                    let message = &messages[0];
                    assert_eq!((message.tokens.input, message.tokens.cache_read), (40, 60));
                    assert_eq!(message.tokens.output, output);
                    assert_eq!(message.tokens.cache_write, 20);
                    assert_eq!(message.tokens.reasoning, 3);
                    assert_eq!(message.tokens.total(), 123 + output);
                }
            }
        }
    }

    #[test]
    fn gemini_usage_splits_cached_prompt_once_and_keeps_reasoning_cold_and_warm() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let cache = dir.path().join("cache.json");
        write_jsonl(
            &p.join("session.jsonl"),
            &[json!({
                "type":"assistant", "sessionId":"gemini-session",
                "timestamp":"2026-11-11T10:00:00Z",
                "message":{"id":"gemini-request","model":"gemini-2.5-pro",
                    "usage":{"promptTokenCount":100,"cachedContentTokenCount":40,
                        "candidatesTokenCount":20,"thoughtsTokenCount":10,"totalTokenCount":130}}
            })],
        );
        for _ in 0..2 {
            let messages = parse_with(&p, &cache);
            assert_eq!(messages.len(), 1);
            assert_eq!(
                (messages[0].tokens.input, messages[0].tokens.cache_read),
                (60, 40)
            );
            assert_eq!(messages[0].tokens.output, 20);
            assert_eq!(messages[0].tokens.reasoning, 10);
            assert_eq!(messages[0].tokens.total(), 130);
        }
    }

    #[test]
    fn numeric_custom_profiles_preserve_released_model_identity() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        write_jsonl(
            &p.join("ws/s-1.jsonl"),
            &[assistant(
                "m1",
                "qoder-custom-123/openai/gpt-5",
                100,
                60,
                10,
                "/work/app",
            )],
        );
        for _ in 0..2 {
            let messages = parse_with(&p, &dir.path().join("cache.json"));
            assert_eq!(messages[0].model_id, "openai/gpt-5");
            assert_eq!(messages[0].provider_id, "openai");
            assert_eq!(messages[0].pricing_model_id, None);
        }
    }

    #[test]
    fn upstream_assembly_tracks_runtime_models_and_deduplicates_forks() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let file = p.join("ws/s-1.jsonl");
        let mut first = assistant("m1", "auto", 100, 60, 2, "/work/app");
        first["message"]["content"] = json!([{"type":"text","text":"one"}]);
        let mut final_fragment = first.clone();
        final_fragment["message"]["usage"]["output_tokens"] = json!(10);
        let rows = [
            json!({"type":"runtime-config","model":"gfmodel"}),
            first,
            final_fragment,
        ];
        write_jsonl(&file, &rows);
        write_jsonl(&p.join("ws/fork.jsonl"), &rows);
        let cache = dir.path().join("cache.json");
        for _ in 0..2 {
            let messages = parse_with(&p, &cache);
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].model_id, "gfmodel");
            assert_eq!(messages[0].tokens.total(), 110);
            assert!(messages[0].session_id.ends_with(":s-1"));
        }
    }

    #[test]
    fn managed_text_is_not_invented_usage_and_sidecar_changes_invalidate_cache() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let file = p.join("ws/s-1/subagents/agent-a.jsonl");
        let mut row = assistant("m1", "auto", 0, 0, 0, "/work/app");
        row["isSidechain"] = json!(true);
        row["agentId"] = json!("a");
        row["message"]["content"] =
            json!([{"type":"text","text":"an answer with enough text to estimate tokens"}]);
        write_jsonl(&file, &[row.clone()]);
        let cache = dir.path().join("cache.json");
        assert!(parse_with(&p, &cache).is_empty());
        row["message"]["usage"] = json!({"input_tokens":100,"output_tokens":10});
        write_jsonl(&file, &[row]);
        parse_with(&p, &cache);
        std::fs::write(
            file.with_extension("meta.json"),
            r#"{"agentType":"explore"}"#,
        )
        .unwrap();
        let messages = parse_with(&p, &cache);
        assert_eq!(messages[0].agent.as_deref(), Some("Explore"));
        assert!(messages[0].session_id.ends_with(":s-1"));
        assert!(messages[0]
            .parent_session_id
            .as_deref()
            .unwrap()
            .ends_with(":s-1"));
    }

    #[test]
    fn cn_keeps_upstream_request_timing_and_cache_write_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let p = paths(dir.path());
        let mut row = assistant("timed", "gpt-5", 100, 60, 10, "/work/app");
        row["message"]["usage"]["cache_creation_input_tokens"] = json!(20);
        row["message"]["usage"]["thinking_tokens"] = json!(3);
        write_jsonl(
            &p.join("ws/s-1.jsonl"),
            &[
                json!({"type":"user","sessionId":"s-1","timestamp":"2026-10-01T09:59:58Z","message":{"content":"request"}}),
                row,
            ],
        );
        for _ in 0..2 {
            let messages = parse_with(&p, &dir.path().join("cache.json"));
            assert_eq!(messages[0].duration_ms, Some(2000));
            assert_eq!(messages[0].tokens.cache_write, 20);
            assert_eq!(messages[0].tokens.reasoning, 3);
        }
    }

    #[test]
    fn cache_upgrade_keeps_jsonl_snapshots_but_discards_retired_database_sources() {
        for version in 1..=3 {
            let dir = tempfile::tempdir().unwrap();
            let p = paths(dir.path());
            let file = p.join("s-1.jsonl");
            write_jsonl(&file, &[assistant("m1", "gmodel", 7, 0, 3, "")]);
            let cache = dir.path().join("cache.json");
            parse_with(&p, &cache);
            let mut old: Value = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
            old["version"] = json!(version);
            let native = old["sources"][key(&file)]["messages"][0].clone();
            let row = json!({"session_id":native["session_id"],"message_id":"old-jsonl",
                "model":"gmodel","workspace":"","input":7,"output":3,
                "cache_read":0,"created_at":1790848800000_i64,
                "native":if version==3 {native} else {Value::Null}});
            let source = &mut old["sources"][key(&file)];
            source.as_object_mut().unwrap().remove("messages");
            source["rows"] = json!([row.clone()]);
            source["fingerprint"]
                .as_object_mut()
                .unwrap()
                .remove("parser_version");
            source["fingerprint"]
                .as_object_mut()
                .unwrap()
                .remove("agent_meta");
            let retired = p.join("local.db");
            old["sources"][key(&retired)] = source.clone();
            std::fs::write(&retired, b"SQLite format 3\0retired").unwrap();
            std::fs::write(&cache, old.to_string()).unwrap();
            let mut deep = p.clone();
            for level in 0..=JSONL_MAX_DEPTH {
                deep = deep.join(format!("d{level}"));
            }
            std::fs::create_dir_all(deep).unwrap();
            let messages = parse_with(&p, &cache);
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].tokens.total(), 10);
            let saved: Value = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
            assert_eq!(saved["version"], json!(CACHE_VERSION));
            assert!(saved["sources"].get(key(&retired)).is_none());
            assert!(saved["sources"][key(&file)].get("rows").is_none());
            assert_eq!(std::fs::read(retired).unwrap(), b"SQLite format 3\0retired");
        }
    }
}
