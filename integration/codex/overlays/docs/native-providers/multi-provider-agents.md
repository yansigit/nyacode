# Native providers and complex agent tools

`spawn_agent` accepts an optional `model_provider` in both collaboration tool
versions. It resolves an existing configured provider ID, including aliases,
and validates the explicit model against that provider's catalog. Omitting the
provider keeps the parent provider. A provider change requires a fresh child:
`fork_turns: "none"` in V2 or `fork_context: false` in V1.

For example, a parent on Cursor can request this V2 child:

```json
{
  "task_name": "check",
  "model_provider": "opencode-go",
  "model": "glm-5.3",
  "fork_turns": "none",
  "message": "Inspect the requested files and report your findings."
}
```

The child retains the parent's effective workspace and permission restrictions.
It uses the target provider's model metadata, credentials, and account pool.
Parent-specific catalog, context limits, and default child model settings do not
leak into the target provider. Reloading a child restores its recorded provider.
Each thread has its own account affinity; automatic account failover stays within
that thread's provider and endpoint. Provider selection is an explicit spawn
choice, not a fallback for an unavailable model or account.

Plaintext V2 task, follow-up, and completion messages are translated in the native
adapter as attributed conversational input. Native tool schemas omit the
Responses-only encrypted-argument annotation, and returned calls carry explicit
plaintext metadata. Unexpected encrypted Responses arguments and encrypted agent
messages are refused.
Full-history forks across providers are also refused: neither signed thinking nor
opaque history is silently rewritten to fit another provider.

Native sessions defer incoming agent mail until durable session completion; they
do not abandon an in-flight provider request to deliver a mailbox update. Mail is
then included in the next request. During a session, `list_agents` supplies current
worker status and completed results. Explicit interruption still preserves the
existing uncertain-execution fence and recovery requirement.

Native Responses function tools explicitly retain optional schema arguments.
Cursor accepts the complete collaboration catalog within a bounded 512 KiB local
serialization limit. Descriptions and parameter schemas are not truncated.
Antigravity expands local schema references with depth and allocation limits;
remote, recursive, unresolved, and conflicting references remain explicit errors.

## Reproducible checks

The shared [integration manifest](integration-manifest.json) includes child
provider resolution, collaboration tool schemas and catalog overrides, adapter
message translation, and provider codec fixtures. The pooled-turn regression
combines out-of-order parallel results, an approval denial, a conclusive rejection,
and cross-account continuation. Repeated call IDs reuse recorded results; a new
ID with identical command text still executes.

The opt-in live runner uses the built app-server and existing private credentials:

```sh
python3 scripts/native_provider_multiagent_acceptance.py \
  --codex "$PWD/codex-rs/target/debug/codex" \
  --codex-home "$NYACODE_HOME" \
  --provider cursor --model gpt-5.6-luna-none \
  --child opencode-go:glm-5.3 \
  --child antigravity:claude-sonnet-4-6 \
  --receipt /tmp/native-multiagent-receipt.json
```

Every participant must have an enabled managed pool. The runner verifies saved
child ownership, provider/model routing, effective account labels, command records,
exact temporary file contents, chained tools, deliberate exit-code failure,
follow-up delivery, and parent aggregation. It never accepts an agent's final
claim as sufficient evidence. Add `--lifecycle` to exercise child approval
decline/accept, child turn interruption, and explicit background-terminal
termination. The harness approves only its exact temporary-file command,
workspace, and child thread.

`exec_command` creates a resumable process. `interrupt_agent` stops the child's
turn; the process deliberately survives until explicitly terminated or the session
shuts down. The lifecycle runner uses `thread/backgroundTerminals/terminate` for
the exact fixture process, then waits past its delayed-write deadline to verify
that it stopped. Interruption and process termination are separate checks. A
rejected approval can have no command item because process creation never occurred;
the harness requires the matching child approval request and absence of its write.

Fixture threads disable connected apps and plugins by default. `--with-apps`
advertises them too, but Antigravity currently refuses some app schemas (including
schema-valued `additionalProperties`) rather than weakening their constraints.
The new home can contain ChatGPT credentials alongside native credentials, making
those app tools visible even when a native model is selected. This is an explicit
compatibility gap; local-tool acceptance does not establish arbitrary MCP/app-tool
support.

These are real model requests and can consume quota. The runner neither injects
quota failures nor changes pool membership. Separate test-only transport wrappers
cover the handoff boundaries described in [account-pools.md](account-pools.md).
Passing representative workflows does not establish every model, tool, operating
system, or simultaneous quota-failure combination.

## Recorded acceptance, 2026-10-11

Successful live workflows used managed pools in the private `.nyacode` home:

| Parent                                 | Children                                                             | Effective account labels                |
| -------------------------------------- | -------------------------------------------------------------------- | --------------------------------------- |
| Cursor `gpt-5.6-luna-none`             | Go Chat `glm-5.3`, Antigravity Claude `claude-sonnet-4-6`            | `house`, `opencode_go_1`, `adn`         |
| Go Responses `gpt-6-luna`              | Go Messages `minimax-m3`, Antigravity Gemini `gemini-3.5-flash-lite` | `opencode_go_1`, `opencode_go_1`, `adn` |
| Antigravity Claude `claude-sonnet-4-6` | Go Chat `glm-5.3`, Go Responses `gpt-6-luna`                         | `adn`, `opencode_go_1`, `opencode_go_1` |

The Antigravity parent also passed child approval decline/accept, turn
interruption, and explicit terminal termination. The delayed write remained absent
after its deadline.

The [live receipts](multi-provider-agents-live.json) retain failed attempts as well
as successful runs. Early attempts exposed adapter and mailbox bugs fixed in this
change. Later model errors included shell quoting, a mistyped path, concurrent
execution of dependent commands, and a parent doing work assigned to children.
The strict fixture rejected these even when the final answer claimed success.
Simplified commands and explicit dependency instructions improved reproducibility;
a successful representative run is not a reliability guarantee for every model.

The [deterministic check receipts](multi-provider-agents-checks.json) include the
parallel-tool handoff regression, child routing and reload, mailbox ordering,
schema compatibility, architecture boundaries, lint, feature builds, and Bazel.
The shared manifest is consumed by the existing Linux workflow and upstream probe;
those remote jobs were not dispatched in this local run.

These workflows did not trigger live account exhaustion or combine live failover
with active children. The existing [pool acceptance](account-pools-live-acceptance.json)
uses test-only conclusive rejection injection around real transports; the new
parallel handoff test is deterministic. Native Claude has no new two-account
multiagent acceptance; Claude through Antigravity is a separate route.

Cursor native shell requests above the host's one-hour timeout limit remain
explicitly rejected; the fixture uses `exec_command`. Connected app schema
restrictions, encrypted messages, cross-provider full-history forks, and unsafe
mid-turn account handoffs retain the refusal boundaries described above and in
[account-pools.md](account-pools.md).
