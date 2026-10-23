#!/usr/bin/env python3
"""Build V8 with bounded retries for transient Git repository fetch failures."""

import re
import subprocess
import sys
import time
from collections.abc import Callable
from typing import BinaryIO

from run_bazel_with_buildbuddy import bazel_command

TAIL_BYTES = 256 * 1024
MAX_ATTEMPTS = 3
TRANSIENT_GIT_HTTP_ERROR = re.compile(
    r"fatal: unable to access 'https://[^'\r\n]+': "
    r"The requested URL returned error: (?:500|502|503|504)(?:\s|$)"
)


def retryable_fetch_failure(status: int, output: str) -> bool:
    return (
        status == 1
        and "Analysis failed" in output
        and "error running 'git fetch " in output
        and "while working with @" in output
        and TRANSIENT_GIT_HTTP_ERROR.search(output) is not None
    )


def run_once(command: list[str], output: BinaryIO) -> tuple[int, str]:
    tail = bytearray()
    with subprocess.Popen(
        command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT
    ) as process:
        assert process.stdout is not None
        try:
            while chunk := process.stdout.read1(64 * 1024):
                output.write(chunk)
                output.flush()
                tail.extend(chunk)
                del tail[:-TAIL_BYTES]
            status = process.wait()
        except BaseException:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            raise
    return status, tail.decode("utf-8", errors="replace")


def run_build(
    command: list[str],
    attempt: Callable[[list[str]], tuple[int, str]],
    pause: Callable[[float], None] = time.sleep,
) -> int:
    for number in range(1, MAX_ATTEMPTS + 1):
        status, output = attempt(command)
        if number == MAX_ATTEMPTS or not retryable_fetch_failure(status, output):
            return status
        delay = 10 * number
        print(
            "::warning::Transient upstream Git fetch failure; "
            f"retrying V8 build ({number + 1}/{MAX_ATTEMPTS}) in {delay}s.",
            flush=True,
        )
        pause(delay)
    raise AssertionError("unreachable retry state")


def main() -> int:
    args = sys.argv[1:]
    command_name = next((arg for arg in args if not arg.startswith("-")), None)
    if command_name != "build":
        raise SystemExit("run_v8_bazel_build.py requires a Bazel build command")
    return run_build(
        bazel_command(*args), lambda command: run_once(command, sys.stdout.buffer)
    )


if __name__ == "__main__":
    raise SystemExit(main())
