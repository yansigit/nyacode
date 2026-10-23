# Native providers

The CLI recognizes `cursor`, `opencode-go`, `claude`, and `antigravity` as
native provider IDs. They use the ordinary model-provider setting and model
selection:

```toml
model_provider = "claude"
model = "claude-haiku-5-5"
```

Custom provider entries may use the generic native protocol with a registered
provider ID:

```toml
model_provider = "my_cursor"
model = "gpt-5.6-luna-none"

[model_providers.my_cursor]
name = "Cursor native"
wire_api = "native"
native_provider = "cursor"
```

The existing `cursor`, `opencode-go`, `claude`, and `antigravity` wire values
remain accepted and retain their serialized spelling. A legacy wire value may
only be paired with its matching `native_provider`. The generic form requires
a nonempty registered ID; `native_provider` cannot accompany `responses`.
An installed build may recognize a provider whose transport feature was omitted;
attempting to use it reports that it was not compiled into the binary.

Use a model ID printed by `nyacode provider models <provider>`. Discovery reports
the catalog visible to the selected account; it is not an entitlement check.
Availability and supported features can vary by account and model.

## Login and account selection

The provider commands are:

```text
nyacode provider login <provider> [--account <name>] [--with-token] [--add]
nyacode provider status [<provider>]
nyacode provider logout <provider> [--account <name>]
nyacode provider models <provider>
nyacode provider accounts <provider> [--json]
nyacode provider pool <provider> <action>
```

For an OAuth provider, start its browser flow and inspect status or available
models after it finishes:

```sh
nyacode provider login claude
nyacode provider status claude
nyacode provider models claude
```

Antigravity requires the OAuth application's registration credentials in the
CLI or app-server process environment, including when using a stored account:

```sh
export ANTIGRAVITY_OAUTH_CLIENT_ID="<OAuth application client ID>"
export ANTIGRAVITY_OAUTH_CLIENT_SECRET="<OAuth application client secret>"
nyacode provider login antigravity
```

Load the real values from a secret manager or a protected environment file
outside the repository. They identify the OAuth application, not your account's
access or refresh token. Use the same registration that issued any existing
refresh token, with the callback `http://localhost:51121/oauth-callback`.
Missing configuration produces an error before credentials are refreshed;
application credentials are excluded from debug output.

OpenCode Go accepts an API key from stdin. Keep the key in a protected file or
pipe it from a secret manager; do not put it in a command argument:

```sh
nyacode provider login opencode-go --account work --with-token \
  < /path/to/protected-token-file
nyacode provider status opencode-go
```

`login` selects the named account (default: `default`). `--add` saves and enrolls
it without changing the manually selected account. Enrolling a second account
automatically activates pooling for that provider and endpoint. An explicit
`disable` persists across subsequent logins and membership changes. Removing
accounts does not silently deactivate a pool.

```sh
nyacode provider login cursor --account personal
nyacode provider login cursor --account work --add
nyacode provider accounts cursor --json
nyacode provider pool cursor order personal work
nyacode provider pool cursor disable-account personal
nyacode provider pool cursor prefer work
nyacode provider pool cursor disable
nyacode provider pool cursor enable
nyacode provider pool cursor enroll previously-saved-label
nyacode provider pool cursor remove personal
nyacode provider logout cursor --account personal
```

Pool actions are `status [--json]`, `enable`, `disable`, `enroll <label>`,
`remove <label>`, `enable-account <label>`, `disable-account <label>`,
`order <all labels in priority order>`, and `prefer <label>`. Removal preserves
credentials; named logout deletes credentials and fences outstanding callbacks.
Older unselected hashed credential files are enrolled by their known label;
they are never guessed or deleted.

New conversations try eligible accounts in priority order. Existing conversations
retain their working account, including after a higher-priority account recovers.
The selected account pointer is never changed by automatic failover. Model,
tools, reasoning, provider, endpoint, and billing configuration stay fixed.

Use `/providers` to open the interactive account manager. It supports OAuth
login/cancellation, masked token entry, enrollment, ordering, enablement,
preferred selection, removal, and logout. Availability and reset/recheck times
are shown beside accounts. OAuth URLs can be opened in the browser. The same
backend powers CLI commands and `provider/accounts/list`, `provider/pool/update`,
and `provider/account/logout` RPCs. RPC updates require the last observed revision;
thread context resolves the existing endpoint configuration. Native login accepts
optional `select` (default true) and `threadId` fields. Pool and thread account
change notifications contain labels and state, never credentials.

A verified account-specific rejection can switch accounts only at a supported
continuation boundary. HTTP tool rounds wait for actual persisted history and
completed tool journals. Signed thinking and opaque histories are refused when
cross-account validity has not been established. Cursor supports initial rejection
and ordinary completed-turn history; mid-turn handoffs remain disabled.
Timeouts, broken streams, and unknown outcomes stop automatic continuation.

Known retry/reset deadlines are honored. Otherwise temporary limits use a
60-second recheck, doubling up to 15 minutes; exhausted quota uses 15 minutes,
doubling up to six hours. These are next eligible attempts, not claimed resets.
Only one process may use the next real request as a recovery probe. There are no
synthetic probes, background retry loops, or waiting for exhausted pools.
The error lists account reasons and recovery actions. Login-required credentials
remain unavailable until repaired. Shared upstream quota identities and duplicate
keys share restrictions.

## Credential source and configuration

Credentials can come from the native provider login store or from a configured
bearer token/environment variable. A configured token is authoritative for
requests. Login and logout are refused while the selected provider has a token
override, so remove that override before changing the stored login. For example,
OpenCode Go's built-in provider reads `OPENCODE_API_KEY`; while it is set,
`nyacode provider status opencode-go` reports a configured token and
`nyacode provider logout opencode-go` asks you to remove the override.

Built-in provider entries are not overridden by a same-named TOML table. If you
need an environment key for a native protocol, define a separate provider ID
with that native `wire_api`:

```toml
model_provider = "claude_from_env"
model = "claude-haiku-5-5"

[model_providers.claude_from_env]
name = "Claude native from environment"
wire_api = "claude"
env_key = "CLAUDE_API_KEY"
```

The native protocol still identifies as `claude`, so use the
`nyacode provider login claude`, `nyacode provider status claude`,
`nyacode provider logout claude`, and `nyacode provider models claude` commands for
the native account store.
When `CLAUDE_API_KEY` supplies the effective token, login/logout of stored
Claude credentials are disabled until it is unset. Avoid putting secrets
directly in `config.toml`; use an environment variable or the stdin login path.

Native providers own their authentication, discovery, headers, and transport.
Unsupported auth/header/catalog overrides and per-provider stream/connect
timeout settings are rejected. Request and stream retry settings must be zero;
the provider runtime owns the bounded, evidence-based account failover above.
Resume is explicit after interruption. Do not set nonzero
`request_max_retries` or `stream_max_retries`, or set
`stream_idle_timeout_ms` or `websocket_connect_timeout_ms`, for a native
provider.

## Private state and interrupted execution

Native credentials, refresh metadata, checkpoints, tool journals, and
interruption markers are stored under
`$NYACODE_HOME/native-providers/` (or the default Codex home when `CODEX_HOME` is
unset). Filenames are derived from hashed provider/account/session keys. On
Unix, the state directory is private and state files are owner-readable only.
Pool settings, account indexes, availability, affinity, and handoff journals are
additive versioned private records. An older build ignores these records and
retains the existing credentials/checkpoints/tool-journal formats.
This state is separate from ChatGPT's `auth.json`; do not edit or remove it
while Codex is running.

Session keys and history bindings keep the original Cargo JSON representation:
object keys are sorted recursively before hashing. Cursor context hashes retain
typed message field order and sort only embedded JSON objects. This makes stored
state compatible across Cargo and Bazel builds, including builds that enable
`serde_json/preserve_order`. Normal provider request payloads are unchanged;
Cursor bootstrap history blobs use the same normalization because their JSON
bytes determine their content identifiers.

If a native request is interrupted after a host tool may have started, Codex
refuses automatic replay. Inspect the local and external effects first, then
explicitly fork the conversation before continuing:

```sh
codex fork <session-id>
```

The recovery fence is thread-scoped. Resuming the same thread with another
model or provider account does not clear it. A new explicit branch has a
separate execution history; keep the original thread for review. Interruptions
before any tool starts can use the saved clean checkpoint, but once tool
execution begins its effects may be uncertain.

## Cursor native file and search operations

On Linux, Cursor native file read/write and search operations are available
when their required local executables are present and the request advertises
the normal `exec_command` and `write_stdin` tools. Codex translates them into
its existing shell-tool path, so normal Codex execution policy applies. Live
read/write, `ls`, `grep`, and delete passed in the full Codex runtime. The
[coverage matrix](coverage-matrix.md) records the exact tested paths and
platform limits.

## Reproducing live acceptance

[`scripts/native_provider_acceptance.py`](../../scripts/native_provider_acceptance.py)
runs the lifecycle, interrupted-tool recovery, or approval allow/deny check
against an existing `CODEX_HOME`. It uses the account already selected in
that home; it does not perform login, logout, account rotation, or fallback.
The checks create unique temporary workspaces and only print provider/thread
and pass/fail details.

```sh
CODEX_BINARY="$PWD/codex-rs/target/debug/codex"
ACCEPTANCE_HOME="/path/to/private-acceptance-home"

python3 scripts/native_provider_acceptance.py \
  --codex "$CODEX_BINARY" --codex-home "$ACCEPTANCE_HOME" \
  --provider claude --check lifecycle
python3 scripts/native_provider_acceptance.py \
  --codex "$CODEX_BINARY" --codex-home "$ACCEPTANCE_HOME" \
  --provider antigravity --check interruption
python3 scripts/native_provider_acceptance.py \
  --codex "$CODEX_BINARY" --codex-home "$ACCEPTANCE_HOME" \
  --provider opencode-go --check permissions
```

The interruption check intentionally runs a temporary 45-second foreground
command, interrupts it, restarts the app-server, checks the model-change fence,
and recovers through an explicit branch. The permission check declines then
accepts only its exact temporary-file write through the normal approval RPC.
Run against a private acceptance home with a previously selected account, not
the default user home. The checked-in runner has live receipts for Go
lifecycle, Antigravity interruption recovery, and Claude permissions; direct
acceptance runs cover these scenarios across all four providers. Details and
receipts are in the [coverage matrix](coverage-matrix.md).

## Supported limits

Full filesystem/shell acceptance is currently verified on Linux. Cursor uses
Codex's normal host path. When the upstream stream stays open after `turnEnded`,
a bounded 500 ms drain collects late checkpoint updates; metadata arriving
after that period can be missed. Background shells are unsupported. Binary native
writes, audio/resource input, remote-image resources, and unsupported
model/protocol combinations return explicit errors. Ambiguous provider requests do not
automatically retry or switch accounts. Model capability and live acceptance
are account- and route-specific; see the [coverage matrix](coverage-matrix.md).

See the [pinned reference inventory](references.md) for source revisions and
the [evidence matrix](coverage-matrix.md) for current local and live acceptance
limits. The published baseline verification is separate from account-pool acceptance;
see [account-pools.md](account-pools.md) for current verification and gaps.
