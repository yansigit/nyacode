import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from engine_compatibility import check, schema_changes


class CompatibilityTests(unittest.TestCase):
    def test_optional_fields_and_named_types_are_additive(self):
        before = {
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "required": ["id"],
        }
        after = {
            **before,
            "properties": {**before["properties"], "label": {"type": "string"}},
        }
        self.assertEqual(schema_changes(before, after), [])
        after["required"] = ["id", "label"]
        self.assertTrue(schema_changes(before, after))

    def test_removed_method_type_or_changed_meaning_fails(self):
        before = {
            "properties": {
                "method": {"const": "thread/start"},
                "id": {"type": "string", "description": "Thread ID"},
            }
        }
        for after in (
            {"properties": {}},
            {
                "properties": {
                    "method": {"const": "thread/start"},
                    "id": {"type": "integer", "description": "Thread ID"},
                }
            },
            {
                "properties": {
                    "method": {"const": "thread/start"},
                    "id": {"type": "string", "description": "Account ID"},
                }
            },
        ):
            self.assertTrue(schema_changes(before, after))

    def test_enum_extensions_are_allowed_but_removals_are_not(self):
        self.assertEqual(schema_changes({"enum": ["a"]}, {"enum": ["a", "b"]}), [])
        self.assertTrue(schema_changes({"enum": ["a", "b"]}, {"enum": ["a"]}))

    def test_regeneration_cannot_approve_changed_storage(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "rpc.json").write_text("{}")
            (root / "migration.sql").write_text("CREATE TABLE saved(id TEXT);")
            expected = hashlib.sha256((root / "migration.sql").read_bytes()).hexdigest()
            (root / "storage.json").write_text(json.dumps({"migration.sql": expected}))
            self.assertEqual(check(root, root), [])
            (root / "migration.sql").write_text("DROP TABLE saved;")
            self.assertTrue(check(root, root))


if __name__ == "__main__":
    unittest.main()
