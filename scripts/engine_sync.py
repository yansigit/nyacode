#!/usr/bin/env python3
"""Carry the Nyacode patch stack onto upstream main one day at a time.

A snapshot is a commit on sync/upstream-main whose tree is the Nyacode product tree at
main_base, with integration/codex/**, engine.lock and inventory/downstream.json replaced by
the rebased versions, plus .engine-sync/sync.json. Each run starts from the latest snapshot
(or from main), rebases its patches onto the upstream revision, folds main-side patch edits
in, validates the result, and commits the next snapshot.

plan            choose the source and upstream revision; `noop` when nothing moved.
rebase          materialize the source tree, rebase onto upstream, fold. Exit 0 clean, 2 conflicts.
finish          apply a resolution (if any), export into the source tree, validate, commit.
verify          check a snapshot bundle against the sync branch before it is pushed.
push            verify, then push the snapshot to origin sync/upstream-main.
resolve-local   download a run's plan and rebase it locally for manual resolution.
"""

from __future__ import annotations

import argparse
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from collections.abc import Callable
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import engine
import engine_patches
import engine_status

ROOT = Path(__file__).resolve().parents[1]
ENGINE = Path(__file__).resolve().with_name("engine.py")
REPOSITORY = engine_patches.REPOSITORY
UPSTREAM_URL = f"https://github.com/{REPOSITORY}.git"
UPSTREAM_REF = "refs/heads/main"
BRANCH = "sync/upstream-main"
REMOTE_BRANCH = f"origin/{BRANCH}"
INCOMING = "refs/nyacode/incoming"
GENERATOR = "nyacode-engine-sync-v1"
SCHEMA_VERSION = 1
PLAN_NAME = "plan.json"
SNAPSHOT_NAME = "snapshot.json"
BUNDLE_NAME = "snapshot.bundle"
DIRECTORY_MARKER = "sync-directory.json"
META_PATH = ".engine-sync/sync.json"
REBASE_DIR = "rebase"
SOURCE_DIR = "source"
PREPARED_DIR = "prepared"
CHECKED_DIR = "checked"
CONFLICTS_DIR = "conflicts"
# Paths a snapshot may change relative to the product tree at its main_base.
ALLOWLIST = (
    "integration/codex",
    "engine.lock",
    "inventory/downstream.json",
    ".engine-sync/sync.json",
)
# Inputs taken from the snapshot when the product tree comes from main.
SNAPSHOT_OWNED = ("integration/codex", "engine.lock", "inventory/downstream.json")
FOLD_INPUTS = engine_patches.FOLD_INPUTS
RESOLUTION_KIND = re.compile(r"human|ai:[A-Za-z0-9._-]+/[A-Za-z0-9._:-]+")
EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
HEX40 = re.compile(r"[0-9a-f]{40}")
IDENTITY = engine_patches.IDENTITY

Runner = Callable[[list[str], Path, dict[str, str] | None], int]


def now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def short(revision: str | None) -> str:
    return revision[:12] if revision else "none"


def run_command(argv: list[str], cwd: Path, env: dict[str, str] | None = None) -> int:
    """Run a command from the trusted checkout; output is inherited. Returns the exit code."""
    print("+ " + " ".join(argv), flush=True)
    merged = None if env is None else {**os.environ, **env}
    return subprocess.run(argv, cwd=cwd, env=merged, check=False).returncode


def record(kind: str, message: str) -> None:
    engine_status.record(kind, message, os.environ.get("RUN_URL") or None)


def step_summary(lines: list[str]) -> None:
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with open(path, "a") as stream:
            stream.write("\n".join(lines) + "\n")


def plain_git(repo: Path, *args: str) -> str:
    """Git with the caller's configuration (credentials included), for remote operations."""
    result = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True, check=False
    )
    if result.returncode:
        raise engine.EngineError(
            f"git {' '.join(args)} failed: {result.stderr.strip()}"
        )
    return result.stdout


def optional_commit(repo: Path, ref: str) -> str | None:
    result = engine.git_process(
        repo, "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}"
    )
    value = result.stdout.decode().strip()
    return value if result.returncode == 0 and value else None


def commit_of(repo: Path, ref: str) -> str:
    commit = optional_commit(repo, ref)
    if commit is None:
        raise engine.EngineError(f"ref does not resolve to a commit: {ref}")
    return commit


def allowed(path: str) -> bool:
    return any(path == item or path.startswith(item + "/") for item in ALLOWLIST)


def export_paths(
    repo: Path, commit: str, paths: tuple[str, ...] | list[str], destination: Path
) -> Path:
    """Extract the listed paths of a commit into destination; missing paths are skipped."""
    present = [
        path
        for path in paths
        if engine.git_process(repo, "cat-file", "-e", f"{commit}:{path}").returncode
        == 0
    ]
    destination.mkdir(parents=True, exist_ok=True)
    if present:
        archive = engine.git(repo, "archive", "--format=tar", commit, "--", *present)
        options = {"filter": "data"} if hasattr(tarfile, "data_filter") else {}
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
            tar.extractall(destination, **options)
    return destination


def remove_entries(root: Path, names: tuple[str, ...]) -> None:
    for name in names:
        path = root / name
        if path.is_dir() and not path.is_symlink():
            shutil.rmtree(path)
        elif path.exists() or path.is_symlink():
            path.unlink()


def read_meta(repo: Path, commit: str) -> dict[str, Any]:
    try:
        meta = json.loads(engine.git(repo, "show", f"{commit}:{META_PATH}"))
    except (engine.EngineError, ValueError) as error:
        raise engine.EngineError(
            f"{commit} carries no sync metadata: {error}"
        ) from error
    if meta.get("schema_version") != SCHEMA_VERSION:
        raise engine.EngineError(f"unsupported sync metadata in {commit}")
    return meta


def pin_at(repo: Path, commit: str) -> tuple[Any, Any]:
    lock = json.loads(engine.git(repo, "show", f"{commit}:engine.lock"))
    return engine_patches.pin_of(lock)


def fold_blockers_at(repo: Path, base: str, main: str) -> list[str]:
    with tempfile.TemporaryDirectory(prefix="engine-sync-blockers-") as scratch:
        scratch_path = Path(scratch)
        base_root = export_paths(repo, base, FOLD_INPUTS, scratch_path / "base")
        main_root = export_paths(repo, main, FOLD_INPUTS, scratch_path / "main")
        return engine_patches.fold_blockers(base_root, main_root)


def resolve_upstream(upstream_ref: str, local_repo: Path | None) -> tuple[str, str]:
    """Return (revision, source) where source is local-ref or github."""
    if local_repo is not None:
        return engine_patches.resolve_local(local_repo, upstream_ref), "local-ref"
    if HEX40.fullmatch(upstream_ref):
        return upstream_ref, "github"
    result = subprocess.run(
        ["git", "ls-remote", UPSTREAM_URL, upstream_ref],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode:
        raise engine.EngineError(f"ls-remote failed: {result.stderr.strip()}")
    matches = [
        line.split()[0]
        for line in result.stdout.splitlines()
        if line.split()[1:2] == [upstream_ref]
    ]
    if len(matches) != 1:
        raise engine.EngineError(f"expected one {upstream_ref} on {UPSTREAM_URL}")
    return matches[0], "github"


def upstream_date(revision: str, source: str, local_repo: Path | None) -> str:
    if source == "local-ref" and local_repo is not None:
        return (
            engine.git(local_repo, "log", "-1", "--format=%cI", revision)
            .decode()
            .strip()
        )
    with tempfile.TemporaryDirectory(prefix="engine-sync-date-") as scratch:
        commit = engine.github_json(
            f"repos/{REPOSITORY}/commits/{revision}", Path(scratch) / "commit.log"
        )
    return str(commit["commit"]["committer"]["date"])


def plan(
    *,
    repo: Path = ROOT,
    upstream_ref: str = UPSTREAM_REF,
    local_repo: Path | None = None,
    snapshot_ref: str = REMOTE_BRANCH,
    main_ref: str = "HEAD",
) -> dict[str, Any]:
    main_sha = commit_of(repo, main_ref)
    revision, source = resolve_upstream(upstream_ref, local_repo)
    if not HEX40.fullmatch(revision):
        raise engine.EngineError("upstream did not resolve to an immutable commit")
    snapshot_sha = optional_commit(repo, snapshot_ref)
    result: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "action": "sync",
        "upstream": {
            "revision": revision,
            "date": upstream_date(revision, source, local_repo),
            "source": source,
            "ref": upstream_ref,
            "local_repo": str(local_repo) if local_repo else None,
        },
        "main_sha": main_sha,
        "main_base": main_sha,
        "snapshot_sha": snapshot_sha,
        "from": {"kind": "main", "ref": main_sha},
        "reason": "no snapshot on " + REMOTE_BRANCH if snapshot_sha is None else None,
    }
    if snapshot_sha is None:
        return result
    meta = read_meta(repo, snapshot_sha)
    if meta["upstream"]["revision"] == revision and meta["main_base"] == main_sha:
        result.update(
            {
                "action": "noop",
                "main_base": meta["main_base"],
                "from": {"kind": "snapshot", "ref": snapshot_sha},
                "reason": "snapshot already at upstream and main",
            }
        )
        return result
    source_main = meta["main_base"]
    if pin_at(repo, source_main) != pin_at(repo, main_sha):
        result["reason"] = (
            "main moved to a new pinned upstream revision; starting from main"
        )
        return result
    blockers = fold_blockers_at(repo, source_main, main_sha)
    if blockers:
        result["reason"] = "main changed beyond patch edits: " + "; ".join(blockers)
        return result
    result.update(
        {
            "main_base": source_main,
            "from": {"kind": "snapshot", "ref": snapshot_sha},
            "reason": "carry the snapshot forward and fold main's patch edits",
        }
    )
    return result


def load_plan(path: Path) -> dict[str, Any]:
    value = engine.read_json(path)
    if value.get("schema_version") != SCHEMA_VERSION or value.get("action") != "sync":
        raise engine.EngineError(f"not a sync plan: {path}")
    for key in ("main_sha", "main_base"):
        if not HEX40.fullmatch(str(value.get(key, ""))):
            raise engine.EngineError(f"plan needs an immutable {key}")
    if not HEX40.fullmatch(str(value["upstream"]["revision"])):
        raise engine.EngineError("plan needs an immutable upstream revision")
    if value["from"]["kind"] not in {"main", "snapshot"}:
        raise engine.EngineError("plan source kind must be main or snapshot")
    return value


def claim_directory(repo: Path, directory: Path) -> None:
    if directory == repo or repo.is_relative_to(directory):
        raise engine.EngineError(f"sync directory must be disposable: {directory}")
    if directory.exists() or directory.is_symlink():
        if not (directory / DIRECTORY_MARKER).is_file():
            raise engine.EngineError(
                f"refusing to reuse a directory that is not a sync directory: {directory}"
            )
        drop_worktree(repo, directory / SOURCE_DIR)
        shutil.rmtree(directory)
    directory.mkdir(parents=True)
    engine.write_json(directory / DIRECTORY_MARKER, {"generated_by": GENERATOR})


def drop_worktree(repo: Path, path: Path) -> None:
    if path.exists() or path.is_symlink():
        engine.git_process(repo, "worktree", "remove", "--force", str(path))
        if path.exists() or path.is_symlink():
            shutil.rmtree(path, ignore_errors=True)
    engine.git_process(repo, "worktree", "prune")


def make_source(repo: Path, plan_value: dict[str, Any], destination: Path) -> Path:
    """Materialize the product tree a rebase runs in, as a detached worktree at main."""
    engine.git(
        repo,
        "worktree",
        "add",
        "--quiet",
        "--detach",
        str(destination),
        plan_value["main_sha"],
    )
    if plan_value["from"]["kind"] == "snapshot":
        # The product tree is main's; the Nyacode integration state is the snapshot's.
        remove_entries(destination, SNAPSHOT_OWNED)
        export_paths(repo, plan_value["from"]["ref"], SNAPSHOT_OWNED, destination)
    return destination


def conflicts_bundle(directory: Path, report: dict[str, Any]) -> list[str]:
    output = directory / REBASE_DIR
    bundle = directory / CONFLICTS_DIR
    bundle.mkdir(exist_ok=True)
    shutil.copyfile(
        output / engine_patches.REPORT_NAME, bundle / engine_patches.REPORT_NAME
    )
    paths = sorted(engine_patches.conflict_paths(report))
    engine_patches.stage_workspace(output / "workspace")
    diff = engine.git(
        output / "workspace",
        "diff",
        "--cached",
        "--no-ext-diff",
        "--no-renames",
        engine_patches.PRISTINE,
        "--",
        *paths,
    )
    limit = 512 * 1024
    note = (
        b"\n# truncated; inspect the workspace directly\n" if len(diff) > limit else b""
    )
    (bundle / "conflicts.diff").write_bytes(diff[:limit] + note)
    return paths


def report_rebase(
    directory: Path, report: dict[str, Any], plan_value: dict[str, Any]
) -> int:
    output = directory / REBASE_DIR
    for line in engine_patches.summary_lines(report):
        print(line)
    print(f"output: {output}")
    upstream = short(plan_value["upstream"]["revision"])
    conflicts = engine_patches.conflict_paths(report)
    if not conflicts:
        record(
            "sync-rebased",
            f"Upstream {upstream} rebased cleanly onto {len(report['patches'])} patches",
        )
        return 0
    paths = conflicts_bundle(directory, report)
    run_id = os.environ.get("GITHUB_RUN_ID")
    instruction = (
        f"run `just sync-resolve {run_id}` locally"
        if run_id
        else "run `just sync-resolve <run-id>` locally"
    )
    listing = ", ".join(paths[:10]) + (" ..." if len(paths) > 10 else "")
    record(
        "sync-conflict",
        f"Upstream {upstream} conflicts in {len(paths)} files ({listing}); {instruction}",
    )
    step_summary(
        [
            f"## Upstream sync conflicts ({upstream})",
            "",
            *[f"- `{path}`" for path in paths],
            "",
            f"Resolve locally: {instruction}.",
        ]
    )
    return 2


def rebase_plan(
    plan_value: dict[str, Any],
    directory: Path,
    *,
    repo: Path = ROOT,
    cache_dir: Path | None = None,
) -> int:
    """Deterministic from the plan alone: source, rebase onto upstream, then fold if needed."""
    claim_directory(repo, directory)
    engine.write_json(directory / PLAN_NAME, plan_value)
    source = make_source(repo, plan_value, directory / SOURCE_DIR)
    upstream = plan_value["upstream"]
    if upstream["source"] == "local-ref":
        target = {
            "onto_local_ref": upstream["revision"],
            "local_repo": Path(upstream["local_repo"]),
        }
    else:
        target = {"onto": upstream["revision"]}
    output = directory / REBASE_DIR
    engine_patches.rebase(root=source, output=output, cache_dir=cache_dir, **target)
    if plan_value["from"]["kind"] == "snapshot":
        with tempfile.TemporaryDirectory(prefix="engine-sync-fold-") as scratch:
            base_root = export_paths(
                repo, plan_value["main_base"], FOLD_INPUTS, Path(scratch) / "base"
            )
            main_root = export_paths(
                repo, plan_value["main_sha"], FOLD_INPUTS, Path(scratch) / "main"
            )
            engine_patches.fold(
                output=output,
                base_root=base_root,
                main_root=main_root,
                cache_dir=cache_dir,
                base_commit=plan_value["main_base"],
                main_commit=plan_value["main_sha"],
            )
    report = engine.read_json(output / engine_patches.REPORT_NAME)
    return report_rebase(directory, report, plan_value)


def generate_supported(runner: Runner) -> bool:
    return runner([sys.executable, str(ENGINE), "generate", "--help"], ROOT, None) == 0


def create_commit(source: Path, parent: str | None, message: str) -> str:
    engine.git(source, "add", "--force", "--all", "--", ".")
    tree = engine.git(source, "write-tree").decode().strip()
    arguments = [*IDENTITY, "commit-tree", tree, "-m", message]
    if parent:
        arguments += ["-p", parent]
    return engine.git(source, *arguments).decode().strip()


def apply_resolution(
    workspace: Path, report: dict[str, Any], resolution: Path | None, kind: str | None
) -> str:
    """Apply a resolution patch if given, then check every change is a conflicted path."""
    conflicts = engine_patches.conflict_paths(report)
    if resolution is None:
        if kind not in (None, "human"):
            raise engine.EngineError(
                "a non-human resolution kind needs --resolution FILE"
            )
        label = "human" if conflicts else "none"
    else:
        if not conflicts:
            raise engine.EngineError("resolution given but the rebase has no conflicts")
        if kind is None or not RESOLUTION_KIND.fullmatch(kind):
            raise engine.EngineError(
                "--resolution needs --resolution-kind human|ai:<agent>/<model>"
            )
        outside = engine_patches.resolution_paths(resolution) - conflicts
        if outside:
            raise engine.EngineError(
                f"resolution touches non-conflicted paths: {sorted(outside)}"
            )
        engine.git(workspace, "apply", "--whitespace=nowarn", str(resolution))
        label = kind
    changed = engine_patches.workspace_changes(workspace, report["workspace_tree"])
    outside = changed - conflicts
    if outside:
        raise engine.EngineError(
            f"resolution touches non-conflicted paths: {sorted(outside)}"
        )
    for path in sorted(conflicts):
        target = engine_patches.located(workspace, path)
        if target.is_file() and not target.is_symlink():
            if engine_patches.MARKER_LINE.search(target.read_bytes()):
                raise engine.EngineError(
                    f"resolution leaves conflict markers in {path}"
                )
    return label


def finish(
    *,
    plan_value: dict[str, Any],
    directory: Path,
    repo: Path = ROOT,
    resolution: Path | None = None,
    resolution_kind: str | None = None,
    accept: list[str] | None = None,
    assign: list[str] | None = None,
    skip_cargo: bool = False,
    runner: Runner = run_command,
) -> Path:
    output = directory / REBASE_DIR
    workspace = output / "workspace"
    source = directory / SOURCE_DIR
    report = engine.read_json(output / engine_patches.REPORT_NAME)
    label = apply_resolution(workspace, report, resolution, resolution_kind)
    notes: list[str] = []

    pristine_changed = engine.changed_paths(workspace, engine_patches.PRISTINE)
    fold_assign = report.get("fold", {}).get("assign", {})
    assignments = [
        f"{path}={owner}"
        for path, owner in sorted(fold_assign.items())
        if path in pristine_changed
    ]
    assignments += assign or []
    engine_patches.export(
        root=source,
        workspace=workspace,
        update_lock=True,
        assign=assignments,
        accept=accept or [],
    )
    if generate_supported(runner):
        code = runner(
            [
                sys.executable,
                str(ENGINE),
                "--root",
                str(source),
                "generate",
                "--workspace",
                str(workspace),
                "--write-back",
            ],
            ROOT,
            None,
        )
        if code:
            raise engine.EngineError("engine generate --write-back failed")
    else:
        notes.append(
            "engine generate is unavailable; generated paths were not regenerated"
        )
        print(notes[-1])
    if "fold" in report:
        notes.append(
            f"folded {len(report['fold']['changed'])} main-side patch paths; "
            f"{len(report['fold']['deferred'])} deferred to resolution"
        )

    lock = engine.load_lock(source)
    prepared = directory / PREPARED_DIR
    checks = [
        (
            "manifest",
            [sys.executable, str(ENGINE), "--root", str(source), "manifest", "--check"],
            None,
        ),
        (
            "prepare",
            [
                sys.executable,
                str(ENGINE),
                "--root",
                str(source),
                "prepare",
                "--output",
                str(prepared),
            ],
            None,
        ),
        (
            "controls",
            [
                sys.executable,
                str(ENGINE),
                "--root",
                str(source),
                "check",
                "--mode",
                "controls",
                "--output",
                str(directory / CHECKED_DIR),
            ],
            None,
        ),
    ]
    if not skip_cargo:
        checks.append(
            (
                "cargo check",
                ["cargo", "check", "--workspace", "--locked"],
                prepared / "codex-rs",
            )
        )
    for name, argv, cwd in checks:
        env = {"RUSTUP_TOOLCHAIN": lock["toolchain"]}
        code = runner(argv, cwd or ROOT, env if name == "cargo check" else None)
        if code:
            raise engine.EngineError(f"validation failed: {name}")
    if skip_cargo:
        notes.append("cargo check was skipped")

    upstream = plan_value["upstream"]
    meta = {
        "schema_version": SCHEMA_VERSION,
        "upstream": {"revision": upstream["revision"], "date": upstream["date"]},
        "main_base": plan_value["main_sha"],
        "from": {"kind": plan_value["from"]["kind"], "ref": plan_value["from"]["ref"]},
        "report": {
            "totals": report["totals"],
            "patches": [
                {"name": patch["name"], "status": patch["status"]}
                for patch in report["patches"]
            ],
        },
        "resolution": label,
        "notes": notes,
        "created_at": now(),
    }
    engine.write_json(source / META_PATH, meta)
    parent = plan_value.get("snapshot_sha")
    message = (
        f"Sync upstream {short(upstream['revision'])} with main {short(plan_value['main_sha'])}\n\n"
        f"Resolution: {label}"
    )
    commit = create_commit(source, parent, message)
    engine.git(source, "branch", "-f", BRANCH, commit)
    bundle_args = ["bundle", "create", str(directory / BUNDLE_NAME), BRANCH]
    if parent:
        bundle_args.append(f"^{parent}")
    engine.git(source, *bundle_args)
    snapshot = directory / SNAPSHOT_NAME
    engine.write_json(
        snapshot,
        {
            "schema_version": SCHEMA_VERSION,
            "commit": commit,
            "parent": parent,
            "main_base": plan_value["main_sha"],
            "upstream_revision": upstream["revision"],
            "bundle": BUNDLE_NAME,
        },
    )
    record(
        "sync-ready",
        f"Snapshot {short(commit)} for upstream {short(upstream['revision'])} "
        f"(resolution: {label}); {'; '.join(notes) or 'validated'}",
    )
    return snapshot


def verify_bundle(
    repo: Path,
    commit: str,
    parent: str | None,
    *,
    product_base: str | None = None,
    main_ref: str | None = None,
) -> list[str]:
    """Problems that would stop a snapshot commit from reaching sync/upstream-main.

    Without product_base, every changed path must be allowlisted. With it, every path outside
    the allowlist must match product_base exactly, so a snapshot may follow main's product tree.
    """
    problems: list[str] = []
    actual = optional_commit(repo, f"{commit}^")
    if actual != parent:
        problems.append(f"parent {short(actual)} is not the expected {short(parent)}")
    if product_base is None:
        names = engine_patches.tree_names(repo, parent or EMPTY_TREE, commit)
        problems += [
            f"path outside the allowlist: {name}"
            for name in sorted(names)
            if not allowed(name)
        ]
    else:
        excludes = [f":(exclude){item}" for item in ALLOWLIST]
        output = engine.git(
            repo,
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            product_base,
            commit,
            "--",
            ".",
            *excludes,
        )
        names = sorted(os.fsdecode(name) for name in output.split(b"\0") if name)
        problems += [
            f"product content differs from main_base: {name}" for name in names
        ]
    try:
        meta = read_meta(repo, commit)
    except engine.EngineError as error:
        return problems + [str(error)]
    if product_base is not None and meta["main_base"] != product_base:
        problems.append("sync metadata names a different main_base")
    if product_base is not None and main_ref is not None:
        reachable = engine.git_process(
            repo, "merge-base", "--is-ancestor", product_base, main_ref
        )
        if reachable.returncode:
            problems.append(f"main_base {short(product_base)} is not on {main_ref}")
    return problems


def fetch_remote_head(repo: Path) -> str | None:
    """Refresh origin's sync head; returns it, or None when the branch does not exist yet."""
    if not plain_git(repo, "ls-remote", "--heads", "origin", BRANCH).strip():
        engine.git_process(repo, "update-ref", "-d", f"refs/remotes/{REMOTE_BRANCH}")
        return None
    plain_git(
        repo,
        "fetch",
        "--quiet",
        "--no-tags",
        "origin",
        f"+refs/heads/{BRANCH}:refs/remotes/{REMOTE_BRANCH}",
    )
    return commit_of(repo, REMOTE_BRANCH)


def verify_directory(
    repo: Path, directory: Path, *, main_ref: str = "HEAD"
) -> tuple[str, list[str]]:
    snapshot = engine.read_json(directory / SNAPSHOT_NAME)
    head = fetch_remote_head(repo)
    engine.git(
        repo,
        "fetch",
        "--quiet",
        "--no-tags",
        str(directory / snapshot["bundle"]),
        f"+{BRANCH}:{INCOMING}",
    )
    commit = commit_of(repo, INCOMING)
    problems: list[str] = []
    if commit != snapshot["commit"]:
        problems.append("bundle head does not match snapshot.json")
    if snapshot["parent"] != head:
        problems.append("snapshot parent is not the current sync head")
    problems += verify_bundle(
        repo,
        commit,
        head,
        product_base=snapshot["main_base"],
        main_ref=main_ref,
    )
    return commit, problems


def push(
    *,
    repo: Path = ROOT,
    directory: Path,
    confirm: bool = False,
    main_ref: str = "HEAD",
    input_fn: Callable[[str], str] = input,
) -> int:
    commit, problems = verify_directory(repo, directory, main_ref=main_ref)
    if problems:
        raise engine.EngineError("snapshot not pushed: " + "; ".join(problems))
    if confirm:
        answer = input_fn(f"Push snapshot {short(commit)} to origin {BRANCH}? [y/N] ")
        if answer.strip().lower() not in {"y", "yes"}:
            print("not pushed")
            return 0
    plain_git(repo, "push", "origin", f"{commit}:refs/heads/{BRANCH}")
    record("sync-published", f"Snapshot {short(commit)} pushed to {BRANCH}")
    print(f"pushed {commit} to origin {BRANCH}")
    return 0


def resolve_local(
    *, run_id: str, directory: Path, repo: Path = ROOT, cache_dir: Path | None = None
) -> int:
    with tempfile.TemporaryDirectory(prefix="engine-sync-download-") as scratch:
        subprocess.run(
            [
                "gh",
                "run",
                "download",
                run_id,
                "--name",
                "engine-sync-plan",
                "--dir",
                scratch,
            ],
            check=True,
        )
        plan_value = load_plan(Path(scratch) / PLAN_NAME)
        code = rebase_plan(plan_value, directory, repo=repo, cache_dir=cache_dir)
    print(f"edit the conflicted files under {directory / REBASE_DIR / 'workspace'}")
    print(f"then run: just sync-finish {run_id}")
    return code


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    sub = parser.add_subparsers(dest="command", required=True)

    planner = sub.add_parser("plan", help="choose the source and upstream revision")
    planner.add_argument("--upstream-ref", default=UPSTREAM_REF)
    planner.add_argument("--local-repo", type=Path)
    planner.add_argument("--snapshot-ref", default=REMOTE_BRANCH)
    planner.add_argument("--main-ref", default="HEAD")
    planner.add_argument("--repo", type=Path, default=ROOT)
    planner.add_argument("--output", type=Path, required=True)

    rebaser = sub.add_parser(
        "rebase", help="rebase the plan's source onto upstream and fold"
    )
    rebaser.add_argument("--plan", type=Path, required=True)
    rebaser.add_argument("--output", type=Path, required=True)
    rebaser.add_argument("--repo", type=Path, default=ROOT)
    rebaser.add_argument("--cache-dir", type=Path)

    finisher = sub.add_parser(
        "finish", help="validate the rebase and commit the snapshot"
    )
    finisher.add_argument("--plan", type=Path, required=True)
    finisher.add_argument("--dir", type=Path, required=True)
    finisher.add_argument("--repo", type=Path, default=ROOT)
    finisher.add_argument("--resolution", type=Path)
    finisher.add_argument("--resolution-kind")
    finisher.add_argument("--accept", action="append", default=[], metavar="PATH")
    finisher.add_argument("--assign", action="append", default=[], metavar="PATH=PATCH")
    finisher.add_argument("--skip-cargo", action="store_true")
    finisher.add_argument("--cache-dir", type=Path)

    verifier = sub.add_parser("verify", help="check a snapshot bundle before pushing")
    verifier.add_argument("--dir", type=Path, required=True)
    verifier.add_argument("--repo", type=Path, default=ROOT)
    verifier.add_argument("--main-ref", default="HEAD")

    pusher = sub.add_parser("push", help="verify and push the snapshot")
    pusher.add_argument("--dir", type=Path, required=True)
    pusher.add_argument("--repo", type=Path, default=ROOT)
    pusher.add_argument("--main-ref", default="HEAD")
    pusher.add_argument("--confirm", action="store_true", help="ask y/N before pushing")

    local = sub.add_parser(
        "resolve-local", help="download a run's plan and rebase it locally"
    )
    local.add_argument("--run-id", required=True)
    local.add_argument("--output", type=Path, required=True)
    local.add_argument("--repo", type=Path, default=ROOT)
    local.add_argument("--cache-dir", type=Path)

    args = parser.parse_args(argv)
    try:
        if args.command == "plan":
            value = plan(
                repo=args.repo,
                upstream_ref=args.upstream_ref,
                local_repo=args.local_repo,
                snapshot_ref=args.snapshot_ref,
                main_ref=args.main_ref,
            )
            engine.write_json(args.output, value)
            if value["action"] == "noop":
                record(
                    "sync-noop",
                    f"Upstream {short(value['upstream']['revision'])} and main {short(value['main_sha'])} are already synced",
                )
            else:
                record(
                    "sync-planned",
                    f"Upstream {short(value['upstream']['revision'])} from {value['from']['kind']}: {value['reason']}",
                )
            print(json.dumps(value, indent=2, sort_keys=True))
            return 0
        if args.command == "rebase":
            return rebase_plan(
                load_plan(args.plan),
                args.output,
                repo=args.repo,
                cache_dir=args.cache_dir,
            )
        if args.command == "finish":
            snapshot = finish(
                plan_value=load_plan(args.plan),
                directory=args.dir,
                repo=args.repo,
                resolution=args.resolution,
                resolution_kind=args.resolution_kind,
                accept=args.accept,
                assign=args.assign,
                skip_cargo=args.skip_cargo,
            )
            print(snapshot)
            return 0
        if args.command == "verify":
            _, problems = verify_directory(args.repo, args.dir, main_ref=args.main_ref)
            for problem in problems:
                print(f"verify: {problem}", file=sys.stderr)
            return 1 if problems else 0
        if args.command == "push":
            return push(
                repo=args.repo,
                directory=args.dir,
                confirm=args.confirm,
                main_ref=args.main_ref,
            )
        return resolve_local(
            run_id=args.run_id,
            directory=args.output,
            repo=args.repo,
            cache_dir=args.cache_dir,
        )
    except (
        engine.EngineError,
        OSError,
        ValueError,
        KeyError,
        tarfile.TarError,
        subprocess.CalledProcessError,
    ) as error:
        record("sync-failed", f"{args.command} stopped: {error}")
        print(f"engine sync: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
