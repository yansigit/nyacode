# Compiled provider boundary verification

The refactor is on `refactor/compiled-native-providers`, based on published
commit `29413a496e28042629125693b3d8c218e49be879`. The published branch and its
history remain intact. The [architecture guide](refactor-architecture.md) and
[integration manifest](integration-manifest.json) describe the final dependency
boundary and every retained host hook.

## Compatibility evidence

The sanitized fixtures were captured before runtime extraction. They passed
against the original implementation after the mechanical directory move, and
the refactored runtime reads them and emits identical fixture bytes and storage
identities. Fixed digests also cover both JSON map configurations used by Cargo
and Bazel. No credential, checkpoint, journal, or lock migration is required.
The explicit generic configuration form requires this refactored build;
existing builtin and legacy configuration serialization is preserved.

Receipts are under `/tmp/native-refactor-receipts/`. Published-baseline results
are recorded separately in [refactor-baseline.md](refactor-baseline.md); they do
not count as validation of this refactor. The combined result is recorded in
`final-verification.json`: 20 original stages passed, and the two test-setup
failures described below passed their corrected targeted reruns. Original
failed receipts are retained unchanged.

| Check                                        | Refactor result                                                                                         | Receipt                                                                                                                         |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| Architecture and negative controls           | 23 controls pass; full Cargo dependency boundary passes                                                 | `final-compatibility/`, `architecture-corrected.log`                                                                            |
| Provider packages and compatibility fixtures | 134 Cargo tests and strict provider clippy pass; all 14 Linux Bazel targets pass                        | `final-compatibility/`, `bazel-final.log`                                                                                       |
| Configuration and model-provider facade      | 363 configuration tests and 103 facade tests pass                                                       | `final-compatibility/`                                                                                                          |
| Provider metadata and generic registration   | 40 tests pass under both Cargo and Bazel                                                                | `model-provider-metadata-corrected.log`, `bazel-metadata-corrected.log`                                                         |
| Core native routing and account/fork/resume  | Core routing and all three account/fork/resume tests pass under Cargo and Bazel                         | `final-compatibility/`, `bazel-host-candidate.log`                                                                              |
| Responses, CLI and schema tests              | 33 Responses client tests, six schema checks, and CLI provider parsing pass                             | `final-compatibility/`, `core-responses-corrected.log`                                                                          |
| TUI status                                   | 56 tests pass with repository test stack size                                                           | `tui-status-candidate.log`                                                                                                      |
| Strict host clippy                           | Core, CLI, and app-server pass                                                                          | `host-clippy-final.log`                                                                                                         |
| Formatting                                   | Repository formatter and Prettier checks pass                                                           | `fmt-check-final.log`, `pnpm-format-final.log`                                                                                  |
| Generated artifacts                          | Remote proto, configuration schema, stable/experimental API exports and Python SDK regenerated          | `generate-remote.log`, `generate-config-final.log`, `generate-appserver-final.log`, `generate-appserver-experimental-final.log` |
| Final CLI and feature matrix                 | CLI build and all six adapter feature combinations pass; metadata-only/runtime-only registry tests pass | `refactored-cli-final.log`, `final-compatibility/`                                                                              |
| Fresh live acceptance                        | All 16 provider checks and all seven CLI model routes pass                                              | `live/summary.json`, `services-run.log`                                                                                         |

The metadata test target initially exposed a missing `serde_json` dev dependency;
the dependency was added without changing production Rust code. The original
stage failure and the corrected rerun are retained separately. The test-count
guard also rejected a Responses filter that selected zero integration tests.
The manifest now selects the established 33 `client::tests::` library tests;
all 33 passed using the freshly compiled core test binary. Their receipt is
`core-responses-corrected.log`; the original zero-test result remains archived.

The TUI status run initially exceeded the default test-thread stack. The rerun
uses `RUST_MIN_STACK=8388608`, matching `just test`; the compatibility runner
now supplies the same setting. Four Windows cross-test targets were skipped by
the Linux Bazel invocation. The full repository suite and other platforms were
not tested.

## Live acceptance

All four providers passed fresh discovery, lifecycle, approval deny/allow, and
interruption checks. The lifecycle and interruption cases include tool
continuation, branch/compaction, cancellation, restart/model-change fencing, and
explicit branch recovery. Valid accounts were reused; no interactive login or
account replacement was needed.

| Provider    | Discovered models | CLI routes with write/read and process restart/resume               |
| ----------- | ----------------: | ------------------------------------------------------------------- |
| Cursor      |               256 | `gpt-5.6-luna-none`                                                 |
| OpenCode Go |                29 | `glm-5.3` (Chat), `minimax-m3` (Messages), `gpt-6-luna` (Responses) |
| Claude      |                14 | `claude-haiku-5-5`                                                  |
| Antigravity |                26 | `gemini-3.5-flash-lite`, `claude-sonnet-4-6`                        |

Each CLI route executed two successful commands in the initial turn and one in
a separate resumed process, retained the exact file contents, and returned both
completion markers without error events. The route summary is
`/tmp/native-refactor-services-mkx1156a/summary.json`.

These runs used source revision `23b376d59885268911c708758f03a2e2f5ca955f` and binary
SHA-256 `3a40c9a6beaa49df8f699b8f061636208813bc7d1444fd991bd53ecaf5809ce7`.
The coordinator records both in `live/build.json`; raw logs remain private.
Subsequent test dependency, CI test selection, and documentation fixes do not
change production Rust code.
This is selected-model Linux acceptance, not broader platform or SDK parity.

## Upstream footprint and automation

The comparison uses upstream base
`de8fab6d7adfcef8b4ce6f02f3b5c8be4092015a` and excludes provider implementation
directories in both layouts. Counts include upstream Rust tests and generated
remote bindings; they are review context, not a target. Exact filenames and
counts are in `footprint-comparison.json`.

| Source snapshot                  | Upstream Rust files | Added lines | Removed lines |
| -------------------------------- | ------------------: | ----------: | ------------: |
| Published baseline               |                  40 |       3,861 |            40 |
| Refactored source at `23b376d59` |                  46 |       1,615 |            81 |

The file count grows because generic configuration, constructor validation, and
their tests touch additional integration files. Session driving, persistence,
authentication, and protocol translation now reside inside the owned subtree.
The retained hooks are documented by purpose and regression coverage in the
integration manifest.

The hosted Linux workflow has read-only repository permissions and no live
credentials. Its temporary upstream merge probe records both revisions,
separates source and generated conflicts, and never resolves or pushes them.
Local negative controls exercise the probe; no hosted workflow run is claimed.
The daily schedule remains inactive until the workflow reaches the fork's
default `main` branch. The default branch has not been changed.
