# Provider state compatibility fixtures, version 1

These snapshots freeze the persistence format emitted by the native provider
implementation at baseline `29413a496e28042629125693b3d8c218e49be879`.
The token strings and all provider/account/model/thread labels are synthetic;
they are deliberately invalid and must never be used for authentication.
The checkpoint's signature and host-lineage digests are also placeholders.

`../../compatibility_fixtures.rs` verifies that the baseline can read each snapshot,
write the same bytes, and retain the hashed state and stable lock filenames.
The hashed identities are included here so future storage migrations can check
compatibility explicitly.
