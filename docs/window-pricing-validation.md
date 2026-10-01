# Window pricing: behavior and validation

The implementation is based on `Javis603/tokscale` main `ab1067f38edda3faa822c67b6df016c5c38ded9b`, also consumed by token-monitor. Runtime validation and default-release measurements apply to implementation commit `62744c8e9d6c918ea67fb66f3d160553d3900588`; later changes are documentation only. Related discussion: [token-monitor#637](https://github.com/Javis603/token-monitor/issues/637), including the [maintainer's requested scope](https://github.com/Javis603/token-monitor/issues/637#issuecomment-5937267244).

## Behavior and scope

The optimization is in tokscale-core, not token-monitor. Date-filtered model reports can skip estimated pricing for unchanged warm-cache messages whose final report day is outside the requested window. Every message still participates in the existing deduplication, ownership, retention, and recovery reducers. Cold and changed-source parsing still use full pricing; specialized SQLite/Prime/MiMo/uncached paths and other report entry points retain their prior behavior. Grok reprices after metadata selection and therefore does not currently retain the warm pricing saving. See [the complete client matrix](window-pricing-clients.md).

Unlike the original experiment, pinned bucket timezones are supported. The pricing predicate predicts the exact final rebucket day without changing reducer inputs, using the same timestamp sentinel/fallback rules as `rebucket_date`. It resolves timezone from the actual scan settings, not a second copy of settings carried in report options. Codex warm/cold/append finalization shares one implementation. Estimated prices still use the existing authority guard, provider multipliers, and current catalog; no cache format, public option, grouping, or wire field changed.

The general cache shard-size cliff is outside this change. No phase instrumentation or benchmark machinery is included.

## Executed validation

All commands run with Rust 1.98.0 on this macOS Apple Silicon machine, offline dependency resolution, and isolated temporary fixtures. Bundled Apple Foundation Models compilation is included in all-features checks; normal process permissions were needed for Swift's own build sandbox. No private account histories or account services were used for the differential fixtures.

- Original experiment: `cargo test --offline --locked -p tokscale-core --lib window_sieve` passed all six original tests.
- A regression requiring fixed-timezone warm scans to skip historical estimates failed before the implementation change, then passed after it. This demonstrates the previous production-path coverage gap, not a proven historical accounting corruption.
- Fork-aligned public-report differential tests: `cargo test --offline --locked -p tokscale-core --lib tests::window_pricing -- --nocapture` passed the current fixture suite. They compare full report entries, session/workspace metadata, and totals against the same fork's full-pricing pipeline; repeated calls are compared too.
- `cargo clippy --offline --locked --workspace --all-features --all-targets -- -D warnings` passed.
- `cargo test --offline --locked --workspace --all-features` passed every nonignored suite. Existing ignored tests remain ignored; this does not claim those scenarios executed.
- `cargo test --offline --locked -p tokscale-core --lib window_` selected 19 passing tests: 16 concern this window-pricing feature, while three existing LM Studio tests concern streaming-buffer windows. The latter are unrelated and are excluded from the feature-specific count.
- `cargo fmt --all -- --check` passed.

Public-report fixtures cover Codex, Claude, Cursor, OpenCode SQLite, Hermes SQLite, OpenClaw SQLite/JSON overlap, and GJC reported-zero amounts. Cases include all supported grouping modes (including `client,workspace,session,model`), narrow/wide/narrow queries, new price catalogs, missing pricing becoming available, raw cache shard byte invariance, file append/Claude compaction/CSV rewrite/deletion, SQL insert/update/delete, Codex priority pricing, and the fork's request archive/daily-floor recovery overlay. Prices and dates are deterministic. Date-prediction tests include positive, nonpositive, unrepresentable timestamps, year/day boundaries, inclusive ranges, contradictory predicates, and US daylight-saving transitions.

## Actual command-line output comparison

A pristine binary built from `ab1067f3` and the candidate binary were run against the same fixed source fixtures with separate cache/config roots. The standard-library harness used `--home`, `TOKSCALE_CONFIG_DIR`, `TOKSCALE_PRICING_CACHE_ONLY=1`, and `--no-spinner`; it did not repoint the shell HOME or use real account data.

All 39 comparisons passed: Codex/Claude/Cursor/OpenCode/OpenClaw cold and warm reports, all grouping modes, narrow/year/narrow windows, warm price-catalog replacement, Claude append/compaction/deletion, Cursor export replacement/deletion, OpenCode SQL update/insert/delete, and OpenClaw SQLite/JSON migration overlap plus SQL changes, and Codex incremental append/truncation plus fallback modification-time movement across dates. Full JSON (including diagnostics and metadata) matched after excluding only elapsed processing-time fields and normalizing unordered report arrays. This is output-equivalence evidence, not a performance benchmark.

The comparison harness and execution logs were preserved separately from the source change; temporary fixture trees and build caches were removed after execution. The permanent Rust regression tests remain in this branch. These results are historical validation, not a promise that temporary replay paths remain available.

## Limits of this evidence

Every registered client and Synthetic has a static review record. New runtime fixtures are representative, not per-client runtime acceptance: public-report fixtures cover Codex, Claude, Cursor, OpenCode, Hermes and OpenClaw; GJC has only a reported-zero case. Pristine-baseline CLI comparisons cover only Codex, Claude, Cursor, OpenCode and OpenClaw. Other registered clients have no dedicated window-feature runtime evidence in this round, and covered clients still have untested subpaths. Existing parser tests and shared-loader reasoning do not fill that gap. No exhaustive real-history execution is claimed. The full workspace suite also executes existing parser/reducer and CLI fixtures on this platform. Linux/Windows execution remains for CI when a PR is created. This correctness document does not supply performance measurements. The earlier v12 percentages describe the old experiment/baseline. Later default-release CPU and powermetrics measurements were archived separately for implementation commit `62744c8e`; they apply to their recorded corpora and do not establish per-client correctness.

## Integration

The public CLI parameters and JSON report shape are unchanged. token-monitor can continue its current serial today/month/allTime model-report scans and its today-only anchored watch scans. A full collector tick is not exclusively a today query. The graph entry point is unchanged and does not receive this optimization. Building the fork release and updating token-monitor's binary pin are separate maintainer steps after merging.

No additional benchmark collection is required for this review, as requested in the linked maintainer reply. Reuse historical results only while the implementation and target baseline remain the same; reassess relevant checks if either changes. Coverage limits above must remain visible when reporting the results.
