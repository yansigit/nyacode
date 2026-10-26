# Upstream sync runbook

The daily sync carries Nyacode's patch stack forward onto upstream `openai/codex` main
about one day at a time. Each successful run publishes a **snapshot** on the long-lived
branch `sync/upstream-main`. A later stable-tag promotion therefore starts from a nearby
snapshot instead of from a pin that may be months old. The sync never changes `engine.lock`
on `main`, never pushes to `main`, and never promotes a release; those remain the job of
the upgrade pipeline in `docs/engine.md`.

## What a run does

Workflow: `.github/workflows/engine-sync.yml`, scheduled at 05:43 UTC.

| Job | Permissions | Does |
| --- | --- | --- |
| `plan` | read | Resolves upstream main, finds the latest snapshot, and chooses the source. Writes `engine-sync-plan`. Ends early as `noop` when nothing moved. |
| `rebase` | read | Materializes the source tree, rebases the patch stack onto upstream, and folds main's patch edits. Exit 2 means conflicts. |
| `resolve` | read | Only when conflicts exist and `NYACODE_RESOLVER_AGENT` is set. Calls the reusable `engine-resolve.yml`, which produces `engine-resolution`. |
| `finish` | read, no model secrets | Rebuilds the rebase, applies a resolution if one exists, exports patches, validates, and builds the snapshot bundle. |
| `commit` | write | Verifies the bundle against the current sync head and pushes it. Runs only for scheduled or default-ref dispatches. |
| `status` | issues | Upserts the single "Upstream sync status" issue. |

Every job records its outcome. The status issue lists the last 30 events.

### Source selection

A snapshot is used as the source only when all of these hold:

- Its `main_base` pins the same upstream revision and archive as current main.
- The only integration changes on main since `main_base` are patch file contents or patch
  ownership (`integration/codex/patches/*.patch`, `assembly.json` path lists).
- The patch set is unchanged.

Otherwise the run starts from main (`from.kind = main`). Typical reasons: a stable promotion
moved the pin, main changed an overlay or `ownership.json`, or no snapshot exists yet. The
plan records its reason in the status issue.

### Snapshot contents

A snapshot is a commit on `sync/upstream-main` whose tree is the Nyacode product tree at
`main_base`, with `integration/codex/**`, `engine.lock`, and `inventory/downstream.json`
replaced by the rebased versions. It adds `.engine-sync/sync.json`:

- `upstream`: the revision and commit date the snapshot was rebased onto.
- `main_base`: the main commit whose product tree it carries.
- `from`: `main` or `snapshot`, with the ref it started from.
- `report`: rebase totals and per-patch status.
- `resolution`: `none`, `human`, or `ai:<agent>/<model>`.
- `notes`: anything skipped, such as an unavailable `engine generate`.

`engine.lock` in a snapshot pins the upstream main revision with `release_tag: null`.
Stable-tag candidates start from a snapshot but are built by `engine-upgrades.yml`, not by
this workflow; see [Stable candidates](engine.md#stable-candidates).

## Stable candidates

`scripts/engine_sync.py plan --tag rust-vX.Y.Z --revision <commit>` plans a release. The
plan's `candidate` field makes `rebase` and `finish` behave differently: `finish` writes
`candidate.json` (schema 2) and `candidate.bundle` instead of a snapshot. That bundle holds
one commit on the current default branch, limited to the candidate allowlist, and it is the
only artifact the publisher accepts. The sync's daily snapshots never move `main`.

Classification runs in the qualification workflow:

```sh
python3 scripts/engine_sync.py classify --base <main-sha> --commit <candidate-sha> \
  --tag rust-vX.Y.Z --dir .engine/classify
```

It rebases main's committed patches onto the candidate's pin with no snapshot and no
resolver, regenerates, and compares the allowlisted bytes with the candidate's. The result
is `mechanical` only when nothing conflicts, the bytes match exactly, and the RPC and storage
fixtures are unchanged. Storage drift fails the job.

An unresolved carry (conflicts, no resolver configured) publishes a draft whose commit only
repins `engine.lock`, with the conflicting files listed in the pull request. Such a draft
cannot pass qualification. A maintainer repairs the candidate branch as a single commit on
the base. Then re-dispatch the qualification:

```sh
gh workflow run engine-qualification.yml --ref engine/rust-vX.Y.Z \
  -f base_sha=<base> -f pull_request=<number> -f release_tag=rust-vX.Y.Z
```

Assisted candidates (see [engine.md](engine.md#stable-candidates)) need an approval of their
exact head by a maintainer with write access before promotion.

## Configuration

| Name | Kind | Purpose |
| --- | --- | --- |
| `NYACODE_ENGINE_SYNC_ENABLED` | variable | Must be `true`. Otherwise the plan job is skipped and nothing runs. |
| `NYACODE_RESOLVER_AGENT` | variable | Enables the `resolve` job when set. Empty means conflicts go to a human. |
| `NYACODE_RESOLVER_MODEL` | variable | Model passed to the resolver. |
| `CURSOR_API_KEY`, `OPENCODE_API_KEY` | secrets | Passed explicitly to the resolver only. Other jobs receive no model secrets. |

The workflow shares the `engine-upgrades-status` concurrency group, so its status upsert
never races the upgrade pipeline's.

## Reading the status issue

The issue is titled "Upstream sync status" and carries the `engine:status` label. Relevant
event kinds:

- `sync-noop`: upstream and main are already reflected in the latest snapshot.
- `sync-planned`: the chosen source and the reason, including any reset.
- `sync-rebased` / `sync-conflict`: the rebase result. A conflict event lists the
  conflicted files and the local command to run.
- `sync-ready`: a snapshot was built and validated, with its resolution label.
- `sync-published`: the snapshot reached `sync/upstream-main`.
- `failure` events come from the status job for any job that failed or was cancelled.

## Resolving conflicts locally

When the resolver is not configured, or it fails, a human resolves the run:

```sh
just sync-resolve <run-id>     # downloads engine-sync-plan, rebases into .engine/sync/<run-id>
$EDITOR .engine/sync/<run-id>/rebase/workspace/...   # edit each conflicted file
just sync-finish <run-id>      # validates, commits the snapshot, asks y/N before pushing
```

`sync-finish` records the resolution as `human`. Every changed path must be one the rebase
reported as conflicted, and no conflict markers may remain. The local run needs `gh`
authentication and a checkout that contains the run's `main_sha` and any snapshot commit.

Rebase output under `.engine/sync/<run-id>/rebase/` includes `rebase-report.json`, a
`stack/` repository, and the `workspace/`. The `conflicts/` directory holds the report and a
`conflicts.diff` of conflicted files, capped at 512 KiB.

## Checks before a snapshot is published

`finish` runs these in order and stops at the first failure:

1. `engine.py manifest --check` on the rebased source.
2. `engine.py prepare` into `prepared/`, the strict assembly.
3. `engine.py check --mode controls`, which runs the engine control tests.
4. `cargo check --workspace --locked` in the prepared workspace, with the toolchain from
   `engine.lock`. Skipped locally with `--skip-cargo`.

`engine generate --write-back` runs when the subcommand exists. Until it lands, the snapshot
records "generate unavailable" in `notes`.

## Limits

- `Cargo.lock` is not regenerated. When upstream changes dependencies, `cargo check --locked`
  fails and no snapshot is published until a maintainer updates the lock through the
  normal engine path.
- Provider checks (`engine.py check --mode providers`) are not in the daily path. Promotion
  still runs the full engine qualification.
- Fold carries only patch content edits. Any other integration change on main forces a
  fresh snapshot from main, which is always correct but drops the rolling base.
- Resolution patches may touch only conflicted paths. A main-side edit to an already
  conflicted file is listed as `fold.deferred` in the report and must be resolved by hand
  against both sides.
- Dispatching with an explicit `upstream_ref` runs the whole pipeline but never publishes.

## Local dry run

Plan against a fetched upstream remote and rebase without pushing:

```sh
git fetch upstream main
python3 scripts/engine_sync.py plan --upstream-ref upstream/main --local-repo . --output .engine/sync/plan.json
python3 scripts/engine_sync.py rebase --plan .engine/sync/plan.json --output .engine/sync/local
```

Exit 0 means a clean rebase and 2 means conflicts; the totals print to stdout.
`finish` needs a GitHub-sourced plan (its export writes an archive digest into `engine.lock`),
so a local-ref plan stops at the rebase.
