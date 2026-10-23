#!/usr/bin/env python3
"""Publish verified Nyacode assets; interrupted uploads remain private drafts."""

import argparse
import json
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))

from nyacode_release import CHECKSUMS, PROVENANCE, seal, sha256, validate_version
from releases import should_make_latest

REPOSITORY = "yansigit/nyacode"


def api(endpoint: str, body: dict | None = None, method: str | None = None):
    command = ["gh", "api", f"repos/{REPOSITORY}/{endpoint}"]
    if body is not None:
        command += ["--method", method or "POST", "--input", "-"]
    result = subprocess.run(
        command,
        input=json.dumps(body) if body is not None else None,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode:
        # A genuine 404 is the only acceptable absent release/latest response.
        if body is None and "(HTTP 404)" in result.stderr:
            return None
        raise RuntimeError(f"GitHub request failed: {result.stderr.strip()}")
    return json.loads(result.stdout)


def verify_assets(
    assets: list[dict], files: dict[str, Path], *, complete: bool
) -> None:
    by_name = {asset["name"]: asset for asset in assets}
    if len(by_name) != len(assets) or set(by_name) - files.keys():
        raise ValueError("Release has duplicate or unexpected assets")
    if complete and by_name.keys() != files.keys():
        raise ValueError("Release upload is incomplete")
    for name, asset in by_name.items():
        if asset.get("digest") != f"sha256:{sha256(files[name])}":
            raise ValueError(f"Existing asset differs from this build: {name}")
        if asset.get("state") != "uploaded":
            raise ValueError(f"Incomplete remote asset: {name}")


def publish(dist: Path, version: str, commit: str) -> None:
    tag = f"rust-v{version}"
    validate_version(version, tag)
    resolved = api(f"commits/{tag}")
    if resolved is None or resolved["sha"] != commit:
        raise ValueError("The remote release tag no longer names the verified commit")
    seal(dist, version, commit)
    files = {path.name: path for path in dist.glob("*.tar.gz")}
    files.update({name: dist / name for name in (CHECKSUMS, PROVENANCE)})
    release = api(f"releases/tags/{tag}")
    if release is None:
        notes = api(
            "releases/generate-notes", {"tag_name": tag, "target_commitish": commit}
        )
        body = (
            "Optimized Nyacode packages for Linux, macOS, and Windows (x64 and ARM64).\n\n"
            "See [installation instructions](https://github.com/yansigit/nyacode/blob/"
            f"{tag}/docs/install.md). Assets retain the codex-package names used by the "
            "Nyacode installers. SHA-256 checksums and build metadata are attached. "
            "macOS packages are not notarized and Windows packages are not Authenticode signed.\n\n"
            f"Source commit: `{commit}`.\n\n{notes['body']}"
        )
        release = api(
            "releases",
            {
                "tag_name": tag,
                "target_commitish": commit,
                "name": f"Nyacode {version}",
                "body": body,
                "draft": True,
                "prerelease": "-" in version,
            },
        )
    if release["target_commitish"] != commit or release["prerelease"] != (
        "-" in version
    ):
        raise ValueError(
            "Existing release belongs to different source/channel metadata"
        )
    verify_assets(release["assets"], files, complete=not release["draft"])
    if not release["draft"]:
        print(f"Release {tag} already contains exactly these verified assets")
        return
    existing = {asset["name"] for asset in release["assets"]}
    missing = [str(path) for name, path in files.items() if name not in existing]
    if missing:
        subprocess.run(
            ["gh", "release", "upload", tag, "--repo", REPOSITORY, *missing],
            check=True,
        )
    uploaded = api(f"releases/{release['id']}")
    verify_assets(uploaded["assets"], files, complete=True)
    latest = False
    if "-" not in version:
        current = api("releases/latest")
        latest = current is None or should_make_latest(version, current)
    api(
        f"releases/{release['id']}",
        {
            "draft": False,
            "make_latest": str(latest).lower(),
        },
        method="PATCH",
    )
    print(f"Published https://github.com/{REPOSITORY}/releases/tag/{tag}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit", required=True)
    arguments = parser.parse_args()
    publish(arguments.dist, arguments.version, arguments.commit)
