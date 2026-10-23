#!/usr/bin/env python3
"""Select affected Rust packages and prepare verified container build inputs."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import tempfile
import urllib.request
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parents[1]
WORKSPACE_INPUTS = {
    "engine.lock",
    "VERSION",
    "scripts/engine.py",
    "integration/codex/assembly.json",
    "integration/codex/Cargo.lock",
    "crates/providers/Cargo.toml",
    "crates/providers/Cargo.lock",
    "crates/providers/rust-toolchain.toml",
    "Containerfile",
    "Dockerfile",
    "scripts/verify-container.sh",
    "scripts/verify_container.py",
    "codex-rs/Cargo.toml",
    "codex-rs/Cargo.lock",
    "codex-rs/rust-toolchain.toml",
    "codex-rs/rustfmt.toml",
}


def assembly_changes(root: Path, changed: set[str]) -> set[str]:
    """Map owned sources and patches to the engine files their consumers compile."""
    manifest_path = root / "integration/codex/assembly.json"
    if not manifest_path.is_file():
        return changed
    manifest = json.loads(manifest_path.read_text())
    destinations: dict[str, set[str]] = {}
    for overlay in manifest["overlays"]:
        destinations.setdefault(overlay["source"], set()).add(overlay["destination"])
    for patch in manifest["patches"]:
        destinations.setdefault(patch["path"], set()).update(patch["paths"])
    return {
        destination
        for source in changed
        for destination in (
            {source}
            if source in WORKSPACE_INPUTS
            else destinations.get(source, {source})
        )
    }


def select_packages(root: Path, changed: set[str], packages: list[dict]) -> list[str]:
    """Include reverse dependencies; shared or unclassified Rust inputs select all."""
    names = {package["name"] for package in packages}
    if any(
        path in WORKSPACE_INPUTS
        or path.startswith(("codex-rs/.cargo/", "codex-rs/.config/", "third_party/v8/"))
        for path in changed
    ):
        return sorted(names)
    selected = set()
    directories = sorted(
        ((Path(p["manifest_path"]).parent, p["name"]) for p in packages),
        key=lambda item: len(item[0].parts),
        reverse=True,
    )
    for relative in changed:
        path = root / relative
        owner = next(
            (name for directory, name in directories if path.is_relative_to(directory)),
            None,
        )
        if owner:
            selected.add(owner)
        elif relative.startswith(("codex-rs/", "crates/", "integration/codex/")):
            return sorted(names)
    while True:
        dependents = {
            p["name"]
            for p in packages
            if any(d.get("path") and d["name"] in selected for d in p["dependencies"])
        }
        if dependents <= selected:
            return sorted(selected)
        selected |= dependents


def affected_packages(root: Path, workspace: Path | None = None) -> list[str]:
    def git(*args: str) -> bytes:
        return subprocess.check_output(["git", *args], cwd=root)

    base = None
    for ref in ("origin/main", "main"):
        exists = subprocess.run(
            ["git", "rev-parse", "--verify", "--quiet", ref],
            cwd=root,
            stdout=subprocess.DEVNULL,
            check=False,
        )
        if exists.returncode == 0:
            base = git("merge-base", "HEAD", ref).decode().strip()
            break
    if base is None:
        raise ValueError(
            "fast verification requires origin/main or main with shared history"
        )
    paths = git("diff", "--name-only", "--no-renames", "-z", base)
    paths += git("ls-files", "--others", "--exclude-standard", "-z")
    changed = {os.fsdecode(path) for path in paths.split(b"\0") if path}
    workspace = workspace or root
    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
            cwd=workspace / "codex-rs",
        )
    )
    mapped = assembly_changes(root, changed)
    if workspace != root:
        # Metadata paths refer to the assembly, while Git paths refer to owned sources.
        for package in metadata["packages"]:
            relative = Path(package["manifest_path"]).relative_to(workspace)
            package["manifest_path"] = str(root / relative)
    return select_packages(root, mapped, metadata["packages"])


def manifest_hashes(content: bytes) -> dict[str, str]:
    result = {}
    for line in content.decode("ascii").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9_.-]+)", line)
        if not match or match[2] in result:
            raise ValueError("invalid or duplicate artifact checksum entry")
        result[match[2]] = match[1]
    return result


def file_hash(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verified_download(url: str, destination: Path, expected: str, limit: int) -> None:
    if destination.is_file() and file_hash(destination) == expected:
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(
            dir=destination.parent, delete=False
        ) as output:
            temporary = Path(output.name)
            with urllib.request.urlopen(url, timeout=120) as response:
                if not response.url.startswith("https://"):
                    raise ValueError("artifact download redirected away from HTTPS")
                count = 0
                while chunk := response.read(1024 * 1024):
                    count += len(chunk)
                    if count > limit:
                        raise ValueError("artifact exceeds its download size limit")
                    output.write(chunk)
        if file_hash(temporary) != expected:
            raise ValueError(f"checksum mismatch for {destination.name}")
        temporary.replace(destination)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def prepare_v8(root: Path, cache: Path, target: str) -> tuple[Path, Path]:
    if target not in {"x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"}:
        raise ValueError(f"unsupported verification host: {target}")
    lock = tomllib.loads((root / "codex-rs/Cargo.lock").read_text())
    versions = [p["version"] for p in lock["package"] if p["name"] == "v8"]
    if len(versions) != 1 or not re.fullmatch(r"\d+\.\d+\.\d+", versions[0]):
        raise ValueError("expected one pinned V8 crate version")
    version = versions[0]
    profile = "ptrcomp_sandbox_release"
    manifest = f"rusty_v8_{profile}_{target}.sha256"
    archive = f"librusty_v8_{profile}_{target}.a.gz"
    binding = f"src_binding_{profile}_{target}.rs"
    trusted = manifest_hashes(
        (
            root
            / f"third_party/v8/rusty_v8_{version.replace('.', '_')}_release_manifests.sha256"
        ).read_bytes()
    )
    destination = cache / version / target
    base_url = f"https://github.com/openai/codex/releases/download/rusty-v8-v{version}"
    verified_download(
        f"{base_url}/{manifest}", destination / manifest, trusted[manifest], 64 * 1024
    )
    hashes = manifest_hashes((destination / manifest).read_bytes())
    if set(hashes) != {archive, binding}:
        raise ValueError(
            "V8 checksum manifest must contain exactly the archive and bindings"
        )
    for name, limit in [(archive, 2 * 1024**3), (binding, 16 * 1024**2)]:
        verified_download(f"{base_url}/{name}", destination / name, hashes[name], limit)
    return destination / archive, destination / binding


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--workspace", type=Path)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("affected")
    v8 = commands.add_parser("prepare-v8")
    v8.add_argument("--cache", type=Path, required=True)
    v8.add_argument("--target", required=True)
    args = parser.parse_args()
    if args.command == "affected":
        print("\n".join(affected_packages(args.root, args.workspace)))
    else:
        print(*prepare_v8(args.root, args.cache, args.target), sep="\n")


if __name__ == "__main__":
    main()
