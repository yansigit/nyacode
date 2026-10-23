# CI dependency baseline

The October 2026 release-pipeline update includes major dependency migrations,
not just lockfile refreshes. Rust remains pinned to **1.95.0**. Cargo resolves
versions compatible with that toolchain; pnpm and uv retain the repository's
seven-day release-age policy. Builds use the committed lockfiles and immutable
GitHub Action revisions.

The migrations include Reqwest 0.13, OpenTelemetry 0.33, Schemars 1, ts-rs 12,
Rand 0.10, SHA-2 0.11, Ed25519 Dalek 3, SQLx 0.9, V8 152.2, ZIP 9, pulldown-cmark
0.13, pnpm 12, ESLint 10, Jest 30, Zod 4, and datamodel-code-generator 0.83.
The unmaintained serde_yaml implementation is replaced by serde_norway under
the existing import name. The native musl dependency setup uses OpenSSL 4.0.3
and libcap 2.78, with verified archive hashes.

## Compatibility constraints

Some dependencies deliberately remain below their newest published version:

| Dependency                      | Retained version                 | Constraint                                                                                                                                  |
| ------------------------------- | -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| Rust                            | 1.95.0                           | Project toolchain requirement. Dependencies requiring 1.96 are excluded.                                                                    |
| TypeScript                      | 6.0.3                            | The current typescript-eslint and ts-jest peers do not support TypeScript 7.                                                                |
| Keyring                         | 3.6.3                            | The 4.x facade changes Linux persistent keyutils/Secret Service storage. Moving existing credentials requires a separate backend migration. |
| Tokio Tungstenite / Tungstenite | Pinned maintained fork revisions | The application uses fork-specific proxy and compression support absent from the upstream replacement.                                      |
| libsqlite3-sys                  | 0.37                             | SQLx 0.9 requires a version below 0.38; SQLite native libraries must have one Cargo links owner.                                            |
| blake3                          | 1.8.2                            | Starlark 0.14.2 pins this exact version.                                                                                                    |
| sse-stream                      | 0.2.6                            | The pinned MCP Rust SDK exposes this version in its public types.                                                                           |
| rules_foreign_cc                | 0.15.1                           | Version 0.16 removes CMake/pkg-config repository interfaces used by the native voice builds.                                                |
| bzip2 Bazel module              | 1.0.8.bcr.3                      | The newer registry revision declares rules_cc twice and fails module resolution.                                                            |
| OpenSSL Bazel module            | 4.0.1.bcr.0                      | Latest available registry module; Cargo musl packaging builds verified upstream 4.0.3 separately.                                           |
| Python SDK runtime distribution | 0.153.4                          | This compatibility package pin names an existing upstream wheel. Nyacode CI tests use the newly built Nyacode CLI via CODEX_EXEC_PATH.      |

Transitive packages may retain older major versions required by their parents.
Security audits and native builds remain required; a newer version number alone
does not establish API, credential-store, or platform compatibility. Refresh
this baseline and its constraints with subsequent dependency updates.
