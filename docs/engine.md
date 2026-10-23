# Engine migration

The local engine migration is an opt-in rehearsal. The regular local build still
uses the checked-in `codex-rs` source. CI qualifies the assembled workspace across
provider, SDK, and all six package platforms before cutover.

`engine.lock` pins the upstream revision and archive hash. The assembly manifest
maps strict per-file patches, owned overlays, and provider crates into a complete
workspace. Preparation requires GitHub CLI (`gh`) and writes the result to
`.engine/workspace`:

```sh
just engine-controls
python3 scripts/build_nyacode.py --engine
```

The opt-in build produces `nyacode`, `codex`, and the Code Mode host under
`.engine/target`, outside the disposable source workspace. SDK integration tests prefer an explicit
`CODEX_EXEC_PATH`; otherwise they use that assembled debug binary when present,
then the source-tree debug binary. Set the variable to the binary under test so
an installed CLI cannot be selected accidentally.

Run provider and engine checks with:

```sh
just test-providers
just engine-checks
scripts/verify-container.sh --engine-workspace full
```

The container option selects the assembled workspace. The existing
`--engine docker|podman` option still selects the container runtime.

Do not switch the default build or remove duplicate upstream tracking until the
assembled workspace passes controls, provider and SDK checks, and builds on
Linux, macOS, and Windows for x64 and ARM64. Candidate engine updates may run
daily only when the repository variable `NYACODE_ENGINE_UPGRADES_ENABLED=true`; scheduled runs must remain
gated until that qualification is in place.

`just engine-schema` regenerates schemas and Python SDK bindings inside the
assembly, then checks the independent fixtures in `integration/codex/compatibility`.
Review generated changes before moving them into owned integration inputs; this
command cannot update the compatibility fixtures.

Automatic promotion verifies the exact candidate commit, current base, all
qualification jobs, and strict branch protection. It performs an immediate squash
merge; it never leaves approval queued for a later commit. Failed candidates stay
drafts with linked diagnostics. Product release tags and publication remain explicit.
