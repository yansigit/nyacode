# Native provider evidence matrix

This matrix separates local tests, provider-account checks, and product-level
acceptance. A passing check covers only the named path, account, and model;
fixtures do not prove behavior against a provider service. Pinned upstream
evidence is catalogued in [references.md](references.md). Reproduction commands
for the reusable app-server checks are in [usage.md](usage.md).

## Automatic account pools

The additive pool implementation and its new evidence are tracked separately in
[account-pools.md](account-pools.md). Earlier live receipts below cover the
published single-account baseline, not cross-account continuation.

## Cross-provider children and complex tools

[Multi-provider agents](multi-provider-agents.md) describes explicit fresh-child
provider selection and the complex-tool acceptance runner. Account-pool and
single-agent receipts do not by themselves establish cross-provider collaboration.

## Compiled boundary refactor

Fresh results and exact receipt paths are in
[refactor-validation.md](refactor-validation.md). All four providers passed
new live discovery, lifecycle, approval, interruption, restart fencing, and
explicit recovery checks. Seven CLI model routes passed tool execution and
clean resume, including all three Go protocols and both Antigravity families.
The [integration manifest](integration-manifest.json) maps retained hooks to
regression groups; fixtures, architecture controls, and a fifth-provider test
cover the new boundary.

## Published implementation baseline

The detailed counts and receipts below are the published baseline. They are
kept for comparison and do not substitute for the fresh refactor results above.

Implementation and regression entry points: [native core routing](../../codex-rs/core/src/native_provider.rs),
[adapter tests](../../codex-rs/providers/provider-adapter/src/session.rs),
[host-tool translation](../../codex-rs/providers/provider-adapter/src/host_operations.rs),
[shared driver tests](../../codex-rs/providers/provider-runtime/tests/driver.rs),
[auth security tests](../../codex-rs/providers/provider-auth/tests/security.rs),
[codec tests](../../codex-rs/providers/provider-codecs/src/tests.rs),
[Cursor transport tests](../../codex-rs/providers/provider-cursor/src/tests.rs),
[native account app-server tests](../../codex-rs/app-server/tests/suite/v2/native_account.rs),
[thread inheritance tests](../../codex-rs/app-server/src/request_processors/thread_processor_tests.rs),
and [schema fixture tests](../../codex-rs/app-server-protocol/src/schema_fixtures_tests.rs).

| Area                                     | Local and integrated evidence                                                                                                                                                                                                                                                                                                    | Live evidence                                                                                                                                                                                                                                                                                                                      | Remaining limits                                                                                                                                                                                 |
| ---------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| CLI, selection, configuration            | Native CLI, config, account RPCs, and additive schemas are implemented. App-server native account tests: 3 passed. No-default-features check passed.                                                                                                                                                                             | All four providers completed account login/discovery, model change, selected tool calls, approval allow/deny, branch/compaction, and interruption recovery. Claude TUI `/status`, provider/account/model display, and model listing passed in a real PTY.                                                                          | Final CLI build, schema fixtures, and strict clippy passed. Bazel and the full repository suite were not run.                                                                                    |
| Login, cancellation, credentials         | Shared API/auth regressions cover PKCE/state, duplicate state, exchange, cancellation, persistence, refresh coordination, revocation, provider/account isolation, and token-override ownership. App-server login/read/logout notifications, credential isolation, env override rejection, and native fork inheritance: 3 passed. | Browser login and refresh passed for Cursor, Claude, and Antigravity; OpenCode Go uses an API key. Discovery passed for all four. OAuth refresh preserves provider/account identity and handles rotation.                                                                                                                          | Runs cover selected accounts; there is no automatic account rotation or fallback. This does not prove every callback-failure or cancellation case.                                               |
| Model discovery and protocol families    | Codec/catalog tests cover model-specific routing and supported capabilities. Model-change checks cover all four providers.                                                                                                                                                                                                       | OpenCode Go Chat (`glm-5.3`), Messages (`minimax-m3`), and Responses (`gpt-6-luna`) each completed two initial exec/write/read tool calls and one resumed read. Claude Haiku and Antigravity Gemini/Claude-family models also passed.                                                                                              | Model and account coverage is representative, not exhaustive; provider quotas and entitlements vary.                                                                                             |
| Streaming, framing, cancellation         | Core has 22 native-provider tests; provider API/auth and transport suites cover fragmented/malformed streams, terminal states, result order, timeouts, cancellation, checkpoint validation, and interruption fencing. Responses compatibility: 33 passed.                                                                        | All four providers passed cancellation after a host tool started, process restart, model-change fence, and explicit-branch continuation. All 45-second completion markers remained absent after their waits.                                                                                                                       | Live cancellation is Linux-specific and covers selected tool/model routes. Continue testing provider-specific failures and timing boundaries.                                                    |
| Tools, approvals, sandbox                | Driver and translator fixtures cover serial/parallel calls, matching results, failure propagation, cancellation, checkpointing, and native read/write/grep/list semantics.                                                                                                                                                       | All four providers passed live approval decline and accept through Codex's normal host permission path. Denied commands did not run; accepted temporary writes matched their exact expected file contents. Cursor also passed ordinary exec/stdin, shell output, MCP text success/error, native file errors, and policy rejection. | Full native filesystem/shell acceptance was on Linux. Background shells, binary native writes, and other unsupported operations fail explicitly; this does not establish every OS/policy branch. |
| Cursor native file/search operations     | Cursor suite: 26 tests; codec suite: 29 tests. Translator temporary-fixture tests cover quoting, output caps, malformed/binary input, grep, and listing.                                                                                                                                                                         | Native read/write, `ls`, `grep`, delete, shell output, ordinary exec/stdin, and their filesystem/checkpoint results passed in the actual Codex CLI.                                                                                                                                                                                | Live acceptance is Linux-specific; platform-specific executables and boundaries still need separate checks.                                                                                      |
| Images and multimodal                    | Codec tests cover Messages, Responses, Chat, Gemini, Cursor typed MCP, and explicit unsupported formats. Production model catalogs gate image capability.                                                                                                                                                                        | Full Codex core correctly identified the tested red image through Claude, Go Chat, Go Responses, Go Messages view-image, Antigravity Gemini/Claude, Cursor native image, Cursor MCP image, and Cursor historical user/tool image branches.                                                                                         | These are selected models and a simple image fixture; audio, remote-image resources, and unsupported formats remain explicit errors. This does not imply general multimodal support.             |
| Checkpoints, restart, branch, compaction | Checkpoints are bound to provider/account/model/thread/history. Tests cover tool journals, edited history, cross-namespace isolation, endpoint leases, compaction, and interrupted-operation fences.                                                                                                                             | All four providers passed branch and compaction without explicit fork provider overrides, model changes, clean tool resume, and interrupted-tool restart/model-change fencing followed by explicit branch recovery.                                                                                                                | Continue signature/reasoning checks across additional models, accounts, and longer histories.                                                                                                    |
| App-server and TUI                       | Native account integration tests: 3 passed, including notifications and fork/cold-resume provider inheritance.                                                                                                                                                                                                                   | Claude TUI account/status/model presentation and discovery passed through a real PTY. The reusable app-server lifecycle, interruption, and permission runner checks passed live.                                                                                                                                                   | Other provider TUI presentations and interactive error cases are not established.                                                                                                                |

## Verification receipts

- `/tmp/native-codex-services-final-ve861xmx/summary.json`: all six selected
  provider/model routes passed two initial exec/write/read calls and one
  resumed read, with exact file contents and completion markers.
- `/tmp/native-interruption-{opencode-go,claude}.log`,
  `/tmp/native-interruption-antigravity-recovery.log`, and
  `/tmp/native-harness-interruption-cursor.log`: all four active-tool cancel,
  restart/model-change fence, and explicit branch checks passed.
- The checked-in acceptance runner was live-run for Go lifecycle, Antigravity
  and Cursor interruption recovery, and Claude permission decline/accept:
  `/tmp/native-harness-lifecycle-go.log`,
  `/tmp/native-harness-interruption-{antigravity,cursor}.log`, and
  `/tmp/native-harness-permissions-claude.log`. Direct live acceptance receipts
  cover the same scenarios for the other providers.
- `/tmp/native-core-tests-ultimate.log`: 22 core native-provider tests passed,
  including per-thread model metadata isolation and explicit catalog precedence.
- `/tmp/native-responses-tests-final.log`: 33 Responses compatibility tests
  passed. `/tmp/native-final-codec-cursor-tests.log`: Cursor 26 tests and codec
  29 tests passed.
- `/tmp/native-appserver-tests.log`: 3 account/fork tests passed.
- `/tmp/native-core-services-images-a0h8auhv/tool-proof.json` and
  `/tmp/native-core-services-images-hn3m7ymr/`: Go Messages view-image and
  Antigravity Gemini/Claude full-core image checks passed. Earlier Claude/Go
  Chat/Responses receipts are under `/tmp/native-core-services-images-per7emtn/`.
- `/tmp/native-cursor-core-acceptance/history-cli-final.log` and its
  `history-*/mcp-image-default-catalog` receipts: Cursor image checks passed
  with the production-default catalog.
- `/tmp/native-interruption-*.log` and `/tmp/native-codex-services-final-ve861xmx/`
  are private receipts outside the repository. Credentials and token-bearing
  logs are not committed.

## Final gates and boundaries

- Final CLI build, four schema fixture tests, strict clippy for affected
  provider/core/CLI/app-server crates, and the no-default-features check passed.
  Receipts are `/tmp/native-cli-build-ultimate.log`,
  `/tmp/native-schema-ultimate.log`, `/tmp/native-clippy-ultimate.log`, and
  `/tmp/native-feature-check.log`. Bazel and the full repository suite were not run.
- The package regression snapshot recorded 198 passing tests; see the ledger
  for scope and the limits of that snapshot.
- Live results cover only the named accounts, models, commands, and dates.
  Provider entitlement, broader platform support, and catalog-wide behavior
  remain unproven. No automatic account rotation or fallback is used.
- A baseline optional code-mode host could not be built because its upstream
  V8 sandbox archive returned 404. Native providers use the normal Codex host
  and do not depend on that optional binary; this does not justify weakening
  the default sandbox.
- Cursor uses a bounded 500 ms upstream drain after `turnEnded`; checkpoint
  metadata arriving later can be missed. See the
  [protocol assumption](../../codex-rs/providers/provider-cursor/README.md).
- The implemented paths have passed the listed local and live acceptance
  checks. Future model/platform combinations require their own validation.
