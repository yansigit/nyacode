#!/usr/bin/env python3
"""Build Nyacode and its Code Mode helper using the pinned V8 artifacts."""

import argparse
import os
from pathlib import Path
import subprocess
import sys


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--release", action="store_true", help="Build optimized binaries"
    )
    parser.add_argument(
        "--engine",
        action="store_true",
        help="Compatibility flag; the pinned engine workspace is always used",
    )
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    from engine import prepare

    build_root = prepare(root=root)
    os.environ["CODEX_REPO_ROOT"] = str(build_root)
    # Reuse the package builder's archive/bindings checksum validation.
    sys.path.insert(0, str(build_root / "scripts"))
    from codex_package.targets import TARGET_SPECS
    from codex_package.v8 import resolve_codex_v8_cargo_env

    rustc_version = subprocess.check_output(
        ["rustc", "-vV"], cwd=build_root / "codex-rs", text=True
    )
    host = next(
        line.removeprefix("host: ")
        for line in rustc_version.splitlines()
        if line.startswith("host: ")
    )
    target = os.environ.get("CARGO_BUILD_TARGET", host)
    if target not in TARGET_SPECS:
        parser.error(f"No pinned V8 package configuration for {target}")
    env = os.environ.copy()
    env.setdefault("CARGO_TARGET_DIR", str(root / ".engine/target"))
    env.update(resolve_codex_v8_cargo_env(TARGET_SPECS[target]))
    command = [
        "cargo",
        "build",
        "--locked",
        "-p",
        "codex-cli",
        "-p",
        "codex-code-mode-host",
        "--bin",
        "nyacode",
        "--bin",
        "codex",
        "--bin",
        "codex-code-mode-host",
    ]
    if args.release:
        command.append("--release")
    return subprocess.run(
        command, cwd=build_root / "codex-rs", env=env, check=False
    ).returncode


if __name__ == "__main__":
    raise SystemExit(main())
