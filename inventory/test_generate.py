import hashlib
import json
from pathlib import Path
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "inventory"))
import generate


class InventoryTests(unittest.TestCase):
    def test_safe_paths_and_symlink_targets(self):
        for path in ("/absolute", "../escape", "a/../../escape", "a\\b"):
            with self.assertRaises(ValueError):
                generate.safe_path(path)
        for target in ("../../outside", "/etc/passwd", "..\\outside"):
            with self.assertRaises(ValueError):
                generate.verify_symlink_target("nested/link", target)
        generate.verify_symlink_target("nested/link", "../inside")

    def test_inventory_covers_exact_baseline_diff(self):
        manifest = json.loads((ROOT / "inventory/downstream.json").read_text())
        expected = {path: status for status, path in generate.diff_paths()}
        self.assertEqual({row["path"]: row["git_status"] for row in manifest["entries"]}, expected)
        self.assertEqual(len(manifest["entries"]), 1617)
        self.assertTrue(all(row["owner"] and row["purpose"] and row["regression_groups"] for row in manifest["entries"]))

    def test_assembly_sources_and_patches_are_present(self):
        assembly = json.loads((ROOT / "integration/codex/assembly.json").read_text())
        self.assertEqual(len(assembly["overlays"]), 434)
        self.assertEqual(len(assembly["deletes"]), 7)
        for overlay in assembly["overlays"]:
            source = ROOT / overlay["source"]
            if overlay["source"].startswith("scripts/") or overlay["source"] in {
                "Containerfile", "Dockerfile"
            }:
                self.assertTrue(source.is_file(), overlay["source"])
                continue
            if overlay["source"].startswith(("integration/codex/adapter/", "crates/providers/")):
                data = subprocess.run(
                    ["git", "show", f"{generate.PROVIDERS}:{overlay['source']}"],
                    cwd=ROOT, check=True, stdout=subprocess.PIPE,
                ).stdout
            else:
                self.assertTrue(source.is_file() or source.is_symlink(), overlay["source"])
                data = source.readlink().as_posix().encode() if source.is_symlink() else source.read_bytes()
            self.assertEqual(hashlib.sha256(data).hexdigest(), overlay["sha256"], overlay["source"])
        for patch in assembly["patches"]:
            self.assertTrue((ROOT / patch["path"]).is_file(), patch["path"])
            self.assertTrue(patch["paths"])
        provider_inventory = json.loads((ROOT / "inventory/provider-inputs.json").read_text())
        self.assertEqual(len(provider_inventory["entries"]), 86)

    def test_baseline_manifest_hashes_and_modes(self):
        baseline = json.loads((ROOT / "inventory/baseline-tree.json").read_text())
        self.assertEqual(baseline["revision"], generate.PRODUCT)
        self.assertGreater(len(baseline["entries"]), 9000)
        self.assertTrue(all(len(row["sha256"]) == 64 and row["mode"] in {"100644", "100755", "120000"}
                            for row in baseline["entries"]))


if __name__ == "__main__":
    unittest.main()
