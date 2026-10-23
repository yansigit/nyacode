# Refactor baseline receipt

The architecture refactor starts from provider implementation commit
`29413a496e28042629125693b3d8c218e49be879`. The first owned-subtree move is
`b3240782f5ccebdced0628bac1ca924118473043`, whose sole parent is that baseline.
The checked-in compatibility fixtures were captured at the baseline and
rebased across the move commit.

The [persisted-state fixture suite](../../codex-rs/providers/provider-runtime/tests/compatibility_fixtures.rs)
freezes credential/account selection, checkpoint and tool-journal JSON, the
checkpoint host-lineage/signature fields, thread-scoped interrupted-execution
markers, and provider namespace hashes. All credential-like values and
signature/digest strings are invalid placeholders. Existing codec regressions
also cover signed thinking blocks, signed Google function parts, and host
verified history replay.

The baseline comparison against upstream main recorded 109 changed files,
17,217 insertions, and 49 deletions. The full name/status and per-file summary
was captured in `/tmp/native-refactor-receipts/baseline-footprint.txt`.
Cargo metadata was captured in
`/tmp/native-refactor-receipts/baseline-metadata.json` with SHA-256
`2aeed1bb8a02ac474b8f46c9fbb6d96faeb17935a6f12767f5812bc4693c18c0`.
That snapshot lists the seven baseline provider packages and their direct
dependency declarations:

| Package                      | Direct dependency entries |
| ---------------------------- | ------------------------: |
| `codex-provider-api`         |                         7 |
| `codex-provider-auth`        |                        14 |
| `codex-provider-codecs`      |                        10 |
| `codex-provider-cursor`      |                        22 |
| `codex-provider-opencode`    |                         8 |
| `codex-provider-claude`      |                         8 |
| `codex-provider-antigravity` |                        11 |

These counts include repeated dependencies enabled for different targets or
features. The baseline graph shows provider packages depending on
`codex-provider-api`, auth, and codecs as needed; no provider package directly
depends on `codex-core`. The full metadata JSON stays in the receipt directory
because it contains machine-specific absolute paths. Recreate it by checking
out the baseline commit and running:

```sh
cargo metadata --manifest-path codex-rs/Cargo.toml --format-version 1 \
  > /tmp/native-refactor-receipts/baseline-metadata.json
sha256sum /tmp/native-refactor-receipts/baseline-metadata.json
```

The moved CLI build receipt is `/tmp/native-refactor-receipts/moved-cli.log`.
It records a default-feature `codex-cli` build after the package relocation;
the receipt should be retained with the refactor review artifacts.
