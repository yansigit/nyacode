import importlib.util
import json
import os
from pathlib import Path
import re
import runpy
import subprocess
import sys
import tempfile
import types
import unittest
from unittest.mock import Mock, patch


ROOT = Path(__file__).resolve().parents[2]


def load_python_harness():
    openai_codex = types.ModuleType("openai_codex")
    openai_codex.CodexConfig = lambda **kwargs: kwargs
    spec = importlib.util.spec_from_file_location(
        "nyacode_app_server_harness", ROOT / "sdk/python/tests/app_server_harness.py"
    )
    module = importlib.util.module_from_spec(spec)
    with patch.dict(sys.modules, {"openai_codex": openai_codex, spec.name: module}):
        spec.loader.exec_module(module)
        return module


class EnginePathTests(unittest.TestCase):
    def test_proxy_npm_sources_come_from_prepared_engine(self):
        spec = importlib.util.spec_from_file_location(
            "engine_proxy_npm", ROOT / "codex-cli/scripts/build_npm_package.py"
        )
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as temporary:
            workspace = Path(temporary) / "engine"
            proxy = workspace / "codex-rs/responses-api-proxy/npm"
            (proxy / "bin").mkdir(parents=True)
            (proxy / "bin/codex-responses-api-proxy.js").write_text("prepared launcher")
            (proxy / "package.json").write_text(
                json.dumps({"name": "codex-responses-api-proxy"})
            )
            output = Path(temporary) / "package"
            output.mkdir()
            engine = types.ModuleType("engine")
            engine.prepare = Mock(return_value=workspace)
            with (
                patch.dict(sys.modules, {"engine": engine}),
                patch.object(sys, "path", sys.path.copy()),
            ):
                module.stage_sources(output, "1.2.3", "codex-responses-api-proxy")
            engine.prepare.assert_called_once_with(root=ROOT)
            self.assertEqual(
                (output / "bin/codex-responses-api-proxy.js").read_text(),
                "prepared launcher",
            )
            self.assertEqual(
                json.loads((output / "package.json").read_text())["version"], "1.2.3"
            )

    def test_public_commands_are_available(self):
        result = subprocess.run(
            [sys.executable, str(ROOT / "scripts/build_nyacode.py"), "--help"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        )
        self.assertIn("--engine", result.stdout)
        justfile = (ROOT / "justfile").read_text()
        for recipe in (
            "prepare-engine",
            "test-providers",
            "engine-checks",
            "engine-schema",
        ):
            self.assertRegex(justfile, rf"(?m)^{recipe}(?:\s|:)", recipe)

    def run_builder(self, args, engine_root=None):
        engine = types.ModuleType("engine")
        engine.prepare = Mock(return_value=engine_root)
        targets = types.ModuleType("codex_package.targets")
        targets.TARGET_SPECS = {"x86_64-unknown-linux-gnu": object()}
        v8 = types.ModuleType("codex_package.v8")
        v8.resolve_codex_v8_cargo_env = Mock(
            return_value={"RUSTY_V8_ARCHIVE": "pinned"}
        )
        package = types.ModuleType("codex_package")
        package.__path__ = []
        modules = {
            "engine": engine,
            "codex_package": package,
            "codex_package.targets": targets,
            "codex_package.v8": v8,
        }
        rustc = "rustc 1.95.0\nhost: x86_64-unknown-linux-gnu\n"
        with (
            patch.dict(sys.modules, modules),
            patch.object(sys, "path", sys.path.copy()),
            patch.object(sys, "argv", [str(ROOT / "scripts/build_nyacode.py"), *args]),
            patch.dict(os.environ, {}, clear=False),
            patch("subprocess.check_output", return_value=rustc),
            patch(
                "subprocess.run", return_value=types.SimpleNamespace(returncode=0)
            ) as run,
        ):
            with self.assertRaises(SystemExit) as exit_code:
                runpy.run_path(
                    str(ROOT / "scripts/build_nyacode.py"), run_name="__main__"
                )
        self.assertEqual(exit_code.exception.code, 0)
        return engine.prepare, v8.resolve_codex_v8_cargo_env, run.call_args

    def test_builder_defaults_to_engine_and_keeps_packaging_setup(self):
        with tempfile.TemporaryDirectory() as temp:
            engine_root = Path(temp)
            (engine_root / "codex-rs").mkdir()
            (engine_root / "scripts").mkdir()
            prepare, resolve_v8, call = self.run_builder(["--release"], engine_root)
            prepare.assert_called_once_with(root=ROOT)
            resolve_v8.assert_called_once()
            command = call.args[0]
            self.assertIn("--locked", command)
            self.assertIn("codex-cli", command)
            self.assertIn("codex-code-mode-host", command)
            self.assertIn("nyacode", command)
            self.assertIn("codex", command)
            self.assertIn("--release", command)
            self.assertEqual(call.kwargs["cwd"], engine_root / "codex-rs")
            self.assertEqual(call.kwargs["env"]["CODEX_REPO_ROOT"], str(engine_root))
            self.assertEqual(
                call.kwargs["env"]["NYACODE_ENGINE_ROOT"], str(engine_root)
            )
            self.assertEqual(call.kwargs["env"]["RUSTY_V8_ARCHIVE"], "pinned")
            self.assertEqual(
                call.kwargs["env"]["RUSTUP_TOOLCHAIN"],
                json.loads((ROOT / "engine.lock").read_text())["toolchain"],
            )
            self.assertEqual(
                call.kwargs["env"]["CARGO_TARGET_DIR"], str(ROOT / ".engine/target")
            )

    def test_builder_engine_flag_remains_compatible(self):
        with tempfile.TemporaryDirectory() as temp:
            engine_root = Path(temp)
            prepare, _, call = self.run_builder(["--engine"], engine_root)
            prepare.assert_called_once_with(root=ROOT)
            self.assertEqual(call.kwargs["cwd"], engine_root / "codex-rs")

    def test_engine_run_uses_assembly_and_external_target(self):
        engine = types.ModuleType("engine")
        workspace = ROOT / ".engine/workspace"
        engine.prepare = Mock(return_value=workspace)
        for args, cwd in [
            (["cargo", "check"], workspace / "codex-rs"),
            (["--", "cargo", "check"], workspace / "codex-rs"),
            (["--workspace-root", "bazel", "test", "//..."], workspace),
        ]:
            with (
                self.subTest(args=args),
                patch.dict(sys.modules, {"engine": engine}),
                patch.object(sys, "argv", ["engine_run.py", *args]),
                patch.dict(
                    os.environ,
                    {
                        "CARGO_TARGET_DIR": "/external-target",
                        "NYACODE_ENGINE_ROOT": "/stale-engine",
                        "RUSTUP_TOOLCHAIN": "unexpected-toolchain",
                    },
                ),
                patch(
                    "subprocess.run", return_value=types.SimpleNamespace(returncode=7)
                ) as run,
            ):
                with self.assertRaises(SystemExit) as code:
                    runpy.run_path(
                        str(ROOT / "scripts/engine_run.py"), run_name="__main__"
                    )
                self.assertEqual(code.exception.code, 7)
                self.assertEqual(run.call_args.kwargs["cwd"], cwd)
                self.assertEqual(
                    run.call_args.kwargs["env"]["CODEX_REPO_ROOT"], str(workspace)
                )
                self.assertEqual(
                    run.call_args.kwargs["env"]["CARGO_TARGET_DIR"], "/external-target"
                )
                self.assertEqual(
                    run.call_args.kwargs["env"]["NYACODE_ENGINE_ROOT"], str(workspace)
                )
                self.assertNotIn("--", run.call_args.args[0])
                self.assertEqual(
                    run.call_args.kwargs["env"]["RUSTUP_TOOLCHAIN"],
                    json.loads((ROOT / "engine.lock").read_text())["toolchain"],
                )

    def test_engine_run_locks_cargo_before_cli_arguments(self):
        engine = types.ModuleType("engine")
        engine.prepare = Mock()
        with patch.dict(sys.modules, {"engine": engine}):
            locked = runpy.run_path(str(ROOT / "scripts/engine_run.py"))[
                "locked_command"
            ]
        for command, expected in [
            (
                ["cargo", "run", "--bin", "nyacode", "--", "--help"],
                ["cargo", "run", "--locked", "--bin", "nyacode", "--", "--help"],
            ),
            (
                ["cargo", "nextest", "run", "--profile", "local"],
                ["cargo", "nextest", "run", "--locked", "--profile", "local"],
            ),
            (
                ["cargo", "run", "--", "--locked"],
                ["cargo", "run", "--locked", "--", "--locked"],
            ),
            (
                ["cargo", "test", "--locked", "--", "--nocapture"],
                ["cargo", "test", "--locked", "--", "--nocapture"],
            ),
            (["cargo", "update"], ["cargo", "update"]),
            (["cargo", "fmt", "--check"], ["cargo", "fmt", "--check"]),
        ]:
            with self.subTest(command=command):
                self.assertEqual(locked(command), expected)
        for command in [
            "build",
            "check",
            "test",
            "bench",
            "run",
            "clippy",
            "fetch",
            "metadata",
        ]:
            self.assertEqual(locked(["cargo", command]), ["cargo", command, "--locked"])

    def test_python_sdk_binary_precedence(self):
        harness = load_python_harness()
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            assembled = root / ".engine/target/debug/codex"
            source = root / "codex-rs/target/debug/codex"
            assembled.parent.mkdir(parents=True)
            source.parent.mkdir(parents=True)
            assembled.touch()
            source.touch()
            self.assertEqual(
                harness.resolve_codex_exec_path(root, "/missing/explicit", "codex"),
                "/missing/explicit",
            )
            self.assertEqual(
                harness.resolve_codex_exec_path(root, None, "codex"), str(assembled)
            )
            assembled.unlink()
            self.assertEqual(
                harness.resolve_codex_exec_path(root, None, "codex"), str(source)
            )
            source.unlink()
            self.assertIsNone(harness.resolve_codex_exec_path(root, None, "codex"))

    def test_typescript_sdk_prefers_explicit_then_assembled_binary(self):
        source = (ROOT / "sdk/typescript/tests/testCodex.ts").read_text()
        self.assertIn("process.env.CODEX_EXEC_PATH ??", source)
        self.assertIn('".engine",\n  "target"', source)
        self.assertIn(
            "existsSync(engineExecPath) ? engineExecPath : sourceExecPath", source
        )

    def test_container_engine_workspace_flag_is_explicit(self):
        result = subprocess.run(
            ["bash", str(ROOT / "scripts/verify-container.sh"), "--help"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        )
        self.assertIn("--engine-workspace", result.stdout)
        self.assertIn("--engine docker|podman", result.stdout)
        self.assertIn("gh \\\n", (ROOT / "Containerfile").read_text())
        verify_script = (ROOT / "scripts/verify-container.sh").read_text()
        self.assertIn(
            'CODEX_EXEC_PATH="${CARGO_TARGET_DIR}/debug/codex"', verify_script
        )


if __name__ == "__main__":
    unittest.main()
