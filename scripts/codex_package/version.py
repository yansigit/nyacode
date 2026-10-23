"""Version discovery for Codex packages."""

import re
from pathlib import Path

from .targets import REPO_ROOT


WORKSPACE_VERSION_PATTERN = re.compile(r'^version\s*=\s*"([^"]+)"')


def read_workspace_version(root: Path | None = None) -> str:
    root = REPO_ROOT if root is None else root
    product_version = root / "VERSION"
    if product_version.is_file():
        version = product_version.read_text(encoding="utf-8").strip()
        if not re.fullmatch(r"\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?", version):
            raise RuntimeError(f"Invalid product version in {product_version}")
        return version
    cargo_toml = root / "codex-rs" / "Cargo.toml"
    in_workspace_package = False
    with open(cargo_toml, encoding="utf-8") as fh:
        for line in fh:
            stripped = line.strip()
            if stripped == "[workspace.package]":
                in_workspace_package = True
                continue

            if in_workspace_package and stripped.startswith("["):
                break

            if in_workspace_package:
                match = WORKSPACE_VERSION_PATTERN.match(stripped)
                if match is not None:
                    return match.group(1)

    raise RuntimeError(f"Could not find [workspace.package].version in {cargo_toml}")
