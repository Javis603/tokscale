//! Opt-in source accounting wire contract. Raw owners never namespace events.
use crate::{sessions::UnifiedMessage, TokenBreakdown};
use std::collections::BTreeSet;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceUsage {
    pub version: u32,
    pub event_identity_version: u32,
    pub captured_at: String,
    /// Date-selection coverage only; never read/identity authority.
    pub full_history: bool,
    pub bucket_timezone: String,
    pub day_key: String,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub scan_roots: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub root_usage: Vec<SourceRootUsage>,
    pub replacement_clients: Vec<String>,
    pub partial_clients: Vec<String>,
    pub scopes: Vec<SourceScopeUsage>,
    pub sessions: Vec<SourceSessionUsage>,
    pub rows_without_identity: std::collections::BTreeMap<String, u64>,
}

/// Local scanner diagnostics only: never include roots in retained event proof.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceRootUsage {
    pub root: String,
    pub client: String,
    pub tokens: i64,
    /// Conservative whole-Claude read health; any Claude failure marks every root partial.
    pub read_status: &'static str,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceScopeUsage {
    pub id: String,
    pub clients: Vec<String>,
    pub read_status: &'static str,
    pub identity_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSessionUsage {
    pub client: String,
    pub session_id: String,
    pub scope_id: String,
    pub events: Vec<SourceEventUsage>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceEventUsage {
    pub id: Option<String>,
    pub observation_kind: &'static str,
    pub contributions: Vec<SourceContribution>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceContribution {
    pub timestamp: i64,
    pub model_id: String,
    pub provider_id: String,
    pub tokens: SourceTokens,
    pub cost: f64,
    pub messages: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pricing: Option<SourcePricing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub performance: Option<SourcePerformance>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcePricing {
    pub unpriced_tokens: i64,
    pub unpriced_messages: i32,
}

/// Additive samples use the native total (output and reasoning are disjoint).
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcePerformance {
    pub total_duration_ms: i64,
    pub timed_tokens: i64,
    pub timed_output_tokens: i64,
    pub sample_count: i32,
}

/// cacheWrite includes the 1-hour subset, as in normal graph token totals.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceTokens {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cache_write_1h: i64,
    pub reasoning: i64,
}

impl SourceContribution {
    pub(crate) fn from_message(message: &UnifiedMessage) -> Self {
        Self {
            timestamp: message.timestamp,
            model_id: crate::canonical_model_id(&message.model_id),
            provider_id: message.provider_id.clone(),
            tokens: SourceTokens::default(),
            cost: 0.0,
            messages: 0,
            pricing: Some(SourcePricing::default()),
            performance: None,
        }
    }
    pub(crate) fn merge(&mut self, other: Self) {
        self.tokens.input = self.tokens.input.saturating_add(other.tokens.input);
        self.tokens.output = self.tokens.output.saturating_add(other.tokens.output);
        self.tokens.cache_read = self
            .tokens
            .cache_read
            .saturating_add(other.tokens.cache_read);
        self.tokens.cache_write = self
            .tokens
            .cache_write
            .saturating_add(other.tokens.cache_write);
        self.tokens.reasoning = self.tokens.reasoning.saturating_add(other.tokens.reasoning);
        self.tokens.cache_write_1h = self
            .tokens
            .cache_write_1h
            .saturating_add(other.tokens.cache_write_1h);
        if let Some(other) = other.pricing {
            let pricing = self.pricing.get_or_insert_with(SourcePricing::default);
            pricing.unpriced_tokens = pricing
                .unpriced_tokens
                .saturating_add(other.unpriced_tokens);
            pricing.unpriced_messages = pricing
                .unpriced_messages
                .saturating_add(other.unpriced_messages);
        }
        if let Some(other) = other.performance {
            self.add_performance(other);
        }
        self.cost += other.cost;
        self.messages = self.messages.saturating_add(other.messages);
    }
    fn add_performance(&mut self, other: SourcePerformance) {
        let performance = self
            .performance
            .get_or_insert_with(SourcePerformance::default);
        performance.total_duration_ms = performance
            .total_duration_ms
            .saturating_add(other.total_duration_ms);
        performance.timed_tokens = performance.timed_tokens.saturating_add(other.timed_tokens);
        performance.timed_output_tokens = performance
            .timed_output_tokens
            .saturating_add(other.timed_output_tokens);
        performance.sample_count = performance.sample_count.saturating_add(other.sample_count);
    }
    pub(crate) fn add(
        &mut self,
        message: &UnifiedMessage,
        pricing: Option<&crate::pricing::PricingService>,
    ) {
        let TokenBreakdown {
            input,
            output,
            cache_read,
            cache_write,
            reasoning,
            ..
        } = message.tokens;
        self.tokens.input = self.tokens.input.saturating_add(input);
        self.tokens.output = self.tokens.output.saturating_add(output);
        self.tokens.cache_read = self.tokens.cache_read.saturating_add(cache_read);
        self.tokens.cache_write = self.tokens.cache_write.saturating_add(cache_write);
        self.tokens.reasoning = self.tokens.reasoning.saturating_add(reasoning);
        self.tokens.cache_write_1h = self
            .tokens
            .cache_write_1h
            .saturating_add(message.tokens.cache_write_1h);
        let total = crate::positive_token_total(&message.tokens);
        if crate::message_has_unpriced_usage(message, pricing) {
            let pricing = self.pricing.get_or_insert_with(SourcePricing::default);
            pricing.unpriced_tokens = pricing.unpriced_tokens.saturating_add(total);
            pricing.unpriced_messages = pricing
                .unpriced_messages
                .saturating_add(message.message_count.max(0));
        }
        if let Some(duration) = message
            .duration_ms
            .filter(|duration| *duration > 0 && total > 0)
        {
            self.add_performance(SourcePerformance {
                total_duration_ms: duration,
                timed_tokens: total,
                timed_output_tokens: message
                    .tokens
                    .output
                    .max(0)
                    .saturating_add(message.tokens.reasoning.max(0)),
                sample_count: 1,
            });
        }
        self.cost += message.cost;
        self.messages = self.messages.saturating_add(message.message_count.max(0));
    }
}

pub(crate) fn source_dedup_domain(client: &str) -> &str {
    let client = crate::ClientId::from_str(client).map_or(client, |id| id.as_str());
    match client {
        "micode" | "micode-desktop" => "micode",
        "antigravity" | "antigravity-cli" | "antigravity-extension" => "antigravity",
        client if client.eq_ignore_ascii_case("9router") => "gjc",
        client if client.starts_with("cc-mirror/") => "claude",
        client => client,
    }
}

fn digest(namespace: &[u8], domain: &str, key: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(namespace);
    hash.update((domain.len() as u64).to_be_bytes());
    hash.update(domain.as_bytes());
    hash.update(key);
    format!("sha256:{:x}", hash.finalize())
}

pub(crate) fn source_scope_id(client: &str) -> String {
    digest(
        b"tokscale-source-scope-v2\0",
        source_dedup_domain(client),
        b"",
    )
}

pub(crate) fn source_event_id(message: &UnifiedMessage) -> Option<String> {
    if message.client == "codex" {
        return message
            .source_group_key
            .as_ref()
            .map(|key| digest(b"tokscale-source-event-v2\0", "codex", key.as_bytes()));
    }
    let key = message
        .dedup_key
        .as_deref()
        .filter(|key| !key.trim().is_empty())?;
    Some(digest(
        b"tokscale-source-event-v2\0",
        source_dedup_domain(&message.client),
        key.as_bytes(),
    ))
}

pub(crate) fn has_session_identity(client: &str, id: &str) -> bool {
    !id.trim().is_empty() && id != "unknown" && !(client == "copilot" && id == "unknown-session")
}

pub(crate) struct SourceReadEvidence {
    pub domain: String,
    pub complete: bool,
    pub identity_exact: bool,
    pub sessions: Vec<(String, String)>,
    pub observation_kinds: Vec<(String, &'static str)>,
    pub null_observation_kinds: Vec<((String, String), &'static str)>,
    pub root_samples: std::collections::BTreeMap<(String, String), i64>,
    pub scan_roots: std::collections::BTreeMap<String, Vec<String>>,
}
impl SourceReadEvidence {
    pub(crate) fn new(domain: &str, complete: bool, identity_exact: bool) -> Self {
        Self {
            domain: domain.into(),
            complete,
            identity_exact,
            sessions: Vec::new(),
            observation_kinds: Vec::new(),
            null_observation_kinds: Vec::new(),
            root_samples: Default::default(),
            scan_roots: Default::default(),
        }
    }
}

#[derive(Default)]
pub(crate) struct SourceScopeFold {
    pub clients: BTreeSet<String>,
    pub unresolved_rows: bool,
    evidence: Option<(bool, bool)>,
}
impl SourceScopeFold {
    pub(crate) fn observe(&mut self, evidence: &SourceReadEvidence) {
        let prior = self.evidence.unwrap_or((true, true));
        self.evidence = Some((
            prior.0 && evidence.complete,
            prior.1 && evidence.identity_exact,
        ));
    }
    pub(crate) fn finish(mut self, domain: &str, selected: &[String]) -> SourceScopeUsage {
        // Even an empty scan requires both surfaces for a shared-store claim.
        match domain {
            "micode" => {
                self.clients
                    .extend(["micode".into(), "micode-desktop".into()]);
            }
            "antigravity" => {
                self.clients.extend([
                    "antigravity".into(),
                    "antigravity-cli".into(),
                    "antigravity-extension".into(),
                ]);
            }
            "gjc" => {
                self.clients.extend(["gjc".into(), "9router".into()]);
            }
            _ => {}
        }
        let mut scope = SourceScopeUsage {
            id: source_scope_id(domain),
            clients: self.clients.into_iter().collect(),
            read_status: match self.evidence {
                Some((true, _)) => "complete",
                Some((false, _)) => "partial",
                None => "unknown",
            },
            identity_status: if self.evidence.is_some_and(|(_, exact)| exact)
                && !self.unresolved_rows
            {
                "exact"
            } else {
                "unresolved"
            },
            reason: None,
        };
        if !scope_has_selection_coverage(&scope, selected) {
            scope.read_status = "partial";
            scope.reason = Some("client selection does not cover the scanner dedup domain");
        } else if scope.read_status == "unknown" {
            scope.reason = Some("parser read health is not instrumented");
        } else if scope.read_status == "partial" {
            scope.reason = Some("source discovery, parse or stable-read check failed");
        } else if scope.identity_status == "unresolved" {
            scope.reason =
                Some("scanner keys or equivalence aliases do not prove stable event identity");
        }
        scope
    }
}

pub(crate) fn scope_has_selection_coverage(scope: &SourceScopeUsage, selected: &[String]) -> bool {
    scope
        .clients
        .iter()
        .all(|client| crate::source_client_has_replacement_scope(client, selected))
}

/// Explicit upstream session + turn, length delimited; no ordinal fallback.
pub(crate) fn codex_group_key(session: &str, turn: &str) -> String {
    digest(
        b"tokscale-codex-source-group-v2\0",
        session,
        turn.as_bytes(),
    )
}
