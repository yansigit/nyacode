"""Generators must run in the assembled workspace, report drift, and write back only declared outputs."""

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import engine
import engine_generate
import engine_manifest

REVISION = "a" * 40
FAKE = {
    "id": "fake",
    "command": "fixture generator",
    "outputs": ["out/*.json"],
    "owner": "downstream-build",
    "purpose": "fixture",
    "regression_groups": ["dependency_build"],
    "deterministic": True,
}


def python(code):
    return [sys.executable, "-c", code]


class GenerateTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.root = self.base / "product"
        self.workspace = self.base / "workspace"
        self.root.mkdir()
        self.workspace.mkdir()
        self.ownership = {"schema_version": 1, "generated": [dict(FAKE)]}
        self.write(
            self.root,
            "engine.lock",
            json.dumps(
                {
                    "schema_version": 1,
                    "repository": "openai/codex",
                    "revision": REVISION,
                    "archive_sha256": "b" * 64,
                    "toolchain": "1.95.0",
                    "integration_contract_version": 1,
                    "assembly_manifest": "integration/codex/assembly.json",
                    "assembled_lock": "integration/codex/Cargo.lock",
                    "product_version_file": "VERSION",
                }
            ),
        )
        self.write(
            self.root, "integration/codex/assembly.json", json.dumps(self.manifest())
        )
        self.save_ownership()
        self.write(
            self.workspace,
            ".git/nyacode/inputs.json",
            json.dumps(
                {
                    "generated_by": "nyacode-engine-v1",
                    "revision": REVISION,
                    "pristine_tree": "0" * 40,
                }
            ),
        )
        self.write(self.workspace, "out/a.json", "old\n")
        self.write(self.workspace, "out/keep.json", "same\n")
        self.write(self.workspace, "out/b.json", "orig b\n")
        self.upstream: dict[str, str] = {}

    @staticmethod
    def manifest():
        return {
            "schema_version": 1,
            "deletes": [],
            "generated": [],
            "overlays": [],
            "patches": [],
        }

    @staticmethod
    def write(base, name, data):
        path = base / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(data)
        return path

    def save_ownership(self):
        self.write(
            self.root, "integration/codex/ownership.json", json.dumps(self.ownership)
        )

    def generate(self, commands, **options):
        """Run generators with upstream bytes injected from self.upstream (path -> bytes)."""
        upstream = {
            name: engine_generate.blob_oid(data.encode())
            for name, data in self.upstream.items()
        }
        with (
            patch.object(engine_generate, "pristine_oids", return_value=upstream),
            patch.object(engine_manifest, "run", return_value=0),
        ):
            return engine_generate.generate(
                root=self.root, workspace=self.workspace, commands=commands, **options
            )

    def test_reports_added_changed_and_removed_outputs_against_the_workspace_baseline(
        self,
    ):
        result = self.generate(
            {
                "fake": [
                    (
                        python(
                            "import pathlib\n"
                            "p = pathlib.Path('out')\n"
                            "(p / 'a.json').write_text('new\\n')\n"
                            "(p / 'c.json').write_text('added\\n')\n"
                        ),
                        ".",
                    )
                ]
            },
            only=["fake"],
        )
        [item] = result["generators"]
        self.assertFalse(item["identical"])
        self.assertEqual(
            item["changed"],
            {"added": ["out/c.json"], "removed": [], "changed": ["out/a.json"]},
        )
        self.assertNotIn("write_back", result)

    def test_identical_output_is_reported_identical(self):
        result = self.generate(
            {"fake": [(python("pass"), ".")]},
            only=["fake"],
        )
        self.assertTrue(result["generators"][0]["identical"])

    def test_failing_generator_reports_exit_status_and_log_tail(self):
        with self.assertRaisesRegex(engine.EngineError, r"(?s)exited 3.*boom"):
            self.generate(
                {"fake": [(python("import sys\nprint('boom')\nsys.exit(3)"), ".")]},
                only=["fake"],
            )
        self.assertTrue((self.workspace / ".git/nyacode/generate/fake-1.log").is_file())

    def test_timed_out_generator_is_reported_with_its_limit(self):
        self.ownership["generated"][0]["timeout_seconds"] = 1
        self.save_ownership()
        with self.assertRaisesRegex(engine.EngineError, r"timed out after 1s"):
            self.generate(
                {"fake": [(python("import time\ntime.sleep(30)"), ".")]},
                only=["fake"],
            )

    def test_unknown_generator_ids_are_rejected_before_anything_runs(self):
        with self.assertRaisesRegex(engine.EngineError, "unknown generator ids"):
            self.generate({}, only=["missing"])

    def test_opt_in_generators_run_only_when_named(self):
        opt_in = dict(FAKE, id="snap", outputs=["snap/*.txt"], opt_in=True)
        self.ownership["generated"].append(opt_in)
        self.save_ownership()
        commands = {
            "fake": [(python("pass"), ".")],
            "snap": [(python("raise SystemExit(9)"), ".")],
        }
        result = self.generate(commands)
        self.assertEqual([item["id"] for item in result["generators"]], ["fake"])
        with self.assertRaisesRegex(engine.EngineError, "exited 9"):
            self.generate(commands, only=["snap"])

    def test_deterministic_selection_skips_non_deterministic_generators(self):
        extra = dict(
            FAKE, id="loose", outputs=["loose/*.json"], deterministic=False, tree=False
        )
        self.ownership["generated"].append(extra)
        self.save_ownership()
        result = self.generate(
            {
                "fake": [(python("pass"), ".")],
                "loose": [(python("raise SystemExit(9)"), ".")],
            },
            deterministic=True,
        )
        self.assertEqual([item["id"] for item in result["generators"]], ["fake"])

    def test_write_back_carries_only_outputs_that_differ_from_upstream(self):
        self.upstream = {
            "out/a.json": "upstream a\n",
            "out/b.json": "orig b\n",
            "out/keep.json": "same\n",
        }
        self.write(self.workspace, "out/stale.json", "stale\n")
        self.write(self.root, "integration/codex/generated/out/stale.json", "stale\n")
        command = python(
            "import pathlib\n"
            "p = pathlib.Path('out')\n"
            "(p / 'a.json').write_text('new\\n')\n"
            "(p / 'stale.json').unlink()\n"
        )
        result = self.generate({"fake": [(command, ".")]}, only=["fake"], write=True)
        tree = self.root / "integration/codex/generated/out"
        self.assertEqual((tree / "a.json").read_text(), "new\n")
        self.assertFalse((tree / "b.json").exists())
        self.assertFalse((tree / "keep.json").exists())
        self.assertFalse((tree / "stale.json").exists())
        write = result["write_back"]
        self.assertEqual(write["copied"], ["integration/codex/generated/out/a.json"])
        self.assertEqual(
            write["removed"], ["integration/codex/generated/out/stale.json"]
        )
        self.assertEqual(write["upstream_identical"], ["out/b.json", "out/keep.json"])

    def test_write_back_refuses_outputs_that_create_upstream_files(self):
        command = python(
            "import pathlib\npathlib.Path('out/new.json').write_text('x')\n"
        )
        with self.assertRaisesRegex(engine.EngineError, "declare an overlay"):
            self.generate({"fake": [(command, ".")]}, only=["fake"], write=True)

    def test_patch_owned_outputs_are_copied_but_leave_the_manifest_to_export(self):
        manifest = self.manifest()
        manifest["patches"] = [
            {"path": "integration/codex/patches/x.patch", "paths": ["out/a.json"]}
        ]
        self.write(self.root, "integration/codex/assembly.json", json.dumps(manifest))
        self.upstream = {
            "out/a.json": "upstream\n",
            "out/b.json": "orig b\n",
            "out/keep.json": "same\n",
        }
        before = (self.root / "integration/codex/assembly.json").read_text()
        result = self.generate(
            {"fake": [(python("pass"), ".")]}, only=["fake"], write=True
        )
        self.assertEqual(result["write_back"]["patch_owned"], ["out/a.json"])
        self.assertIsNone(result["write_back"]["manifest"])
        self.assertEqual(
            (self.root / "integration/codex/assembly.json").read_text(), before
        )
        self.assertTrue(
            (self.root / "integration/codex/generated/out/a.json").is_file()
        )

    def test_generator_removing_an_upstream_output_must_declare_a_delete(self):
        self.write(self.workspace, "out/upstream.json", "upstream\n")
        self.upstream = {"out/upstream.json": "upstream\n"}
        command = python("import pathlib\npathlib.Path('out/upstream.json').unlink()\n")
        with self.assertRaisesRegex(engine.EngineError, "declare a delete"):
            self.generate({"fake": [(command, ".")]}, only=["fake"], write=True)

    def test_workspace_must_be_an_assembled_workspace_of_this_revision(self):
        (self.workspace / ".git/nyacode/inputs.json").write_text(
            json.dumps({"generated_by": "nyacode-engine-v1", "revision": "c" * 40})
        )
        with self.assertRaisesRegex(engine.EngineError, "different engine revision"):
            self.generate({"fake": [(python("pass"), ".")]}, only=["fake"])
        (self.workspace / ".git/nyacode/inputs.json").unlink()
        with self.assertRaisesRegex(
            engine.EngineError, "not an assembled engine workspace"
        ):
            self.generate({"fake": [(python("pass"), ".")]}, only=["fake"])

    def test_cli_dispatches_generate_with_injected_commands(self):
        report = self.base / "report.json"
        with (
            patch.object(
                engine_generate,
                "commands_for",
                return_value=[(python("pass"), ".")],
            ) as commands_for,
            patch.object(
                sys,
                "argv",
                [
                    "engine.py",
                    "--root",
                    str(self.root),
                    "generate",
                    "--workspace",
                    str(self.workspace),
                    "--only",
                    "fake",
                    "--report",
                    str(report),
                ],
            ),
        ):
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(engine.main(), 0)
        commands_for.assert_called_once()
        self.assertEqual(commands_for.call_args.args[0], "fake")
        self.assertEqual(json.loads(report.read_text())["generators"][0]["id"], "fake")

    def test_snapshots_need_crates_and_run_insta_per_crate(self):
        with self.assertRaisesRegex(engine.EngineError, "needs at least one --crate"):
            engine_generate.commands_for("snapshots", python=sys.executable, crates=[])
        commands = engine_generate.commands_for(
            "snapshots", python=sys.executable, crates=["codex-cli", "codex-tui"]
        )
        self.assertEqual([argv[-1] for argv, _ in commands], ["codex-cli", "codex-tui"])
        self.assertIn("--unreferenced=delete", commands[0][0])

    def test_schema_generator_runs_stable_then_experimental_without_prettier(self):
        commands = engine_generate.commands_for(
            "app-server-schema", python="py", crates=[]
        )
        self.assertEqual(
            [argv for argv, _ in commands],
            [
                ["py", engine_generate.SCHEMA_SCRIPT],
                ["py", engine_generate.SCHEMA_SCRIPT, "--experimental"],
            ],
        )

    def test_glob_patterns_match_whole_repo_paths(self):
        regex = engine_manifest.glob_regex("codex-rs/**/*.snap")
        self.assertTrue(regex.fullmatch("codex-rs/a/b/c.snap"))
        self.assertTrue(regex.fullmatch("codex-rs/c.snap"))
        self.assertFalse(regex.fullmatch("codex-rs/a/c.snap.new"))
        self.assertFalse(regex.fullmatch("other/c.snap"))
        single = engine_manifest.glob_regex("out/*.json")
        self.assertFalse(single.fullmatch("out/deep/a.json"))

    def test_generator_declarations_are_validated(self):
        with self.assertRaisesRegex(engine.EngineError, "duplicate generator id"):
            engine_manifest.read_generators({"generated": [FAKE, dict(FAKE)]})
        with self.assertRaisesRegex(engine.EngineError, "must be relative"):
            engine_manifest.read_generators(
                {"generated": [dict(FAKE, outputs=["/abs/*.json"])]}
            )
        with self.assertRaisesRegex(engine.EngineError, "unsupported generator fields"):
            engine_manifest.read_generators({"generated": [dict(FAKE, surprise=1)]})
        with self.assertRaisesRegex(engine.EngineError, "must be a boolean"):
            engine_manifest.read_generators({"generated": [dict(FAKE, opt_in="yes")]})

    def test_overlapping_generator_outputs_are_rejected(self):
        generators = engine_manifest.read_generators(
            {"generated": [FAKE, dict(FAKE, id="other", outputs=["out/a.json"])]}
        )
        with self.assertRaisesRegex(engine.EngineError, "several generators"):
            engine_manifest.generator_of("out/a.json", generators)


if __name__ == "__main__":
    unittest.main()
