# Engine migration

The engine migration is an opt-in rehearsal. The regular build, installers, and
release packages still use the checked-in `codex-rs` source until the assembled
workspace passes provider, SDK, and six-platform qualification.

`engine.lock` pins the upstream revision and archive hash. The assembly manifest
maps strict per-file patches, owned overlays, and provider crates into a complete
workspace. Preparation requires GitHub CLI (`gh`) and writes the result to
`.engine/workspace`:

```sh
just engine-schema
just prepare-engine
python3 scripts/build_nyacode.py --engine
```

The opt-in build produces `nyacode`, `codex`, and the Code Mode host under
`.engine/workspace/codex-rs/target`. SDK integration tests prefer an explicit
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
daily only when `NYACODE_ENGINE_UPGRADES_ENABLED=1`; scheduled runs must remain
gated until that qualification is in place.
