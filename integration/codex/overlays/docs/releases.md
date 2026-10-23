# Nyacode CI and releases

The `Nyacode packages` workflow validates release tooling on pushes to `main`.
Pull requests that change release inputs, version tags, and manual runs build
optimized, self-contained CLI packages on all six target platforms. The existing `Native providers` workflow continues to test ordinary
code changes; release runs also require that suite and the native Chat unit tests
to pass.

## Build matrix

| Platform            | Rust target                  | Standard GitHub runner |
| ------------------- | ---------------------------- | ---------------------- |
| Linux x64           | `x86_64-unknown-linux-musl`  | `ubuntu-24.04`         |
| Linux ARM64         | `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm`     |
| macOS Intel         | `x86_64-apple-darwin`        | `macos-15-intel`       |
| macOS Apple silicon | `aarch64-apple-darwin`       | `macos-15`             |
| Windows x64         | `x86_64-pc-windows-msvc`     | `windows-2025`         |
| Windows ARM64       | `aarch64-pc-windows-msvc`    | `windows-11-arm`       |

The matrix uses [standard hosted runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
There is one native build per target, using Rust 1.95.0, `Cargo.lock`, and the
workspace's release optimization settings. Debug information is disabled to
reduce disk and memory requirements. No `target-cpu=native` tuning is applied.
Packages are platform specific; there is no single binary that runs on every OS.

See the [dependency baseline](dependency-upgrades.md) for the major-version
migrations and remaining compatibility constraints.

Linux packages use musl and the existing verified OpenSSL/libcap build setup.
All builds verify the pinned V8 archive and bindings. The Linux sandbox helper
is stripped and hashed before its digest is embedded in the CLI. Packaging
preserves those exact helper bytes.

## Dry run

After the workflow is on the default branch:

```sh
gh workflow run nyacode-release.yml --repo yansigit/nyacode --ref main
gh run list --repo yansigit/nyacode --workflow nyacode-release.yml
gh run watch RUN_ID --repo yansigit/nyacode --exit-status
gh run download RUN_ID --repo yansigit/nyacode --name verified-nyacode-release --dir dist
```

A manual run builds and tests the same packages as a tagged release, including
development version `0.0.0`, but cannot publish. Downloadable workflow artifacts
expire after 3 days for verified bundles and 1 day for intermediate platform
packages. After verification and every publication consumer succeeds, CI deletes
the intermediate packages. A successful tagged publication also removes its
verified bundle because the durable copies are GitHub Release assets. Fork PRs
use retention expiry because their tokens cannot delete artifacts. A failed platform or check prevents the complete verified
artifact from being produced. Do a full dry run before the first version tag;
local Python tests do not establish successful foreign-platform builds.

## Publish a version

1. Set `[workspace.package].version` in `codex-rs/Cargo.toml` to the intended
   version. Refresh `codex-rs/Cargo.lock` with Cargo and review the diff; retain
   the dependency pins. Include both files in the reviewed change on `main`.
2. Run the dry run against that exact commit and inspect all platform results.
3. Create a tag naming that commit. For example, after choosing a version:

   ```sh
   gh api repos/yansigit/nyacode/git/refs --method POST \
     -f ref=refs/tags/rust-v1.0.0 -f sha=FULL_REVIEWED_COMMIT_SHA
   ```

The tag must be `rust-v` followed by the workspace version, and its commit must
be an ancestor of `main`. Development version `0.0.0` cannot be published. Stable
versions use `X.Y.Z`; installer-compatible prereleases use `X.Y.Z-alpha.N` or
`X.Y.Z-beta.N`. No version is incremented automatically.

The workflow builds all six targets, runs package/installer tests and native
provider/Chat checks, schema contracts, and terminal snapshots, then tests each
extracted package on its native runner. Both SDKs also run against the packaged
Linux CLI before the release set is accepted.
Smoke tests exercise both CLI names, public help, feature discovery, Code Mode
host startup/help, bundled ripgrep, Linux bubblewrap, and app-server schema
generation without account credentials. They do not run live provider inference.

Only the publication job receives `contents: write`. It accepts tag-push runs
in `yansigit/nyacode`, verifies the remote tag still names the built commit, and
creates a draft release. It publishes only after every asset upload has the
expected SHA-256 digest. Interrupted uploads remain drafts. Rerun failed jobs
to reuse successful build artifacts; existing assets with different bytes are
rejected and never overwritten. A full rebuild may produce different bytes,
so use a new version if replacing an already published build.

Prereleases never become `latest`; publishing an older stable version also
preserves the newer stable release. A manual run never publishes, even when run
against a tag. No account, login session, or Chat conversation is needed by CI.

## Assets and installation

Each release contains six `codex-package-TARGET.tar.gz` files,
`codex-package_SHA256SUMS`, and `nyacode-release.json`. The internal names preserve
the existing installer contract. Archives contain `bin/nyacode`, the identical
`bin/codex` compatibility alias, the Code Mode host, ripgrep, platform sandbox
helpers, package metadata, and bundled license notices. Windows names have
`.exe` suffixes. Build metadata records the source commit, target, Rust version,
profile, smoke-test result, and archive digest.

The checked-in [standalone installers](install.md#standalone-installers-and-npm)
install these packages. GitHub's automatic source archives retain the tagged
source. GitHub Releases are the distribution channel for this pipeline; npm,
Homebrew, WinGet, Apple notarization, and Windows Authenticode signing are not
configured. macOS/Windows packages may therefore show OS trust prompts.

Existing repository-wide CI findings remain visible in their own workflows.
This release workflow does not override branch protection or waive their checks.
Configure required checks on `main` separately if those checks must gate merges.
