# Released cache producers

These sanitized shards were written by the actual Darwin arm64 binaries, without modifying either writer: Token Monitor vendor `0154445d978eb081ea70d5154d98aa8670a1f9a2` (`timing-*`, Tokscale 4.19.1) and official npm `@tokscale/cli-darwin-arm64@4.20.0` (`billing-*`, upstream `c1973901fc2f7ba0d134252bfd923c714cc123db`). The Claude fixtures include real append checkpoints. Amp exercises the generic envelope; MiMo exercises its separate envelope and row provenance.

Both sources contain one assistant message with 100 input and 20 output tokens. Claude also has 30 cache-read and 10 cache-write tokens; Amp and MiMo record 0.75 credits. All paths and identifiers refer to synthetic fixtures under `/private/tmp/tm-tokscale-cache-fixture-{timing,billing}`. No user transcripts or credentials are included. The corresponding JSON reports record each producer's scan output.

The old producers reused generic generation 11 and Claude generation 13 for incompatible positional layouts. The migration tests require both to preserve all messages, discard obsolete checkpoints, and rewrite into a format that reloads warm. Codex's parser generation changes separately and requires reparsing its timing state.

Regenerate with Node 22.15+ using `node generate.cjs timing /absolute/path/to/pinned-vendor-binary` and `node generate.cjs billing /absolute/path/to/official-4.20.0-binary`. The script refuses an existing synthetic home, disables network pricing, and writes only sanitized fixtures here and beneath the named temporary home. The recorded reports also include an independent Hermes token-contract fixture; Hermes uses its database reader rather than these source-cache envelopes.
