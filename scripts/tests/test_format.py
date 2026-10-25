import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("repo_format", ROOT / "scripts/format.py")
formatter = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = formatter
spec.loader.exec_module(formatter)


class OwnedFormatTests(unittest.TestCase):
    def test_rust_formatter_uses_owned_sources_without_engine_workspace(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "engine.lock").write_text('{"toolchain": "1.95.0"}')
            for name in [
                "crates/provider/src/lib.rs",
                "integration/codex/overlays/helper.rs",
                "codex-rs/old.rs",
                ".engine/workspace/upstream.rs",
                "third_party/rust/aws-config/src/lib.rs",
                "third_party/rust/aws-config/src/login/dpop.rs",
                "third_party/rust/aws-sigv4/src/lib.rs",
                "third_party/rust/aws-sigv4/src/sign/v4a.rs",
                "third_party/rust/custom/src/lib.rs",
            ]:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.touch()
            names = (
                b"\0".join(
                    str(path.relative_to(root)).encode() for path in root.rglob("*.rs")
                )
                + b"\0deleted.rs\0"
            )
            with (
                patch.object(formatter, "REPO_ROOT", root),
                patch.object(formatter.subprocess, "check_output", return_value=names),
            ):
                group = formatter.rust_formatter_group(check=True)
            self.assertIn(
                str(root / "crates/provider/src/lib.rs"), group.commands[0].args
            )
            self.assertIn(
                str(root / "integration/codex/overlays/helper.rs"),
                group.commands[0].args,
            )
            self.assertFalse(
                any(
                    "/codex-rs/" in name or "/.engine/" in name
                    for name in group.commands[0].args
                )
            )
            self.assertIn(
                str(root / "third_party/rust/custom/src/lib.rs"), group.commands[0].args
            )
            self.assertFalse(
                any("/third_party/rust/aws-" in arg for arg in group.commands[0].args)
            )
            self.assertEqual(len(group.commands), 2)
            upstream = group.commands[1]
            self.assertEqual(
                upstream.args[:8],
                (
                    "rustup",
                    "run",
                    "1.95.0",
                    "rustfmt",
                    "--edition",
                    "2021",
                    "--config",
                    "skip_children=true",
                ),
            )
            self.assertIn("--check", upstream.args)
            self.assertEqual(
                set(upstream.args[9:]),
                {
                    str(root / "third_party/rust/aws-config/src/login/dpop.rs"),
                    str(root / "third_party/rust/aws-sigv4/src/sign/v4a.rs"),
                },
            )
            self.assertEqual(upstream.cwd, root)
            self.assertNotIn("deleted.rs", group.commands[0].args)
            self.assertNotIn("--config-path", group.commands[0].args)
            self.assertEqual(group.commands[0].cwd, root)
            self.assertEqual(
                group.commands[0].args[:4], ("rustup", "run", "1.95.0", "rustfmt")
            )

    def test_python_formatter_does_not_walk_ignored_assembled_sources(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "owned.py").touch()
            with (
                patch.object(formatter, "REPO_ROOT", root),
                patch.object(
                    formatter.subprocess,
                    "check_output",
                    return_value=b"owned.py\0deleted.py\0",
                ) as git,
            ):
                group = formatter.python_scripts_formatter_group(check=True)
            self.assertIn("--exclude-standard", git.call_args.args[0])
            self.assertIn("owned.py", group.commands[0].args)
            self.assertNotIn("deleted.py", group.commands[0].args)
            self.assertNotIn(".", group.commands[0].args)


if __name__ == "__main__":
    unittest.main()
