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
//! A client here is requested only by name. It is never part of an unfiltered
//! scan, so plain `tokscale` output is identical to upstream's.

mod js;
mod proma;
mod qodercn;

use crate::sessions::UnifiedMessage;

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

/// Token Monitor-owned client ids.
pub fn client_ids() -> impl Iterator<Item = &'static str> {
    CLIENTS.iter().map(|client| client.id)
}

fn requested<'a>(clients: &'a [String]) -> impl Iterator<Item = &'static Client> + 'a {
    CLIENTS
        .iter()
        .filter(move |owned| clients.iter().any(|client| client == owned.id))
}

/// Unpriced messages for every requested Token Monitor client.
pub fn requested_messages(home_dir: &str, clients: &[String]) -> Vec<UnifiedMessage> {
    requested(clients)
        .flat_map(|client| (client.parse)(home_dir))
        .collect()
}

/// Streaming-lane hook: parse, price and append the requested clients.
pub(crate) fn extend_requested(
    home_dir: &str,
    clients: &[String],
    pricing: Option<&crate::pricing::PricingService>,
    all_messages: &mut Vec<UnifiedMessage>,
) {
    for mut message in requested_messages(home_dir, clients) {
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
}
