#!/usr/bin/env python3
"""Assemble pinned Codex inputs without changing the accepted engine or its lockfile."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path, PurePosixPath
from typing import Any

import tomllib

ROOT = Path(__file__).resolve().parents[1]
CACHE = (
    Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "nyacode/engine"
)
MARKER = Path(".git/nyacode/inputs.json")
STABLE_RELEASE = re.compile(r"rust-v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)\Z")


class EngineError(RuntimeError):
    pass


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def relative(value: str) -> Path:
    if (
        not isinstance(value, str)
        or not value
        or "\\" in value
        or ":" in value
        or any(ord(char) < 32 for char in value)
    ):
        raise EngineError(f"unsafe path: {value!r}")
    path = PurePosixPath(value)
    if path.is_absolute() or any(
        part.lower() in {"", ".", "..", ".git"}
        or part.endswith((".", " "))
        or re.fullmatch(r"(?i)(con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\..*)?", part)
        for part in value.split("/")
    ):
        raise EngineError(f"unsafe path: {value!r}")
    return Path(*path.parts)


def located(root: Path, name: str) -> Path:
    path = root / relative(name)
    if not path.parent.resolve().is_relative_to(root.resolve()):
        raise EngineError(f"path escapes root: {name}")
    if any(
        parent.is_symlink()
        for parent in path.parents
        if parent != root and parent.is_relative_to(root)
    ):
        raise EngineError(f"path writes through a symlink: {name}")
    return path


def safe_link(name: str, target: str) -> None:
    if not target or "\\" in target or PurePosixPath(target).is_absolute():
        raise EngineError(f"unsafe link: {name} -> {target}")
    parts = list(PurePosixPath(name).parent.parts)
    for part in PurePosixPath(target).parts:
        if part == "..":
            if not parts:
                raise EngineError(f"escaping link: {name} -> {target}")
            parts.pop()
        elif part != ".":
            relative(part)
            parts.append(part)


def blob(path: Path) -> bytes:
    return os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()


def file_mode(path: Path) -> str:
    return (
        "120000"
        if path.is_symlink()
        else "100755"
        if path.stat().st_mode & 0o111
        else "100644"
    )


def read_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text())
    if not isinstance(value, dict):
        raise EngineError(f"expected a JSON object: {path}")
    return value


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def load_lock(root: Path) -> dict[str, Any]:
    lock = read_json(root / "engine.lock")
    if lock.get("schema_version") != 1:
        raise EngineError("unsupported engine.lock schema_version")
    if lock.get("repository") != "openai/codex":
        raise EngineError("engine input must be pristine openai/codex")
    for key, size in (("revision", 40), ("archive_sha256", 64)):
        if not re.fullmatch(rf"[0-9a-f]{{{size}}}", str(lock.get(key, ""))):
            raise EngineError(f"engine.lock needs an immutable {key}")
    if lock.get("integration_contract_version") != 1:
        raise EngineError("unsupported integration contract")
    for key in ("assembly_manifest", "assembled_lock", "product_version_file"):
        relative(lock[key])
    if not re.fullmatch(r"\d+\.\d+\.\d+", str(lock.get("toolchain", ""))):
        raise EngineError("engine.lock needs an exact Rust toolchain")
    tag = lock.get("release_tag")
    if tag is not None and not STABLE_RELEASE.fullmatch(tag):
        raise EngineError("release_tag must be a stable rust-vX.Y.Z tag")
    return lock


def download(repository: str, revision: str, destination: Path) -> None:
    with destination.open("wb") as output:
        result = subprocess.run(
            ["gh", "api", f"repos/{repository}/tarball/{revision}"],
            stdout=output,
            stderr=subprocess.PIPE,
            check=False,
        )
    if result.returncode:
        raise EngineError(
            f"gh archive download failed: {result.stderr.decode(errors='replace')}"
        )


def archive_for(lock: dict[str, Any], cache_dir: Path) -> Path:
    cache_dir.mkdir(parents=True, exist_ok=True)
    archive = cache_dir / f"{lock['revision']}-{lock['archive_sha256']}.tar.gz"
    if archive.exists():
        if digest(archive.read_bytes()) != lock["archive_sha256"]:
            raise EngineError(f"corrupt archive cache; remove {archive} and retry")
        return archive
    descriptor, name = tempfile.mkstemp(
        prefix=".archive-", suffix=".partial", dir=cache_dir
    )
    os.close(descriptor)
    temporary = Path(name)
    try:
        legacy = cache_dir / f"{lock['revision']}.tar.gz"
        if legacy.exists() and digest(legacy.read_bytes()) == lock["archive_sha256"]:
            shutil.copyfile(legacy, temporary)
        else:
            download(lock["repository"], lock["revision"], temporary)
        if digest(temporary.read_bytes()) != lock["archive_sha256"]:
            raise EngineError("downloaded archive checksum does not match engine.lock")
        os.replace(temporary, archive)
    finally:
        temporary.unlink(missing_ok=True)
    return archive


def extract_archive(archive: Path, workspace: Path) -> dict[str, str]:
    """Extract regular files and confined symlinks after validating the whole archive."""
    with tarfile.open(archive, "r:gz") as source:
        records: dict[str, tarfile.TarInfo] = {}
        prefix = None
        for member in source.getmembers():
            name = member.name.rstrip("/")
            relative(name)
            top, _, name = name.partition("/")
            prefix = prefix or top
            if top != prefix:
                raise EngineError("archive has multiple top-level directories")
            if not name:
                if not member.isdir():
                    raise EngineError("archive root must be a directory")
                continue
            relative(name)
            if name in records:
                raise EngineError(f"duplicate archive path: {name}")
            if not (member.isdir() or member.isfile() or member.issym()):
                raise EngineError(f"unsupported archive entry: {name}")
            if member.issym():
                safe_link(name, member.linkname)
            records[name] = member
        if not records:
            raise EngineError("empty engine archive")
        for name in records:
            for parent in PurePosixPath(name).parents:
                if str(parent) in records and not records[str(parent)].isdir():
                    raise EngineError(f"archive writes through a link or file: {name}")
        for name, member in records.items():
            path = located(workspace, name)
            path.parent.mkdir(parents=True, exist_ok=True)
            if member.isdir():
                path.mkdir(exist_ok=True)
            elif member.issym():
                create_link(path, member.linkname)
            else:
                stream = source.extractfile(member)
                if stream is None:
                    raise EngineError(f"unreadable archive file: {name}")
                with stream, path.open("xb") as output:
                    shutil.copyfileobj(stream, output)
                path.chmod(0o755 if member.mode & 0o111 else 0o644)
        validate_links(
            workspace, {name for name, member in records.items() if member.issym()}
        )
        return {
            name: "120000"
            if member.issym()
            else "100755"
            if member.mode & 0o111
            else "100644"
            for name, member in records.items()
            if not member.isdir()
        }


def git(workspace: Path, *args: str) -> bytes:
    env = os.environ.copy()
    env.update(
        {"LC_ALL": "C", "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
    )
    for name in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR"):
        env.pop(name, None)
    result = subprocess.run(
        [
            "git",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.filemode=false",
            "-c",
            "core.symlinks=true",
            "-c",
            "core.hooksPath=" + os.devnull,
            "-C",
            str(workspace),
            *args,
        ],
        env=env,
        capture_output=True,
        check=False,
    )
    if result.returncode:
        raise EngineError(
            f"git {' '.join(args)} failed:\n{result.stderr.decode(errors='replace')}"
        )
    return result.stdout + (result.stderr if "--verbose" in args else b"")


def create_link(path: Path, target: str) -> None:
    try:
        path.symlink_to(target)
    except OSError as error:
        raise EngineError(
            f"cannot preserve symlink {path}; enable symlink creation privileges: {error}"
        ) from error


def source_modes(root: Path) -> dict[str, str]:
    if os.name != "nt":
        return {}
    try:
        records = git(root, "ls-files", "--stage", "-z").split(b"\0")
    except EngineError:
        return {}
    return {
        os.fsdecode(record.split(b"\t", 1)[1]): record[:6].decode()
        for record in records
        if record
    }


def index_modes(workspace: Path, modes: dict[str, str]) -> None:
    for mode, option in (("100755", "+x"), ("100644", "-x")):
        paths = [name for name, value in modes.items() if value == mode]
        for start in range(0, len(paths), 100):
            git(
                workspace,
                "update-index",
                "--chmod=" + option,
                "--",
                *paths[start : start + 100],
            )


def changed_paths(workspace: Path, tree: str, *flags: str) -> set[str]:
    output = git(
        workspace, "diff", "--cached", "--name-only", "--no-renames", "-z", *flags, tree
    )
    return {os.fsdecode(name) for name in output.split(b"\0") if name}


def validate_links(workspace: Path, paths: set[str]) -> None:
    for name in paths:
        path = located(workspace, name)
        if path.is_symlink():
            safe_link(name, os.readlink(path))
            try:
                confined = path.resolve().is_relative_to(workspace.resolve())
            except (OSError, RuntimeError) as error:
                raise EngineError(f"invalid symlink chain: {name}") from error
            if not confined:
                raise EngineError(f"symlink chain escapes workspace: {name}")


def product_version(workspace: Path, version: str, toolchain: str) -> None:
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", version):
        raise EngineError("VERSION must contain one semantic product version")
    manifest = workspace / "codex-rs/Cargo.toml"
    text = manifest.read_text()
    parsed = tomllib.loads(text)
    before = parsed["workspace"]["package"]["version"]
    section = re.search(r"(?ms)^\[workspace\.package\]\s*\n(.*?)(?=^\[|\Z)", text)
    if section is None:
        raise EngineError("missing [workspace.package]")
    replaced, count = re.subn(
        r'(?m)^version\s*=\s*"[^"]*"', f'version = "{version}"', section[1]
    )
    if count != 1:
        raise EngineError("expected exactly one workspace.package.version")
    if before != version:
        manifest.write_text(
            text[: section.start(1)] + replaced + text[section.end(1) :]
        )
    actual_toolchain = tomllib.loads(
        (workspace / "codex-rs/rust-toolchain.toml").read_text()
    )["toolchain"]["channel"]
    if actual_toolchain != toolchain:
        raise EngineError(
            f"Rust toolchain mismatch: assembled {actual_toolchain}, engine.lock {toolchain}"
        )
    lock = tomllib.loads((workspace / "codex-rs/Cargo.lock").read_text())
    local = {
        (package["name"], package["version"])
        for package in lock["package"]
        if "source" not in package
    }
    for member in parsed["workspace"]["members"]:
        paths = sorted((workspace / "codex-rs").glob(f"{member}/Cargo.toml"))
        if not paths:
            raise EngineError(f"missing workspace member: {member}")
        for path in paths:
            package = tomllib.loads(path.read_text())["package"]
            expected = package["version"]
            if isinstance(expected, dict) and expected.get("workspace") is True:
                expected = version
            if (package["name"], expected) not in local:
                raise EngineError(
                    f"assembled Cargo.lock lacks {package['name']} {expected}; explicitly regenerate and commit it"
                )


def assemble(
    root: Path, workspace: Path, lock: dict[str, Any], archive: Path
) -> dict[str, Any]:
    inputs: dict[str, Any] = {}
    canonical = source_modes(root)

    def input_file(name: str) -> Path:
        path = located(root, name)
        if not path.is_file() and not path.is_symlink():
            raise EngineError(f"missing assembly input: {name}")
        inputs[name] = {
            "sha256": digest(blob(path)),
            "mode": canonical.get(name, file_mode(path)),
        }
        return path

    for name in (
        "engine.lock",
        lock["assembly_manifest"],
        lock["assembled_lock"],
        lock["product_version_file"],
    ):
        input_file(name)
    script = root / "scripts/engine.py"
    if script.is_file():
        input_file("scripts/engine.py")
    manifest = read_json(root / lock["assembly_manifest"])
    if manifest.get("schema_version") != 1:
        raise EngineError("unsupported assembly manifest schema_version")
    if manifest.get("inventory"):
        input_file(manifest["inventory"])
        input_file(manifest["regression_manifest"])
        import native_provider_architecture

        validator = getattr(
            native_provider_architecture, "validate_assembly_ownership", None
        )
        if validator is None:
            raise EngineError(
                "architecture checker is missing assembly ownership validation"
            )
        errors = validator(root, manifest)
        if errors:
            raise EngineError("invalid assembly ownership: " + "; ".join(errors))
    archive_modes = extract_archive(archive, workspace)
    git(workspace, "init", "--quiet")
    git(workspace, "add", "--force", "--all", "--", ".")
    index_modes(
        workspace,
        {name: mode for name, mode in archive_modes.items() if mode == "100755"},
    )
    pristine = git(workspace, "write-tree").decode().strip()
    allowed: set[str] = {"codex-rs/Cargo.lock", "codex-rs/Cargo.toml"}
    touched: set[str] = set()
    deletes = manifest.get("deletes", [])
    if len(deletes) != len(set(deletes)):
        raise EngineError("duplicate delete declarations")
    for name in deletes:
        relative(name)
    for patch in manifest.get("patches", []):
        paths = set(patch["paths"])
        if "codex-rs/Cargo.lock" in paths:
            raise EngineError(
                "Cargo.lock must come only from the committed assembled_lock input"
            )
        if not paths or len(paths) != len(patch["paths"]) or paths & touched:
            raise EngineError(f"duplicate or empty patch destination: {patch['path']}")
        for name in paths:
            relative(name)
        before = git(workspace, "write-tree").decode().strip()
        patch_file = input_file(patch["path"])
        checked = git(
            workspace,
            "apply",
            "--check",
            "--index",
            "--verbose",
            "--whitespace=nowarn",
            str(patch_file),
        )
        if re.search(
            rb"\(offset [+-]?\d+ lines?\)|\bfuzz \d+\b", checked, re.IGNORECASE
        ):
            raise EngineError(f"patch would require offset/fuzz: {patch['path']}")
        git(workspace, "apply", "--index", "--whitespace=nowarn", str(patch_file))
        actual = changed_paths(workspace, before)
        if actual != paths:
            raise EngineError(
                f"unexpected changed paths in {patch['path']}: expected {sorted(paths)}, got {sorted(actual)}"
            )
        validate_links(workspace, actual)
        touched.update(paths)
        allowed.update(paths)
    patched_deletes = changed_paths(workspace, pristine, "--diff-filter=D")
    if patched_deletes - set(deletes):
        raise EngineError(
            f"undeclared patch deletions: {sorted(patched_deletes - set(deletes))}"
        )
    for overlay in manifest.get("overlays", []):
        name = overlay["destination"]
        path = located(workspace, name)
        if name in touched or name in deletes or name == "codex-rs/Cargo.lock":
            raise EngineError(f"overlay destination collision: {name}")
        if (path.exists() or path.is_symlink()) != overlay.get("replace", False):
            raise EngineError(
                f"undeclared overlay replacement or missing replacement: {name}"
            )
        mode = overlay["mode"]
        if os.name == "nt":
            canonical.setdefault(overlay["source"], mode)
        source = input_file(overlay["source"])
        if (
            mode not in {"100644", "100755", "120000"}
            or inputs[overlay["source"]]["mode"] != mode
        ):
            raise EngineError(f"overlay source mode mismatch: {overlay['source']}")
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.exists() or path.is_symlink():
            if path.is_dir() and not path.is_symlink():
                raise EngineError(f"overlay replaces directory: {name}")
            path.unlink()
        if mode == "120000":
            target = (
                os.readlink(source)
                if source.is_symlink()
                else os.fsdecode(source.read_bytes())
            )
            safe_link(name, target)
            create_link(path, target)
        else:
            shutil.copyfile(source, path)
            path.chmod(0o755 if mode == "100755" else 0o644)
        touched.add(name)
        allowed.add(name)
    for name in deletes:
        path = located(workspace, name)
        if name in touched and name not in patched_deletes:
            raise EngineError(f"delete destination collision: {name}")
        if name not in patched_deletes:
            if not path.is_file() and not path.is_symlink():
                raise EngineError(f"delete must name an existing file: {name}")
            path.unlink()
        allowed.add(name)
    shutil.copyfile(root / lock["assembled_lock"], workspace / "codex-rs/Cargo.lock")
    version = (root / lock["product_version_file"]).read_text().strip()
    product_version(workspace, version, lock["toolchain"])
    git(workspace, "add", "--force", "--all", "--", ".")
    index_modes(
        workspace,
        {item["destination"]: item["mode"] for item in manifest.get("overlays", [])},
    )
    actual = changed_paths(workspace, pristine)
    if actual - allowed:
        raise EngineError(f"unexpected assembled paths: {sorted(actual - allowed)}")
    final_deletes = changed_paths(workspace, pristine, "--diff-filter=D")
    if final_deletes != set(deletes):
        raise EngineError(
            f"assembled deletes differ from declarations: {sorted(final_deletes)}"
        )
    validate_links(workspace, actual)
    diff = git(
        workspace,
        "diff",
        "--cached",
        "--binary",
        "--full-index",
        "--no-ext-diff",
        "--no-renames",
        pristine,
    )
    metadata = workspace / ".git/nyacode"
    metadata.mkdir()
    (metadata / "assembled.diff").write_bytes(diff)
    revision = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=root,
        capture_output=True,
        text=True,
        check=False,
    )
    report = {
        "source_revision": revision.stdout.strip()
        if revision.returncode == 0
        else None,
        "generated_by": "nyacode-engine-v1",
        "revision": lock["revision"],
        "archive_sha256": lock["archive_sha256"],
        "product_version": version,
        "inputs": inputs,
        "input_sha256": digest(json.dumps(inputs, sort_keys=True).encode()),
        "pristine_tree": pristine,
        "assembled_tree": git(workspace, "write-tree").decode().strip(),
        "diff_sha256": digest(diff),
        "changed_paths": sorted(actual),
    }
    write_json(workspace / MARKER, report)
    return report


def prepare(
    *, root: Path = ROOT, output: Path | None = None, cache_dir: Path | None = None
) -> Path:
    root = root.resolve()
    output = (output or root / ".engine/workspace").absolute()
    if output == root or root.is_relative_to(output) or output.is_symlink():
        raise EngineError("assembly output must be a disposable directory")
    if output.exists() and (
        not (output / MARKER).is_file()
        or read_json(output / MARKER).get("generated_by") != "nyacode-engine-v1"
    ):
        raise EngineError(f"refusing to replace non-engine directory: {output}")
    lock = load_lock(root)
    cache = (cache_dir or CACHE).resolve()
    if cache.is_relative_to(root):
        raise EngineError("archive cache must be outside the tracked product tree")
    original_lock = digest((root / "engine.lock").read_bytes())
    archive = archive_for(lock, cache)
    output.parent.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix=f".{output.name}.prepare-", dir=output.parent))
    backup = stage.with_name(stage.name + ".previous")
    try:
        report = assemble(root, stage, lock, archive)
        if report["inputs"]["engine.lock"]["sha256"] != original_lock:
            raise EngineError("engine.lock changed during preparation")
        canonical = source_modes(root)
        if os.name == "nt":
            for item in read_json(root / lock["assembly_manifest"]).get("overlays", []):
                canonical.setdefault(item["source"], item["mode"])
        for name, expected in report["inputs"].items():
            source = located(root, name)
            if {
                "sha256": digest(blob(source)),
                "mode": canonical.get(name, file_mode(source)),
            } != expected:
                raise EngineError(f"assembly input changed during preparation: {name}")
        if output.exists():
            output.rename(backup)
        try:
            stage.rename(output)
        except BaseException:
            if backup.exists():
                backup.rename(output)
            raise
        if backup.exists():
            shutil.rmtree(backup, ignore_errors=True)
    except BaseException as error:
        if stage.exists():
            write_json(stage / ".engine-failure.json", {"error": str(error)})
        raise
    return output


def assembly_report(workspace: Path) -> dict[str, Any]:
    return read_json(workspace / MARKER)


def check(
    *,
    root: Path = ROOT,
    output: Path | None = None,
    cache_dir: Path | None = None,
    mode: str = "controls",
    artifact_dir: Path | None = None,
) -> Path:
    if mode not in {"controls", "providers", "full"}:
        raise EngineError(f"unknown check mode: {mode}")
    workspace = prepare(root=root, output=output, cache_dir=cache_dir)
    artifacts = artifact_dir or workspace / ".git/nyacode/checks"
    artifacts.mkdir(parents=True, exist_ok=True)
    commands = [
        [
            sys.executable,
            "-m",
            "unittest",
            "discover",
            "-s",
            "scripts/tests",
            "-p",
            "test_engine.py",
        ]
    ]
    if mode in {"providers", "full"}:
        commands.extend(
            [
                [
                    "cargo",
                    "test",
                    "--locked",
                    "--manifest-path",
                    str(root / "crates/providers/Cargo.toml"),
                    "--workspace",
                ],
                [
                    sys.executable,
                    str(root / "scripts/native_provider_checks.py"),
                    "--repo",
                    str(workspace),
                    "--artifact-dir",
                    str(artifacts / "native-providers"),
                ],
            ]
        )
    stages = []
    for index, command in enumerate(commands):
        log = artifacts / f"{index + 1:02d}.log"
        with log.open("w") as stream:
            result = subprocess.run(
                command, cwd=root, stdout=stream, stderr=subprocess.STDOUT, check=False
            )
        stages.append(
            {"command": command, "exit_code": result.returncode, "log": str(log)}
        )
        write_json(artifacts / "summary.json", {"mode": mode, "stages": stages})
        if result.returncode:
            raise EngineError(f"engine {mode} check failed; see {log}")
    return workspace


def github_json(endpoint: str, log: Path) -> dict[str, Any]:
    result = subprocess.run(
        ["gh", "api", endpoint], capture_output=True, text=True, check=False
    )
    log.write_text(result.stdout + result.stderr)
    if result.returncode:
        raise EngineError(f"GitHub lookup failed; see {log}")
    value = json.loads(result.stdout)
    if not isinstance(value, dict):
        raise EngineError(f"unexpected GitHub response; see {log}")
    return value


def upgrade(
    *,
    root: Path = ROOT,
    tag: str,
    output: Path | None = None,
    cache_dir: Path | None = None,
    mode: str = "full",
) -> Path:
    if not STABLE_RELEASE.fullmatch(tag):
        raise EngineError("upgrade accepts only stable rust-vX.Y.Z release tags")
    root = root.resolve()
    proposal = (output or root / ".engine/upgrades" / tag).absolute()
    if proposal.exists():
        raise EngineError(f"upgrade diagnostics already exist: {proposal}")
    if proposal == root or root.is_relative_to(proposal):
        raise EngineError("upgrade output must be a disposable directory")
    lock = load_lock(root)
    cache = (cache_dir or CACHE).resolve()
    if cache.is_relative_to(root):
        raise EngineError("archive cache must be outside the tracked product tree")
    proposal.mkdir(parents=True)
    try:
        release = github_json(
            f"repos/{lock['repository']}/releases/tags/{tag}", proposal / "release.log"
        )
        if (
            release.get("tag_name") != tag
            or release.get("draft") is not False
            or release.get("prerelease") is not False
        ):
            raise EngineError("upgrade requires a published stable GitHub release")
        commit = github_json(
            f"repos/{lock['repository']}/commits/{tag}", proposal / "resolve.log"
        )
        revision = commit.get("sha", "")
        if not isinstance(revision, str) or not re.fullmatch(r"[0-9a-f]{40}", revision):
            raise EngineError(
                "gh could not resolve stable release to an immutable commit"
            )
        comparison = github_json(
            f"repos/{lock['repository']}/compare/{lock['revision']}...{revision}",
            proposal / "ancestry.log",
        )
        if (
            comparison.get("status") != "ahead"
            or comparison.get("merge_base_commit", {}).get("sha") != lock["revision"]
        ):
            raise EngineError(
                "release must descend from and be newer than the accepted engine"
            )
        archive = proposal / "upstream.tar.gz"
        download(lock["repository"], revision, archive)
        candidate = {
            **lock,
            "release_tag": tag,
            "revision": revision,
            "archive_sha256": digest(archive.read_bytes()),
        }
        write_json(proposal / "engine.lock", candidate)
        cache = (cache_dir or CACHE).resolve()
        cache.mkdir(parents=True, exist_ok=True)
        pinned = cache / f"{revision}-{candidate['archive_sha256']}.tar.gz"
        if (
            pinned.exists()
            and digest(pinned.read_bytes()) != candidate["archive_sha256"]
        ):
            raise EngineError(f"corrupt archive cache: {pinned}")
        if not pinned.exists():
            temporary = cache / f".{revision}-{os.getpid()}.partial"
            try:
                shutil.copyfile(archive, temporary)
                os.replace(temporary, pinned)
            finally:
                temporary.unlink(missing_ok=True)
        # Candidate inputs are copied; preflight cannot edit accepted patches or fixtures.
        candidate_root = proposal / "product"
        candidate_root.mkdir()
        manifest = read_json(root / lock["assembly_manifest"])
        names = {
            lock["assembly_manifest"],
            lock["assembled_lock"],
            lock["product_version_file"],
        }
        if manifest.get("inventory"):
            names.add(manifest["inventory"])
            names.add(manifest["regression_manifest"])
        if (root / "docs/native-providers/integration-manifest.json").exists():
            names.add("docs/native-providers/integration-manifest.json")
        names.update(entry["path"] for entry in manifest.get("patches", []))
        names.update(entry["source"] for entry in manifest.get("overlays", []))
        names.update(
            path.relative_to(root).as_posix()
            for path in (root / "scripts").rglob("*.py")
        )
        providers = root / "crates/providers"
        if providers.exists():
            names.update(
                path.relative_to(root).as_posix()
                for path in providers.rglob("*")
                if path.is_file() and "target" not in path.relative_to(providers).parts
            )
        for name in sorted(names):
            source = located(root, name)
            destination = located(candidate_root, name)
            destination.parent.mkdir(parents=True, exist_ok=True)
            if source.is_symlink():
                safe_link(name, os.readlink(source))
            shutil.copy2(source, destination, follow_symlinks=False)
        write_json(candidate_root / "engine.lock", candidate)
        workspace = check(
            root=candidate_root,
            output=proposal / "workspace",
            cache_dir=cache,
            mode=mode,
            artifact_dir=proposal / "checks",
        )
        write_json(
            proposal / "result.json",
            {
                "result": "passed",
                "mode": mode,
                "workspace": str(workspace),
                "accepted_lock_changed": False,
                "qualification_required": [
                    "six-platform-release",
                    "provider-sdk-suites",
                ],
            },
        )
    except BaseException as error:
        write_json(
            proposal / "result.json",
            {"result": "failed", "error": str(error), "accepted_lock_changed": False},
        )
        raise
    return proposal


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--cache-dir", type=Path)
    parser.add_argument("--json", action="store_true")
    subparsers = parser.add_subparsers(dest="command", required=True)
    for name in ("prepare", "check", "upgrade"):
        command = subparsers.add_parser(name)
        command.add_argument("--output", type=Path)
        if name in {"check", "upgrade"}:
            command.add_argument(
                "--mode",
                choices=("controls", "providers", "full"),
                default="full" if name == "upgrade" else "controls",
            )
        if name == "check":
            command.add_argument("--artifact-dir", type=Path)
        if name == "upgrade":
            command.add_argument("tag")
    args = parser.parse_args()
    kwargs = {"root": args.root, "output": args.output, "cache_dir": args.cache_dir}
    try:
        if args.command == "prepare":
            output = prepare(**kwargs)
        elif args.command == "check":
            output = check(**kwargs, mode=args.mode, artifact_dir=args.artifact_dir)
        else:
            output = upgrade(**kwargs, tag=args.tag, mode=args.mode)
        result = {"output": str(output)}
        if args.command != "upgrade":
            result.update(
                {
                    "report": str(output / MARKER),
                    "diff": str(output / ".git/nyacode/assembled.diff"),
                    "input_sha256": assembly_report(output)["input_sha256"],
                }
            )
        print(json.dumps(result, sort_keys=True) if args.json else str(output))
        return 0
    except (EngineError, OSError, ValueError, KeyError, tarfile.TarError) as error:
        print(f"engine: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
