# Nyacode identity and compatibility

Nyacode is maintained at <https://github.com/yansigit/nyacode> as an independent fork of OpenAI Codex. Public CLI/TUI labels, command help, terminal titles, documentation, npm distribution names, installer destinations, and update checks use Nyacode.

## Preserved interfaces

- `nyacode` is the primary command; `codex` is a compatibility executable sharing the same implementation.
- `NYACODE_HOME`, then `CODEX_HOME`, then `~/.nyacode` determines private state. Overrides must already exist. Switching defaults does not import or delete old state.
- Python distribution/import names, Rust workspace paths, crate names, exported SDK types, provider identifiers, RPC methods, and JSON field names retain upstream spellings.
- Provider credentials, account generations, namespace hashes, checkpoints, execution locks, and tool/handoff journals are unchanged.
- Project `.codex` directories, `codex-package.json`, package resource/helper names, protocol headers, OAuth client registrations, and other `CODEX_*` variables remain supported. The new home name does not change project trust or sandbox rules.
- References to OpenAI models, services, the Codex IDE extension, the official desktop application, historical acceptance evidence, licenses, and upstream source links retain their actual names.

## Distribution

Build from source using [the installation guide](install.md). Nyacode's standalone installers and updater only use this fork's GitHub releases or npm packages; they do not download an upstream build as a fallback. Native release asset and package layout names remain compatible. Source changes and a repository rename do not publish a release, npm package, Homebrew cask, or signed desktop application.

Upstream release automation requiring OpenAI infrastructure remains gated to the upstream repository. Nyacode's hosted checks run separately. Signing and package publication need a fork-specific release setup before distribution.

## Existing installations

To explicitly reuse an existing home, launch with `NYACODE_HOME=/path/to/existing/home`. Both executable names use the same home resolution. Keep `NYACODE_HOME` unset when testing the legacy `CODEX_HOME` alias. Existing private records remain readable; no automatic migration is performed.

## Verification

The Linux rebrand checks cover the real CLI's help, version, completion, home precedence, symlinked homes, legacy aliases, and dotenv isolation. A staged npm package exercises both launchers and npm/Vite+ ownership detection. Installer fixtures verify fork-only downloads, checksums, package layouts, and interrupted updates. These checks use temporary homes and no real credentials.

The source-build helper successfully builds both command names and the Code Mode host. All seven CLI/launcher regressions, 16 installer tests, and 27 package-builder tests pass. The TypeScript SDK builds and passes all 49 tests, including streaming, cancellation, and MCP conformance. The TUI suite passes 5,743 tests (four skipped); the final CLI/auth/reconnect rerun passes all 539 selected tests. Affected home, config, daemon, exec, and install-context crates are checked separately. The affected crates pass Clippy with warnings denied. Formatting, the provider architecture suite (18 tests), three affected Bazel test targets, and Bazel analysis of `//codex-rs/cli:nyacode` are covered.

The upstream Cargo manifest policy check still rejects native-provider feature declarations and a stale Code Mode exception. Its output is identical on the pre-rebrand parent `cb01e6e8a` and this change. The check remains enabled; this rebrand does not remove provider feature configurations to satisfy it.

Windows and macOS execution, signing, and published package installation have not been verified by this Linux run. No release or npm package is published by the rebrand. Provider live-acceptance records remain separate from these branding checks.
