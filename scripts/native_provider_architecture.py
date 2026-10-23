#!/usr/bin/env python3
"""Check native-provider crate ownership and upstream dependency boundaries."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "docs/native-providers/integration-manifest.json"
CARGO_MANIFEST = ROOT / "codex-rs/Cargo.toml"
REQUIRED_HOOK_PATHS = {
    "model-info-registry-and-legacy-enum": "codex-rs/model-provider-info/src/lib.rs",
    "remote-thread-config-proto": "codex-rs/config/src/thread_config/remote.rs",
    "optional-model-provider-facade": "codex-rs/model-provider/src/native.rs",
    "core-provider-bridge": "codex-rs/core/src/native_provider.rs",
    "client-stream-and-auth-routing": "codex-rs/core/src/client.rs",
    "client-session-control": "codex-rs/core/src/client_common.rs",
    "turn-recorded-completion-retry-terminal": "codex-rs/core/src/session/turn.rs",
    "parallel-structured-completion-capture": "codex-rs/core/src/tools/parallel.rs",
    "thread-scoped-model-metadata": "codex-rs/core/src/thread_manager.rs",
    "generic-account-operations-and-ownership-lock": "codex-rs/app-server/src/request_processors/account_processor.rs",
    "thread-provider-inheritance": "codex-rs/app-server/src/request_processors/thread_processor.rs",
    "generic-cli-metadata-choices": "codex-rs/cli/src/provider_cmd.rs",
}


def provider_branch_patterns(provider_ids: list[str], provider_symbols: list[str]):
    literals = "(?:" + "|".join(re.escape(value) for value in provider_ids) + ")"
    variables = r"(?:provider(?:_id|_name|_kind)?|wire_api(?:_type)?|backend|id)"
    quoted_id = r"(?:\"|')" + literals + r"(?:\"|')"
    comparisons = re.compile(
        r"(?is)\b(?:if|else\s+if|while)\b.{0,700}?\b"
        + variables
        + r"\b.{0,300}?(?:==|matches!\s*\().{0,200}?"
        + quoted_id
    )
    reversed_comparisons = re.compile(
        r"(?is)\b(?:if|else\s+if|while)\b.{0,700}?"
        + quoted_id
        + r".{0,200}?(?:==).{0,200}?\b"
        + variables
        + r"\b"
    )
    matches_macro = re.compile(
        r"(?is)matches!\s*\(\s*.{0,200}?\b"
        + variables
        + r"\b.{0,100}?[,)]\s*"
        + quoted_id
    )
    match_arms = re.compile(
        r"(?is)\bmatch\b[^{};]{0,500}?\b"
        + variables
        + r"\b[^{};]{0,300}?\{[^{}]{0,1200}?"
        + quoted_id
        + r"\s*=>"
    )
    symbols = []
    for symbol in provider_symbols:
        if "::" in symbol:
            prefix, variant = symbol.split("::", 1)
            symbols.append(re.escape(prefix) + r"\s*::\s*" + re.escape(variant))
        else:
            symbols.append(re.escape(symbol))
    symbol_pattern = re.compile(r"\b(?:" + "|".join(symbols) + r")\b")
    return comparisons, reversed_comparisons, matches_macro, match_arms, symbol_pattern


def registered_provider_ids(root: Path, manifest: dict[str, Any]) -> set[str]:
    relative_path = manifest.get("registry_metadata_source")
    if not isinstance(relative_path, str):
        return set()
    source_path = root / relative_path
    if not source_path.is_file():
        return set()
    source = source_path.read_text()
    descriptor_pattern = re.compile(
        r"pub\s+static\s+\w+\s*:\s*Descriptor\s*=\s*Descriptor\s*\{(.*?)\n\};",
        re.DOTALL,
    )
    return {
        match.group(1)
        for descriptor in descriptor_pattern.findall(source)
        if (match := re.search(r'\bid\s*:\s*"([^"]+)"', descriptor))
    }


def production_rust_sources(root: Path, scan_roots: list[str]) -> list[Path]:
    sources: set[Path] = set()
    test_file = re.compile(r"(?:^|_)(?:test|tests)\.rs$")
    for relative_root in scan_roots:
        path = root / relative_root
        if path.is_file() and path.suffix == ".rs":
            candidates = [path]
        elif path.is_dir():
            candidates = path.rglob("*.rs")
        else:
            continue
        for candidate in candidates:
            relative = candidate.relative_to(root).as_posix()
            if "/tests/" in f"/{relative}/" or test_file.search(candidate.name):
                continue
            sources.add(candidate)
    return sorted(sources)


def without_inline_test_modules(source: str) -> str:
    """Remove cfg(test) module bodies before scanning production branches."""
    module = re.compile(
        r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*"
        r"(?:pub\s+)?mod\s+\w+\s*\{"
    )
    pieces: list[str] = []
    cursor = 0
    while match := module.search(source, cursor):
        opening = source.find("{", match.start(), match.end())
        depth = 1
        index = opening + 1
        in_string = False
        escaped = False
        line_comment = False
        block_depth = 0
        while index < len(source) and depth:
            char = source[index]
            following = source[index : index + 2]
            if line_comment:
                if char == "\n":
                    line_comment = False
            elif block_depth:
                if following == "/*":
                    block_depth += 1
                    index += 1
                elif following == "*/":
                    block_depth -= 1
                    index += 1
            elif in_string:
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == '"':
                    in_string = False
            elif following == "//":
                line_comment = True
                index += 1
            elif following == "/*":
                block_depth = 1
                index += 1
            elif char == '"':
                in_string = True
            elif char == "{":
                depth += 1
            elif char == "}":
                depth -= 1
            index += 1
        pieces.append(source[cursor : match.start()])
        cursor = index
    pieces.append(source[cursor:])
    return "".join(pieces)


def load_metadata(metadata_path: Path | None) -> dict[str, Any]:
    if metadata_path is not None:
        return json.loads(metadata_path.read_text())
    result = subprocess.run(
        [
            "cargo",
            "metadata",
            "--locked",
            "--manifest-path",
            str(CARGO_MANIFEST),
            "--format-version",
            "1",
        ],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(result.stdout)


def validate_assembly_ownership(root: Path, assembly: dict[str, Any]) -> list[str]:
    """Every engine delta needs an explicit owner and an executable regression group."""
    errors: list[str] = []
    inventory = json.loads((root / assembly["inventory"]).read_text())
    contract = json.loads((root / assembly["regression_manifest"]).read_text())
    groups = contract["product_test_groups"]
    entries = inventory["entries"]
    by_path = {entry["path"]: entry for entry in entries}
    if len(by_path) != len(entries):
        errors.append("downstream inventory contains duplicate paths")
    declared = {"codex-rs/Cargo.lock"}
    declared.update(assembly["deletes"])
    declared.update(item["destination"] for item in assembly["overlays"])
    for patch in assembly["patches"]:
        declared.update(patch["paths"])
    # A new owned module must not silently disappear from the linked product.
    sources = {item.get("source") for item in assembly["overlays"]}
    owned_roots = [
        root / "crates/chatgpt-chat",
        root / "integration/codex/adapter",
        root / "integration/codex/overlays",
        *(root / "crates/providers").glob("provider-*"),
    ]
    for owned in owned_roots:
        for path in owned.rglob("*"):
            if any(part in {"target", "__pycache__", ".git"} for part in path.parts):
                continue
            if (path.is_file() or path.is_symlink()) and path.relative_to(
                root
            ).as_posix() not in sources:
                errors.append(
                    f"owned engine input has no assembly mapping: {path.relative_to(root)}"
                )
    if declared != set(by_path):
        errors.append(
            f"assembly ownership differs from inventory: undeclared={sorted(declared - set(by_path))}, "
            f"unused={sorted(set(by_path) - declared)}"
        )
    for path, entry in by_path.items():
        if not entry.get("owner") or not entry.get("purpose"):
            errors.append(f"engine modification needs an owner and purpose: {path}")
        regressions = entry.get("regression_groups", [])
        if not regressions:
            errors.append(f"engine modification needs a regression group: {path}")
        for name in regressions:
            group = groups.get(name)
            if (
                not group
                or not group.get("checks")
                or group.get("minimum_tests", 0) < 1
            ):
                errors.append(f"unknown or unguarded regression group {name!r}: {path}")
    return errors


def validate(
    metadata: dict[str, Any], manifest: dict[str, Any], root: Path = ROOT
) -> list[str]:
    errors: list[str] = []
    if manifest.get("schema_version") != 1:
        return ["integration manifest has an unsupported schema_version"]

    subsystem = manifest.get("subsystem_packages", [])
    adapter = manifest.get("adapter_package")
    if not isinstance(adapter, str) or not adapter:
        errors.append("integration manifest must name one adapter_package")
    if not subsystem or len(subsystem) != len(set(subsystem)):
        errors.append("subsystem_packages must be a non-empty list without duplicates")
    if adapter in subsystem:
        errors.append("adapter_package must not also appear in subsystem_packages")
    packages = metadata.get("packages", [])
    by_name: dict[str, list[dict[str, Any]]] = {}
    by_id = {package["id"]: package for package in packages}
    for package in packages:
        by_name.setdefault(package["name"], []).append(package)

    owned_root = (root / manifest["owned_root"]).resolve()
    expected_owned = set(subsystem) | ({adapter} if isinstance(adapter, str) else set())
    actual_owned: set[str] = set()
    for package in packages:
        manifest_path = Path(package["manifest_path"]).resolve()
        try:
            manifest_path.relative_to(owned_root)
        except ValueError:
            continue
        actual_owned.add(package["name"])

    for name in sorted(expected_owned - set(by_name)):
        errors.append(f"manifest package is missing from Cargo metadata: {name}")
    for name in sorted(set(by_name) - expected_owned):
        if any(
            Path(package["manifest_path"]).resolve().is_relative_to(owned_root)
            for package in by_name[name]
        ):
            errors.append(f"unmanifested package under owned provider root: {name}")
    for name in sorted(expected_owned - actual_owned):
        if name in by_name:
            errors.append(
                f"provider package is outside owned root {owned_root}: {name}"
            )

    workspace_ids = set(metadata.get("workspace_members", []))
    graph: dict[str, set[tuple[str, str]]] = {
        package["id"]: set() for package in packages
    }
    for package in packages:
        for dependency in package.get("dependencies", []):
            dependency_name = dependency.get("name")
            candidates = [
                candidate
                for candidate in by_name.get(dependency_name, [])
                if candidate["id"] in workspace_ids
            ]
            dependency_path = dependency.get("path")
            if dependency_path:
                resolved_dependency_path = Path(dependency_path).resolve()
                path_candidates = [
                    candidate
                    for candidate in candidates
                    if (
                        Path(candidate["manifest_path"]).resolve()
                        == resolved_dependency_path
                        or Path(candidate["manifest_path"]).resolve().parent
                        == resolved_dependency_path
                    )
                ]
                if path_candidates:
                    candidates = path_candidates
            elif dependency.get("source") is not None:
                candidates = []
            for candidate in candidates:
                graph.setdefault(package["id"], set()).add(
                    (candidate["id"], dependency.get("kind") or "normal")
                )

    # Cargo's resolved nodes include renamed dependencies and package identities;
    # include those edges as well as every declared dependency (including optional
    # and target-specific edges, which may not be active in metadata's resolution).
    for node in (metadata.get("resolve") or {}).get("nodes", []):
        package_id = node.get("id")
        if package_id not in by_id:
            continue
        for dependency in node.get("deps", []):
            dependency_id = dependency.get("pkg")
            if dependency_id in by_id and dependency_id in workspace_ids:
                dep_kinds = dependency.get("dep_kinds") or [{"kind": None}]
                for dep_kind in dep_kinds:
                    graph.setdefault(package_id, set()).add(
                        (dependency_id, dep_kind.get("kind") or "normal")
                    )

    owned_ids = {
        package["id"]
        for package in packages
        if Path(package["manifest_path"]).resolve().is_relative_to(owned_root)
    }

    for root_name in sorted(expected_owned):
        if root_name not in by_name:
            continue
        allowed_upstream = root_name == adapter
        for root_package in by_name[root_name]:
            stack: list[tuple[str, tuple[str, ...]]] = [
                (root_package["id"], (root_package["name"],))
            ]
            visited: set[str] = set()
            while stack:
                package_id, chain = stack.pop()
                if package_id in visited:
                    continue
                visited.add(package_id)
                package = by_id.get(package_id)
                if package is None:
                    continue
                package_name = package["name"]
                is_inside_owned_root = package_id in owned_ids
                if package_name == "codex-core":
                    errors.append(
                        f"owned package {root_name} reaches codex-core through "
                        + " -> ".join(chain)
                    )
                    continue
                if not is_inside_owned_root and not allowed_upstream:
                    errors.append(
                        f"subsystem package {root_name} reaches upstream workspace "
                        f"package {package_name} through " + " -> ".join(chain)
                    )
                    continue
                for dependency_id, dependency_kind in sorted(
                    graph.get(package_id, set())
                ):
                    if not is_inside_owned_root and dependency_kind not in {
                        "normal",
                        "build",
                    }:
                        continue
                    dependency_package = by_id[dependency_id]
                    stack.append((dependency_id, (*chain, dependency_package["name"])))

    branch_scan = manifest.get("provider_branch_scan", {})
    scan_paths = branch_scan.get("paths", [])
    scan_roots = branch_scan.get("scan_roots", [])
    compatibility_paths = set(branch_scan.get("allowed_compatibility_paths", []))
    provider_ids = branch_scan.get("provider_ids", [])
    provider_symbols = list(branch_scan.get("provider_symbols", []))
    provider_symbols.extend(
        f"codex_provider_{provider_id.replace('-', '_')}"
        for provider_id in provider_ids
    )
    if not scan_paths or not scan_roots or not provider_ids or not provider_symbols:
        errors.append(
            "provider_branch_scan must name scan paths, scan_roots, provider_ids, "
            "and provider_symbols"
        )
    source_ids = registered_provider_ids(root, manifest)
    if not source_ids:
        errors.append(
            "registry metadata source is missing or has no provider descriptors"
        )
    elif set(provider_ids) != source_ids:
        errors.append(
            "provider_branch_scan.provider_ids must exactly match registry descriptors; "
            f"declared={sorted(provider_ids)}, registered={sorted(source_ids)}"
        )
    branch_patterns = (
        provider_branch_patterns(provider_ids, provider_symbols)
        if provider_ids and provider_symbols
        else ()
    )
    source_paths = production_rust_sources(root, scan_roots)
    for scan_path in scan_paths:
        path = root / scan_path
        if not path.is_file():
            errors.append(f"provider branch scan path is missing: {scan_path}")
        elif path not in source_paths:
            source_paths.append(path)
    for path in sorted(set(source_paths)):
        scan_path = path.relative_to(root).as_posix()
        if scan_path in compatibility_paths:
            continue
        if not path.is_file():
            errors.append(f"provider branch scan path is missing: {scan_path}")
            continue
        source = without_inline_test_modules(path.read_text())
        if any(pattern.search(source) for pattern in branch_patterns):
            errors.append(
                f"provider-specific execution branch found outside registry: {scan_path}"
            )

    focused_test_groups = {
        group.get("name"): group
        for group in manifest.get("focused_test_groups", [])
        if isinstance(group, dict) and isinstance(group.get("name"), str)
    }
    hook_ids: set[str] = set()
    for hook in manifest.get("integration_hooks", []):
        hook_id = hook.get("id")
        hook_path = hook.get("path")
        if not hook_id or hook_id in hook_ids:
            errors.append(
                f"integration hook has a missing or duplicate id: {hook_id!r}"
            )
        hook_ids.add(hook_id)
        if not isinstance(hook_path, str) or not (root / hook_path).is_file():
            errors.append(f"integration hook path is missing: {hook_path!r}")
        if not hook.get("purpose"):
            errors.append(f"integration hook {hook_id!r} is missing its purpose")
        regression_group = hook.get("regression_test_group")
        if not isinstance(regression_group, str) or not regression_group:
            errors.append(
                f"integration hook {hook_id!r} is missing regression_test_group"
            )
        elif regression_group not in focused_test_groups:
            errors.append(
                f"integration hook {hook_id!r} names unknown regression test group "
                f"{regression_group!r}"
            )
        else:
            minimum_tests = focused_test_groups[regression_group].get("minimum_tests")
            if not isinstance(minimum_tests, int) or minimum_tests < 1:
                errors.append(
                    f"integration hook {hook_id!r} regression test group "
                    f"{regression_group!r} must require at least one test"
                )
    missing_hooks = sorted(set(REQUIRED_HOOK_PATHS) - hook_ids)
    if missing_hooks:
        errors.append(
            "integration manifest is missing required hook(s): "
            + ", ".join(missing_hooks)
        )
    for hook in manifest.get("integration_hooks", []):
        hook_id = hook.get("id")
        expected_path = REQUIRED_HOOK_PATHS.get(hook_id)
        if expected_path and hook.get("path") != expected_path:
            errors.append(
                f"integration hook {hook_id!r} must remain at {expected_path}"
            )

    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--metadata-json", type=Path, help="use saved Cargo metadata")
    parser.add_argument("--manifest", type=Path, default=MANIFEST)
    parser.add_argument("--repo", type=Path, default=ROOT)
    parser.add_argument(
        "--list-test-packages",
        action="store_true",
        help="print manifested provider package names one per line",
    )
    args = parser.parse_args()

    global CARGO_MANIFEST
    CARGO_MANIFEST = args.repo.resolve() / "codex-rs/Cargo.toml"

    manifest = json.loads(args.manifest.read_text())
    if args.list_test_packages:
        for package in manifest["subsystem_packages"] + [manifest["adapter_package"]]:
            print(package)
        return 0

    try:
        metadata = load_metadata(args.metadata_json)
    except (OSError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        print(f"unable to load Cargo metadata: {error}", file=sys.stderr)
        return 2
    errors = validate(metadata, manifest, args.repo.resolve())
    assembly_path = ROOT / "integration/codex/assembly.json"
    if assembly_path.is_file():
        assembly = json.loads(assembly_path.read_text())
        if "inventory" in assembly:
            errors.extend(validate_assembly_ownership(ROOT, assembly))
    if errors:
        print("native provider architecture check failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print("native provider package ownership and dependency boundaries are valid")
    print(
        f"checked {len(manifest['subsystem_packages'])} subsystem packages and adapter"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
