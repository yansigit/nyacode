set working-directory := "."
set positional-arguments
export CODEX_REPO_ROOT := justfile_directory() / ".engine/workspace"
export CARGO_TARGET_DIR := env("CARGO_TARGET_DIR", justfile_directory() / ".engine/target")
export JUST_SHELL := justfile_directory() / "scripts/just-shell.py"
set shell := ["python3", "-c", 'import os, runpy; runpy.run_path(os.environ["JUST_SHELL"], run_name="__main__")']
set windows-shell := ["python", "-c", 'import os, runpy; runpy.run_path(os.environ["JUST_SHELL"], run_name="__main__")']

rust_min_stack := "8388608" # 8 MiB
python := if os_family() == "windows" { "python" } else { "python3" }
engine_run := python + " " + quote(justfile_directory() / "scripts/engine_run.py")

# Display help
help:
    just -l

# Run Nyacode; retain `just codex` as a compatibility alias.
alias c := nyacode
alias codex := nyacode
nyacode *args:
    {{ engine_run }} cargo run --bin nyacode -- {args}

# `codex exec`
exec *args:
    {{ engine_run }} cargo run --bin nyacode -- exec {args}

# Start `codex exec-server` and run codex-tui.
[no-cd]
[positional-arguments]
[unix]
tui-with-exec-server *args:
    {{ justfile_directory() }}/scripts/run_tui_with_exec_server.sh "$@"

# Run the CLI version of the file-search crate.
file-search *args:
    {{ engine_run }} cargo run --bin codex-file-search -- {args}

# Run the standalone code-mode host from source.
code-mode-host *args:
    {{ engine_run }} cargo run --bin codex-code-mode-host -- {args}

# Assemble a local Codex package.
[no-cd]
assemble-codex-package *args:
    {{ engine_run }} --workspace-root {{ python }} scripts/build_codex_package.py {args}

# Build the CLI and run the app-server test client
app-server-test-client *args:
    {{ engine_run }} cargo build -p codex-cli
    {{ engine_run }} cargo run -p codex-app-server-test-client -- --codex-bin {{ quote(env("CARGO_TARGET_DIR", justfile_directory() / ".engine/target") / "debug/codex") }} {args}

# Format the justfile, Rust, Bazel/Starlark, Python SDK code, and Python scripts.
fmt:
    @{{ python }} {{ justfile_directory() }}/scripts/format.py

# Check formatting without modifying files.
fmt-check:
    @{{ python }} {{ justfile_directory() }}/scripts/format.py --check

# Edits the disposable assembly; copy changes into owned inputs before preparing again.
fix *args:
    {{ engine_run }} cargo clippy --fix --tests --allow-dirty {args}

clippy *args:
    {{ engine_run }} cargo clippy --tests {args}

[unix]
install:
    rustup show active-toolchain
    {{ engine_run }} cargo fetch

[windows]
install:
    #!powershell.exe -File
    $pwsh = Get-Command pwsh.exe -ErrorAction SilentlyContinue
    if (-not $pwsh) {
        winget install --exact --id Microsoft.PowerShell --source winget --accept-package-agreements --accept-source-agreements
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    rustup show active-toolchain
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    {{ engine_run }} cargo fetch
    exit $LASTEXITCODE

# Run nextest with --no-fail-fast so all tests are run.
#
# Run `cargo install --locked cargo-nextest` if you don't have it installed.
# Prefer this for routine local runs. Workspace crate features are banned, so
# there should be no need to add `--all-features`.
[unix]
test *args:
    RUST_MIN_STACK={{ rust_min_stack }} NEXTEST_PROFILE=local {{ engine_run }} cargo nextest run --no-fail-fast "$@"

[windows]
test *args:
    $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "local"; {{ engine_run }} cargo nextest run --no-fail-fast @($args | Select-Object -Skip 1)

# Run from the repository root so scripts that resolve paths from `cwd` see
# the same layout they use in GitHub Actions.
[no-cd]
test-github-scripts:
    {{ engine_run }} --workspace-root {{ python }} -m unittest discover -s {{ justfile_directory() }}/.github/scripts -p 'test_*.py'

# Run explicit workspace benchmark targets.
bench *args:
    {{ engine_run }} cargo bench --workspace --bench '*' {args}

# Run benchmark targets once to ensure they start successfully.
bench-smoke:
    just bench -- --test

# Run Bazel-backed end-to-end macrobenchmarks with optimized binaries.
bench-e2e:
    {{ engine_run }} --workspace-root just --justfile justfile bench-e2e

# Run Bazel-backed end-to-end macrobenchmarks once per case with release-like
# Rust cfg paths but fastbuild codegen.
bench-e2e-smoke:
    {{ engine_run }} --workspace-root just --justfile justfile bench-e2e-smoke

# Build and run Codex from source using Bazel.
# On Unix, use `[no-cd]` and `--run_under="cd $PWD &&"` to ensure Bazel runs
# the command in the current working directory.
[no-cd]
[unix]
bazel-codex *args:
    {{ engine_run }} --workspace-root bazel run //codex-rs/cli:codex --run_under={{ quote("cd " + quote(invocation_directory()) + " &&") }} -- {args}

[windows]
bazel-codex *args:
    {{ engine_run }} --workspace-root bazel run //codex-rs/cli:codex --run_under='cd /d "{{ invocation_directory_native() }}" &&' -- {args}

# Build and run the standalone code-mode host from source using Bazel.
[no-cd]
[unix]
bazel-code-mode-host *args:
    {{ engine_run }} --workspace-root bazel run //codex-rs/code-mode-host:codex-code-mode-host --run_under={{ quote("cd " + quote(invocation_directory()) + " &&") }} -- {args}

[windows]
bazel-code-mode-host *args:
    {{ engine_run }} --workspace-root bazel run //codex-rs/code-mode-host:codex-code-mode-host --run_under='cd /d "{{ invocation_directory_native() }}" &&' -- {args}

# Edits the disposable assembly; copy changes into owned inputs before preparing again.
[no-cd]
bazel-lock-update:
    {{ engine_run }} --workspace-root just --justfile justfile bazel-lock-update

[no-cd]
[unix]
bazel-lock-check:
    {{ engine_run }} --workspace-root just --justfile justfile bazel-lock-check

[windows]
bazel-lock-check:
    {{ engine_run }} --workspace-root just --justfile justfile bazel-lock-check

bazel-test:
    {{ engine_run }} --workspace-root just --justfile justfile bazel-test

[no-cd]
[unix]
bazel-clippy:
    {{ engine_run }} --workspace-root just --justfile justfile bazel-clippy

[no-cd]
[unix]
bazel-argument-comment-lint:
    {{ engine_run }} --workspace-root just --justfile justfile bazel-argument-comment-lint

build-for-release:
    {{ engine_run }} --workspace-root just --justfile justfile build-for-release

# Regenerate the json schema for config.toml from the current config types.
# Edits the disposable assembly; copy changes into owned inputs before preparing again.
write-config-schema:
    {{ engine_run }} cargo run -p codex-config-schema --bin codex-write-config-schema

# Regenerate app-server protocol schemas and the Python SDK derived from them.
# Edits the disposable assembly; copy changes into owned inputs before preparing again.
write-app-server-schema *args:
    {{ engine_run }} {{ python }} app-server-protocol/scripts/write_schema_fixtures.py {args}

# Prepare the pinned upstream engine into the assembly workspace.
[no-cd]
prepare-engine *args:
    {{ python }} {{ justfile_directory() }}/scripts/engine.py prepare {args}

# Run the standalone provider workspace suite.
[working-directory("crates/providers")]
test-providers:
    cargo test --locked --all-features

# Run strict engine assembly and native-provider qualification checks.
[no-cd]
engine-checks *args:
    {{ python }} {{ justfile_directory() }}/scripts/engine.py check --mode full {args}

# Regenerate schemas and SDK bindings from the assembled engine, then check compatibility.
[no-cd]
engine-schema *args: prepare-engine
    {{ python }} {{ justfile_directory() }}/.engine/workspace/codex-rs/app-server-protocol/scripts/write_schema_fixtures.py {args}
    {{ python }} {{ justfile_directory() }}/scripts/engine_compatibility.py --workspace {{ justfile_directory() }}/.engine/workspace

# Check pinned engine inputs, assembly schema, and engine control tests.
[no-cd]
engine-controls:
    {{ python }} {{ justfile_directory() }}/scripts/engine.py check --mode controls

# Regenerate integration/codex/assembly.json and inventory/downstream.json from ownership metadata.
[no-cd]
engine-manifest:
    {{ python }} {{ justfile_directory() }}/scripts/engine.py manifest --write

# Carry the patch stack onto an upstream revision (40-hex commit); exits 2 on conflicts.
[no-cd]
engine-rebase REV *args:
    {{ python }} {{ justfile_directory() }}/scripts/engine.py rebase --onto {{ quote(REV) }} {args}

# Write the reviewed rebase workspace back into patches (pass --workspace DIR/workspace).
[no-cd]
engine-export *args:
    {{ python }} {{ justfile_directory() }}/scripts/engine.py export {args}

# Edits the disposable assembly; copy changes into owned inputs before preparing again.
[no-cd]
write-hooks-schema:
    {{ engine_run }} cargo run -p codex-hooks --bin write_hooks_schema_fixtures

# Run the argument-comment Dylint checks across codex-rs.
[no-cd]
[unix]
argument-comment-lint *args:
    {{ engine_run }} --workspace-root just --justfile justfile argument-comment-lint {args}

[no-cd]
argument-comment-lint-from-source *args:
    {{ engine_run }} --workspace-root just --justfile justfile argument-comment-lint-from-source {args}

# Tail logs from the state SQLite database
[unix]
log *args:
    if [ "${1:-}" = "--" ]; then shift; fi; {{ engine_run }} cargo run -p codex-cli --bin logs_client -- "$@"

[windows]
log *args:
    $forwarded_args = @($args | Select-Object -Skip 1); if ($forwarded_args.Count -gt 0 -and $forwarded_args[0] -eq "--") { $forwarded_args = @($forwarded_args | Select-Object -Skip 1) }; {{ engine_run }} cargo run -p codex-cli --bin logs_client -- @forwarded_args
