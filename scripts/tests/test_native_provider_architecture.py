from __future__ import annotations

import sys
import json
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import native_provider_architecture as architecture


def package(name: str, path: str, dependencies: list[str] | None = None) -> dict:
    return {
        "name": name,
        "id": f"{name} 0.0.0 (path+file://{path})",
        "manifest_path": path,
        "dependencies": [{"name": dependency} for dependency in dependencies or []],
    }


class NativeProviderArchitectureTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        (self.root / "codex-rs/providers/subsystem").mkdir(parents=True)
        (self.root / "codex-rs/providers/adapter").mkdir(parents=True)
        hooks = []
        for hook_id in sorted(architecture.REQUIRED_HOOK_PATHS):
            hook_path = self.root / architecture.REQUIRED_HOOK_PATHS[hook_id]
            hook_path.parent.mkdir(parents=True, exist_ok=True)
            hook_path.write_text("// retained hook fixture\n")
            hooks.append(
                {
                    "id": hook_id,
                    "path": architecture.REQUIRED_HOOK_PATHS[hook_id],
                    "purpose": "test hook",
                    "regression_test_group": "control",
                }
            )
        integration_file = self.root / "codex-rs/integration.rs"
        integration_file.parent.mkdir(parents=True, exist_ok=True)
        integration_file.write_text("fn route_provider() {}\n")
        registry_source = self.root / "codex-rs/providers/registry.rs"
        registry_source.write_text(
            "pub static CURSOR: Descriptor = Descriptor {\n"
            '    id: "cursor",\n};\n'
            "pub static CLAUDE: Descriptor = Descriptor {\n"
            '    id: "claude",\n};\n'
        )
        self.manifest = {
            "schema_version": 1,
            "owned_root": "codex-rs/providers",
            "registry_metadata_source": "codex-rs/providers/registry.rs",
            "subsystem_packages": ["codex-provider-runtime"],
            "adapter_package": "codex-provider-adapter",
            "focused_test_groups": [{"name": "control", "minimum_tests": 1}],
            "integration_hooks": hooks,
            "provider_branch_scan": {
                "paths": ["codex-rs/integration.rs"],
                "scan_roots": ["codex-rs/core/src"],
                "allowed_compatibility_paths": [],
                "provider_ids": ["cursor", "claude"],
                "provider_symbols": ["WireApi::Cursor", "codex_provider_cursor"],
            },
        }
        self.metadata = {
            "packages": [
                package(
                    "codex-provider-runtime",
                    str(self.root / "codex-rs/providers/subsystem/Cargo.toml"),
                ),
                package(
                    "codex-provider-adapter",
                    str(self.root / "codex-rs/providers/adapter/Cargo.toml"),
                    ["codex-config", "codex-provider-runtime"],
                ),
                package("codex-core", str(self.root / "codex-rs/core/Cargo.toml")),
                package("codex-config", str(self.root / "codex-rs/config/Cargo.toml")),
            ]
        }
        self.metadata["workspace_members"] = [
            package["id"] for package in self.metadata["packages"]
        ]

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def test_adapter_may_depend_on_upstream_while_subsystem_stays_owned(self) -> None:
        self.assertEqual(
            architecture.validate(self.metadata, self.manifest, self.root), []
        )

    def test_subsystem_upstream_dependency_is_rejected(self) -> None:
        self.metadata["packages"][0]["dependencies"] = [{"name": "codex-core"}]
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(
            any("owned package codex-provider-runtime" in error for error in errors)
        )
        self.assertTrue(any("reaches codex-core" in error for error in errors))

    def test_adapter_may_not_depend_on_codex_core(self) -> None:
        adapter = next(
            package
            for package in self.metadata["packages"]
            if package["name"] == "codex-provider-adapter"
        )
        adapter["dependencies"] = [{"name": "codex-core"}]
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(any("reaches codex-core" in error for error in errors))

    def test_subsystem_closure_cannot_reach_upstream_through_adapter(self) -> None:
        self.metadata["packages"][0]["dependencies"] = [
            {"name": "codex-provider-adapter"}
        ]
        adapter = next(
            package
            for package in self.metadata["packages"]
            if package["name"] == "codex-provider-adapter"
        )
        adapter["dependencies"] = [{"name": "codex-config"}]
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(
            any("codex-provider-runtime reaches upstream" in error for error in errors)
        )

    def test_new_upstream_workspace_dependency_is_rejected_from_metadata_paths(
        self,
    ) -> None:
        self.metadata["packages"][0]["dependencies"] = [{"name": "codex-config"}]
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(any("codex-config" in error for error in errors))

    def test_newly_named_upstream_workspace_package_is_rejected(self) -> None:
        new_package = package(
            "codex-new-integration",
            str(self.root / "codex-rs/new-integration/Cargo.toml"),
        )
        self.metadata["packages"].append(new_package)
        self.metadata["workspace_members"].append(new_package["id"])
        self.metadata["packages"][0]["dependencies"] = [
            {"name": "codex-new-integration"}
        ]
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(any("codex-new-integration" in error for error in errors))

    def test_third_party_registry_package_outside_workspace_is_allowed(self) -> None:
        third_party = package("tokio", str(self.root / "registry/tokio/Cargo.toml"))
        third_party["source"] = "registry+https://example.invalid"
        self.metadata["packages"].append(third_party)
        runtime = self.metadata["packages"][0]
        runtime["dependencies"] = [
            {
                "name": "tokio",
                "source": "registry+https://example.invalid",
                "kind": None,
            }
        ]
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertFalse(any("tokio" in error for error in errors))

    def test_subsystem_dev_dependency_on_upstream_workspace_package_is_rejected(
        self,
    ) -> None:
        self.metadata["packages"][0]["dependencies"] = [
            {"name": "codex-config", "kind": "dev"}
        ]
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(any("codex-config" in error for error in errors))

    def test_adapter_skips_upstream_dev_only_transitive_test_dependencies(self) -> None:
        models_manager = package(
            "codex-models-manager",
            str(self.root / "codex-rs/models-manager/Cargo.toml"),
        )
        models_manager["dependencies"] = [{"name": "core_test_support", "kind": "dev"}]
        test_support = package(
            "core_test_support",
            str(self.root / "codex-rs/core/tests/common/Cargo.toml"),
        )
        test_support["dependencies"] = [{"name": "codex-core", "kind": None}]
        self.metadata["packages"].extend([models_manager, test_support])
        self.metadata["workspace_members"].extend(
            [models_manager["id"], test_support["id"]]
        )
        adapter = next(
            package
            for package in self.metadata["packages"]
            if package["name"] == "codex-provider-adapter"
        )
        adapter["dependencies"].append({"name": "codex-models-manager", "kind": None})
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertFalse(any("reaches codex-core" in error for error in errors))

    def test_provider_specific_execution_branch_is_rejected(self) -> None:
        (self.root / "codex-rs/integration.rs").write_text(
            'if request\n    .provider\n    == "cursor" {\n    route_cursor();\n}\n'
        )
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(
            any("provider-specific execution branch" in error for error in errors)
        )

    def test_new_production_helper_is_scanned_but_test_code_is_skipped(self) -> None:
        helper = self.root / "codex-rs/core/src/provider_helper.rs"
        helper.write_text(
            "pub fn route(provider_id: &str) {\n"
            '    if provider_id == "claude" { route_claude(); }\n'
            "}\n"
        )
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(
            any("provider-specific execution branch" in error for error in errors)
        )
        helper.write_text(
            "#[cfg(test)]\n"
            "mod tests {\n"
            '    fn test_route() { if provider_id == "claude" { } }\n'
            "}\n"
        )
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertFalse(
            any("provider-specific execution branch" in error for error in errors)
        )
        test_file = self.root / "codex-rs/core/src/provider_helper_tests.rs"
        test_file.write_text('if provider_id == "claude" { route_claude(); }\n')
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertFalse(
            any("provider-specific execution branch" in error for error in errors)
        )

    def test_provider_match_arms_and_multiline_legacy_variants_are_rejected(
        self,
    ) -> None:
        integration = self.root / "codex-rs/integration.rs"
        integration.write_text(
            "match backend.provider_id() {\n"
            '    "claude" => use_claude(),\n'
            "    _ => use_default(),\n}\n"
        )
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(
            any("provider-specific execution branch" in error for error in errors)
        )
        integration.write_text(
            "if matches!(\n"
            "    provider_kind,\n"
            '    "claude"\n'
            ") {\n"
            "    use_claude();\n}\n"
        )
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(
            any("provider-specific execution branch" in error for error in errors)
        )
        integration.write_text(
            "match wire_api {\n"
            "    WireApi\n"
            "        :: Cursor => route_cursor(),\n"
            "    _ => route_default(),\n}\n"
        )
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(
            any("provider-specific execution branch" in error for error in errors)
        )

    def test_new_registry_provider_is_scanned_and_must_be_listed(self) -> None:
        registry = self.root / "codex-rs/providers/registry.rs"
        registry.write_text(
            registry.read_text()
            + "pub static FUTURE: Descriptor = Descriptor {\n"
            + '    id: "future-provider",\n};\n'
        )
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(
            any("must exactly match registry descriptors" in error for error in errors)
        )
        self.manifest["provider_branch_scan"]["provider_ids"].append("future-provider")
        (self.root / "codex-rs/integration.rs").write_text(
            "match backend.provider_id() {\n"
            '    "future-provider" => route_future(),\n'
            "    _ => route_default(),\n}\n"
        )
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(
            any("provider-specific execution branch" in error for error in errors)
        )

    def test_unmanifested_owned_package_is_rejected(self) -> None:
        self.metadata["packages"].append(
            package(
                "codex-provider-accidental",
                str(self.root / "codex-rs/providers/accidental/Cargo.toml"),
            )
        )
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(any("unmanifested package" in error for error in errors))

    def test_manifested_package_outside_owned_root_is_rejected(self) -> None:
        self.metadata["packages"][0]["manifest_path"] = str(
            self.root / "codex-rs/provider-runtime/Cargo.toml"
        )
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(any("outside owned root" in error for error in errors))

    def test_required_integration_hook_cannot_be_dropped_from_manifest(self) -> None:
        self.manifest["integration_hooks"] = [
            hook
            for hook in self.manifest["integration_hooks"]
            if hook["id"] != "client-session-control"
        ]
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(any("client-session-control" in error for error in errors))

    def test_required_integration_hook_cannot_move_to_an_unreviewed_path(self) -> None:
        hook = next(
            hook
            for hook in self.manifest["integration_hooks"]
            if hook["id"] == "client-session-control"
        )
        hook["path"] = "codex-rs/integration.rs"
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(any("must remain at" in error for error in errors))

    def test_hook_regression_coverage_is_required_and_must_name_a_test_group(
        self,
    ) -> None:
        hook = next(
            hook
            for hook in self.manifest["integration_hooks"]
            if hook["id"] == "client-session-control"
        )
        hook.pop("regression_test_group")
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(
            any("missing regression_test_group" in error for error in errors)
        )
        hook["regression_test_group"] = "untracked-test-group"
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(
            any("unknown regression test group" in error for error in errors)
        )
        hook["regression_test_group"] = "control"
        self.manifest["focused_test_groups"][0]["minimum_tests"] = 0
        errors = architecture.validate(self.metadata, self.manifest, self.root)
        self.assertTrue(
            any("must require at least one test" in error for error in errors)
        )


class AssemblyOwnershipTests(unittest.TestCase):
    def test_undeclared_changes_and_missing_regressions_fail(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            assembly = {
                "inventory": "inventory.json",
                "regression_manifest": "tests.json",
                "deletes": [],
                "patches": [],
                "overlays": [],
            }
            entry = {
                "path": "codex-rs/Cargo.lock",
                "owner": "build",
                "purpose": "locked dependencies",
                "regression_groups": ["build"],
            }
            (root / "inventory.json").write_text(json.dumps({"entries": [entry]}))
            (root / "tests.json").write_text(
                json.dumps(
                    {
                        "product_test_groups": {
                            "build": {"checks": ["linked-build"], "minimum_tests": 1}
                        }
                    }
                )
            )
            self.assertEqual(
                architecture.validate_assembly_ownership(root, assembly), []
            )
            assembly["overlays"] = [{"destination": "codex-rs/core/src/unknown.rs"}]
            self.assertTrue(architecture.validate_assembly_ownership(root, assembly))
            assembly["overlays"] = []
            entry["regression_groups"] = ["missing"]
            (root / "inventory.json").write_text(json.dumps({"entries": [entry]}))
            self.assertTrue(architecture.validate_assembly_ownership(root, assembly))


if __name__ == "__main__":
    unittest.main()
