"""The rolling sync must plan, rebase, fold, validate, and snapshot without network or cargo."""

import hashlib
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import engine
import engine_patches
import engine_sync
import test_engine_patches as fixtures

PIN = "a" * 40
PREVIOUS = "e" * 40
README = fixtures.README
UPSTREAM_FILES = {
    "README.md": README,
    "codex-rs/rust-toolchain.toml": b'[toolchain]\nchannel = "1.95.0"\n',
    "docs/extra.md": b"extra one\nextra two\n",
}
OTHER_FILE_RESOLUTION = b"""diff --git a/docs/extra.md b/docs/extra.md
--- a/docs/extra.md
+++ b/docs/extra.md
@@ -1,2 +1,2 @@
-extra one
+extra ONE
 extra two
"""


class FakeRunner:
    """Records validation commands instead of running them; generate is optional."""

    def __init__(self, generate=True):
        self.calls = []
        self.generate = generate

    def __call__(self, argv, cwd, env=None):
        self.calls.append(argv)
        if "generate" in argv and "--help" in argv:
            return 0 if self.generate else 2
        return 0


class SyncTest(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory(prefix="engine-sync-test-")
        self.addCleanup(scratch.cleanup)
        self.base = Path(scratch.name)
        self.cache = self.base / "cache"
        self.cache.mkdir()
        self.archives = {}
        self.status = self.base / "status.jsonl"
        environment = patch.dict(os.environ, {"NYACODE_STATUS_FILE": str(self.status)})
        environment.start()
        self.addCleanup(environment.stop)

        pinned = self.base / "pinned.tar.gz"
        fixtures.make_archive(pinned, UPSTREAM_FILES)
        self.pin_sha = engine.digest(pinned.read_bytes())
        pinned.rename(self.cache / f"{PIN}-{self.pin_sha}.tar.gz")
        self.lock = {
            "schema_version": 1,
            "repository": "openai/codex",
            "revision": PIN,
            "release_tag": None,
            "archive_sha256": self.pin_sha,
            "toolchain": "1.95.0",
            "integration_contract_version": 1,
            "baseline_product_revision": "b" * 40,
            "product_version_file": "VERSION",
            "assembly_manifest": "integration/codex/assembly.json",
            "assembled_lock": "integration/codex/Cargo.lock",
        }

        self.upstream = self.base / "upstream"
        self.upstream.mkdir()
        engine.git(self.upstream, "init", "--quiet")
        self.repo = self.base / "product"
        self.repo.mkdir()
        engine.git(self.repo, "init", "--quiet", "-b", "main")

        fetcher = patch.object(engine_patches, "fetch_revision", side_effect=self.fetch)
        fetcher.start()
        self.addCleanup(fetcher.stop)

    def fetch(self, revision, cache_dir, *, repository=engine_patches.REPOSITORY):
        return self.archives[revision]

    def readme(self, edits):
        lines = README.decode().splitlines(keepends=True)
        for line, text in edits.items():
            lines[line - 1] = text + "\n"
        return "".join(lines).encode()

    def product_files(self, readme_edits, lock=None):
        diff, paths = fixtures.make_patch(
            UPSTREAM_FILES, {"README.md": self.readme(readme_edits)}
        )
        manifest = {
            "schema_version": 1,
            "patches": [
                {
                    "path": "integration/codex/patches/documentation.patch",
                    "paths": paths,
                }
            ],
            "overlays": [],
            "deletes": [],
        }
        return {
            "VERSION": b"0.1.0\n",
            "engine.lock": (json.dumps(lock or self.lock, indent=2) + "\n").encode(),
            "integration/codex/assembly.json": (
                json.dumps(manifest, indent=2, sort_keys=True) + "\n"
            ).encode(),
            "integration/codex/patches/documentation.patch": diff,
            "inventory/downstream.json": b"{}\n",
        }

    def product_commit(self, readme_edits=None, *, lock=None, extra=None):
        """Commit the product tree on main; readme_edits define the documentation patch."""
        files = self.product_files(readme_edits or {}, lock)
        files.update(extra or {})
        fixtures.write_files(self.repo, files)
        return fixtures.commit(self.repo, "product")

    def commit_on(self, base, files, meta=None, branch=None):
        """A commit that changes files on top of base; HEAD returns to main afterwards."""
        engine.git(self.repo, "checkout", "--quiet", "--detach", base)
        payload = dict(files)
        if meta is not None:
            payload[engine_sync.META_PATH] = (
                json.dumps(meta, indent=2) + "\n"
            ).encode()
        fixtures.write_files(self.repo, payload)
        commit = fixtures.commit(self.repo, "commit")
        if branch:
            engine.git(self.repo, "branch", "-f", branch, commit)
        engine.git(self.repo, "checkout", "--quiet", "main")
        return commit

    def snapshot(self, base, *, upstream, main_base):
        meta = {
            "schema_version": 1,
            "upstream": {"revision": upstream, "date": "2026-10-23T18:50:54+00:00"},
            "main_base": main_base,
            "from": {"kind": "main", "ref": main_base},
            "report": {"totals": {}, "patches": []},
            "resolution": "none",
            "notes": [],
            "created_at": "2026-10-26T00:00:00Z",
        }
        return self.commit_on(base, {}, meta, branch=engine_sync.BRANCH)

    def upstream_commit(self, edits):
        files = dict(UPSTREAM_FILES)
        files.update(edits)
        archive = self.base / f"upstream-{len(self.archives)}.tar.gz"
        fixtures.make_archive(archive, files)
        fixtures.write_files(self.upstream, files)
        revision = fixtures.commit(self.upstream, "upstream")
        self.archives[revision] = (archive, engine.digest(archive.read_bytes()))
        return revision

    def plan(self, upstream):
        return engine_sync.plan(
            repo=self.repo,
            upstream_ref=upstream,
            local_repo=self.upstream,
            main_ref="main",
            snapshot_ref=engine_sync.BRANCH,
        )

    def github(self, plan_value):
        """Plans from a local ref are rebased from the GitHub archive in production."""
        plan_value["upstream"].update({"source": "github", "local_repo": None})
        return plan_value

    def events(self):
        if not self.status.exists():
            return []
        return [json.loads(line) for line in self.status.read_text().splitlines()]

    def finish(self, plan_value, directory, runner=None, **kwargs):
        runner = runner or FakeRunner()
        snapshot = engine_sync.finish(
            plan_value=plan_value,
            directory=directory,
            repo=self.repo,
            runner=runner,
            **kwargs,
        )
        return snapshot, runner

    def read_at(self, commit, path):
        return engine.git(self.repo, "show", f"{commit}:{path}")


class PlanTest(SyncTest):
    def test_noop_when_snapshot_already_matches_upstream_and_main(self):
        upstream = self.upstream_commit({})
        main = self.product_commit({2: "PATCH SIDE"})
        self.snapshot(main, upstream=upstream, main_base=main)
        value = self.plan(upstream)
        self.assertEqual(value["action"], "noop")
        self.assertEqual(value["from"]["kind"], "snapshot")

    def test_starts_from_main_without_a_snapshot(self):
        upstream = self.upstream_commit({})
        main = self.product_commit({2: "PATCH SIDE"})
        value = self.plan(upstream)
        self.assertEqual(
            (value["action"], value["from"]), ("sync", {"kind": "main", "ref": main})
        )
        self.assertEqual(value["main_base"], main)
        self.assertIn("no snapshot", value["reason"])

    def test_resets_to_main_when_the_pinned_upstream_changed(self):
        upstream = self.upstream_commit({})
        base = self.product_commit({2: "PATCH SIDE"})
        self.snapshot(base, upstream=PREVIOUS, main_base=base)
        moved = dict(self.lock, revision="c" * 40, archive_sha256="d" * 64)
        main = self.product_commit({2: "PATCH SIDE"}, lock=moved)
        value = self.plan(upstream)
        self.assertEqual(value["from"]["kind"], "main")
        self.assertEqual(value["main_base"], main)
        self.assertIn("pinned", value["reason"])

    def test_folds_when_main_changed_only_patch_edits(self):
        upstream = self.upstream_commit({})
        base = self.product_commit({2: "PATCH SIDE"})
        snapshot = self.snapshot(base, upstream=PREVIOUS, main_base=base)
        self.product_commit({2: "PATCH SIDE", 3: "MAIN THREE"})
        value = self.plan(upstream)
        self.assertEqual(value["action"], "sync")
        self.assertEqual(value["from"], {"kind": "snapshot", "ref": snapshot})
        self.assertEqual(value["main_base"], base)

    def test_resets_to_main_when_main_changed_other_integration_inputs(self):
        upstream = self.upstream_commit({})
        base = self.product_commit({2: "PATCH SIDE"})
        self.snapshot(base, upstream=PREVIOUS, main_base=base)
        self.product_commit(
            {2: "PATCH SIDE"}, extra={"integration/codex/overlays/note.txt": b"new\n"}
        )
        value = self.plan(upstream)
        self.assertEqual(value["from"]["kind"], "main")
        self.assertIn("integration input changed", value["reason"])


class RebaseFinishTest(SyncTest):
    def carried_scenario(self):
        """Snapshot at base has a patch edit on line 2; main later adds an edit on line 3."""
        upstream = self.upstream_commit({"README.md": self.readme({28: "UPSTREAM 28"})})
        base = self.product_commit({2: "PATCH SIDE"})
        snapshot = self.snapshot(base, upstream=PREVIOUS, main_base=base)
        main = self.product_commit({2: "PATCH SIDE", 3: "MAIN THREE"})
        return upstream, base, snapshot, main

    def test_carries_main_patch_edit_onto_a_new_snapshot(self):
        upstream, base, snapshot, main = self.carried_scenario()
        plan_value = self.github(self.plan(upstream))
        self.assertEqual(plan_value["main_base"], base)
        directory = self.base / "sync-carry"
        self.assertEqual(
            engine_sync.rebase_plan(
                plan_value, directory, repo=self.repo, cache_dir=self.cache
            ),
            0,
        )
        snapshot_path, _ = self.finish(
            plan_value, directory, runner=FakeRunner(generate=False)
        )
        result = engine.read_json(snapshot_path)
        commit = result["commit"]
        self.assertEqual(result["parent"], snapshot)
        self.assertEqual(
            engine.git(self.repo, "rev-parse", f"{commit}^").decode().strip(), snapshot
        )
        meta = json.loads(self.read_at(commit, engine_sync.META_PATH))
        self.assertEqual(meta["upstream"]["revision"], upstream)
        self.assertEqual(meta["main_base"], main)
        self.assertEqual(
            (meta["from"]["kind"], meta["resolution"]), ("snapshot", "none")
        )
        self.assertTrue(any("generate" in note for note in meta["notes"]))
        patch_text = self.read_at(
            commit, "integration/codex/patches/documentation.patch"
        )
        self.assertIn(b"PATCH SIDE", patch_text)
        self.assertIn(b"MAIN THREE", patch_text)
        self.assertEqual(
            json.loads(self.read_at(commit, "engine.lock"))["revision"], upstream
        )
        self.assertTrue((directory / "snapshot.bundle").is_file())
        kinds = [event["kind"] for event in self.events()]
        self.assertIn("sync-rebased", kinds)
        self.assertIn("sync-ready", kinds)

    def test_finish_validates_in_order_and_skips_cargo_on_request(self):
        upstream, _, _, _ = self.carried_scenario()
        plan_value = self.github(self.plan(upstream))
        directory = self.base / "sync-validate"
        engine_sync.rebase_plan(
            plan_value, directory, repo=self.repo, cache_dir=self.cache
        )
        _, runner = self.finish(
            plan_value, directory, runner=FakeRunner(generate=True), skip_cargo=True
        )
        commands = runner.calls
        self.assertTrue(
            any("generate" in call and "--write-back" in call for call in commands)
        )
        controls = [
            index
            for index, call in enumerate(commands)
            if "check" in call and "controls" in call
        ]
        manifest = [
            index
            for index, call in enumerate(commands)
            if "manifest" in call and "--check" in call
        ]
        prepare = [
            index
            for index, call in enumerate(commands)
            if "prepare" in call and str(directory / engine_sync.PREPARED_DIR) in call
        ]
        generation = [
            index
            for index, call in enumerate(commands)
            if "prepare" in call and str(directory / engine_sync.GENERATE_DIR) in call
        ]
        write_back = next(
            index for index, call in enumerate(commands) if "--write-back" in call
        )
        self.assertTrue(manifest and prepare and controls and generation)
        # Generators run in an assembled workspace, so generation prepares before writing back.
        self.assertLess(generation[0], write_back)
        self.assertLess(write_back, manifest[0])
        self.assertLess(manifest[0], prepare[0])
        self.assertLess(prepare[0], controls[0])
        self.assertFalse(any(call[0] == "cargo" for call in commands))

    def conflict_setup(self):
        """Snapshot patches line 2; upstream also edits line 2, so the rebase conflicts."""
        base = self.product_commit({2: "PATCH SIDE"})
        snapshot = self.snapshot(base, upstream=PREVIOUS, main_base=base)
        upstream = self.upstream_commit(
            {"README.md": self.readme({2: "UPSTREAM SIDE"})}
        )
        plan_value = self.github(self.plan(upstream))
        directory = self.base / "sync-conflict"
        self.assertEqual(
            engine_sync.rebase_plan(
                plan_value, directory, repo=self.repo, cache_dir=self.cache
            ),
            2,
        )
        return snapshot, directory, plan_value

    def test_conflict_writes_a_bundle_and_records_the_status(self):
        _, directory, _ = self.conflict_setup()
        self.assertTrue((directory / "conflicts" / "rebase-report.json").is_file())
        self.assertIn(
            b"<<<<<<<", (directory / "conflicts" / "conflicts.diff").read_bytes()
        )
        conflict = [
            event for event in self.events() if event["kind"] == "sync-conflict"
        ]
        self.assertEqual(len(conflict), 1)
        self.assertIn("sync-resolve", conflict[0]["message"])

    def test_finish_refuses_a_resolution_that_touches_other_paths(self):
        _, directory, plan_value = self.conflict_setup()
        resolution = self.base / "resolution.patch"
        resolution.write_bytes(OTHER_FILE_RESOLUTION)
        with self.assertRaisesRegex(engine.EngineError, "non-conflicted paths"):
            self.finish(
                plan_value,
                directory,
                resolution=resolution,
                resolution_kind="ai:agent/model",
            )

    def test_finish_refuses_unresolved_conflict_markers(self):
        _, directory, plan_value = self.conflict_setup()
        with self.assertRaisesRegex(engine.EngineError, "conflict markers"):
            self.finish(plan_value, directory)

    def test_human_resolution_commits_a_snapshot_with_its_label(self):
        snapshot, directory, plan_value = self.conflict_setup()
        workspace = directory / "rebase" / "workspace"
        (workspace / "README.md").write_bytes(self.readme({2: "HUMAN RESOLVED"}))
        snapshot_path, _ = self.finish(plan_value, directory, resolution_kind="human")
        commit = engine.read_json(snapshot_path)["commit"]
        meta = json.loads(self.read_at(commit, engine_sync.META_PATH))
        self.assertEqual(meta["resolution"], "human")
        self.assertEqual(meta["report"]["totals"]["conflicted_patches"], 1)
        self.assertIn(
            b"HUMAN RESOLVED",
            self.read_at(commit, "integration/codex/patches/documentation.patch"),
        )
        self.assertEqual(
            engine.git(self.repo, "rev-parse", f"{commit}^").decode().strip(), snapshot
        )


class VerifyBundleTest(SyncTest):
    def test_allowlisted_changes_pass_and_other_paths_fail(self):
        base = self.product_commit({2: "PATCH SIDE"})
        parent = self.snapshot(base, upstream=PREVIOUS, main_base=base)
        meta = json.loads(self.read_at(parent, engine_sync.META_PATH))
        allowed = self.commit_on(
            parent,
            {"integration/codex/patches/documentation.patch": b"changed\n"},
            meta,
        )
        self.assertEqual(engine_sync.verify_bundle(self.repo, allowed, parent), [])
        product = self.commit_on(parent, {"VERSION": b"9.9.9\n"}, meta)
        problems = engine_sync.verify_bundle(self.repo, product, parent)
        self.assertTrue(any("VERSION" in problem for problem in problems))

    def test_wrong_parent_is_reported(self):
        base = self.product_commit({2: "PATCH SIDE"})
        parent = self.snapshot(base, upstream=PREVIOUS, main_base=base)
        meta = json.loads(self.read_at(parent, engine_sync.META_PATH))
        child = self.commit_on(
            parent, {"integration/codex/patches/documentation.patch": b"x\n"}, meta
        )
        problems = engine_sync.verify_bundle(self.repo, child, base)
        self.assertTrue(any("parent" in problem for problem in problems))

    def test_product_tree_must_equal_the_declared_main_base(self):
        base = self.product_commit({2: "PATCH SIDE"})
        parent = self.snapshot(base, upstream=PREVIOUS, main_base=base)
        meta = json.loads(self.read_at(parent, engine_sync.META_PATH))
        meta["main_base"] = base
        child = self.commit_on(
            parent, {"integration/codex/patches/documentation.patch": b"x\n"}, meta
        )
        self.assertEqual(
            engine_sync.verify_bundle(self.repo, child, parent, product_base=base), []
        )
        newer = self.product_commit({2: "PATCH SIDE"}, extra={"VERSION": b"0.2.0\n"})
        problems = engine_sync.verify_bundle(
            self.repo, child, parent, product_base=newer
        )
        self.assertTrue(
            any("product content differs" in problem for problem in problems)
        )


DATE = "2026-10-23T18:50:54+00:00"
TAG = "rust-v1.2.3"
UPSTREAM_ONE = "1" * 40
UPSTREAM_TWO = "2" * 40
MERGE_BASE = "3" * 40


def snapshot_meta(upstream, main_base):
    return {
        "schema_version": 1,
        "upstream": {"revision": upstream, "date": DATE},
        "main_base": main_base,
        "from": {"kind": "main", "ref": main_base},
        "report": {"totals": {}, "patches": []},
        "resolution": "none",
        "notes": [],
        "created_at": "2026-10-26T00:00:00Z",
    }


class CandidateTest(SyncTest):
    def stable_plan(self, upstream):
        with patch.object(engine_sync, "upstream_date", return_value=DATE):
            return engine_sync.plan(
                repo=self.repo,
                tag=TAG,
                release_revision=upstream,
                main_ref="main",
                snapshot_ref=engine_sync.BRANCH,
            )

    def build_candidate(self, upstream, *, allow_unresolved=False, name="candidate"):
        plan_value = self.stable_plan(upstream)
        directory = self.base / name
        code = engine_sync.rebase_plan(
            plan_value, directory, repo=self.repo, cache_dir=self.cache
        )
        path = engine_sync.finish(
            plan_value=plan_value,
            directory=directory,
            repo=self.repo,
            runner=FakeRunner(),
            skip_cargo=True,
            allow_unresolved=allow_unresolved,
        )
        return engine.read_json(path), code

    def classify(self, commit, base, runner=None):
        with (
            patch.object(engine_sync, "upstream_date", return_value=DATE),
            patch.object(engine_sync, "compatibility_reasons", return_value=[]),
        ):
            return engine_sync.classify(
                repo=self.repo,
                directory=self.base / f"classify-{commit[:12]}",
                base=base,
                commit=commit,
                tag=TAG,
                runner=runner or FakeRunner(),
                cache_dir=self.cache,
            )

    def candidate_commit(self, base, files, *, tag=TAG, symlink=None):
        """A commit on base that pins the release and changes only the given files."""
        engine.git(self.repo, "checkout", "--quiet", "--detach", base)
        lock = json.loads(engine.git(self.repo, "show", f"{base}:engine.lock"))
        lock.update(release_tag=tag, revision=UPSTREAM_ONE)
        payload = {"engine.lock": (json.dumps(lock, indent=2) + "\n").encode(), **files}
        fixtures.write_files(self.repo, payload)
        if symlink:
            link, target = symlink
            (self.repo / link).parent.mkdir(parents=True, exist_ok=True)
            os.symlink(target, self.repo / link)
        commit = fixtures.commit(self.repo, "candidate")
        engine.git(self.repo, "checkout", "--quiet", "main")
        return commit

    def test_stable_plan_starts_from_the_closest_snapshot_at_or_before_the_release_base(
        self,
    ):
        upstream = self.upstream_commit({})
        base = self.product_commit({2: "PATCH SIDE"})
        older = self.snapshot(base, upstream=UPSTREAM_ONE, main_base=base)
        newer = self.commit_on(
            older,
            {},
            snapshot_meta(UPSTREAM_TWO, base),
            branch=engine_sync.BRANCH,
        )
        with (
            patch.object(engine_sync, "upstream_date", return_value=DATE),
            patch.object(engine_sync, "upstream_merge_base", return_value=MERGE_BASE),
            patch.object(
                engine_sync,
                "upstream_ancestor_or_equal",
                side_effect=lambda ancestor, descendant: ancestor == UPSTREAM_ONE,
            ),
        ):
            value = engine_sync.plan(
                repo=self.repo,
                tag=TAG,
                release_revision=upstream,
                main_ref="main",
                snapshot_ref=engine_sync.BRANCH,
            )
        self.assertEqual(value["from"], {"kind": "snapshot", "ref": older})
        self.assertNotEqual(value["from"]["ref"], newer)
        self.assertEqual(value["candidate"], {"tag": TAG, "revision": upstream})

    def test_stable_plan_falls_back_to_the_newest_snapshot_then_to_main(self):
        upstream = self.upstream_commit({})
        base = self.product_commit({2: "PATCH SIDE"})
        older = self.snapshot(base, upstream=UPSTREAM_ONE, main_base=base)
        newer = self.commit_on(
            older, {}, snapshot_meta(UPSTREAM_TWO, base), branch=engine_sync.BRANCH
        )
        with (
            patch.object(engine_sync, "upstream_date", return_value=DATE),
            patch.object(engine_sync, "upstream_merge_base", return_value=MERGE_BASE),
            patch.object(engine_sync, "upstream_ancestor_or_equal", return_value=False),
        ):
            value = engine_sync.plan(
                repo=self.repo,
                tag=TAG,
                release_revision=upstream,
                main_ref="main",
                snapshot_ref=engine_sync.BRANCH,
            )
        self.assertEqual(value["from"], {"kind": "snapshot", "ref": newer})
        self.assertIn("newest snapshot", value["reason"])

    def test_candidate_is_one_allowlisted_commit_on_main(self):
        upstream = self.upstream_commit({"README.md": self.readme({28: "UPSTREAM 28"})})
        main = self.product_commit({2: "PATCH SIDE"})
        candidate, code = self.build_candidate(upstream)
        self.assertEqual(code, 0)
        self.assertEqual(
            (candidate["schema_version"], candidate["tag"], candidate["prepared"]),
            (2, TAG, True),
        )
        self.assertEqual(
            (candidate["resolution"], candidate["base_sha"]), ("none", main)
        )
        commit = candidate["commit"]
        self.assertEqual(
            engine_sync.import_candidate_bundle(
                self.repo, self.base / "candidate" / candidate["bundle"]
            ),
            commit,
        )
        self.assertEqual(
            engine.git(self.repo, "rev-parse", f"{commit}^").decode().strip(), main
        )
        changed = (
            engine.git(self.repo, "diff", "--name-only", main, commit).decode().split()
        )
        self.assertIn("engine.lock", changed)
        self.assertTrue(all(engine_sync.candidate_allowed(name) for name in changed))
        self.assertEqual(
            json.loads(self.read_at(commit, "engine.lock"))["release_tag"], TAG
        )
        self.assertEqual(
            engine_sync.verify_candidate_commit(self.repo, commit, main, TAG), []
        )

    def test_unresolved_conflicts_draft_an_engine_lock_only_candidate(self):
        base = self.product_commit({2: "PATCH SIDE"})
        upstream = self.upstream_commit(
            {"README.md": self.readme({2: "UPSTREAM SIDE"})}
        )
        candidate, code = self.build_candidate(
            upstream, allow_unresolved=True, name="draft"
        )
        self.assertEqual(code, 2)
        self.assertFalse(candidate["prepared"])
        self.assertTrue(candidate["conflicts"])
        commit = candidate["commit"]
        self.assertEqual(
            engine.git(self.repo, "diff", "--name-only", base, commit).decode().split(),
            ["engine.lock"],
        )
        self.assertEqual(
            engine_sync.verify_candidate_commit(self.repo, commit, base, TAG), []
        )

    def test_candidate_commit_rejects_paths_symlinks_wrong_parents_and_wrong_tags(self):
        base = self.product_commit({2: "PATCH SIDE"})
        good = self.candidate_commit(
            base, {"integration/codex/patches/documentation.patch": b"changed\n"}
        )
        self.assertEqual(
            engine_sync.verify_candidate_commit(self.repo, good, base, TAG), []
        )
        outside = self.candidate_commit(base, {".github/workflows/x.yml": b"x\n"})
        self.assertTrue(
            any(
                "outside the candidate allowlist: .github/workflows/x.yml" in problem
                for problem in engine_sync.verify_candidate_commit(
                    self.repo, outside, base, TAG
                )
            )
        )
        linked = self.candidate_commit(
            base,
            {},
            symlink=("integration/codex/patches/link.patch", "documentation.patch"),
        )
        self.assertTrue(
            any(
                "symlink or submodule" in problem
                for problem in engine_sync.verify_candidate_commit(
                    self.repo, linked, base, TAG
                )
            )
        )
        self.assertTrue(
            any(
                "does not pin" in problem
                for problem in engine_sync.verify_candidate_commit(
                    self.repo, good, base, "rust-v9.9.9"
                )
            )
        )
        stacked = self.candidate_commit(
            good, {"integration/codex/patches/documentation.patch": b"again\n"}
        )
        problems = engine_sync.verify_candidate_commit(self.repo, stacked, base, TAG)
        self.assertTrue(
            any("parent is not the base" in problem for problem in problems)
        )

    def test_bundle_must_carry_exactly_one_candidate_head(self):
        base = self.product_commit({2: "PATCH SIDE"})
        good = self.candidate_commit(
            base, {"integration/codex/patches/documentation.patch": b"x\n"}
        )
        engine.git(self.repo, "update-ref", engine_sync.CANDIDATE_REF, good)
        two_heads = self.base / "two-heads.bundle"
        engine.git(
            self.repo,
            "bundle",
            "create",
            str(two_heads),
            engine_sync.CANDIDATE_REF,
            "main",
        )
        with self.assertRaisesRegex(engine.EngineError, "exactly one candidate head"):
            engine_sync.import_candidate_bundle(self.repo, two_heads)
        single = self.base / "one-head.bundle"
        engine.git(
            self.repo,
            "bundle",
            "create",
            str(single),
            engine_sync.CANDIDATE_REF,
            f"^{base}",
        )
        self.assertEqual(engine_sync.import_candidate_bundle(self.repo, single), good)

    def test_mechanical_when_main_patches_carry_to_exactly_the_candidate_bytes(self):
        self.product_commit({2: "PATCH SIDE"})
        upstream = self.upstream_commit({"README.md": self.readme({28: "UPSTREAM 28"})})
        main = engine.git(self.repo, "rev-parse", "main").decode().strip()
        candidate, _ = self.build_candidate(upstream, name="mechanical")
        result = self.classify(candidate["commit"], main)
        self.assertEqual(result["classification"], "mechanical", result["reasons"])
        self.assertEqual(result["reasons"], [])
        self.assertEqual(
            engine.read_json(
                self.base
                / f"classify-{candidate['commit'][:12]}"
                / "classification.json"
            )["classification"],
            "mechanical",
        )

    def test_assisted_when_the_candidate_bytes_differ_from_the_carry(self):
        main = self.product_commit({2: "PATCH SIDE"})
        upstream = self.upstream_commit({"README.md": self.readme({28: "UPSTREAM 28"})})
        candidate, _ = self.build_candidate(upstream, name="baseline")
        lock_bytes = self.read_at(candidate["commit"], "engine.lock")
        tampered = self.commit_on(
            main,
            {
                "integration/codex/patches/documentation.patch": b"tampered\n",
                "engine.lock": lock_bytes,
            },
        )
        result = self.classify(tampered, main)
        self.assertEqual(result["classification"], "assisted")
        self.assertTrue(
            any("allowlisted paths differ" in reason for reason in result["reasons"])
        )

    def test_conflicting_carry_is_assisted_not_mechanical(self):
        base = self.product_commit({2: "PATCH SIDE"})
        upstream = self.upstream_commit(
            {"README.md": self.readme({2: "UPSTREAM SIDE"})}
        )
        candidate, code = self.build_candidate(
            upstream, allow_unresolved=True, name="conflict"
        )
        self.assertEqual(code, 2)
        result = self.classify(candidate["commit"], base)
        self.assertEqual(result["classification"], "assisted")
        self.assertTrue(any("conflicts" in reason for reason in result["reasons"]))

    def test_rpc_drift_needs_review_and_storage_drift_fails_the_job(self):
        source = self.base / "source"
        fixtures_dir = source / "integration/codex/compatibility"
        fixtures_dir.mkdir(parents=True)
        schema = {"type": "object", "properties": {"method": {"enum": ["a"]}}}
        (fixtures_dir / "rpc.json").write_text(json.dumps({"schema/x.json": schema}))
        (fixtures_dir / "storage.json").write_text(
            json.dumps({"state/db.bin": hashlib.sha256(b"old").hexdigest()})
        )
        workspace = self.base / "generated"
        (workspace / "schema").mkdir(parents=True)
        (workspace / "state").mkdir()
        (workspace / "schema/x.json").write_text(
            json.dumps(
                {"type": "object", "properties": {"method": {"enum": ["a", "b"]}}}
            )
        )
        (workspace / "state/db.bin").write_bytes(b"old")
        reasons = engine_sync.compatibility_reasons(workspace, source)
        self.assertTrue(
            any("changed existing contract" in reason for reason in reasons)
        )
        (workspace / "state/db.bin").write_bytes(b"new")
        with self.assertRaisesRegex(
            engine.EngineError, "durable storage fixtures changed"
        ):
            engine_sync.compatibility_reasons(workspace, source)


if __name__ == "__main__":
    unittest.main()
