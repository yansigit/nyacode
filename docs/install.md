# Installing and building Nyacode

Build from source until this fork publishes its own release packages. Upstream Codex installers and packages do not contain Nyacode's native provider features.

The [release pipeline](releases.md) builds optimized packages for Linux, macOS,
and Windows on x64 and ARM64. Manual workflow runs produce downloadable test
artifacts; matching `rust-vX.Y.Z` tags publish GitHub Releases after validation.

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

The checked-in `scripts/install/install.sh` and `install.ps1` install the `nyacode` command from **yansigit/nyacode** GitHub releases. They retain the native archive layout for compatibility and never fall back to OpenAI's release service. They require a matching release to have been published first.

Once a release is published, run the installer from this checkout:

```sh
sh scripts/install/install.sh --release latest
```

On Windows, run `pwsh -File scripts/install/install.ps1 -Release latest`.
To select a prerelease, replace `latest` with its explicit version. Release
archives can also be downloaded from GitHub; retain their directory layout so
the CLI can find its bundled helpers.

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

## Logging

Nyacode honors `RUST_LOG`. The TUI records diagnostics in bounded local stores by default. To enable plaintext logging:

```sh
nyacode -c log_dir=./.nyacode-log
tail -F ./.nyacode-log/codex-tui.log
```

`nyacode exec` defaults to `RUST_LOG=error` and prints messages inline. Internal log filenames remain compatible.
