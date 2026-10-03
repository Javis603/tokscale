//! Command-line side of Token Monitor-owned clients
//! (`tokscale_core::token_monitor`).
//!
//! Owned client ids are not `ClientFilter` variants, so clap would reject
//! them. [`parse_cli`] removes them from `--client`/`-c` values before clap
//! parses argv, but keeps that parse only for commands whose client filter is
//! built by `build_client_filter`, which merges the ids back through
//! [`merge_client_filter`]. Every other command is parsed from the original
//! argv, so it behaves exactly as upstream: `submit --client proma` fails with
//! clap's own error instead of silently widening to the default clients, and
//! `headless` passthrough arguments are never rewritten.

use crate::{Cli, Commands};
use clap::Parser;
use std::ffi::OsString;
use std::sync::OnceLock;

static CLI_CLIENTS: OnceLock<Vec<String>> = OnceLock::new();

/// Parses argv, accepting Token Monitor-owned ids in `--client`.
pub(crate) fn parse_cli<I>(args: I) -> Cli
where
    I: IntoIterator<Item = OsString>,
{
    let original: Vec<OsString> = args.into_iter().collect();
    let (split, taken) = split_cli_clients(&original);
    if taken.is_empty() {
        return Cli::parse_from(original);
    }
    match Cli::try_parse_from(&split) {
        Ok(cli) if accepts_owned_clients(&cli.command) => {
            let _ = CLI_CLIENTS.set(taken);
            cli
        }
        _ => Cli::parse_from(original),
    }
}

/// The commands that resolve their filter through `build_client_filter`.
fn accepts_owned_clients(command: &Option<Commands>) -> bool {
    matches!(
        command,
        None | Some(
            Commands::Models { .. }
                | Commands::Monthly { .. }
                | Commands::Hourly { .. }
                | Commands::Graph { .. }
                | Commands::Tui { .. }
                | Commands::Wrapped { .. }
                | Commands::TimeMetrics { .. }
        )
    )
}

/// Removes owned ids from `--client`/`-c` values and returns them. A flag
/// whose value holds no owned id is passed through byte for byte, a flag left
/// with no values is dropped, and nothing after `--` is touched.
fn split_cli_clients(args: &[OsString]) -> (Vec<OsString>, Vec<String>) {
    let mut out = Vec::with_capacity(args.len());
    let mut taken = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy();
        if text == "--" {
            out.push(arg.clone());
            out.extend(iter.cloned());
            break;
        }
        let inline = match text.strip_prefix("--client=") {
            Some(value) => Some(value.to_string()),
            None => text
                .strip_prefix("-c")
                .filter(|value| !value.is_empty())
                .map(|value| value.strip_prefix('=').unwrap_or(value).to_string()),
        };
        let (value, flag_tokens) = match inline {
            Some(value) => (value, vec![arg.clone()]),
            None if text == "--client" || text == "-c" => match iter.next() {
                Some(next) => (
                    next.to_string_lossy().into_owned(),
                    vec![arg.clone(), next.clone()],
                ),
                None => {
                    out.push(arg.clone());
                    break;
                }
            },
            None => {
                out.push(arg.clone());
                continue;
            }
        };
        let mut kept = Vec::new();
        let mut owned = Vec::new();
        for part in value.split(',') {
            let trimmed = part.trim();
            match tokscale_core::token_monitor::client_ids()
                .find(|id| id.eq_ignore_ascii_case(trimmed))
            {
                Some(id) => owned.push(id.to_string()),
                None => kept.push(part),
            }
        }
        if owned.is_empty() {
            out.extend(flag_tokens);
            continue;
        }
        taken.extend(owned);
        if kept.iter().any(|part| !part.trim().is_empty()) {
            out.push(OsString::from("--client"));
            out.push(OsString::from(kept.join(",")));
        }
    }
    (out, taken)
}

/// Appends the ids [`parse_cli`] removed to the canonical filter.
pub(crate) fn merge_client_filter(
    filter: Option<Vec<String>>,
    canonical_was_empty: bool,
) -> Option<Vec<String>> {
    let taken = CLI_CLIENTS.get().map(Vec::as_slice).unwrap_or(&[]);
    merge_owned(filter, canonical_was_empty, taken)
}

/// When the command line asked for owned clients only, the filter is exactly
/// those ids: the canonical filter would otherwise fall back to the TUI
/// default clients, or to `None`, which scans every client.
fn merge_owned(
    filter: Option<Vec<String>>,
    canonical_was_empty: bool,
    taken: &[String],
) -> Option<Vec<String>> {
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

    fn owned(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn removes_owned_ids_in_every_clap_spelling() {
        for spelling in [
            args(&["tokscale", "--client", "claude,proma"]),
            args(&["tokscale", "--client=claude,proma"]),
            args(&["tokscale", "-c", "claude,proma"]),
            args(&["tokscale", "-cclaude,proma"]),
            args(&["tokscale", "-c=claude,proma"]),
        ] {
            let (out, taken) = split_cli_clients(&spelling);
            assert_eq!(
                out,
                args(&["tokscale", "--client", "claude"]),
                "{spelling:?}"
            );
            assert_eq!(taken, owned(&["proma"]), "{spelling:?}");
        }
    }

    #[test]
    fn leaves_flags_without_owned_ids_untouched() {
        let input = args(&["tokscale", "-c", " claude ,codex", "-cgemini", "--json"]);
        let (out, taken) = split_cli_clients(&input);
        assert_eq!(out, input);
        assert!(taken.is_empty());
    }

    #[test]
    fn drops_a_flag_left_without_values() {
        let (out, taken) =
            split_cli_clients(&args(&["tokscale", "-c", "proma", "--client", "QoderCN"]));
        assert_eq!(out, args(&["tokscale"]));
        assert_eq!(taken, owned(&["proma", "qodercn"]));
    }

    #[test]
    fn stops_at_the_argument_terminator() {
        let input = args(&["tokscale", "headless", "codex", "--", "-c", "proma"]);
        let (out, taken) = split_cli_clients(&input);
        assert_eq!(out, input);
        assert!(taken.is_empty());
    }

    #[test]
    fn report_commands_keep_owned_clients() {
        for command in [&[][..], &["models"][..], &["graph"][..], &["monthly"][..]] {
            let mut argv = vec!["tokscale"];
            argv.extend_from_slice(command);
            argv.extend_from_slice(&["--client", "proma,claude"]);
            let (split, taken) = split_cli_clients(&args(&argv));
            let cli = Cli::try_parse_from(&split).expect("split argv parses");
            assert!(accepts_owned_clients(&cli.command), "{command:?}");
            assert_eq!(taken, owned(&["proma"]));
        }
    }

    #[test]
    fn other_commands_fall_back_to_the_original_argv() {
        // submit would otherwise lose its only filter and submit the default
        // clients; headless must forward its arguments verbatim.
        let (split, _) = split_cli_clients(&args(&["tokscale", "submit", "--client", "proma"]));
        let cli = Cli::try_parse_from(&split).expect("split argv parses");
        assert!(!accepts_owned_clients(&cli.command));
        assert!(Cli::try_parse_from(args(&["tokscale", "submit", "--client", "proma"])).is_err());

        let headless = args(&[
            "tokscale", "headless", "codex", "exec", "-c", "proma", "-c", "model=o3",
        ]);
        let cli = parse_cli(headless);
        let Some(Commands::Headless {
            args: forwarded, ..
        }) = cli.command
        else {
            panic!("expected headless");
        };
        assert_eq!(forwarded, owned(&["exec", "-c", "proma", "-c", "model=o3"]));
    }

    #[test]
    fn merge_owned_replaces_defaults_only_when_no_canonical_client_was_given() {
        let taken = owned(&["proma"]);
        assert_eq!(merge_owned(None, true, &taken), Some(owned(&["proma"])));
        // TUI default clients apply when no canonical flag is present; an
        // owned-only command line must not inherit them.
        assert_eq!(
            merge_owned(Some(owned(&["claude"])), true, &taken),
            Some(owned(&["proma"]))
        );
        assert_eq!(
            merge_owned(Some(owned(&["claude", "proma"])), false, &taken),
            Some(owned(&["claude", "proma"]))
        );
        assert_eq!(
            merge_owned(Some(owned(&["claude"])), false, &[]),
            Some(owned(&["claude"]))
        );
        assert_eq!(merge_owned(None, true, &[]), None);
    }
}
