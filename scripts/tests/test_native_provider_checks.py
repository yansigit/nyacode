"""Run-order regressions for the native provider CI driver."""

import contextlib
import io
import json
import os
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import types
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import native_provider_checks as checks


class NativeProviderChecksTest(unittest.TestCase):
    def test_cli_default_prepares_product_and_preserves_explicit_or_assembled_roots(
        self,
    ):
        for product, explicit in [(True, False), (True, True), (False, False)]:
            with (
                self.subTest(product=product, explicit=explicit),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                if product:
                    (root / "scripts").mkdir()
                    (root / "scripts/engine.py").touch()
                engine = types.ModuleType("engine")
                engine.prepare = Mock(return_value=root / ".engine/workspace")
                expected = (
                    root / "explicit"
                    if explicit
                    else root / ".engine/workspace"
                    if product
                    else root
                )
                args = [
                    "native_provider_checks.py",
                    "--artifact-dir",
                    str(root / "logs"),
                ]
                if explicit:
                    args.extend(["--repo", str(expected)])
                with (
                    patch.object(checks, "ROOT", root),
                    patch.object(sys, "argv", args),
                    patch.dict(sys.modules, {"engine": engine}),
                    patch.object(checks, "run_checks", return_value=[]) as run,
                    contextlib.redirect_stdout(io.StringIO()),
                ):
                    self.assertEqual(checks.main(), 0)
                run.assert_called_once_with(expected, root / "logs")
                if product and not explicit:
                    engine.prepare.assert_called_once_with(root=root)
                else:
                    engine.prepare.assert_not_called()

    @unittest.skipUnless(
        os.name == "posix"
        and shutil.which("just")
        and (
            Path(__file__).resolve().parents[2] / "crates/providers/Cargo.toml"
        ).is_file(),
        "requires the product standalone workspace and Unix just",
    )
    def test_standalone_just_recipe_uses_provider_working_directory(self):
        root = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as temporary:
            cargo = Path(temporary) / "cargo"
            cargo.write_text(
                f"#!{sys.executable}\nimport json, os, sys\nfrom pathlib import Path\nprint(json.dumps({{'cwd': os.getcwd(), 'args': sys.argv[1:], 'toolchain_file': Path('rust-toolchain.toml').is_file()}}))\n"
            )
            cargo.chmod(0o755)
            result = subprocess.run(
                ["just", "--justfile", str(root / "justfile"), "test-providers"],
                cwd=temporary,
                env={**os.environ, "PATH": temporary + os.pathsep + os.environ["PATH"]},
                capture_output=True,
                text=True,
                check=True,
            )
            actual = json.loads(result.stdout)
            self.assertEqual(
                actual,
                {
                    "cwd": str(root / "crates/providers"),
                    "args": ["test", "--locked", "--all-features"],
                    "toolchain_file": True,
                },
            )

    def test_fresh_checkout_builds_helpers_before_integration_tests_spawn_them(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            manifest = root / "docs/native-providers/integration-manifest.json"
            manifest.parent.mkdir(parents=True)
            manifest.write_text(
                json.dumps(
                    {
                        "subsystem_packages": [],
                        "adapter_package": "adapter",
                        "single_provider_features": [],
                        "focused_test_groups": [
                            {
                                "name": "core-integration",
                                "packages": ["codex-core"],
                                "test_target": "all",
                                "minimum_tests": 1,
                            }
                        ],
                    }
                )
            )
            (root / "codex-rs").mkdir()
            (root / "codex-rs/rust-toolchain.toml").write_text(
                '[toolchain]\nchannel = "1.95.0"\n'
            )
            helpers = [
                root / "codex-rs/target/debug" / name
                for name in ("codex", "nyacode", "codex-code-mode-host")
            ]

            def run(command, **kwargs):
                if command[:2] == ["git", "rev-parse"]:
                    return subprocess.CompletedProcess(command, 0, "a" * 40)
                self.assertEqual(kwargs["cwd"], root)
                self.assertEqual(kwargs["env"]["RUSTUP_TOOLCHAIN"], "1.95.0")
                self.assertEqual(kwargs["env"]["CODEX_REPO_ROOT"], str(root))
                self.assertEqual(kwargs["env"]["NYACODE_ENGINE_ROOT"], str(root))
                log = kwargs["stdout"]
                if command[:2] == ["cargo", "build"]:
                    self.assertIn("codex-cli", command)
                    for helper in helpers:
                        self.assertIn(helper.name, command)
                        helper.parent.mkdir(parents=True, exist_ok=True)
                        helper.touch()
                if command[:2] == ["cargo", "test"]:
                    self.assertIn("--locked", command)
                    if "codex-core" in command and not all(
                        helper.is_file() for helper in helpers
                    ):
                        log.write("required integration test helper is missing\n")
                        return subprocess.CompletedProcess(command, 101)
                    log.write("test result: ok. 1 passed; 0 failed;\n")
                if "test_nyacode_branding.py" in command:
                    log.write("Ran 7 tests in 0.1s\nOK\n")
                    self.assertEqual(
                        kwargs["env"]["NYACODE_TEST_BINARY"], str(helpers[1])
                    )
                return subprocess.CompletedProcess(command, 0)

            with (
                patch.object(checks.subprocess, "run", side_effect=run),
                patch.dict(
                    checks.os.environ,
                    {
                        "RUSTUP_TOOLCHAIN": "wrong-toolchain",
                        "NYACODE_ENGINE_ROOT": "/wrong-engine",
                        "CODEX_REPO_ROOT": "/wrong-engine",
                    },
                    clear=True,
                ),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                stages = checks.run_checks(root, root / "logs")

            self.assertTrue(all(helper.is_file() for helper in helpers))
            self.assertTrue(all(stage["exit_code"] == 0 for stage in stages))
            summary = json.loads((root / "logs/summary.json").read_text())
            self.assertEqual(summary["result"], "passed")


if __name__ == "__main__":
    unittest.main()
