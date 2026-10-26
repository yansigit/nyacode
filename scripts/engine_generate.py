"""Recreate generated engine outputs in an assembled workspace, and write them back to the repository.

ownership.json `generated` declares generators by id: the output patterns they own, their owner,
purpose and regression groups, whether they are deterministic (drift-checked), opt-in, and whether
their outputs are whole files in the generated tree (`tree`, default true). Tree outputs live under
integration/codex/generated/<repo path>, are copied into assembled workspaces by prepare, and are
excluded from patch export.

generate: runs generators in an assembled workspace. A generator's outputs are compared with the
workspace bytes before it ran (the baseline), so a prepared workspace reports exactly what the
generator would change relative to the committed patches and generated tree.

--write-back: copies every current output of the selected tree generators into the generated tree
(full sync: outputs no longer produced are removed), copies the cargo lock to
integration/codex/Cargo.lock, then rewrites assembly.json through the manifest.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
from collections.abc import Mapping
from pathlib import Path
from typing import Any

import engine
from engine import EngineError, located, read_json, relative
import engine_manifest
from engine_manifest import (
    GENERATED_ROOT,
    OWNERSHIP,
    glob_regex,
    read_generators,
    static_prefix,
)

DEFAULT_TIMEOUT_SECONDS = 1800
PRUNED_DIRS = {".git", ".engine", ".venv", "node_modules", "target", "__pycache__"}
CARGO_LOCK = "codex-rs/Cargo.lock"
CARGO_LOCK_GENERATOR = "cargo-lock"
SCHEMA_SCRIPT = "codex-rs/app-server-protocol/scripts/write_schema_fixtures.py"
MARKER = Path(".git/nyacode/inputs.json")
LOG_DIR = Path(".git/nyacode/generate")
LOG_TAIL_LINES = 25

Command = tuple[list[str], str]


def commands_for(
    generator_id: str,
    *,
    python: str,
    crates: list[str],
) -> list[Command]:
    """The commands behind each generator id; each runs in the workspace-relative directory.

    The schema writer runs without Prettier: upstream's TypeScript fixtures are unformatted,
    and formatting them changes hundreds of files.
    """
    if generator_id == "app-server-schema":
        command = [python, SCHEMA_SCRIPT]
        return [(command, "."), (command + ["--experimental"], ".")]
    if generator_id == "config-schema":
        return [
            (
                [
                    "cargo",
                    "run",
                    "--locked",
                    "-p",
                    "codex-config-schema",
                    "--bin",
                    "codex-write-config-schema",
                ],
                "codex-rs",
            )
        ]
    if generator_id == "hooks-schema":
        return [
            (
                [
                    "cargo",
                    "run",
                    "--locked",
                    "-p",
                    "codex-hooks",
                    "--bin",
                    "write_hooks_schema_fixtures",
                ],
                "codex-rs",
            )
        ]
    if generator_id == "pnpm-lock":
        return [(["pnpm", "install", "--lockfile-only"], ".")]
    if generator_id == "uv-lock":
        return [(["uv", "lock"], "sdk/python"), (["uv", "lock"], "scripts")]
    if generator_id == "bazel-lock":
        return [(["bazel", "mod", "deps", "--lockfile_mode=update"], ".")]
    if generator_id == CARGO_LOCK_GENERATOR:
        return [(["cargo", "metadata", "--format-version", "1"], "codex-rs")]
    if generator_id == "snapshots":
        if not crates:
            raise EngineError("the snapshots generator needs at least one --crate")
        return [
            (
                [
                    "cargo",
                    "insta",
                    "test",
                    "--accept",
                    "--unreferenced=delete",
                    "-p",
                    crate,
                ],
                "codex-rs",
            )
            for crate in crates
        ]
    raise EngineError(f"no command is defined for generator {generator_id}")


def walk_outputs(workspace: Path, patterns: list[str]) -> dict[str, Path]:
    """Workspace files matched by any pattern; only the static prefix of each pattern is walked."""
    found: dict[str, Path] = {}
    for pattern in patterns:
        if not any(character in pattern for character in "*?"):
            if (workspace / pattern).is_file() and not (
                workspace / pattern
            ).is_symlink():
                found[pattern] = workspace / pattern
    wildcards = [pattern for pattern in patterns if any(c in pattern for c in "*?")]
    regexes = [glob_regex(pattern) for pattern in wildcards]
    if not regexes:
        return found
    for base in sorted({static_prefix(pattern) for pattern in wildcards}):
        start = workspace / base if base else workspace
        if not start.is_dir():
            continue
        for directory, subdirectories, names in os.walk(start):
            subdirectories[:] = sorted(
                name for name in subdirectories if name not in PRUNED_DIRS
            )
            for name in sorted(names):
                full = Path(directory, name)
                if full.is_symlink() or not full.is_file():
                    continue
                relative_name = full.relative_to(workspace).as_posix()
                if any(regex.fullmatch(relative_name) for regex in regexes):
                    found[relative_name] = full
    return found


def digests(outputs: Mapping[str, Path]) -> dict[str, str]:
    return {
        name: hashlib.sha256(path.read_bytes()).hexdigest()
        for name, path in outputs.items()
    }


def changes_between(
    before: dict[str, str], after: dict[str, str]
) -> dict[str, list[str]]:
    return {
        "added": sorted(after.keys() - before.keys()),
        "removed": sorted(before.keys() - after.keys()),
        "changed": sorted(
            name for name in before.keys() & after.keys() if before[name] != after[name]
        ),
    }


def generator_environment(
    root: Path, workspace: Path, toolchain: str
) -> dict[str, str]:
    environment = os.environ.copy()
    environment.update(
        {
            "CODEX_REPO_ROOT": str(workspace),
            "NYACODE_ENGINE_ROOT": str(workspace),
            "RUSTUP_TOOLCHAIN": toolchain,
        }
    )
    environment.setdefault("CARGO_TARGET_DIR", str(root / ".engine/target"))
    return environment


def run_command(
    argv: list[str],
    cwd: Path,
    environment: dict[str, str],
    timeout: int,
    log: Path,
) -> None:
    log.parent.mkdir(parents=True, exist_ok=True)
    with log.open("ab") as stream:
        stream.write(f"$ {' '.join(argv)}  (in {cwd})\n".encode())
        stream.flush()
        try:
            result = subprocess.run(
                argv,
                cwd=cwd,
                env=environment,
                stdout=stream,
                stderr=subprocess.STDOUT,
                timeout=timeout,
                check=False,
            )
        except FileNotFoundError as error:
            raise EngineError(f"generator tool is not installed: {argv[0]}") from error
        except subprocess.TimeoutExpired as error:
            raise EngineError(
                f"`{' '.join(argv)}` timed out after {timeout}s; see {log}"
            ) from error
    if result.returncode:
        lines = log.read_text(errors="replace").splitlines()[-LOG_TAIL_LINES:]
        raise EngineError(
            f"`{' '.join(argv)}` exited {result.returncode}; see {log}\n"
            + "\n".join(lines)
        )


def select(
    generators: list[dict[str, Any]],
    only: list[str] | None,
    *,
    deterministic: bool,
) -> list[dict[str, Any]]:
    known = {generator["id"] for generator in generators}
    if only:
        unknown = sorted(set(only) - known)
        if unknown:
            raise EngineError(f"unknown generator ids: {unknown}")
        return [generator for generator in generators if generator["id"] in set(only)]
    if deterministic:
        return [
            generator
            for generator in generators
            if generator.get("deterministic") and not generator.get("opt_in")
        ]
    return [generator for generator in generators if not generator.get("opt_in")]


def check_workspace(root: Path, workspace: Path) -> dict[str, Any]:
    marker = workspace / MARKER
    if not marker.is_file():
        raise EngineError(f"not an assembled engine workspace: {workspace}")
    report = read_json(marker)
    if report.get("generated_by") != "nyacode-engine-v1":
        raise EngineError(f"not an assembled engine workspace: {workspace}")
    return report


def blob_oid(data: bytes) -> str:
    """The git object id of a blob with these bytes."""
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


def pristine_oids(workspace: Path, tree: str) -> dict[str, str]:
    """Upstream blob ids by path, from the pristine tree the workspace was assembled from."""
    entries: dict[str, str] = {}
    for record in engine.git(
        workspace, "ls-tree", "-r", "-z", "--full-tree", tree
    ).split(b"\0"):
        if not record:
            continue
        meta, _, name = record.partition(b"\t")
        mode, kind, oid = meta.decode().split(" ")
        if kind == "blob":
            entries[os.fsdecode(name)] = oid
    return entries


def write_back(
    root: Path,
    workspace: Path,
    selected: list[dict[str, Any]],
    outputs: dict[str, dict[str, Path]],
    baseline: dict[str, dict[str, str]],
    pristine: dict[str, str],
    lock: dict[str, Any],
) -> dict[str, Any]:
    """Make the generated tree hold exactly the tree outputs that differ from upstream.

    Outputs whose bytes equal upstream's need no carrying: upstream provides them and the
    generator reproduces them, so the drift check still covers them. Replacing an upstream file
    needs no overlay; creating one does.
    """
    manifest = read_json(root / lock["assembly_manifest"])
    overlays = {item["destination"] for item in manifest.get("overlays", [])}
    patched = {path for patch in manifest.get("patches", []) for path in patch["paths"]}
    copied: list[str] = []
    removed: list[str] = []
    unchanged: list[str] = []
    patch_owned: list[str] = []
    for generator in selected:
        identifier = generator["id"]
        if identifier == CARGO_LOCK_GENERATOR:
            source = workspace / CARGO_LOCK
            target = located(root, lock["assembled_lock"])
            if source.read_bytes() != target.read_bytes():
                shutil.copyfile(source, target)
                copied.append(lock["assembled_lock"])
            continue
        if not generator.get("tree", True):
            continue
        current = outputs[identifier]
        for name in sorted(set(baseline[identifier]) - set(current)):
            if name in pristine and name not in overlays:
                raise EngineError(
                    f"generator {identifier} no longer produces upstream output {name}; "
                    "declare a delete for it in ownership.json"
                )
        carried: dict[str, bytes] = {}
        for name in sorted(current):
            if name in overlays:
                continue
            if name in patched:
                patch_owned.append(name)
            data = current[name].read_bytes()
            if pristine.get(name) == blob_oid(data):
                unchanged.append(name)
                continue
            if name not in pristine:
                raise EngineError(
                    f"generator {identifier} creates {name}, which upstream lacks; "
                    "declare an overlay for it"
                )
            carried[name] = data
        regexes = [glob_regex(pattern) for pattern in generator["outputs"]]
        for name in engine_manifest.generated_files(root):
            if name in carried or name in overlays:
                continue
            if any(regex.fullmatch(name) for regex in regexes):
                located(root, f"{GENERATED_ROOT}/{name}").unlink()
                removed.append(f"{GENERATED_ROOT}/{name}")
        for name, data in sorted(carried.items()):
            target = located(root, f"{GENERATED_ROOT}/{name}")
            if not target.is_file() or target.read_bytes() != data:
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(data)
                copied.append(f"{GENERATED_ROOT}/{name}")
            target.chmod(0o755 if os.access(current[name], os.X_OK) else 0o644)
    # While a patch still carries an output, the manifest would reject the tree copy; export
    # removes the path from the patches, and the manifest is rewritten by export.
    manifest_result = None
    if not patch_owned:
        manifest_result = engine_manifest.run(root, write=True)
    return {
        "copied": copied,
        "removed": removed,
        "upstream_identical": unchanged,
        "patch_owned": sorted(set(patch_owned)),
        "manifest": manifest_result,
    }


def generate(
    *,
    root: Path = engine.ROOT,
    workspace: Path,
    only: list[str] | None = None,
    deterministic: bool = False,
    write: bool = False,
    crates: list[str] | None = None,
    python: str = sys.executable,
    commands: Mapping[str, list[Command]] | None = None,
    timeout_seconds: int | None = None,
) -> dict[str, Any]:
    """Run the selected generators in workspace and report what they changed."""
    root = root.resolve()
    workspace = workspace.absolute()
    workspace_report = check_workspace(root, workspace)
    lock = engine.load_lock(root)
    if workspace_report.get("revision") != lock["revision"]:
        raise EngineError("workspace was assembled from a different engine revision")
    ownership = read_json(root / OWNERSHIP) if (root / OWNERSHIP).is_file() else {}
    generators = read_generators(ownership)
    selected = select(generators, only, deterministic=deterministic)
    if not selected:
        raise EngineError("no generators selected")
    environment = generator_environment(root, workspace, lock["toolchain"])
    patterns = {generator["id"]: list(generator["outputs"]) for generator in selected}
    baseline_outputs = {
        identifier: walk_outputs(workspace, pattern)
        for identifier, pattern in patterns.items()
    }
    baseline = {
        identifier: digests(outputs) for identifier, outputs in baseline_outputs.items()
    }
    log = workspace / LOG_DIR
    results = []
    for generator in selected:
        identifier = generator["id"]
        plan = (
            commands[identifier]
            if commands is not None and identifier in commands
            else commands_for(identifier, python=python, crates=crates or [])
        )
        limit = timeout_seconds or generator.get(
            "timeout_seconds", DEFAULT_TIMEOUT_SECONDS
        )
        for index, (argv, directory) in enumerate(plan):
            run_command(
                argv,
                workspace / relative(directory) if directory != "." else workspace,
                environment,
                limit,
                log / f"{identifier}-{index + 1}.log",
            )
        if identifier == "snapshots":
            for stale in (workspace / "codex-rs").rglob("*.snap.new"):
                stale.unlink()
    for generator in selected:
        identifier = generator["id"]
        after = walk_outputs(workspace, patterns[identifier])
        changes = changes_between(baseline[identifier], digests(after))
        results.append(
            {
                "id": identifier,
                "status": "ok",
                "identical": not any(changes.values()),
                "changed": changes,
                "outputs": len(after),
            }
        )
    outputs_after = {
        identifier: walk_outputs(workspace, patterns[identifier])
        for identifier in patterns
    }
    result: dict[str, Any] = {
        "workspace": str(workspace),
        "generators": results,
    }
    if write:
        pristine = pristine_oids(workspace, workspace_report["pristine_tree"])
        result["write_back"] = write_back(
            root, workspace, selected, outputs_after, baseline, pristine, lock
        )
    return result


def summary_lines(result: dict[str, Any]) -> list[str]:
    lines = []
    for item in result["generators"]:
        changes = item["changed"]
        if item["identical"]:
            verdict = "identical"
        else:
            verdict = (
                f"differs (changed {len(changes['changed'])}, added {len(changes['added'])}, "
                f"removed {len(changes['removed'])})"
            )
        lines.append(f"{item['id']}: {verdict}")
        for name in (
            *changes["changed"],
            *changes["added"],
            *changes["removed"],
        ):
            lines.append(f"    {name}")
    if "write_back" in result:
        write = result["write_back"]
        lines.append(
            f"write-back: {len(write['copied'])} copied, {len(write['removed'])} removed, "
            f"{len(write['upstream_identical'])} outputs identical to upstream"
        )
        if write["patch_owned"]:
            lines.append(
                "patch-owned outputs copied; run engine.py export to remove them from the "
                f"patches, then manifest --write: {write['patch_owned']}"
            )
    return lines


def run(args: Any) -> int:
    result = generate(
        root=args.root,
        workspace=args.workspace,
        only=args.only,
        deterministic=args.deterministic,
        write=args.write_back,
        crates=args.crate,
    )
    if args.report:
        args.report.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    for line in summary_lines(result):
        print(line)
    return 0
