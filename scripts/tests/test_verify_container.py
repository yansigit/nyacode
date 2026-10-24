"""Regression tests for container verification selection and artifact trust."""

import hashlib
import io
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import verify_container as verifier


class PackageSelectionTests(unittest.TestCase):
    def setUp(self):
        self.root = Path("/fixture/repository")
        self.packages = [
            {
                "name": "leaf",
                "manifest_path": str(self.root / "codex-rs/leaf/Cargo.toml"),
                "dependencies": [],
            },
            {
                "name": "core",
                "manifest_path": str(self.root / "codex-rs/core/Cargo.toml"),
                "dependencies": [{"name": "leaf", "path": "../leaf"}],
            },
            {
                "name": "cli",
                "manifest_path": str(self.root / "codex-rs/cli/Cargo.toml"),
                "dependencies": [{"name": "core", "path": "../core"}],
            },
            {
                "name": "unrelated",
                "manifest_path": str(self.root / "codex-rs/unrelated/Cargo.toml"),
                "dependencies": [],
            },
        ]

    def test_changed_leaf_selects_transitive_consumers(self):
        self.assertEqual(
            verifier.select_packages(
                self.root, {"codex-rs/leaf/src/lib.rs"}, self.packages
            ),
            ["cli", "core", "leaf"],
        )

    def test_workspace_inputs_never_report_zero_affected_crates(self):
        for path in verifier.WORKSPACE_INPUTS | {
            "codex-rs/.cargo/config.toml",
            "third_party/v8/BUILD.bazel",
        }:
            with self.subTest(path=path):
                self.assertEqual(
                    len(verifier.select_packages(self.root, {path}, self.packages)), 4
                )

    def test_deleted_package_or_unclassified_rust_input_selects_workspace(self):
        self.assertEqual(
            len(
                verifier.select_packages(
                    self.root, {"codex-rs/removed/Cargo.toml"}, self.packages
                )
            ),
            4,
        )

    def test_documentation_only_change_has_no_rust_packages(self):
        self.assertEqual(
            verifier.select_packages(self.root, {"docs/usage.md"}, self.packages), []
        )

    def test_owned_sources_and_patches_select_assembled_consumers(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = root / "integration/codex/assembly.json"
            manifest.parent.mkdir(parents=True)
            manifest.write_text(
                json.dumps(
                    {
                        "overlays": [
                            {
                                "source": "crates/providers/leaf/src/lib.rs",
                                "destination": "codex-rs/leaf/src/lib.rs",
                            }
                        ],
                        "patches": [
                            {
                                "path": "integration/codex/patches/leaf.patch",
                                "paths": ["codex-rs/leaf/src/lib.rs"],
                            }
                        ],
                    }
                )
            )
            for source in (
                "crates/providers/leaf/src/lib.rs",
                "integration/codex/patches/leaf.patch",
            ):
                changed = verifier.assembly_changes(root, {source})
                self.assertEqual(
                    verifier.select_packages(self.root, changed, self.packages),
                    ["cli", "core", "leaf"],
                )

    def test_undeclared_owned_source_selects_all_consumers(self):
        self.assertEqual(
            len(
                verifier.select_packages(
                    self.root, {"crates/new/src/lib.rs"}, self.packages
                )
            ),
            4,
        )

    def test_missing_base_fails_instead_of_claiming_no_tests(self):
        with (
            patch.object(
                verifier.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 1),
            ),
            self.assertRaisesRegex(ValueError, "shared history"),
        ):
            verifier.affected_packages(self.root)

    def test_git_failure_is_not_treated_as_an_empty_diff(self):
        with (
            patch.object(
                verifier.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 0),
            ),
            patch.object(
                verifier.subprocess,
                "check_output",
                side_effect=subprocess.CalledProcessError(128, "git"),
            ),
            self.assertRaises(subprocess.CalledProcessError),
        ):
            verifier.affected_packages(self.root)


class MountTests(unittest.TestCase):
    def test_container_build_uses_the_pinned_rust_toolchain(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "arguments.jsonl"
            engine = Path(temporary) / "container-engine"
            engine.write_text(
                f"#!{sys.executable}\n"
                "import json, sys\n"
                f"with open({str(log)!r}, 'a') as stream:\n"
                "    stream.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            )
            engine.chmod(0o755)
            subprocess.run(
                [
                    "bash",
                    str(verifier.ROOT / "scripts/verify-container.sh"),
                    "--engine",
                    str(engine),
                    "run",
                    "--",
                    "true",
                ],
                check=True,
                capture_output=True,
                text=True,
            )
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            build = next(call for call in calls if call[0] == "build")
            toolchain = json.loads((verifier.ROOT / "engine.lock").read_text())[
                "toolchain"
            ]
            self.assertEqual(
                build[build.index("--build-arg") + 1], f"RUST_VERSION={toolchain}"
            )

    def check_mount(self, source):
        script = (verifier.ROOT / "scripts/verify-container.sh").read_text()
        function = re.search(
            r"^assert_mount_is_not_host_home\(\) \{\n.*?^\}",
            script,
            re.MULTILINE | re.DOTALL,
        ).group()
        return subprocess.run(
            [
                "bash",
                "-c",
                function
                + "\nassert_mount_is_not_host_home "
                + shlex.quote(source + ":/src"),
            ],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_checkout_under_home_is_allowed(self):
        self.assertEqual(
            self.check_mount(str(Path.home() / "Projects/nyacode")).returncode, 0
        )

    def test_entire_home_and_ancestors_are_rejected(self):
        for path in (Path.home(), Path.home().parent, Path("/")):
            with self.subTest(path=path):
                self.assertNotEqual(self.check_mount(str(path)).returncode, 0)

    def test_named_cache_volume_is_allowed(self):
        self.assertEqual(self.check_mount("nyacode-verify-target").returncode, 0)

    def test_home_symlink_cannot_allow_the_entire_physical_home(self):
        with tempfile.TemporaryDirectory() as temporary:
            physical = Path(temporary) / "physical"
            physical.mkdir()
            alias = Path(temporary) / "alias"
            alias.symlink_to(physical, target_is_directory=True)
            with patch.dict(os.environ, {"HOME": str(alias)}):
                self.assertNotEqual(self.check_mount(str(physical)).returncode, 0)
                self.assertEqual(
                    self.check_mount(str(physical / "Projects/nyacode")).returncode,
                    0,
                )


class DependencyOverrideTests(unittest.TestCase):
    def test_upstream_suites_use_engine_environment_and_propagate_failures(self):
        script = (verifier.ROOT / "scripts/verify-container.sh").read_text()
        functions = "\n".join(
            re.search(
                rf"^{name}\(\) \{{\n.*?^\}}",
                script,
                re.MULTILINE | re.DOTALL,
            ).group()
            for name in ("phase", "run_dependency_tests")
        )
        for failed_package in ("", "aws-config", "aws-sigv4"):
            with (
                self.subTest(failed_package=failed_package),
                tempfile.TemporaryDirectory(prefix="dependency checks ") as temporary,
            ):
                root = Path(temporary)
                workspace = root / "assembled engine"
                for package in ("aws-config", "aws-sigv4"):
                    directory = workspace / "third_party/rust" / package
                    directory.mkdir(parents=True)
                    (directory / "Cargo.toml").touch()
                log = root / "calls.jsonl"
                cargo = root / "cargo"
                cargo.write_text(
                    f"#!{sys.executable}\n"
                    "import json, os, pathlib, sys\n"
                    "cwd = pathlib.Path.cwd()\n"
                    f"with open({str(log)!r}, 'a') as stream:\n"
                    "    stream.write(json.dumps({'args': sys.argv[1:], 'cwd': str(cwd), "
                    "'no_color': os.environ.get('NO_COLOR'), "
                    "'toolchain': os.environ.get('RUSTUP_TOOLCHAIN')}) + '\\n')\n"
                    "sys.exit(23 if cwd.name == os.environ['FAIL_PACKAGE'] else 0)\n"
                )
                cargo.chmod(0o755)
                toolchain = json.loads((verifier.ROOT / "engine.lock").read_text())[
                    "toolchain"
                ]
                result = subprocess.run(
                    [
                        "bash",
                        "-c",
                        "set -euo pipefail\n"
                        + functions
                        + "\nphase dependencies run_dependency_tests\n"
                        + '[[ "$NO_COLOR" == 1 ]]\nprintf "phase completed\\n"',
                    ],
                    env={
                        **os.environ,
                        "PATH": str(root) + os.pathsep + os.environ["PATH"],
                        "WORKSPACE_ROOT": str(workspace),
                        "TIMING_LOG": str(root / "timings"),
                        "NO_COLOR": "1",
                        "RUSTUP_TOOLCHAIN": toolchain,
                        "FAIL_PACKAGE": failed_package,
                    },
                    capture_output=True,
                    check=False,
                    text=True,
                )
                calls = [json.loads(line) for line in log.read_text().splitlines()]
                expected = (
                    ["aws-config"]
                    if failed_package == "aws-config"
                    else ["aws-config", "aws-sigv4"]
                )
                self.assertEqual([Path(call["cwd"]).name for call in calls], expected)
                for call in calls:
                    self.assertEqual(
                        call["args"],
                        [
                            "test",
                            "--locked",
                            "--all-features",
                            "--manifest-path",
                            str(Path(call["cwd"]) / "Cargo.toml"),
                            "--",
                            "--test-threads=2",
                        ],
                    )
                    self.assertIsNone(call["no_color"])
                    self.assertEqual(call["toolchain"], toolchain)
                self.assertEqual(result.returncode, 23 if failed_package else 0)
                self.assertEqual("phase completed" in result.stdout, not failed_package)


class ArtifactTests(unittest.TestCase):
    def test_checksum_manifest_accepts_release_crlf(self):
        digest = "a" * 64
        self.assertEqual(
            verifier.manifest_hashes(f"{digest}  artifact.a.gz\r\n".encode()),
            {"artifact.a.gz": digest},
        )

    def test_manifest_rejects_paths_and_duplicate_entries(self):
        digest = "a" * 64
        for content in [
            f"{digest}  ../escape\n",
            f"{digest}  file\n{digest}  file\n",
            "invalid\n",
        ]:
            with self.subTest(content=content), self.assertRaises(ValueError):
                verifier.manifest_hashes(content.encode())

    def test_valid_cached_artifact_needs_no_network(self):
        with tempfile.TemporaryDirectory() as temporary:
            artifact = Path(temporary) / "archive"
            artifact.write_bytes(b"verified")
            with patch.object(verifier.urllib.request, "urlopen") as network:
                verifier.verified_download(
                    "https://example.test/archive",
                    artifact,
                    verifier.file_hash(artifact),
                    100,
                )
                network.assert_not_called()

    def test_failed_download_never_replaces_existing_cache(self):
        for content, url, limit in [
            (b"wrong", "https://example.test/archive", 100),
            (b"verified", "http://example.test/archive", 100),
            (b"verified", "https://example.test/archive", 2),
        ]:
            with (
                self.subTest(content=content, url=url, limit=limit),
                tempfile.TemporaryDirectory() as temporary,
            ):
                artifact = Path(temporary) / "archive"
                artifact.write_bytes(b"old")
                response = io.BytesIO(content)
                response.url = url
                with (
                    patch.object(
                        verifier.urllib.request, "urlopen", return_value=response
                    ),
                    self.assertRaises(ValueError),
                ):
                    verifier.verified_download(
                        "https://example.test/archive",
                        artifact,
                        hashlib.sha256(b"verified").hexdigest(),
                        limit,
                    )
                self.assertEqual(artifact.read_bytes(), b"old")
                self.assertEqual(list(Path(temporary).iterdir()), [artifact])


if __name__ == "__main__":
    unittest.main()
