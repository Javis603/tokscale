# Source snapshot export

`tokscale --json --source-snapshots --no-spinner` (also `models --json --source-snapshots`) and `tokscale graph --source-snapshots --no-spinner` add the following v2 accounting evidence. Native period reports attach it as `ModelReport.sourceUsage` from the same priced, deduplicated scan; `--today` does not run an additional graph scan. Ordinary graph/report output without the flag remains unchanged; submission/recovery payloads never contain `sourceUsage`. The opt-in v1 daily snapshot shape is superseded, not emitted beside v2.

```ts
type SourceUsage = {
  version: 2;
  eventIdentityVersion: 2;
  capturedAt: string; // pre-scan ISO timestamp
  fullHistory: boolean;
  bucketTimezone: string; // IANA
  dayKey: string; // YYYY-MM-DD in that zone
  scanRoots?: { claude: string[] }; // acknowledged JSON config roots; LOCAL ONLY
  rootUsage?: { root: string; client: 'claude'; tokens: number; readStatus: 'complete' | 'partial' | 'unknown' }[]; // LOCAL ONLY
  replacementClients: string[]; // literal non-synthetic client selections
  partialClients: string[];
  scopes: {
    id: string; // opaque stable scanner dedup-domain digest
    clients: string[]; // raw ownership partitions in the domain
    readStatus: 'complete' | 'partial' | 'unknown';
    identityStatus: 'exact' | 'unresolved';
    reason?: string;
  }[];
  sessions: {
    client: string; // raw owner, independent of event identity
    sessionId: string;
    scopeId: string;
    events: {
      id: string | null;
      observationKind: 'native' | 'mirror';
      contributions: {
        timestamp: number; // epoch milliseconds
        modelId: string;
        providerId: string;
        tokens: {
          input: number;
          output: number;
          cacheRead: number;
          cacheWrite: number;
          cacheWrite1h: number; // subset of cacheWrite, never add again
          reasoning: number;
        };
        cost: number;
        messages: number;
        pricing?: { unpricedTokens: number; unpricedMessages: number };
        performance?: {
          timedTokens: number;
          timedOutputTokens: number;
          totalDurationMs: number;
          sampleCount: number;
        };
      }[];
    }[];
  }[];
  rowsWithoutIdentity: Record<string, number>;
};
```

Consumers derive session/day/model projections from timestamped contributions; neither sessions nor events duplicate daily accounting. Each contribution merges only equal `(timestamp, modelId, providerId)` rows inside one event, with saturated token/message arithmetic and the existing priced cost. `cacheWrite` includes the 1-hour subset already included by ordinary graph totals; the exported `cacheWrite1h` subset is not added again. Opt-in native `entries[].performance.timedOutputTokens` uses exactly the same timed-message predicate and report grouping as the contributions. It is present even as zero when no row was timed, and absent from ordinary unflagged reports; consumers must prefer it to the legacy whole-output approximation when present. Opt-in native `entries[].unpricedTokens` and graph `contributions[].clients[].unpricedTokens` classify the same filtered priced messages as contribution pricing, independent of whether a fallback cost is zero. They are explicit zero for covered/authoritative usage and absent from unflagged reports. Native output and reasoning are disjoint token buckets; generation throughput uses their sum. Performance records positive-duration, positive-total samples using the same native performance predicate: `timedTokens` counts all positive token buckets; `timedOutputTokens` counts positive output plus reasoning; `totalDurationMs` and `sampleCount` are additive integers. Pricing records token-bearing messages without provider-reported authoritative cost whose buckets are not covered by the current native pricing service; absent pricing counts do not imply zero usage. Costs retain native lenient pricing rather than submission zeroing. Summing emitted event contributions reproduces identified-session graph accounting, apart from the usual floating-point fold-order rounding. Active time and intensity remain aggregate graph metrics, not retained per-event accounting.

`fullHistory` is true only without `since`, `until`, or `year`. It describes date selection, never read health. `replacementClients` describes client selection, never successful parsing. An observed stable event can carry an exact downward correction within a date-selected report, including `--today`; `fullHistory: false` does not invalidate its identity/read proof. Whole-scope replacement additionally needs full-history date coverage. Corrections and ownership transfer require supported v2 identity, a **complete scope**, and **exact identity**. Scope mode and permanent retained accounting belong to the consumer; an unavailable export must not restore legacy accounting for migrated scopes. Omitted IDs/events never prove deletion. Unsupported versions, unknown health, partial reads and unresolved identities cannot authorize destructive replacement.

The shared MiMo scope always lists both `micode` and `micode-desktop`; selecting only one surface leaves that scope partial, even when every selected row parsed. The same conservative selection rule applies to other shared domains: Antigravity's three surfaces; Claude plus observed `cc-mirror/*` aliases; GJC plus `9router`. Selecting `gjc` covers `9router`; selecting only `9router` does not cover GJC. Synthetic-only gateway selection never grants a full real-client scope. Raw clients/session owners are retained in storage and export; normalizing a display label must not promote a partial scope. Token Monitor-owned clients remain explicit opt-ins rather than joining default scans.

## Read and inventory evidence

Claude, Codex and MiMo provide instrumented evidence. Other parser scopes are `unknown`/`unresolved`, even when they emitted plausible rows or a cache returned data. Known traversal/explicit-root errors, parse failures and stable-read failures are partial. Missing optional default roots produce no observed IDs; missing explicitly configured roots are partial. An empty successful scan never authorizes deletion of omitted IDs.

For source snapshots only, Claude, Codex and MiMo bypass pre-existing message caches and pair freshly parsed rows with fresh health/inventory. This prevents old envelopes, bounded-sample cache hits and cached partial data from acquiring completeness. Snapshot reads leave legacy message caches intact. Codex uses a separate source-read check: recognized usage/accounting-context records that fail typed decoding, malformed declared usage objects, and invalid or missing supported numeric counters make the read partial, even when tolerant native parsing succeeds. Declared counters must be nonnegative integers in a supported layout. Unsupported/conflicting aliases or usage locations, discarded positive usage, and declared totals exceeding accounted counters cannot authorize completeness. Optional absent/null usage containers, explicit numeric zero and rate-limit-only updates remain valid; unknown non-usage records remain compatible. Native parser output/cache success is not source-read authority. Codex also propagates JSON/I/O failures and unresolved-model reads; MiMo propagates `MiMoSource.complete`, SQL/schema failures, invalid assistant payloads and session metadata read failures. SQLite session inventory and message rows are read in one transaction. Full fingerprints bracket the instrumented read (including MiMo's WAL); a differing/unavailable fingerprint makes the scope partial. This adds opt-in I/O and detects known instability; it does not make a multi-file scan atomic or guarantee coherent observation of an adversarial concurrent writer.

MiMo inspects the raw declared accounting on the same parsed JSON tape before native normalization. Required input/output counters and present optional reasoning/cache counters must be nonnegative i64 integers; declared cost must be finite and nonnegative. Malformed accounting objects, positional array layouts, negative values that native parsing clamps, known unsupported counter aliases or usage locations that would discard nonzero usage, and totals exceeding accounted counters make the scope partial. Supported absent/null optional reasoning, cache, cache read/write and cost remain valid, including integer zero cost and explicit zero usage. Unknown non-usage metadata is ignored. Invalid rows retain their ordinary native decoding, clamping, pricing, dedup and cache admission behavior. Ordinary scans use the original assistant/accounting query and do not run raw source accounting validation; unchanged stores can remain warm. Opt-in export also inspects assistant rows without accounting, bypasses native caches and cannot certify source completeness from cached rows. The cache envelope and parser accounting versions are unchanged.

IDs are observed before date filtering. A decoded Codex `session_meta` proves its native path-derived owner ID even when it emits no usage. MiMo's successfully read session table proves both raw surface IDs, including sessions without assistant rows; message IDs supplement compatible legacy databases without a session table. Those sessions export `events: []`. Inventory is filtered to literal selected raw clients; synthetic-only filters cannot turn excluded native rows into empty authoritative sessions. A file omitted from discovery is never an empty-session inventory entry. Ordinary parser-emitted zero rows still contribute their native message count. Codex's native suppression of zero token_count snapshots stays unchanged; an explicit complete empty session can carry the zero correction.

`rowsWithoutIdentity` counts emitted rows before date filtering with blank/whitespace session IDs, `unknown`, or Copilot's `unknown-session` sentinel. Their accounting remains in aggregate graph contributions and outside identified snapshots. Copilot trace-ID fallback provenance is not upgraded to exact identity. Null event groups collect unresolved rows per session. Invalid/nonpositive timestamps or session identity gaps also prevent exact scope authority.

## Stable identity

Scope IDs are `sha256:` plus 64 lowercase hex digits, hashing UTF-8 `tokscale-source-scope-v2`, NUL, the domain byte length as unsigned big-endian u64, and the UTF-8 scanner domain. Event IDs use the corresponding `tokscale-source-event-v2` namespace and append the native key or export group key. Domain aliases match scanner accounting lanes: MiMo CLI/Desktop share `micode`; Antigravity shares `antigravity`; `cc-mirror/*` shares `claude`; case-insensitive `9router` shares `gjc`. Independent clients reusing a key remain distinct. Neither digest adds the current owner, path, timezone, day, model, tokens or price.

**Codex explicit turns:** the freshly parsed normalized message carries a transient export-only `source_group_key`, derived from explicit upstream session (`session_forked_from_id`, otherwise `session_id_from_meta`) plus explicit `current_turn_id`. Its digest uses `tokscale-codex-source-group-v2`, NUL, the length-delimited upstream ID, then turn ID. All already-priced/native-deduplicated rows for that logical turn share one export event and can have multiple timestamp/model/provider contributions. When native dedup leaves different contributions for one turn under several rollout owners, export consolidates the whole group under the lexicographically first observed raw `(client, sessionId)`; other observed session IDs remain present, possibly with empty events. There is one whole-group claim per exact Codex event. Mutable native token_count keys are still used by ordinary scanner dedup; they never substitute for a missing export group. Missing upstream/turn ID yields a null identity and unresolved scope. No ordinal, cumulative-total, model/provider or path fallback is invented. A complete explicit-turn 45 → 15 correction preserves the event ID; a malformed 15 is partial. OpenClaw mirror precedence remains unresolved rather than acquiring Codex's exact scope authority.

Codex group keys are calculated only during opt-in full-file source reads and are never persisted. The serde-skipped normalized-message field leaves the ordinary message bincode layout unchanged; Codex uses the existing shared cache envelope format v10 and existing parser version 15. Ordinary full/incremental/warm scans do not compute export groups. Source export reparses the full file without a cache entry, so warm native caches cannot supply or suppress its turn identity or read health. MiMo keeps its existing cache envelope format v11.

**MiMo explicit responses:** the stable embedded response ID is shared across CLI/Desktop, including when the winning owner changes. Missing embedded IDs use native DB/row fallback keys only for diagnostic export and leave the scope unresolved. Different-key fingerprint/chronology merges also leave the scope unresolved: the scanner can prove equivalence within a scan but the surviving alias set is not a permanent cross-scan identity. No exact digest is fabricated from mutable usage fingerprints. A single unresolved row/equivalence group makes the scope unresolved.

Recovered aggregates are excluded to avoid recursive archiving. An unpinned local timezone must resolve to an equivalent IANA zone or the opt-in export asks for a named timezone. Sessions sort by raw client/session, event groups by null first then digest, contributions by timestamp/model/provider, and scopes by scanner domain. Memory/output grows with distinct session/event/timestamp/model/provider tuples; the opt-in sink retains contribution maps, not message vectors or duplicated session/day folds. There are no new dependencies or changes to ordinary parser dedup/accounting without the flag.


## Native Claude and additional roots

`TOKSCALE_SCAN_ROOTS_JSON` is a JSON object mapping supported public client IDs to arrays of absolute roots. Claude entries are **config directories**, e.g. `{"claude":["/home/user/.claude"]}` or a native Windows UNC `\\wsl.localhost\\Ubuntu\\home\\user\\.claude` path encoded as a JSON string. The scanner walks each Claude root's `projects` and `transcripts`, even if no usage marker exists yet. Other supported clients use their normal scan-root interpretation. Commas and whitespace inside paths survive unchanged. JSON roots combine with defaults, legacy `TOKSCALE_EXTRA_DIRS`, and `scanner.extraScanPaths`; canonical roots/files deduplicate. Unknown clients, unsupported clients, malformed JSON, non-array values, non-string paths and non-absolute paths cannot grant complete evidence. Missing/unreadable explicit config directories are partial; missing optional children beneath a readable config directory can be empty.

Only `use_env_roots` scans respect this variable; explicit `--home` ignores it and does not acknowledge it. Both opt-in graph and native results export `scanRoots.claude` from the actual scan's respected roots, once each with the requested spelling, including respected missing roots that failed to read. This acknowledgement describes scanner capability and selection, **not** read success. No acknowledgement is emitted without a respected Claude JSON root. `rootUsage` folds freshly parsed Claude token totals by acknowledged root before cross-file event dedup, within the current report period and bucket timezone. Each root carries the conservative whole-Claude scope read health; any Claude discovery, parse or stable-read failure marks every acknowledged root partial, even if one root emitted rows. Positive `tokens` with `readStatus: complete` indicate that root supplied data; they can overlap and must never enter accounting totals or establish global ownership. These local diagnostic paths must not be copied into retained event proof, device wire records, or Hub submissions.

Claude snapshot reads pair the tolerant parser's actual rows with strict health in the same JSONL pass, bracketed by full fingerprints. Invalid JSON/I/O, invalid or explicitly null declared counters/cache containers (missing optional legacy buckets remain supported), ignored performance declarations, invalid declared timing timestamps, missing required input/output counters, unsupported accounting, discarded model attribution and invalid source timestamps make the read partial. Unsupported headless JSON layouts remain partial. Stable response keys are hashed with the existing graph v2 identity algorithm; unkeyed/blank-key rows remain null, and path-scoped tool-result keys leave identity unresolved. Inventory comes from emitted native rows; an empty file never invents a session ID. Cache-retained historical rows are excluded and legacy cache entries are left untouched, permitting a fresh 45 to 15 correction with the same event ID.

`observationKind: 'mirror'` requires a Claude `projects/ssh-<sessionId>` directory and a matching transcript stem or nested parent-session directory. An unrelated `ssh-` path is insufficient. Within an opt-in scan, a native copy wins the same stable event over a proven SSH mirror, including a lower native counter; disjoint events from both survive. Same-origin duplicates retain ordinary scanner completeness merging. A native observation may come from either the host or WSL, so it cannot establish WSL data status by itself. Null event groups remain unresolved even when they carry origin diagnostics. Metadata remains on native report `sessions`, `workspaces`, and session/workspace-grouped `entries`; event proof contains no source paths.
