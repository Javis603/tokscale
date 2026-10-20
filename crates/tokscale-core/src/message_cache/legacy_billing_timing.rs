// Frozen release layouts. Upstream billing and downstream timing reused the
// same envelope generations with incompatible positional payloads. Decode both
// exactly and reject ambiguous payloads; never guess using the current type.
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TimingMessage {
    client: String,
    model_id: String,
    provider_id: String,
    session_id: String,
    workspace_key: Option<String>,
    workspace_label: Option<String>,
    timestamp: i64,
    date: String,
    tokens: crate::TokenBreakdown,
    cost: f64,
    cost_source: crate::sessions::CostSource,
    service_tier: Option<String>,
    duration_ms: Option<i64>,
    performance_timing: Option<crate::sessions::PerformanceTiming>,
    message_count: i32,
    agent: Option<String>,
    dedup_key: Option<String>,
    session_title: Option<String>,
    parent_session_id: Option<String>,
    is_turn_start: bool,
    model_attribution_conflicted: bool,
}

impl From<TimingMessage> for UnifiedMessage {
    fn from(message: TimingMessage) -> Self {
        Self {
            client: message.client,
            model_id: message.model_id,
            provider_id: message.provider_id,
            session_id: message.session_id,
            workspace_key: message.workspace_key,
            workspace_label: message.workspace_label,
            timestamp: message.timestamp,
            date: message.date,
            tokens: message.tokens,
            cost: message.cost,
            cost_source: message.cost_source,
            service_tier: message.service_tier,
            duration_ms: message.duration_ms,
            performance_timing: message.performance_timing,
            message_count: message.message_count,
            agent: message.agent,
            dedup_key: message.dedup_key,
            session_title: message.session_title,
            parent_session_id: message.parent_session_id,
            is_turn_start: message.is_turn_start,
            model_attribution_conflicted: message.model_attribution_conflicted,
            billing_identity: crate::sessions::BillingIdentity::Unknown,
            pricing_model_id: None,
            estimate_status: crate::sessions::EstimateStatus::Unpriced,
            copilot_context: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TimingEntry {
    parser_namespace: String,
    parser_version: u32,
    path: CachedPath,
    fingerprint: SourceFingerprint,
    messages: Vec<TimingMessage>,
    fallback_timestamp_indices: Vec<usize>,
    codex_incremental: Option<LegacyCodexIncrementalV15>,
    prime_accounting: Option<crate::sessions::prime_agent::PrimeFileAccounting>,
    opencode_incremental: Option<crate::sessions::opencode_schema::OpenCodeIncrementalState>,
}

impl From<TimingEntry> for CachedSourceEntry {
    fn from(entry: TimingEntry) -> Self {
        Self {
            parser_namespace: entry.parser_namespace,
            parser_version: entry.parser_version,
            path: entry.path,
            fingerprint: entry.fingerprint,
            messages: entry.messages.into_iter().map(Into::into).collect(),
            fallback_timestamp_indices: entry.fallback_timestamp_indices,
            codex_incremental: entry.codex_incremental.map(Into::into),
            prime_accounting: entry.prime_accounting,
            opencode_incremental: entry.opencode_incremental,
            micode_metadata: None,
            claude_checkpoint: None,
            billing_identity_generation: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BillingMessage {
    client: String,
    model_id: String,
    provider_id: String,
    session_id: String,
    workspace_key: Option<String>,
    workspace_label: Option<String>,
    timestamp: i64,
    date: String,
    tokens: crate::TokenBreakdown,
    cost: f64,
    cost_source: crate::sessions::CostSource,
    service_tier: Option<String>,
    duration_ms: Option<i64>,
    message_count: i32,
    agent: Option<String>,
    dedup_key: Option<String>,
    session_title: Option<String>,
    parent_session_id: Option<String>,
    is_turn_start: bool,
    model_attribution_conflicted: bool,
    billing_identity: crate::sessions::BillingIdentity,
    pricing_model_id: Option<String>,
    estimate_status: crate::sessions::EstimateStatus,
}

impl From<BillingMessage> for UnifiedMessage {
    fn from(message: BillingMessage) -> Self {
        Self {
            client: message.client,
            model_id: message.model_id,
            provider_id: message.provider_id,
            session_id: message.session_id,
            workspace_key: message.workspace_key,
            workspace_label: message.workspace_label,
            timestamp: message.timestamp,
            date: message.date,
            tokens: message.tokens,
            cost: message.cost,
            cost_source: message.cost_source,
            service_tier: message.service_tier,
            duration_ms: message.duration_ms,
            performance_timing: None,
            message_count: message.message_count,
            agent: message.agent,
            dedup_key: message.dedup_key,
            session_title: message.session_title,
            parent_session_id: message.parent_session_id,
            is_turn_start: message.is_turn_start,
            model_attribution_conflicted: message.model_attribution_conflicted,
            billing_identity: message.billing_identity,
            pricing_model_id: message.pricing_model_id,
            estimate_status: message.estimate_status,
            copilot_context: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct BillingEntry {
    parser_namespace: String,
    parser_version: u32,
    path: CachedPath,
    fingerprint: SourceFingerprint,
    messages: Vec<BillingMessage>,
    fallback_timestamp_indices: Vec<usize>,
    codex_incremental: Option<LegacyCodexIncrementalV15>,
    prime_accounting: Option<crate::sessions::prime_agent::PrimeFileAccounting>,
    opencode_incremental: Option<crate::sessions::opencode_schema::OpenCodeIncrementalState>,
    billing_identity_generation: u32,
}

impl From<BillingEntry> for CachedSourceEntry {
    fn from(entry: BillingEntry) -> Self {
        Self {
            parser_namespace: entry.parser_namespace,
            parser_version: entry.parser_version,
            path: entry.path,
            fingerprint: entry.fingerprint,
            messages: entry.messages.into_iter().map(Into::into).collect(),
            fallback_timestamp_indices: entry.fallback_timestamp_indices,
            codex_incremental: entry.codex_incremental.map(Into::into),
            prime_accounting: entry.prime_accounting,
            opencode_incremental: entry.opencode_incremental,
            micode_metadata: None,
            claude_checkpoint: None,
            billing_identity_generation: entry.billing_identity_generation,
        }
    }
}

fn decode<T: serde::de::DeserializeOwned>(payload: &[u8], limit: u64) -> bincode::Result<T> {
    bincode::options().with_limit(limit).deserialize(payload)
}

fn choose(
    timing: bincode::Result<Vec<CachedSourceEntry>>,
    billing: bincode::Result<Vec<CachedSourceEntry>>,
) -> ShardReadStatus {
    match (timing, billing) {
        (Ok(a), Ok(b)) => {
            if bincode::options().serialize(&a).ok() == bincode::options().serialize(&b).ok() {
                ShardReadStatus::Migrated(a)
            } else {
                ShardReadStatus::Invalid("ambiguous legacy billing/timing cache layout".into())
            }
        }
        (Ok(entries), Err(_)) | (Err(_), Ok(entries)) => ShardReadStatus::Migrated(entries),
        (Err(error), Err(_)) => ShardReadStatus::Invalid(error.to_string()),
    }
}

pub(super) fn read(
    envelope: &CachedShardEnvelope,
    identity: CacheIdentity,
    limit: u64,
) -> Option<ShardReadStatus> {
    let payload = &envelope.payload;
    if identity.namespace == ClientId::Copilot.as_str() && envelope.format_version == 13 {
        // Copilot has no retained-history lane. Both old layouts must reparse.
        return Some(ShardReadStatus::Stale);
    }
    if identity.namespace == ClientId::Claude.as_str() && envelope.format_version == 13 {
        type TimingWire = (TimingEntry, Option<LegacyClaudeCheckpointV12>);
        type BillingWire = (
            BillingEntry,
            Option<crate::sessions::claudecode::ClaudeCheckpoint>,
        );
        // The message digest changed with performance/billing fields. Preserve
        // every cached row, but rebuild append checkpoints from the source.
        return Some(choose(
            decode::<Vec<TimingWire>>(payload, limit)
                .map(|rows| rows.into_iter().map(|(entry, _)| entry.into()).collect()),
            decode::<Vec<BillingWire>>(payload, limit)
                .map(|rows| rows.into_iter().map(|(entry, _)| entry.into()).collect()),
        ));
    }
    if identity.namespace == ClientId::MiMoCode.as_str() && envelope.format_version == 12 {
        type Metadata = Option<Vec<crate::sessions::micode::MiMoRowMetadata>>;
        fn restore<E: Into<CachedSourceEntry>>(rows: Vec<(E, Metadata)>) -> Vec<CachedSourceEntry> {
            rows.into_iter()
                .map(|(entry, metadata)| {
                    let mut entry = entry.into();
                    entry.micode_metadata = metadata;
                    if !entry.has_valid_micode_metadata() {
                        entry.micode_metadata = None;
                    }
                    entry
                })
                .collect()
        }
        return Some(choose(
            decode::<Vec<(TimingEntry, Metadata)>>(payload, limit).map(restore),
            decode::<Vec<(BillingEntry, Metadata)>>(payload, limit).map(restore),
        ));
    }
    if envelope.format_version == 11 && identity.namespace != ClientId::MiMoCode.as_str() {
        return Some(choose(
            decode::<Vec<TimingEntry>>(payload, limit)
                .map(|rows| rows.into_iter().map(Into::into).collect()),
            decode::<Vec<BillingEntry>>(payload, limit)
                .map(|rows| rows.into_iter().map(Into::into).collect()),
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_released_billing_and_timing_shards_preserve_messages_and_rewrite_warm() {
        let fixtures: [(&[u8], ClientId, u32); 6] = [
            (
                include_bytes!("../../tests/fixtures/billing-timing/timing-claude.bin"),
                ClientId::Claude,
                0,
            ),
            (
                include_bytes!("../../tests/fixtures/billing-timing/billing-claude.bin"),
                ClientId::Claude,
                1,
            ),
            (
                include_bytes!("../../tests/fixtures/billing-timing/timing-amp.bin"),
                ClientId::Amp,
                0,
            ),
            (
                include_bytes!("../../tests/fixtures/billing-timing/billing-amp.bin"),
                ClientId::Amp,
                1,
            ),
            (
                include_bytes!("../../tests/fixtures/billing-timing/timing-micode.bin"),
                ClientId::MiMoCode,
                0,
            ),
            (
                include_bytes!("../../tests/fixtures/billing-timing/billing-micode.bin"),
                ClientId::MiMoCode,
                1,
            ),
        ];
        for (bytes, client, generation) in fixtures {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("legacy.bin");
            std::fs::write(&path, bytes).unwrap();
            let identity = CacheIdentity::for_client(client);
            let status = read_shard(&path, identity);
            let ShardReadStatus::Migrated(entries) = status else {
                panic!("{} generation {generation} must migrate", client.as_str());
            };
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].messages.len(), 1);
            assert_eq!(entries[0].messages[0].tokens.input, 100);
            assert_eq!(entries[0].messages[0].tokens.output, 20);
            assert_eq!(entries[0].billing_identity_generation, generation);
            assert!(entries[0].claude_checkpoint.is_none());
            if client == ClientId::MiMoCode {
                assert!(entries[0].has_valid_micode_metadata());
                assert_eq!(entries[0].messages[0].cost, 0.75);
            }
            assert_eq!(
                std::fs::read(&path).unwrap(),
                bytes,
                "migration reads must not rewrite source bytes"
            );
            write_shard_with_limit(&path, identity, &entries, MAX_CACHE_SHARD_BYTES).unwrap();
            let ShardReadStatus::Loaded(warm) = read_shard(&path, identity) else {
                panic!("combined cache must reload warm");
            };
            assert_eq!(warm[0].messages, entries[0].messages);
            assert_eq!(warm[0].billing_identity_generation, generation);
            assert_eq!(warm[0].micode_metadata, entries[0].micode_metadata);
        }
    }
}
