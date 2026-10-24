use super::*;
use message_cache::{CacheIdentity, SourceMessageCache};
use rusqlite::{params, Connection};
use std::fs;

fn varint(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    while value >= 128 {
        bytes.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    bytes.push(value as u8);
    bytes
}

fn number(field: u64, value: u64) -> Vec<u8> {
    [varint(field << 3), varint(value)].concat()
}

fn blob(field: u64, value: &[u8]) -> Vec<u8> {
    [
        varint((field << 3) | 2),
        varint(value.len() as u64),
        value.to_vec(),
    ]
    .concat()
}

fn generation(response: &str, input: u64) -> Vec<u8> {
    let usage = [
        number(2, input),
        number(9, 20),
        blob(11, response.as_bytes()),
    ]
    .concat();
    let chat = [
        blob(4, &usage),
        blob(19, b"gemini-3-flash-a"),
        blob(9, &blob(4, &number(1, 1_781_502_653))),
        blob(11, &number(1, 1)),
        blob(12, &number(1, 2)),
    ]
    .concat();
    blob(1, &chat)
}

fn database(home: &Path, surface: &str, session: &str) -> (PathBuf, Connection) {
    let path = home
        .join(".gemini")
        .join(surface)
        .join("conversations")
        .join(format!("{session}.db"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE gen_metadata(idx INTEGER PRIMARY KEY, data BLOB);
        CREATE TABLE trajectory_metadata_blob(data BLOB);
        CREATE TABLE steps(idx INTEGER, step_type INTEGER, metadata BLOB);",
    )
    .unwrap();
    let workspace = blob(1, &blob(1, b"file:///test/workspace"));
    conn.execute(
        "INSERT INTO trajectory_metadata_blob VALUES (?1)",
        [workspace],
    )
    .unwrap();
    insert(&conn, 0, "shared-response", 100);
    (path, conn)
}

fn insert(conn: &Connection, idx: i64, response: &str, input: u64) {
    conn.execute(
        "INSERT OR REPLACE INTO gen_metadata VALUES (?1, ?2)",
        params![idx, generation(response, input)],
    )
    .unwrap();
}

fn scan(home: &Path, clients: &[&str]) -> Vec<UnifiedMessage> {
    parse_all_messages_with_pricing_with_env_strategy(
        home.to_str().unwrap(),
        &clients
            .iter()
            .map(|client| client.to_string())
            .collect::<Vec<_>>(),
        None,
        false,
        &scanner::ScannerSettings::default(),
    )
}

fn cache_env(home: &Path) -> paths::test_env::EnvGuard {
    let mut env = paths::test_env::EnvGuard::capture(&["TOKSCALE_CONFIG_DIR"]);
    env.set("TOKSCALE_CONFIG_DIR", home.join("cache-config"));
    env
}

#[test]
#[serial_test::serial]
fn antigravity_db_cache_persists_before_family_dedup_and_matches_cold_scan() {
    let home = tempfile::TempDir::new().unwrap();
    let _env = cache_env(home.path());
    let (cli, conn) = database(home.path(), "antigravity-cli", "cli");
    insert(&conn, 1, "cli-only", 200);
    drop(conn);
    let (extension, conn) = database(home.path(), "antigravity", "extension");
    insert(&conn, 1, "extension-only", 300);
    drop(conn);

    let clients = ["antigravity-cli", "antigravity-extension"];
    let cold = scan(home.path(), &clients);
    assert_eq!(cold.len(), 3);
    for (client, path) in [
        (ClientId::AntigravityCli, &cli),
        (ClientId::AntigravityExtension, &extension),
    ] {
        let cached = SourceMessageCache::load()
            .get(CacheIdentity::for_client(client), path)
            .expect("successful Antigravity DB parses must persist across CLI processes");
        assert_eq!(
            cached.messages.len(),
            2,
            "cache owns raw rows before family dedup"
        );
        assert_eq!(cached.messages[0].duration_ms, Some(3000));
        assert!(cached.messages[0].workspace_key.is_some());
    }
    assert_eq!(scan(home.path(), &clients), cold);
    // Removing the winning source must expose the duplicate stored in the other
    // source; deduped results must never have been persisted as raw DB contents.
    fs::remove_file(cli).unwrap();
    let remaining = scan(home.path(), &clients);
    assert_eq!(remaining.len(), 2);
    assert!(remaining
        .iter()
        .all(|message| message.client == "antigravity-extension"));
}

#[test]
#[serial_test::serial]
fn antigravity_db_cache_wal_updates_replace_old_rows_and_match_cold_scan() {
    let home = tempfile::TempDir::new().unwrap();
    let _env = cache_env(home.path());
    let (path, conn) = database(home.path(), "antigravity", "wal");
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    assert_eq!(
        scan(home.path(), &["antigravity-extension"])[0]
            .tokens
            .input,
        100
    );
    let main_before = fs::read(&path).unwrap();
    let modified_before = fs::metadata(&path).unwrap().modified().unwrap();
    insert(&conn, 0, "shared-response", 900);
    assert_eq!(fs::read(&path).unwrap(), main_before);
    assert_eq!(
        fs::metadata(&path).unwrap().modified().unwrap(),
        modified_before
    );
    let warm = scan(home.path(), &["antigravity-extension"]);
    assert_eq!(warm[0].tokens.input, 900);
    let _cold_env = cache_env(&home.path().join("cold"));
    assert_eq!(scan(home.path(), &["antigravity-extension"]), warm);
    drop(conn);
}

#[test]
#[serial_test::serial]
fn antigravity_db_cache_locked_read_keeps_last_success_and_retries() {
    let home = tempfile::TempDir::new().unwrap();
    let _env = cache_env(home.path());
    let (path, conn) = database(home.path(), "antigravity", "locked");
    let before = scan(home.path(), &["antigravity-extension"]);
    let identity = CacheIdentity::for_client(ClientId::AntigravityExtension);
    let fingerprint = SourceMessageCache::load()
        .get(identity, &path)
        .unwrap()
        .fingerprint;
    conn.execute_batch("BEGIN EXCLUSIVE").unwrap();
    insert(&conn, 0, "shared-response", 700);
    let later =
        fs::metadata(&path).unwrap().modified().unwrap() + std::time::Duration::from_secs(2);
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(later))
        .unwrap();
    assert_eq!(scan(home.path(), &["antigravity-extension"]), before);
    assert_eq!(
        SourceMessageCache::load()
            .get(identity, &path)
            .unwrap()
            .fingerprint,
        fingerprint
    );
    conn.execute_batch("COMMIT").unwrap();
    assert_eq!(
        scan(home.path(), &["antigravity-extension"])[0]
            .tokens
            .input,
        700
    );
    drop(conn);
}

#[test]
#[serial_test::serial]
fn antigravity_db_cache_failed_optional_or_generation_rows_are_not_persisted() {
    let home = tempfile::TempDir::new().unwrap();
    let _env = cache_env(home.path());
    let (path, conn) = database(home.path(), "antigravity", "invalid");
    let before = scan(home.path(), &["antigravity-extension"]);
    let identity = CacheIdentity::for_client(ClientId::AntigravityExtension);
    let original = SourceMessageCache::load()
        .get(identity, &path)
        .unwrap()
        .fingerprint;
    for bad_sql in [
        "UPDATE trajectory_metadata_blob SET data = 'not a blob'",
        "INSERT INTO steps VALUES (0,15,'not a blob')",
        "INSERT INTO gen_metadata VALUES (99,'not a blob')",
    ] {
        conn.execute_batch(bad_sql).unwrap();
        insert(&conn, 0, "shared-response", 800);
        assert_eq!(scan(home.path(), &["antigravity-extension"]), before);
        assert_eq!(
            SourceMessageCache::load()
                .get(identity, &path)
                .unwrap()
                .fingerprint,
            original
        );
        conn.execute_batch("DELETE FROM trajectory_metadata_blob; DELETE FROM steps; DELETE FROM gen_metadata WHERE idx=99").unwrap();
    }
    let repaired = scan(home.path(), &["antigravity-extension"]);
    assert_eq!(repaired[0].tokens.input, 800);
    assert_eq!(scan(home.path(), &["antigravity-extension"]), repaired);
    drop(conn);
}

#[test]
#[serial_test::serial]
fn antigravity_db_cache_empty_snapshot_is_valid_and_recreation_replaces_it() {
    let home = tempfile::TempDir::new().unwrap();
    let _env = cache_env(home.path());
    let (path, conn) = database(home.path(), "antigravity", "empty");
    conn.execute_batch(
        "DELETE FROM gen_metadata; DROP TABLE steps; DROP TABLE trajectory_metadata_blob",
    )
    .unwrap();
    assert!(scan(home.path(), &["antigravity-extension"]).is_empty());
    let cached = SourceMessageCache::load()
        .get(
            CacheIdentity::for_client(ClientId::AntigravityExtension),
            &path,
        )
        .unwrap();
    assert!(cached.messages.is_empty());
    insert(&conn, 0, "new-response", 50);
    assert_eq!(
        scan(home.path(), &["antigravity-extension"])[0]
            .tokens
            .input,
        50
    );
    drop(conn);
    fs::remove_file(&path).unwrap();
    assert!(scan(home.path(), &["antigravity-extension"]).is_empty());
    let (_, conn) = database(home.path(), "antigravity", "empty");
    assert_eq!(
        scan(home.path(), &["antigravity-extension"])[0]
            .tokens
            .input,
        100
    );
    drop(conn);
}

#[test]
#[serial_test::serial]
fn antigravity_db_cache_without_steps_metadata_reuses_and_refreshes_snapshots() {
    for (surface, client) in [
        ("antigravity-cli", ClientId::AntigravityCli),
        ("antigravity", ClientId::AntigravityExtension),
    ] {
        for previously_cached in [false, true] {
            let home = tempfile::TempDir::new().unwrap();
            let _env = cache_env(home.path());
            let (path, conn) = database(home.path(), surface, "payload-only");
            let identity = CacheIdentity::for_client(client);
            if previously_cached {
                assert_eq!(scan(home.path(), &[client.as_str()])[0].tokens.input, 100);
                assert!(SourceMessageCache::load().get(identity, &path).is_some());
            }
            // Observed IDE schema: step_payload exists, but optional metadata
            // timestamps are unavailable. Token rows remain fully readable.
            conn.execute_batch(
                "DROP TABLE steps;
                CREATE TABLE steps(idx INTEGER PRIMARY KEY,
                    step_type INTEGER NOT NULL DEFAULT 0,
                    status INTEGER NOT NULL DEFAULT 0, step_payload BLOB);
                INSERT INTO steps VALUES(0, 15, 0, X'00');",
            )
            .unwrap();
            for input in [400_i64, 900] {
                insert(&conn, 0, "shared-response", input as u64);
                let fresh =
                    sessions::antigravity_cli::scan_antigravity_db_file(&path, client.as_str());
                assert_eq!(fresh.messages[0].tokens.input, input);
                assert_eq!(scan(home.path(), &[client.as_str()])[0].tokens.input, input);
                assert!(
                    fresh.complete,
                    "absent optional timestamps are not a failed read"
                );
                let cached = SourceMessageCache::load().get(identity, &path).unwrap();
                assert_eq!(cached.messages, fresh.messages);
                let (warm, entry) = load_or_parse_antigravity_db_source(
                    &path,
                    client,
                    &SourceMessageCache::load(),
                    |_, _| panic!("unchanged payload-only schema must reuse its cache"),
                );
                assert_eq!(warm, fresh.messages);
                assert!(entry.is_none());
            }
        }
    }
}

#[test]
#[serial_test::serial]
fn antigravity_db_cache_warm_hit_never_opens_sqlite_and_racing_read_is_retried() {
    let home = tempfile::TempDir::new().unwrap();
    let _env = cache_env(home.path());
    let (path, conn) = database(home.path(), "antigravity", "racing");
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    insert(&conn, 0, "shared-response", 100);
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let before = scan(home.path(), &["antigravity-extension"]);
    let client = ClientId::AntigravityExtension;
    let identity = CacheIdentity::for_client(client);
    let cached = SourceMessageCache::load().get(identity, &path).unwrap();
    let (warm, entry) =
        load_or_parse_antigravity_db_source(&path, client, &SourceMessageCache::load(), |_, _| {
            panic!("unchanged DB must not open SQLite")
        });
    assert_eq!(warm, before);
    assert!(entry.is_none());
    insert(&conn, 0, "shared-response", 400);
    let (racing, entry) = load_or_parse_antigravity_db_source(
        &path,
        client,
        &SourceMessageCache::load(),
        |path, client| {
            let snapshot = sessions::antigravity_cli::scan_antigravity_db_file(path, client);
            insert(&conn, 0, "shared-response", 600);
            snapshot
        },
    );
    assert_eq!(
        racing[0].tokens.input, 400,
        "successful WAL reads return their fresh snapshot"
    );
    assert!(
        entry.is_none(),
        "never cache the old snapshot under the newer WAL"
    );
    assert_eq!(
        SourceMessageCache::load()
            .get(identity, &path)
            .unwrap()
            .fingerprint,
        cached.fingerprint
    );
    assert_eq!(
        scan(home.path(), &["antigravity-extension"])[0]
            .tokens
            .input,
        600
    );
    drop(conn);
}

#[test]
#[serial_test::serial]
fn antigravity_db_cache_nonempty_wal_bypasses_poisoned_warm_entry() {
    let home = tempfile::TempDir::new().unwrap();
    let _env = cache_env(home.path());
    let (path, conn) = database(home.path(), "antigravity", "publication");
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    insert(&conn, 0, "shared-response", 900);
    let identity = CacheIdentity::for_client(ClientId::AntigravityExtension);
    let fresh = sessions::antigravity_cli::scan_antigravity_db_file(&path, "antigravity-extension");
    assert!(fresh.complete);
    let mut stale = fresh.messages.clone();
    stale[0].tokens.input = 100;
    // Model the publication race: old SQL values stored under the FINAL WAL
    // fingerprint. No DB/WAL bytes change between this entry and the scan.
    let fingerprint = message_cache::SourceFingerprint::from_sqlite_path(&path).unwrap();
    let mut cache = SourceMessageCache::load();
    cache.insert(message_cache::CachedSourceEntry::new(
        identity,
        &path,
        fingerprint.clone(),
        stale,
        Vec::new(),
        None,
    ));
    cache.save_if_dirty();
    let (messages, entry) = load_or_parse_antigravity_db_source(
        &path,
        ClientId::AntigravityExtension,
        &SourceMessageCache::load(),
        sessions::antigravity_cli::scan_antigravity_db_file,
    );
    assert_eq!(messages[0].tokens.input, 900);
    assert!(
        entry.is_none(),
        "a nonempty WAL must never publish a new cache entry"
    );
    assert_eq!(
        message_cache::SourceFingerprint::from_sqlite_path(&path).unwrap(),
        fingerprint
    );
    // The bypass also protects a cold cache, and a failed parse still falls
    // back to the previous accepted snapshot instead of treating it as empty.
    let empty_cache = SourceMessageCache::default();
    let (messages, entry) = load_or_parse_antigravity_db_source(
        &path,
        ClientId::AntigravityExtension,
        &empty_cache,
        sessions::antigravity_cli::scan_antigravity_db_file,
    );
    assert_eq!(messages[0].tokens.input, 900);
    assert!(entry.is_none());
    let (fallback, entry) = load_or_parse_antigravity_db_source(
        &path,
        ClientId::AntigravityExtension,
        &SourceMessageCache::load(),
        |_, _| sessions::antigravity_cli::AntigravityDbScan::default(),
    );
    assert_eq!(fallback[0].tokens.input, 100);
    assert!(entry.is_none());
    drop(conn);
}

#[test]
#[serial_test::serial]
fn antigravity_db_cache_wal_appearing_during_parse_prevents_publication() {
    let home = tempfile::TempDir::new().unwrap();
    let _env = cache_env(home.path());
    let (path, conn) = database(home.path(), "antigravity", "appearing");
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    assert!(antigravity_db_cache_eligible(&path));
    let (messages, entry) = load_or_parse_antigravity_db_source(
        &path,
        ClientId::AntigravityExtension,
        &SourceMessageCache::default(),
        |path, client| {
            let scan = sessions::antigravity_cli::scan_antigravity_db_file(path, client);
            insert(&conn, 0, "shared-response", 900);
            scan
        },
    );
    assert_eq!(messages[0].tokens.input, 100);
    assert!(entry.is_none());
    assert!(!antigravity_db_cache_eligible(&path));
    assert_eq!(
        scan(home.path(), &["antigravity-extension"])[0]
            .tokens
            .input,
        900
    );
    drop(conn);
}

#[test]
#[serial_test::serial]
fn antigravity_db_cache_absent_and_empty_wals_allow_warm_hits() {
    let home = tempfile::TempDir::new().unwrap();
    let _env = cache_env(home.path());
    let (path, conn) = database(home.path(), "antigravity", "eligible");
    drop(conn);
    for empty_wal in [false, true] {
        if empty_wal {
            fs::write(path.with_extension("db-wal"), []).unwrap();
        }
        let expected = scan(home.path(), &["antigravity-extension"]);
        assert!(antigravity_db_cache_eligible(&path));
        let (warm, entry) = load_or_parse_antigravity_db_source(
            &path,
            ClientId::AntigravityExtension,
            &SourceMessageCache::load(),
            |_, _| panic!("eligible unchanged DB must not open SQLite"),
        );
        assert_eq!(warm, expected);
        assert!(entry.is_none());
    }
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn antigravity_db_cache_symlink_wal_updates_bypass_old_entries() {
    for (surface, client) in [
        ("antigravity-cli", ClientId::AntigravityCli),
        ("antigravity", ClientId::AntigravityExtension),
    ] {
        let home = tempfile::TempDir::new().unwrap();
        let _env = cache_env(home.path());
        let target_home = tempfile::TempDir::new().unwrap();
        let (target, conn) = database(target_home.path(), surface, "linked");
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let path = home
            .path()
            .join(".gemini")
            .join(surface)
            .join("conversations/linked.db");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let identity = CacheIdentity::for_client(client);
        let initial = scan(home.path(), &[client.as_str()]);
        assert_eq!(initial[0].tokens.input, 100);
        let cold_published = SourceMessageCache::load().get(identity, &path).is_some();
        // Seed an entry created before the symlink guard. SQLite follows the
        // target's WAL, while fingerprinting the alias misses that WAL.
        let fingerprint = message_cache::SourceFingerprint::from_sqlite_path(&path).unwrap();
        let mut cache = SourceMessageCache::load();
        cache.insert(message_cache::CachedSourceEntry::new(
            identity,
            &path,
            fingerprint.clone(),
            initial,
            Vec::new(),
            None,
        ));
        cache.save_if_dirty();
        let main_before = fs::read(&target).unwrap();
        let modified_before = fs::metadata(&target).unwrap().modified().unwrap();
        insert(&conn, 0, "shared-response", 900);
        assert_eq!(fs::read(&target).unwrap(), main_before);
        assert_eq!(
            fs::metadata(&target).unwrap().modified().unwrap(),
            modified_before
        );
        assert!(!path.with_extension("db-wal").exists());
        assert!(fs::metadata(target.with_extension("db-wal")).unwrap().len() > 0);
        assert_eq!(
            message_cache::SourceFingerprint::from_sqlite_path(&path).unwrap(),
            fingerprint
        );
        let fresh = sessions::antigravity_cli::scan_antigravity_db_file(&path, client.as_str());
        assert!(fresh.complete);
        assert_eq!(fresh.messages[0].tokens.input, 900);
        assert_eq!(scan(home.path(), &[client.as_str()])[0].tokens.input, 900);
        assert!(
            !cold_published,
            "symlink scans must not publish a cache entry"
        );
        let (messages, entry) = load_or_parse_antigravity_db_source(
            &path,
            client,
            &SourceMessageCache::load(),
            sessions::antigravity_cli::scan_antigravity_db_file,
        );
        assert_eq!(messages[0].tokens.input, 900);
        assert!(entry.is_none());
        assert!(!antigravity_db_cache_eligible(&path));
    }
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn antigravity_db_cache_symlink_appearing_during_parse_prevents_publication() {
    let home = tempfile::TempDir::new().unwrap();
    let _env = cache_env(home.path());
    let (path, conn) = database(home.path(), "antigravity", "moved");
    drop(conn);
    let target = home.path().join("target.db");
    let before = message_cache::SourceFingerprint::from_sqlite_path(&path).unwrap();
    let (messages, entry) = load_or_parse_antigravity_db_source(
        &path,
        ClientId::AntigravityExtension,
        &SourceMessageCache::default(),
        |path, client| {
            let snapshot = sessions::antigravity_cli::scan_antigravity_db_file(path, client);
            fs::rename(path, &target).unwrap();
            std::os::unix::fs::symlink(&target, path).unwrap();
            snapshot
        },
    );
    assert_eq!(messages[0].tokens.input, 100);
    assert_eq!(
        message_cache::SourceFingerprint::from_sqlite_path(&path).unwrap(),
        before
    );
    assert!(entry.is_none());
}

#[test]
#[serial_test::serial]
fn antigravity_db_cache_complete_unstable_reads_return_fresh_messages() {
    for wal in [false, true] {
        let home = tempfile::TempDir::new().unwrap();
        let _env = cache_env(home.path());
        let (path, conn) = database(home.path(), "antigravity", "unstable");
        let checkpoint = || {
            if wal {
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                    .unwrap();
            }
        };
        if wal {
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        }
        checkpoint();
        assert_eq!(
            scan(home.path(), &["antigravity-extension"])[0]
                .tokens
                .input,
            100
        );
        let identity = CacheIdentity::for_client(ClientId::AntigravityExtension);
        let cached = SourceMessageCache::load().get(identity, &path).unwrap();
        insert(&conn, 0, "shared-response", 400);
        checkpoint();
        assert!(antigravity_db_cache_eligible(&path));
        let before = message_cache::SourceFingerprint::from_sqlite_path(&path).unwrap();
        let (messages, entry) = load_or_parse_antigravity_db_source(
            &path,
            ClientId::AntigravityExtension,
            &SourceMessageCache::load(),
            |path, client| {
                let snapshot = sessions::antigravity_cli::scan_antigravity_db_file(path, client);
                assert!(snapshot.complete);
                assert_eq!(snapshot.messages[0].tokens.input, 400);
                insert(&conn, 0, "shared-response", 600);
                checkpoint();
                snapshot
            },
        );
        assert!(antigravity_db_cache_eligible(&path));
        assert_ne!(
            message_cache::SourceFingerprint::from_sqlite_path(&path).unwrap(),
            before
        );
        assert_eq!(
            messages[0].tokens.input, 400,
            "return the complete SQL snapshot"
        );
        assert!(
            entry.is_none(),
            "do not cache a snapshot under an unstable fingerprint"
        );
        let retained = SourceMessageCache::load().get(identity, &path).unwrap();
        assert_eq!(retained.fingerprint, cached.fingerprint);
        assert_eq!(retained.messages, cached.messages);
        assert_eq!(
            scan(home.path(), &["antigravity-extension"])[0]
                .tokens
                .input,
            600
        );
    }
}

#[test]
fn antigravity_db_cache_metadata_failures_and_nonfiles_fail_closed() {
    use std::io::{Error, ErrorKind};
    for kind in [
        ErrorKind::PermissionDenied,
        ErrorKind::Interrupted,
        ErrorKind::Other,
    ] {
        assert!(!antigravity_wal_allows_cache(Err(Error::from(kind))));
    }
    assert!(antigravity_wal_allows_cache(Err(Error::from(
        ErrorKind::NotFound
    ))));
    let directory = tempfile::TempDir::new().unwrap();
    assert!(!antigravity_wal_allows_cache(fs::metadata(
        directory.path()
    )));
}
