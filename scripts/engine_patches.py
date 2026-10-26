"""Carry the Nyacode patch stack onto a new upstream revision and export edits back to patches.

rebase: pristine OLD (the pinned revision) and NEW trees are imported into a scratch repository.
Each patch is committed alone on OLD, then 3-way merged onto NEW with merge-tree (rename
detection disabled; patched paths that upstream renamed fail loudly). The combined result is
left uncommitted in a clean workspace so `git diff` shows the patch stack against `pristine`.

export: the workspace's changes against `pristine` are assigned to patches by declared ownership
and written back as `git diff --binary --full-index` patches, with assembly.json paths updated.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import tempfile
import tomllib
from collections.abc import Callable
from pathlib import Path, PurePosixPath
from typing import Any

import engine
from engine import EngineError, digest, git, located, read_json, relative, write_json

ROOT = engine.ROOT
REPOSITORY = "openai/codex"
REPORT_NAME = "rebase-report.json"
REBASE_GENERATOR = "nyacode-engine-rebase-v1"
PRISTINE = "pristine"
IDENTITY = ("-c", "user.name=nyacode-engine", "-c", "user.email=engine@nyacode.invalid")
CONFLICTED = {"conflict", "deleted_upstream"}
OFFSET_OR_FUZZ = re.compile(rb"\(offset [+-]?\d+ lines?\)|\bfuzz \d+\b", re.IGNORECASE)
MARKER_LINE = re.compile(rb"(?m)^(<<<<<<< |>>>>>>> )")
BLOCK_START = re.compile(rb"(?m)^<<<<<<< ")


def patch_name(entry: dict[str, Any]) -> str:
    return PurePosixPath(entry["path"]).stem


def patch_entries(manifest: dict[str, Any]) -> list[dict[str, Any]]:
    entries = manifest.get("patches")
    if not isinstance(entries, list) or not entries:
        raise EngineError("assembly manifest declares no patches")
    names = [patch_name(entry) for entry in entries]
    if len(names) != len(set(names)):
        raise EngineError("duplicate patch names in assembly manifest")
    owned: set[str] = set()
    for entry in entries:
        relative(entry["path"])
        paths = entry.get("paths") or []
        if not paths or len(set(paths)) != len(paths):
            raise EngineError(
                f"patch paths must be non-empty and unique: {entry['path']}"
            )
        for name in paths:
            relative(name)
        if owned & set(paths):
            raise EngineError(
                f"patches must be file-disjoint: {sorted(owned & set(paths))}"
            )
        owned.update(paths)
    return entries


def fetch_revision(
    revision: str, cache_dir: Path, *, repository: str = REPOSITORY
) -> tuple[Path, str]:
    """Download an upstream tarball into the cache and record its digest; no pin is checked."""
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise EngineError("upstream revision must be an immutable 40-hex commit")
    cache_dir.mkdir(parents=True, exist_ok=True)
    for candidate in sorted(cache_dir.glob(f"{revision}-*.tar.gz")):
        recorded = candidate.name[len(revision) + 1 : -len(".tar.gz")]
        if re.fullmatch(r"[0-9a-f]{64}", recorded) and (
            digest(candidate.read_bytes()) == recorded
        ):
            return candidate, recorded
    descriptor, name = tempfile.mkstemp(
        prefix=".fetch-", suffix=".partial", dir=cache_dir
    )
    os.close(descriptor)
    temporary = Path(name)
    try:
        engine.download(repository, revision, temporary)
        sha256 = digest(temporary.read_bytes())
        archive = cache_dir / f"{revision}-{sha256}.tar.gz"
        os.replace(temporary, archive)
    finally:
        temporary.unlink(missing_ok=True)
    return archive, sha256


def resolve_local(repo: Path, ref: str) -> str:
    return (
        git(repo, "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}")
        .decode()
        .strip()
    )


def local_archive(repo: Path, revision: str, destination: Path) -> Path:
    """Build a tarball of a local commit; the top-level directory is normalized on extraction."""
    destination.write_bytes(
        git(repo, "archive", "--format=tar.gz", "--prefix=upstream/", revision)
    )
    return destination


def clear_worktree(repo: Path) -> None:
    for child in repo.iterdir():
        if child.name == ".git":
            continue
        if child.is_dir() and not child.is_symlink():
            shutil.rmtree(child)
        else:
            child.unlink()


def import_pristine(
    stack: Path, archive: Path, *, parent: str | None, message: str
) -> tuple[str, str]:
    """Commit an upstream archive exactly as the engine would extract and index it."""
    clear_worktree(stack)
    git(stack, "read-tree", "--empty")
    modes = engine.extract_archive(archive, stack)
    git(stack, "add", "--force", "--all", "--", ".")
    engine.index_modes(
        stack, {name: mode for name, mode in modes.items() if mode == "100755"}
    )
    tree = git(stack, "write-tree").decode().strip()
    parents = ["-p", parent] if parent else []
    commit = (
        git(stack, *IDENTITY, "commit-tree", tree, *parents, "-m", message)
        .decode()
        .strip()
    )
    return commit, tree


def apply_patch(
    stack: Path, base: str, base_tree: str, patch: Path, paths: set[str], name: str
) -> str:
    """Commit one patch alone on the base, applied strictly (no offset or fuzz)."""
    git(stack, "read-tree", "--reset", "-u", base)
    checked = git(
        stack,
        "apply",
        "--check",
        "--index",
        "--verbose",
        "--whitespace=nowarn",
        str(patch),
    )
    if OFFSET_OR_FUZZ.search(checked):
        raise EngineError(f"patch would require offset/fuzz: {name}")
    git(stack, "apply", "--index", "--whitespace=nowarn", str(patch))
    actual = engine.changed_paths(stack, base_tree)
    if actual != paths:
        raise EngineError(
            f"unexpected changed paths in {name}: expected {sorted(paths)}, got {sorted(actual)}"
        )
    tree = git(stack, "write-tree").decode().strip()
    return (
        git(stack, *IDENTITY, "commit-tree", tree, "-p", base, "-m", f"patch {name}")
        .decode()
        .strip()
    )


def renamed_pairs(stack: Path, old: str, new: str) -> list[tuple[str, str]]:
    fields = git(
        stack, "diff", "--name-status", "-z", "--find-renames", old, new
    ).split(b"\0")
    pairs: list[tuple[str, str]] = []
    index = 0
    while index < len(fields) and fields[index]:
        status = fields[index].decode()
        if status[:1] in {"R", "C"}:
            pairs.append(
                (os.fsdecode(fields[index + 1]), os.fsdecode(fields[index + 2]))
            )
            index += 3
        else:
            index += 2
    return pairs


def merge_tree(stack: Path, base: str, ours: str, theirs: str) -> tuple[str, set[str]]:
    """3-way merge with renames disabled; returns the merged tree and conflicted paths."""
    result = engine.git_process(
        stack,
        "merge-tree",
        "--write-tree",
        "-z",
        "--name-only",
        "--merge-base",
        base,
        "-X",
        "no-renames",
        ours,
        theirs,
    )
    if result.returncode not in (0, 1):
        raise EngineError(
            f"merge-tree failed: {result.stderr.decode(errors='replace')}"
        )
    fields = result.stdout.split(b"\0")
    tree = fields[0].decode()
    conflicted: set[str] = set()
    for field in fields[1:]:
        if not field:
            break
        conflicted.add(os.fsdecode(field))
    return tree, conflicted


def tree_entries(stack: Path, tree: str) -> dict[str, tuple[str, str]]:
    entries: dict[str, tuple[str, str]] = {}
    for record in git(stack, "ls-tree", "-r", "-z", "--full-tree", tree).split(b"\0"):
        if not record:
            continue
        meta, _, name = record.partition(b"\t")
        mode, kind, oid = meta.decode().split(" ")
        if kind != "blob":
            raise EngineError(f"unsupported tree entry {kind}: {os.fsdecode(name)}")
        entries[os.fsdecode(name)] = (mode, oid)
    return entries


def conflict_blocks(stack: Path, entry: tuple[str, str] | None) -> int:
    if entry is None:
        return 1
    content = git(stack, "cat-file", "blob", entry[1])
    if b"\0" in content:
        return 1
    # A conflicted file without markers (modify/delete, binary) is one whole-file block.
    return len(BLOCK_START.findall(content)) or 1


def classify_patch(
    stack: Path,
    paths: set[str],
    old_entries: dict[str, tuple[str, str]],
    new_entries: dict[str, tuple[str, str]],
    merged: dict[str, tuple[str, str]],
    conflicted: set[str],
) -> list[dict[str, Any]]:
    files = []
    for path in sorted(paths):
        in_old = path in old_entries
        in_new = path in new_entries
        found = merged.get(path)
        if path in conflicted:
            status = "deleted_upstream" if in_old and not in_new else "conflict"
            blocks = conflict_blocks(stack, found)
        else:
            status = "added" if found is not None and not in_old else "clean"
            blocks = 0
        record: dict[str, Any] = {
            "path": path,
            "status": status,
            "conflict_blocks": blocks,
        }
        if path in conflicted:
            record["merged_blob"] = found[1] if found is not None else None
        files.append(record)
    return files


def check_renames(stack: Path, old: str, new: str, paths: set[str]) -> None:
    for source, target in renamed_pairs(stack, old, new):
        if source in paths or target in paths:
            raise EngineError(
                f"upstream renamed a patched path ({source} -> {target}); "
                "rebase cannot route it safely, port that patch change by hand"
            )


def compose_workspace(
    stage: Path,
    stack: Path,
    new_commit: str,
    new_entries: dict[str, tuple[str, str]],
    merges: list[tuple[set[str], dict[str, tuple[str, str]]]],
) -> Path:
    git(stack, "branch", "-f", PRISTINE, new_commit)
    workspace = stage / "workspace"
    git(
        stage,
        "clone",
        "--quiet",
        "--local",
        "--no-checkout",
        "--branch",
        PRISTINE,
        str(stack),
        str(workspace),
    )
    git(workspace, "remote", "remove", "origin")
    git(workspace, "reset", "--quiet", "--hard")
    for paths, merged in merges:
        added: list[str] = []
        for path in sorted(paths):
            destination = located(workspace, path)
            found = merged.get(path)
            if destination.is_symlink() or destination.exists():
                destination.unlink()
            if found is None:
                continue
            mode, oid = found
            data = git(stack, "cat-file", "blob", oid)
            destination.parent.mkdir(parents=True, exist_ok=True)
            if mode == "120000":
                target = os.fsdecode(data)
                engine.safe_link(path, target)
                engine.create_link(destination, target)
            elif mode in {"100644", "100755"}:
                destination.write_bytes(data)
                destination.chmod(0o755 if mode == "100755" else 0o644)
            else:
                raise EngineError(f"unsupported mode {mode}: {path}")
            if path not in new_entries:
                added.append(path)
        for start in range(0, len(added), 200):
            git(workspace, "add", "--intent-to-add", "--", *added[start : start + 200])
    return workspace


def totals_for(patches: list[dict[str, Any]]) -> dict[str, int]:
    conflicted_patches = sum(1 for patch in patches if patch["status"] == "conflict")
    files = [
        record
        for patch in patches
        for record in patch["files"]
        if record["status"] in CONFLICTED
    ]
    return {
        "clean_patches": len(patches) - conflicted_patches,
        "conflicted_patches": conflicted_patches,
        "conflicted_files": len(files),
        "conflict_blocks": sum(record["conflict_blocks"] for record in files),
    }


def is_rebase_output(path: Path) -> bool:
    report = path / REPORT_NAME
    try:
        return (
            report.is_file()
            and read_json(report).get("generated_by") == REBASE_GENERATOR
        )
    except (EngineError, ValueError, OSError):
        return False


def rebase(
    *,
    root: Path = ROOT,
    onto: str | None = None,
    onto_local_ref: str | None = None,
    tag: str | None = None,
    output: Path | None = None,
    cache_dir: Path | None = None,
    local_repo: Path | None = None,
) -> tuple[Path, dict[str, Any]]:
    if (onto is None) == (onto_local_ref is None):
        raise EngineError("rebase needs exactly one of --onto or --onto-local-ref")
    root = root.resolve()
    lock = engine.load_lock(root)
    if tag is not None and not engine.STABLE_RELEASE.fullmatch(tag):
        raise EngineError("--tag must be a stable rust-vX.Y.Z release tag")
    cache = (cache_dir or engine.CACHE).resolve()
    if cache.is_relative_to(root):
        raise EngineError("archive cache must be outside the tracked product tree")
    manifest = read_json(root / lock["assembly_manifest"])
    entries = patch_entries(manifest)
    local = (local_repo or root).resolve()
    if onto_local_ref is not None:
        revision = resolve_local(local, onto_local_ref)
        source, ref = "local-ref", onto_local_ref
    else:
        if re.fullmatch(r"[0-9a-f]{40}", onto or ""):
            revision = onto or ""
        else:
            with tempfile.TemporaryDirectory(prefix="nyacode-rebase-") as scratch:
                commit = engine.github_json(
                    f"repos/{lock['repository']}/commits/{onto}",
                    Path(scratch) / "resolve.log",
                )
            revision = str(commit.get("sha", ""))
        source, ref = "github", None
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise EngineError("rebase target did not resolve to an immutable commit")
    output = (output or root / ".engine/rebase" / revision[:12]).absolute()
    if output == root or root.is_relative_to(output) or output.is_symlink():
        raise EngineError("rebase output must be a disposable directory")
    if output.exists() and not is_rebase_output(output):
        raise EngineError(f"refusing to replace non-rebase directory: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix=f".{output.name}.rebase-", dir=output.parent))
    try:
        if source == "local-ref":
            archive_new = local_archive(local, revision, stage / "onto.tar.gz")
            new_sha = digest(archive_new.read_bytes())
        else:
            archive_new, new_sha = fetch_revision(
                revision, cache, repository=lock["repository"]
            )
        archive_old = engine.archive_for(lock, cache)
        report = build_report(
            stage,
            root=root,
            lock=lock,
            entries=entries,
            archive_old=archive_old,
            archive_new=archive_new,
            revision=revision,
            new_sha=new_sha,
            source=source,
            ref=ref,
            tag=tag,
        )
        (stage / "onto.tar.gz").unlink(missing_ok=True)
        engine.write_json(stage / REPORT_NAME, report)
        publish(stage, output)
    except BaseException as error:
        if stage.exists():
            write_json(stage / "rebase-failure.json", {"error": str(error)})
        raise
    return output, report


def build_report(
    stage: Path,
    *,
    root: Path,
    lock: dict[str, Any],
    entries: list[dict[str, Any]],
    archive_old: Path,
    archive_new: Path,
    revision: str,
    new_sha: str,
    source: str,
    ref: str | None,
    tag: str | None,
) -> dict[str, Any]:
    stack = stage / "stack"
    stack.mkdir()
    git(stack, "init", "--quiet")
    old_commit, old_tree = import_pristine(
        stack, archive_old, parent=None, message=f"pristine {lock['revision']}"
    )
    new_commit, new_tree = import_pristine(
        stack, archive_new, parent=old_commit, message=f"pristine {revision}"
    )
    try:
        toolchain = tomllib.loads((stack / "codex-rs/rust-toolchain.toml").read_text())[
            "toolchain"
        ]["channel"]
    except (OSError, KeyError, tomllib.TOMLDecodeError) as error:
        raise EngineError(
            f"NEW tree lacks a readable Rust toolchain: {error}"
        ) from error
    old_entries = tree_entries(stack, old_tree)
    new_entries = tree_entries(stack, new_tree)
    all_paths = {path for entry in entries for path in entry["paths"]}
    check_renames(stack, old_commit, new_commit, all_paths)

    commits: list[tuple[dict[str, Any], set[str], str]] = []
    for entry in entries:
        paths = set(entry["paths"])
        commit = apply_patch(
            stack,
            old_commit,
            old_tree,
            root / entry["path"],
            paths,
            patch_name(entry),
        )
        commits.append((entry, paths, commit))

    patches: list[dict[str, Any]] = []
    merges: list[tuple[set[str], dict[str, tuple[str, str]]]] = []
    for entry, paths, commit in commits:
        name = patch_name(entry)
        merged_tree, conflicted = merge_tree(stack, old_commit, new_commit, commit)
        if not conflicted <= paths:
            raise EngineError(
                f"unexpected conflicts outside {name}: {sorted(conflicted - paths)}"
            )
        merged = tree_entries(stack, merged_tree)
        files = classify_patch(
            stack, paths, old_entries, new_entries, merged, conflicted
        )
        merges.append((paths, merged))
        patches.append(
            {
                "name": name,
                "path": entry["path"],
                "status": "conflict"
                if any(f["status"] in CONFLICTED for f in files)
                else "clean",
                "files": files,
            }
        )
    compose_workspace(stage, stack, new_commit, new_entries, merges)
    return {
        "schema_version": 1,
        "generated_by": REBASE_GENERATOR,
        "from": {
            "revision": lock["revision"],
            "archive_sha256": lock["archive_sha256"],
            "release_tag": lock.get("release_tag"),
            "toolchain": lock["toolchain"],
        },
        "onto": {
            "revision": revision,
            "archive_sha256": new_sha,
            "toolchain": toolchain,
            "tag": tag,
            "source": source,
            "ref": ref,
        },
        "patches": patches,
        "totals": totals_for(patches),
    }


def publish(stage: Path, output: Path) -> None:
    backup = None
    if output.exists() or output.is_symlink():
        backup = stage.with_name(stage.name + ".previous")
        output.rename(backup)
    try:
        stage.rename(output)
    except BaseException:
        if backup is not None:
            backup.rename(output)
        raise
    if backup is not None:
        shutil.rmtree(backup, ignore_errors=True)


def summary_lines(report: dict[str, Any]) -> list[str]:
    lines = []
    for patch in report["patches"]:
        conflicts = [
            record for record in patch["files"] if record["status"] in CONFLICTED
        ]
        if conflicts:
            blocks = sum(record["conflict_blocks"] for record in conflicts)
            lines.append(
                f"{patch['name']}: conflict ({len(conflicts)} files, {blocks} conflict blocks)"
            )
        else:
            lines.append(f"{patch['name']}: clean ({len(patch['files'])} files)")
    totals = report["totals"]
    lines.append(
        f"totals: {totals['clean_patches']} clean, {totals['conflicted_patches']} conflicted patches; "
        f"{totals['conflicted_files']} conflicted files; {totals['conflict_blocks']} conflict blocks"
    )
    return lines


def assign_new_path(path: str) -> str | None:
    """Ownership rule for changed paths that no patch declares yet.

    TODO: map new paths to patches (for example by directory). Until a rule exists every new
    path is an error, even with --allow-new-paths; use --assign PATH=PATCH instead.
    """
    del path
    return None


def unresolved_conflicts(
    workspace: Path, report_path: Path, accepted: set[str]
) -> list[str]:
    """Conflicts without markers stay unresolved while the file still holds the merged bytes."""
    if not report_path.is_file():
        return []
    report = read_json(report_path)
    unresolved = []
    for patch in report.get("patches", []):
        for record in patch.get("files", []):
            blob = record.get("merged_blob")
            path = record["path"]
            if record["status"] not in CONFLICTED or blob is None or path in accepted:
                continue
            target = located(workspace, path)
            if target.is_symlink() or not target.is_file():
                continue
            if (
                engine.git(workspace, "hash-object", "--no-filters", "--", path)
                .decode()
                .strip()
                == blob
            ):
                unresolved.append(path)
    return unresolved


def export(
    *,
    root: Path = ROOT,
    workspace: Path,
    report_path: Path | None = None,
    update_lock: bool = False,
    allow_new_paths: bool = False,
    assign: list[str] | None = None,
    accept: list[str] | None = None,
    hook: Callable[[str], str | None] = assign_new_path,
) -> dict[str, Any]:
    root = root.resolve()
    workspace = workspace.absolute()
    if not (workspace / ".git").exists():
        raise EngineError(f"not a rebase workspace: {workspace}")
    lock = engine.load_lock(root)
    manifest_path = located(root, lock["assembly_manifest"])
    manifest_text = manifest_path.read_text()
    manifest = json.loads(manifest_text)
    if manifest_text != json.dumps(manifest, indent=2, sort_keys=True) + "\n":
        raise EngineError(
            "assembly manifest is not in canonical form; refusing to rewrite it"
        )
    entries = patch_entries(manifest)
    by_name = {patch_name(entry): entry for entry in entries}
    declared = {path: patch_name(entry) for entry in entries for path in entry["paths"]}
    try:
        git(workspace, "rev-parse", "--verify", "--quiet", f"{PRISTINE}^{{commit}}")
    except EngineError as error:
        raise EngineError(f"workspace has no {PRISTINE} commit: {workspace}") from error

    git(workspace, "-c", "core.filemode=true", "add", "--force", "--all", "--", ".")
    changed = engine.changed_paths(workspace, PRISTINE)
    deleted = engine.changed_paths(workspace, PRISTINE, "--diff-filter=D")

    markers = []
    for path in sorted(changed - deleted):
        target = located(workspace, path)
        if target.is_symlink():
            continue
        content = target.read_bytes()
        if b"\0" not in content and MARKER_LINE.search(content):
            markers.append(path)
    if markers:
        raise EngineError(f"refusing export: conflict markers remain in {markers}")
    report_file = report_path or workspace.parent / REPORT_NAME
    accepted = set(accept or [])
    pending = unresolved_conflicts(workspace, report_file, accepted)
    if pending:
        raise EngineError(
            f"refusing export: conflicts still hold merged bytes: {pending}; "
            "resolve them or pass --accept PATH"
        )

    assigned: dict[str, str] = {}
    for item in assign or []:
        path, separator, name = item.partition("=")
        if not separator or name not in by_name:
            raise EngineError(f"--assign needs PATH=PATCH with a known patch: {item}")
        relative(path)
        if path not in changed:
            raise EngineError(f"--assign target is not changed: {path}")
        assigned[path] = name
    undeclared = sorted(
        path for path in changed if path not in declared and path not in assigned
    )
    if undeclared:
        if not allow_new_paths:
            raise EngineError(
                f"changed paths not owned by any patch: {undeclared}; "
                "use --assign PATH=PATCH or --allow-new-paths"
            )
        still_open = []
        for path in undeclared:
            owner = hook(path)
            if owner is None:
                still_open.append(path)
            elif owner not in by_name:
                raise EngineError(f"ownership rule named an unknown patch: {owner}")
            else:
                assigned[path] = owner
        if still_open:
            raise EngineError(
                f"no ownership rule for new paths: {still_open}; use --assign PATH=PATCH"
            )

    owner_of = {path: assigned.get(path, declared.get(path)) for path in changed}
    new_paths = {
        name: sorted(path for path, owner in owner_of.items() if owner == name)
        for name in by_name
    }
    for name, paths in new_paths.items():
        if not paths:
            raise EngineError(
                f"patch {name} would be empty; remove it from the manifest explicitly"
            )

    old_patch_paths = {path for entry in entries for path in entry["paths"]}
    lock_plan = plan_lock(root, lock, report_file) if update_lock else None
    rewritten = []
    reshaped: list[str] = []
    dropped: dict[str, list[str]] = {}
    outputs: list[tuple[Path, str | bytes]] = []
    for entry in entries:
        name = patch_name(entry)
        dropped[name] = sorted(set(entry["paths"]) - set(new_paths[name]))
        diff = git(
            workspace,
            "-c",
            "core.filemode=true",
            "--literal-pathspecs",
            "diff",
            "--cached",
            "--binary",
            "--full-index",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            PRISTINE,
            "--",
            *new_paths[name],
        )
        target = located(root, entry["path"])
        if target.is_symlink():
            raise EngineError(f"patch file must be a regular file: {entry['path']}")
        if not target.exists() or target.read_bytes() != diff:
            outputs.append((target, diff))
            rewritten.append(entry["path"])
        if entry["paths"] != new_paths[name]:
            reshaped.append(name)
        entry["paths"] = new_paths[name]

    # Engine-level deletes are the declared deletes no patch owns; patch deletes come from the diff.
    engine_deletes = [
        path for path in manifest.get("deletes", []) if path not in old_patch_paths
    ]
    manifest["deletes"] = sorted(set(engine_deletes) | deleted)
    manifest_out = json.dumps(manifest, indent=2, sort_keys=True) + "\n"
    if manifest_out != manifest_text:
        outputs.append((manifest_path, manifest_out))
    lock_result = None
    if lock_plan is not None:
        outputs.insert(0, (lock_plan["path"], lock_plan["text"]))
        lock_result = lock_plan["summary"]

    # Every output is computed and validated above; writes follow lock, manifest, then patches.
    for target, data in outputs:
        if isinstance(data, bytes):
            target.write_bytes(data)
        else:
            target.write_text(data)
    if (root / "integration/codex/ownership.json").is_file():
        import engine_manifest

        engine_manifest.run(root, write=True)
    return {
        "rewritten": rewritten,
        "dropped": {name: paths for name, paths in dropped.items() if paths},
        "assigned": assigned,
        "deletes": manifest["deletes"],
        "lock": lock_result,
    }


def plan_lock(root: Path, lock: dict[str, Any], report_file: Path) -> dict[str, Any]:
    """Validate the rebase report and build the new engine.lock text without writing it."""
    if not report_file.is_file():
        raise EngineError(f"--update-lock needs the rebase report: {report_file}")
    report = read_json(report_file)
    if report.get("generated_by") != REBASE_GENERATOR:
        raise EngineError("--update-lock needs a rebase report")
    origin = report["from"]
    if (
        origin["revision"] != lock["revision"]
        or origin["archive_sha256"] != lock["archive_sha256"]
    ):
        raise EngineError("rebase report was built from a different pinned revision")
    onto = report["onto"]
    if onto["source"] != "github":
        raise EngineError(
            "--update-lock needs a GitHub archive digest; this report used a local ref"
        )
    lock_path = located(root, "engine.lock")
    lock_text = lock_path.read_text()
    data = json.loads(lock_text)
    if lock_text != json.dumps(data, indent=2) + "\n":
        raise EngineError(
            "engine.lock is not in canonical form; refusing to rewrite it"
        )
    data.update(
        {
            "revision": onto["revision"],
            "archive_sha256": onto["archive_sha256"],
            "toolchain": onto["toolchain"],
            "release_tag": onto["tag"],
        }
    )
    candidate = json.dumps(data, indent=2) + "\n"
    with tempfile.TemporaryDirectory(prefix="nyacode-lock-") as scratch:
        Path(scratch, "engine.lock").write_text(candidate)
        engine.load_lock(Path(scratch))
    return {
        "path": lock_path,
        "text": candidate,
        "summary": {
            "revision": onto["revision"],
            "archive_sha256": onto["archive_sha256"],
            "toolchain": onto["toolchain"],
            "release_tag": onto["tag"],
        },
    }


def run(args: Any) -> int:
    root = args.subroot or args.root
    if args.command == "rebase":
        output, report = rebase(
            root=root,
            onto=args.onto,
            onto_local_ref=args.onto_local_ref,
            tag=args.tag,
            output=args.output,
            cache_dir=args.cache_dir,
            local_repo=args.local_repo,
        )
        for line in summary_lines(report):
            print(line)
        print(f"output: {output}")
        return 2 if report["totals"]["conflicted_patches"] else 0
    result = export(
        root=root,
        workspace=args.workspace,
        report_path=args.report,
        update_lock=args.update_lock,
        allow_new_paths=args.allow_new_paths,
        assign=args.assign,
        accept=args.accept,
    )
    for name in result["rewritten"]:
        print(f"rewrote {name}")
    for name, paths in result["dropped"].items():
        print(f"dropped from {name}: {paths}")
    if result["lock"]:
        print(f"updated engine.lock: {result['lock']}")
    return 0
