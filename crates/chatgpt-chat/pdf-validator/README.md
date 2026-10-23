# Bounded PDF validator

This small Rust program compiles to an import-free WebAssembly module. The native
Chat attachment stager interprets that fixed module using Wasmi; PDF bytes are
data within a fresh instance. No browser, JavaScript engine, WASI, external worker,
filesystem access, or host callbacks are involved.

The host bounds the complete parse to 128 MiB of linear memory, a 128 KiB Wasmi
value stack, 256 calls, 200 million fuel units, and five seconds. It checks local
cancellation at every fuel yield. Bulk memory operations are charged by size and
remain subject to the memory ceiling. The existing decoder semaphore permits
only one parse per service. Dropping the instance releases all parser memory.
These limits cover allocations during decompression and cross-reference loading,
before post-parse object and page-tree checks can run. A resource limit rejects
the attachment with `AttachmentError::Limit`.

The guest uses strict lopdf parsing with an 8 MiB per-stream decompression ceiling,
then verifies a nonempty catalog/page tree, parent links, counts, and cycles.
Additional object, depth, and page limits remain in place. Errors and panics never
return document bytes or parser diagnostics to application logging.

`validator.wasm` is a build artifact, not third-party source. Its source, locked
dependencies, build script, and SHA-256 manifest are checked in so ordinary Cargo
and Bazel builds need no Wasm compiler. The manifest is checked by native unit
tests. Reproduce the artifact with:

```sh
rustup target add --toolchain 1.95.0 wasm32-unknown-unknown
python3 codex-rs/chatgpt-chat/pdf-validator/build.py --check
```

Omit `--check` after intentional source changes, then review the source/lockfile,
manifest, and bytecode change together. The script uses the pinned Rust toolchain,
remaps source paths, and disables incremental compilation. The `unsupported`
getrandom backend prevents browser imports; parsing needs no entropy. The host
also rejects imports and module start functions independently.

The parser is MIT-licensed lopdf 0.45.0. Dependency copyrights and licenses are
retained in [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES). The Rust 1.95 standard
library notices are retained in [RUST_LIBRARY_NOTICES](RUST_LIBRARY_NOTICES).
Wasmi 2.0.0 is a native Cargo/Bazel dependency under MIT OR Apache-2.0.
