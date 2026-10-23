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
Engine upgrades qualify native contracts, both SDKs, and all six platform packages.
The always-present `Engine acceptance` check fails on missing or cancelled results.

```sh
python3 scripts/engine.py upgrade rust-vX.Y.Z --mode controls
```

An upgrade proposal lives under `.engine/upgrades` and never changes the accepted
pin. Stable-release discovery runs daily at 04:17 UTC when repository variable
`NYACODE_ENGINE_UPGRADES_ENABLED=true`. Failed candidates remain drafts with linked
diagnostics. Promotion verifies the exact candidate commit, current base, allowed
files, complete qualification, and strict branch protection before an immediate
squash merge. Product release tags and publication remain explicit.

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
