# Installing and building Nyacode

Install a published release with the standalone installer, or build from source. Upstream Codex installers and packages do not contain Nyacode's native provider features.

The [release pipeline](releases.md) builds optimized packages for Linux, macOS,
and Windows on x64 and ARM64. Manual workflow runs produce downloadable test
artifacts; matching `rust-vX.Y.Z` tags publish GitHub Releases after validation.

## Install a release

On macOS or Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/yansigit/nyacode/main/scripts/install/install.sh | sh
```

On Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/yansigit/nyacode/main/scripts/install/install.ps1 | iex
```

The installers download the matching package from **yansigit/nyacode** GitHub releases, verify it against the release's SHA-256 checksums, and add the command directory to `PATH`. They never fall back to OpenAI's release service. macOS packages are not notarized and Windows packages are not Authenticode signed, so the OS may show a trust prompt.

To install a specific version, including prereleases:

```sh
curl -fsSL https://raw.githubusercontent.com/yansigit/nyacode/main/scripts/install/install.sh | sh -s -- --release 0.1.0
```

```powershell
$env:NYACODE_RELEASE = "0.1.0"; irm https://raw.githubusercontent.com/yansigit/nyacode/main/scripts/install/install.ps1 | iex
```

Both installers read these environment variables:

| Variable                  | Purpose                                                                                       |
| ------------------------- | --------------------------------------------------------------------------------------------- |
| `NYACODE_RELEASE`         | Version to install (default `latest`); `--release` / `-Release` takes precedence.             |
| `NYACODE_INSTALL_DIR`     | Command directory (default `~/.local/bin`; on Windows `%LOCALAPPDATA%\Programs\Nyacode\bin`). |
| `NYACODE_HOME`            | Private state directory (default `~/.nyacode`).                                               |
| `NYACODE_NON_INTERACTIVE` | Set to `1`, `true`, or `yes` to skip prompts.                                                 |

Legacy `CODEX_` aliases remain supported. Run the installer again to upgrade.

## Build from source

Install Git, the GitHub CLI (`gh`), Python 3.11 or newer, the pinned Rust toolchain (via rustup), and your platform's C/C++ build tools. Linux builds also need `pkg-config`, OpenSSL development headers, Clang/libclang, CMake, and Protocol Buffers. On Ubuntu:

```sh
sudo apt-get install build-essential pkg-config libssl-dev clang libclang-dev cmake protobuf-compiler
gh repo clone yansigit/nyacode
cd nyacode
python3 scripts/build_nyacode.py
./.engine/target/debug/nyacode
```

The build helper verifies the pinned V8 archive and bindings before building both executable names and the Code Mode host. The default V8 crate download does not provide every artifact used by this fork.

The build first assembles the pinned engine in `.engine/workspace`; its source and locked dependencies are disposable. Edit owned crates, overlays, or patches in the product checkout. See [engine development](engine.md) for qualification and regeneration commands.

For an optimized installation into `~/.local/bin` on macOS or Linux:

```sh
python3 scripts/build_nyacode.py --release
mkdir -p ~/.local/bin
install -m 755 .engine/target/release/nyacode .engine/target/release/codex-code-mode-host ~/.local/bin/
nyacode --help
```

Copy the built `codex` binary too if you need the legacy command. Ensure the install directory is on `PATH`. Crate names and source directory names intentionally retain their upstream names.

## State directory

Nyacode defaults to `~/.nyacode`. To use another existing directory:

```sh
mkdir -p /path/to/nyacode-home
NYACODE_HOME=/path/to/nyacode-home nyacode
```

`NYACODE_HOME` takes precedence over the legacy `CODEX_HOME` variable. Explicit paths must exist and be directories. Empty overrides are ignored. No state is copied or deleted when switching homes.

## Standalone installers and npm

The [one-line installs](#install-a-release) fetch the checked-in `scripts/install/install.sh` and `install.ps1`. They retain the native archive layout for compatibility. An explicit `--release` version must already be published on GitHub.

The same installers can run from a checkout:

```sh
sh scripts/install/install.sh --release latest
```

On Windows, run `pwsh -File scripts/install/install.ps1 -Release latest`.
Release archives can also be downloaded from GitHub; retain their directory
layout so the CLI can find its bundled helpers.

The npm launcher and TypeScript SDK target `@yansigit/nyacode` and `@yansigit/nyacode-sdk`. These names are distribution configuration, not a claim that a package is already published. There is no Nyacode Homebrew cask configured.

## Development checks

```sh
cargo install --locked just cargo-nextest dotslash
just nyacode --help
just fmt-check
just clippy -p codex-cli
just test -p codex-cli
```

`just codex` and the internal `codex-*` build targets remain compatible aliases. Run affected crate tests; full workspace builds need substantially more resources.

For a repeatable Ubuntu 26.04 run, use the container verifier. It pins Rust 1.95, leaves the host home unmounted, and caches Cargo outputs between runs:

```sh
scripts/verify-container.sh fast
scripts/verify-container.sh
```

Full workspace checks, cache volumes, and container requirements are described in [container verification](container-verify.md).

### Engine generator prerequisites

`engine.py generate` regenerates schemas, lockfiles, and the Bazel lock inside an assembled engine workspace. Before it runs any generator, it checks that each selected generator's tools are on `PATH` and reports every missing tool once. `clang` also counts as present when `LIBCLANG_PATH` names a directory that holds libclang. The versions below are the CI pins:

| Tool               | Version                                                 | Generators                                                         |
| ------------------ | ------------------------------------------------------- | ------------------------------------------------------------------ |
| Rust toolchain     | the `toolchain` in `engine.lock`, installed with rustup | `app-server-schema`, `config-schema`, `hooks-schema`, `cargo-lock` |
| Node.js            | 26.11.1                                                 | `pnpm-lock`                                                        |
| pnpm               | 12.11.2, the `packageManager` pin in `package.json`     | `pnpm-lock`                                                        |
| uv                 | 0.13.0                                                  | `app-server-schema`, `uv-lock`                                     |
| Bazelisk           | 1.29.0, which reads `.bazelversion`                     | `bazel-lock`                                                       |
| cmake              | apt `cmake`                                             | cargo generators (builds `aws-lc-sys`)                             |
| clang and libclang | apt `clang libclang-dev`, or `LIBCLANG_PATH`            | cargo generators (`bindgen` in `btls-sys`)                         |
| cargo-insta        | `cargo install --locked cargo-insta`                    | `snapshots` (opt-in)                                               |

On ubuntu-24.04, `.github/actions/setup-engine-generators` installs these with the same pins. The `generated` job in `engine-qualification.yml` and the `finish` job in `engine-sync.yml` both use it. Outside CI, install the same tools with your platform's package manager.

## Logging

Nyacode honors `RUST_LOG`. The TUI records diagnostics in bounded local stores by default. To enable plaintext logging:

```sh
nyacode -c log_dir=./.nyacode-log
tail -F ./.nyacode-log/codex-tui.log
```

`nyacode exec` defaults to `RUST_LOG=error` and prints messages inline. Internal log filenames remain compatible.
