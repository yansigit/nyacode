#!/usr/bin/env python3
"""Run a developer command in the pinned, assembled engine workspace."""

import argparse
import os
from pathlib import Path
import subprocess

from engine import prepare

ROOT = Path(__file__).resolve().parents[1]


def locked_command(command: list[str]) -> list[str]:
    if Path(command[0]).name not in {"cargo", "cargo.exe"}:
        return command
    position = 2
    if command[1:3] == ["nextest", "run"]:
        position = 3
    elif len(command) < 2 or command[1] not in {
        "build",
        "check",
        "test",
        "bench",
        "run",
        "clippy",
        "fetch",
        "metadata",
    }:
        return command
    options = command[: command.index("--")] if "--" in command else command
    if "--locked" not in options:
        return command[:position] + ["--locked"] + command[position:]
    return command


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace-root", action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.command[:1] == ["--"]:
        args.command = args.command[1:]
    if not args.command:
        parser.error("a command is required")
    workspace = prepare(root=ROOT)
    env = os.environ.copy()
    env["CODEX_REPO_ROOT"] = str(workspace)
    env["NYACODE_ENGINE_ROOT"] = str(workspace)
    env.setdefault("CARGO_TARGET_DIR", str(ROOT / ".engine/target"))
    return subprocess.run(
        locked_command(args.command),
        cwd=workspace if args.workspace_root else workspace / "codex-rs",
        env=env,
        check=False,
    ).returncode


if __name__ == "__main__":
    raise SystemExit(main())
