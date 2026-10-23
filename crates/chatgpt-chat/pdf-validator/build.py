#!/usr/bin/env python3
"""Rebuild the import-free parser on the pinned toolchain; --check verifies it."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent
    cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo")).resolve()
    with tempfile.TemporaryDirectory(prefix="codex-chat-pdf-") as target:
        env = os.environ.copy()
        # No JS random imports: parsing does not require cryptographic entropy.
        # Any accidental dependency on it returns Unsupported within the guest.
        flags = [
            '--cfg=getrandom_backend="unsupported"',
            f"--remap-path-prefix={root}=/pdf-validator",
            f"--remap-path-prefix={cargo_home}=/cargo",
        ]
        env.pop("RUSTFLAGS", None)
        env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
        env["CARGO_INCREMENTAL"] = "0"
        subprocess.run(
            [
                "cargo",
                "+1.95.0",
                "build",
                "--locked",
                "--release",
                "--target",
                "wasm32-unknown-unknown",
                "--target-dir",
                target,
                "--manifest-path",
                str(root / "Cargo.toml"),
            ],
            cwd=root,
            env=env,
            check=True,
        )
        output = (
            Path(target)
            / "wasm32-unknown-unknown/release/codex_chat_pdf_validator.wasm"
        )
        artifact = root / "validator.wasm"
        inputs = ["Cargo.toml", "Cargo.lock", "src/lib.rs", "build.py"]
        manifest = {
            "version": 1,
            "toolchain": "1.95.0",
            "target": "wasm32-unknown-unknown",
            "artifact_sha256": digest(output),
            "input_sha256": {name: digest(root / name) for name in inputs},
        }
        encoded = json.dumps(manifest, indent=2) + "\n"
        manifest_path = root / "manifest.json"
        if args.check:
            if artifact.read_bytes() != output.read_bytes():
                raise SystemExit(
                    "PDF validator artifact differs; rebuild and review it"
                )
            if manifest_path.read_text() != encoded:
                raise SystemExit(
                    "PDF validator source manifest differs; rebuild and review it"
                )
            print("PDF validator source manifest and Wasm artifact match")
        else:
            artifact.write_bytes(output.read_bytes())
            manifest_path.write_text(encoded)
            print("Wrote PDF validator artifact and source manifest")


if __name__ == "__main__":
    main()
