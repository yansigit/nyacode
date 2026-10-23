#!/usr/bin/env python3
"""Run a developer command in the pinned, assembled engine workspace."""

import argparse
import os
from pathlib import Path
import subprocess

from engine import prepare

ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace-root", action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if not args.command:
        parser.error("a command is required")
    workspace = prepare(root=ROOT)
    env = os.environ.copy()
    env["CODEX_REPO_ROOT"] = str(workspace)
    env.setdefault("CARGO_TARGET_DIR", str(ROOT / ".engine/target"))
    return subprocess.run(
        args.command,
        cwd=workspace if args.workspace_root else workspace / "codex-rs",
        env=env,
        check=False,
    ).returncode


if __name__ == "__main__":
    raise SystemExit(main())
