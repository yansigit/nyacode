"""Pinned assembly must fail closed and never replace the last good workspace."""

import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import engine


class EngineTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.root = self.base / "product"
        self.root.mkdir()
        self.cache = self.base / "cache"
        self.cache.mkdir()
        self.files = {
            "codex-rs/Cargo.toml": b'[workspace]\nmembers = ["cli"]\n[workspace.package]\nversion = "0.0.0"\n',
            "codex-rs/cli/Cargo.toml": b'[package]\nname = "codex-cli"\nversion.workspace = true\n',
            "codex-rs/Cargo.lock": b'version = 4\n[[package]]\nname = "codex-cli"\nversion = "0.0.0"\n',
            "codex-rs/rust-toolchain.toml": b'[toolchain]\nchannel = "1.95.0"\n',
            "README.md": b"upstream\n",
            "delete-me": b"remove this\n",
        }
        self.archive = self.cache / ("a" * 40 + ".tar.gz")
        self.make_archive(self.archive, self.files)
        self.lock = {
            "schema_version": 1,
            "repository": "openai/codex",
            "revision": "a" * 40,
            "archive_sha256": engine.digest(self.archive.read_bytes()),
            "assembly_manifest": "integration/codex/assembly.json",
            "assembled_lock": "integration/codex/Cargo.lock",
            "product_version_file": "VERSION",
            "release_tag": None,
            "toolchain": "1.95.0",
            "integration_contract_version": 1,
        }
        self.manifest = {
            "schema_version": 1,
            "patches": [],
            "overlays": [],
            "deletes": [],
        }
        self.write("VERSION", "0.0.0\n")
        self.write(self.lock["assembled_lock"], self.files["codex-rs/Cargo.lock"])
        self.save_inputs()

    def write(self, name, data):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data.encode() if isinstance(data, str) else data)
        return path

    def save_inputs(self):
        self.write("engine.lock", json.dumps(self.lock))
        self.write(self.lock["assembly_manifest"], json.dumps(self.manifest))

    @staticmethod
    def make_archive(path, files, extras=()):
        with tarfile.open(path, "w:gz") as archive:
            for name, data in files.items():
                member = tarfile.TarInfo("codex-sha/" + name)
                member.mode = 0o644
                member.size = len(data)
                archive.addfile(member, io.BytesIO(data))
            for member, data in extras:
                archive.addfile(member, io.BytesIO(data) if data is not None else None)

    def prepare(self):
        return engine.prepare(root=self.root, cache_dir=self.cache)

    def overlay(
        self,
        source="owned/README.md",
        destination="README.md",
        replace=True,
        mode="100644",
    ):
        self.manifest["overlays"].append(
            {
                "source": source,
                "destination": destination,
                "replace": replace,
                "mode": mode,
                "sha256": engine.digest(engine.blob(self.root / source)),
            }
        )
        self.save_inputs()

    def refresh_overlay_hashes(self):
        """Record reviewed overlay source bytes, as `manifest --write` does."""
        for overlay in self.manifest["overlays"]:
            overlay["sha256"] = engine.digest(
                engine.blob(self.root / overlay["source"])
            )
        self.save_inputs()

    def add_patch(self, contents, paths, name="change.patch"):
        self.write(name, contents)
        self.manifest["patches"].append({"path": name, "paths": paths})
        self.save_inputs()

    def test_preserves_layout_hashes_inputs_and_records_complete_diff(self):
        self.write("owned/README.md", "Nyacode\n")
        self.overlay()
        self.write("crates/providers/api/src/lib.rs", "pub fn native() {}\n")
        self.overlay(
            "crates/providers/api/src/lib.rs",
            "codex-rs/providers/api/src/lib.rs",
            False,
        )
        self.manifest["deletes"] = ["delete-me"]
        self.save_inputs()
        output = self.prepare()
        report = engine.assembly_report(output)
        self.assertEqual(
            (output / "codex-rs/cli/Cargo.toml").read_bytes(),
            self.files["codex-rs/cli/Cargo.toml"],
        )
        self.assertEqual((output / "README.md").read_text(), "Nyacode\n")
        self.assertFalse((output / "delete-me").exists())
        self.assertEqual(
            report["inputs"]["owned/README.md"]["sha256"], engine.digest(b"Nyacode\n")
        )
        diff = (output / ".git/nyacode/assembled.diff").read_bytes()
        self.assertIn(b"deleted file mode", diff)
        self.assertIn(b"pub fn native()", diff)
        self.assertEqual(report["diff_sha256"], engine.digest(diff))
        self.assertEqual(
            self.files["codex-rs/Cargo.lock"],
            (self.root / self.lock["assembled_lock"]).read_bytes(),
        )

    def test_changed_source_rebuilds_without_stale_workspace(self):
        self.write("owned/README.md", "first\n")
        self.overlay()
        output = self.prepare()
        old_hash = engine.assembly_report(output)["input_sha256"]
        (output / "stray-build-file").write_text("stale")
        self.write("owned/README.md", "second\n")
        self.refresh_overlay_hashes()
        self.prepare()
        self.assertEqual((output / "README.md").read_text(), "second\n")
        self.assertFalse((output / "stray-build-file").exists())
        self.assertNotEqual(engine.assembly_report(output)["input_sha256"], old_hash)

    def test_overlay_source_edit_with_stale_digest_records_actual_bytes(self):
        self.write("owned/README.md", "reviewed\n")
        self.overlay()
        self.prepare()
        self.write("owned/README.md", "edited\n")
        output = self.prepare()
        self.assertEqual((output / "README.md").read_text(), "edited\n")
        self.assertEqual(
            engine.assembly_report(output)["inputs"]["owned/README.md"]["sha256"],
            engine.digest(b"edited\n"),
        )

    def test_failed_patch_preserves_last_good_output_and_diagnostics(self):
        output = self.prepare()
        original = engine.assembly_report(output)
        self.add_patch("not a patch\n", ["README.md"])
        with self.assertRaises(engine.EngineError):
            self.prepare()
        self.assertEqual(engine.assembly_report(output), original)
        failures = list(output.parent.glob(".workspace.prepare-*/.engine-failure.json"))
        self.assertEqual(len(failures), 1)

    def test_patch_paths_and_deletes_must_be_declared(self):
        patch_text = "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-upstream\n+Nyacode\n"
        self.add_patch(patch_text, ["another-path"])
        with self.assertRaisesRegex(engine.EngineError, "unexpected changed paths"):
            self.prepare()
        self.manifest["patches"] = []
        self.add_patch(
            "diff --git a/delete-me b/delete-me\ndeleted file mode 100644\n--- a/delete-me\n+++ /dev/null\n@@ -1 +0,0 @@\n-remove this\n",
            ["delete-me"],
        )
        with self.assertRaisesRegex(engine.EngineError, "undeclared patch deletions"):
            self.prepare()
        self.manifest["deletes"] = ["delete-me"]
        self.save_inputs()
        self.assertFalse((self.prepare() / "delete-me").exists())

    def test_patch_offset_is_rejected_even_when_git_can_apply_it(self):
        self.files["README.md"] = b"extra\nzero\nfirst\nupstream\nlast\nend\n"
        self.make_archive(self.archive, self.files)
        self.lock["archive_sha256"] = engine.digest(self.archive.read_bytes())
        self.add_patch(
            "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -2,3 +2,3 @@\n first\n-upstream\n+Nyacode\n last\n",
            ["README.md"],
        )
        with self.assertRaisesRegex(engine.EngineError, "offset/fuzz"):
            self.prepare()

    def test_overlay_collision_duplicate_and_mode_errors_are_rejected(self):
        self.write("owned/README.md", "Nyacode\n")
        self.overlay(replace=False)
        with self.assertRaisesRegex(engine.EngineError, "replacement"):
            self.prepare()
        self.manifest["overlays"][0]["replace"] = True
        self.overlay()
        with self.assertRaisesRegex(engine.EngineError, "collision"):
            self.prepare()
        self.manifest["overlays"] = self.manifest["overlays"][:1]
        self.manifest["overlays"][0]["mode"] = "100755"
        self.save_inputs()
        with (
            patch.object(
                engine, "source_modes", return_value={"owned/README.md": "100644"}
            ),
            self.assertRaisesRegex(engine.EngineError, "mode mismatch"),
        ):
            self.prepare()

    def test_structural_inputs_cannot_follow_leaf_symlinks_outside_product(self):
        self.add_patch("unused patch", ["README.md"])
        self.manifest.update(
            {
                "inventory": "inventory/downstream.json",
                "regression_manifest": "contract.json",
            }
        )
        self.write("inventory/downstream.json", "{}")
        self.write("contract.json", "{}")
        self.write("scripts/engine.py", "script")
        self.save_inputs()
        names = (
            "engine.lock",
            self.lock["assembly_manifest"],
            self.lock["assembled_lock"],
            "VERSION",
            "scripts/engine.py",
            "inventory/downstream.json",
            "contract.json",
            "change.patch",
        )
        module = SimpleNamespace(validate_assembly_ownership=lambda root, assembly: [])
        for index, name in enumerate(names):
            with self.subTest(name=name):
                source = self.root / name
                contents = source.read_bytes()
                mode = source.stat().st_mode
                outside = self.base / f"external-{index}"
                outside.write_bytes(contents)
                source.unlink()
                source.symlink_to(outside)
                try:
                    with (
                        patch.dict(
                            sys.modules, {"native_provider_architecture": module}
                        ),
                        self.assertRaisesRegex(
                            engine.EngineError,
                            "structural input must be a regular file",
                        ),
                    ):
                        self.prepare()
                    self.assertFalse((self.root / ".engine/workspace").exists())
                finally:
                    source.unlink()
                    source.write_bytes(contents)
                    source.chmod(mode)

    def test_declared_symlink_overlay_hashes_target_bytes_and_preserves_link(self):
        source = self.write("owned/readme", "placeholder")
        source.unlink()
        source.symlink_to("../README.md")
        self.overlay("owned/readme", "docs/readme", False, "120000")
        output = self.prepare()
        self.assertEqual(os.readlink(output / "docs/readme"), "../README.md")
        observed = engine.assembly_report(output)["inputs"]["owned/readme"]
        self.assertEqual(
            observed, {"mode": "120000", "sha256": engine.digest(b"../README.md")}
        )

    def test_input_version_and_toolchain_must_match_committed_lock(self):
        self.write("VERSION", "0.1.0\n")
        with self.assertRaisesRegex(engine.EngineError, "explicitly regenerate"):
            self.prepare()
        self.write(
            self.lock["assembled_lock"],
            self.files["codex-rs/Cargo.lock"].replace(b"0.0.0", b"0.1.0"),
        )
        output = self.prepare()
        self.assertEqual(
            __import__("tomllib").loads((output / "codex-rs/Cargo.toml").read_text())[
                "workspace"
            ]["package"]["version"],
            "0.1.0",
        )
        self.lock["toolchain"] = "1.96.0"
        self.save_inputs()
        with self.assertRaisesRegex(engine.EngineError, "toolchain mismatch"):
            self.prepare()
        self.assertEqual(engine.assembly_report(output)["product_version"], "0.1.0")

    def test_catalog_version_uses_pristine_engine_identity_and_exact_binding(self):
        destination = "codex-rs/models-manager/src/lib.rs"
        marker = 'const ENGINE_CLIENT_VERSION: &str = "@NYACODE_ENGINE_VERSION@";\n'
        self.write("owned/catalog.rs", marker)
        self.overlay("owned/catalog.rs", destination, False)
        for tag, product, patched in (
            (None, "0.1.0", True),
            ("rust-v0.164.0", "0.1.0", True),
            ("rust-v0.164.0", "0.0.0", False),
        ):
            with self.subTest(tag=tag, product=product, patched=patched):
                self.lock["release_tag"] = tag
                self.manifest["patches"] = []
                self.write("VERSION", product + "\n")
                self.write(
                    self.lock["assembled_lock"],
                    self.files["codex-rs/Cargo.lock"].replace(
                        b"0.0.0", product.encode()
                    ),
                )
                self.save_inputs()
                if patched:
                    self.add_patch(
                        "diff --git a/codex-rs/Cargo.toml b/codex-rs/Cargo.toml\n"
                        "--- a/codex-rs/Cargo.toml\n+++ b/codex-rs/Cargo.toml\n"
                        '@@ -4 +4 @@\n-version = "0.0.0"\n+version = "9.9.9"\n',
                        ["codex-rs/Cargo.toml"],
                    )
                output = self.prepare()
                expected = "0.164.0" if tag else "0.0.0"
                self.assertEqual(
                    (output / destination).read_bytes(),
                    f'const ENGINE_CLIENT_VERSION: &str = "{expected}";\n'.encode(),
                )
                self.assertEqual(
                    engine.assembly_report(output)["product_version"], product
                )
        accepted = engine.assembly_report(output)
        for source in ("// missing binding\n", marker * 2):
            with self.subTest(source=source):
                self.write("owned/catalog.rs", source)
                self.refresh_overlay_hashes()
                with self.assertRaisesRegex(
                    engine.EngineError, "exactly one engine catalog"
                ):
                    self.prepare()
                self.assertEqual(engine.assembly_report(output), accepted)

    def test_unsafe_archive_paths_links_and_special_entries_are_rejected(self):
        cases = [
            ("codex-sha/../../escape", tarfile.REGTYPE, ""),
            ("/absolute", tarfile.REGTYPE, ""),
            ("codex-sha/link", tarfile.SYMTYPE, "../../escape"),
            ("codex-sha/link", tarfile.SYMTYPE, ".git/config"),
            ("codex-sha/hardlink", tarfile.LNKTYPE, "codex-sha/README.md"),
            ("codex-sha/device", tarfile.CHRTYPE, ""),
        ]
        for name, kind, target in cases:
            with self.subTest(name=name, kind=kind):
                member = tarfile.TarInfo(name)
                member.type = kind
                member.linkname = target
                archive = self.base / "unsafe.tar.gz"
                self.make_archive(
                    archive,
                    self.files,
                    [(member, b"" if kind == tarfile.REGTYPE else None)],
                )
                output = self.base / "extraction"
                output.mkdir(exist_ok=True)
                with self.assertRaises(engine.EngineError):
                    engine.extract_archive(archive, output)
                self.assertEqual(list(output.iterdir()), [])

    def test_safe_symlinks_are_preserved_and_link_parent_writes_rejected(self):
        link = tarfile.TarInfo("codex-sha/docs/readme")
        link.type = tarfile.SYMTYPE
        link.linkname = "../README.md"
        self.make_archive(self.archive, self.files, [(link, None)])
        self.lock["archive_sha256"] = engine.digest(self.archive.read_bytes())
        self.save_inputs()
        output = self.prepare()
        self.assertEqual(os.readlink(output / "docs/readme"), "../README.md")
        self.write("owned/link", "unused")
        (self.root / "owned/link").unlink()
        (self.root / "owned/link").symlink_to("../../outside")
        self.overlay("owned/link", "docs/other", False, "120000")
        with self.assertRaisesRegex(engine.EngineError, "escaping link"):
            self.prepare()
        self.files["docs/readme/nested"] = b"cannot write through symlink"
        self.make_archive(self.archive, self.files, [(link, None)])
        with self.assertRaisesRegex(engine.EngineError, "through a link"):
            engine.extract_archive(self.archive, self.base / "other")

    def test_archive_checksum_failure_never_publishes_partial_cache(self):
        self.archive.unlink()
        with (
            patch.object(
                engine,
                "download",
                side_effect=lambda repo, rev, dest: dest.write_bytes(b"corrupt"),
            ),
            self.assertRaisesRegex(engine.EngineError, "checksum"),
        ):
            self.prepare()
        self.assertEqual(list(self.cache.iterdir()), [])
        self.assertFalse((self.root / ".engine/workspace").exists())

    def test_cache_is_revalidated_on_every_prepare(self):
        output = self.prepare()
        archive = (
            self.cache / f"{self.lock['revision']}-{self.lock['archive_sha256']}.tar.gz"
        )
        archive.write_bytes(b"corrupt cache")
        with patch.object(engine, "download") as download:
            with self.assertRaisesRegex(engine.EngineError, "corrupt archive cache"):
                self.prepare()
            download.assert_not_called()
        self.assertTrue((output / "README.md").exists())

    def test_paths_reject_windows_aliases_and_writes_through_symlink_parents(self):
        for value in (
            "C:/escape",
            "file:stream",
            ".GIT/config",
            ".git./config",
            "NUL",
            "con.txt",
            "trailing.",
            "x//y",
        ):
            with self.subTest(value=value), self.assertRaises(engine.EngineError):
                engine.relative(value)
        (self.root / "real").mkdir()
        (self.root / "alias").symlink_to("real", target_is_directory=True)
        with self.assertRaisesRegex(engine.EngineError, "through a symlink"):
            engine.located(self.root, "alias/undeclared")

    def test_output_and_cache_must_be_disposable(self):
        with self.assertRaisesRegex(engine.EngineError, "disposable"):
            engine.prepare(root=self.root, output=self.root, cache_dir=self.cache)
        arbitrary = self.base / "existing"
        arbitrary.mkdir()
        with self.assertRaisesRegex(engine.EngineError, "non-engine directory"):
            engine.prepare(root=self.root, output=arbitrary, cache_dir=self.cache)
        with self.assertRaisesRegex(engine.EngineError, "outside"):
            engine.prepare(root=self.root, cache_dir=self.root / "cache")

    def test_revised_pin_never_reuses_archive_from_previous_revision(self):
        self.prepare()
        self.lock["revision"] = "b" * 40
        self.files["README.md"] = b"new upstream\n"
        revised = self.base / "new-upstream.tar.gz"
        self.make_archive(revised, self.files)
        self.lock["archive_sha256"] = engine.digest(revised.read_bytes())
        self.save_inputs()
        with patch.object(
            engine,
            "download",
            side_effect=lambda repo, rev, dest: shutil.copyfile(revised, dest),
        ) as download:
            output = self.prepare()
        self.assertEqual(download.call_args.args[:2], ("openai/codex", "b" * 40))
        self.assertEqual((output / "README.md").read_bytes(), b"new upstream\n")

    def test_canonical_executable_and_symlink_modes_are_recorded_in_git(self):
        script = tarfile.TarInfo("codex-sha/run.sh")
        script.mode = 0o755
        script.size = len(b"#!/bin/sh\n")
        self.make_archive(self.archive, self.files, [(script, b"#!/bin/sh\n")])
        self.lock["archive_sha256"] = engine.digest(self.archive.read_bytes())
        owned = self.write("owned/tool", "tool\n")
        owned.chmod(0o755)
        self.overlay("owned/tool", "bin/tool", False, "100755")
        output = self.prepare()
        modes = engine.git(output, "ls-files", "--stage", "run.sh", "bin/tool").decode()
        self.assertEqual(modes.count("100755"), 2)
        owned.chmod(0o644)
        self.manifest["overlays"][0]["mode"] = "100644"
        self.save_inputs()
        output = self.prepare()
        modes = engine.git(output, "ls-files", "--stage", "run.sh", "bin/tool").decode()
        self.assertIn("100644", modes)
        self.assertIn("100755", modes)

    def test_unavailable_symlink_privileges_fail_explicitly(self):
        with (
            patch.object(
                Path, "symlink_to", side_effect=PermissionError("not permitted")
            ),
            self.assertRaisesRegex(engine.EngineError, "symlink creation privileges"),
        ):
            engine.create_link(self.base / "link", "README.md")

    def test_input_changed_during_assembly_cannot_replace_accepted_workspace(self):
        output = self.prepare()
        original = engine.assembly_report(output)
        assemble = engine.assemble

        def changed(*args):
            report = assemble(*args)
            self.write("VERSION", "0.1.0\n")
            return report

        with (
            patch.object(engine, "assemble", side_effect=changed),
            self.assertRaisesRegex(engine.EngineError, "input changed"),
        ):
            self.prepare()
        self.assertEqual(engine.assembly_report(output), original)

    def test_ownership_validation_runs_before_extracting_and_hashes_contract_inputs(
        self,
    ):
        self.manifest.update(
            {
                "inventory": "inventory/downstream.json",
                "regression_manifest": "contract.json",
            }
        )
        self.write("inventory/downstream.json", "{}")
        self.write("contract.json", "{}")
        self.save_inputs()
        module = SimpleNamespace(
            validate_assembly_ownership=lambda root, assembly: ["missing owner"]
        )
        with patch.dict(sys.modules, {"native_provider_architecture": module}):
            with self.assertRaisesRegex(engine.EngineError, "missing owner"):
                self.prepare()
            module.validate_assembly_ownership = lambda root, assembly: []
            report = engine.assembly_report(self.prepare())
        self.assertIn("inventory/downstream.json", report["inputs"])
        self.assertIn("contract.json", report["inputs"])

    def test_upgrade_only_accepts_stable_release_tags_before_network(self):
        for tag in (
            "main",
            "a" * 40,
            "rust-v1.2.3-alpha.1",
            "v1.2.3",
            "rust-v01.2.3",
            "rust-v1.2.3/../../main",
        ):
            with self.subTest(tag=tag), patch.object(engine.subprocess, "run") as run:
                with self.assertRaisesRegex(engine.EngineError, "stable"):
                    engine.upgrade(root=self.root, tag=tag, cache_dir=self.cache)
                run.assert_not_called()

    def test_upgrade_rejects_draft_prerelease_and_mismatched_release_before_download(
        self,
    ):
        original = (self.root / "engine.lock").read_bytes()
        for index, changed in enumerate(
            (
                {"draft": True},
                {"prerelease": True},
                {"tag_name": "other"},
                {"draft": None},
            )
        ):
            release = {
                "tag_name": "rust-v1.2.3",
                "draft": False,
                "prerelease": False,
                **changed,
            }
            output = self.base / f"rejected-{index}"
            with (
                patch.object(engine, "github_json", return_value=release),
                patch.object(engine, "download") as download,
            ):
                with self.assertRaisesRegex(engine.EngineError, "published stable"):
                    engine.upgrade(
                        root=self.root,
                        tag="rust-v1.2.3",
                        output=output,
                        cache_dir=self.cache,
                    )
                download.assert_not_called()
            self.assertEqual((self.root / "engine.lock").read_bytes(), original)
            self.assertFalse((output / "engine.lock").exists())
            self.assertEqual(
                json.loads((output / "result.json").read_text())["result"], "failed"
            )

    def test_upgrade_rejects_downgrade_divergence_and_same_revision(self):
        for index, comparison in enumerate(
            (
                {"status": "behind"},
                {"status": "identical"},
                {"status": "diverged"},
                {"status": "ahead", "merge_base_commit": {"sha": "c" * 40}},
            )
        ):
            responses = [
                {"tag_name": "rust-v1.2.3", "draft": False, "prerelease": False},
                {"sha": "b" * 40},
                comparison,
            ]
            with (
                patch.object(engine, "github_json", side_effect=responses),
                patch.object(engine, "download") as download,
            ):
                with self.assertRaisesRegex(
                    engine.EngineError, "newer than the accepted"
                ):
                    engine.upgrade(
                        root=self.root,
                        tag="rust-v1.2.3",
                        output=self.base / f"ancestry-{index}",
                        cache_dir=self.cache,
                    )
                download.assert_not_called()

    def test_upgrade_uses_exact_pristine_toolchain_and_rejects_nonexact(self):
        accepted = (self.root / "engine.lock").read_bytes()
        for index, channel in enumerate(("1.99.1", "stable", "1.99", "1.99.1-beta.1")):
            with self.subTest(channel=channel):
                archive = self.base / f"upstream-{index}.tar.gz"
                self.make_archive(
                    archive,
                    {
                        **self.files,
                        "codex-rs/rust-toolchain.toml": (
                            f'[toolchain]\nchannel = "{channel}"\n'.encode()
                        ),
                    },
                )
                output = self.base / f"toolchain-{index}"
                responses = [
                    {"tag_name": "rust-v1.2.3", "draft": False, "prerelease": False},
                    {"sha": "b" * 40},
                    {
                        "status": "ahead",
                        "merge_base_commit": {"sha": self.lock["revision"]},
                    },
                ]
                with (
                    patch.object(engine, "github_json", side_effect=responses),
                    patch.object(
                        engine,
                        "download",
                        side_effect=lambda repo,
                        revision,
                        destination,
                        source=archive: (shutil.copyfile(source, destination)),
                    ),
                    patch.object(
                        engine, "check", return_value=output / "workspace"
                    ) as check,
                ):
                    if channel == "1.99.1":
                        engine.upgrade(
                            root=self.root,
                            tag="rust-v1.2.3",
                            output=output,
                            cache_dir=self.cache,
                        )
                        check.assert_called_once()
                        self.assertEqual(
                            engine.load_lock(output / "product")["toolchain"], channel
                        )
                    else:
                        with self.assertRaisesRegex(
                            engine.EngineError, "exact Rust toolchain"
                        ):
                            engine.upgrade(
                                root=self.root,
                                tag="rust-v1.2.3",
                                output=output,
                                cache_dir=self.cache,
                            )
                        check.assert_not_called()
                        self.assertFalse((output / "product").exists())
                self.assertEqual(
                    json.loads((output / "engine.lock").read_text())["toolchain"],
                    channel,
                )
                self.assertEqual((self.root / "engine.lock").read_bytes(), accepted)

    def test_failed_upgrade_retains_candidate_and_never_edits_accepted_inputs(self):
        before = {
            path.relative_to(self.root): path.read_bytes()
            for path in self.root.rglob("*")
            if path.is_file()
        }
        output = self.base / "proposal"

        def download(repo, revision, destination):
            shutil.copyfile(self.archive, destination)

        with (
            patch.object(
                engine.subprocess,
                "run",
                side_effect=[
                    subprocess.CompletedProcess(
                        [],
                        0,
                        json.dumps(
                            {
                                "tag_name": "rust-v1.2.3",
                                "draft": False,
                                "prerelease": False,
                            }
                        ),
                        "",
                    ),
                    subprocess.CompletedProcess(
                        [], 0, json.dumps({"sha": "b" * 40}), ""
                    ),
                    subprocess.CompletedProcess(
                        [],
                        0,
                        json.dumps(
                            {
                                "status": "ahead",
                                "merge_base_commit": {"sha": self.lock["revision"]},
                            }
                        ),
                        "",
                    ),
                ],
            ),
            patch.object(engine, "download", side_effect=download),
            patch.object(
                engine,
                "check",
                side_effect=engine.EngineError("strict patch preflight failed"),
            ),
            self.assertRaisesRegex(engine.EngineError, "preflight failed"),
        ):
            engine.upgrade(
                root=self.root,
                tag="rust-v1.2.3",
                output=output,
                cache_dir=self.cache,
            )
        self.assertEqual(
            json.loads((output / "result.json").read_text())["result"], "failed"
        )
        candidate = json.loads((output / "engine.lock").read_text())
        self.assertEqual(candidate["revision"], "b" * 40)
        self.assertEqual(candidate["release_tag"], "rust-v1.2.3")
        self.assertTrue((output / "resolve.log").exists())
        for name, content in before.items():
            self.assertEqual((self.root / name).read_bytes(), content)
        self.assertFalse((self.root / ".engine/workspace").exists())

    def generated(self, path, data, generator="fixture"):
        """Commit a generated output as `manifest --write` records it."""
        payload = data.encode() if isinstance(data, str) else data
        self.write(f"integration/codex/generated/{path}", payload)
        self.manifest.setdefault("generated", []).append(
            {
                "generator": generator,
                "mode": "100644",
                "path": path,
                "sha256": engine.digest(payload),
            }
        )
        self.save_inputs()

    def test_generated_output_replaces_upstream_file_and_digest_mismatch_fails_closed(
        self,
    ):
        self.generated("README.md", "generated\n")
        output = self.prepare()
        self.assertEqual((output / "README.md").read_text(), "generated\n")
        report = engine.assembly_report(output)
        self.assertIn("README.md", report["changed_paths"])
        self.assertIn("integration/codex/generated/README.md", report["inputs"])
        self.write("integration/codex/generated/README.md", "edited\n")
        with self.assertRaisesRegex(
            engine.EngineError,
            r"generated output sha256 does not match.*README.md.*manifest --write",
        ):
            self.prepare()
        self.assertEqual((output / "README.md").read_text(), "generated\n")

    def test_generated_output_may_not_be_patched_overlaid_or_create_a_new_file(self):
        self.generated("README.md", "generated\n")
        self.add_patch(
            "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n"
            "@@ -1 +1 @@\n-upstream\n+patched\n",
            ["README.md"],
        )
        with self.assertRaisesRegex(
            engine.EngineError, "generated destination collision"
        ):
            self.prepare()
        self.manifest["patches"] = []
        self.manifest["generated"] = [
            {
                "generator": "fixture",
                "mode": "100644",
                "path": "docs/new.md",
                "sha256": engine.digest(b"new\n"),
            }
        ]
        self.write("integration/codex/generated/docs/new.md", "new\n")
        self.save_inputs()
        with self.assertRaisesRegex(
            engine.EngineError, "must replace an upstream regular file"
        ):
            self.prepare()


if __name__ == "__main__":
    unittest.main()
