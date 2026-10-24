#!/usr/bin/env bash
# Run Nyacode's Rust verification inside a Ubuntu 26.04 container.
#
#   scripts/verify-container.sh fast
#   scripts/verify-container.sh
#
# Docker and Podman both work. The host home directory is not mounted.
# See docs/container-verify.md.

set -euo pipefail

IMAGE_TAG="nyacode-verify:ubuntu-26.04-rust-1.95"
VOLUME_REGISTRY="nyacode-verify-cargo-registry"
VOLUME_GIT="nyacode-verify-cargo-git"
VOLUME_TARGET="nyacode-verify-target"
VOLUME_SCCACHE="nyacode-verify-sccache"
CONTAINER_HOME="/home/verifier"
CONTAINER_SRC="/src"
CONTAINER_TARGET="/var/cache/nyacode-target"
VOLUME_V8="nyacode-verify-v8"

usage() {
    cat <<'EOF'
Usage: scripts/verify-container.sh [options] [full|fast|run -- command]

  full    fmt, dependency suites, clippy, debug build, nextest, and CLI smoke checks (default)
  fast    fmt, then clippy and nextest for crates changed since origin/main
  run     run a command in the verification container

Options:
  --engine docker|podman   container engine (default: the first one that responds)
  --engine-workspace       compatibility flag; the assembled workspace is always used
  --reset-cache            delete Cargo, target, and sccache volumes first
  --no-cache               rebuild the image without layer cache
  -h, --help               show this help
EOF
}

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)"

refuse_if_home_is_mounted() {
    local target
    target="$(findmnt -n -T "$HOME" -o TARGET 2>/dev/null || true)"
    if [[ "$target" == "$HOME" ]]; then
        echo "refusing to run: $HOME is a mount point. Sandbox tests bind-mount the home directory, and a nested host-home mount crashes that run." >&2
        exit 1
    fi
}

configure_inner_env() {
    export HOME="${HOME:-$CONTAINER_HOME}"
    export CARGO_HOME="${CARGO_HOME:-/usr/local/cargo}"
    export RUSTUP_HOME="${RUSTUP_HOME:-/usr/local/rustup}"
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$CONTAINER_TARGET}"
    export SCCACHE_DIR="${SCCACHE_DIR:-/var/cache/sccache}"
    # Host networking must not attach to a developer's existing cache server.
    export SCCACHE_SERVER_UDS="$HOME/.sccache.sock"
    export RUSTC_WRAPPER="${RUSTC_WRAPPER:-sccache}"
    export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
    export CARGO_NET_GIT_FETCH_WITH_CLI="${CARGO_NET_GIT_FETCH_WITH_CLI:-true}"
    export RUST_MIN_STACK="${RUST_MIN_STACK:-67108864}"
    export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"
    # Keep panic traces without changing anyhow's user-facing error output.
    export RUST_LIB_BACKTRACE=0
    export TMPDIR="${TMPDIR:-/tmp}"
    mkdir -p "$HOME" "$CARGO_TARGET_DIR" "$SCCACHE_DIR" "$TMPDIR"
    if ! git config --global --get-all safe.directory 2>/dev/null | grep -qx "$ROOT"; then
        git config --global --add safe.directory "$ROOT"
    fi
    ulimit -s 65536 || echo "warning: could not raise the stack ulimit (current soft limit: $(ulimit -s) KiB)" >&2
    if [[ -z "${CARGO_BUILD_JOBS:-}" ]]; then
        local mem_kb jobs cpus
        mem_kb="$(awk '/MemAvailable:/ {print $2}' /proc/meminfo)"
        jobs=$((mem_kb / 2500000))
        cpus="$(nproc)"
        if ((jobs < 1)); then
            jobs=1
        fi
        if ((jobs > cpus)); then
            jobs=$cpus
        fi
        export CARGO_BUILD_JOBS="$jobs"
    fi
    if [[ "$RUSTC_WRAPPER" == "sccache" ]]; then
        sccache --start-server
        sccache --zero-stats >/dev/null
    fi
}

prepare_v8() {
    local host prepared
    host="$(rustc -vV | sed -n 's/^host: //p')"
    prepared="$(python3 "$ROOT/scripts/verify_container.py" --root "$WORKSPACE_ROOT" prepare-v8 --cache /var/cache/nyacode-v8 --target "$host")"
    local paths=()
    mapfile -t paths <<<"$prepared"
    [[ ${#paths[@]} == 2 ]]
    export RUSTY_V8_ARCHIVE="${paths[0]}"
    export RUSTY_V8_SRC_BINDING_PATH="${paths[1]}"
}

run_helpers() {
    cargo build --locked -p codex-cli -p codex-code-mode-host -p codex-rmcp-client --bins
}

run_sandbox_check() {
    cargo build --locked -p codex-bwrap --bin bwrap
    # Exercise the bundled helper, which may have different AppArmor rules
    # from the distribution's /usr/bin/bwrap. Fail before the full build.
    "${CARGO_TARGET_DIR}/debug/bwrap" \
        --unshare-user --unshare-pid --unshare-net --uid 0 --gid 0 \
        --ro-bind / / --dev /dev --proc /proc --die-with-parent \
        -- python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); s.close()'
}

phase() {
    local name="$1"
    shift
    local start end elapsed
    start="$(date +%s)"
    echo "==> ${name}"
    "$@"
    end="$(date +%s)"
    elapsed="$((end - start))"
    echo "==> ${name} done in ${elapsed}s"
    printf '%s\t%s\n' "$name" "$elapsed" >>"$TIMING_LOG"
}

run_fmt() {
    cargo fmt -- --config imports_granularity=Item --check
}

run_clippy() {
    cargo clippy --locked --workspace --features codex-v8-poc/sandbox --tests --keep-going -- -D warnings
}

run_build() {
    cargo build --locked --workspace --features codex-v8-poc/sandbox --bins --tests --keep-going
}

run_dependency_tests() {
    local package
    for package in aws-config aws-sigv4; do
        (
            # Upstream fixtures are relative to the package, and one logging test expects ANSI.
            cd "$WORKSPACE_ROOT/third_party/rust/$package"
            env -u NO_COLOR cargo test --locked --all-features \
                --manifest-path "$PWD/Cargo.toml" -- --test-threads=2
        )
    done
}

run_tests() {
    CODEX_EXEC_PATH="${CARGO_TARGET_DIR}/debug/codex" \
        cargo nextest run --locked --workspace --features codex-v8-poc/sandbox --no-fail-fast --test-threads "${CARGO_BUILD_JOBS}"
}

run_cli_smoke() {
    local cli="${CARGO_TARGET_DIR}/debug/nyacode"
    if [[ ! -x "$cli" ]]; then
        echo "nyacode binary not found at $cli" >&2
        exit 1
    fi
    "$cli" --version
    "$cli" --help >/dev/null
}

print_timing_summary() {
    echo "==> timings"
    if [[ -s "$TIMING_LOG" ]]; then
        awk -F '\t' '{printf "  %-12s %ss\n", $1, $2}' "$TIMING_LOG"
    fi
    if [[ "${RUSTC_WRAPPER:-}" == "sccache" ]]; then
        echo "==> sccache"
        sccache --show-stats || true
    fi
}

run_inner() {
    local mode="${1:-full}"
    WORKSPACE_ROOT="$ROOT/.engine/workspace"
    python3 "$ROOT/scripts/engine.py" --root "$ROOT" prepare --output "$WORKSPACE_ROOT"
    if [[ ! -f "$WORKSPACE_ROOT/codex-rs/Cargo.toml" ]]; then
        echo "engine preparation did not produce $WORKSPACE_ROOT" >&2
        exit 1
    fi
    export CODEX_REPO_ROOT="$WORKSPACE_ROOT"
    export NYACODE_ENGINE_ROOT="$WORKSPACE_ROOT"
    export RUSTUP_TOOLCHAIN="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["toolchain"])' "$ROOT/engine.lock")"
    configure_inner_env
    refuse_if_home_is_mounted
    TIMING_LOG="$(mktemp)"
    trap 'status=$?; if ((status != 0)); then print_timing_summary || true; fi' EXIT
    echo "rustc: $(rustc --version)"
    echo "cargo: $(command -v cargo) ($(cargo --version))"
    echo "HOME=${HOME} (mount target: $(findmnt -n -T "$HOME" -o TARGET))"
    echo "CARGO_TARGET_DIR=${CARGO_TARGET_DIR}"
    echo "CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}"
    echo "RUST_MIN_STACK=${RUST_MIN_STACK} ulimit_stack_kib=$(ulimit -s)"
    cd "$WORKSPACE_ROOT/codex-rs"

    case "$mode" in
        full)
            phase inputs prepare_v8
            phase sandbox run_sandbox_check
            phase fmt run_fmt
            phase dependencies run_dependency_tests
            phase clippy run_clippy
            phase build run_build
            phase test run_tests
            phase smoke run_cli_smoke
            ;;
        fast)
            phase fmt run_fmt
            local affected
            affected="$(python3 "$ROOT/scripts/verify_container.py" --root "$ROOT" --workspace "$WORKSPACE_ROOT" affected)"
            local packages=()
            local package
            while IFS= read -r package; do
                if [[ -n "$package" ]]; then
                    packages+=("$package")
                fi
            done <<<"$affected"
            if ((${#packages[@]} == 0)); then
                echo "==> no affected crates; skipping clippy and tests"
            else
                echo "==> affected: ${packages[*]}"
                local -a package_args=()
                for package in "${packages[@]}"; do
                    package_args+=(-p "$package")
                    if [[ "$package" == "codex-v8-poc" ]]; then
                        package_args+=(--features codex-v8-poc/sandbox)
                    fi
                done
                phase inputs prepare_v8
                phase sandbox run_sandbox_check
                phase clippy cargo clippy --locked --tests "${package_args[@]}" -- -D warnings
                phase helpers run_helpers
                local -a test_args=(nextest run --locked --no-fail-fast --test-threads "${CARGO_BUILD_JOBS}" "${package_args[@]}")
                phase test cargo "${test_args[@]}"
            fi
            ;;
        run)
            shift
            if [[ "${1:-}" == "--" ]]; then
                shift
            fi
            if (($# == 0)); then
                echo "run requires a command" >&2
                exit 2
            fi
            phase inputs prepare_v8
            phase run "$@"
            ;;
        *)
            echo "unknown mode: $mode" >&2
            usage >&2
            exit 2
            ;;
    esac
    print_timing_summary
}

engine_works() {
    "$@" info >/dev/null 2>&1
}

select_engine() {
    local requested="${1:-}"
    local candidate
    ENGINE=()
    if [[ -n "$requested" ]]; then
        if engine_works "$requested"; then
            ENGINE=("$requested")
        elif engine_works sudo -n "$requested"; then
            ENGINE=(sudo -n "$requested")
        else
            echo "container engine '$requested' is not available" >&2
            exit 1
        fi
        return
    fi
    for candidate in docker podman; do
        if engine_works "$candidate"; then
            ENGINE=("$candidate")
            return
        fi
        if engine_works sudo -n "$candidate"; then
            ENGINE=(sudo -n "$candidate")
            return
        fi
    done
    echo "docker or podman is required" >&2
    exit 1
}

assert_mount_is_not_host_home() {
    local spec="$1"
    local source="${spec%%:*}"
    local home
    if [[ "$source" != /* ]]; then
        return
    fi
    home="$(CDPATH= cd -- "$HOME" && pwd -P)"
    if [[ "$source" == "$home" || "$home" == "$source"/* || "$source" == / ]]; then
        echo "refusing to mount host home path $source" >&2
        exit 1
    fi
}

run_outer() {
    local engine_name=""
    local reset_cache=0
    local no_cache=0
    local engine_workspace=0
    local -a mode_args=()
    while (($#)); do
        case "$1" in
            --engine)
                engine_name="${2:-}"
                if [[ -z "$engine_name" ]]; then
                    echo "--engine requires docker or podman" >&2
                    exit 2
                fi
                shift 2
                ;;
            --engine-workspace)
                engine_workspace=1
                shift
                ;;
            --reset-cache)
                reset_cache=1
                shift
                ;;
            --no-cache)
                no_cache=1
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            --)
                mode_args+=("$@")
                break
                ;;
            *)
                mode_args+=("$1")
                shift
                ;;
        esac
    done
    if ((${#mode_args[@]} == 0)); then
        mode_args=(full)
    fi

    select_engine "$engine_name"
    local engine_bin="${ENGINE[-1]}"
    echo "engine: ${ENGINE[*]}"

    local rust_version
    rust_version="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["toolchain"])' "$ROOT/engine.lock")"
    local -a build_args=(build --network=host --build-arg "RUST_VERSION=$rust_version" -f "$ROOT/Containerfile" -t "$IMAGE_TAG")
    if ((no_cache)); then
        build_args+=(--no-cache)
    fi
    build_args+=("$ROOT")
    local build_start build_end
    build_start="$(date +%s)"
    "${ENGINE[@]}" "${build_args[@]}"
    build_end="$(date +%s)"
    echo "==> image build done in $((build_end - build_start))s"

    if ((reset_cache)); then
        "${ENGINE[@]}" volume rm -f "$VOLUME_REGISTRY" "$VOLUME_GIT" "$VOLUME_TARGET" "$VOLUME_SCCACHE" "$VOLUME_V8" >/dev/null
    fi

    local -a mounts=(
        "${ROOT}:${CONTAINER_SRC}"
        "${VOLUME_REGISTRY}:/usr/local/cargo/registry"
        "${VOLUME_GIT}:/usr/local/cargo/git"
        "${VOLUME_TARGET}:${CONTAINER_TARGET}"
        "${VOLUME_SCCACHE}:/var/cache/sccache"
        "${VOLUME_V8}:/var/cache/nyacode-v8"
    )
    # Linked worktrees need only the common Git metadata, never the host home.
    local git_common
    git_common="$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir)"
    if [[ "$git_common" != "$ROOT/.git" ]]; then
        mounts+=("${git_common}:${git_common}:ro")
    fi
    local spec
    for spec in "${mounts[@]}"; do
        assert_mount_is_not_host_home "$spec"
    done

    local -a run_args=(
        run --rm
        --user "$(id -u):$(id -g)"
        -e NYACODE_VERIFY_INNER=1
        -e HOME="$CONTAINER_HOME"
        -e CARGO_HOME=/usr/local/cargo
        -e RUSTUP_HOME=/usr/local/rustup
        -e CARGO_TARGET_DIR="$CONTAINER_TARGET"
        -e SCCACHE_DIR=/var/cache/sccache
        -e RUSTC_WRAPPER=sccache
        -e CARGO_INCREMENTAL=0
        -e CARGO_PROFILE_DEV_DEBUG=0
        -e CARGO_PROFILE_TEST_DEBUG=0
        -e CARGO_NET_GIT_FETCH_WITH_CLI=true
        -e CARGO_TERM_COLOR=always
        -e RUST_MIN_STACK=67108864
        -e RUST_BACKTRACE=1
        -e RUST_LIB_BACKTRACE=0
        -e TMPDIR=/tmp
        -w "$CONTAINER_SRC"
        --ulimit "stack=67108864:67108864"
        --cap-add SYS_ADMIN
        --cap-add NET_ADMIN
        --cap-add SYS_PTRACE
        --security-opt seccomp=unconfined
    )
    if [[ -n "${GH_TOKEN:-}" ]]; then
        run_args+=(-e GH_TOKEN)
    fi
    if [[ -n "${CARGO_BUILD_JOBS:-}" ]]; then
        run_args+=(-e "CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}")
    fi
    if ((engine_workspace)); then
        run_args+=(-e NYACODE_VERIFY_ENGINE_WORKSPACE=1)
    fi
    if [[ "$engine_bin" == "podman" ]]; then
        run_args+=(--userns keep-id --security-opt label=disable --security-opt 'unmask=/proc/*')
    else
        # Docker can enforce AppArmor even when securityfs is not visible to
        # this caller. "unconfined" also works when AppArmor is unavailable.
        # Locked masks in the parent /proc prevent an unprivileged nested
        # PID namespace from mounting its own proc filesystem.
        run_args+=(--security-opt apparmor=unconfined --security-opt systempaths=unconfined)
    fi
    for spec in "${mounts[@]}"; do
        run_args+=(-v "$spec")
    done
    run_args+=("$IMAGE_TAG" "${CONTAINER_SRC}/scripts/verify-container.sh" "${mode_args[@]}")

    local run_start run_end
    run_start="$(date +%s)"
    "${ENGINE[@]}" "${run_args[@]}"
    run_end="$(date +%s)"
    echo "==> container run done in $((run_end - run_start))s"
}

if [[ "${NYACODE_VERIFY_INNER:-}" == "1" ]]; then
    run_inner "$@"
else
    run_outer "$@"
fi
