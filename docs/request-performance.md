# Request performance timing

Model report `performance` counters describe observed request timing from client logs. They are estimates, may include queueing, network and first-output waiting, and are not provider-side decoding measurements. Timing definitions still differ between clients.

`totalDurationMs`, `timedTokens` and `sampleCount` sum only positive-duration samples with positive token totals. `timedOutputTokens` and `timedReasoningTokens` retain the exact non-negative token buckets from those same messages before grouping. Explicit zero is a measured empty bucket, not a reason to substitute all output from the grouped entry. Reasoning remains separate: consumers must apply the same client-specific inclusive/disjoint rules they use for ordinary output. Do not estimate output coverage from total-token coverage.

`UnifiedMessage.duration_ms` and its timestamp continue to define session/activity spans. The optional `performance_timing` overrides only performance: `Duration` supplies the request duration; `Unavailable` excludes the message from performance without dropping usage. An absent override keeps each existing parser's duration behavior. Parsed-message conversion and source caches preserve the override.

## Codex

Codex can write `token_count` after local tool execution. The parser keeps token_count as the usage authority but measures performance using the matching request's `token_usage_record` completion when available. Counter matching and timestamp order are checked; mismatched or impossible samples remain untimed.

The start comes from a turn/user/tool-result boundary and is frozen when model output is observed. Later tool results do not move an already-observed request's start. For older rollouts without request usage records, a tool result flushed with token_count uses the last model item's completion as an estimated endpoint; otherwise token_count remains the fallback endpoint. This is a heuristic, not a streaming measurement. Parallel tool intervals are never summed and subtracted. An explicit new turn clears the preceding response's 100 ms flush window, so fast tool calls remain eligible; post-count items without a new turn remain subject to the legacy flush guard.

Explicitly empty assistant completions with no reasoning are excluded from performance while retaining their token and cost accounting. Tool calls, reasoning, nonempty short replies and legacy messages without content evidence remain eligible, even when their estimated rate is low. This is a content-based distinction, not a minimum-rate threshold.

Codex parser generation 22 tracks empty-completion evidence independently of timestamp availability and invalidates pre-generation-22 incremental state and performance aggregates. Combined cache formats 14 (shared), 15 (MiMo), and 16 (Claude/Copilot) avoid the upstream/downstream generation collisions. Frozen decoders migrate both released layouts while preserving retained messages and MiMo row provenance; obsolete Claude checkpoints rebuild from source. Copilot's old envelopes reparse. The separate TUI JSON cache uses schema 12 for the added performance buckets and rejects aggregates from an older parser generation. A missing or non-positive sample must never remove tokens, costs or session relationships.

The timer consumes events during the existing parse pass and is serialized with incremental state. It adds no second transcript scan or provider request. Upgrading invalidates old Codex shards once; subsequent scans retain the existing cache and append behavior. Per-event bookkeeping adds some CPU and state overhead; its runtime cost has not been benchmarked.
