"""Exercise the public CLI and private home without using real credentials."""

import importlib.util
import json
import shutil
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
BINARY = Path(
    os.environ.get("NYACODE_TEST_BINARY", ROOT / ".engine/target/debug/nyacode")
)


@unittest.skipUnless(BINARY.is_file(), "build the nyacode CLI first")
class NyacodeBrandingTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("CODEX_", "NYACODE_"))
        }
        self.env["HOME"] = str(self.home)

    def run_cli(self, *args, binary=BINARY):
        return subprocess.run(
            [str(binary), *args],
            env=self.env,
            capture_output=True,
            text=True,
            check=False,
        )

    def accounts(self):
        result = self.run_cli("provider", "accounts", "cursor", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        response = json.loads(result.stdout)
        self.assertEqual(response["settings"]["members"], [])
        return response

    def test_public_help_version_and_completions(self):
        help_text = self.run_cli("--help").stdout
        self.assertIn("Nyacode CLI", help_text)
        self.assertIn("Usage: nyacode", help_text)
        self.assertIn("~/.nyacode/config.toml", help_text)
        self.assertTrue(self.run_cli("--version").stdout.startswith("nyacode "))
        self.assertIn("_nyacode()", self.run_cli("completion", "bash").stdout)
        alias = BINARY.with_name("codex" + BINARY.suffix)
        if alias.is_file():
            self.assertEqual(
                self.run_cli("--version").stdout,
                self.run_cli("--version", binary=alias).stdout,
            )

    def test_default_home_is_isolated_from_legacy_home(self):
        self.accounts()
        self.assertTrue((self.home / ".nyacode/native-providers").is_dir())
        self.assertFalse((self.home / ".codex").exists())

    def test_primary_override_precedes_legacy_and_uses_its_symlink_target(self):
        primary = self.home / "primary"
        legacy = self.home / "legacy"
        primary.mkdir()
        legacy.mkdir()
        alias = self.home / "alias"
        alias.symlink_to(primary, target_is_directory=True)
        self.env.update(NYACODE_HOME=str(alias), CODEX_HOME=str(legacy))
        self.accounts()
        self.assertTrue((primary / "native-providers").is_dir())
        self.assertFalse((legacy / "native-providers").exists())
        self.assertFalse((self.home / ".nyacode").exists())

    def test_legacy_override_still_works(self):
        legacy = self.home / "legacy"
        legacy.mkdir()
        self.env.update(NYACODE_HOME="", CODEX_HOME=str(legacy))
        self.accounts()
        self.assertTrue((legacy / "native-providers").is_dir())
        self.assertFalse((self.home / ".nyacode").exists())

    def test_missing_primary_override_does_not_fall_back(self):
        legacy = self.home / "legacy"
        legacy.mkdir()
        self.env.update(NYACODE_HOME=str(self.home / "missing"), CODEX_HOME=str(legacy))
        result = self.run_cli("provider", "accounts", "cursor", "--json")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("NYACODE_HOME", result.stderr)
        self.assertFalse((legacy / "native-providers").exists())

    def test_dotenv_cannot_change_either_home_override(self):
        primary = self.home / ".nyacode"
        primary.mkdir()
        injected = self.home / "injected"
        injected.mkdir()
        (primary / ".env").write_text(
            f"NYACODE_HOME={injected}\nCODEX_HOME={injected}\n", encoding="utf-8"
        )
        self.accounts()
        self.assertTrue((primary / "native-providers").is_dir())
        self.assertFalse((injected / "native-providers").exists())


@unittest.skipUnless(
    shutil.which("node") and sys.platform == "linux",
    "Node.js is required for launcher tests",
)
class NpmLauncherBrandingTest(unittest.TestCase):
    def test_staged_package_launches_both_names_and_overrides_stale_manager(self):
        spec = importlib.util.spec_from_file_location(
            "build_npm_package", ROOT / "codex-cli/scripts/build_npm_package.py"
        )
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as temp:
            package = Path(temp)
            module.stage_sources(package, "1.2.3", "codex")
            metadata = json.loads((package / "package.json").read_text())
            self.assertEqual(metadata["name"], "@yansigit/nyacode")
            self.assertEqual(set(metadata["bin"]), {"nyacode", "codex"})
            self.assertTrue(
                all(
                    value.startswith("npm:@yansigit/nyacode@")
                    for value in metadata["optionalDependencies"].values()
                )
            )
            target = subprocess.check_output(
                [
                    "node",
                    "-p",
                    "process.arch === 'arm64' ? 'aarch64-unknown-linux-musl' : 'x86_64-unknown-linux-musl'",
                ],
                text=True,
            ).strip()
            native = package / "vendor" / target / "bin/codex"
            native.parent.mkdir(parents=True)
            native.write_text(
                '#!/bin/sh\nprintf "%s|%s|%s" "$1" "$NYACODE_MANAGED_BY_NPM" "${CODEX_MANAGED_BY_BUN:-}"\n'
            )
            native.chmod(0o755)
            env = {
                **os.environ,
                "CODEX_MANAGED_BY_BUN": "1",
                "npm_config_user_agent": "npm/10",
            }
            for entrypoint in metadata["bin"].values():
                result = subprocess.run(
                    ["node", str(package / entrypoint), "--help"],
                    env=env,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "--help|1|")

            for install_id in ["#legacy", "current"]:
                with (
                    self.subTest(vite_install_id=install_id),
                    tempfile.TemporaryDirectory() as managed,
                ):
                    packages = Path(managed) / "packages"
                    install = (
                        packages / f"@yansigit/nyacode{install_id}"
                        if install_id.startswith("#")
                        else packages / "@yansigit/nyacode" / install_id
                    )
                    managed_package = install / "lib/node_modules/@yansigit/nyacode"
                    shutil.copytree(package, managed_package)
                    ownership = packages / "@yansigit/nyacode.json"
                    ownership.write_text(
                        json.dumps(
                            {"name": "@yansigit/nyacode", "installId": install_id}
                        )
                    )
                    (managed_package / native.relative_to(package)).write_text(
                        '#!/bin/sh\nprintf "%s|%s|%s" "$NYACODE_MANAGED_BY_VITE_PLUS" "$CODEX_MANAGED_BY_VITE_PLUS" "${NYACODE_MANAGED_BY_NPM:-}"\n'
                    )
                    result = subprocess.run(
                        ["node", str(managed_package / "bin/nyacode.js")],
                        env=env,
                        capture_output=True,
                        text=True,
                        check=False,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, "1|1|")


if __name__ == "__main__":
    unittest.main()
