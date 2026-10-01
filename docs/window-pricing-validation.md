# Window pricing: local validation and fork handoff

Status: locally prepared on `codex/window-scoped-pricing-ready`, based on token-monitor's consumed fork pin `ab1067f38edda3faa822c67b6df016c5c38ded9b`. Original experimental branch `perf/window-scoped-pricing` remains at `52ee12d7`; the original `codex/mcode-store-draft` checkout was not edited. No fork push, PR, release, or token-monitor binary pin change has been made. The maintainer has now invited a PR to `Javis603/tokscale` main; the user requires internal review before any push or PR creation. Publication and token-monitor pin updates remain the maintainer's responsibility.

## Behavior and scope

The optimization is in tokscale-core, not token-monitor. Date-filtered model reports can skip estimated pricing for unchanged warm-cache messages whose final report day is outside the requested window. Every message still participates in the existing deduplication, ownership, retention, and recovery reducers. Cold and changed-source parsing still use full pricing; specialized SQLite/Prime/MiMo/uncached paths and other report entry points retain their prior behavior. Grok reprices after metadata selection and therefore does not currently retain the warm pricing saving. See [the complete client matrix](window-pricing-clients.md).

Unlike the original experiment, pinned bucket timezones are supported. The pricing predicate predicts the exact final rebucket day without changing reducer inputs, using the same timestamp sentinel/fallback rules as `rebucket_date`. It resolves timezone from the actual scan settings, not a second copy of settings carried in report options. Codex warm/cold/append finalization shares one implementation. Estimated prices still use the existing authority guard, provider multipliers, and current catalog; no cache format, public option, grouping, or wire field changed.

The unrelated shard-size-cliff regression was removed from the final feature diff. Its original test remains preserved on the untouched experimental branch; an extracted patch is also available locally at `/private/tmp/window-cache-cliff-regression.patch`.

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

Historical local replay artifacts (the debug binaries and temporary source fixtures were subsequently removed during user-requested cleanup):

- Harness: `/private/tmp/window-cli-acceptance.py`.
- Full baseline/candidate JSON reports: `/private/tmp/window-cli-results.json`.
- Baseline binary: `/private/tmp/tokscale-window-target/debug/tokscale-window-baseline`.
- Candidate binary: `/private/tmp/tokscale-window-target/debug/tokscale-window-candidate`.
- Full test log: `/private/tmp/window-tests-final.log`.
- Strict clippy log: `/private/tmp/window-clippy-final.log`.

The paths above record the original execution; they are not a currently runnable replay command. Test logs, the harness and JSON comparisons are preserved in the separate handoff archive, and the permanent Rust regression tests remain in the branch. Restoring fixtures or rebuilding is a separate action; this documentation correction did neither.

## Limits of this evidence

Every registered client and Synthetic has a static review record. New runtime fixtures are representative, not per-client runtime acceptance: public-report fixtures cover Codex, Claude, Cursor, OpenCode, Hermes and OpenClaw; GJC has only a reported-zero case. Pristine-baseline CLI comparisons cover only Codex, Claude, Cursor, OpenCode and OpenClaw. Other registered clients have no dedicated window-feature runtime evidence in this round, and covered clients still have untested subpaths. Existing parser tests and shared-loader reasoning do not fill that gap. No exhaustive real-history execution is claimed. The full workspace suite also executes existing parser/reducer and CLI fixtures on this platform. Linux/Windows execution remains for CI when a PR is created. This correctness document does not supply performance measurements. The earlier v12 percentages describe the old experiment/baseline. Later default-release CPU and powermetrics measurements were archived separately for implementation commit `62744c8e`; they apply to their recorded corpora and do not establish per-client correctness.

## Delivery after internal review

The maintainer's reply on token-monitor#637 explicitly accepts the existing equivalence tests and measurements for code review and asks for no further benchmark expansion. The target is `Javis603/tokscale` main. The user's subsequent instruction requires internal review before any push or PR creation; the maintainer invitation does not override that instruction.

1. Complete and record the internal review, retaining both findings and the per-client coverage gaps. Do not turn representative tests into a claim of exhaustive client acceptance.
2. Compare the then-current fork target with the saved baseline. If it is unchanged and only documentation changed locally, use the existing results. If implementation or target changes, decide the smallest relevant checks and discuss new measurement costs before regenerating workloads; never apply old percentages blindly to changed code.
3. Keep the review scope to the core window optimization, its equivalence tests and directly supporting documentation. Exclude phase instrumentation, benchmark machinery and the independent shard-cliff regression. Existing measured results may be referenced with their exact implementation SHA, release settings, workload and limitations; no v12/profiling rerun is required to open this review.
4. Push and create the review PR only after the user's review instruction has been satisfied and submission is authorized. Do not change the token-monitor pin or publish a build; the maintainer will handle those after merging.

Suggested PR title: `perf(core): skip out-of-window warm estimates in model reports`.

Suggested description: Date-filtered model reports currently re-estimate all warm cached historical messages before discarding dates outside the requested period. Gate that estimate by the final report day, including pinned timezones, while retaining the full message set for existing reducers and raw cache persistence. Preserve the fork's session/workspace metadata, recovery overlay, and tier pricing. Specialized unoptimized paths keep their existing behavior. Include the executed checks above and link `Javis603/token-monitor#637`; do not attach old performance percentages as measurements of this revision.
