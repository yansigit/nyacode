<p align="center"><img src=".github/nyacode-logo.svg" width="100" alt="Nyacode" /></p>

# Nyacode

Nyacode is a multi-provider coding agent for your terminal, forked from [OpenAI Codex](https://github.com/openai/codex). It connects directly to Cursor, OpenCode Go, Claude, and Antigravity, alongside the existing OpenAI-compatible providers.

- Manage native provider accounts and automatic account pools with `nyacode provider` or `/providers`.
- Run tools, approvals, resumable conversations, and child agents with provider and model overrides.
- Keep private configuration, credentials, and conversation state in `~/.nyacode`.

Account switching is bounded by verified continuation safety. See the [provider coverage and acceptance records](docs/native-providers/README.md) for supported routes and remaining limitations.

## Build and run

Build from source while Nyacode release packages are being prepared:

```sh
git clone https://github.com/yansigit/nyacode.git
cd nyacode
python3 scripts/build_nyacode.py
./codex-rs/target/debug/nyacode --help
./codex-rs/target/debug/nyacode
```

The repository pins its Rust toolchain. See [installation and development](docs/install.md) for prerequisites and installation instructions.

To rehearse the pinned engine assembly, run `python3 scripts/build_nyacode.py --engine`. It writes under `.engine/workspace` and does not change the default build or release packages. See [engine migration](docs/engine.md).

```sh
nyacode provider --help
nyacode provider accounts cursor
nyacode provider pool cursor status
```

Use `/providers` in the interactive UI to manage accounts. A second enrolled account automatically activates its provider pool unless you explicitly disabled it. Existing conversations retain their working account.

## Configuration and compatibility

`NYACODE_HOME` overrides the private state directory. `CODEX_HOME` remains a compatibility alias; `NYACODE_HOME` takes precedence. No credentials or conversations are migrated automatically.

The `codex` executable remains available as a compatibility entrypoint. Internal Rust crate names, SDK types, provider IDs, protocol identifiers, project `.codex` configuration, and stored state formats remain compatible. See the [rebranding boundaries](docs/rebranding.md).

## Documentation

- [Native providers and verification](docs/native-providers/README.md)
- [Account pools](docs/native-providers/account-pools.md)
- [Multi-provider child agents](docs/native-providers/multi-provider-agents.md)
- [Installation and development](docs/install.md)
- [Reporting issues](docs/contributing.md)

Nyacode is an independent fork, not an official OpenAI distribution. Upstream acknowledgements are retained in [NOTICE](NOTICE). Licensed under [Apache-2.0](LICENSE).
