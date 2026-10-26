# Engine development

Nyacode owns its provider crates, Chat implementation, integration patches, SDKs,
and release tooling. `engine.lock` pins pristine Codex by full revision and archive
checksum. `VERSION` controls the independent Nyacode product version.

The build prepares `.engine/workspace` using GitHub CLI (`gh`), checks every patch
and overlay against the architecture inventory, and builds with the committed
`integration/codex/Cargo.lock`. Preparation never updates dependency versions.

```sh
python3 scripts/build_nyacode.py
just nyacode --help
just test-providers
just engine-controls
just engine-checks
scripts/verify-container.sh full
```

Binaries are written to `.engine/target` unless `CARGO_TARGET_DIR` is explicitly
set. Both `nyacode` and `codex` are built, alongside the Code Mode helper. SDK tests
accept `CODEX_EXEC_PATH`; set it to the binary being qualified. Credential and
storage namespaces are unchanged, and this migration moves no user data.

Edit provider implementations in `crates/providers`, Chat in `crates/chatgpt-chat`,
and engine-dependent product modules in `integration/codex/adapter` or
`integration/codex/overlays`. Ownership metadata lives in
`integration/codex/ownership.json`. Run `just engine-manifest` to regenerate
`integration/codex/assembly.json` and `inventory/downstream.json`; controls enforce
that both stay in sync with the ownership metadata, patches, and overlay bytes.
Retired baseline changes remain in `ownership.json` alongside the active inventory.

`just fix`, schema generators, and Bazel lock updates modify disposable assembled
source. Review and copy the changes into owned sources or integration patches
before preparing again. `just engine-schema` checks regenerated RPC schemas against
the independent compatibility fixtures; it cannot approve incompatible API or
stored-state changes. Explicit dependency updates must commit the assembled Cargo
lock and any affected provider/Bazel locks together.

Preparation records all input hashes and the complete assembled diff under
`.engine/workspace/.git/nyacode`. Failure diagnostics remain beside a failed
assembly. Baseline byte parity, linked tests, and upgrade rehearsals are recorded
under `inventory/`; they describe their recorded inputs and do not substitute for
qualification of later changes.

Provider implementation changes run standalone and linked integration checks.
Shared, product, adapter, and unclassified inputs take the broader acceptance path.
Engine upgrades qualify native contracts, every Rust workspace crate in the container,
both SDKs, and all six platform packages.
Native provider sessions do not expose individual inference completion boundaries.
Synchronous `PostModelRequest` hooks therefore fail before native inference starts;
Responses backends support the upstream request hook ordering.
An existing native checkpoint limitation affects textless turns ending in user or
agent-mail content: a later adjacent user message can change the saved history
prefix and cause continuation to fail closed. The migration preserves this guard
and the stored checkpoint format; it does not replay uncertain tool execution.
The always-present `Engine acceptance` check fails on missing or cancelled results.
Model-catalog requests and cache identity use the pinned upstream release version,
or the pristine workspace version for an untagged baseline. Public package and CLI
versions continue to come from `VERSION`.

```sh
python3 scripts/engine.py upgrade rust-vX.Y.Z --mode controls
```

An upgrade proposal lives under `.engine/upgrades` and never changes the accepted
pin. Discovery runs every three hours and is idempotent. Repository variable
`NYACODE_ENGINE_UPGRADES_ENABLED=true` is the kill switch: when it is not `true`,
discovery and publication refuse in every mode, including manual dispatch. Observation
and promotion of an already-running candidate continue, so in-flight work can finish.
Product release tags and publication remain explicit.

The trusted publisher records the exact qualification run ID returned by GitHub.
It never reopens a candidate that was merged or closed with the `engine:reject`
label. A candidate closed without that label gets a new pull request on a rebuilt
branch. An open candidate whose branch is not based on the current default branch is
rebuilt from the same `engine.lock`, force-updated, and re-dispatched. Publishing a
newer tag closes each older open `engine/rust-v*` candidate with a comment linking
its replacement. Failed candidates remain drafts with linked diagnostics.

Every three hours the sweep job finds the latest qualification run for each open
candidate. Once that run has completed, the sweep reports it and promotes it if
eligible, without waiting on a hosted job. Promotion verifies the exact candidate
commit, current base, allowed files, complete qualification, and strict branch
protection before an immediate squash merge.

A separate trusted observer waits up to 350 minutes for a run dispatched by the same
invocation, reports its result, and passes its validated ID to the promotion job.
Candidate qualification keeps read-only credentials. The `workflow_run` trigger
remains a fallback, and automatic continuation does not depend on delivery of
bot-generated events.

Each job records its events as an artifact, and a final `status` job upserts the
single open issue titled "Upstream sync status" (label `engine:status`). The issue
shows the current pin, the last update, and the 30 most recent events, including
skips, rejections, supersessions, qualification results, and failed jobs. Maintainers
reject a candidate by closing its pull request and adding the `engine:reject` label.
Create the `engine:status` and `engine:reject` labels before enabling the kill switch.

Qualification packages may run for 360 minutes. If observation times out or an
older run needs its report restored, replay only the observer from the default
branch with the existing run ID:

```sh
gh workflow run engine-upgrades.yml --ref main -f qualification_run=RUN_ID
```

Replay skips discovery and publication, waits boundedly if the run is still in
progress, and keeps all exact-head and promotion checks. An incomplete run cannot
be promoted.

The Nix development shell uses the exact Rust version from `engine.lock` and
includes the GitHub CLI, Python, and Git required for explicit preparation:

```sh
nix develop --no-update-lock-file
python3 scripts/engine.py prepare
export NYACODE_ENGINE_ROOT="$(pwd)/.engine/workspace"
nix build --impure --no-update-lock-file
```

The package consumes that prepared source snapshot, checks its recorded engine
pin and committed Cargo lock, and installs its assembly provenance under
`share/nyacode`. Nix fetches the dependencies pinned by `flake.lock` and Cargo.lock;
it does not prepare an engine or update either lock. After an accepted toolchain
change, explicitly update and review the rust-overlay entry in `flake.lock` if
needed. Preparation is not required for `nix develop`.
