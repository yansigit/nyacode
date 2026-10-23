#!/usr/bin/env python3
"""Generate the downstream migration inventory and exact engine assembly map."""

import argparse
import hashlib
import json
import os
from pathlib import PurePosixPath
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
UPSTREAM = "ce0323f2f2d3b25e82806cd555237da3db4e1897"
PRODUCT = "b8bbac5e2ad84168eee5a8b08564a910e8006564"
PROVIDERS = "d0b5ef7ad34dde7be10d5ba263583b99277882bb"
SPECIAL = {
    "codex-rs/Cargo.lock": ("coordinator", "dependency_override", "integration/codex/Cargo.lock"),
}


def git(*args, input=None):
    return subprocess.run(["git", *args], cwd=ROOT, input=input, check=True, stdout=subprocess.PIPE).stdout


def safe_path(path):
    p = PurePosixPath(path)
    if p.is_absolute() or ".." in p.parts or "\\" in path or not path:
        raise ValueError(f"unsafe repository path: {path!r}")
    return path


def tree(revision):
    result = {}
    for item in git("ls-tree", "-rz", "--full-tree", revision).split(b"\0"):
        if not item:
            continue
        header, raw_path = item.split(b"\t", 1)
        mode, kind, oid = header.decode("ascii").split()
        path = safe_path(os.fsdecode(raw_path))
        result[path] = {"mode": mode, "oid": oid, "kind": kind}
    return result


def blob_hashes(objects):
    ids = sorted({value["oid"] for value in objects.values() if value["kind"] == "blob"})
    data = git("cat-file", "--batch", input=("\n".join(ids) + "\n").encode())
    cursor = 0
    hashes = {}
    for oid in ids:
        header_end = data.index(b"\n", cursor)
        fields = data[cursor:header_end].decode("ascii").split()
        if fields[:2] != [oid, "blob"]:
            raise ValueError(f"unexpected git object: {fields}")
        size = int(fields[2])
        cursor = header_end + 1
        hashes[oid] = hashlib.sha256(data[cursor:cursor + size]).hexdigest()
        cursor += size + 1
    return hashes


def diff_paths():
    fields = git("diff", "--name-status", "--no-renames", "-z", UPSTREAM, PRODUCT).split(b"\0")
    out = []
    i = 0
    while i < len(fields) and fields[i]:
        status = fields[i].decode("ascii")
        path = safe_path(os.fsdecode(fields[i + 1]))
        out.append((status, path))
        i += 2
    return out


def classify(path):
    if path == "codex-rs/Cargo.lock":
        return SPECIAL[path][0], "dependency_override", "dependency_build"
    if path == ".github/nyacode-logo.svg":
        return "downstream-product", "product_registration_config_branding", "product_registration_config_branding"
    if path in {"MODULE.bazel.lock", "pnpm-lock.yaml", "scripts/uv.lock", "sdk/python/uv.lock"}:
        return "downstream-build", "dependency_override", "dependency_build"
    if path.startswith("codex-rs/providers/provider-adapter/"):
        return "provider_workspace", "chat_integration", "chat_integration"
    if path.startswith("codex-rs/providers/provider-"):
        return "provider_workspace", "native_provider_integration", "native_provider"
    if path.startswith(("docs/", "README", "SECURITY", "CHANGELOG", "CONTRIBUTING")):
        return "downstream-docs", "documentation", "documentation"
    if path.startswith(("sdk/", "codex-rs/app-server-protocol/schema/")):
        return "downstream-sdk", "sdk_protocol", "sdk_protocol"
    if path.startswith((".github/", "bazel/", "patches/", "tools/", "scripts/", "Containerfile", "Dockerfile")) or path in {
        ".bazelrc", ".dockerignore", ".gitattributes", ".gitignore", "MODULE.bazel", "package.json",
        "pnpm-workspace.yaml", "justfile", "codex-rs/Cargo.toml", "codex-cli/package.json",
    }:
        return "downstream-build", "dependency_build", "dependency_build"
    if path.startswith("codex-cli/bin/"):
        return "downstream-product", "product_registration_config_branding", "product_registration_config_branding"
    if path.startswith(("codex-cli/", "codex-rs/")):
        lower = path.lower()
        if "chat" in lower or "chatgpt" in lower:
            return "codex-rs", "chat_integration", "chat_integration"
        if any(word in lower for word in ("tool", "history", "rollout", "persist", "compaction", "response_item")):
            return "codex-rs", "tool_completion_persistence", "tool_completion_persistence"
        if any(word in lower for word in ("thread", "model", "provider", "child", "agent", "routing", "turn")):
            return "codex-rs", "thread_model_child_routing", "thread_model_child_routing"
        if any(word in lower for word in ("brand", "config", "registration", "onboarding", "version", "logo")):
            return "codex-rs", "product_registration_config_branding", "product_registration_config_branding"
        return "codex-rs", "backend_dispatch", "backend_dispatch"
    if path.startswith(("crates/", "integration/")):
        return "provider_workspace", "chat_integration", "chat_integration"
    return "downstream-product", "product_registration_config_branding", "product_registration_config_branding"


def is_generated(path):
    return (
        "/generated/" in f"/{path}/"
        or "/schema/json/" in path
        or "/schema/typescript/" in path
        or path.endswith(".snap")
        or path.endswith(".generated.rs")
    )


def overlay_source(path):
    if path.startswith("codex-rs/providers/provider-adapter/"):
        return "integration/codex/adapter/" + path.removeprefix("codex-rs/providers/provider-adapter/")
    if path.startswith("codex-rs/providers/provider-"):
        return "crates/providers/" + path.removeprefix("codex-rs/providers/")
    if path.startswith("scripts/"):
        return path
    return "integration/codex/overlays/" + path


def output_path(path):
    if path.startswith("codex-rs/providers/provider-adapter/"):
        return path
    return path


def verify_symlink_target(path, target):
    if PurePosixPath(target).is_absolute() or "\\" in target:
        raise ValueError(f"unsafe symlink target at {path}: {target!r}")
    resolved = PurePosixPath(path).parent.joinpath(target)
    parts = []
    for part in resolved.parts:
        if part == "..":
            if not parts:
                raise ValueError(f"symlink escapes repository at {path}: {target!r}")
            parts.pop()
        elif part not in ("", ".", "/"):
            parts.append(part)


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def generate():
    old_tree, new_tree = tree(UPSTREAM), tree(PRODUCT)
    all_hashes = blob_hashes({row["oid"]: row for row in [*old_tree.values(), *new_tree.values()]})
    provider_tree = tree(PROVIDERS)
    provider_files = {
        path: row for path, row in provider_tree.items()
        if row["kind"] == "blob" and (
            path.startswith("integration/codex/adapter/")
            or (path.startswith("crates/providers/provider-") and path.count("/") >= 2)
        )
    }
    provider_hashes = blob_hashes(provider_files)
    baseline = {
        "schema_version": 1,
        "revision": PRODUCT,
        "entries": [
            {"path": p, "mode": row["mode"], "sha256": all_hashes[row["oid"]]}
            for p, row in sorted(new_tree.items()) if row["kind"] == "blob"
        ],
    }
    status_rows = diff_paths()
    if len(status_rows) != 1617:
        raise ValueError(f"expected 1617 changed paths, found {len(status_rows)}")
    groups = {}
    assembly = {"schema_version": 1, "patches": [], "overlays": [], "deletes": []}
    entries = []
    overlay_root = ROOT / "integration/codex/overlays"
    if overlay_root.exists():
        shutil.rmtree(overlay_root)
    for status, path in status_rows:
        old, new = old_tree.get(path), new_tree.get(path)
        owner, purpose, regression = classify(path)
        artifact = "generated" if is_generated(path) else ("dependency_override" if purpose == "dependency_override" else "source")
        source = None if new is None else {"path": path, "sha256": all_hashes[new["oid"]], "mode": new["mode"]}
        upstream_source = None if old is None else {"path": path, "sha256": all_hashes[old["oid"]], "mode": old["mode"]}
        mode_changed = bool(old and new and old["mode"] != new["mode"])
        if status == "M" and old and new and old["oid"] == new["oid"] and mode_changed:
            change = "mode"
        else:
            change = {"A": "added", "M": "modified", "D": "deleted", "T": "type_changed"}[status]
        destination = None
        if path == "codex-rs/Cargo.lock":
            destination = {"kind": "dependency_override", "path": SPECIAL[path][2]}
        elif status == "A":
            destination = {"kind": "overlay", "path": output_path(path), "source": overlay_source(path)}
        elif status in ("M", "D", "T"):
            destination = {"kind": "delete" if status == "D" else "patch", "path": path}
        entry = {
            "path": path, "git_status": status, "change": change, "artifact": artifact,
            "owner": owner, "purpose": purpose, "regression_groups": [regression],
            "source": source, "upstream_source": upstream_source, "mode_changed": mode_changed,
            "destination": destination,
        }
        entries.append(entry)
        if path == "codex-rs/Cargo.lock":
            continue
        if status == "A":
            src = overlay_source(path)
            if path.startswith("codex-rs/providers/provider-adapter/"):
                destination_path = path
            else:
                destination_path = path
            if new["mode"] == "120000":
                verify_symlink_target(path, git("show", f"{PRODUCT}:{path}").decode())
            replace = path in old_tree
            overlay = {"source": src, "destination": destination_path, "replace": replace, "mode": new["mode"]}
            if not src.startswith("scripts/"):
                overlay["sha256"] = source["sha256"]
            assembly["overlays"].append(overlay)
            if not path.startswith("codex-rs/providers/provider-") and not path.startswith("scripts/"):
                target = ROOT / src
                target.parent.mkdir(parents=True, exist_ok=True)
                content = git("cat-file", "blob", new["oid"])
                if new["mode"] == "120000":
                    verify_symlink_target(path, content.decode())
                    target.symlink_to(content.decode())
                else:
                    target.write_bytes(content)
                    target.chmod(0o755 if new["mode"] == "100755" else 0o644)
        elif status in ("M", "D", "T"):
            groups.setdefault(purpose, []).append(path)
            if status == "D":
                assembly["deletes"].append(path)
    for purpose, paths in sorted(groups.items()):
        patch_path = f"integration/codex/patches/{purpose}.patch"
        patch_bytes = git("diff", "--binary", "--no-ext-diff", "--no-renames", UPSTREAM, PRODUCT, "--", *sorted(paths))
        target = ROOT / patch_path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(patch_bytes)
        assembly["patches"].append({"path": patch_path, "paths": sorted(paths)})
        for row in entries:
            if row["path"] in paths:
                row["destination"]["patch"] = patch_path
    provider_inventory = []
    for path, row in sorted(provider_files.items()):
        if path.startswith("integration/codex/adapter/"):
            relative = path.removeprefix("integration/codex/adapter/")
            destination = f"codex-rs/providers/provider-adapter/{relative}"
            owner = "provider_workspace"
            purpose, regression = "chat_integration", "chat_integration"
        else:
            crate_path = path.removeprefix("crates/providers/")
            destination = "codex-rs/providers/" + crate_path
            owner = "provider_workspace"
            purpose, regression = "native_provider_integration", "native_provider"
        provider_inventory.append({
            "path": path, "source_commit": PROVIDERS,
            "source": {"path": path, "sha256": provider_hashes[row["oid"]], "mode": row["mode"]},
            "destination": {"kind": "overlay", "path": destination},
            "owner": owner, "purpose": purpose, "regression_groups": [regression],
        })
    write_json(ROOT / "inventory/baseline-tree.json", baseline)
    write_json(ROOT / "inventory/downstream.json", {
        "schema_version": 1, "upstream_base": UPSTREAM, "product_baseline": PRODUCT,
        "entries": entries,
    })
    write_json(ROOT / "inventory/provider-inputs.json", {
        "schema_version": 1, "source_commit": PROVIDERS, "entries": provider_inventory,
    })
    write_json(ROOT / "integration/codex/assembly.json", assembly)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--generate", action="store_true", help="write inventory, overlays, and patches")
    args = parser.parse_args()
    if args.generate:
        generate()
    else:
        parser.error("pass --generate")
