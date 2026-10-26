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

## Generated outputs

Whole files produced by generators live under `integration/codex/generated/<repo
path>` instead of the patches. `prepare` copies them into the assembly and checks
their sha256 against `assembly.json`; export never writes them back into patches.
`ownership.json` `generated` declares each generator with its output patterns,
owner, purpose, regression groups, and whether it is `deterministic` (drift-checked)
and `tree` (its outputs are moved into the generated tree).

```sh
just engine-generate --deterministic                      # report differences from the assembly
just engine-generate --only config-schema --write-back   # update integration/codex/generated
just engine-generate --only snapshots --crate codex-cli  # opt-in; not drift-checked
```

`generate` compares each generator's outputs with the workspace before it ran.
`--write-back` copies outputs whose bytes differ from upstream into the generated tree
and removes tree files the generator no longer produces; it rewrites `assembly.json`
unless a patch still carries an output, which export must remove first. Outputs that
upstream lacks need an overlay. During rebase, a conflict in a generated path is
`regenerate` rather than a conflict: the workspace takes upstream's bytes, and export
refuses that path until it is regenerated or passed with `--accept`.

The schema, config, hook, pnpm, uv, and bazel lock generators reproduce their committed
bytes at the pinned revision. `snapshots` is opt-in and its output is not part of the
drift check; its snapshots stay in the patches.

## Carrying patches forward

`engine-rebase` carries the ordered patch stack onto a newer upstream commit. Each
patch is committed alone on the pinned revision, then 3-way merged onto the new
revision with renames disabled. The result is a clean workspace whose uncommitted
diff against `pristine` is the patch stack. Work is staged under `.engine/rebase/<rev>`.

```sh
just engine-rebase <40-hex-commit>                 # exit 0 clean, 2 conflicts, 1 error
python3 scripts/engine.py rebase --onto-local-ref upstream/main   # rehearse from a local ref
just engine-export --workspace .engine/rebase/<rev>/workspace     # write patches back
just engine-export --workspace ... --update-lock                  # also repin engine.lock
```

Review `rebase-report.json` first; every conflicted file lists its conflict blocks.
Resolve conflict markers in the workspace. `export` refuses while markers remain,
and refuses modify/delete or binary conflicts still holding merged bytes unless
they are passed with `--accept PATH`. A changed path owned by no patch is an error
until it is assigned with `--assign PATH=PATCH`. A rename of a patched path fails the
rebase loudly; port that change by hand.

`--update-lock` needs a GitHub-sourced report (`--onto`, not `--onto-local-ref`),
because local archives do not carry GitHub's digest. The tool does not regenerate
`integration/codex/Cargo.lock`, `inventory/downstream.json`, or overlay sources;
run `just engine-checks` after an export and regenerate the inventory when it reports
changed ownership.

A daily workflow runs this carry against upstream main and publishes snapshots on
`sync/upstream-main`, so a stable-tag promotion starts from a nearby snapshot. See
[the upstream sync runbook](sync-runbook.md).

## AI-assisted conflict resolution

`engine-resolve` (`.github/workflows/engine-resolve.yml`) is a reusable workflow that
asks a coding agent to resolve the conflict markers left by a rebase. Only the Cursor
CLI and OpenCode are supported. Claude and Anthropic are not used, and the resolver
refuses any model name containing `claude`, `anthropic`, `opus`, `sonnet`, or `haiku`.

Caller configuration:

- `NYACODE_RESOLVER_AGENT=cursor|opencode` selects the CLI and is passed as `agent`.
- `NYACODE_RESOLVER_MODEL` is the model for that CLI: a Cursor model name, or
  `provider/model` for OpenCode.
- Secret `CURSOR_API_KEY` for Cursor, or `OPENCODE_API_KEY` for OpenCode. Each secret
  is exposed only to its own step. Other provider keys are not forwarded.
- `cli_version` pins the CLI. For OpenCode it is an exact `opencode-ai` npm version, and
  the step fails unless `opencode --version` reports it. For Cursor, the installer is
  checked against `integration/codex/agent/cursor-install.sha256`, which holds
  `UNPINNED` until a reviewer commits the SHA-256 of the reviewed installer. The
  installed `agent --version` must also contain `cli_version`.

Run it locally, from a rebase output and with the CLI and key available:

```sh
python3 scripts/engine_resolve.py --dir .engine/rebase/<rev> --agent opencode \
  --model provider/model --out .engine/rebase/<rev>/resolution.patch \
  --validate-cmd "cargo check --offline --manifest-path codex-rs/Cargo.toml"
```

The resolver edits only the files the report lists as conflicted, up to `--iterations`
attempts (default 3). A rebase with more than `--max-blocks` conflict blocks (default
60), any modify/delete conflict, or a conflict without text markers exits 3 for a
human. A timeout, or failure after the last attempt, exits 4. Success exits 0 and
writes `resolution.patch` and `resolution.json` (agent, model, attempts, durations).
The workflow uploads both as the `engine-resolution` artifact.

Security model:

- The agent job has only `contents: read` and `actions: read`, with
  `persist-credentials: false`. The agent process receives a minimal environment: PATH,
  HOME, the Rust variables, and its own key. No GitHub token reaches it.
- The agent's writes are limited to the conflicted files. Its config comes from
  trusted templates in `integration/codex/agent`, read from the default branch, and is
  removed after the run. Its config denies `.cursor`, `.github`, and `opencode.json`.
  The OpenCode template sets no `ask` values, and a test asserts that. Whether OpenCode
  prompts for tools the template does not list must be verified in a live run.
- Each attempt is checked. Edits outside the conflicted files are reverted and count
  as a failure. Conflict markers must be gone; markers that already existed outside
  conflict blocks are allowed. The validation command must pass. Failure output is
  passed back, truncated, as untrusted data inside delimited blocks.
- `resolution.patch` contains only the conflicted files' diff against the rebase output.
  The resolver checks that it applies to that output. A downstream validator job must
  re-run the rebase, apply the patch, and repeat the checks without any agent
  credentials before anything is merged. This workflow publishes nothing.
- A merge that contains AI resolutions always requires human approval.

The agent may not run shell commands, so no build script ever runs with the agent's
key in its environment. The resolver runs the validation command itself, without any
agent key, and feeds failures back to the agent on the next iteration.
