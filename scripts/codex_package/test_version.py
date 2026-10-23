import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from codex_package import version


class ProductVersionTests(unittest.TestCase):
    def test_product_version_precedes_upstream_version(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "codex-rs").mkdir()
            (root / "codex-rs/Cargo.toml").write_text(
                '[workspace.package]\nversion = "0.164.0"\n'
            )
            (root / "VERSION").write_text("1.2.3\n")
            with patch.object(version, "REPO_ROOT", root):
                self.assertEqual(version.read_workspace_version(), "1.2.3")
                (root / "VERSION").write_text("../invalid\n")
                with self.assertRaises(RuntimeError):
                    version.read_workspace_version()

    def test_explicit_product_root_overrides_assembled_engine_root(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            product = root / "product"
            engine = root / "engine"
            product.mkdir()
            engine.mkdir()
            (product / "VERSION").write_text("1.2.3\n")
            (engine / "VERSION").write_text("9.8.7\n")
            with patch.object(version, "REPO_ROOT", engine):
                self.assertEqual(version.read_workspace_version(), "9.8.7")
                self.assertEqual(version.read_workspace_version(product), "1.2.3")


if __name__ == "__main__":
    unittest.main()
