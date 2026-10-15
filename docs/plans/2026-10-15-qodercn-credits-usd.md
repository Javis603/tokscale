# Qoder CN CLI credits in ordinary usage reports

## Scope and decisions

- Extend the existing downstream Qoder CN parser, not the upstream client registry.
- Preserve measured input/output/cache counts. Keep masked-token requests when they carry per-request CLI credits; never infer tokens from money or context occupancy.
- CN reference: CNY 40 / 1,000 credits. Default FX is CNY 6.8/USD; the embedding app can supply `TOKEN_MONITOR_QODER_CN_USD_PER_CREDIT` from its effective FX. Invalid overrides warn and use the documented default.
- Cache raw credits and convert when emitting UnifiedMessage. Increment source-cache version to reparse historical credit-only rows once.
- Deduplicate stable request ids across transcript paths, falling back to namespaced message id or envelope uuid. Keep the newest observation.
- Mark converted cost provider-reported, following Kiro, so ordinary catalog pricing does not add a second charge. Existing explicit custom-price precedence remains unchanged.
- Map qfmodel to Qwen3.8-Flash, verified against the CLI's actual get_models catalog.

## Acceptance and evidence

- CLI numeric nonnegative credits, including explicit zero and billable:false, enter standard USD costs.
- Non-CLI and invalid credits do not become costs; measured legacy/custom token behavior remains intact.
- Cold/warm copied requests count once; FX changes revalue raw cache without fabricating tokens.
- Targeted parser suite: 11 passed. Full workspace/all-features tests passed; final scoped tests and Clippy passed after the identity/price assertions were tightened. Format check passed.
- Companion Token Monitor PR #1057 passes effective FX and reuses the ordinary model/session/history cost path. Official app rollout requires building/pinning this native change.
