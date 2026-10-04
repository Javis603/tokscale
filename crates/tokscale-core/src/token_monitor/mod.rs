//! Token Monitor-owned clients.
//!
//! Clients that only Token Monitor needs live here instead of in `ClientId`,
//! so carrying them across upstream syncs never touches the contiguous client
//! index, the CLI `ClientFilter` enum, or the per-client lanes in `lib.rs`.
//! Upstream files reach this module through these hooks:
//!
//! - `lib.rs`: `pub mod token_monitor;`
//! - `lib.rs` streaming parse: `extend_requested` before the synthetic lane
//! - `lib.rs` `parse_local_clients`: [`requested_messages`] before the synthetic lane
//! - `tokscale-cli` `main.rs`: `mod token_monitor;`, `token_monitor::parse_cli`
//!   in place of `Cli::parse`, and `token_monitor::merge_client_filter` where
//!   the client filter is built (see `tokscale-cli/src/token_monitor.rs`)
//!
//! An owned client is requested only by name. It is never part of an
//! unfiltered scan, so it never appears in plain `tokscale` output.
//!
//! A supplement adds a second source to an upstream client under that
//! client's own id, for data upstream does not read yet; it is dropped once
//! upstream reads that data itself. The CLI never strips its id, so the
//! upstream lane runs as before, and the supplement runs wherever that lane
//! does, filtered or not, so the id means the same data in every scan.

mod js;
mod mcode;
mod proma;
mod qodercn;

use crate::sessions::UnifiedMessage;
use crate::{ClientCounts, ClientId};

/// A Token Monitor-owned client: its `--client` id and its parser.
struct Client {
    id: &'static str,
    parse: fn(&str) -> Vec<UnifiedMessage>,
}

/// Every Token Monitor-owned client, in the order they are parsed. Adding a
/// client is one entry here plus its module; no upstream file changes.
const CLIENTS: &[Client] = &[
    Client {
        id: proma::CLIENT_ID,
        parse: proma::parse,
    },
    Client {
        id: qodercn::CLIENT_ID,
        parse: qodercn::parse,
    },
];

/// What a supplement sees of the scan it extends: the home, the env-root
/// strategy, and the files the upstream lanes found, so it can tell what they
/// already count without scanning again.
pub struct Scope<'a> {
    pub home_dir: &'a str,
    pub use_env_roots: bool,
    pub scan: &'a crate::scanner::ScanResult,
}

/// A source added to an upstream client: the upstream id it reports under and
/// its parser.
struct Supplement {
    id: &'static str,
    parse: fn(&Scope) -> Vec<UnifiedMessage>,
}

const SUPPLEMENTS: &[Supplement] = &[Supplement {
    id: mcode::CLIENT_ID,
    parse: mcode::parse,
}];

/// Token Monitor-owned client ids. Supplement ids are upstream ids, so they
/// are not listed here.
pub fn client_ids() -> impl Iterator<Item = &'static str> {
    CLIENTS.iter().map(|client| client.id)
}

fn requested<'a>(clients: &'a [String]) -> impl Iterator<Item = &'static Client> + 'a {
    CLIENTS
        .iter()
        .filter(move |owned| clients.iter().any(|client| client == owned.id))
}

/// An empty filter scans every upstream client, so it runs every supplement.
fn requested_supplements<'a>(
    clients: &'a [String],
) -> impl Iterator<Item = &'static Supplement> + 'a {
    SUPPLEMENTS.iter().filter(move |supplement| {
        clients.is_empty() || clients.iter().any(|client| client == supplement.id)
    })
}

fn parse_requested(clients: &[String], scope: &Scope) -> Vec<UnifiedMessage> {
    requested(clients)
        .flat_map(|client| (client.parse)(scope.home_dir))
        .chain(requested_supplements(clients).flat_map(|supplement| (supplement.parse)(scope)))
        .collect()
}

/// Local-lane hook: unpriced messages for every requested Token Monitor client
/// and supplement. Supplement messages are added to their upstream client's
/// count.
pub fn requested_messages(
    clients: &[String],
    scope: &Scope,
    counts: &mut ClientCounts,
) -> Vec<UnifiedMessage> {
    let messages = parse_requested(clients, scope);
    for message in &messages {
        if let Some(client) = ClientId::from_str(&message.client) {
            counts.add(client, message.message_count.max(0));
        }
    }
    messages
}

/// Streaming-lane hook: parse, price and append the requested clients and
/// supplements.
pub(crate) fn extend_requested(
    clients: &[String],
    scope: &Scope,
    pricing: Option<&crate::pricing::PricingService>,
    all_messages: &mut Vec<UnifiedMessage>,
) {
    for mut message in parse_requested(clients, scope) {
        message.refresh_derived_fields();
        crate::apply_pricing_if_available(&mut message, pricing);
        all_messages.push(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_ignores_unrequested_ids() {
        assert_eq!(requested(&["claude".to_string()]).count(), 0);
        assert_eq!(
            requested(&["proma".to_string()])
                .map(|client| client.id)
                .collect::<Vec<_>>(),
            vec!["proma"]
        );
    }

    #[test]
    fn supplements_are_requested_by_their_upstream_id_but_never_stripped() {
        assert_eq!(
            requested_supplements(&["mcode".to_string()])
                .map(|supplement| supplement.id)
                .collect::<Vec<_>>(),
            vec!["mcode"]
        );
        assert_eq!(requested_supplements(&["claude".to_string()]).count(), 0);
        assert_eq!(requested_supplements(&[]).count(), SUPPLEMENTS.len());
        assert!(client_ids().all(|id| id != "mcode"));
        assert!(SUPPLEMENTS
            .iter()
            .all(|supplement| ClientId::from_str(supplement.id).is_some()));
    }
}
