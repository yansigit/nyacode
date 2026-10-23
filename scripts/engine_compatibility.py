#!/usr/bin/env python3
"""Check assembled RPC schemas and durable storage against independent fixtures."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]


def schema_changes(previous: Any, current: Any, path: str = "") -> list[str]:
    if type(previous) is not type(current):
        return [f"{path}: schema value changed type"]
    if isinstance(previous, dict):
        errors = [f"{path}/{key}: removed" for key in previous.keys() - current.keys()]
        # Only new named definitions/properties are automatically compatible.
        if path.rsplit("/", 1)[-1] not in {"properties", "$defs", "definitions"}:
            errors.extend(
                f"{path}/{key}: added schema constraint"
                for key in current.keys() - previous.keys()
            )
        for key in previous.keys() & current.keys():
            errors.extend(schema_changes(previous[key], current[key], f"{path}/{key}"))
        return errors
    if isinstance(previous, list) and path.rsplit("/", 1)[-1] in {
        "enum",
        "anyOf",
        "oneOf",
    }:
        unmatched = list(current)
        for value in previous:
            match = next(
                (
                    i
                    for i, item in enumerate(unmatched)
                    if not schema_changes(value, item, path)
                ),
                None,
            )
            if match is None:
                return [f"{path}: removed or changed existing alternative"]
            unmatched.pop(match)
        return []
    if previous != current:
        return [f"{path}: changed existing contract"]
    return []


def check(workspace: Path, fixtures: Path) -> list[str]:
    errors = []
    for relative, previous in json.loads((fixtures / "rpc.json").read_text()).items():
        path = workspace / relative
        if not path.is_file():
            errors.append(f"{relative}: removed RPC schema")
        else:
            errors.extend(
                schema_changes(previous, json.loads(path.read_text()), relative)
            )
    for relative, expected in json.loads(
        (fixtures / "storage.json").read_text()
    ).items():
        path = workspace / relative
        if (
            not path.is_file()
            or hashlib.sha256(path.read_bytes()).hexdigest() != expected
        ):
            errors.append(f"{relative}: changed durable storage input")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, required=True)
    parser.add_argument(
        "--fixtures", type=Path, default=ROOT / "integration/codex/compatibility"
    )
    args = parser.parse_args()
    errors = check(args.workspace, args.fixtures)
    for error in errors:
        print(error)
    if not errors:
        print("RPC and durable-storage compatibility fixtures passed")
    return bool(errors)


if __name__ == "__main__":
    raise SystemExit(main())
