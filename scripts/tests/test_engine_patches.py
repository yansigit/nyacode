"""Rebase and export must keep the patch stack exact, fail loudly on ambiguity, and never misroute."""

import contextlib
import io
import json
import shutil
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import engine
import engine_manifest
import engine_patches

PIN = "a" * 40
README = "".join(f"line {number}\n" for number in range(1, 31)).encode()
BASE_FILES = {
    "README.md": README,
    "codex-rs/Cargo.toml": b'[workspace]\nmembers = ["cli"]\n[workspace.package]\nversion = "0.0.0"\n',
    "codex-rs/rust-toolchain.toml": b'[toolchain]\nchannel = "1.95.0"\n',
    "docs/extra.md": b"extra one\nextra two\n",
    "docs/old.md": b"obsolete\n",
    "legacy.txt": b"engine-level delete\n",
}


def write_files(root: Path, files: dict[str, bytes | None]) -> None:
    for name, data in files.items():
        path = root / name
        if data is None:
            path.unlink(missing_ok=True)
            continue
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)


def commit(repo: Path, message: str) -> str:
    engine.git(repo, "add", "--force", "--all", "--", ".")
    engine.git(
        repo,
        "-c",
        "user.name=test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        message,
    )
    return engine.git(repo, "rev-parse", "HEAD").decode().strip()


def make_archive(path: Path, files: dict[str, bytes]) -> None:
    with tarfile.open(path, "w:gz") as archive:
        for name, data in files.items():
            member = tarfile.TarInfo("codex-sha/" + name)
            member.mode = 0o644
            member.size = len(data)
            archive.addfile(member, io.BytesIO(data))


def make_patch(
    base: dict[str, bytes], edits: dict[str, bytes | None]
) -> tuple[bytes, list[str]]:
    """Generate a patch the way inventory/generate.py does: git diff --binary between two trees."""
    with tempfile.TemporaryDirectory(prefix="patch-fixture-") as scratch:
        repo = Path(scratch)
        engine.git(repo, "init", "--quiet")
        write_files(repo, base)
        commit(repo, "base")
        write_files(repo, edits)
        engine.git(repo, "add", "--force", "--all", "--", ".")
        diff = engine.git(
            repo,
            "diff",
            "--cached",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-renames",
            "HEAD",
        )
        return diff, sorted(engine.changed_paths(repo, "HEAD"))


class PatchStackTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="engine-patches-")
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.root = self.base / "product"
        self.root.mkdir()
        self.cache = self.base / "cache"
        self.cache.mkdir()
        self.files = dict(BASE_FILES)

        staged = self.base / "pinned.tar.gz"
        make_archive(staged, self.files)
        sha = engine.digest(staged.read_bytes())
        staged.rename(self.cache / f"{PIN}-{sha}.tar.gz")
        self.lock = {
            "schema_version": 1,
            "repository": "openai/codex",
            "revision": PIN,
            "release_tag": None,
            "archive_sha256": sha,
            "toolchain": "1.95.0",
            "integration_contract_version": 1,
            "baseline_product_revision": "b" * 40,
            "product_version_file": "VERSION",
            "assembly_manifest": "integration/codex/assembly.json",
            "assembled_lock": "integration/codex/Cargo.lock",
        }
        self.write_lock()
        self.manifest = {
            "schema_version": 1,
            "patches": [],
            "overlays": [],
            "deletes": ["legacy.txt"],
        }
        self.write_manifest()
        self.upstream = self.base / "upstream"
        self.upstream.mkdir()
        engine.git(self.upstream, "init", "--quiet")
        write_files(self.upstream, self.files)
        self.pin_commit = commit(self.upstream, "pin")

    def write_lock(self):
        (self.root / "engine.lock").write_text(json.dumps(self.lock, indent=2) + "\n")

    def write_manifest(self):
        path = self.root / self.lock["assembly_manifest"]
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(self.manifest, indent=2, sort_keys=True) + "\n")

    def manifest_text(self):
        return (self.root / self.lock["assembly_manifest"]).read_text()

    def add_patch(self, name, edits, base=None):
        diff, paths = make_patch(base or self.files, edits)
        path = self.root / f"integration/codex/patches/{name}.patch"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(diff)
        self.manifest["patches"].append(
            {"path": f"integration/codex/patches/{name}.patch", "paths": paths}
        )
        self.write_manifest()
        return diff

    def upstream_commit(self, edits):
        engine.git(self.upstream, "reset", "--quiet", "--hard", self.pin_commit)
        write_files(self.upstream, edits)
        return commit(self.upstream, "upstream")

    def rebase(self, onto, name="out", **kwargs):
        return engine_patches.rebase(
            root=self.root,
            onto_local_ref=onto,
            local_repo=self.upstream,
            cache_dir=self.cache,
            output=self.base / name,
            **kwargs,
        )

    def edited_readme(self, line, text):
        lines = README.decode().splitlines(keepends=True)
        lines[line - 1] = text + "\n"
        return "".join(lines).encode()


class RebaseAndExportTest(PatchStackTest):
    def test_same_base_rebase_is_noop_and_export_is_byte_identical(self):
        original = self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "LINE TWO")}
        )
        original_manifest = self.manifest_text()
        output, report = self.rebase(self.pin_commit, "same")
        self.assertEqual(report["totals"]["conflicted_patches"], 0)
        self.assertEqual(report["onto"]["revision"], self.pin_commit)
        engine_patches.export(root=self.root, workspace=output / "workspace")
        self.assertEqual(
            (self.root / "integration/codex/patches/documentation.patch").read_bytes(),
            original,
        )
        self.assertEqual(self.manifest_text(), original_manifest)

    def test_non_overlapping_upstream_edit_is_clean_and_merged(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "LINE TWO")}
        )
        onto = self.upstream_commit(
            {"README.md": self.edited_readme(28, "LINE TWENTY EIGHT")}
        )
        output, report = self.rebase(onto, "clean")
        self.assertEqual(report["totals"]["conflicted_patches"], 0)
        self.assertEqual(report["patches"][0]["files"][0]["status"], "clean")
        merged = (output / "workspace/README.md").read_text()
        self.assertIn("LINE TWO", merged)
        self.assertIn("LINE TWENTY EIGHT", merged)

    def test_overlapping_upstream_edit_conflicts_and_exits_two(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "PATCH SIDE")}
        )
        onto = self.upstream_commit(
            {"README.md": self.edited_readme(2, "UPSTREAM SIDE")}
        )
        output, report = self.rebase(onto, "conflict")
        record = report["patches"][0]["files"][0]
        self.assertEqual((record["status"], record["conflict_blocks"]), ("conflict", 1))
        self.assertEqual(report["totals"]["conflicted_patches"], 1)
        self.assertEqual(report["totals"]["conflict_blocks"], 1)
        self.assertIn("<<<<<<< ", (output / "workspace/README.md").read_text())
        argv = [
            "engine.py",
            "--root",
            str(self.root),
            "--cache-dir",
            str(self.cache),
            "rebase",
            "--onto-local-ref",
            onto,
            "--local-repo",
            str(self.upstream),
            "--output",
            str(self.base / "cli-conflict"),
        ]
        with patch.object(sys, "argv", argv), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(engine.main(), 2)

    def test_upstream_deletes_patched_file_reports_deleted_upstream(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "PATCH SIDE")}
        )
        onto = self.upstream_commit({"README.md": None})
        _, report = self.rebase(onto, "deleted")
        record = report["patches"][0]["files"][0]
        self.assertEqual(record["status"], "deleted_upstream")
        self.assertEqual(record["conflict_blocks"], 1)

    def test_export_refuses_conflict_markers(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "PATCH SIDE")}
        )
        onto = self.upstream_commit(
            {"README.md": self.edited_readme(2, "UPSTREAM SIDE")}
        )
        output, _ = self.rebase(onto, "markers")
        with self.assertRaisesRegex(engine.EngineError, "conflict markers"):
            engine_patches.export(root=self.root, workspace=output / "workspace")

    def test_unresolved_modify_delete_refuses_until_accepted(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "PATCH SIDE")}
        )
        onto = self.upstream_commit({"README.md": None})
        output, _ = self.rebase(onto, "unresolved")
        with self.assertRaisesRegex(engine.EngineError, "still hold merged bytes"):
            engine_patches.export(root=self.root, workspace=output / "workspace")
        engine_patches.export(
            root=self.root, workspace=output / "workspace", accept=["README.md"]
        )
        patch_text = (
            self.root / "integration/codex/patches/documentation.patch"
        ).read_bytes()
        self.assertIn(b"PATCH SIDE", patch_text)

    def test_changed_undeclared_path_is_error_until_assigned(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "LINE TWO")}
        )
        output, _ = self.rebase(self.pin_commit, "undeclared")
        workspace = output / "workspace"
        (workspace / "notes").mkdir()
        (workspace / "notes/new.txt").write_text("new\n")
        with self.assertRaisesRegex(engine.EngineError, "not owned by any patch"):
            engine_patches.export(root=self.root, workspace=workspace)
        engine_patches.export(
            root=self.root,
            workspace=workspace,
            assign=["notes/new.txt=documentation"],
        )
        manifest = json.loads(self.manifest_text())
        self.assertEqual(
            manifest["patches"][0]["paths"], ["README.md", "notes/new.txt"]
        )
        self.assertIn(
            b"notes/new.txt",
            (self.root / "integration/codex/patches/documentation.patch").read_bytes(),
        )

    def test_declared_path_reverted_is_dropped_from_patch(self):
        self.add_patch(
            "documentation",
            {
                "README.md": self.edited_readme(2, "LINE TWO"),
                "docs/extra.md": b"changed\n",
            },
        )
        output, _ = self.rebase(self.pin_commit, "reverted")
        workspace = output / "workspace"
        (workspace / "docs/extra.md").write_bytes(BASE_FILES["docs/extra.md"])
        result = engine_patches.export(root=self.root, workspace=workspace)
        self.assertEqual(result["dropped"], {"documentation": ["docs/extra.md"]})
        manifest = json.loads(self.manifest_text())
        self.assertEqual(manifest["patches"][0]["paths"], ["README.md"])
        self.assertNotIn(
            b"docs/extra.md",
            (self.root / "integration/codex/patches/documentation.patch").read_bytes(),
        )

    def test_upstream_rename_of_patched_file_fails_loudly(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "LINE TWO")}
        )
        onto = self.upstream_commit({"README.md": None, "GUIDE.md": README})
        with self.assertRaisesRegex(engine.EngineError, "renamed a patched path"):
            self.rebase(onto, "renamed")

    def test_binary_patch_round_trips_byte_identically(self):
        blob = bytes(range(256)) * 4
        original = self.add_patch(
            "assets",
            {"assets/logo.bin": blob, "README.md": self.edited_readme(3, "LINE THREE")},
        )
        self.assertIn(b"GIT binary patch", original)
        onto = self.upstream_commit({"docs/extra.md": b"extra one\nupstream\n"})
        output, report = self.rebase(onto, "binary")
        self.assertEqual(report["totals"]["conflicted_patches"], 0)
        self.assertEqual((output / "workspace/assets/logo.bin").read_bytes(), blob)
        engine_patches.export(root=self.root, workspace=output / "workspace")
        self.assertEqual(
            (self.root / "integration/codex/patches/assets.patch").read_bytes(),
            original,
        )

    def test_patch_deletions_are_recorded_and_engine_deletes_preserved(self):
        self.add_patch(
            "documentation",
            {"README.md": self.edited_readme(2, "LINE TWO"), "docs/old.md": None},
        )
        self.manifest["deletes"] = sorted(["docs/old.md", "legacy.txt"])
        self.write_manifest()
        output, _ = self.rebase(self.pin_commit, "deletes")
        self.manifest["deletes"] = ["legacy.txt"]
        self.write_manifest()
        engine_patches.export(root=self.root, workspace=output / "workspace")
        manifest = json.loads(self.manifest_text())
        self.assertEqual(manifest["deletes"], ["docs/old.md", "legacy.txt"])

    def test_export_update_lock_writes_engine_lock_from_github_report(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "LINE TWO")}
        )
        onto = self.upstream_commit({"README.md": self.edited_readme(28, "LINE 28")})
        new_files = dict(BASE_FILES, **{"README.md": self.edited_readme(28, "LINE 28")})
        new_archive = self.base / "new.tar.gz"
        make_archive(new_archive, new_files)
        new_sha = engine.digest(new_archive.read_bytes())
        with patch.object(
            engine_patches, "fetch_revision", return_value=(new_archive, new_sha)
        ):
            output, report = engine_patches.rebase(
                root=self.root,
                onto=onto,
                cache_dir=self.cache,
                output=self.base / "github",
            )
        self.assertEqual(report["onto"]["source"], "github")
        engine_patches.export(
            root=self.root, workspace=output / "workspace", update_lock=True
        )
        lock = json.loads((self.root / "engine.lock").read_text())
        self.assertEqual((lock["revision"], lock["archive_sha256"]), (onto, new_sha))
        self.assertIsNone(lock["release_tag"])
        engine.load_lock(self.root)

    def test_update_lock_refuses_local_ref_reports(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "LINE TWO")}
        )
        output, _ = self.rebase(self.pin_commit, "local")
        with self.assertRaisesRegex(engine.EngineError, "GitHub archive digest"):
            engine_patches.export(
                root=self.root, workspace=output / "workspace", update_lock=True
            )

    def test_export_rejects_non_pristine_workspace(self):
        with self.assertRaisesRegex(engine.EngineError, "not a rebase workspace"):
            engine_patches.export(root=self.root, workspace=self.base / "missing")


class FoldTest(PatchStackTest):
    """Fold carries main's patch edits onto a rebased workspace and reports what it cannot merge."""

    def main_tree(self, edits, manifest_changes=None):
        """Copy the product root as main would look after the given patch edits."""
        main = self.base / "main-root"
        if main.exists():
            shutil.rmtree(main)
        shutil.copytree(self.root, main)
        diff, paths = make_patch(self.files, edits)
        (main / "integration/codex/patches/documentation.patch").write_bytes(diff)
        manifest = json.loads((main / self.lock["assembly_manifest"]).read_text())
        manifest["patches"][0]["paths"] = paths
        for key, value in (manifest_changes or {}).items():
            manifest[key] = value
        (main / self.lock["assembly_manifest"]).write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n"
        )
        return main

    def base_tree(self):
        base = self.base / "base-root"
        if base.exists():
            shutil.rmtree(base)
        shutil.copytree(self.root, base)
        return base

    def readme_with(self, edits):
        lines = README.decode().splitlines(keepends=True)
        for line, text in edits.items():
            lines[line - 1] = text + "\n"
        return "".join(lines).encode()

    def test_main_side_patch_edit_is_carried_and_upstream_edit_kept(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "PATCH SIDE")}
        )
        base = self.base_tree()
        main = self.main_tree(
            {"README.md": self.readme_with({2: "PATCH SIDE", 3: "MAIN THREE"})}
        )
        onto = self.upstream_commit(
            {"README.md": self.edited_readme(28, "UPSTREAM 28")}
        )
        output, _ = self.rebase(onto, "fold-clean")
        report = engine_patches.fold(
            output=output, base_root=base, main_root=main, cache_dir=self.cache
        )
        self.assertEqual(report["fold"]["changed"], ["README.md"])
        self.assertEqual(report["fold"]["deferred"], [])
        self.assertEqual(report["totals"]["conflicted_patches"], 0)
        merged = (output / "workspace/README.md").read_text()
        for expected in ("PATCH SIDE", "MAIN THREE", "UPSTREAM 28"):
            self.assertIn(expected, merged)
        self.assertEqual(report["fold"]["assign"], {"README.md": "documentation"})

    def test_main_added_path_is_written_and_assigned_to_its_owner(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "PATCH SIDE")}
        )
        base = self.base_tree()
        main = self.main_tree(
            {
                "README.md": self.edited_readme(2, "PATCH SIDE"),
                "notes/extra.txt": b"extra note\n",
            }
        )
        onto = self.upstream_commit(
            {"README.md": self.edited_readme(28, "UPSTREAM 28")}
        )
        output, _ = self.rebase(onto, "fold-added")
        report = engine_patches.fold(
            output=output, base_root=base, main_root=main, cache_dir=self.cache
        )
        self.assertEqual(report["fold"]["changed"], ["notes/extra.txt"])
        self.assertEqual(report["fold"]["assign"], {"notes/extra.txt": "documentation"})
        self.assertEqual(
            (output / "workspace/notes/extra.txt").read_text(), "extra note\n"
        )
        listed = engine.git(
            output / "workspace",
            "ls-tree",
            "-r",
            "--name-only",
            report["workspace_tree"],
        )
        self.assertIn(b"notes/extra.txt", listed.split(b"\n"))

    def test_overlapping_main_and_upstream_edits_are_a_reported_conflict(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "PATCH SIDE")}
        )
        base = self.base_tree()
        # Edits far apart: the rebase itself is clean, and only main's edit meets upstream's.
        main = self.main_tree(
            {"README.md": self.readme_with({2: "PATCH SIDE", 25: "MAIN 25"})}
        )
        onto = self.upstream_commit(
            {"README.md": self.edited_readme(25, "UPSTREAM 25")}
        )
        output, _ = self.rebase(onto, "fold-conflict")
        report = engine_patches.fold(
            output=output, base_root=base, main_root=main, cache_dir=self.cache
        )
        record = next(
            item
            for item in report["patches"][0]["files"]
            if item["path"] == "README.md"
        )
        self.assertEqual((record["status"], record["conflict_blocks"]), ("conflict", 1))
        self.assertEqual(report["fold"]["conflicted"], ["README.md"])
        self.assertEqual(report["totals"]["conflicted_patches"], 1)
        self.assertIn("<<<<<<< ", (output / "workspace/README.md").read_text())

    def test_main_edit_to_an_already_conflicted_path_is_deferred(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "PATCH SIDE")}
        )
        base = self.base_tree()
        main = self.main_tree(
            {"README.md": self.readme_with({2: "PATCH SIDE", 4: "MAIN FOUR"})}
        )
        onto = self.upstream_commit(
            {"README.md": self.edited_readme(2, "UPSTREAM SIDE")}
        )
        output, rebase_report = self.rebase(onto, "fold-deferred")
        self.assertEqual(rebase_report["totals"]["conflicted_patches"], 1)
        report = engine_patches.fold(
            output=output, base_root=base, main_root=main, cache_dir=self.cache
        )
        self.assertEqual(report["fold"]["deferred"], ["README.md"])
        self.assertEqual(report["totals"]["conflicted_files"], 1)
        self.assertNotIn("MAIN FOUR", (output / "workspace/README.md").read_text())

    def test_fold_refuses_integration_inputs_it_cannot_carry(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "PATCH SIDE")}
        )
        base = self.base_tree()
        main = self.base / "main-extra"
        shutil.copytree(base, main)
        (main / "integration/codex/overlays").mkdir()
        (main / "integration/codex/overlays/note.txt").write_text("new\n")
        onto = self.upstream_commit({"README.md": self.edited_readme(28, "X")})
        output, _ = self.rebase(onto, "fold-refused")
        with self.assertRaisesRegex(engine.EngineError, "integration input changed"):
            engine_patches.fold(
                output=output, base_root=base, main_root=main, cache_dir=self.cache
            )
        self.assertNotIn(
            "fold", json.loads((output / "rebase-report.json").read_text())
        )

    def test_fold_refuses_manifest_changes_beyond_patch_path_lists(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "PATCH SIDE")}
        )
        base = self.base_tree()
        main = self.main_tree(
            {"README.md": self.edited_readme(2, "PATCH SIDE")},
            {"deletes": ["legacy.txt", "docs/old.md"]},
        )
        blockers = engine_patches.fold_blockers(base, main)
        self.assertIn("assembly manifest changed beyond patch path lists", blockers)

    def test_fold_refuses_a_second_fold(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "PATCH SIDE")}
        )
        base = self.base_tree()
        main = self.main_tree(
            {"README.md": self.readme_with({2: "PATCH SIDE", 3: "MAIN THREE"})}
        )
        onto = self.upstream_commit({"README.md": self.edited_readme(28, "X")})
        output, _ = self.rebase(onto, "fold-twice")
        engine_patches.fold(
            output=output, base_root=base, main_root=main, cache_dir=self.cache
        )
        with self.assertRaisesRegex(engine.EngineError, "already folded"):
            engine_patches.fold(
                output=output, base_root=base, main_root=main, cache_dir=self.cache
            )

    def test_fold_refuses_a_pin_change(self):
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "PATCH SIDE")}
        )
        base = self.base_tree()
        main = self.main_tree({"README.md": self.edited_readme(2, "PATCH SIDE")})
        lock = json.loads((main / "engine.lock").read_text())
        lock["revision"] = "c" * 40
        (main / "engine.lock").write_text(json.dumps(lock, indent=2) + "\n")
        blockers = engine_patches.fold_blockers(base, main)
        self.assertIn("pinned upstream revision changed", blockers)


class GeneratedOutputsTest(PatchStackTest):
    """Generated outputs leave the patches; their conflicts are regenerated, not resolved by hand."""

    def write_ownership(self, outputs, tree=True):
        ownership = {
            "schema_version": 1,
            "generated": [
                {
                    "id": "readme",
                    "command": "fixture",
                    "outputs": list(outputs),
                    "owner": "downstream-docs",
                    "purpose": "documentation",
                    "regression_groups": ["documentation"],
                    "deterministic": True,
                    "tree": tree,
                }
            ],
        }
        path = self.root / "integration/codex/ownership.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(ownership, indent=2, sort_keys=True) + "\n")

    def test_export_drops_generated_outputs_from_patches_and_reports_them(self):
        self.write_ownership(["README.md"])
        committed = self.root / "integration/codex/generated/README.md"
        committed.parent.mkdir(parents=True, exist_ok=True)
        committed.write_bytes(self.edited_readme(2, "LINE TWO"))
        self.add_patch(
            "documentation",
            {
                "README.md": self.edited_readme(2, "LINE TWO"),
                "docs/extra.md": b"EXTRA ONE\nextra two\n",
            },
        )
        output, _ = self.rebase(self.pin_commit, "generated-export")
        (output / "workspace/README.md").write_text("regenerated elsewhere\n")
        with patch.object(engine_manifest, "run", return_value=0):
            result = engine_patches.export(
                root=self.root, workspace=output / "workspace"
            )
        self.assertEqual(result["generated_ignored"], ["README.md"])
        self.assertEqual(result["dropped"], {"documentation": ["README.md"]})
        patch_bytes = (
            self.root / "integration/codex/patches/documentation.patch"
        ).read_bytes()
        self.assertNotIn(b"README.md", patch_bytes)
        self.assertIn(b"docs/extra.md", patch_bytes)
        self.assertEqual(self.manifest_text().count("README.md"), 0)

    def test_export_refuses_generator_outputs_that_are_not_committed(self):
        self.write_ownership(["README.md"])
        self.add_patch(
            "documentation",
            {
                "README.md": self.edited_readme(2, "LINE TWO"),
                "docs/extra.md": b"EXTRA ONE\nextra two\n",
            },
        )
        output, _ = self.rebase(self.pin_commit, "uncommitted")
        with self.assertRaisesRegex(engine.EngineError, "not committed.*README.md"):
            engine_patches.export(root=self.root, workspace=output / "workspace")
        self.assertIn(
            b"README.md",
            (self.root / "integration/codex/patches/documentation.patch").read_bytes(),
        )

    def test_regenerate_conflict_blocks_export_until_regenerated_or_accepted(self):
        self.write_ownership(["README.md"], tree=False)
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "LINE TWO")}
        )
        onto = self.upstream_commit(
            {"README.md": self.edited_readme(2, "UPSTREAM TWO")}
        )
        output, report = self.rebase(onto, "regenerate-blocks")
        self.assertEqual(report["totals"]["regenerate_files"], 1)
        with (
            patch.object(engine_manifest, "run", return_value=0),
            self.assertRaisesRegex(
                engine.EngineError, "conflicts still hold merged bytes"
            ),
        ):
            engine_patches.export(root=self.root, workspace=output / "workspace")
        (output / "workspace/README.md").write_bytes(
            self.edited_readme(2, "REGENERATED")
        )
        with patch.object(engine_manifest, "run", return_value=0):
            result = engine_patches.export(
                root=self.root, workspace=output / "workspace"
            )
        self.assertEqual(
            result["rewritten"], ["integration/codex/patches/documentation.patch"]
        )
        self.assertIn(
            b"REGENERATED",
            (self.root / "integration/codex/patches/documentation.patch").read_bytes(),
        )

    def test_conflict_in_generated_output_is_regenerate_and_takes_upstream(self):
        self.write_ownership(["README.md"])
        self.add_patch(
            "documentation", {"README.md": self.edited_readme(2, "LINE TWO")}
        )
        onto = self.upstream_commit(
            {"README.md": self.edited_readme(2, "UPSTREAM TWO")}
        )
        output, report = self.rebase(onto, "generated-regenerate")
        [record] = report["patches"][0]["files"]
        self.assertEqual(record["status"], "regenerate")
        self.assertEqual(record["conflict_blocks"], 0)
        self.assertEqual(record["generator"], "readme")
        self.assertEqual(report["patches"][0]["status"], "clean")
        totals = report["totals"]
        self.assertEqual(totals["conflicted_files"], 0)
        self.assertEqual(totals["conflict_blocks"], 0)
        self.assertEqual(totals["regenerate_files"], 1)
        self.assertEqual(
            (output / "workspace/README.md").read_bytes(),
            (self.upstream / "README.md").read_bytes(),
        )
        self.assertIn("1 regenerate files", engine_patches.summary_lines(report)[-1])

    def test_generated_tree_file_is_regenerate_only_when_upstream_bytes_differ(self):
        self.write_ownership(["README.md"])
        committed = self.root / "integration/codex/generated/README.md"
        committed.parent.mkdir(parents=True, exist_ok=True)
        committed.write_bytes(README)
        self.manifest["generated"] = [
            {
                "generator": "readme",
                "mode": "100644",
                "path": "README.md",
                "sha256": engine.digest(README),
            }
        ]
        self.add_patch("documentation", {"docs/extra.md": b"EXTRA ONE\nextra two\n"})
        same, same_report = self.rebase(self.pin_commit, "generated-same")
        self.assertEqual(same_report["generated"][0]["status"], "clean")
        self.assertEqual(same_report["totals"]["regenerate_files"], 0)
        onto = self.upstream_commit(
            {"README.md": self.edited_readme(3, "UPSTREAM THREE")}
        )
        output, report = self.rebase(onto, "generated-moved")
        [record] = report["generated"]
        self.assertEqual(record["status"], "regenerate")
        self.assertEqual(record["generator"], "readme")
        self.assertEqual(report["totals"]["regenerate_files"], 1)
        self.assertEqual(
            (output / "workspace/README.md").read_bytes(),
            (self.upstream / "README.md").read_bytes(),
        )


class FetchRevisionTest(unittest.TestCase):
    def test_downloads_once_then_reuses_verified_cache(self):
        with tempfile.TemporaryDirectory(prefix="fetch-") as scratch:
            cache = Path(scratch) / "cache"
            source = Path(scratch) / "source.tar.gz"
            make_archive(source, BASE_FILES)
            payload = source.read_bytes()
            revision = "c" * 40

            def fake_download(repository, rev, destination):
                self.assertEqual((repository, rev), ("openai/codex", revision))
                destination.write_bytes(payload)

            with patch.object(
                engine, "download", side_effect=fake_download
            ) as download:
                first = engine_patches.fetch_revision(revision, cache)
                second = engine_patches.fetch_revision(revision, cache)
            self.assertEqual(download.call_count, 1)
            self.assertEqual(first, second)
            self.assertEqual(first[1], engine.digest(payload))
            self.assertTrue(first[0].name.startswith(revision))

    def test_rejects_mutable_revisions(self):
        with self.assertRaisesRegex(engine.EngineError, "immutable"):
            engine_patches.fetch_revision("main", Path(tempfile.gettempdir()))


class ManifestValidationTest(unittest.TestCase):
    def test_patch_paths_must_be_file_disjoint(self):
        manifest = {
            "patches": [
                {"path": "integration/codex/patches/a.patch", "paths": ["x.rs"]},
                {"path": "integration/codex/patches/b.patch", "paths": ["x.rs"]},
            ]
        }
        with self.assertRaisesRegex(engine.EngineError, "file-disjoint"):
            engine_patches.patch_entries(manifest)


if __name__ == "__main__":
    unittest.main()
