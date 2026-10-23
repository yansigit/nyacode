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


if __name__ == "__main__":
    unittest.main()
