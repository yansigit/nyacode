# Automatic native account pools

This feature branch starts at the published `34b445493` baseline. Existing
credentials, generation identities, checkpoint version 1, tool journals,
namespace hashes, and execution-lock keys remain compatible. Pool state is
additive under the private native-provider directory.

The implementation keeps selection and handoff in provider-runtime, credential
coordination in provider-auth, failure evidence and portability in each provider,
and host translation in provider-adapter. Callers use registry metadata and
provider IDs. Core's broad inference retry remains disabled.

## State and concurrency

The account index records known labels and generations. Legacy selected accounts
are imported lazily; known unselected labels can be enrolled explicitly. Login
operations reserve ownership before OAuth starts. Credential commits and logout
hold only the affected stable credential lock. An interrupted credential/index
commit is recoverable, and pool enrollment has a generation-bound commit intent.
Late callbacks cannot resurrect logged-out or replaced credentials.

Pool enablement, manual membership, credential condition, and scoped availability
are independent. The second enrolled member activates pooling; explicit disable
and prior activation persist. Settings revisions reject stale managers. Recovery
uses a nonblocking per-restriction lease for a real request, never an inference
queue or a synthetic request. Observation revisions prevent old successes from
clearing newer failures. Credential hashes and known upstream account/workspace/
project identities join shared quota restrictions.

Handoffs retain the existing thread execution lease and all prior tool-journal
namespaces. A new attempt is durably marked before dispatch. The additive dispatch
fence covers provider/account/model/window changes. A conclusive rejection or
persisted completion clears it; an unknown outcome requires explicit recovery.
The existing interrupted-tool fence is never cleared by selecting another account.

HTTP tool results stop at an explicit continuation boundary. The ordinary host
turn loop flushes the actual recorded history before acknowledgement. Ephemeral
history cannot authorize a mid-turn handoff. Compaction and other auxiliary
consumers do not claim this acknowledgement. Old transports are cancelled before
replacement, and logout cancellation follows the current attempt's credentials.

## Provider boundaries

| Provider / route            | Failure evidence                                                                                                                   | Cross-account history                                                          |
| --------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| Cursor Connect              | Allowlisted HTTP/Connect auth, request, and outage codes; ambiguous resource exhaustion is unknown                                 | Initial rejection and ordinary completed-turn input; mid-turn refusal          |
| Go Chat Completions         | Gateway GoUsageLimitError, MonthlyLimitError, UserLimitError, CreditsError, AuthError; IP/capacity RateLimitError is provider-wide | Ordinary history                                                               |
| Go Messages                 | Same gateway evidence, independent Messages transport                                                                              | Ordinary history; signed thinking refused                                      |
| Go Responses                | Same gateway evidence, independent Responses transport                                                                             | Ordinary understood output; opaque reasoning refused                           |
| Claude Messages             | Documented authentication_error, rate_limit_error/spend caps, overloaded_error, invalid_request_error                              | Ordinary history; thinking/signatures refused without cross-account acceptance |
| Antigravity Gemini / Claude | Google RPC ErrorInfo domain/reason and RetryInfo; unknown 429 is not enough                                                        | Initial rejection; unfamiliar signed/opaque replay refused                     |

Raw errors and echoed prompts are not persisted. Classifiers read at most 64 KiB
and retain closed diagnostic codes, scope, and retry/reset deadlines. Deadline
fallbacks are recheck opportunities, not purported quota reset times.

Evidence sources: [Claude error contract](https://platform.claude.com/docs/en/api/errors),
[OpenCode gateway implementation](https://github.com/anomalyco/opencode/blob/dev/packages/console/app/src/routes/zen/util/handler.ts),
and [Google RPC error details](https://github.com/googleapis/googleapis/blob/master/google/rpc/error_details.proto).
Sanitized fixtures are synthetic examples of these allowlisted envelopes; they
must not be described as captured production failures.

## Verification

The integration manifest adds pool selection/recovery, account concurrency,
fifth-provider handoffs, CLI management, and RPC compatibility to the existing
hosted Linux suite and upstream probe. The fifth provider exercises the generic
runtime/backend without adding a provider wire enum or caller branches.

The [local check receipt](account-pools-checks.json) records the consolidated
suite, strict host linting, affected Bazel targets, Python SDK contracts, and an
isolated CLI smoke test. The CLI test covers token input, JSON redaction,
automatic activation, selection, ordering, enablement, removal, enrollment,
and named logout. Hosted workflow and upstream-probe commands are extended through
the shared manifest; the remote jobs were not dispatched during this local run.
All 29 local check stages have passing results, with the schema stage rerun after
regenerating the experimental export bundle. The receipt preserves the initial
schema failure and its passing rerun.

Deterministic coverage includes second-account activation, persistent disable,
priority and affinity, duplicate quota groups, model/endpoint separation,
coordinated probes, stale observations, generation/login/logout races, recorded
results, prior call-ID replay/conflicts, dispatch crash fences, legacy state,
redaction, and revision checks. Provider fixtures and existing transport tests
cover the different HTTP routes and conservative signature refusal.

The opt-in `provider-adapter/tests/live_pool.rs` harness wraps real transports.
Each scenario injects one rejection before dispatch in a test binary; production
builds contain no failure-injection path. Initial-rejection and recorded-tool
scenarios use natural inference around that injection. The tool fixture persists
and acknowledges its actual result, then verifies either a single handoff with
no repeated call or an explicit signature refusal. It requires a dedicated home with exactly two
enabled, authorized pool members:

```sh
CODEX_POOL_ACCEPTANCE_HOME=/private/acceptance-home \
CODEX_POOL_ACCEPTANCE_PROVIDER=opencode-go \
CODEX_POOL_ACCEPTANCE_MODEL=glm-5.3 \
cargo test --manifest-path codex-rs/Cargo.toml -p codex-provider-adapter \
  --test live_pool -- --ignored --nocapture
```

## Live acceptance, 2026-10-10

The [sanitized acceptance receipt](account-pools-live-acceptance.json) records
results and log hashes. Two saved accounts were used for each authorized provider.
All six routes below passed initial rejection failover, restart affinity, real
host tool execution, fork, compaction, active-tool cancellation, restart fencing,
explicit branch recovery, and approval allow/deny. Cursor's compaction assertion
failed once on the model's final answer and passed on one repeat; the persisted
summary contained the expected content in both runs.

| Provider / model                    | Recorded-tool cross-account continuation  |
| ----------------------------------- | ----------------------------------------- |
| Cursor / gpt-5.6-luna-none          | Unsupported; deterministic refusal        |
| Go Chat / glm-5.3                   | Explicit thinking-history refusal         |
| Go Messages / minimax-m3            | Accepted; completed tool was not repeated |
| Go Responses / gpt-6-luna           | Accepted; completed tool was not repeated |
| Antigravity / gemini-3.5-flash-lite | Explicit signed/opaque-history refusal    |
| Antigravity / claude-sonnet-4-6     | Explicit signed/opaque-history refusal    |

MiniMax required a concrete tool argument before it exercised the boundary.
An exploratory GLM request disabling reasoning was rejected and was not retried;
[GLM-5.3 requires thinking](https://docs.z.ai/guides/capabilities/thinking-mode).
The implementation keeps its original reasoning settings and refuses unverified
thinking portability.

Claude's native endpoint has deterministic fixtures and protocol coverage, but
no new authorized two-account live run. Natural quota exhaustion/reset recovery
was not forced. Windows/macOS execution, live logout during refresh, and concurrent
mixed-version writers remain verification gaps. Existing published baseline
receipts are not evidence for these new pool boundaries.
