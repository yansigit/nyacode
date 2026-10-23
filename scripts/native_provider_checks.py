#!/usr/bin/env python3
"""Run the focused native-provider validation suite and save every stage log."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]


def run_checks(root: Path, artifact_dir: Path) -> list[dict[str, Any]]:
    manifest = json.loads(
        (root / "docs/native-providers/integration-manifest.json").read_text()
    )
    artifact_dir.mkdir(parents=True, exist_ok=True)
    summary_path = artifact_dir / "summary.json"
    stages: list[dict[str, Any]] = []
    env = os.environ.copy()
    env["CARGO_TERM_COLOR"] = "never"
    # Match the repository's `just test` stack size for large host test futures.
    env.setdefault("RUST_MIN_STACK", "8388608")
    env.setdefault("CARGO_BUILD_JOBS", "2")
    env["CARGO_INCREMENTAL"] = "0"
    target_dir = Path(env.get("CARGO_TARGET_DIR", root / "codex-rs/target"))
    if not target_dir.is_absolute():
        target_dir = root / target_dir
    binary_dir = target_dir / env.get("CARGO_BUILD_TARGET", "") / "debug"
    suffix = ".exe" if sys.platform == "win32" else ""
    env["NYACODE_TEST_BINARY"] = str(binary_dir / f"nyacode{suffix}")
    env["CODEX_EXEC_PATH"] = str(binary_dir / f"codex{suffix}")
    revision = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=root,
        capture_output=True,
        text=True,
        check=False,
    )
    metadata = {
        "repository_commit": revision.stdout.strip()
        if revision.returncode == 0
        else None,
        "github_sha": env.get("GITHUB_SHA"),
    }
    (artifact_dir / "run-metadata.json").write_text(
        json.dumps(metadata, indent=2) + "\n"
    )

    def run_stage(
        name: str, command: list[str], minimum_tests: int | None = None
    ) -> None:
        if command[0] == "cargo" and command[1] != "fmt" and "--locked" not in command:
            command = command[:2] + ["--locked"] + command[2:]
        print(f"[native-provider-checks] starting {name}", flush=True)
        started = time.monotonic()
        log_path = artifact_dir / f"{len(stages) + 1:02d}-{name}.log"
        try:
            with log_path.open("w") as log:
                result = subprocess.run(
                    command,
                    cwd=root,
                    env=env,
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    text=True,
                    check=False,
                )
        except OSError as error:
            log_path.write_text(f"unable to start {command[0]}: {error}\n")
            return_code = 127
        else:
            return_code = result.returncode
        test_count = None
        if minimum_tests is not None:
            log_contents = log_path.read_text()
            test_count = sum(
                int(count)
                for count in re.findall(
                    r"test result: [^\n]*?(\d+) passed;", log_contents
                )
            )
            if match := re.search(r"Ran (\d+) tests? in", log_contents):
                skipped = re.search(r"skipped=(\d+)", log_contents)
                test_count += int(match[1]) - (int(skipped[1]) if skipped else 0)
            if return_code == 0 and test_count < minimum_tests:
                with log_path.open("a") as log:
                    log.write(
                        f"\nexpected at least {minimum_tests} tests, observed {test_count}\n"
                    )
                return_code = 1
        stage = {
            "name": name,
            "command": command,
            "exit_code": return_code,
            "duration_seconds": round(time.monotonic() - started, 3),
            "log": log_path.name,
        }
        if test_count is not None:
            stage["test_count"] = test_count
        stages.append(stage)
        summary_path.write_text(json.dumps({"stages": stages}, indent=2) + "\n")
        state = "passed" if return_code == 0 else f"failed ({return_code})"
        print(
            f"[native-provider-checks] {state}: {name} ({stage['duration_seconds']}s)",
            flush=True,
        )

    run_stage(
        "architecture",
        [sys.executable, "scripts/native_provider_architecture.py"],
    )
    run_stage(
        "python-controls",
        [
            sys.executable,
            "-m",
            "unittest",
            "discover",
            "-s",
            "scripts/tests",
            "-p",
            "test_native_provider_*.py",
        ],
    )
    run_stage(
        "rust-format",
        [
            "cargo",
            "fmt",
            "--manifest-path",
            "codex-rs/Cargo.toml",
            "--all",
            "--",
            "--config",
            "imports_granularity=Item",
            "--check",
        ],
    )
    provider_packages = manifest["subsystem_packages"] + [manifest["adapter_package"]]
    clippy_command = [
        "cargo",
        "clippy",
        "--manifest-path",
        "codex-rs/Cargo.toml",
    ]
    for package in provider_packages:
        clippy_command.extend(["-p", package])
    clippy_command.extend(["--all-targets", "--", "-D", "warnings"])
    run_stage("provider-clippy", clippy_command)

    # Core integration tests launch the CLI exec-server and Code Mode host.
    # Build every helper before tests discover them on a fresh runner.
    run_stage(
        "cli-build",
        [
            "cargo",
            "build",
            "--locked",
            "--manifest-path",
            "codex-rs/Cargo.toml",
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
        ],
    )

    for group in manifest["focused_test_groups"]:
        command = ["cargo", "test", "--manifest-path", "codex-rs/Cargo.toml"]
        for package in group["packages"]:
            command.extend(["-p", package])
        if "test_target" in group:
            if group["test_target"] == "lib":
                command.append("--lib")
            elif group["test_target"].startswith("bin:"):
                command.extend(["--bin", group["test_target"].removeprefix("bin:")])
            else:
                command.extend(["--test", group["test_target"]])
        if "filter" in group:
            command.append(group["filter"])
        if "test_threads" in group:
            command.extend(["--", "--test-threads", str(group["test_threads"])])
        run_stage(group["name"], command, group.get("minimum_tests"))

    registry = "codex-provider-registry"
    for name, feature_args in (
        ("registry-metadata-only", ["--no-default-features"]),
        (
            "registry-runtime-only",
            ["--no-default-features", "--features", "runtime"],
        ),
    ):
        run_stage(
            name,
            [
                "cargo",
                "test",
                "--manifest-path",
                "codex-rs/Cargo.toml",
                "-p",
                registry,
                *feature_args,
                "--lib",
            ],
            minimum_tests=1,
        )

    run_stage(
        "nyacode-branding",
        [
            sys.executable,
            "-m",
            "unittest",
            "discover",
            "-s",
            "scripts/tests",
            "-p",
            "test_nyacode_branding.py",
        ],
        minimum_tests=7,
    )

    adapter = manifest["adapter_package"]
    feature_commands = [
        ("adapter-no-default-features", ["--no-default-features"]),
        *[
            (f"adapter-{feature}", ["--no-default-features", "--features", feature])
            for feature in manifest["single_provider_features"]
        ],
        ("adapter-all-features", ["--all-features"]),
    ]
    for name, feature_args in feature_commands:
        run_stage(
            name,
            [
                "cargo",
                "check",
                "--manifest-path",
                "codex-rs/Cargo.toml",
                "-p",
                adapter,
                *feature_args,
            ],
        )

    failed = [stage["name"] for stage in stages if stage["exit_code"] != 0]
    summary = {
        "result": "failed" if failed else "passed",
        "failed_stages": failed,
        **metadata,
        "stages": stages,
    }
    summary_path.write_text(json.dumps(summary, indent=2) + "\n")
    artifact_hashes = {
        path.name: hashlib.sha256(path.read_bytes()).hexdigest()
        for path in sorted(artifact_dir.glob("*.log"))
        + [summary_path, artifact_dir / "run-metadata.json"]
    }
    (artifact_dir / "artifact-sha256.json").write_text(
        json.dumps(artifact_hashes, indent=2) + "\n"
    )
    github_summary = env.get("GITHUB_STEP_SUMMARY")
    if github_summary:
        lines = [
            "## Native provider checks",
            "",
            f"- Result: **{summary['result']}**",
            f"- Checkout commit: {metadata['repository_commit'] or 'unavailable'}",
            f"- GitHub SHA: {metadata['github_sha'] or 'unavailable'}",
            "",
            "| Stage | Result | Duration (s) | Log |",
            "| --- | --- | ---: | --- |",
        ]
        for stage in stages:
            result = (
                "passed"
                if stage["exit_code"] == 0
                else f"failed ({stage['exit_code']})"
            )
            lines.append(
                f"| {stage['name']} | {result} | "
                f"{stage['duration_seconds']} | {stage['log']} |"
            )
        with Path(github_summary).open("a") as summary_file:
            summary_file.write("\n".join(lines) + "\n")
    return stages


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=ROOT)
    parser.add_argument("--artifact-dir", type=Path, required=True)
    args = parser.parse_args()
    stages = run_checks(args.repo.resolve(), args.artifact_dir.resolve())
    failures = [stage for stage in stages if stage["exit_code"] != 0]
    if failures:
        print(
            "native provider checks failed: "
            + ", ".join(stage["name"] for stage in failures),
            file=sys.stderr,
        )
        return 1
    print(f"native provider checks passed ({len(stages)} logged stages)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
