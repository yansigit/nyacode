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
from types import SimpleNamespace

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
        environment = patch.dict(os.environ)
        environment.start()
        self.addCleanup(environment.stop)
        os.environ.pop("NYACODE_ENGINE_ROOT", None)
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

    def test_verification_uses_the_accepted_toolchain(self):
        product = self.root / "product"
        product.mkdir()
        release.write_json(product / "engine.lock", {"toolchain": "1.96.0"})
        for path in self.dist.glob("*.json"):
            report = json.loads(path.read_text())
            release.write_json(path, {**report, "rust_toolchain": "1.96.0"})
        with patch.object(release, "ROOT", product):
            self.assertEqual(len(release.verify(self.dist, VERSION, COMMIT)), 6)

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

    def test_overlay_license_inputs_exist_in_product_source(self):
        lock = json.loads((ROOT / "engine.lock").read_text())
        manifest = json.loads((ROOT / lock["assembly_manifest"]).read_text())
        overlays = {item["destination"]: item["source"] for item in manifest["overlays"]}
        for notice in release.NOTICES:
            if notice in overlays:
                self.assertTrue((ROOT / overlays[notice]).is_file(), notice)

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


class EngineReleaseTest(unittest.TestCase):
    def setUp(self):
        import engine

        self.engine = engine
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "product"
        self.workspace = Path(self.temp.name) / "engine"
        self.root.mkdir()
        self.workspace.mkdir()
        self.lock = json.loads((ROOT / "engine.lock").read_text())
        release.write_json(self.root / "engine.lock", self.lock)
        manifest = self.root / self.lock["assembly_manifest"]
        manifest.parent.mkdir(parents=True)
        release.write_json(manifest, {"overlays": [
            {"source": "helper.sh", "destination": "helper.sh", "mode": "100755"}
        ]})
        (self.root / "VERSION").write_text(VERSION + "\n")
        (self.root / "helper.sh").write_text("engine input\n")
        (self.root / "helper.sh").chmod(0o755)
        self.report = {
            "generated_by": "nyacode-engine-v1",
            "revision": self.lock["revision"],
            "archive_sha256": self.lock["archive_sha256"],
            "inputs": {
                name: {"sha256": engine.digest(engine.blob(self.root / name)),
                       "mode": engine.file_mode(self.root / name)}
                for name in ("engine.lock", self.lock["assembly_manifest"], "VERSION", "helper.sh")
            },
        }
        self.report["input_sha256"] = engine.digest(
            json.dumps(self.report["inputs"], sort_keys=True).encode()
        )
        self.write_report()
        for context in (
            patch.object(release, "ROOT", self.root),
            patch.dict(os.environ, {"NYACODE_ENGINE_ROOT": str(self.workspace)}),
        ):
            context.start()
            self.addCleanup(context.stop)

    def write_report(self):
        self.engine.write_json(self.workspace / self.engine.MARKER, self.report)

    def test_release_records_the_accepted_pin_and_input_digest(self):
        self.assertEqual(release.engine_provenance(), {
            "engine_revision": self.lock["revision"],
            "engine_archive_sha256": self.lock["archive_sha256"],
            "integration_input_sha256": self.report["input_sha256"],
            "integration_contract_version": self.lock["integration_contract_version"],
        })

    def test_changed_pin_or_archive_is_rejected(self):
        for key, value in (("revision", "e" * 40), ("archive_sha256", "a" * 64)):
            with self.subTest(key=key), patch.dict(self.report, {key: value}):
                self.write_report()
                with self.assertRaisesRegex(ValueError, "accepted engine pin"):
                    release.engine_provenance()

    def test_changed_missing_or_mode_changed_inputs_are_rejected(self):
        helper = self.root / "helper.sh"
        helper.write_text("changed\n")
        with self.assertRaisesRegex(ValueError, "input changed"):
            release.engine_provenance()
        helper.write_text("engine input\n")
        helper.chmod(0o644)
        with self.assertRaisesRegex(ValueError, "input changed"):
            release.engine_provenance()
        helper.unlink()
        with self.assertRaisesRegex(ValueError, "input changed"):
            release.engine_provenance()

    def test_input_digest_and_preparation_marker_are_required(self):
        for key, value in (("input_sha256", "0" * 64), ("generated_by", "other")):
            with self.subTest(key=key), patch.dict(self.report, {key: value}):
                self.write_report()
                with self.assertRaisesRegex(ValueError, "input provenance"):
                    release.engine_provenance()

    def test_windows_uses_canonical_git_modes(self):
        (self.root / "helper.sh").chmod(0o644)
        with patch.object(self.engine, "source_modes", return_value={"helper.sh": "100755"}):
            self.assertTrue(release.engine_provenance())

    def test_windows_untracked_overlay_uses_declared_mode(self):
        (self.root / "helper.sh").chmod(0o644)
        with (
            patch.object(self.engine, "source_modes", return_value={}),
            patch.object(release, "os", SimpleNamespace(environ=os.environ, name="nt")),
        ):
            self.assertTrue(release.engine_provenance())

    def test_assembly_requires_prepared_engine(self):
        with patch.dict(os.environ):
            os.environ.pop("NYACODE_ENGINE_ROOT")
            with self.assertRaisesRegex(ValueError, "NYACODE_ENGINE_ROOT is required"):
                release.assemble(release.RELEASE_TARGETS[0], self.root, self.root / "dist")

    def test_assembly_ships_engine_resources_with_product_metadata(self):
        from codex_package import version

        target = release.RELEASE_TARGETS[0]
        for name in ("nyacode", "codex-code-mode-host", "bwrap"):
            binary = self.root / name
            binary.write_bytes(b"executable fixture")
            binary.chmod(0o755)
        for notice in release.NOTICES:
            path = self.workspace / notice
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("assembled engine license\n")
        (self.workspace / "codex-rs/rust-toolchain.toml").write_text(
            '[toolchain]\nchannel = "1.95.0"\n'
        )
        (self.workspace / "VERSION").write_text("9.8.7\n")
        dist = self.root / "dist"
        with (
            patch.object(release, "fetch_rg", return_value=self.root / "nyacode") as fetch,
            patch.object(release, "smoke"),
            patch.object(release, "source_commit", return_value=COMMIT),
            patch("codex_package.cli.strip_binary"),
            patch.object(version, "REPO_ROOT", self.workspace),
        ):
            release.assemble(target, self.root, dist)
        fetch.assert_called_once_with(
            TARGET_SPECS[target], manifest_path=self.workspace / "scripts/codex_package/rg"
        )
        report = json.loads((dist / f"nyacode-{target}.json").read_text())
        self.assertEqual((report["version"], report["commit"]), (VERSION, COMMIT))
        extracted = self.root / "extracted"
        release.extract_package(dist / report["archive"], extracted)
        for notice in release.NOTICES:
            self.assertEqual((extracted / "licenses" / notice).read_text(),
                             "assembled engine license\n")

    def test_packaging_helpers_use_engine_while_metadata_uses_product(self):
        code = """
import json
import nyacode_release as release
from codex_package import cargo, targets, v8
print(json.dumps([str(targets.REPO_ROOT), str(cargo.CODEX_RS_ROOT), str(v8.REPO_ROOT)]))
release.metadata(None, None)
"""
        result = subprocess.run(
            [sys.executable, "-c", code], cwd=ROOT,
            env={**os.environ, "PYTHONPATH": str(ROOT / "scripts"),
                 "CODEX_REPO_ROOT": str(ROOT)},
            check=True, text=True, capture_output=True,
        )
        roots, metadata = map(json.loads, result.stdout.splitlines())
        self.assertEqual(roots, [str(self.workspace), str(self.workspace / "codex-rs"),
                                 str(self.workspace)])
        self.assertEqual(metadata["version"], (ROOT / "VERSION").read_text().strip())
        self.assertEqual(metadata["commit"], subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True,
        ).strip())


class PublicationTest(unittest.TestCase):
    def setUp(self):
        environment = patch.dict(os.environ)
        environment.start()
        self.addCleanup(environment.stop)
        os.environ.pop("NYACODE_ENGINE_ROOT", None)
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
