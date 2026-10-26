"""Generated engine manifest must derive from ownership inputs and stay in sync."""

import contextlib
import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import engine
import engine_manifest

SCRIPTS = Path(__file__).resolve().parents[1]

PATCH = (
    "diff --git a/codex-rs/core.rs b/codex-rs/core.rs\n"
    "index 1111111..2222222 100644\n"
    "--- a/codex-rs/core.rs\n"
    "+++ b/codex-rs/core.rs\n"
    "@@ -1 +1 @@\n"
    "-old\n"
    "+new\n"
)


class ManifestTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.write(
            "engine.lock",
            json.dumps({"assembled_lock": "integration/codex/Cargo.lock"}),
        )
        self.write("integration/codex/patches/core.patch", PATCH)
        self.write("owned/readme.md", "Nyacode\n")
        self.ownership = {
            "schema_version": 1,
            "description": "fixture",
            "product_baseline": "b" * 40,
            "upstream_base": "a" * 40,
            "regression_manifest": "contract.json",
            "artifact_rules": {
                "generated_fragments": ["/generated/"],
                "generated_suffixes": [".snap"],
            },
            "deletes": [],
            "patches": {
                "integration/codex/patches/core.patch": {
                    "owner": "codex-rs",
                    "purpose": "core",
                    "regression_groups": ["core"],
                }
            },
            "overlay_roots": {
                "docs/": {
                    "owner": "downstream-docs",
                    "purpose": "documentation",
                    "regression_groups": ["documentation"],
                }
            },
            "overlays": {"docs/readme.md": "owned/readme.md"},
            "replaced_destinations": [],
            # The assembled lock has no patch or overlay default, so it is always overridden.
            "overrides": {
                "codex-rs/Cargo.lock": {
                    "owner": "coordinator",
                    "purpose": "dependency_override",
                    "regression_groups": ["dependency_build"],
                }
            },
            "retired_entries": [],
        }
        self.save_ownership()

    def write(self, name, data):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data.encode() if isinstance(data, str) else data)
        return path

    def save_ownership(self):
        self.write(
            engine_manifest.OWNERSHIP,
            json.dumps(self.ownership, indent=2, sort_keys=True),
        )

    def run_quietly(self, *, write):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = engine_manifest.run(self.root, write=write)
        return code, out.getvalue() + err.getvalue()

    def read_generated(self):
        return (
            (self.root / engine_manifest.ASSEMBLY).read_bytes(),
            (self.root / engine_manifest.INVENTORY).read_bytes(),
        )

    def entry(self, path):
        inventory = json.loads((self.root / engine_manifest.INVENTORY).read_text())
        return next(item for item in inventory["entries"] if item["path"] == path)

    def test_round_trip_write_then_check_passes(self):
        code, _ = self.run_quietly(write=True)
        self.assertEqual(code, 0)
        code, output = self.run_quietly(write=False)
        self.assertEqual(code, 0, output)
        assembly = json.loads((self.root / engine_manifest.ASSEMBLY).read_text())
        self.assertEqual(assembly["patches"][0]["paths"], ["codex-rs/core.rs"])
        self.assertEqual(assembly["overlays"][0]["sha256"], engine.digest(b"Nyacode\n"))

    def test_new_path_in_patch_gets_patch_default(self):
        self.run_quietly(write=True)
        self.write(
            "integration/codex/patches/core.patch",
            PATCH
            + "diff --git a/codex-rs/extra.rs b/codex-rs/extra.rs\n"
            + "--- a/codex-rs/extra.rs\n+++ b/codex-rs/extra.rs\n@@ -1 +1 @@\n-a\n+b\n",
        )
        code, output = self.run_quietly(write=True)
        self.assertEqual(code, 0, output)
        added = self.entry("codex-rs/extra.rs")
        self.assertEqual(added["owner"], "codex-rs")
        self.assertEqual(added["purpose"], "core")
        self.assertEqual(added["regression_groups"], ["core"])
        self.assertEqual(added["git_status"], "M")
        self.assertEqual(
            added["destination"],
            {
                "kind": "patch",
                "patch": "integration/codex/patches/core.patch",
                "path": "codex-rs/extra.rs",
            },
        )

    def test_overlay_source_edit_fails_check_until_regenerated(self):
        self.run_quietly(write=True)
        self.write("owned/readme.md", "changed\n")
        code, output = self.run_quietly(write=False)
        self.assertEqual(code, 1)
        self.assertIn("overlays changed=1", output)
        self.assertIn("docs/readme.md", output)
        self.assertIn("manifest --write", output)
        code, _ = self.run_quietly(write=True)
        self.assertEqual(code, 0)
        code, _ = self.run_quietly(write=False)
        self.assertEqual(code, 0)

    def test_path_override_applies_over_root_default(self):
        self.ownership["overrides"]["docs/readme.md"] = {"owner": "downstream-product"}
        self.save_ownership()
        self.run_quietly(write=True)
        entry = self.entry("docs/readme.md")
        self.assertEqual(entry["owner"], "downstream-product")
        self.assertEqual(entry["purpose"], "documentation")
        self.assertEqual(entry["regression_groups"], ["documentation"])
        self.assertEqual(entry["artifact"], "source")

    def test_output_is_deterministic_across_writes(self):
        self.run_quietly(write=True)
        first = self.read_generated()
        self.run_quietly(write=True)
        self.assertEqual(self.read_generated(), first)

    def test_committed_baseline_hashes_are_carried_forward(self):
        self.run_quietly(write=True)
        inventory_path = self.root / engine_manifest.INVENTORY
        inventory = json.loads(inventory_path.read_text())
        source = {"mode": "100644", "path": "codex-rs/core.rs", "sha256": "c" * 64}
        upstream = {"mode": "100644", "path": "codex-rs/core.rs", "sha256": "d" * 64}
        for item in inventory["entries"]:
            if item["path"] == "codex-rs/core.rs":
                item["source"], item["upstream_source"] = source, upstream
        inventory_path.write_text(
            json.dumps(inventory, indent=2, sort_keys=True) + "\n"
        )
        code, output = self.run_quietly(write=True)
        self.assertEqual(code, 0, output)
        entry = self.entry("codex-rs/core.rs")
        self.assertEqual(entry["source"], source)
        self.assertEqual(entry["upstream_source"], upstream)
        self.assertFalse(entry["mode_changed"])

    def test_patch_deletions_must_match_declared_deletes(self):
        self.write(
            "integration/codex/patches/core.patch",
            PATCH
            + "diff --git a/codex-rs/gone.rs b/codex-rs/gone.rs\n"
            + "deleted file mode 100644\n--- a/codex-rs/gone.rs\n+++ /dev/null\n",
        )
        with self.assertRaisesRegex(
            engine.EngineError, "deletes must equal patch deletions"
        ):
            engine_manifest.build(self.root)
        self.ownership["deletes"] = ["codex-rs/gone.rs"]
        self.save_ownership()
        assembly, inventory = engine_manifest.build(self.root)
        entry = next(
            item for item in inventory["entries"] if item["path"] == "codex-rs/gone.rs"
        )
        self.assertEqual(entry["git_status"], "D")
        self.assertEqual(entry["change"], "deleted")
        self.assertEqual(assembly["deletes"], ["codex-rs/gone.rs"])

    def test_unlisted_patch_file_is_rejected(self):
        self.write("integration/codex/patches/extra.patch", PATCH)
        with self.assertRaisesRegex(engine.EngineError, "unlisted"):
            engine_manifest.build(self.root)

    def test_overlay_without_root_defaults_is_rejected(self):
        self.ownership["overlays"]["scripts/tool.py"] = "owned/readme.md"
        self.save_ownership()
        with self.assertRaisesRegex(
            engine.EngineError, "no ownership defaults for overlay"
        ):
            engine_manifest.build(self.root)

    def test_cli_dispatches_manifest_check_with_exit_status(self):
        engine_py = SCRIPTS / "engine.py"
        command = [sys.executable, str(engine_py), "--root", str(self.root), "manifest"]
        written = subprocess.run(command + ["--write"], capture_output=True, text=True)
        self.assertEqual(written.returncode, 0, written.stderr)
        checked = subprocess.run(command + ["--check"], capture_output=True, text=True)
        self.assertEqual(checked.returncode, 0, checked.stderr)
        self.write("owned/readme.md", "drift\n")
        drifted = subprocess.run(command + ["--check"], capture_output=True, text=True)
        self.assertEqual(drifted.returncode, 1)
        self.assertIn("overlays changed=1", drifted.stderr)


if __name__ == "__main__":
    unittest.main()
