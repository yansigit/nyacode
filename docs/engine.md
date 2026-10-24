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
`integration/codex/overlays`. Every new owned file must be mapped in
`integration/codex/assembly.json`. Engine changes belong in the ordered patches,
with an owner, purpose, and guarded regression group in `inventory/downstream.json`.
The inventory retains retired baseline changes separately from active integration.

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
pin. Stable-release discovery runs daily at 04:17 UTC when repository variable
`NYACODE_ENGINE_UPGRADES_ENABLED=true`. Failed candidates remain drafts with linked
diagnostics. Promotion verifies the exact candidate commit, current base, allowed
files, complete qualification, and strict branch protection before an immediate
squash merge. Product release tags and publication remain explicit.

The trusted publisher records the exact qualification run ID returned by GitHub.
A separate trusted observer waits up to 350 minutes for that run, reports its
result, and passes its validated ID to the separate promotion job. Candidate
qualification keeps read-only credentials. The `workflow_run` trigger remains a
fallback, but automatic continuation does not depend on delivery of bot-generated
events.

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
