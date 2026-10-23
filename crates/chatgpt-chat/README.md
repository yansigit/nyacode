# Native ChatGPT

Status: **experimental, disabled by default**. Nyacode's independent **Codex | Chat**
surfaces support native ChatGPT history, saved and temporary conversations,
streaming, follow-ups, restart recovery, parallel conversations, and PNG, JPEG,
WebP, PDF, and UTF-8 attachments. App-server owns sessions and durable saved
submissions; temporary content stays in memory. The transport uses managed
authentication, Rust HTTP, and the bounded in-process Sentinel interpreter.

Enable `features.chatgpt_chat`, then restart Nyacode. Chat requires the owned
full-screen renderer and alternate screen; see the
[configuration example](../../docs/config.md). F6, `/chat`, and `/codex` switch
surfaces. Embedded app-server, stdio, and the trusted local daemon socket are
supported; TCP connections remain unsupported.

The [acceptance assessment](evidence/native-integrated-acceptance-2026-10-12.json)
maps the live embedded and daemon scenarios to their evidence, including all five
attachment formats, temporary continuation, cancellation, surface switching,
approvals, disablement, and reconnection without resending. The
[final platform regressions](evidence/native-msvc-final-platform-regressions-2026-10-12.json)
and [MSVC checks](evidence/native-msvc-accepted-transport-builds-2026-10-12.json)
cover nine target configurations on Rust 1.95.0. Linux x86_64 GNU and musl ran
tests; foreign targets establish compilation and linking, not native runtime
execution. Both MSVC architectures also passed static CRT import audits.
The [final validation record](evidence/native-release-validation-2026-10-12.json)
identifies the adopted sources, application checks, and retained limitations.

The sections below retain incremental implementation and research checkpoints.
Their outstanding-work statements describe those revisions; the acceptance
assessment above records the current release decision. The compatibility harness
remains read-only unless a write scenario is explicitly selected.

## Local service foundations

The protected daemon socket now assigns `ConnectionOrigin::LocalSocket` at the
server acceptor. TCP connections, including loopback and a spoofed client origin
header, retain `WebSocket`. Existing native user-verification key restrictions
still reject socket clients. The Chat service checks this origin before
accessing account data or local files. Raw network requests are rejected before
deserializing Chat parameters, so client-supplied origin claims cannot grant access.

The shared `AuthManager` supplies the service's principal/account/workspace and
authentication generation. Every saved session has an opaque scope UUID.
Authentication changes revoke sessions; disabling Chat synchronously cancels
work and clears native sessions, operations, and upload handles. Configuration
refresh uses global settings, independent of project/thread feature overrides.
Re-enabling creates a fresh scope while preserving saved drafts and conversations.

Native `SavedChatSession` exposes one-use preparations and asynchronous
`TurnSink` checkpoints. The service commits its SQLite transmission marker
before the native adapter may poll the message POST. A lease heartbeat keeps
ownership alive during streaming. Exact-ID reconciliation never prepares or
resubmits a user message. Local operations reject overlapping drafts or
conversations, allow up to eight concurrent independent turns, and retain up to
32 latest snapshots with scope, operation, and sequence information. A client
disconnect does not discard the server-owned operation.

Saved selections are snapshotted as a batch before remote upload. The service
retains validated file digests in a new draft revision before uploading, so
recovered files can be checked for changes. Ready handles must still match the
draft's selection identity, source, name, digest, and native transport session.
History reads retain a separate read-only path when recovery storage is
unavailable; saved sends and uploads require the durable profile.

The [live saved-service report](evidence/native-saved-service-2026-10-12-report.json)
completed one new `gpt-6-thinking` response, recorded acceptance and completion
in SQLite, and retrieved the exact fixture response and user-message ID from
ChatGPT. It also verified service disablement and TCP rejection. The
[source manifest](evidence/native-saved-service-2026-10-12-source.json) and
[validation record](evidence/native-saved-service-validation-2026-10-12.json)
retain the binary/source hashes and check results: all 400 app-server unit tests,
157 native Chat tests, and five harness tests passed. The 13 new service tests
and 157 native tests also passed in Bazel with dependency-lock verification;
Clippy passed with warnings denied. This is direct service acceptance; RPC and
TUI acceptance remain outstanding at that checkpoint.

## Experimental Chat RPCs

The twelve `chat/*` methods require `experimentalApi` negotiation and a trusted
server-assigned connection origin. Status returns an opaque scope and a process
local availability generation. Every subsequent operation must present that
scope. Authentication changes and feature disablement revoke the scope and live
capabilities. Reconnected clients read status and draft/conversation snapshots;
operation sequences are meaningful only within their scope and operation ID.
Clients discard older snapshots and events from revoked scopes.

Draft writes compare opaque revisions. `chat/turn/start` requires the flushed
revision, matching literal text, stable submission and client-message IDs, and
an expected parent. A successful RPC response only creates a local operation;
its acceptance remains unknown until native evidence and durable checkpoints
establish acceptance. `chat/conversation/read` can explicitly reconcile a
recorded submission without transmitting another user message. Unresolved saved
submissions also remain visible through draft reads after process restart.

Attachment upload accepts the complete selection for a submit. Explicit local
paths must match the draft. Clipboard base64 is bounded before database waits,
and decoded capacity is reserved before a worker validates image bytes. The
service snapshots and validates the entire batch before allocating remote files.
Progress snapshots include the updated draft revision and ready opaque handles.
A failed selection stays in the draft. Cancel reports local stopping/removal;
it never claims server cancellation. Notifications are targeted to subscribed
local clients, and responses and events recheck scope after queue backpressure.

Catalog entries are visibility metadata, not send entitlement. The backend init
response supplies the default model and any reported limits; absent usage data
remains unknown. A model becomes `sendVerified` only after an accepted, completed
operation with that resolved model in the current scope.

Temporary RPC requests use a separate owner without a database or saved journal.
Status creates only an account/workspace/auth-generation scope. Draft revisions,
submission identities, upload capabilities, and partial transcript snapshots
stay in memory and are revoked on identity changes, disablement, or shutdown.
Unknown acceptance keeps the submitted snapshot pending and prevents a resend;
accepted user messages survive subsequent generation failure. A continuation
uses the response-issued conduit and verifies the previous message IDs and
completed response before transmission. Missing or consumed capabilities fail
explicitly. Temporary requests cannot switch into saved mode for recovery.
The [live temporary RPC report](evidence/native-embedded-rpc-temporary-2026-10-12-report.json)
records two completed `gpt-6-thinking` turns, verified continuation, 21 ordered
updates, and matching memory snapshots. Saved retrieval rejected the temporary
conversation and recent history excluded it. Complete saved Chat table digests
and private saved journals were unchanged. The
[source manifest](evidence/native-embedded-rpc-temporary-2026-10-12-source.json)
identifies the binary and dirty source hashes; the
[validation record](evidence/native-embedded-rpc-temporary-validation-2026-10-12.json)
records 435 app-server, 171 native, and six harness tests plus Clippy. This proves
the embedded two-turn text scenario. Temporary attachments over RPC, the daemon,
and terminal acceptance remain outstanding.
Chat types redact `Debug`, Chat parse errors never echo submitted values, and
Chat notifications cannot enter Codex thread history. Experimental JSON and
TypeScript exports include these types; stable exports must exclude all of them.

The harness preserves its original read-only defaults. `--rpc-read` explicitly
exercises embedded status, native catalog, and history with
`--transport browser-emulated`. `--rpc-saved` additionally requires
`--write-scenario saved-text --preparation-protocol two-phase-v1` and creates one
harmless fixture, consumes streaming events, and retrieves authoritative IDs.
`--rpc-temporary` instead requires `--write-scenario temporary
--preparation-protocol two-phase-v1 --temporary-protocol conduit-refresh-v1`.
It attempts exactly two new fixture turns, verifies the memory snapshots and
follow-up, checks that a saved read fails and recent history excludes the
conversation, and compares all saved Chat database rows in memory. Its report
contains capability booleans and model names, never temporary IDs or contents.
`--rpc-attachments --write-scenario attachments --preparation-protocol two-phase-v1`
also requires `--attachment-fixtures` pointing to the generated corpus. It checks
all five fixture digests before any upload, records the expected metadata in
revisioned drafts, and then uploads each file through `chat/attachment/upload`.
Each turn must return the file's distinct code, appear in authoritative history
with its exact client-message ID and attachment metadata, and use the previous
turn's parent. The probe stops at the first failure without retrying a submission.
Report version 11 adds sanitized per-format RPC observations. This scenario is
separate from the earlier direct-transport upload evidence.

`--rpc-lifecycle --write-scenario saved-text --preparation-protocol two-phase-v1`
explicitly exercises runtime enablement, cancellation after acceptance, and
disablement during an accepted active turn. It creates two harmless saved
fixtures, retrieves their exact user-message IDs after re-enabling, rejects a
revoked scope, and verifies that a temporary draft disappeared. It restores Chat
to disabled at the end. An owned embedded server uses its normal runtime feature
configuration for this scenario, so an explicit CLI override cannot mask the
disable operation. A daemon must likewise permit runtime feature changes.

All RPC probes require `--live --current-account` (or the designated account
option). Other RPC scenarios also require `-c features.chatgpt_chat=true`.
They do not open the integrated release gate.

The [embedded saved-RPC report](evidence/native-embedded-rpc-saved-2026-10-12-report.json)
now verifies catalog and history reads, one completed saved `gpt-6-thinking`
response, 16 ordered stream updates, authoritative exact-ID retrieval, and a
cleared pending submission. Its [source manifest](evidence/native-embedded-rpc-saved-2026-10-12-source.json)
records the debug build and 8 MiB worker-stack setting. Chat dispatch runs outside
the large Codex request handler; a separate regression exercises status and
draft RPCs on a 2 MiB worker. The [validation record](evidence/native-embedded-rpc-validation-2026-10-12.json)
records 417 app-server, 162 native, 324 protocol, and five harness tests, plus
Clippy. At this saved-RPC checkpoint, embedded follow-up/restart, daemon,
temporary service, attachments over RPC, and TUI acceptance remained outstanding.
The subsequent temporary-RPC checkpoint is recorded above.

State migration `0062_saved_chat.sql` adds account/workspace-scoped profiles,
saved drafts, file-reference metadata, and immutable submission snapshots. Draft
edits and deletion compare opaque revisions. Submission IDs and client-message
IDs remain reserved after completion. Unique active-draft and conversation keys
serialize competing processes while allowing independent conversations.

Migration `0063_chat_cache.sql` adds saved conversations, ordered message nodes,
branch heads, and attachment metadata. Only successful authoritative saved reads
populate these tables. Replacement and eviction are transactional; late replies
cannot overwrite snapshots from later-started requests. Each account/workspace
cache is bounded to 128 conversations and 64 MiB, with an 8 MiB per-conversation
limit. Eviction never removes drafts or submission journals. Hidden message
contents, file bytes, download capabilities, and temporary conversations are
excluded. Cache failure does not change the authoritative read result.

`chat/conversation/read` accepts explicit `cacheOnly: true` for saved display
recovery. The response identifies its source as `authoritative`, `localCache`,
or `temporaryMemory`. Cached snapshots include their observation time and are
always blocked from continuation until refreshed. Cache reads cannot reconcile
a pending submission, and ordinary reads never silently fall back to the cache. The
[cache and PDF validation record](evidence/native-cache-pdf-validation-2026-10-12.json)
records 1,017 state/app-server/protocol tests, 179 native tests, all four Bazel
suites, and Clippy with warnings denied. It also identifies the exact dirty source
hashes and the reproducible parser artifact.

Chat writes require WAL and FULL synchronous durability on their actual database
connection. Only the initial lease owner can obtain a transmission permit, once,
after the transmission marker commits. Lease takeover always permits only
reconciliation, including after a crash before transmission. A newly assigned
conversation ID is journaled without claiming message acceptance. Unknown
acceptance remains pending after stream failure or local cancellation; exact
message and parent IDs are required to record acceptance. Accepted user messages
remain stored if generation fails, and later draft edits are preserved.

Only typed text, model, file references, digests, and metadata can reach these
tables. Clipboard references retain no bytes and require reattachment. Temporary
sessions must never obtain `SavedChatStore`; end-to-end temporary persistence
isolation remains part of service/TUI acceptance. This foundation has passed all
246 state unit tests, a documentation test, Clippy with warnings denied, and
Bazel tests with dependency-lock verification. The tests include separate
processes exiting without cleanup before and after the transmission marker,
interrupted transactions, competing database pools, and account isolation.

Native history now exposes paginated summaries and normalized active-branch
snapshots. The adapter validates graph identities, missing parents and cycles,
retains attachment/image metadata, and omits hidden message contents and storage
URLs. It explains why a conversation cannot be continued, including custom GPTs,
read-only or temporary state, unsupported modes, and unfinished generation.
Saved reads reject temporary contents before returning a cacheable snapshot.
Bounded citation markers and public HTTP(S) links are retained; other reference
formats become unsupported metadata. These APIs feed the harness and saved
service. The normalized-history checkpoint passed 153 deterministic tests in
Cargo and Bazel, with Clippy warnings denied. The native service adapter adds
asynchronous journal, cancellation, parent-conflict, and session-binding tests.

The [live normalized-read report](evidence/native-normalized-history-2026-10-12-report.json)
then returned 28 models, 20 native history summaries, and 25 active-branch
messages from the previously created attachment fixture. No message or upload
was sent; private recovery journals were unchanged. The
[source manifest](evidence/native-normalized-history-2026-10-12-source.json)
and [validation record](evidence/native-normalized-history-validation-2026-10-12.json)
distinguish this read acceptance from the outstanding integrated gate.

## Run the compatibility probe

From `codex-rs/`, print the capability matrix without loading configuration,
credentials, or making requests:

```sh
cargo run -p codex-app-server --example chatgpt_chat_probe
```

Explicitly designate the currently selected managed ChatGPT login:

```sh
cargo run -p codex-app-server --example chatgpt_chat_probe -- \
  -c features.chatgpt_chat=true --live --current-account
```

Explicitly exercise one new saved fixture through the app-server service and
SQLite journal, including authoritative retrieval and service disablement:

```sh
cargo run -p codex-app-server --example chatgpt_chat_probe -- \
  -c features.chatgpt_chat=true --live --current-account \
  --transport browser-emulated --preparation-protocol two-phase-v1 \
  --write-scenario saved-text --service-saved
```

This emits a sanitized `service` observation in report version 8. It never
selects an existing personal conversation, and it does not establish embedded or
daemon RPC/TUI acceptance. Its saved draft and submission stay in account-scoped
SQLite recovery storage; no conversation, draft, or submission IDs are printed.

Alternatively pass `--account-id ACCOUNT_ID` instead of `--current-account` to
require an exact workspace/account match. This never switches accounts. Use
`--codex-home /absolute/path` to select the existing managed login location and
`--conversation-id CONVERSATION_ID` to check a specific saved conversation's
active branch. No conversation is selected automatically from history.

Explicitly select write acceptance separately from the existing read-only run:

```sh
cargo run -p codex-app-server --example chatgpt_chat_probe -- \
  -c features.chatgpt_chat=true --live --current-account --write-scenario all
```

To compare transports, select one explicitly; neither transport falls back to
the other:

```sh
cargo run -p codex-app-server --example chatgpt_chat_probe -- \
  -c features.chatgpt_chat=true --live --current-account \
  --transport browser-emulated --write-scenario all
```

Use `--transport standard` for the control run. Omitting `--transport` preserves
the existing standard, read-only invocation. Selecting a transport alone never
enables preparation or writes. Write diagnostics now bootstrap before catalog or
history reads, and stop the run at the first failed preparation prerequisite.
An explicit challenge stops all further network requests in that session.

Scenario selectors are `preparation`, `saved-text`, `continuation`,
`restart-resume`, `temporary`, `attachments`, and `all`. These identify the
requested acceptance work; they do **not** imply the scenarios are implemented
or supported. With `--transport browser-emulated --preparation-protocol two-phase-v1`,
`saved-text` creates one harmless saved fixture and its private recovery journal.
`continuation` adds a correct-parent follow-up after successful retrieval.
`restart-resume --resume-run RUN_ID` reconciles the designated journal and then
continues it; it never replays the journaled submission. `all` currently runs the
saved scenarios only and leaves the remaining capabilities unverified. Other
combinations retain the diagnostic behavior. The `attachments` selector requires `--attachment-fixtures` pointing to the
generated corpus under `chatgpt-chat/fixtures/attachments`. It verifies every
file digest before upload, journals each saved message before transmission, and
requires the model to return the code contained only in that attachment. `--conversation-id`
cannot be combined with a write scenario, so a personal history selection cannot
accidentally become a write target. The temporary selector sends `true` for
`history_and_training_disabled` if it reaches preparation; other selectors use
`false`. This is a request-shape test, not evidence of temporary-chat support.

Preparation diagnostics attempt these candidate steps, stopping at the first
failure:

1. GET the routed ChatGPT homepage to seed the shared infrastructure cookie jar,
   without a bearer token or account ID. HTML is discarded.
2. POST `conversation/init` with the primary-assistant mode.
3. POST `f/conversation/prepare` with `model: "auto"` and the selected history mode.
4. POST `sentinel/chat-requirements` with an empty object, explicitly labeled
   `sentinel_requirements_without_proof` in the report.

The last step is a diagnostic of the native client's missing proof, not a valid
Sentinel implementation. No browser fingerprint, undistributed bridge, challenge
solver, or new credential flow is assumed. The diagnostic path discards preparation tokens. The native saved-text adapter
consumes tokens only in the bound session and keeps the release gate closed until
all required scenarios pass.
POSTs are never automatically replayed, including after a 401 or an ambiguous
connection failure. The existing invocation without `--write-scenario` makes
only the original GET requests.

Exit status **2** means this individual probe does not establish full release
acceptance, even when its selected scenarios succeed. Its `release_gate_passed`
field remains false; the aggregate acceptance assessment makes the release decision.
Exit status 1 indicates harness setup failure. JSON output includes operation
status, elapsed milliseconds, normalized errors, and counts. It excludes tokens,
account IDs, titles, message text, attachment bytes, URLs, and model claims inferred
from catalog availability. A successful model read does not prove send entitlement.
Report version 15 records the experimental feature stage, explicitly selected transport, pinned library versions,
and browser profile, the preparation protocol, and distinguishes `not_run`, `observed`, `failed`, `unsupported`, and
`blocked`; dependent entries include `blocked_by`. Preparation observations retain
only HTTP status, the presence of a Cloudflare challenge, token-presence booleans,
and required-challenge flags. Response bodies, HTML, tokens, unknown fields, and
challenge payloads never enter reports or debug output. `unsupported` describes
the harness's missing implementation, not a proven backend entitlement denial.

The harness uses the ordinary configuration loader without project context and
constructs the shared `AuthManager`. The library accepts that manager by `Arc`;
it has no credential reader, login flow, refresh timer, or inference fallback.
GET requests use shared auth recovery. Ownership revisions cancel local work even
if logout and login notifications coalesce. `ChatProbe::stop()` also cancels local
work; it does not claim server-side cancellation.

HTTP clients inherit certificate, proxy, infrastructure-cookie, residency-header,
and application network policy configuration. Pools live for the session, with
connections reused by resolved route. Ownership changes, credential refreshes,
feature disablement (`stop`), and dropping the probe release the session pool.
Bearer/account headers remain per-request. Browser-emulated pools reject
configured cookie imports and credential-bearing defaults.

The browser-emulated transport pins `wreq = 6.0.0-rc.29` and
`wreq-util = 3.0.0-rc.14`, selecting Chrome 136 on Linux consistently for TLS,
HTTP/2, user agent, and client hints. This is a native Rust client, not a browser.
It uses the application proxy resolver, normalizes custom CA bundles through the
shared loader, verifies certificates and hostnames, and enforces network permits
while sending and consuming response bodies. There is no TLS or proxy fallback.
It rejects unsupported proxy configurations, cross-origin requests, and profile
header overrides. Automatic HTTP retries are disabled.

Preparation attempts are serialized per session. Three consecutive failures
trigger a ten-minute cooldown. `Retry-After` is retained as a deadline and blocks
subsequent operations until it expires. Cloudflare challenges and unsupported
Sentinel challenges have distinct errors; both stop the session immediately.
Redirects and request diagnostics are disabled.
Response bodies are bounded to 8 MiB and operations to 30 seconds. The app-server
workspace-routing resolver is used when present. The standalone harness currently
rejects workspace/FedRAMP accounts without that resolver, and custom backend
configuration, rather than choosing an unverified destination.

## Research evidence

### Native two-phase candidate

Select the native candidate explicitly while keeping the old diagnostic available:

```sh
cargo run -p codex-app-server --example chatgpt_chat_probe -- \
  -c features.chatgpt_chat=true --live --current-account \
  --transport browser-emulated --write-scenario preparation \
  --preparation-protocol two-phase-v1
```

`two-phase-v1` bootstraps without bearer credentials, extracts bounded build and
script metadata, obtains the backend default model and conduit, and submits a session
fingerprint to `/sentinel/chat-requirements/prepare`. Its native CPU worker uses
the exact submitted fingerprint for the returned program and implements the
candidate FNV-1a/avalanche PoW revision. It never tries the legacy SHA3 algorithm
as a fallback. A successful worker result is finalized only if all required
challenge capabilities are supported.
No user message is sent by this inspection entry point.

The register interpreter has no filesystem, process, network, JavaScript engine,
or arbitrary host-reflection access. It uses an arena for guest objects, including
cycles; cumulative allocation reservations, input size, nesting, instruction
count, elapsed time and cancellation are bounded. The default interpreter limits
are 512 KiB of encoded input, 8 MiB of charged allocations, 50,000 steps (including
imports), nesting depth 32 and two seconds. PoW also has a two-second deadline
and at most 500,000 attempts. Unknown instructions, missing
environment capabilities, malformed input, and resource exhaustion are errors,
never encoded into an apparent success token. Unsupported layout and CSS, imported browser state, geolocation and unsupported
string semantics remain explicit failures. Native elements support bounded
reflection, inline declarations and attachment to/removal from the native body.
Session Observer has an isolated collector/snapshot VM with shared state and a
cumulative resource budget; native collection, snapshot and ping have passed the
first live saved-text checkpoint. Explicit
collector-owned fields start unassigned and retain only values written by its
bytecode. Listener registrations preserve callback identity and options; no input
events are fabricated. This native runtime has no WebDriver remote end or session,
so its WebDriver-active flag remains initially false under the
[WebDriver interface contract](https://w3c.github.io/webdriver/#interface). Canvas context acquisition returns null
because this native environment has no drawing backend; pixel serialization or
required graphics values fail explicitly. Script matching accepts a bounded
common ASCII regex subset. Leading regular look-ahead/look-behind uses bounded
forward/reverse DFAs, preserving the original URL's anchors and word boundaries.
A trailing positive assertion preserves greedy/lazy body boundaries. Each
compiler has explicit memory and nesting limits; scans charge the shared work
budget. Backreferences and nested/middle assertions remain unsupported. It does
not run JavaScript or backtracking regexes. Native local and
session storage start empty, accept only native session writes, are bounded to
64 KiB and 512 entries each, and never reach disk. Neither SDK history nor browser
cookies are manufactured to populate them. The environment exposes only its declared Chrome 136
Linux profile, clock, random source, native memory stores, and observed bootstrap
metadata. The profile and device ID are reused within an authenticated generation;
authentication or build changes invalidate them. A literal React Router server
handoff can supply allowlisted, bounded configuration metadata from the same
bootstrap response. Session/loader data, streamed hydration, arbitrary expressions
and missing fields never become invented application state. The parser follows
the [React Router server handoff format](https://github.com/remix-run/react-router/blob/dee9ad2dab41889e8fbdc6848b5781255d9f133a/packages/react-router/lib/dom/ssr/components.tsx).

Known absent globals support presence checks. Explicit guest try instructions can
catch a defined null/undefined property TypeError, using the
[Chrome 136 V8 message template](https://github.com/v8/v8/blob/13.6.233/src/common/message-template.h).
Uncaught exceptions remain failures. Unknown instructions, unavailable capabilities
and resource/cancellation failures bypass those catches. A completed regex search with no match returns null; an empty match remains an
empty string. Opcode 23 skips undefined only. These contracts were exercised with
synthetic programs against the official public runtime after the user authorized
Patchright inspection. Only an explicit success instruction produces a result;
subsequent unsupported operations still stop the native run.

Each owned element has independent native inline declaration state. A small validated
subset of CSS keywords, bounded decimals and literal font families supports
assignment, replacement, removal and ordered serialization following the
[CSSOM declaration contract](https://drafts.csswg.org/cssom/#css-declaration-blocks).
Values are bounded to 128 ASCII bytes; parsing and retained values charge the VM
budget before mutation. Unsupported values leave prior state intact and stop the
program. Stylesheet loading, arbitrary CSS, `cssText` mutation,
and unsupported shorthand serialization are unavailable.

Native geometry has a deliberately small environment: directly attached,
absolutely/fixed positioned, single-line text boxes with pixel lengths. Parley
0.12.0 shapes the text using three immutable DejaVu 2.37 faces bundled with their
[license](fonts/LICENSE) and [profile hashes](fonts/profile.json). System font
discovery is disabled. This profile exposes DejaVu Sans, Sans Mono and Serif,
maps the corresponding generic families, and defaults unavailable families to
Serif. It reads no host fonts or external stylesheets. Browser measurements are
used only as synthetic regression oracles; the interpreter computes each result
from its owned element state and native font data.

Measurements accept at most 512 UTF-8 text bytes and font sizes up to 512 px.
The VM reserves 2 MiB for its private font context and 2 MiB of peak scratch for
each measurement, within the existing 8 MiB allocation limit. Scratch is released
after the native call; work and cancellation checks surround it. Unsupported
flow contexts, wrapping, missing glyphs, transforms, opposing insets and rendered
line breaks return structural failures. The
[layout investigation](evidence/native-layout-research-2026-10-12.json) records
the Chromium comparisons, rejected full-DOM candidates and allocation probe;
the [dependency audit](evidence/native-layout-dependencies-2026-10-12.json)
records the resolved features and Rust requirements. Neither proves live
challenge acceptance.

Credential-generation changes cancel preparation, including CPU work. Native
requests also check the routed origin, retain policy-aware transport behavior,
and never replay a POST. Reports contain only structural observations and a
`native_failure` enum; neither interpreter operands nor challenge/token values
are included. Report version 5 adds allowlisted DOM/prototype/CSS failure details
and a separate observer snapshot operation. Diagnostic strings come from static
public property names; arbitrary challenge operands remain redacted.

This is an implementation candidate, not live acceptance. Successful finalization
does not establish send entitlement, completion, model resolution, or attachment
access. The observer collector runs before finalization; its snapshot runs after
finalization with the same state/key and stays in memory. Neither a message nor
an observer ping is sent by this diagnostic. The original read-only invocation
still makes only GET requests.

The native instruction/PoW references are Aurora at
[`cbfbabc9c14c7278e4dd91cf8807b2bf500860b1`](https://github.com/aurora-develop/aurora/tree/cbfbabc9c14c7278e4dd91cf8807b2bf500860b1)
(MIT; attribution retained in `NOTICE`). The two-phase ordering and token-binding
research also uses the
[versioned adapter observations](https://github.com/kymuco/chatgpt-web-adapter/blob/52113a5d93795676f2c02a1a5e4c1d4154fb7f05/docs/sentinel_bundle_lifecycle.md).
Its browser acquisition mechanism is not used. No private bridge, downloaded SDK
JavaScript, or GPL/AGPL interpreter is vendored or translated.

The follow-up `gh` and web search also examined
[Fire162/chatgpt-unofficial-api at c00be587](https://github.com/Fire162/chatgpt-unofficial-api/blob/c00be58779f14b24a7fef14d094a082f09ac22fd/src/turnstile.mjs).
Although that repository declares MIT, the file explicitly credits
`realasfngl/ChatGPT`, whose repository does not declare a license. It was excluded
as implementation material; none of its decompiler or solver code was adapted.
Search results advertising a native proxy do not establish a usable challenge
implementation or current-account acceptance.

The protocol reference is
[gpt2agent at 20669693dd608a94a5b5aba54c4c4a036f064375](https://github.com/robotlearning123/gpt2agent/tree/20669693dd608a94a5b5aba54c4c4a036f064375).
Research was performed on 2026-10-11. Its
[transport notes](https://github.com/robotlearning123/gpt2agent/blob/20669693dd608a94a5b5aba54c4c4a036f064375/docs/how-it-works.md)
describe a working send path requiring an owner-supplied Sentinel bridge that is
not distributed. The
[replacement specification](https://github.com/robotlearning123/gpt2agent/blob/20669693dd608a94a5b5aba54c4c4a036f064375/docs/dev/specs/sentinel-vm.md)
names `gpt2agent/sentinel_vm.py` and
`tests/fixtures/sentinel_vm/fixtures.json`; both paths returned 404 at that
revision. A proposed algorithm without its implementation, fixtures, and live
acceptance is insufficient transport evidence.

The reference's
[README limitations](https://github.com/robotlearning123/gpt2agent/blob/20669693dd608a94a5b5aba54c4c4a036f064375/README.md#limitations)
exclude file upload. Its browser and manual handoff paths do not meet this
project's native-login requirement. Its model catalog also includes models that
may resolve to another Chat model, and temporary conversations can become
unrecoverable after server handoff. The production transport must preserve both
requested and resolved model IDs and never change temporary chat into saved chat.

[Official OpenAI documentation](https://developers.openai.com/siwc/token-sharing-open-source)
says ChatGPT-plan Responses access does not grant access to ChatGPT conversations.
The documented [preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations)
also exclude persistent conversation state. That API therefore does not fulfill
the native synced-history requirement.

### Upload investigation

There is reusable upload infrastructure in the repository beyond the attachment
metadata registry: `core/src/mcp_openai_file.rs` calls the existing OpenAI file
uploader with `use_case: "codex"`. Its tests cover allocation, signed byte transfer,
and upload completion. This establishes an implementation to inspect and reuse,
not model access to files in native Chat messages.

The external [chat2api upload implementation](https://github.com/lanqian528/chat2api/blob/main/chatgpt/ChatService.py)
(file blob `7260de01b44551cffd586f46a45a705f606361c2`, inspected 2026-10-11)
describes a candidate native sequence: allocate `/files`, PUT bytes to a signed
URL without bearer credentials, finalize `/files/{id}/uploaded`, then check file
processing. Its processing loop can report success after exhausting attempts,
so it must not be copied as proof of readiness. The
[codex-backend-sdk transport notes](https://github.com/B4PT0R/codex-backend-sdk/blob/main/docs/backend-api.md)
(file blob `d1a44e9267f01b542e9a5341f7913ca1f2c4ccf8`, same inspection date)
also describe a `/files/process_upload_stream` pipeline and caller-owned integrity
headers for conversation streams. These are research leads, not verified
contracts for this crate.

Still required: capture the exact use case and processing contract for each
format, determine attachment-to-message association, verify model access, and
discover stricter backend limits. V1's aggregate message budget remains 32 MiB.
Never substitute pasted document text for native upload. Signed storage requests
must use policy-aware clients without account credentials; neither signed URLs
nor file bytes belong in saved conversation tables.

## Capability matrix and release gate

### Latest native preparation evidence

The user authorized Patchright inspection of the service and public scripts on
2026-10-12. Navigation encountered a Cloudflare challenge; the browser HTTP client
could retrieve the public bootstrap and application assets without logging in.
The [runtime contract observations](evidence/official-runtime-contracts-2026-10-12.json)
record the asset/build hashes, synthetic outcomes and research browser version.
Chrome 153 was used for inspection; the native transport retains Chrome 136.
Version-sensitive DOM bindings were checked through `gh` against Chromium
`136.0.7103.113`. No browser token, cookie or account session was imported into
native transport. Browser geometry appears only in synthetic regression fixtures,
never as input to a native challenge answer. No application JavaScript is vendored
or executed by the Rust interpreter.

This resolved the former script no-match ambiguity and established observer
collector/snapshot state sharing. Original Rust changes also correct array
mutation/aliasing and post-resolution execution, preserve bound receivers, and
add bounded string splitting, array pop, reflected element state, inline CSS
values and native body attachment/removal. Observer tests cover isolation,
cancellation, shared budgets, key updates and redacted preparation sequencing.
These fixtures validate contracts; they do not establish live backend acceptance.

**Preparation checkpoint:** native PoW, Turnstile computation and observer
collection complete. Sentinel finalization returns HTTP 200 with a requirements
token, and the native observer snapshot completes. This uses only Rust HTTP and
the bounded interpreter. Bootstrap, initialization, conduit preparation and
Sentinel prepare also returned HTTP 200 without a Cloudflare challenge.

**Saved-text checkpoint:** the first native observer ping and conversation POST
returned HTTP 200. The SSE decoder produced a completed response containing the
exact harmless fixture marker. Authoritative retrieval verified the submitted
user message by its stable ID and expected parent, plus the completed assistant
reply. Requested and resolved model were `gpt-6-thinking`. See the
[saved-text report](evidence/native-saved-text-2026-10-12-report.json) and
[exact source manifest](evidence/native-saved-text-2026-10-12-source.json).

The harness journals saved submissions before polling the POST, retains uncertain
acceptance after interruption, and uses a separate process lock across atomic,
fsynced journal replacements. Journals live under the resolved Nyacode home’s `chat-probe-runs/`
with private directory/file permissions. Explicit resume uses only managed
authentication and the saved non-secret device profile; live cookies, credentials,
preparation tokens and raw stream frames never enter those records. Current
journal permissions are implemented for Unix; other platforms fail explicitly.
Temporary scenarios never create a journal.

**Follow-up and restart checkpoints:** a second process recovered the private
journal, retrieved the prior turn and sent a follow-up using the authoritative
parent. A third process recovered that state and continued again. Both returned
the original marker and passed exact-ID authoritative retrieval. See the
[follow-up report](evidence/native-saved-follow-up-2026-10-12-report.json) and
[fresh-process report](evidence/native-saved-restart-resume-2026-10-12-report.json),
with their corresponding source manifests.

**Temporary checkpoint:** native creation returned a completed exact-marker
response and confirmed user-message acceptance. The ordinary conversation GET
then returned 404, so the history-read candidate stopped before continuation.
The [temporary report](evidence/native-temporary-initial-2026-10-12-report.json)
retains no contents or conversation IDs. The single existing saved journal
predated the temporary run; no temporary journal was created. A separately
selected `--temporary-protocol conduit-refresh-v1` candidate uses the official
stream-resume contract to refresh the in-memory turn by exact identifiers. This
is an explicit read candidate, with no message replay or saved-mode transition;
its live two-turn acceptance passed. The [conduit report](evidence/native-temporary-conduit-2026-10-12-report.json) records two successful exact-ID stream refreshes, continued temporary requests, absent recent history, and unchanged private saved journals. This proves the sole in-memory harness chain; cross-client current-branch discovery is not claimed.

**Attachment checkpoint:** PNG, JPEG, WebP, PDF and UTF-8 text each passed
allocation (200), signed storage PUT (201), and confirmed processing (200).
Each saved response matched the verification code found only inside its file;
all five turns passed exact-ID authoritative retrieval. Requested and resolved
model were `gpt-6-thinking`. See the
[attachment report](evidence/native-attachments-2026-10-12-report.json) and
[source manifest](evidence/native-attachments-2026-10-12-source.json).

The upload-enabled build also passed a fresh-process recovery/continuation of
the earlier saved fixture and repeated the two-turn temporary conduit scenario.
The latter preserved private saved-journal hashes and remained absent from
ordinary history. See the [saved-resume report](evidence/native-saved-resume-after-uploads-2026-10-12-report.json),
[temporary report](evidence/native-temporary-after-uploads-2026-10-12-report.json),
and [validation record](evidence/native-upload-validation-2026-10-12.json).

**Remaining transport work:** failure/recovery cases and live citation/handoff
evidence. PDF parser resource bounds are now enforced by the native interpreter
described below. The normalized history adapter
now retains attachment references and the researched citation metadata contract.
The required native live scenarios have passed. Integrated acceptance remains
closed; these transport results do not establish a complete Chat tab.

The [latest native report](evidence/live-native-two-phase-probe-2026-10-12.json),
[source manifest](evidence/native-two-phase-source-2026-10-12.json) and
[iteration index](evidence/native-two-phase-iterations-2026-10-12.json) retain
sanitized outcomes and source/binary hashes. Earlier
[2026-10-11 evidence](evidence/live-native-two-phase-probe-2026-10-11-v2.json)
remains historical; its script-match blocker is resolved. No raw challenge, bootstrap response, token or private conversation contents
are retained in checked-in evidence. Saved fixture contents and server IDs are
kept only in the private recovery journal.

The feature remains **under development and disabled by default**. The native
saved/temporary/follow-up/restart/upload checkpoint permits service, RPC,
persistence and TUI implementation. Integrated acceptance still gates promotion.

The earlier [live read report from 2026-10-11](evidence/live-read-probe-2026-10-11.json)
used the user's designated current managed login. Model discovery returned 28
entries and the first history page returned 20 summaries. A specific conversation
was not selected. These results establish read access only; no native send,
attachment, temporary-chat, cancellation, or restart/resume acceptance ran.

The subsequent [preparation run](evidence/live-preparation-probe-2026-10-11.json)
used the current managed account and `--write-scenario all`. Model/history reads
now returned `challenge_required`. The native session bootstrap returned HTTP
403 with `cf-mitigated: challenge`, and the runner stopped before any POST.
An earlier exploratory run without homepage bootstrap also received that
challenge on `POST conversation/init`. No response stream, server conversation
ID, server message ID, or upload was obtained. Read success from the earlier
report cannot be carried forward as current write compatibility.

**Earlier blocker (standard transport):** the shared native HTTP transport could not get past the
Cloudflare challenge, even with infrastructure-cookie bootstrap. This is earlier
than the still-unverified Sentinel/conduit sequence; the report does not establish
what that sequence would require after Cloudflare succeeds. The reference still
does not distribute its claimed working solver. Browser login, paid APIs, and
Codex inference cannot satisfy this gate and were not used. Stages B–D remain
unproved, and integrated service/RPC/persistence/TUI work (E–L) is gated.

The [source manifest](evidence/live-preparation-source-2026-10-11.json) records
the repository base revision and hashes of the dirty working-tree harness files
used for that earlier live run. Evidence contains no runtime recovery data or raw
captured response bodies. There are no captured SSE/upload fixtures to sanitize
or treat as acceptance evidence at this point.

### Earlier native transport comparison

The earlier explicit [browser-emulated run](evidence/live-browser-emulated-probe-2026-10-11.json)
and [standard control](evidence/live-standard-comparison-2026-10-11.json) each
used the designated current managed account in a fresh process. Both returned
HTTP 200 for bootstrap, initialization, candidate conduit preparation, and the
requirements diagnostic. Neither encountered a Cloudflare challenge in these
runs. This does not demonstrate that browser emulation caused the improvement
over the earlier result, or that the candidate ordering is correct.

**Earlier diagnostic blocker:** `POST /backend-api/sentinel/chat-requirements`, with the
existing diagnostic `{}` body, returned `proofofwork.required: true`,
`turnstile.required: true`, and a nonempty `turnstile.dx` payload. The harness
returned `unsupported_challenge` and stopped. At that revision the harness had no native requirements fingerprint,
proof-of-work solver, or Sentinel bytecode interpreter.
No challenge payload or token was persisted. An HTTP 200 with tokens present
does not establish a valid preparation-to-send protocol.

The MIT reference's proposed VM specification lacks operand/call details and
its referenced implementation and fixtures. Its proposed environment includes
browser DOM, storage, and location values that have not been established for
this native session. Implementing guessed instructions, skipping unknown
instructions, or manufacturing these values would not satisfy the required
interpreter contract. A bounded native implementation with demonstrated token
relationships and fixtures remains required before the first saved send.

Both runs exited 2. No user message, upload, server conversation/message ID,
saved-run journal, or temporary conversation was created. Requested and resolved
inference models remain untested. The new
[source and validation manifest](evidence/native-transport-source-2026-10-11.json)
records the pinned dependency graph, implementation hashes, binary hash, and
verification results. Service, persistence, RPC, TUI, and integrated acceptance
remain gated; the feature stays under development and disabled by default.

| Capability                             | Implementation/evidence                                              |
| -------------------------------------- | -------------------------------------------------------------------- |
| Managed auth, identity invalidation    | Shared manager; synthetic regression tests                           |
| Model catalog                          | Read-only probe; availability is not entitlement                     |
| Paginated history                      | Native normalized summaries; probe reports count only                |
| Selected active branch                 | Normalized transcript, branch validation and continuation limits     |
| Native conversation preparation        | Native preparation and completed saved sends observed                |
| Saved text and server-parent follow-up | Completed responses and authoritative retrieval observed             |
| Restart/resume                         | Fresh process reconciliation and continuation observed               |
| Temporary chat and handoff             | Sole in-memory temporary chain observed; handoff deferred            |
| PNG, JPEG, WebP, PDF, UTF-8 uploads    | All five completed; exact file-code responses and retrieval observed |
| Server cancellation                    | Unverified; local cancellation is reported separately                |

The harness never promotes these capabilities from mock results. Current unit
tests contain synthetic structural JSON fixtures, not captured live conversation
events. No live SSE or upload fixture is claimed.

The native saved/temporary/follow-up/restart/upload milestone is complete for the
designated account. Service/RPC/UI integration and the live scenarios below are
implemented. The integrated release gate remains closed pending the remaining
platform checks. Ambiguous submissions remain pending for exact-ID reconciliation
and are never automatically resent.

## Developing terminal surfaces

The owned full-screen renderer now has independent **Codex | Chat** surfaces.
Chat requires the alternate screen and a trusted embedded or local-socket server.
`tui.keymap.global.switch_surface` defaults to F6; `/chat` and `/codex` also switch.
Each open Chat keeps its own composer, transcript, attachments and scroll position.
Codex events continue in the background, with running, unread and approval-attention
indicators in the surface header.

Chat supports `/new`, `/temporary`, `/history`, `/drafts`, `/chats`, `/model`,
`/attach <path>`, `/refresh`, `/close` and `/help`. F2 opens a new saved draft,
Shift-F2 opens a temporary draft, F3 selects an open chat and F5 refreshes recovery
state. Ctrl-V captures a bounded image in memory. Ordinary `!`, `@` and `$` are
literal text. Chat content does not enter the Codex composer, history, transcript
recorder or generic application events.

Draft writes are debounced and revision-aware, with flushes on surface navigation
and graceful shutdown. A failed or conflicted save retains the local draft;
Ctrl-Shift-S preserves it as a distinct draft without overwriting another client.
Submit freezes stable message/submission IDs until acceptance is established.
Refresh cannot unlock an active or ambiguously accepted submission, and no failure
automatically resends it. Clipboard selections lost across restart require
reattachment. Closing or replacing unused uploads releases their scoped handles.

The focused terminal regression suite passed 482 tests, including snapshots,
feature-disabled behavior, keymap conflicts, clipboard behavior and active-turn
refresh safety. These deterministic checks do not establish live terminal
acceptance or open the integrated release gate.

The [embedded terminal attachment run](evidence/native-tui-embedded-attachments-2026-10-12-report.json)
submitted all five formats together and displayed each file's distinctive code.
It also switched surfaces during upload, read a generated PNG from an isolated
clipboard without a temporary file, retained temporary state outside saved Chat
tables, rendered a narrow terminal and shut down gracefully. The preceding
[temporary follow-up run](evidence/native-tui-embedded-lifecycle-initial-2026-10-12-report.json)
exposed and verified the fix for missing earlier temporary messages.
These reports retain their original failures. The subsequent
[embedded cancellation and approval run](evidence/native-tui-embedded-cancel-approval-2026-10-12-report.json)
verified cancellation after acceptance, an accurate local-stop notice, unchanged
saved Chat tables for temporary content, and the Codex approval-attention
indicator. Switching to Codex exposed the harmless `pwd` approval; accepting it
once and returning to Chat retained the Chat state.

The [daemon terminal checkpoint](evidence/native-tui-daemon-lifecycle-initial-2026-10-12-report.json)
completed temporary follow-ups, cleared the surface on feature disablement,
rejected new drafts while disabled, and re-established a fresh scope. An exact
accepted saved message remained retrievable from ChatGPT. This checkpoint did
not prove generation was still active at disablement. The subsequent
[daemon attachment and disable run](evidence/native-tui-daemon-attachments-disable-2026-10-12-report.json)
displayed all five fixture codes, including a PNG supplied through the isolated
in-memory clipboard, and switched surfaces during upload. The controller observed
an accepted active turn immediately before disabling Chat. The UI cleared,
rejected new drafts while disabled, and the accepted saved message remained
authoritatively retrievable after re-enabling.

The [daemon approval run](evidence/native-tui-daemon-approval-2026-10-12-report.json)
also displayed Codex's attention indicator from Chat, opened the pending `pwd`
approval on F6, and observed its execution after approval. Both approval fixtures
disabled collaboration tools because the current Codex model rejected their
reserved schema before any approval. This isolates Chat's approval handling;
it does not claim that the unrelated Codex schema rejection is fixed.

The [socket reconnect run](evidence/native-tui-daemon-reconnect-2026-10-12-report.json)
dropped the local connection first with an unsent draft, then during an accepted
response. The draft survived and was saved after reconnection, the stale transport
notice cleared, and the response completed with exactly one durable submission.
The relay captured no traffic. [Inline mode](evidence/native-tui-inline-rejected-2026-10-12-report.json)
and [disabled alternate screen](evidence/native-tui-no-alt-rejected-2026-10-12-report.json)
correctly rejected Chat; [feature-disabled Codex](evidence/native-tui-feature-disabled-2026-10-12-report.json)
retained its ordinary surface.

The [latest validation record](evidence/native-tui-lifecycle-validation-2026-10-12.json)
records 5,758 passing tests in the full TUI run and ten environment-related
failures. Every failed test passed after correcting fixture permissions or the
inherited `NO_COLOR` setting. Debug tests required a 32 MiB thread stack for an
existing core configuration-clone path; production stack settings were unchanged.
All 43 targeted reconnect/Chat tests passed after the final fix, as did all-target
TUI Clippy, the CLI build, and 15 lifecycle harness tests. This is not a claim of
one clean full-suite invocation. The integrated release gate remains closed.

Both the [embedded lifecycle RPC run](evidence/native-rpc-embedded-lifecycle-2026-10-12-report.json)
and [daemon lifecycle RPC run](evidence/native-rpc-daemon-lifecycle-2026-10-12-report.json)
verified cancellation after acceptance, disablement while another accepted turn
was active, revoked-scope rejection, a fresh scope after re-enabling, removal of
temporary state, and retrieval of both exact saved user-message IDs. The user's
configuration file was unchanged. Server cancellation remains unconfirmed and
is reported separately from local stopping.

## Local socket acceptance harness

The ordinary local daemon passed scoped status/catalog/history reads, a saved
response with authoritative retrieval, and two concurrent saved responses. The
[parallel audit](evidence/native-rpc-daemon-parallel-2026-10-12-audit.json)
observed two distinct accepted submissions active at the same time. The
[temporary run](evidence/native-rpc-daemon-temporary-initial-2026-10-12-report.json)
completed its first response, then stopped at `unsupportedChallenge` before
accepting its follow-up. Its saved Chat tables remained unchanged; attachment
acceptance did not run afterward. This failure remains part of the evidence.

Inspection found an independent observer deadline defect: the bounded native VM
counted the HTTP finalization wait against its execution limit. The execution
clock now pauses between collector and snapshot; consumed execution time,
instructions, allocations, and cancellation remain cumulative. The
[validation record](evidence/native-daemon-observer-budget-2026-10-12.json)
records 182 passing native tests. Content-free preparation failure diagnostics
now retain the precise stage and structural failure. The original daemon binary
did not retain that detail, so the timing defect is not asserted to be the cause
of its failure. The rebuilt daemon's
[new temporary run](evidence/native-rpc-daemon-temporary-2026-10-12-report.json)
passed both turns, the correct-parent continuation, saved-read rejection, absence
from recent saved history, and unchanged saved tables/private journals.

The [initial daemon upload run](evidence/native-rpc-daemon-attachments-initial-2026-10-12-report.json)
passed PNG, then stopped after JPEG acceptance when the response stream failed.
A [read-only reconciliation diagnostic](evidence/native-daemon-accepted-jpeg-reconciliation-2026-10-12.json)
found the exact user ID/parent and its completed JPEG fixture response on the
authoritative branch. That diagnostic sent no user messages and is not a Rust
attachment acceptance pass. The native stream now explicitly supplies its
three-minute request budget to the HTTP adapter, whose default whole-response
deadline was 30 seconds. Header/idle limits, cancellation, and policy revocation
still apply. Body timeout categories survive the adapter without native error
contents. The subsequent
[Rust daemon attachment run](evidence/native-rpc-daemon-attachments-2026-10-12-report.json)
passed PNG, JPEG, WebP, PDF, and UTF-8 in one conversation with correct-parent
follow-ups. Every upload reached processing readiness; every response matched
its file's verification code and was authoritatively retrieved with attachment
metadata and no pending submission. The stream delivered 66 ordered updates.

Add `--rpc-socket /absolute/path/to/server.sock` to an explicit `--rpc-read`,
`--rpc-saved`, `--rpc-temporary`, `--rpc-attachments`, or `--rpc-lifecycle` scenario to exercise the
ordinary app-server daemon transport. The harness uses the production local
socket client and requires experimental negotiation, the same application home,
and the designated managed ChatGPT account before accessing Chat data. It does
not accept a WebSocket/TCP URL, start a server, or fall back to an embedded server
if the socket fails. Without this flag, the existing embedded behavior remains.
Local-socket results alone do not establish terminal acceptance.

`--rpc-saved --rpc-resume-draft <private-draft-uuid>` explicitly continues a
fixture in a fresh client process, using shared managed authentication. The
initial saved scenario flushes an empty draft with its authoritative parent only
after acceptance and retrieval. Resume requires that unchanged revision and
branch, the exact completed harness fixture, and no pending submission; personal
content, cached parents, branch changes, and ambiguous submissions are rejected.
The outer runner retains the identifier privately and checks durable submission
IDs across app-server restart. No ID or conversation content enters the report.

The [embedded restart audit](evidence/native-rpc-embedded-restart-2026-10-12-audit.json)
and [daemon restart audit](evidence/native-rpc-daemon-restart-2026-10-12-audit.json)
both passed: fresh Rust client and server processes recovered the saved draft,
continued the same conversation from its exact authoritative parent, and
retrieved the completed follow-up. A preliminary resume stopped before sending
because the fixture guard did not yet traverse the observed completed thinking
node between user and final answer. The corrected guard has cycle/tool-branch
tests; all 14 harness tests pass. UI, cancellation, and integrated disablement
acceptance still gate release.

## Embedded attachment acceptance

The five-format saved RPC scenario passed after fixing the observed saved-history
image reference namespace. The server rewrites uploaded `file-service://` image
references to `sediment://` references. Both are retained as bounded metadata;
neither grants permission to download an asset or reuse an upload handle.
The [live report](evidence/native-embedded-rpc-attachments-2026-10-12-report.json)
and [source manifest](evidence/native-embedded-rpc-attachments-2026-10-12-source.json)
cover exact fixture responses, ordered upload/turn events, authoritative parent
and message identity, attachment metadata, and cleared pending submissions for
PNG, JPEG, WebP, PDF, and UTF-8. The
[decoder observation](evidence/native-history-image-reference-2026-10-12.json)
retains the initial failure and corrects the earlier launcher's private-journal
path check. The integrated daemon/TUI gate remains closed.

## Native attachment candidate

Uploads use `POST /files`, an isolated HTTPS storage PUT, then
`POST /files/process_upload_stream`. Processing is bounded UTF-8 NDJSON; both
matching file-ready and completion events are required. Redirects, automatic
application retries, raw-file indexing fallback, and unverified multipart or
relative-storage protocols are rejected. Storage clients have no ChatGPT
bearer, account, cookie, or Sentinel headers. Ready handles bind principal,
workspace, draft, persistence mode and authentication generation, with a
conservative local expiry. Only metadata and file references enter saved journals.

Selections are copied off the terminal loop into immutable, leased buffers.
The aggregate limit is 32 MiB, service staging is 128 MiB, and images have the
[documented 20 MiB per-image ceiling](https://help.openai.com/en/articles/8555545).
PNG/JPEG/WebP bytes are decoded with allocation/dimension limits; text is strict
UTF-8; PDF uses strict parsing and a checked page tree inside an import-free
[Rust validator](pdf-validator/README.md), interpreted by Wasmi in process.
The entire parse has a 128 MiB linear-memory ceiling, bounded call/value stacks,
200 million fuel units, a five-second deadline, and cancellation at fuel yields.
The existing 8 MiB per-stream ceiling remains. A regression exercises the actual
parser's oversized PNG-predictor allocation; others cover malformed PDFs,
recursion, cancellation, infinite execution, and large valid input. The bundled
module reproduces byte-for-byte from its pinned source and lockfile on Rust 1.95.
Clipboard images stay in memory and
recovered disk selections require a fresh content digest.

The generated corpus contains no personal material. The model prompt never
contains the expected verification code. Synthetic TLS tests establish byte
transfer, credential separation, redirect refusal, incomplete processing failure,
and scoped-handle rejection; they do not establish live attachment access.

## Attribution

The endpoint and history-shape research above uses gpt2agent by robotlearning123
(MIT). The candidate preparation request adapts its field names and request
shape; the copyright and license are retained in [NOTICE](NOTICE). No Python
code, solver, or upstream fixtures are vendored. Browser-profile support comes
from the pinned Apache-2.0 wreq dependencies. The Rust
request lifecycle and tests use the repository's HTTP and authentication
infrastructure.

## Native build requirements

Keep the workspace's Rust 1.95.0 toolchain. The browser transport builds BoringSSL
and needs CMake, a C/C++ toolchain, libclang for bindgen, and native binutils.
Linux enables vendored OpenSSL for the existing native-tls dependency because
BoringSSL's archive names otherwise collide with dynamically linked OpenSSL.
BoringSSL symbols are prefixed. This changes linkage, not the ordinary Codex
HTTP backend. Bazel retains its existing hermetic static OpenSSL archive and
supplies the new native build tools through explicit dependency metadata. Its
BoringSSL build uses the selected LLVM C++ runtime through a C++ library
dependency, rather than relying on a host `libstdc++` installation.

Cargo can export automatically discovered `SSL_CERT_FILE`/`SSL_CERT_DIR` values
to test processes. The native-default HTTP tests assume these overrides are
absent; run those binaries with the original environment. Custom CA tests supply
their own fixtures in isolated subprocesses. Do not strip configured CA values
from live probes.

## Validation

```sh
cargo test -p codex-http-client -p codex-chatgpt-chat -p codex-features
cargo test -p codex-app-server --example chatgpt_chat_probe
cargo clippy -p codex-http-client -p codex-chatgpt-chat -p codex-features --tests -- -D warnings
cargo clippy -p codex-app-server --example chatgpt_chat_probe
cargo build -p codex-app-server --example chatgpt_chat_probe
cargo build -p codex-cli
```

From the repository root, verify the native Bazel build and dependency lock:

```sh
bazel test //codex-rs/http-client:http-client-unit-tests --lockfile_mode=error
bazel test //codex-rs/chatgpt-chat:chatgpt-chat-unit-tests --lockfile_mode=error
bazel mod deps --lockfile_mode=error
```

The native preparation checkpoint passed **294 Cargo tests**, affected-crate and
harness Clippy with `-D warnings`, and the CLI build. Bazel passed the Chat and
HTTP unit tests with its lockfile enforced. See the
[checkpoint validation](evidence/native-preparation-validation-2026-10-12.json).
The saved-text revision then passed **129 Chat tests**, Chat Clippy with
`-D warnings`, and the same 129 tests in Bazel. These include fragmented SSE,
delta inheritance, completion/acceptance separation, private journal recovery,
process ownership, exact-ID reconciliation, and a mock asserting that the durable
pending record exists before the POST. The first live saved-text report above
records the separate server acceptance result.

The earlier 2026-10-11 native two-phase revision passed **266 Cargo tests**: 82 Chat tests, 125 HTTP
unit tests, 10 custom-CA integration tests, 44 feature tests and 5 harness tests.
Bazel independently passed the same 82 Chat tests with the lockfile enforced.
Crate and harness Clippy passed with `-D warnings`; the Bazel dependency-lock
check also passed. The production CLI build and Rust formatting passed. These checks validate the
earlier candidate implementation and failure behavior; at that historical
revision the live challenge stopped before finalization. The complete final check statuses, exact source hashes and
remaining platform gaps are recorded in the validation manifest above.

The earlier transport-only Linux x86_64 GNU validation on Rust 1.95.0 passed 27 Chat tests,
125 HTTP unit tests, 10 custom-CA integration tests, 44 feature tests, and 4
harness argument/offline tests: **210 tests**. Both Clippy commands passed with
`-D warnings`. Rust formatting, Prettier, Buildifier, the harness build, and the
CLI build passed. These checks establish transport and harness behavior only.
Both live comparison runs exited 2; neither acceptance gate passed.
Bazel independently passed all 125 HTTP unit tests with the native dependencies;
the dependency-lock check also passed without updating the lockfile.

The [resolved dependency report](evidence/native-transport-dependencies-2026-10-11.json)
records 188 packages in the browser transport's dependency closure. None declares
a Rust requirement above 1.95. Native builds on macOS, Windows, Linux musl, and
other architectures remain unverified; the Linux result does not establish
support on those platforms.

Subsequent [Windows GNU transport validation](evidence/native-windows-gnullvm-transport-link-2026-10-12.json)
compiled and linked the HTTP test executable for `x86_64-pc-windows-gnullvm`
using the pinned toolchain and hermetic TLS/C++/pthread dependencies. This is a
cross-build result, not a native Windows test run. At that checkpoint, MSVC
executable validation still awaited the configured SDK/runtime and explicit
Microsoft terms acceptance required by `MODULE.bazel`.

The [macOS ARM64 transport cross-build](evidence/native-macos-aarch64-transport-link-2026-10-12.json)
also compiled and linked the HTTP test executable. CMake now uses declared LLVM
`install_name_tool` and `ranlib`; a host GNU archive rewrite is never used for
Darwin objects. Native macOS runtime tests are not claimed by this cross-build.

The final native-tool revision also passed the [Linux HTTP and Chat Bazel tests](evidence/native-linux-final-tools-2026-10-12.json)
and [Windows GNU executable cross-link](evidence/native-windows-gnullvm-final-tools-2026-10-12.json).
[Linux x86_64 musl](evidence/native-linux-musl-transport-tests-2026-10-12.json)
compiled, linked and ran all 127 HTTP tests. [Linux ARM64 musl](evidence/native-linux-aarch64-musl-transport-link-2026-10-12.json)
compiled and linked; ARM64 runtime tests were not run.
[Linux ARM64 GNU](evidence/native-linux-aarch64-gnu-transport-link-2026-10-12.json)
and [macOS x86_64](evidence/native-macos-x86_64-transport-link-2026-10-12.json)
also compiled and linked the HTTP test executable. These are cross-build checks.

[Further MSVC research](evidence/native-msvc-toolchain-research-2026-10-12.json)
identified LLVM 0.8.24's native MSVC ABI support and an upstream missing runtime
library entry. The [candidate patches](research/msvc-toolchain-candidate/manifest.json)
rebase the existing toolchain customizations and add the missing runtime library;
they were verified in isolation and have now been applied. The module graph resolved in an isolated
checkout, revealing required `apple_support`, `rules_python` and `zstd` upgrades.
A reviewable module/lockfile patch accompanies the source patches. The isolated
[Linux HTTP executable build](evidence/native-msvc-candidate-linux-link-2026-10-12.json)
compiled and linked on Rust 1.95.0. The final candidate
[seven-target transport matrix](evidence/native-msvc-candidate-platform-matrix-2026-10-12.json)
passed: Linux GNU ran 127 HTTP and 183 Chat tests; static x86_64 musl ran all 127
HTTP tests; ARM64 Linux GNU/musl, macOS ARM64/x86_64 and Windows GNU x86_64
compiled and linked. The full CLI check then exposed a removed `rules_cc`
`grep-includes` alias still referenced by Bazel. Backporting
[the upstream restoration](https://github.com/bazelbuild/rules_cc/pull/888)
resolved it. The [full Linux CLI build and startup checks](evidence/native-msvc-candidate-linux-cli-2026-10-12.json)
passed, followed by the [final seven-target matrix](evidence/native-msvc-candidate-final-platform-matrix-2026-10-12.json)
with the exact candidate patches. GNU reused valid Bazel test results; musl
freshly ran the static executable. Foreign-platform runtime execution is not
claimed. The user subsequently explicitly accepted the Microsoft Visual Studio
and Windows SDK terms. MSVC verification then exposed compiler-driver options,
Cargo SDK paths, libclang argument parsing, and static CRT configuration in
Bazel and CMake. The
[diagnostic and regression report](evidence/native-msvc-sdk-path-research-2026-10-12.json)
records the fixes and retained failures. Both advertised Windows MSVC release
architectures, x86_64 and ARM64, now pass
[executable cross-build and import checks](evidence/native-msvc-accepted-transport-builds-2026-10-12.json).
Both PE architectures match their targets and neither executable imports a
dynamic CRT. The complete patch set then passed the
[final non-MSVC matrix and Linux CLI checks](evidence/native-msvc-final-platform-regressions-2026-10-12.json)
and was [adopted without changing its tested inputs](evidence/native-msvc-toolchain-adoption-2026-10-12.json).
Existing working-tree changes and the Rust 1.95.0 pin were preserved.

The earlier transport-comparison evidence precedes the pool-lock, preparation-permit, default-header,
and Bazel-toolchain fixes. The source manifest records both snapshots and the live
binary hash. The final deterministic checks cover those fixes; no later live
send or challenge-solving acceptance is implied.

The config schema must be regenerated when feature or keymap configuration types
change. Chat protocol types are experimental; stable app-server exports exclude
the Chat methods and types.
