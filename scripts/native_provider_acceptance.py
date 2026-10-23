#!/usr/bin/env python3
"""Run bounded live native-provider checks against an existing CODEX_HOME.

Examples (use a dedicated CODEX_HOME with an already selected account):
  python scripts/native_provider_acceptance.py --codex "$PWD/.engine/target/debug/codex" --codex-home /path/to/codex-home --provider claude --check lifecycle
  python scripts/native_provider_acceptance.py --codex "$PWD/.engine/target/debug/codex" --codex-home /path/to/codex-home --provider opencode-go --check permissions

The script never logs in, changes accounts, prints protocol payloads, or dumps
provider logs. It creates only uniquely named temporary workspaces and files.
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import shlex
import subprocess
import tempfile
import threading
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

DEFAULT_MODELS = {
    "cursor": "gpt-5.6-luna-none",
    "opencode-go": "glm-5.3",
    "claude": "claude-haiku-5-5",
    "antigravity": "gemini-3.5-flash-lite",
}
DEFAULT_ALTERNATES = {
    "cursor": "gpt-5.6-luna-low",
    "opencode-go": "minimax-m3",
    "claude": "claude-sonnet-4-6",
    "antigravity": "claude-sonnet-4-6",
}


class AcceptanceError(Exception):
    """A sanitized check failure with no provider response attached."""


class AppServer:
    def __init__(self, codex: Path, codex_home: Path, provider: str) -> None:
        self.events: queue.Queue[dict[str, Any]] = queue.Queue()
        self.saved: list[dict[str, Any]] = []
        self.next_id = 0
        self.approval_decision = "decline"
        self.approvals: list[str] = []
        self.expected_approval_command: str | None = None
        self.expected_approval_cwd: Path | None = None
        env = os.environ.copy()
        env["CODEX_HOME"] = str(codex_home)
        self.process = subprocess.Popen(
            [str(codex), "app-server"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            encoding="utf-8",
            env=env,
        )
        assert self.process.stdout is not None

        def read_events() -> None:
            for line in self.process.stdout:
                try:
                    event = json.loads(line)
                except (json.JSONDecodeError, UnicodeDecodeError):
                    continue
                if isinstance(event, dict):
                    self.events.put(event)

        threading.Thread(target=read_events, daemon=True).start()
        self.provider = provider

    def _send(self, payload: dict[str, Any]) -> None:
        if self.process.stdin is None or self.process.poll() is not None:
            raise AcceptanceError("app-server exited")
        self.process.stdin.write(json.dumps(payload, separators=(",", ":")) + "\n")
        self.process.stdin.flush()

    def wait(
        self, predicate: Callable[[dict[str, Any]], bool], seconds: int = 180
    ) -> dict[str, Any]:
        deadline = time.monotonic() + seconds
        while True:
            for index, event in enumerate(self.saved):
                if predicate(event):
                    return self.saved.pop(index)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AcceptanceError("timed out waiting for app-server event")
            try:
                event = self.events.get(timeout=min(1.0, remaining))
            except queue.Empty:
                if self.process.poll() is not None:
                    raise AcceptanceError("app-server exited")
                continue
            if "id" in event and "method" in event:
                method = event["method"]
                if method == "item/commandExecution/requestApproval":
                    if not self._is_expected_approval(event.get("params", {})):
                        self._send(
                            {"id": event["id"], "result": {"decision": "decline"}}
                        )
                        raise AcceptanceError("unexpected command approval request")
                    self.approvals.append(self.approval_decision)
                    self._send(
                        {
                            "id": event["id"],
                            "result": {"decision": self.approval_decision},
                        }
                    )
                    self.expected_approval_command = None
                    self.expected_approval_cwd = None
                else:
                    self._send(
                        {
                            "id": event["id"],
                            "error": {
                                "code": -32601,
                                "message": "Not supported by acceptance harness",
                            },
                        }
                    )
                continue
            if predicate(event):
                return event
            self.saved.append(event)

    def _is_expected_approval(self, params: dict[str, Any]) -> bool:
        if self.expected_approval_command is None or self.expected_approval_cwd is None:
            return False
        try:
            command = shlex.split(params.get("command", ""))
        except (TypeError, ValueError):
            return False
        exact_command = (
            len(command) == 3
            and command[0] in {"/bin/bash", "/usr/bin/bash"}
            and command[1] in {"-lc", "-c"}
            and command[2] == self.expected_approval_command
        )
        amendment = params.get("proposedExecpolicyAmendment")
        exact_amendment = amendment is None or amendment in (
            ["/bin/bash", "-lc", self.expected_approval_command],
            ["/bin/bash", "-c", self.expected_approval_command],
            ["/usr/bin/bash", "-lc", self.expected_approval_command],
            ["/usr/bin/bash", "-c", self.expected_approval_command],
        )
        return (
            exact_command
            and exact_amendment
            and params.get("cwd") == str(self.expected_approval_cwd)
            and params.get("kind") == "command"
        )

    def call(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self.next_id += 1
        request_id = self.next_id
        self._send({"id": request_id, "method": method, "params": params})
        response = self.wait(lambda event: event.get("id") == request_id)
        if "error" in response:
            raise AcceptanceError("app-server request failed")
        result = response.get("result")
        if not isinstance(result, dict):
            raise AcceptanceError("invalid app-server response")
        return result

    def initialize(self) -> None:
        self.call(
            "initialize",
            {
                "clientInfo": {"name": "native-provider-acceptance", "version": "1"},
                "capabilities": {"experimentalApi": True},
            },
        )
        self._send({"method": "initialized"})

    def start_thread(
        self,
        provider: str,
        model: str,
        workspace: Path,
        approval: str,
        sandbox: str,
        approvals_reviewer: str | None = None,
    ) -> str:
        params: dict[str, Any] = {
            "modelProvider": provider,
            "model": model,
            "cwd": str(workspace),
            "approvalPolicy": approval,
            "sandbox": sandbox,
            "baseInstructions": "Follow the user's requested acceptance check exactly. Be concise.",
        }
        if approvals_reviewer is not None:
            params["approvalsReviewer"] = approvals_reviewer
        result = self.call(
            "thread/start",
            params,
        )
        thread = result.get("thread", {}).get("id")
        if not isinstance(thread, str):
            raise AcceptanceError("thread did not start")
        return thread

    def turn_start(self, thread: str, prompt: str, model: str | None = None) -> str:
        params: dict[str, Any] = {
            "threadId": thread,
            "input": [{"type": "text", "text": prompt, "text_elements": []}],
        }
        if model:
            params["model"] = model
        result = self.call("turn/start", params)
        turn = result.get("turn", {}).get("id")
        if not isinstance(turn, str):
            raise AcceptanceError("turn did not start")
        return turn

    def wait_turn_event(
        self, thread: str, turn: str, seconds: int = 180
    ) -> dict[str, Any]:
        return self.wait(
            lambda item: (
                item.get("method") == "turn/completed"
                and item.get("params", {}).get("turn", {}).get("id") == turn
            ),
            seconds,
        )

    def wait_turn(self, thread: str, turn: str, seconds: int = 180) -> str:
        event = self.wait_turn_event(thread, turn, seconds)
        return str(event.get("params", {}).get("turn", {}).get("status", "unknown"))

    def complete_turn(
        self, thread: str, prompt: str, seconds: int = 180, model: str | None = None
    ) -> None:
        turn = self.turn_start(thread, prompt, model)
        if self.wait_turn(thread, turn, seconds) != "completed":
            raise AcceptanceError("turn did not complete")

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)


def text_items(server: AppServer, thread: str, turn: str) -> list[str]:
    texts = []
    for event in server.saved:
        params = event.get("params", {})
        item = params.get("item", {})
        if (
            event.get("method") == "item/completed"
            and params.get("threadId") == thread
            and params.get("turnId") == turn
            and item.get("type") == "agentMessage"
            and isinstance(item.get("text"), str)
        ):
            texts.append(item["text"])
    return texts


def lifecycle(args: argparse.Namespace) -> None:
    workspace = Path(tempfile.mkdtemp(prefix="codex-native-lifecycle-"))
    marker = workspace / "acceptance.txt"
    server = AppServer(args.codex, args.codex_home, args.provider)
    try:
        server.initialize()
        thread = server.start_thread(
            args.provider, args.model, workspace, "never", "workspace-write"
        )
        server.complete_turn(
            thread,
            f'Use exec_command exactly once with cmd="printf ACCEPTED_NATIVE_TOOL > {marker}". Then reply READY.',
        )
        if marker.read_text(encoding="utf-8") != "ACCEPTED_NATIVE_TOOL":
            raise AcceptanceError("authorized host tool result mismatch")
        server.complete_turn(
            thread, "Remember the exact code word ORCHARD-725. Reply READY only."
        )
        fork_result = server.call("thread/fork", {"threadId": thread})
        fork = fork_result.get("thread", {}).get("id")
        if not isinstance(fork, str):
            raise AcceptanceError("thread fork failed")
        fork_turn = server.turn_start(
            fork,
            "What exact code word did I ask you to remember? Reply only that word.",
        )
        if server.wait_turn(fork, fork_turn) != "completed":
            raise AcceptanceError("fork turn did not complete")
        server.call("thread/compact/start", {"threadId": thread})
        server.wait(
            lambda event: (
                event.get("method") == "item/completed"
                and event.get("params", {}).get("threadId") == thread
                and event.get("params", {}).get("item", {}).get("type")
                == "contextCompaction"
            ),
            240,
        )
        server.wait(
            lambda event: (
                event.get("method") == "turn/completed"
                and event.get("params", {}).get("threadId") == thread
            ),
            120,
        )
        compacted_turn = server.turn_start(
            thread,
            "What exact code word did I ask you to remember? Reply only that word.",
        )
        if server.wait_turn(thread, compacted_turn) != "completed":
            raise AcceptanceError("post-compaction turn did not complete")
        if not any(
            "ORCHARD-725" in text for text in text_items(server, fork, fork_turn)
        ):
            raise AcceptanceError("fork did not preserve remembered content")
        if not any(
            "ORCHARD-725" in text for text in text_items(server, thread, compacted_turn)
        ):
            raise AcceptanceError("compaction did not preserve remembered content")
        print(f"PASS provider={args.provider} thread={thread} check=lifecycle")
    finally:
        server.close()


def interruption(args: argparse.Namespace) -> None:
    workspace = Path(tempfile.mkdtemp(prefix="codex-native-interruption-"))
    started = workspace / "started.txt"
    completed = workspace / "completed.txt"
    server = AppServer(args.codex, args.codex_home, args.provider)
    thread = ""
    try:
        server.initialize()
        thread = server.start_thread(
            args.provider, args.model, workspace, "never", "workspace-write"
        )
        turn = server.turn_start(
            thread,
            f'Call exec_command with cmd="printf STARTED > {started}; sleep 45; printf COMPLETED > {completed}", yield_time_ms=1000. Wait for completion with write_stdin if needed. Do not modify the command.',
        )
        deadline = time.monotonic() + 120
        while not started.exists() and time.monotonic() < deadline:
            if server.process.poll() is not None:
                raise AcceptanceError("app-server exited before tool started")
            time.sleep(0.1)
        if not started.exists():
            raise AcceptanceError(
                "host tool never started; interruption was not tested"
            )
        server.call("turn/interrupt", {"threadId": thread, "turnId": turn})
        if server.wait_turn(thread, turn, 30) != "interrupted":
            raise AcceptanceError("active turn was not interrupted")
        if completed.exists():
            raise AcceptanceError("interrupted host process completed")
        print(
            f"PASS provider={args.provider} thread={thread} check=active-tool-interrupted"
        )
        server.close()
        server = AppServer(args.codex, args.codex_home, args.provider)
        server.initialize()
        server.call("thread/resume", {"threadId": thread})
        turn = server.turn_start(
            thread, "Reply READY without tools.", args.alternate_model
        )
        event = server.wait_turn_event(thread, turn, 90)
        status = str(event.get("params", {}).get("turn", {}).get("status", "unknown"))
        if status != "failed":
            raise AcceptanceError(
                "interrupted operation was not fenced after restart/model change"
            )
        error = event.get("params", {}).get("turn", {}).get("error", {})
        message = error.get("message", "") if isinstance(error, dict) else ""
        if "explicitly branch" not in message:
            raise AcceptanceError("failed turn was not the interrupted-operation fence")
        fork = (
            server.call("thread/fork", {"threadId": thread}).get("thread", {}).get("id")
        )
        if not isinstance(fork, str):
            raise AcceptanceError("explicit recovery branch failed")
        branch_turn = server.turn_start(
            fork, "Do not run or retry any command. Reply BRANCH_RECOVERED only."
        )
        if server.wait_turn(fork, branch_turn) != "completed":
            raise AcceptanceError("explicit recovery branch did not complete")
        if not any(
            "BRANCH_RECOVERED" in text for text in text_items(server, fork, branch_turn)
        ):
            raise AcceptanceError("explicit recovery branch did not continue")
        if completed.exists():
            raise AcceptanceError("cancelled host process completed after restart")
        print(
            f"PASS provider={args.provider} thread={thread} check=restart-fence-and-branch"
        )
    finally:
        server.close()


def permissions(args: argparse.Namespace) -> None:
    workspace = Path(tempfile.mkdtemp(prefix="codex-native-permissions-"))
    target = workspace / "approved.txt"
    server = AppServer(args.codex, args.codex_home, args.provider)
    try:
        server.initialize()
        thread = server.start_thread(
            args.provider,
            args.model,
            workspace,
            "on-request",
            "read-only",
            approvals_reviewer="user",
        )

        def prompt(marker: str) -> str:
            return (
                f'Call exec_command exactly once with cmd="printf {marker} > {target}" '
                'and sandbox_permissions="require_escalated", '
                'justification="Run the authorized temporary-file acceptance check?". '
                "If denied, do not retry or use any alternative tool; reply DENIED. "
                "If allowed, reply ALLOWED."
            )

        server.approval_decision = "decline"
        server.expected_approval_command = f"printf DENIED_WRITE > {target}"
        server.expected_approval_cwd = workspace
        server.complete_turn(thread, prompt("DENIED_WRITE"))
        if not server.approvals or any(
            decision != "decline" for decision in server.approvals
        ):
            raise AcceptanceError("decline approval RPC was not exercised")
        if target.exists():
            raise AcceptanceError("declined command modified the temporary workspace")
        print(f"PASS provider={args.provider} thread={thread} check=approval-decline")

        before = len(server.approvals)
        server.approval_decision = "accept"
        server.expected_approval_command = f"printf ACCEPTED_WRITE > {target}"
        server.expected_approval_cwd = workspace
        server.complete_turn(thread, prompt("ACCEPTED_WRITE"))
        if len(server.approvals) <= before:
            raise AcceptanceError("accept approval RPC was not exercised")
        if target.read_text(encoding="utf-8") != "ACCEPTED_WRITE":
            raise AcceptanceError("approved command result mismatch")
        print(f"PASS provider={args.provider} thread={thread} check=approval-accept")
    finally:
        server.close()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run bounded live native-provider checks against an existing CODEX_HOME.",
        epilog=(
            "Example: python scripts/native_provider_acceptance.py --codex "
            "$(pwd)/.engine/target/debug/codex --codex-home /path/to/codex-home "
            "--provider claude --check lifecycle\n"
            "The selected account is used as-is; this script never logs in, "
            "changes accounts, or prints protocol/provider logs."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--codex",
        type=Path,
        required=True,
        help="absolute path to the codex executable",
    )
    parser.add_argument(
        "--codex-home",
        type=Path,
        required=True,
        help="existing CODEX_HOME with a selected provider account",
    )
    parser.add_argument("--provider", choices=DEFAULT_MODELS, required=True)
    parser.add_argument(
        "--model", help="provider model; defaults to a known text-capable test model"
    )
    parser.add_argument(
        "--alternate-model", help="alternate model for interruption fence check"
    )
    parser.add_argument(
        "--check", choices=("lifecycle", "interruption", "permissions"), required=True
    )
    args = parser.parse_args()
    if not args.codex.is_absolute() or not args.codex.is_file():
        parser.error("--codex must name an existing absolute executable path")
    if not args.codex_home.is_absolute() or not args.codex_home.is_dir():
        parser.error("--codex-home must name an existing absolute directory")
    args.model = args.model or DEFAULT_MODELS[args.provider]
    args.alternate_model = args.alternate_model or DEFAULT_ALTERNATES[args.provider]
    return args


def main() -> int:
    args = parse_args()
    checks = {
        "lifecycle": lifecycle,
        "interruption": interruption,
        "permissions": permissions,
    }
    try:
        checks[args.check](args)
    except AcceptanceError as error:
        print(f"FAIL provider={args.provider} check={args.check} error={error}")
        return 1
    except Exception as error:  # noqa: BLE001
        # Do not print unexpected exception payloads.
        print(
            f"FAIL provider={args.provider} check={args.check} error={type(error).__name__}"
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
