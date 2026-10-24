These AWS sources carry temporary dependency compatibility changes for [RUSTSEC-2026-0387](https://rustsec.org/advisories/RUSTSEC-2026-0387.html). They preserve the existing AWS package versions and use the maintained fixed RustCrypto implementation. The patches adapt public APIs; they do not replace cryptographic algorithms.

`sources.json` records the canonical registry archives, published source revisions, patch hashes, and standalone test locks. `aws-config/test-data` restores the upstream fixtures omitted from its registry archive, verified against the recorded Git tree. Original licenses and package sources are retained. The patch files show every production adaptation; the separate Cargo locks are test inputs.

The assembled engine uses these packages through local Cargo overrides. Full container qualification also runs both upstream suites with locked dependencies and all features, including credentials login and SigV4a. Their test subprocesses clear `NO_COLOR` because an upstream assertion requires ANSI-formatted logs.

The engine separately pins `age` and `age-core` to upstream revision `b5b68c4f4f7ea5c368bc060f6fe0237549c3a13f`, which adopts the maintained crypto stack. Synthetic old/new scrypt interoperability checks passed in both directions; application storage tests remain part of qualification. No credential namespaces, file formats, or storage migrations are changed.

`clatter` is pinned to upstream revision `9a8d15c4f80d5911ca0403aa22e0de99ea59df08` because its maintained ML-KEM dependency can resolve alongside the fixed age stack. Existing Noise suites and explicit features are preserved. The unchanged exec-server channel passed old/new process interoperability and invalid-input checks; full application qualification remains required.

Remove each override after an upstream release includes the fixed dependencies and passes the same qualification. Preparation never updates these sources or locks automatically.
