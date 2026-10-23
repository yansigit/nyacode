# Native provider implementation ledger

The compiled provider boundary refactor is on
`refactor/compiled-native-providers`, starting from published baseline
`29413a496e28042629125693b3d8c218e49be879` without rewriting it. Fresh verification,
live acceptance, rollback compatibility, and CI activation are recorded in
[refactor-validation.md](refactor-validation.md). The dependency diagram and
retained host hooks are in [refactor-architecture.md](refactor-architecture.md)
and the [integration manifest](integration-manifest.json).

The publication and live receipts below describe the published implementation;
they remain baseline evidence. Provider source revisions and licenses are in
[references.md](references.md).

## Architecture and integration

- Provider crates depend on `provider-api`, not `codex-core`.
- Core supplies host execution through the normal tool router, preserving
  Codex-owned approvals, sandbox policy, hooks, and user-facing tool events.
- `provider-auth` isolates credentials by provider/account. `provider-runtime`
  stores checkpoints and tool journals with provider/account/model/thread identity.
  One account is selected per provider; automatic rotation and fallback are
  not performed.
- OAuth uses PKCE/state, cancellable callbacks, atomic credential writes, and
  coordinated refresh. Interrupted host tools fail closed: resume requires an
  explicit branch after the caller inspects possible effects.
- CLI provider commands, configuration, account RPCs, model discovery, and
  schemas are additive. Native thread inheritance supports fork and cold
  resume when the client omits a provider override.
- A thread-scoped execution lease preserves the uncertain-tool fence across
  provider/account/model changes. A deliberate fork receives a separate
  execution identity.
- Native provider protocols own their authentication, discovery, headers, and
  transport. Unsupported overrides, timeout settings, and retry behavior are
  rejected instead of silently ignored.

## Verification evidence

- **Core:** `/tmp/native-core-tests-ultimate.log` records 22 native-provider
  tests passing, including output-cap draining and per-thread model metadata
  isolation under an OpenAI-global app-server. Cross-namespace lease/checkpoint
  and provider regression tests are recorded in
  `/tmp/native-review-regressions.log`.
- **Provider packages:** the latest package snapshot recorded 198 passing
  tests. Cursor-specific validation recorded 26 Cursor tests and 29 codec
  tests in `/tmp/native-final-codec-cursor-tests.log`.
- **Responses:** `/tmp/native-responses-tests-final.log` records 33
  compatibility tests passing.
- **App-server:** `/tmp/native-appserver-tests.log` records three native
  account/fork tests passing: login/read/logout notifications, account
  isolation, env-token override rejection, and native fork/cold-resume
  provider inheritance.
- **Schema:** the native `Account`, `LoginAccountParams`, and
  `LoginAccountResponse` variants were regenerated into stable and
  experimental schema artifacts. `/tmp/native-schema-ultimate.log` records
  four passing fixture tests (the manual fixture writer is intentionally ignored).
- **Feature check:** the no-default-features check passed in
  `/tmp/native-feature-check.log`.
- **Final root gates:** the CLI build passed in
  `/tmp/native-cli-build-ultimate.log`. Strict clippy passed for all provider
  crates, the adapter, core, CLI, and app-server in
  `/tmp/native-clippy-ultimate.log`. Bazel and the full repository test suite
  were not run.

## Publication rebase

The `feature/native-rust-providers` branch was rebased onto upstream main at
`34f33e4ada8f417203d87f3b5858c97add8dad6b`. The rebase preserved provider
implementations, regenerated both compressed protocol export bundles, and
adapted native assistant messages to upstream's new optional response metadata.
Regenerating the Python SDK produced no additional changes. A final upstream
sync also incorporated `de8fab6d7adfcef8b4ce6f02f3b5c8be4092015a`, which upgrades
the MCP SDK to 3.5.1, through a clean merge after the initial branch push.
The CLI rebuilt successfully on this final upstream revision
(`/tmp/native-latest-upstream-build.log`), and the focused MCP connection-closure
test passed (`/tmp/native-latest-upstream-mcp-tests.log`).

- The rebased CLI build passed: `/tmp/native-rebase-build-final.log`.
- All 22 native core tests passed: `/tmp/native-rebase-core-tests.log`.
- All three app-server account/fork tests passed:
  `/tmp/native-rebase-appserver-tests.log`.
- All 229 provider and adapter tests passed:
  `/tmp/native-rebase-provider-tests.log`.
- All 33 Responses tests passed serially: `/tmp/native-rebase-responses-tests.log`.
  The initial parallel run had one failure in the existing
  `response_stream_records_last_model_feedback_ids` tracing test; it passed in
  isolation and in the serial suite. The initial failure is retained in
  `/tmp/native-rebase-responses-parallel.log`.
- All four schema fixture checks passed: `/tmp/native-rebase-schema-tests.log`.
  Formatting and diff checks also passed.
- Strict clippy for the provider, adapter, core, CLI, and app-server crates
  passed: `/tmp/native-rebase-clippy.log`. The adapter also compiled with
  default provider features disabled: `/tmp/native-rebase-feature-check.log`.

## Publication credential handling

GitHub push protection rejected the initial publication because the Antigravity
reference implementation's OAuth application client ID and secret were embedded
in source history. These are now runtime environment configuration
(`ANTIGRAVITY_OAUTH_CLIENT_ID` and `ANTIGRAVITY_OAUTH_CLIENT_SECRET`), and the
literals are removed from the unpublished branch history. OAuth debug output
also excludes application credentials and parameter values. Existing account
tokens were not committed. See [usage.md](usage.md) for the required setup.

The affected auth/provider suites passed 127 tests after this change
(`/tmp/native-publication-oauth-tests.log`), and the CLI rebuilt successfully
(`/tmp/native-publication-build.log`). A live check confirmed missing app
configuration leaves the saved account unchanged, while the external
configuration supports refresh, discovery, and exact text generation without
changing account identity (`/tmp/native-publication-live.log`).
Strict clippy and the feature-disabled adapter check passed again in
`/tmp/native-publication-clippy.log` and
`/tmp/native-publication-feature-check.log`.

## Live provider acceptance

All checks used explicitly selected accounts in private test homes and did not
print credentials. The reusable app-server acceptance runner is
[`scripts/native_provider_acceptance.py`](../../scripts/native_provider_acceptance.py);
reproduction commands are in [usage.md](usage.md).

| Provider    | Verified live behavior                                                                                                                                                                                                                                                                                                                                       | Scope and remaining limits                                                                                                                                        |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Cursor      | Browser login, refresh, 256-model discovery, model changes, ordinary native read/write/grep/list/delete, shell/exec/stdin, MCP text/image, file errors, policy rejection, permission decline/accept, lifecycle and interruption recovery passed. Final MCP image tests used the production-default catalog; historical user/tool image branches also passed. | Linux full-runtime checks. Background shells, binary native writes, and unsupported multimodal resources remain explicit errors.                                  |
| OpenCode Go | Chat `glm-5.3`, Messages `minimax-m3`, and Responses `gpt-6-luna` passed two initial exec/write/read rounds plus one resumed read. API-key login/discovery, model change, approvals, branch/compaction, interruption/recovery, and Chat/Responses vision passed. Go Messages view-image passed through the full Codex core.                                  | Selected account/model coverage. The original Go key had exhausted its monthly quota, so tests explicitly chose the available pool account; no fallback was used. |
| Claude      | Browser login/refresh, 14-model discovery, reasoning, view-image, model change, live tool rounds/resume, approvals, branch/compaction, active-tool interruption/recovery, and TUI status/provider/account/model display passed.                                                                                                                              | Selected models/accounts; no automatic account rotation.                                                                                                          |
| Antigravity | Browser login/refresh, 26-model discovery, Gemini and Claude-family model changes, full-core image checks, live tool rounds/resume, approvals, branch/compaction, and active-tool interruption/recovery passed.                                                                                                                                              | Selected models/accounts; broader provider error and entitlement coverage remains open.                                                                           |

For all four providers, model changes passed. All four also passed a clean
branch+compaction check without an explicit fork provider override. Active host
tool cancellation followed by restart and model change stayed fenced, then an
explicit branch continued without replay. Completion markers for the canceled
45-second commands remained absent after the timeout windows.

Full ordinary-tool and resume results are in
`/tmp/native-codex-services-final-ve861xmx/summary.json`. Interruption results
are in `/tmp/native-interruption-{opencode-go,claude}.log`,
`/tmp/native-interruption-antigravity-recovery.log`, and
`/tmp/native-harness-interruption-cursor.log`. Full-core image receipts are
listed in the [coverage matrix](coverage-matrix.md).

## Limits and follow-up

- Live coverage is Linux-only for native filesystem and shell execution. It
  does not establish Windows/macOS command availability or every sandbox
  policy branch.
- Cursor drains its upstream stream for 500 ms after `turnEnded` when the
  service keeps HTTP/2 open. There is no verified final-checkpoint marker;
  metadata arriving after this grace period can be missed. This bounded
  protocol assumption and its delayed-frame regression are documented in the
  [Cursor crate](../../codex-rs/providers/provider-cursor/README.md).
- Binary native writes, audio/resource inputs, remote-image resources, and
  unsupported model/protocol combinations return explicit errors.
- An optional baseline code-mode host build failed because the upstream V8
  sandbox archive returned 404. Native providers use Codex's ordinary host and
  do not depend on that optional binary; retain the normal default sandbox.
- Auth, model, and service checks cover selected accounts and models. Provider
  entitlements, account-wide rotation, and catalog-wide compatibility are not
  claimed. The listed acceptance scenarios pass; untested combinations remain
  outside the validated scope.
