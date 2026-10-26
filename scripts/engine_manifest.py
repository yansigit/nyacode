#!/usr/bin/env python3
"""Generate the engine assembly manifest and downstream inventory from ownership metadata.

Source of truth:
- integration/codex/ownership.json: human-maintained defaults, overrides, overlay membership,
  deletes, the patch list, and retired history.
- integration/codex/patches/*.patch: patch paths, deletions and new-file status come from the
  git diff headers.
- Overlay source files: sha256 and mode come from the bytes on disk.
- The committed inventory/downstream.json carries forward the historical source and
  upstream_source hashes. The baseline trees they came from are not vendored, so they cannot be
  recomputed; no validator reads them.

Generated outputs: integration/codex/assembly.json and inventory/downstream.json.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any

import engine
from engine import EngineError, blob, digest, file_mode, located, relative, source_modes

OWNERSHIP = "integration/codex/ownership.json"
ASSEMBLY = "integration/codex/assembly.json"
INVENTORY = "inventory/downstream.json"
PATCH_DIR = "integration/codex/patches"
REQUIRED_KEYS = (
    "artifact_rules",
    "deletes",
    "overlay_roots",
    "overlays",
    "overrides",
    "patches",
    "product_baseline",
    "regression_manifest",
    "replaced_destinations",
    "retired_entries",
    "upstream_base",
)
OVERRIDE_FIELDS = {
    "added_after_baseline",
    "artifact",
    "owner",
    "purpose",
    "regression_groups",
}
DEFAULT_FIELDS = ("owner", "purpose", "regression_groups")
CARRIED_FIELDS = ("source", "upstream_source")
DIFF_LIMIT = 20


def render(value: Any) -> str:
    return json.dumps(value, indent=2, sort_keys=True) + "\n"


def read_ownership(root: Path) -> dict[str, Any]:
    path = located(root, OWNERSHIP)
    if path.is_symlink() or not path.is_file():
        raise EngineError(f"ownership metadata must be a regular file: {OWNERSHIP}")

    def unique(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise EngineError(f"duplicate key in {OWNERSHIP}: {key}")
            result[key] = value
        return result

    value = json.loads(path.read_text(), object_pairs_hook=unique)
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise EngineError(f"unsupported {OWNERSHIP} schema_version")
    missing = [key for key in REQUIRED_KEYS if key not in value]
    if missing:
        raise EngineError(f"{OWNERSHIP} is missing keys: {', '.join(missing)}")
    return value


def longest_prefix(table: dict[str, Any], path: str) -> str | None:
    """Keys ending in "/" match directory prefixes; other keys match a file or directory."""
    best = None
    for prefix in table:
        if prefix.endswith("/"):
            matched = path.startswith(prefix)
        else:
            matched = path == prefix or path.startswith(prefix + "/")
        if matched and (best is None or len(prefix) > len(best)):
            best = prefix
    return best


def checked_prefix(prefix: str) -> None:
    if not prefix.endswith("/"):
        relative(prefix)
    else:
        relative(prefix[:-1])


def is_generated(path: str, rules: dict[str, Any]) -> bool:
    return any(
        fragment in f"/{path}/" for fragment in rules["generated_fragments"]
    ) or any(path.endswith(suffix) for suffix in rules["generated_suffixes"])


def patch_statuses(root: Path, patch: str) -> dict[str, str]:
    """Return {path: "A" | "D" | "M"} for every file a patch touches, from its diff headers."""
    path = located(root, patch)
    if path.is_symlink() or not path.is_file():
        raise EngineError(f"patch must be a regular file: {patch}")
    statuses: dict[str, str] = {}
    current: str | None = None
    in_header = False
    for line in path.read_bytes().decode("utf-8", "surrogateescape").split("\n"):
        if line.startswith("diff --git "):
            match = re.fullmatch(r"diff --git a/(.+) b/(.+)", line)
            if match is None or match.group(1) != match.group(2):
                raise EngineError(f"unsupported diff header in {patch}: {line[:120]!r}")
            current = match.group(1)
            if current in statuses:
                raise EngineError(f"duplicate diff for {current} in {patch}")
            relative(current)
            statuses[current] = "M"
            in_header = True
            continue
        if current is None:
            continue
        if line.startswith(("@@", "GIT binary patch", "Binary files ")):
            in_header = False
        if not in_header:
            continue
        if line.startswith("new file mode "):
            statuses[current] = "A"
        elif line.startswith("deleted file mode "):
            statuses[current] = "D"
        elif line.startswith(("rename from ", "copy from ")):
            raise EngineError(f"renames and copies are not supported in {patch}")
    return statuses


def carried_baseline(root: Path) -> dict[str, dict[str, Any]]:
    """Historical source hashes from the committed inventory, keyed by path."""
    path = located(root, INVENTORY)
    if not path.is_file():
        return {}
    try:
        committed = json.loads(path.read_text())
    except (OSError, ValueError) as error:
        raise EngineError(f"cannot read committed {INVENTORY}: {error}") from error
    return {
        entry["path"]: {field: entry.get(field) for field in CARRIED_FIELDS}
        for entry in committed.get("entries", [])
    }


def build(root: Path) -> tuple[dict[str, Any], dict[str, Any]]:
    """Derive (assembly, inventory) from committed inputs under root."""
    root = root.resolve()
    ownership = read_ownership(root)
    rules = ownership["artifact_rules"]
    lock = engine.read_json(located(root, "engine.lock"))
    assembled_lock = lock["assembled_lock"]
    relative(assembled_lock)
    canonical = source_modes(root)

    # Patches: the ordered list is explicit; paths and statuses come from the diff headers.
    patch_table: dict[str, Any] = ownership["patches"]
    on_disk = sorted(
        path.relative_to(root).as_posix() for path in (root / PATCH_DIR).glob("*.patch")
    )
    if on_disk != sorted(patch_table):
        raise EngineError(
            f"patch files and {OWNERSHIP} patches differ: "
            f"unlisted={sorted(set(on_disk) - set(patch_table))}, "
            f"missing={sorted(set(patch_table) - set(on_disk))}"
        )
    patch_of: dict[str, str] = {}
    status_of: dict[str, str] = {}
    patch_rows = []
    for patch in sorted(patch_table):
        statuses = patch_statuses(root, patch)
        if not statuses:
            raise EngineError(f"patch touches no files: {patch}")
        for name, status in statuses.items():
            if name in patch_of:
                raise EngineError(f"path is patched twice: {name}")
            if status == "A":
                raise EngineError(
                    f"patches may not add files; declare an overlay: {name}"
                )
            if name == "codex-rs/Cargo.lock":
                raise EngineError("Cargo.lock must come only from the assembled lock")
            patch_of[name] = patch
            status_of[name] = status
        patch_rows.append({"path": patch, "paths": sorted(statuses)})

    deletes = ownership["deletes"]
    if len(deletes) != len(set(deletes)):
        raise EngineError(f"duplicate deletes in {OWNERSHIP}")
    for name in deletes:
        relative(name)
    patch_deletes = {name for name, status in status_of.items() if status == "D"}
    if patch_deletes != set(deletes):
        raise EngineError(
            f"deletes must equal patch deletions: patch-only={sorted(patch_deletes - set(deletes))}, "
            f"declared-only={sorted(set(deletes) - patch_deletes)}"
        )

    # Overlays: membership is explicit; sha256 and mode come from the source bytes.
    replaced = ownership["replaced_destinations"]
    if len(replaced) != len(set(replaced)):
        raise EngineError(f"duplicate replaced destinations in {OWNERSHIP}")
    overlay_rows = []
    overlay_of: dict[str, str] = {}
    for destination, source in sorted(ownership["overlays"].items()):
        relative(destination)
        relative(source)
        if (
            destination in patch_of
            or destination in deletes
            or destination == assembled_lock
        ):
            raise EngineError(f"overlay destination collision: {destination}")
        path = located(root, source)
        if not (path.is_file() or path.is_symlink()):
            raise EngineError(f"missing overlay source: {source}")
        mode = canonical.get(source, file_mode(path))
        overlay_rows.append(
            {
                "destination": destination,
                "mode": mode,
                "replace": destination in replaced,
                "sha256": digest(blob(path)),
                "source": source,
            }
        )
        overlay_of[destination] = source
    unknown_replace = sorted(set(replaced) - set(overlay_of))
    if unknown_replace:
        raise EngineError(f"replaced destinations are not overlays: {unknown_replace}")

    # Defaults and overrides.
    for prefix in ownership["overlay_roots"]:
        checked_prefix(prefix)
    overrides: dict[str, dict[str, Any]] = ownership["overrides"]
    lock_path = "codex-rs/Cargo.lock"
    declared = set(patch_of) | set(overlay_of) | set(deletes) | {lock_path}
    for name, override in overrides.items():
        relative(name)
        if name not in declared:
            raise EngineError(f"override for undeclared path: {name}")
        unknown = set(override) - OVERRIDE_FIELDS
        if unknown:
            raise EngineError(
                f"unsupported override fields for {name}: {sorted(unknown)}"
            )

    carried = carried_baseline(root)
    entries = []
    for name in sorted(declared):
        if name == lock_path:
            destination = {"kind": "dependency_override", "path": assembled_lock}
            defaults: dict[str, Any] = {}
            status = "M"
        elif name in overlay_of:
            destination = {"kind": "overlay", "path": name, "source": overlay_of[name]}
            prefix = longest_prefix(ownership["overlay_roots"], name)
            if prefix is None:
                raise EngineError(
                    f"no ownership defaults for overlay {name}; add an overlay_roots entry or an override"
                )
            defaults = ownership["overlay_roots"][prefix]
            status = "A" if name not in replaced else "M"
        else:
            patch = patch_of[name]
            status = status_of[name]
            destination = {
                "kind": "delete" if status == "D" else "patch",
                "patch": patch,
                "path": name,
            }
            defaults = patch_table[patch]
        metadata = {
            field: defaults[field] for field in DEFAULT_FIELDS if field in defaults
        }
        metadata.update(overrides.get(name, {}))
        for field in DEFAULT_FIELDS:
            if not metadata.get(field):
                raise EngineError(f"no {field} for {name}; add it to {OWNERSHIP}")
        derived_artifact = (
            "generated"
            if is_generated(name, rules)
            else "dependency_override"
            if metadata["purpose"] == "dependency_override"
            else "source"
        )
        history = carried.get(name, {})
        source = history.get("source")
        upstream = history.get("upstream_source")
        mode_changed = bool(source and upstream and source["mode"] != upstream["mode"])
        change = {"A": "added", "M": "modified", "D": "deleted"}[status]
        if (
            status == "M"
            and source
            and upstream
            and source["sha256"] == upstream["sha256"]
            and mode_changed
        ):
            change = "mode"
        entry = {
            "artifact": metadata.get("artifact", derived_artifact),
            "change": change,
            "destination": destination,
            "git_status": status,
            "mode_changed": mode_changed,
            "owner": metadata["owner"],
            "path": name,
            "purpose": metadata["purpose"],
            "regression_groups": metadata["regression_groups"],
            "source": source,
            "upstream_source": upstream,
        }
        if "added_after_baseline" in metadata:
            entry["added_after_baseline"] = metadata["added_after_baseline"]
        entries.append(entry)

    assembly = {
        "schema_version": 1,
        "inventory": INVENTORY,
        "regression_manifest": ownership["regression_manifest"],
        "deletes": sorted(deletes),
        "overlays": overlay_rows,
        "patches": patch_rows,
    }
    inventory = {
        "schema_version": 1,
        "upstream_base": ownership["upstream_base"],
        "product_baseline": ownership["product_baseline"],
        "entries": entries,
        "retired_entries": ownership["retired_entries"],
    }
    return assembly, inventory


def differences(
    root: Path, assembly: dict[str, Any], inventory: dict[str, Any]
) -> list[str]:
    """Return a concise report of committed files that differ from the derivation."""
    report: list[str] = []
    paths: list[str] = []
    for name, expected in ((ASSEMBLY, assembly), (INVENTORY, inventory)):
        path = located(root, name)
        if not path.is_file():
            report.append(f"{name}: missing")
            continue
        text = path.read_text()
        if text == render(expected):
            continue
        try:
            committed = json.loads(text)
        except ValueError:
            report.append(f"{name}: not valid JSON")
            continue
        if committed == expected:
            report.append(f"{name}: formatting differs from canonical JSON")
            continue
        if name == ASSEMBLY:
            old = {item["destination"]: item for item in committed.get("overlays", [])}
            new = {item["destination"]: item for item in expected["overlays"]}
            changed = sorted(
                key for key in old.keys() & new.keys() if old[key] != new[key]
            )
            added = sorted(new.keys() - old.keys())
            removed = sorted(old.keys() - new.keys())
            other = [
                key
                for key in committed
                if key != "overlays" and committed.get(key) != expected.get(key)
            ]
            report.append(
                f"{name}: overlays changed={len(changed)} added={len(added)} removed={len(removed)}"
                + (f"; other keys differ: {sorted(other)}" if other else "")
            )
            paths.extend(changed + added + removed)
        else:
            old = {item["path"]: item for item in committed.get("entries", [])}
            new = {item["path"]: item for item in expected["entries"]}
            changed = sorted(
                key for key in old.keys() & new.keys() if old[key] != new[key]
            )
            added = sorted(new.keys() - old.keys())
            removed = sorted(old.keys() - new.keys())
            other = [
                key
                for key in committed
                if key != "entries" and committed.get(key) != expected.get(key)
            ]
            report.append(
                f"{name}: entries changed={len(changed)} added={len(added)} removed={len(removed)}"
                + (f"; other keys differ: {sorted(other)}" if other else "")
            )
            paths.extend(changed + added + removed)
    if paths:
        report.append(f"first {min(DIFF_LIMIT, len(paths))} differing paths:")
        report.extend(f"  {path}" for path in sorted(set(paths))[:DIFF_LIMIT])
    return report


def run(root: Path, *, write: bool) -> int:
    root = root.resolve()
    assembly, inventory = build(root)
    if write:
        for name, value in ((ASSEMBLY, assembly), (INVENTORY, inventory)):
            path = located(root, name)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(render(value))
        print(
            f"wrote {ASSEMBLY} ({len(assembly['overlays'])} overlays, "
            f"{len(assembly['patches'])} patches) and {INVENTORY} ({len(inventory['entries'])} entries)"
        )
        return 0
    report = differences(root, assembly, inventory)
    if report:
        print(
            "engine manifest is out of sync with ownership inputs; "
            "run `python3 scripts/engine.py manifest --write` after reviewing the change",
            file=sys.stderr,
        )
        for line in report:
            print(line, file=sys.stderr)
        return 1
    print(f"engine manifest is in sync: {ASSEMBLY}, {INVENTORY}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=engine.ROOT)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument(
        "--write", action="store_true", help="regenerate assembly and inventory"
    )
    mode.add_argument(
        "--check", action="store_true", help="fail if committed files drift"
    )
    args = parser.parse_args(argv)
    try:
        return run(args.root, write=args.write)
    except (EngineError, OSError, ValueError, KeyError) as error:
        print(f"engine manifest: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
