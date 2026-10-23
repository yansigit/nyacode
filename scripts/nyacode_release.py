#!/usr/bin/env python3
"""Assemble, smoke-test, and verify Nyacode's installer-compatible releases."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
os.environ.setdefault("CODEX_REPO_ROOT", str(ROOT))
sys.path.insert(0, str(ROOT / ".github/scripts"))

from releases import is_valid_release_version
from codex_package.archive import write_archive
from codex_package.cli import assemble_package, create_parser
from codex_package.layout import validate_package_dir
from codex_package.targets import PACKAGE_VARIANTS, TARGET_SPECS
from codex_package.version import read_workspace_version

RELEASE_TARGETS = (
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
)
CHECKSUMS = "codex-package_SHA256SUMS"
PROVENANCE = "nyacode-release.json"
NOTICES = (
    "LICENSE",
    "NOTICE",
    "codex-rs/chatgpt-chat/NOTICE",
    "codex-rs/chatgpt-chat/fonts/LICENSE",
    "codex-rs/chatgpt-chat/pdf-validator/THIRD_PARTY_NOTICES",
    "codex-rs/chatgpt-chat/pdf-validator/RUST_LIBRARY_NOTICES",
    "codex-rs/providers/provider-opencode/LICENSE-pi",
    "codex-rs/vendor/bubblewrap/COPYING",
)


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def validate_version(version: str, tag: str | None = None) -> None:
    if not is_valid_release_version(version):
        raise ValueError(f"Unsupported installer version: {version!r}")
    # Reject ambiguous numeric components even though older installers accept them.
    if any(
        len(part) > 1 and part.startswith("0") for part in re.findall(r"\d+", version)
    ):
        raise ValueError(f"Noncanonical version: {version!r}")
    if tag is not None:
        if tag != f"rust-v{version}":
            raise ValueError(
                f"Tag {tag!r} does not match workspace version {version!r}"
            )
        if version.split("-", 1)[0] == "0.0.0":
            raise ValueError("Bump the development version before publishing")


def source_commit() -> str:
    return subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
    ).strip()


def engine_provenance() -> dict:
    workspace = os.environ.get("NYACODE_ENGINE_ROOT")
    if workspace is None:
        return {}
    from engine import assembly_report, blob, digest, file_mode, source_modes

    report = assembly_report(Path(workspace))
    lock = json.loads((ROOT / "engine.lock").read_text())
    if (
        report["revision"] != lock["revision"]
        or report["archive_sha256"] != lock["archive_sha256"]
    ):
        raise ValueError("Assembled engine does not match the accepted engine pin")
    modes = source_modes(ROOT)
    for relative, expected in report["inputs"].items():
        path = ROOT / relative
        if (
            not (path.exists() or path.is_symlink())
            or digest(blob(path)) != expected["sha256"]
            or modes.get(relative, file_mode(path)) != expected["mode"]
        ):
            raise ValueError(f"Assembly input changed since preparation: {relative}")
    return {
        "engine_revision": lock["revision"],
        "engine_archive_sha256": lock["archive_sha256"],
        "integration_input_sha256": report["input_sha256"],
        "integration_contract_version": lock["integration_contract_version"],
    }


def metadata(tag: str | None, output: Path | None) -> None:
    version = read_workspace_version()
    validate_version(version, tag)
    if tag:
        # A version must be reviewed on main before it can become a release.
        subprocess.run(
            ["git", "merge-base", "--is-ancestor", "HEAD", "origin/main"],
            cwd=ROOT,
            check=True,
        )
    values = {"version": version, "commit": source_commit()}
    if output:
        with output.open("a", encoding="utf-8") as stream:
            for key, value in values.items():
                stream.write(f"{key}={value}\n")
    print(json.dumps(values))


def check_package(package: Path, target: str, version: str) -> None:
    spec = TARGET_SPECS[target]
    validate_package_dir(package, PACKAGE_VARIANTS["codex"], spec)
    package_metadata = json.loads((package / "codex-package.json").read_text())
    if package_metadata["version"] != version:
        raise ValueError("Package version does not match the release")
    for relative in NOTICES:
        if not (package / "licenses" / relative).is_file():
            raise ValueError(f"Missing license/notice: {relative}")
    public_binary = package / "bin" / f"nyacode{spec.exe_suffix}"
    if not public_binary.is_file():
        raise ValueError("Missing public nyacode executable")
    if sha256(public_binary) != sha256(package / "bin" / f"codex{spec.exe_suffix}"):
        raise ValueError("Public executable and compatibility alias differ")


def smoke(package: Path, target: str, version: str) -> None:
    """Run shipped bytes, with isolated application state and no account access."""
    spec = TARGET_SPECS[target]
    check_package(package, target, version)
    with tempfile.TemporaryDirectory(prefix="nyacode-smoke-home-") as temp:
        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("CODEX_", "NYACODE_", "OPENAI_"))
        }
        env["NYACODE_HOME"] = temp
        commands = [
            (f"bin/nyacode{spec.exe_suffix}", ["--version"], f"nyacode {version}\n"),
            (f"bin/codex{spec.exe_suffix}", ["--version"], f"nyacode {version}\n"),
            (f"bin/nyacode{spec.exe_suffix}", ["--help"], "Usage: nyacode"),
            (f"bin/nyacode{spec.exe_suffix}", ["features", "list"], "chatgpt_chat"),
            (f"bin/codex-code-mode-host{spec.exe_suffix}", ["--help"], "--listen"),
            (f"codex-path/rg{spec.exe_suffix}", ["--version"], "ripgrep"),
        ]
        if spec.is_linux:
            commands.append(("codex-resources/bwrap", ["--version"], "bubblewrap"))
        for binary, arguments, expected in commands:
            result = subprocess.run(
                [str(package / binary), *arguments],
                cwd=temp,
                env=env,
                capture_output=True,
                text=True,
                check=True,
                timeout=60,
            )
            if (
                arguments == ["--version"]
                and binary.startswith("bin/")
                and result.stdout != expected
            ) or expected not in result.stdout:
                raise ValueError(f"Unexpected output from {binary}: {result.stdout!r}")
        # Exercise app-server initialization without authentication or inference.
        subprocess.run(
            [
                str(package / "bin" / f"nyacode{spec.exe_suffix}"),
                "app-server",
                "generate-json-schema",
                "--out",
                str(Path(temp) / "schema"),
            ],
            env=env,
            cwd=temp,
            capture_output=True,
            check=True,
            timeout=120,
        )


def assemble(target: str, binaries: Path, dist: Path) -> None:
    engine_root = Path(os.environ.get("NYACODE_ENGINE_ROOT", ROOT))
    provenance = engine_provenance()
    spec = TARGET_SPECS[target]
    version = read_workspace_version()
    validate_version(version)
    dist.mkdir(parents=True, exist_ok=True)
    archive_name = f"codex-package-{target}.tar.gz"
    report_path = dist / f"nyacode-{target}.json"
    if (dist / archive_name).exists() or report_path.exists():
        raise ValueError("Refusing to overwrite an existing build artifact")
    with tempfile.TemporaryDirectory(prefix="nyacode-package-") as temp:
        package = Path(temp) / "package"
        arguments = [
            "--target",
            target,
            "--package-version",
            version,
            "--package-dir",
            str(package),
            "--cargo-profile",
            "release",
            "--strip",
            "all",
            "--entrypoint-bin",
            str(binaries / f"nyacode{spec.exe_suffix}"),
            "--code-mode-host-bin",
            str(binaries / f"codex-code-mode-host{spec.exe_suffix}"),
        ]
        if spec.is_linux:
            arguments += ["--bwrap-bin", str(binaries / "bwrap")]
        if spec.is_windows:
            for helper in ("codex-command-runner", "codex-windows-sandbox-setup"):
                arguments += [f"--{helper}-bin", str(binaries / f"{helper}.exe")]
        assemble_package(create_parser().parse_args(arguments))
        shutil.copy2(
            package / "bin" / f"codex{spec.exe_suffix}",
            package / "bin" / f"nyacode{spec.exe_suffix}",
        )
        for relative in NOTICES:
            destination = package / "licenses" / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(engine_root / relative, destination)
        check_package(package, target, version)
        archive_path = dist / archive_name
        write_archive(package, archive_path, force=False)
        # Smoke-test a fresh extraction of the actual delivery archive.
        extracted = Path(temp) / "extracted"
        extract_package(archive_path, extracted)
        smoke(extracted, target, version)
    toolchain = tomllib.loads(
        (engine_root / "codex-rs/rust-toolchain.toml").read_text()
    )
    write_json(
        report_path,
        {
            "schema_version": 1,
            "version": version,
            "commit": source_commit(),
            "target": target,
            "profile": "release",
            "rust_toolchain": toolchain["toolchain"]["channel"],
            "archive": archive_name,
            "sha256": sha256(archive_path),
            "smoke_test": "passed",
            "platform_signed": False,
            **provenance,
        },
    )


def extract_package(archive: Path, destination: Path) -> None:
    with tarfile.open(archive, "r:gz") as stream:
        # The canonical package contains only regular files and directories.
        members = stream.getmembers()
        if any(not (member.isfile() or member.isdir()) for member in members):
            raise ValueError("Unexpected link/device in release archive")
        stream.extractall(destination, members=members, filter="data")


def verify(dist: Path, version: str, commit: str) -> list[dict]:
    provenance = engine_provenance()
    expected = {
        name
        for target in RELEASE_TARGETS
        for name in (f"codex-package-{target}.tar.gz", f"nyacode-{target}.json")
    }
    actual = {path.name for path in dist.iterdir()}
    if actual != expected:
        raise ValueError(
            f"Incomplete/unexpected artifacts: missing={expected - actual}, extra={actual - expected}"
        )
    reports = []
    for target in RELEASE_TARGETS:
        report = json.loads((dist / f"nyacode-{target}.json").read_text())
        expected_fields = {
            "schema_version": 1,
            "version": version,
            "commit": commit,
            "target": target,
            "profile": "release",
            "rust_toolchain": "1.95.0",
            "archive": f"codex-package-{target}.tar.gz",
            "smoke_test": "passed",
            "platform_signed": False,
            **provenance,
        }
        if any(report.get(key) != value for key, value in expected_fields.items()):
            raise ValueError(f"Inconsistent build report: {target}")
        if report.get("engine_revision") and not provenance:
            raise ValueError(
                "NYACODE_ENGINE_ROOT is required to verify assembled packages"
            )
        if reports and any(
            report.get(key) != reports[0].get(key)
            for key in (
                "engine_revision",
                "engine_archive_sha256",
                "integration_input_sha256",
                "integration_contract_version",
            )
        ):
            raise ValueError(f"Inconsistent engine provenance: {target}")
        archive = dist / report["archive"]
        if report.get("sha256") != sha256(archive):
            raise ValueError(f"Archive digest mismatch: {target}")
        with tempfile.TemporaryDirectory(prefix="nyacode-verify-") as temp:
            package = Path(temp)
            extract_package(archive, package)
            check_package(package, target, version)
        reports.append(report)
    return reports


def seal(dist: Path, version: str, commit: str) -> None:
    reports = verify(dist, version, commit)
    write_json(
        dist / PROVENANCE,
        {
            "schema_version": 1,
            "version": version,
            "commit": commit,
            **engine_provenance(),
            "packages": reports,
        },
    )
    (dist / CHECKSUMS).write_text(
        "".join(f"{report['sha256']}  {report['archive']}\n" for report in reports),
        encoding="utf-8",
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    meta = commands.add_parser("metadata")
    meta.add_argument("--tag")
    meta.add_argument("--github-output", type=Path)
    package = commands.add_parser("assemble")
    package.add_argument("--target", required=True, choices=RELEASE_TARGETS)
    package.add_argument("--binaries", required=True, type=Path)
    package.add_argument("--dist", required=True, type=Path)
    collect = commands.add_parser("seal")
    collect.add_argument("--dist", required=True, type=Path)
    collect.add_argument("--version", required=True)
    collect.add_argument("--commit", required=True)
    args = parser.parse_args()
    if args.command == "metadata":
        metadata(args.tag, args.github_output)
    elif args.command == "assemble":
        assemble(args.target, args.binaries.resolve(), args.dist.resolve())
    else:
        seal(args.dist, args.version, args.commit)


if __name__ == "__main__":
    main()
