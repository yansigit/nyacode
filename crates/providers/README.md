# Native providers

This workspace owns the nine engine-independent provider crates. It preserves
their existing crate names, APIs, dependency requirements, tests, and fixtures.
The Codex adapter is owned separately at `integration/codex/adapter` because it
depends on engine APIs.

Run the providers without compiling Codex:

```sh
cargo test --manifest-path crates/providers/Cargo.toml --locked
cargo test --manifest-path crates/providers/Cargo.toml --locked --all-features
```

The workspace uses the same Rust toolchain as the pinned engine. Cargo selects
`rust-toolchain.toml` from the working directory, so run from this directory to
select that toolchain when your default differs.

`Cargo.lock` was seeded from the existing engine lockfile and pruned offline.
Every retained package version, source, and checksum matches that lockfile; no
dependency was refreshed during extraction. Preserve the lockfile when testing.

Engine assembly maps each `provider-*` directory to its corresponding
`codex-rs/providers/provider-*` directory and maps `integration/codex/adapter` to
`codex-rs/providers/provider-adapter`. The standalone workspace manifest,
lockfile, toolchain, and documentation are not engine overlays. The mapped
files remain byte-identical to the current engine copies during qualification.
