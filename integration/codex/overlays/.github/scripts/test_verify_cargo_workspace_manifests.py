import copy
import unittest
from unittest.mock import patch

import verify_cargo_workspace_manifests as policy


class ManifestPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.internal_packages = policy.workspace_package_names()

    def check_manifest(self, relative_path, mutate):
        path = policy.ROOT / relative_path
        manifest = copy.deepcopy(policy.load_manifest(path))
        mutate(manifest)
        with patch.object(policy, "load_manifest", return_value=manifest):
            return policy.manifest_errors(
                path, self.internal_packages, set(), set(), set(), set()
            )

    def test_native_provider_manifests_are_allowed(self):
        for path in policy.MANIFEST_FEATURE_EXCEPTIONS:
            with self.subTest(path=path):
                self.assertEqual(self.check_manifest(path, lambda _: None), [])

    def test_unrelated_features_are_rejected(self):
        errors = self.check_manifest(
            "codex-rs/cli/Cargo.toml",
            lambda manifest: manifest.update(features={"unchecked": []}),
        )
        self.assertTrue(any("remove `[features]`" in error for error in errors))

    def test_existing_exception_cannot_gain_a_feature(self):
        errors = self.check_manifest(
            "codex-rs/providers/provider-registry/Cargo.toml",
            lambda manifest: manifest["features"].update(unchecked=[]),
        )
        self.assertTrue(any("limit `[features]`" in error for error in errors))

    def test_existing_exception_cannot_change_dependency_features(self):
        errors = self.check_manifest(
            "codex-rs/providers/provider-adapter/Cargo.toml",
            lambda manifest: manifest["dependencies"]["codex-provider-registry"].update(
                features=["runtime", "provider-cursor"]
            ),
        )
        self.assertTrue(
            any("limit workspace dependency features" in error for error in errors)
        )

    def test_new_optional_dependency_is_rejected(self):
        errors = self.check_manifest(
            "codex-rs/cli/Cargo.toml",
            lambda manifest: manifest["dependencies"]["anyhow"].update(optional=True),
        )
        self.assertTrue(any("remove `optional = true`" in error for error in errors))

    def test_new_default_feature_toggle_is_rejected(self):
        errors = self.check_manifest(
            "codex-rs/cli/Cargo.toml",
            lambda manifest: manifest["dependencies"]["codex-core"].update(
                {"default-features": False}
            ),
        )
        self.assertTrue(
            any("remove `default-features = false`" in error for error in errors)
        )

    def test_removed_exceptions_must_be_deleted(self):
        failures = {}
        policy.add_unused_exception_errors(failures, set(), set(), set(), set())
        messages = "\n".join(error for errors in failures.values() for error in errors)
        for name in (
            "MANIFEST_FEATURE_EXCEPTIONS",
            "OPTIONAL_DEPENDENCY_EXCEPTIONS",
            "INTERNAL_DEPENDENCY_FEATURE_EXCEPTIONS",
            "DEFAULT_FEATURE_EXCEPTIONS",
        ):
            self.assertIn(name, messages)


if __name__ == "__main__":
    unittest.main()
