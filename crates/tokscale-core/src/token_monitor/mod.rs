//! Token Monitor-owned clients.
//!
//! Clients that only Token Monitor needs live here instead of in `ClientId`,
//! so carrying them across upstream syncs never touches the contiguous client
//! index, the CLI `ClientFilter` enum, or the per-client lanes in `lib.rs`.
//! Upstream files reach this module through exactly four hooks:
//!
//! - `lib.rs`: `pub mod token_monitor;`
//! - `lib.rs` streaming parse: `extend_requested` before the synthetic lane
//! - `lib.rs` `parse_local_clients`: [`requested_messages`] before the synthetic lane
//! - `tokscale-cli` `main`: [`split_cli_clients`] before clap parses argv, and
//!   [`merge_client_filter`] where the client filter is built
//!
//! A client here is requested only by name. It is never part of an unfiltered
//! scan, so plain `tokscale` output is identical to upstream's.

mod proma;
mod qodercn;

use crate::sessions::UnifiedMessage;
use std::ffi::OsString;
use std::sync::OnceLock;

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

static CLI_CLIENTS: OnceLock<Vec<String>> = OnceLock::new();

/// Removes Token Monitor client ids from `--client`/`-c` values before clap
/// sees them (clap's `ClientFilter` enum would reject them) and remembers them
/// for [`merge_client_filter`]. A flag left with no values is dropped whole.
pub fn split_cli_clients<I>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    let mut out = Vec::new();
    let mut taken = Vec::new();
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy().into_owned();
        let (flag, inline) = match text.split_once('=') {
            Some((flag, value)) if flag == "--client" => {
                (flag.to_string(), Some(value.to_string()))
            }
            _ => (text.clone(), None),
        };
        if flag != "--client" && flag != "-c" {
            out.push(arg);
            continue;
        }
        let value = match inline {
            Some(value) => value,
            None => match iter.next() {
                Some(next) => next.to_string_lossy().into_owned(),
                None => {
                    out.push(arg);
                    continue;
                }
            },
        };
        let mut kept = Vec::new();
        for part in value.split(',') {
            let trimmed = part.trim();
            if let Some(id) = client_ids().find(|id| id.eq_ignore_ascii_case(trimmed)) {
                taken.push(id.to_string());
            } else if !trimmed.is_empty() {
                kept.push(trimmed.to_string());
            }
        }
        if !kept.is_empty() {
            out.push(OsString::from("--client"));
            out.push(OsString::from(kept.join(",")));
        }
    }
    let _ = CLI_CLIENTS.set(taken);
    out
}

/// Appends the ids [`split_cli_clients`] removed. When the command line asked
/// for Token Monitor clients only, the filter is exactly those ids: falling
/// back to `None` would scan every client instead.
pub fn merge_client_filter(
    filter: Option<Vec<String>>,
    canonical_was_empty: bool,
) -> Option<Vec<String>> {
    let taken = CLI_CLIENTS.get().map(Vec::as_slice).unwrap_or(&[]);
    if taken.is_empty() {
        return filter;
    }
    let mut merged = if canonical_was_empty {
        Vec::new()
    } else {
        filter.unwrap_or_default()
    };
    for id in taken {
        if !merged.iter().any(|existing| existing == id) {
            merged.push(id.clone());
        }
    }
    Some(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn split_cli_clients_removes_owned_ids_and_keeps_the_rest() {
        let out = split_cli_clients(args(&[
            "tokscale",
            "--json",
            "--client",
            "claude,proma,codex",
            "--today",
        ]));
        assert_eq!(
            out,
            args(&["tokscale", "--json", "--client", "claude,codex", "--today"])
        );
    }

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
