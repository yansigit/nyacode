"""AI conflict resolution stays inside its grant, checks every edit, and never receives GitHub credentials."""

import contextlib
import io
import json
import os
import re
import shutil
import sys
import tempfile
import textwrap
import time
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import engine
import engine_patches
import engine_resolve as resolver

ROOT = Path(__file__).resolve().parents[2]
PATCH_PATH = "integration/codex/patches/backend_dispatch.patch"
CONFLICTED = "codex-rs/lib.rs"
CLEAN = "README.md"
UPSTREAM = b"fn main() {\n    upstream();\n}\n"
MERGED = (
    b"fn main() {\n<<<<<<< ours\n    patched();\n=======\n    upstream();\n"
    b">>>>>>> theirs\n}\n"
)
RESOLVED = b"fn main() {\n    patched_upstream();\n}\n"
WRONG = b"fn main() {\n    wrong();\n}\n"
# LC_CTYPE is added by the Python interpreter itself (locale coercion), not by the resolver.
ALLOWED_ENV = {
    "PATH",
    "HOME",
    "LC_CTYPE",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "CURSOR_API_KEY",
    "OPENCODE_API_KEY",
}


def make_rebase(root: Path, *, status: str = "conflict", blocks: int = 1) -> Path:
    """A rebase output: pristine branch holds upstream; the working tree holds the merged result."""
    directory = root / "sync"
    workspace = directory / "workspace"
    workspace.mkdir(parents=True)
    engine.git(workspace, "init", "--quiet")
    engine.git(
        workspace, "symbolic-ref", "HEAD", "refs/heads/" + engine_patches.PRISTINE
    )
    (workspace / CONFLICTED).parent.mkdir(parents=True)
    (workspace / CONFLICTED).write_bytes(UPSTREAM)
    (workspace / CLEAN).write_bytes(b"readme\n")
    engine.git(workspace, "add", "--all", "--", ".")
    engine.git(
        workspace, *engine_patches.IDENTITY, "commit", "--quiet", "-m", "pristine"
    )
    (workspace / CONFLICTED).write_bytes(MERGED)
    report = {
        "schema_version": 1,
        "generated_by": engine_patches.REBASE_GENERATOR,
        "from": {
            "revision": "a" * 40,
            "archive_sha256": "0" * 64,
            "release_tag": None,
            "toolchain": "stable",
        },
        "onto": {
            "revision": "b" * 40,
            "archive_sha256": "1" * 64,
            "toolchain": "stable",
            "tag": None,
            "source": "local-ref",
            "ref": "upstream/main",
        },
        "patches": [
            {
                "name": "backend_dispatch",
                "path": PATCH_PATH,
                "status": "conflict",
                "files": [
                    {"path": CONFLICTED, "status": status, "conflict_blocks": blocks},
                    {"path": CLEAN, "status": "clean", "conflict_blocks": 0},
                ],
            }
        ],
        "totals": {
            "clean_patches": 0,
            "conflicted_patches": 1,
            "conflicted_files": 1,
            "conflict_blocks": blocks,
        },
    }
    engine.write_json(directory / engine_patches.REPORT_NAME, report)
    return directory


def write_fake(root: Path, name: str, log: Path, action: str) -> Path:
    """A Python stand-in for an agent CLI. It logs argv, cwd, env and config, then runs `action`."""
    header = textwrap.dedent(
        f"""\
        #!{sys.executable} -I
        import json, os, sys, time
        record = {{
            "argv": sys.argv[1:],
            "cwd": os.getcwd(),
            "env": dict(os.environ),
            "config": {{
                name: open(name).read()
                for name in ("opencode.json", ".cursor/cli.json")
                if os.path.exists(name)
            }},
        }}
        with open({str(log)!r}, "a") as stream:
            stream.write(json.dumps(record) + "\\n")
        prompt = max(sys.argv, key=len)
        """
    )
    path = root / name
    path.write_text(header + textwrap.dedent(action))
    path.chmod(0o755)
    return path


def logged_calls(log: Path) -> list[dict]:
    if not log.exists():
        return []
    return [json.loads(line) for line in log.read_text().splitlines()]


def resolve_action(content: bytes = RESOLVED) -> str:
    return f"open({CONFLICTED!r}, 'w').write({content.decode()!r})\n"


def attempt_action(counter: Path, *attempts: str) -> str:
    """Run the Nth attempt's action; attempts past the list reuse the last one."""
    return textwrap.dedent(
        f"""\
        import pathlib
        counter = pathlib.Path({str(counter)!r})
        number = int(counter.read_text()) + 1 if counter.exists() else 1
        counter.write_text(str(number))
        actions = {list(attempts)!r}
        exec(actions[min(number, len(actions)) - 1])
        """
    )


def leftover(original: bytes, current: bytes) -> list[str]:
    return resolver.leftover_markers(original, current)


class ResolveTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.directory = make_rebase(self.root)
        self.workspace = self.directory / "workspace"
        self.out = self.root / "out" / "resolution.patch"
        self.log = self.root / "agent.log"
        self.env = patch.dict(
            os.environ,
            {
                "GITHUB_TOKEN": "ghs_github_token_value",
                "GH_TOKEN": "ghs_github_token_value",
                "CURSOR_API_KEY": "cursor-test-key",
                "OPENCODE_API_KEY": "opencode-test-key",
            },
        )
        self.env.start()
        self.addCleanup(self.env.stop)
        self.record = patch.object(resolver, "record")
        self.recorded = self.record.start()
        self.addCleanup(self.record.stop)
        self.agent_root = self.root / "bin"
        self.agent_root.mkdir()

    def fake(self, action: str, agent: str = "cursor") -> Path:
        name = "fake-" + agent
        script = write_fake(self.agent_root, name, self.log, action)
        env_name = "NYACODE_CURSOR_BIN" if agent == "cursor" else "NYACODE_OPENCODE_BIN"
        os.environ[env_name] = str(script)
        return script

    def run_main(
        self, *extra: str, agent: str = "cursor", model: str = "gpt-test-model"
    ) -> int:
        argv = [
            "--dir",
            str(self.directory),
            "--agent",
            agent,
            "--model",
            model,
            "--out",
            str(self.out),
            *extra,
        ]
        with (
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            return resolver.main(argv)

    def result(self) -> dict:
        return json.loads((self.out.parent / "resolution.json").read_text())

    def test_success_writes_patch_that_applies_to_the_rebase_output(self):
        self.fake(resolve_action())
        before = self.root / "before"
        shutil.copytree(self.workspace, before)
        self.assertEqual(self.run_main(), resolver.EXIT_RESOLVED)
        self.assertEqual(self.result()["status"], "resolved")
        self.assertEqual(self.result()["iterations"], 1)
        engine.git(before, "apply", str(self.out))
        self.assertEqual((before / CONFLICTED).read_bytes(), RESOLVED)
        self.assertNotIn(b"README.md", self.out.read_bytes())
        self.assertEqual(self.recorded.call_args.args[0], "resolved")

    def test_edit_outside_the_listed_files_is_reverted_and_retried(self):
        self.fake(
            attempt_action(
                self.root / "counter",
                f"open({CLEAN!r}, 'w').write('changed\\n')\n" + resolve_action(),
                resolve_action(),
            )
        )
        self.assertEqual(self.run_main(), resolver.EXIT_RESOLVED)
        result = self.result()
        self.assertEqual(result["iterations"], 2)
        self.assertIn(CLEAN, " ".join(result["attempts"][0]["failures"]))
        self.assertEqual((self.workspace / CLEAN).read_bytes(), b"readme\n")
        self.assertNotIn(b"README.md", self.out.read_bytes())
        calls = logged_calls(self.log)
        self.assertTrue(any("Attempt 2." in arg for arg in calls[1]["argv"]))
        self.assertTrue(
            any("previous attempt was rejected" in arg for arg in calls[1]["argv"])
        )

    def test_leftover_markers_retry_then_fail_with_exit_4(self):
        self.fake("pass\n")
        self.assertEqual(self.run_main("--iterations", "2"), resolver.EXIT_FAILED)
        self.assertEqual(len(logged_calls(self.log)), 2)
        self.assertFalse(self.out.exists())
        self.assertEqual(self.result()["status"], "failed")
        self.assertIn(
            "conflict markers remain",
            " ".join(self.result()["attempts"][-1]["failures"]),
        )
        self.assertEqual(self.recorded.call_args.args[0], "failed")

    def test_timeout_kills_the_agent_and_exits_4_without_retry(self):
        self.fake("time.sleep(60)\n")
        started = time.monotonic()
        self.assertEqual(self.run_main("--timeout", "1"), resolver.EXIT_FAILED)
        self.assertLess(time.monotonic() - started, 30)
        self.assertEqual(len(logged_calls(self.log)), 1)
        self.assertEqual(self.result()["status"], "timeout")
        self.assertFalse(self.out.exists())

    def test_too_many_blocks_exit_3_without_invoking_the_agent(self):
        self.fake(resolve_action())
        self.assertEqual(self.run_main("--max-blocks", "0"), resolver.EXIT_TOO_LARGE)
        self.assertEqual(logged_calls(self.log), [])
        self.assertEqual(self.result()["status"], "too-large")
        self.assertFalse(self.out.exists())

    def test_modify_delete_conflicts_need_a_human(self):
        report_path = self.directory / engine_patches.REPORT_NAME
        report = json.loads(report_path.read_text())
        report["patches"][0]["files"][0]["status"] = "deleted_upstream"
        report_path.write_text(json.dumps(report))
        self.fake(resolve_action())
        self.assertEqual(self.run_main(), resolver.EXIT_TOO_LARGE)
        self.assertEqual(logged_calls(self.log), [])

    def test_claude_and_anthropic_models_are_refused(self):
        self.fake(resolve_action())
        for model in ("claude-sonnet-4", "anthropic/some-model", "openai/opus"):
            with self.subTest(model=model):
                self.assertEqual(self.run_main(model=model), resolver.EXIT_ERROR)
        self.assertEqual(logged_calls(self.log), [])

    def test_agent_environment_has_no_github_credentials(self):
        self.fake(resolve_action())
        self.assertEqual(self.run_main(), resolver.EXIT_RESOLVED)
        env = logged_calls(self.log)[0]["env"]
        self.assertNotIn("GITHUB_TOKEN", env)
        self.assertNotIn("GH_TOKEN", env)
        self.assertNotIn("OPENCODE_API_KEY", env)
        self.assertEqual(env["CURSOR_API_KEY"], "cursor-test-key")
        self.assertLessEqual(set(env), ALLOWED_ENV)

    def test_opencode_environment_passes_only_its_own_key(self):
        self.fake(resolve_action(), agent="opencode")
        self.assertEqual(self.run_main(agent="opencode"), resolver.EXIT_RESOLVED)
        env = logged_calls(self.log)[0]["env"]
        self.assertEqual(env["OPENCODE_API_KEY"], "opencode-test-key")
        self.assertNotIn("CURSOR_API_KEY", env)
        self.assertNotIn("GITHUB_TOKEN", env)

    def test_missing_agent_key_fails_before_the_agent_runs(self):
        self.fake(resolve_action())
        os.environ.pop("CURSOR_API_KEY")
        self.assertEqual(self.run_main(), resolver.EXIT_ERROR)
        self.assertEqual(logged_calls(self.log), [])

    def test_agent_key_is_redacted_from_stored_failure_output(self):
        self.fake(
            "import sys\n"
            "sys.stderr.write('auth failed for ' + os.environ['CURSOR_API_KEY'])\n"
            "sys.exit(2)\n"
        )
        self.assertEqual(self.run_main("--iterations", "1"), resolver.EXIT_FAILED)
        stored = (self.out.parent / "resolution.json").read_text()
        self.assertNotIn("cursor-test-key", stored)
        self.assertIn("[redacted]", stored)

    def test_config_files_are_written_for_the_agent_and_removed_afterwards(self):
        for agent in ("cursor", "opencode"):
            with self.subTest(agent=agent):
                self.log.unlink(missing_ok=True)
                self.directory = make_rebase(Path(tempfile.mkdtemp(dir=self.root)))
                self.workspace = self.directory / "workspace"
                self.fake(resolve_action(), agent=agent)
                self.assertEqual(self.run_main(agent=agent), resolver.EXIT_RESOLVED)
                self.assertFalse((self.workspace / ".cursor").exists())
                self.assertFalse((self.workspace / "opencode.json").exists())
                self.assertFalse((self.workspace / ".cursor" / "cli.json").exists())
                seen = logged_calls(self.log)[0]["config"]
                self.assertEqual(len(seen), 1)

    def test_cursor_config_grants_only_the_conflicted_files(self):
        self.fake(resolve_action())
        self.assertEqual(self.run_main(), resolver.EXIT_RESOLVED)
        config = json.loads(logged_calls(self.log)[0]["config"][".cursor/cli.json"])
        allow = config["permissions"]["allow"]
        deny = config["permissions"]["deny"]
        self.assertIn(f"Write({CONFLICTED})", allow)
        self.assertEqual(
            [item for item in allow if item.startswith("Write(")],
            [f"Write({CONFLICTED})"],
        )
        for entry in (
            "Shell(git)",
            "Shell(curl)",
            "Shell(wget)",
            "WebFetch(*)",
            "Mcp(*)",
            "Write(.cursor/**)",
            "Write(.github/**)",
            "Write(opencode.json)",
        ):
            self.assertIn(entry, deny)

    def test_opencode_config_has_no_ask_values(self):
        self.fake(resolve_action(), agent="opencode")
        self.assertEqual(self.run_main(agent="opencode"), resolver.EXIT_RESOLVED)
        config = json.loads(logged_calls(self.log)[0]["config"]["opencode.json"])
        assert_no_ask(self, config)
        self.assertEqual(config["permission"]["edit"][CONFLICTED], "allow")
        assert_no_ask(
            self,
            json.loads((ROOT / "integration/codex/agent/opencode.json").read_text()),
        )

    def test_validation_failure_is_fed_back_and_retried(self):
        check = self.root / "check.py"
        seen_env = self.root / "validator-env.json"
        check.write_text(
            "import json, os, sys\n"
            f"json.dump(dict(os.environ), open({str(seen_env)!r}, 'w'))\n"
            "sys.exit(0 if 'patched_upstream' in open('codex-rs/lib.rs').read() else 1)\n"
        )
        self.fake(
            attempt_action(
                self.root / "counter", resolve_action(WRONG), resolve_action()
            )
        )
        validate = f"{sys.executable} -I {check}"
        self.assertEqual(
            self.run_main("--validate-cmd", validate), resolver.EXIT_RESOLVED
        )
        result = self.result()
        self.assertEqual(result["iterations"], 2)
        self.assertIn("validation failed", " ".join(result["attempts"][0]["failures"]))
        # The validator runs without any agent key or GitHub token, and its build output stays out of the tree.
        validator_env = json.loads(seen_env.read_text())
        self.assertNotIn("CURSOR_API_KEY", validator_env)
        self.assertNotIn("GITHUB_TOKEN", validator_env)
        self.assertEqual(
            validator_env["CARGO_TARGET_DIR"],
            str(self.directory / resolver.RESOLVER_DIR / "cargo-target"),
        )

    def test_prompt_wraps_file_content_as_untrusted_data(self):
        conflicts, deleted = resolver.collect(
            json.loads((self.directory / engine_patches.REPORT_NAME).read_text())
        )
        self.assertEqual([item.path for item in conflicts], [CONFLICTED])
        self.assertEqual(deleted, [])
        prompt = resolver.build_prompt(
            nonce="abc123",
            iteration=1,
            conflicts=conflicts,
            ownership={
                PATCH_PATH: {"owner": "codex-rs", "purpose": "backend_dispatch"}
            },
            workspace=self.workspace,
            history={CONFLICTED: "(not available)"},
            feedback=None,
        )
        self.assertIn("Do not run git commands. Do not access the network.", prompt)
        self.assertIn("Owning patch: backend_dispatch (owner: codex-rs)", prompt)
        self.assertIn("BEGIN UNTRUSTED DATA abc123", prompt)
        self.assertIn("END UNTRUSTED DATA abc123", prompt)
        self.assertIn("<<<<<<< ours", prompt)

    def test_no_conflicts_writes_an_empty_patch_without_the_agent(self):
        report_path = self.directory / engine_patches.REPORT_NAME
        report = json.loads(report_path.read_text())
        report["patches"][0]["files"][0]["status"] = "clean"
        report["patches"][0]["files"][0]["conflict_blocks"] = 0
        report_path.write_text(json.dumps(report))
        self.fake(resolve_action())
        self.assertEqual(self.run_main(), resolver.EXIT_RESOLVED)
        self.assertEqual(self.out.read_bytes(), b"")
        self.assertEqual(logged_calls(self.log), [])


class HelperTest(unittest.TestCase):
    def test_markers_legitimately_present_outside_blocks_are_not_leftovers(self):
        original = b"Title\n=======\n" + MERGED
        self.assertEqual(leftover(original, RESOLVED), [])
        self.assertEqual(leftover(original, b"Title\n=======\n" + RESOLVED), [])
        self.assertEqual(leftover(original, MERGED), ["<<<<<<<", ">>>>>>>"])
        self.assertEqual(
            leftover(original, b"Title\n=======\n" + MERGED),
            ["<<<<<<<", "=======", ">>>>>>>"],
        )

    def test_upstream_history_comes_from_the_rebase_range(self):
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)
            engine.git(repo, "init", "--quiet")
            (repo / CONFLICTED).parent.mkdir(parents=True)
            (repo / CONFLICTED).write_bytes(UPSTREAM)
            engine.git(repo, "add", "--all", "--", ".")
            engine.git(
                repo, *engine_patches.IDENTITY, "commit", "--quiet", "-m", "first"
            )
            start = engine.git(repo, "rev-parse", "HEAD").decode().strip()
            (repo / CONFLICTED).write_bytes(RESOLVED)
            engine.git(
                repo,
                *engine_patches.IDENTITY,
                "commit",
                "--quiet",
                "-am",
                "upstream change",
            )
            end = engine.git(repo, "rev-parse", "HEAD").decode().strip()
            history = resolver.upstream_history(repo, start, end, CONFLICTED)
            self.assertIn("upstream change", history)
            self.assertNotIn("first", history)
            self.assertIn(
                "not available", resolver.upstream_history(None, start, end, CONFLICTED)
            )

    def test_agent_templates_are_trusted_and_declare_the_required_controls(self):
        cursor = json.loads(
            (ROOT / "integration/codex/agent/cursor-cli.json").read_text()
        )
        self.assertIn("Read(**)", cursor["permissions"]["allow"])
        self.assertFalse(
            any(rule.startswith("Shell(") for rule in cursor["permissions"]["allow"])
        )
        self.assertIn("Shell(*)", cursor["permissions"]["deny"])
        opencode = json.loads(
            (ROOT / "integration/codex/agent/opencode.json").read_text()
        )
        self.assertEqual(opencode["permission"]["bash"]["*"], "deny")
        self.assertEqual(opencode["permission"]["bash"], {"*": "deny"})
        self.assertEqual(opencode["permission"]["webfetch"], "deny")
        self.assertEqual(opencode["permission"]["external_directory"], "deny")

    def test_installer_pin_is_placeholder_until_reviewed(self):
        self.assertEqual(
            (ROOT / "integration/codex/agent/cursor-install.sha256")
            .read_text()
            .strip(),
            "UNPINNED",
        )

    def test_resolve_workflow_pins_actions_and_never_names_claude(self):
        text = (ROOT / ".github/workflows/engine-resolve.yml").read_text()
        for line in text.splitlines():
            if "uses:" in line:
                self.assertRegex(line, r"@[0-9a-f]{40}\b", msg=line)
        # Descriptions may say that Claude is refused; no key, model id or CLI may be wired in.
        self.assertNotRegex(text, r"ANTHROPIC|claude-|claude_|anthropic/")
        self.assertEqual(
            re.findall(r"secrets\.(\w+)", text).count("OPENCODE_API_KEY"), 1
        )
        self.assertEqual(re.findall(r"secrets\.(\w+)", text).count("CURSOR_API_KEY"), 1)


def assert_no_ask(test: unittest.TestCase, value) -> None:
    if isinstance(value, dict):
        for item in value.values():
            assert_no_ask(test, item)
    elif isinstance(value, list):
        for item in value:
            assert_no_ask(test, item)
    else:
        test.assertNotEqual(value, "ask")


if __name__ == "__main__":
    unittest.main()
