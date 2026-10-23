"""Release regressions: incomplete matrices, mixed revisions, and unsafe retries."""

import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
os.environ.setdefault("CODEX_REPO_ROOT", str(ROOT))

import nyacode_release as release
from codex_package.archive import write_archive
from codex_package.layout import build_package_dir
from codex_package.targets import PACKAGE_VARIANTS, TARGET_SPECS, PackageInputs

spec = importlib.util.spec_from_file_location(
    "publish_nyacode_release", ROOT / ".github/scripts/publish_nyacode_release.py"
)
publisher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publisher)

VERSION = "1.2.3"
COMMIT = "a" * 40


def build_fixtures(root: Path) -> Path:
    dist = root / "dist"
    dist.mkdir()
    executable = root / "executable"
    executable.write_bytes(b"fixture executable")
    executable.chmod(0o755)
    for target in release.RELEASE_TARGETS:
        spec = TARGET_SPECS[target]
        package = root / target
        package.mkdir()
        build_package_dir(
            package,
            VERSION,
            PACKAGE_VARIANTS["codex"],
            spec,
            PackageInputs(
                entrypoint_bin=executable,
                code_mode_host_bin=executable,
                rg_bin=executable,
                bwrap_bin=executable if spec.is_linux else None,
                codex_command_runner_bin=executable if spec.is_windows else None,
                codex_windows_sandbox_setup_bin=executable if spec.is_windows else None,
            ),
        )
        (package / "bin" / f"nyacode{spec.exe_suffix}").write_bytes(
            executable.read_bytes()
        )
        (package / "bin" / f"nyacode{spec.exe_suffix}").chmod(0o755)
        for notice in release.NOTICES:
            path = package / "licenses" / notice
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("license fixture")
        archive = dist / f"codex-package-{target}.tar.gz"
        write_archive(package, archive, force=False)
        release.write_json(
            dist / f"nyacode-{target}.json",
            {
                "schema_version": 1,
                "version": VERSION,
                "commit": COMMIT,
                "target": target,
                "profile": "release",
                "rust_toolchain": "1.95.0",
                "archive": archive.name,
                "sha256": release.sha256(archive),
                "smoke_test": "passed",
                "platform_signed": False,
            },
        )
    return dist


class ReleaseVersionTest(unittest.TestCase):
    def test_supported_installer_channels(self):
        for version in (VERSION, "1.2.3-alpha", "1.2.3-alpha.2.1", "1.2.3-beta.1"):
            release.validate_version(version, f"rust-v{version}")

    def test_reject_mismatched_tags_and_development_releases(self):
        for version, tag in (
            (VERSION, "rust-v1.2.4"),
            (VERSION, "v1.2.3"),
            ("0.0.0", "rust-v0.0.0"),
            ("0.0.0-alpha.1", "rust-v0.0.0-alpha.1"),
        ):
            with self.subTest(version=version, tag=tag), self.assertRaises(ValueError):
                release.validate_version(version, tag)

    def test_reject_versions_installers_cannot_address(self):
        for version in (
            "1.2.3-rc.1",
            "1.2.3+build",
            "01.2.3",
            "1.2.3-beta.01",
            "1.2.3\ninjected",
        ):
            with self.subTest(version=version), self.assertRaises(ValueError):
                release.validate_version(version)

    def test_development_dry_run_is_allowed(self):
        release.validate_version("0.0.0")


class ReleaseSetTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.dist = build_fixtures(self.root)

    def test_complete_set_gets_installer_checksums_and_source_metadata(self):
        release.seal(self.dist, VERSION, COMMIT)
        checksums = (self.dist / release.CHECKSUMS).read_text().splitlines()
        self.assertEqual(len(checksums), 6)
        for entry in checksums:
            digest, name = entry.split("  ")
            self.assertEqual(digest, release.sha256(self.dist / name))
        provenance = json.loads((self.dist / release.PROVENANCE).read_text())
        self.assertEqual(provenance["commit"], COMMIT)
        self.assertEqual(len(provenance["packages"]), 6)

    def test_missing_platform_blocks_publication(self):
        next(self.dist.glob("*.tar.gz")).unlink()
        with self.assertRaisesRegex(ValueError, "Incomplete/unexpected"):
            release.seal(self.dist, VERSION, COMMIT)
        self.assertFalse((self.dist / release.CHECKSUMS).exists())

    def test_engine_provenance_must_match_the_qualified_assembly(self):
        provenance = {
            "engine_revision": "e" * 40,
            "engine_archive_sha256": "a" * 64,
            "integration_input_sha256": "b" * 64,
            "integration_contract_version": 1,
        }
        for path in self.dist.glob("*.json"):
            release.write_json(path, {**json.loads(path.read_text()), **provenance})
        with patch.object(release, "engine_provenance", return_value=provenance):
            self.assertEqual(len(release.verify(self.dist, VERSION, COMMIT)), 6)
            path = next(self.dist.glob("*.json"))
            value = json.loads(path.read_text())
            value["integration_input_sha256"] = "c" * 64
            release.write_json(path, value)
            with self.assertRaisesRegex(ValueError, "Inconsistent build report"):
                release.verify(self.dist, VERSION, COMMIT)

    def test_assembled_reports_cannot_bypass_input_validation(self):
        path = next(self.dist.glob("*.json"))
        release.write_json(
            path, {**json.loads(path.read_text()), "engine_revision": "e" * 40}
        )
        with patch.object(release, "engine_provenance", return_value={}):
            with self.assertRaises(ValueError):
                release.verify(self.dist, VERSION, COMMIT)

    def test_unexpected_asset_blocks_publication(self):
        (self.dist / "surprise.zip").write_bytes(b"unexpected")
        with self.assertRaises(ValueError):
            release.verify(self.dist, VERSION, COMMIT)

    def test_tampered_bytes_block_publication(self):
        next(self.dist.glob("*.tar.gz")).write_bytes(b"modified")
        with self.assertRaisesRegex(ValueError, "digest mismatch"):
            release.verify(self.dist, VERSION, COMMIT)

    def test_mixed_commit_version_profile_or_failed_smoke_blocks_publication(self):
        report_path = next(self.dist.glob("*.json"))
        original = json.loads(report_path.read_text())
        for field, value in (
            ("commit", "b" * 40),
            ("version", "1.2.4"),
            ("profile", "dev"),
            ("smoke_test", "failed"),
            ("archive", "../../other.tar.gz"),
            ("rust_toolchain", "nightly"),
        ):
            with self.subTest(field=field):
                release.write_json(report_path, {**original, field: value})
                with self.assertRaisesRegex(ValueError, "Inconsistent"):
                    release.verify(self.dist, VERSION, COMMIT)
        release.write_json(report_path, original)

    def test_missing_runtime_helper_is_rejected_even_with_valid_archive_digest(self):
        target = release.RELEASE_TARGETS[0]
        package = self.root / target
        (package / "bin/codex-code-mode-host").unlink()
        archive = self.dist / f"codex-package-{target}.tar.gz"
        write_archive(package, archive, force=True)
        report_path = self.dist / f"nyacode-{target}.json"
        report = json.loads(report_path.read_text())
        report["sha256"] = release.sha256(archive)
        release.write_json(report_path, report)
        with self.assertRaisesRegex(RuntimeError, "Missing package file"):
            release.verify(self.dist, VERSION, COMMIT)

    def test_public_binary_must_match_installer_alias(self):
        target = release.RELEASE_TARGETS[0]
        package = self.root / target
        (package / "bin/nyacode").write_bytes(b"wrong executable")
        with self.assertRaisesRegex(ValueError, "alias differ"):
            release.check_package(package, target, VERSION)

    def test_required_license_files_exist_in_source(self):
        for notice in release.NOTICES:
            self.assertTrue((ROOT / notice).is_file(), notice)

    def test_extraction_rejects_traversal_and_links(self):
        for name, kind in (("../escape", tarfile.REGTYPE), ("link", tarfile.SYMTYPE)):
            archive = self.root / "bad.tar.gz"
            with tarfile.open(archive, "w:gz") as stream:
                member = tarfile.TarInfo(name)
                member.type = kind
                member.linkname = "/etc/passwd" if kind == tarfile.SYMTYPE else ""
                stream.addfile(member, io.BytesIO(b""))
            with (
                self.subTest(name=name),
                self.assertRaises((ValueError, tarfile.FilterError)),
            ):
                release.extract_package(archive, self.root / "extracted")
        self.assertFalse((self.root / "escape").exists())


class PublicationTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.file = self.root / "asset.tar.gz"
        self.file.write_bytes(b"verified package")
        self.files = {self.file.name: self.file}
        self.asset = {
            "name": self.file.name,
            "digest": f"sha256:{release.sha256(self.file)}",
            "state": "uploaded",
        }

    def test_existing_identical_asset_can_resume_without_overwrite(self):
        publisher.verify_assets([self.asset], self.files, complete=True)
        publisher.verify_assets([], self.files, complete=False)

    def test_incomplete_public_release_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "incomplete"):
            publisher.verify_assets([], self.files, complete=True)

    def test_conflicting_or_incomplete_assets_are_never_overwritten(self):
        for asset in (
            {**self.asset, "digest": "sha256:wrong"},
            {**self.asset, "name": "unexpected.tar.gz"},
            {**self.asset, "state": "starter"},
        ):
            with self.subTest(asset=asset), self.assertRaises(ValueError):
                publisher.verify_assets([asset], self.files, complete=False)
        with self.assertRaises(ValueError):
            publisher.verify_assets(
                [self.asset, self.asset], self.files, complete=False
            )

    def test_network_or_permission_error_cannot_mean_missing_release(self):
        for error in (
            "network timeout",
            "Forbidden (HTTP 403)",
            "Bad Gateway (HTTP 502)",
        ):
            with patch.object(
                publisher.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 1, "", error),
            ):
                with self.assertRaises(RuntimeError):
                    publisher.api("releases/latest")

    def test_missing_release_is_distinct_from_api_failure(self):
        with patch.object(
            publisher.subprocess,
            "run",
            return_value=subprocess.CompletedProcess([], 1, "", "Not Found (HTTP 404)"),
        ):
            self.assertIsNone(publisher.api("releases/latest"))

    def test_moved_tag_blocks_publication_before_any_release_mutation(self):
        with patch.object(publisher, "api", return_value={"sha": "b" * 40}) as api:
            with self.assertRaisesRegex(ValueError, "tag no longer"):
                publisher.publish(self.root, VERSION, COMMIT)
        api.assert_called_once_with("commits/rust-v1.2.3")

    def test_upload_failure_never_publishes_the_draft(self):
        dist = build_fixtures(self.root)
        draft = {
            "id": 1,
            "draft": True,
            "target_commitish": COMMIT,
            "prerelease": False,
            "assets": [],
        }
        with patch.object(
            publisher, "api", side_effect=[{"sha": COMMIT}, draft]
        ) as api:
            with patch.object(
                publisher.subprocess,
                "run",
                side_effect=subprocess.CalledProcessError(1, "gh release upload"),
            ):
                with self.assertRaises(subprocess.CalledProcessError):
                    publisher.publish(dist, VERSION, COMMIT)
        self.assertEqual(api.call_count, 2)

    def test_new_release_and_fresh_runner_retry_preserve_verified_assets(self):
        dist = build_fixtures(self.root)
        retry_dist = self.root / "retry-dist"
        # Each GitHub job downloads the original unsealed platform artifacts.
        shutil.copytree(dist, retry_dist)
        remote = None
        events = []

        def api(endpoint, body=None, method=None):
            nonlocal remote
            if endpoint == f"commits/rust-v{VERSION}":
                return {"sha": COMMIT}
            if endpoint == f"releases/tags/rust-v{VERSION}":
                return remote
            if endpoint == "releases/generate-notes":
                return {"body": "Generated notes"}
            if endpoint == "releases" and body is not None:
                self.assertTrue(body["draft"])
                remote = {"id": 1, "assets": [], **body}
                events.append("draft-created")
                return remote
            if endpoint == "releases/1" and method == "PATCH":
                events.append("published")
                remote.update(body)
                return remote
            if endpoint == "releases/1":
                return remote
            if endpoint == "releases/latest":
                return None
            self.fail(f"Unexpected GitHub request: {endpoint}")

        def upload(command, **kwargs):
            self.assertEqual(
                command[:6],
                [
                    "gh",
                    "release",
                    "upload",
                    f"rust-v{VERSION}",
                    "--repo",
                    publisher.REPOSITORY,
                ],
            )
            self.assertTrue(remote["draft"])
            for filename in command[6:]:
                path = Path(filename)
                remote["assets"].append(
                    {
                        "name": path.name,
                        "digest": f"sha256:{release.sha256(path)}",
                        "state": "uploaded",
                    }
                )
            events.append("uploaded")
            return subprocess.CompletedProcess(command, 0)

        with (
            patch.object(publisher, "api", side_effect=api),
            patch.object(publisher.subprocess, "run", side_effect=upload) as uploads,
        ):
            publisher.publish(dist, VERSION, COMMIT)
            expected_assets = {
                f"codex-package-{target}.tar.gz" for target in release.RELEASE_TARGETS
            } | {release.CHECKSUMS, release.PROVENANCE}
            self.assertEqual(
                {asset["name"] for asset in remote["assets"]}, expected_assets
            )
            self.assertEqual(events, ["draft-created", "uploaded", "published"])
            self.assertFalse(remote["draft"])
            self.assertEqual(remote["make_latest"], "true")
            self.assertEqual(remote["target_commitish"], COMMIT)
            original_assets = json.dumps(remote["assets"], sort_keys=True)

            publisher.publish(retry_dist, VERSION, COMMIT)
            uploads.assert_called_once()
            self.assertEqual(events, ["draft-created", "uploaded", "published"])
            self.assertEqual(
                json.dumps(remote["assets"], sort_keys=True), original_assets
            )

    def test_prerelease_and_older_stable_never_move_latest(self):
        for version, current, prerelease in (
            ("1.2.3-beta.1", None, True),
            (
                VERSION,
                {"tag_name": "rust-v2.0.0", "draft": False, "prerelease": False},
                False,
            ),
        ):
            with self.subTest(version=version), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                # Publication control test uses a sealed one-file set; archive verification is tested above.
                archive = root / "package.tar.gz"
                archive.write_bytes(b"package")
                for name in (release.CHECKSUMS, release.PROVENANCE):
                    (root / name).write_text("metadata")
                assets = [
                    {
                        "name": path.name,
                        "digest": f"sha256:{release.sha256(path)}",
                        "state": "uploaded",
                    }
                    for path in root.iterdir()
                ]
                draft = {
                    "id": 1,
                    "draft": True,
                    "target_commitish": COMMIT,
                    "prerelease": prerelease,
                    "assets": assets,
                }
                calls = [{"sha": COMMIT}, draft, draft]
                if not prerelease:
                    calls.append(current)
                calls.append({})
                with (
                    patch.object(publisher, "seal"),
                    patch.object(publisher, "api", side_effect=calls) as api,
                ):
                    publisher.publish(root, version, COMMIT)
                self.assertEqual(
                    api.call_args.args[1], {"draft": False, "make_latest": "false"}
                )


if __name__ == "__main__":
    unittest.main()
