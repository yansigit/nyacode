"""Rust toolchain pins come from engine.lock; inherited upstream files stay inert."""

import json
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TOOLCHAIN = json.loads((ROOT / "engine.lock").read_text())["toolchain"]
TOOLCHAIN_LINE = re.compile(
    r"toolchain|rust-version|RUST_VERSION|rustup", re.IGNORECASE
)
VERSION = re.compile(r"\b1\.\d{2,3}\.\d+\b")
REPO_GUARD = "if: github.repository == 'openai/codex'"

# Inherited upstream files that hardcode a Rust version and are inert in this fork.
ALLOWLIST = {
    ".github/workflows/rust-release.yml": "tag-only trigger (rust-v*); every job needs tag-check, which runs only for openai/codex",
    ".github/workflows/rust-release-windows.yml": "reusable only; its sole caller, rust-release.yml build-windows, needs tag-check",
    ".github/workflows/rust-ci-full-nextest-platform.yml": "reusable only; no workflow in this fork calls it",
    ".github/workflows/rusty-v8-release.yml": "tag-only trigger (rusty-v8-v*); its 1.91.0 is the rusty_v8 build toolchain, not the engine pin",
    ".github/workflows/v8-canary.yml": "reached from postmerge-ci only for openai/codex; its metadata job skips elsewhere unless manually dispatched",
    ".github/actions/build-bazel-package-binaries/action.yml": "composite action whose only consumer, build-codex-packages, is unused by any workflow",
}
# Expected trigger keys and, when present, the job guard that keeps each file inert.
GUARDS = {
    ".github/workflows/rust-release.yml": ({"push"}, REPO_GUARD),
    ".github/workflows/rust-release-windows.yml": ({"workflow_call"}, None),
    ".github/workflows/rust-ci-full-nextest-platform.yml": ({"workflow_call"}, None),
    ".github/workflows/rusty-v8-release.yml": ({"push"}, None),
    ".github/workflows/v8-canary.yml": (
        {"workflow_call", "pull_request", "workflow_dispatch"},
        "if: github.repository == 'openai/codex' || github.event_name == 'workflow_dispatch'",
    ),
}
ACTION = ".github/actions/build-bazel-package-binaries/action.yml"


def trigger_keys(text: str) -> set[str]:
    block = text.split("\non:\n", 1)[1].split("\n\n", 1)[0]
    return set(re.findall(r"^  ([a-z_]+):", block, re.MULTILINE))


class ToolchainTest(unittest.TestCase):
    def test_container_image_default_matches_engine_lock(self):
        text = (ROOT / "Containerfile").read_text()
        self.assertEqual(
            re.findall(r"^ARG RUST_VERSION=(\S+)$", text, re.MULTILINE), [TOOLCHAIN]
        )

    def test_hardcoded_rust_versions_are_limited_to_allowlisted_inert_files(self):
        found = set()
        for path in [
            *(ROOT / ".github/workflows").glob("*.yml"),
            *(ROOT / ".github/actions").rglob("*.yml"),
        ]:
            lines = [
                line
                for line in path.read_text().splitlines()
                if TOOLCHAIN_LINE.search(line) and VERSION.search(line)
            ]
            if lines:
                found.add(path.relative_to(ROOT).as_posix())
        self.assertEqual(sorted(found - ALLOWLIST.keys()), [])
        self.assertEqual(sorted(ALLOWLIST.keys() - found), [])

    def test_allowlisted_files_are_still_guarded(self):
        self.assertEqual(set(ALLOWLIST), set(GUARDS) | {ACTION})
        for path, (events, guard) in GUARDS.items():
            text = (ROOT / path).read_text()
            with self.subTest(path=path):
                self.assertEqual(trigger_keys(text), events)
                if guard:
                    self.assertIn(guard, text)
        self.assertIn("using: composite", (ROOT / ACTION).read_text())


if __name__ == "__main__":
    unittest.main()
