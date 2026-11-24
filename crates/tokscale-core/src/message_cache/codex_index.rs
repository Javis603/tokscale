//! An optional, checksummed index after the v2 Codex envelope. The envelope
//! and entry bytes are unchanged, so older readers can still use the shard.
//! Offsets and summaries are published atomically with the payload they index.
use super::*;
use crate::sessions::codex::CodexTurnCoverage;
use crate::SharedStr;
use std::io::BufRead;

const MAGIC: [u8; 8] = *b"TCXIDX03";
const FOOTER_SIZE: u64 = 8 + 32 + MAGIC.len() as u64;

#[derive(Serialize, Deserialize)]
pub(super) struct IndexedEntry {
    pub(super) path: CachedPath,
    pub(super) fingerprint: SourceFingerprint,
    offset: u64,
    len: u64,
    pub(super) summary: Option<CodexHistory>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct CodexHistory {
    pub(crate) coverage: CodexTurnCoverage,
    pub(crate) missing_usage: (bool, bool),
    pub(crate) identities: Vec<CodexRowIdentity>,
    pub(crate) rows: Vec<CodexHistoryRow>,
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct CodexRowIdentity {
    pub(crate) client: SharedStr,
    pub(crate) model: SharedStr,
    pub(crate) provider: SharedStr,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct CodexHistoryRow {
    pub(crate) key: Option<String>,
    pub(crate) identity: usize,
    // None means the source's current mtime, as in finalize_codex_messages.
    pub(crate) timestamp: Option<i64>,
}

impl CodexHistory {
    fn from_entry(entry: &CachedSourceEntry) -> Option<Self> {
        let incremental = entry.codex_incremental.as_ref()?;
        if entry.billing_identity_generation == 0
            || !codex_cache_entry_matches_fingerprint(entry, &entry.fingerprint)
            || entry.messages.iter().any(|message| {
                message.client == crate::sessions::codex::OPENCLAW_CLIENT_ID
                    || crate::sessions::magpie::codex_response_id(message).is_some()
            })
        {
            return None;
        }
        let fallback: HashSet<_> = entry.fallback_timestamp_indices.iter().copied().collect();
        let mut identities = Vec::new();
        let mut identity_indices = HashMap::new();
        let rows = entry
            .messages
            .iter()
            .enumerate()
            .map(|(index, message)| {
                let identity = CodexRowIdentity {
                    client: message.client.clone(),
                    model: message.model_id.clone(),
                    provider: message.provider_id.clone(),
                };
                let identity = *identity_indices.entry(identity.clone()).or_insert_with(|| {
                    identities.push(identity);
                    identities.len() - 1
                });
                CodexHistoryRow {
                    key: message.dedup_key.clone(),
                    identity,
                    timestamp: (!fallback.contains(&index)).then_some(message.timestamp),
                }
            })
            .collect();
        Some(Self {
            coverage: incremental.state.turn_coverage.clone(),
            missing_usage: (
                incremental.state.is_missing_usage(),
                incremental.state.has_own_synthetic_activity,
            ),
            identities,
            rows,
        })
    }
}

impl IndexedEntry {
    pub(super) fn read_payload(&self, shard: &Path) -> Option<CachedSourceEntry> {
        let mut reader = File::open(shard).ok()?;
        reader.seek(SeekFrom::Start(self.offset)).ok()?;
        let mut bytes = vec![0; usize::try_from(self.len).ok()?];
        reader.read_exact(&mut bytes).ok()?;
        let entry: CachedSourceEntry = bincode::options().deserialize(&bytes).ok()?;
        (entry.path == self.path
            && entry.fingerprint == self.fingerprint
            && entry.matches_identity(CacheIdentity::for_client(ClientId::Codex)))
        .then_some(entry)
    }
}

pub(super) fn can_upgrade(path: &Path, entries: &[CachedSourceEntry]) -> bool {
    // The index repeats only a subset of the fields in the envelope (keys,
    // timestamps, identities, turn coverage and source metadata), plus a
    // small byte range per entry. Keep large legacy shards on the compatible
    // decoder if there is insufficient room to upgrade them safely.
    path.metadata().is_ok_and(|metadata| {
        metadata.len() * 2 + entries.len() as u64 * 24 + FOOTER_SIZE <= MAX_CACHE_SHARD_BYTES
    })
}

pub(super) fn encode(
    envelope: &CachedShardEnvelope,
    entries: &[CachedSourceEntry],
    limit: u64,
) -> Option<Vec<u8>> {
    // Bincode encodes a Vec as a length followed by consecutive elements. The
    // first length here is the envelope's byte vector, the second the nested
    // vector of entries. This is the same layout the v2 decoder reads.
    let start = bincode::options()
        .serialized_size(&(
            envelope.format_version,
            &envelope.parser_namespace,
            envelope.parser_version,
            envelope.payload.len() as u64,
            entries.len() as u64,
        ))
        .ok()?;
    let mut offset = start;
    let index: Vec<_> = entries
        .iter()
        .map(|entry| {
            let len = bincode::options().serialized_size(entry).ok()?;
            let record = IndexedEntry {
                path: entry.path.clone(),
                fingerprint: entry.fingerprint.clone(),
                offset,
                len,
                summary: CodexHistory::from_entry(entry),
            };
            offset += len;
            Some(record)
        })
        .collect::<Option<_>>()?;
    if offset != bincode::options().serialized_size(envelope).ok()? {
        return None;
    }
    let mut bytes = bincode::options()
        .with_limit(limit.checked_sub(offset + FOOTER_SIZE)?)
        .serialize(&index)
        .ok()?;
    let len = bytes.len() as u64;
    let mut digest = Sha256::new();
    digest.update(&envelope.payload);
    digest.update(&bytes);
    let digest = digest.finalize();
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&digest);
    bytes.extend_from_slice(&MAGIC);
    Some(bytes)
}

pub(super) fn read(
    path: &Path,
    identity: CacheIdentity,
    shard: &CacheShardKey,
) -> Option<Vec<IndexedEntry>> {
    let mut reader = BufReader::new(File::open(path).ok()?);
    let size = reader.get_ref().metadata().ok()?.len();
    if !(FOOTER_SIZE..=MAX_CACHE_SHARD_BYTES).contains(&size) {
        return None;
    }
    reader.seek(SeekFrom::Start(size - FOOTER_SIZE)).ok()?;
    let mut footer = [0; FOOTER_SIZE as usize];
    reader.read_exact(&mut footer).ok()?;
    if footer[40..] != MAGIC {
        return None;
    }
    let len = u64::from_le_bytes(footer[..8].try_into().ok()?);
    let offset = size.checked_sub(FOOTER_SIZE)?.checked_sub(len)?;
    reader.seek(SeekFrom::Start(0)).ok()?;
    let (format, namespace, parser, payload_len): (u32, String, u32, u64) = bincode::options()
        .with_limit(offset)
        .deserialize_from(&mut reader)
        .ok()?;
    let payload_start = reader.stream_position().ok()?;
    if format != CACHE_FORMAT_VERSION
        || namespace != identity.namespace
        || parser != identity.parser_version
        || payload_len.checked_add(payload_start)? != offset
    {
        return None;
    }
    let count: u64 = bincode::options()
        .with_limit(payload_len)
        .deserialize_from(&mut reader)
        .ok()?;
    let mut expected_offset = reader.stream_position().ok()?;
    // Verify the payload without deserializing the old message rows.
    reader.seek(SeekFrom::Start(payload_start)).ok()?;
    let mut digest = Sha256::new();
    {
        let mut payload = (&mut reader).take(payload_len);
        while let chunk @ [_, ..] = payload.fill_buf().ok()? {
            let count = chunk.len();
            digest.update(chunk);
            payload.consume(count);
        }
    }
    let mut bytes = vec![0; len as usize];
    reader.read_exact(&mut bytes).ok()?;
    digest.update(&bytes);
    if digest.finalize()[..] != footer[8..40] {
        return None;
    }
    let entries: Vec<IndexedEntry> = bincode::options()
        .with_limit(len)
        .deserialize(&bytes)
        .ok()?;
    if entries.len() as u64 != count {
        return None;
    }
    let mut paths = HashSet::with_capacity(entries.len());
    for entry in &entries {
        let key = CacheKey::new(identity, &entry.path.to_path_buf());
        if key.shard() != *shard
            || !paths.insert(key)
            || entry.offset != expected_offset
            || entry.len == 0
            || entry.summary.as_ref().is_some_and(|summary| {
                summary
                    .rows
                    .iter()
                    .any(|row| row.identity >= summary.identities.len())
            })
        {
            return None;
        }
        expected_offset = expected_offset.checked_add(entry.len)?;
    }
    (expected_offset == offset).then_some(entries)
}
