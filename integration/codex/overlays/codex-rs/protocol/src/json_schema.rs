//! Compatibility transforms for input schemas whose optional fields are omitted.

use schemars::Schema;
use schemars::transform::transform_subschemas;
use serde_json::Value;

/// Schemars 1 preserves field declaration order; required names are a set in
/// JSON Schema. Keep the canonical order used by the published schema fixtures.
pub fn sort_required_fields(schema: &mut Schema) {
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        required.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
    }
    transform_subschemas(&mut sort_required_fields, schema);
}

/// Preserve the input contract used before Schemars 1: optional object fields
/// can be absent without advertising `null` as a second way to omit them.
/// Required nullable fields, named nullable types, and nullable collection items
/// remain nullable.
pub fn omit_optional_null(schema: &mut Schema) {
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        for (name, property) in properties {
            if !required.iter().any(|value| value.as_str() == Some(name)) {
                remove_null_alternative(property);
            }
        }
    }
    transform_subschemas(&mut omit_optional_null, schema);
}

fn remove_null_alternative(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if let Some(Value::Array(types)) = object.get_mut("type") {
        if types.iter().all(|value| value.as_str() == Some("null")) {
            return;
        }
        types.retain(|value| value.as_str() != Some("null"));
        if types.len() == 1 {
            let value = types[0].clone();
            object.insert("type".into(), value);
        }
    }
    if let Some(Value::Array(alternatives)) = object.get_mut("anyOf") {
        if alternatives
            .iter()
            .all(|schema| schema == &serde_json::json!({"type": "null"}))
        {
            return;
        }
        alternatives.retain(|schema| schema != &serde_json::json!({"type": "null"}));
        if alternatives.len() == 1
            && let Some(remaining) = alternatives[0].as_object().cloned()
        {
            object.remove("anyOf");
            for (key, value) in remaining {
                object.entry(key).or_insert(value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_wire_values_remain_inline_primitives() {
        let mut generator = schemars::generate::SchemaSettings::draft07().into_generator();
        for schema in [
            generator.subschema_for::<crate::protocol::GitSha>(),
            generator.subschema_for::<crate::response_item_id::ResponseItemId>(),
            generator.subschema_for::<codex_utils_redacted_string::RedactedString>(),
        ] {
            assert_eq!(schema.as_value()["type"], "string");
            assert!(schema.get("$ref").is_none());
        }
        let amendment = generator.subschema_for::<crate::approvals::ExecPolicyAmendment>();
        assert_eq!(amendment.as_value()["type"], "array");
        assert_eq!(amendment.as_value()["items"]["type"], "string");
    }

    #[test]
    fn optional_inputs_omit_null_without_erasing_explicit_nullable_values() {
        let mut schema = schemars::json_schema!({
            "type": "object",
            "required": ["explicit"],
            "properties": {
                "optional": {"type": ["string", "null"]},
                "reference": {"anyOf": [{"$ref": "#/$defs/Name"}, {"type": "null"}], "description": "name"},
                "explicit": {"type": ["string", "null"]},
                "named": {"$ref": "#/$defs/NullableString"},
                "items": {"type": "array", "items": {"type": ["string", "null"]}},
                "null_only": {"type": ["null"]},
                "null_union": {"anyOf": [{"type": "null"}]},
            },
            "$defs": {"NullableString": {"type": ["string", "null"]}}
        });
        omit_optional_null(&mut schema);
        let value = schema.as_value();
        assert_eq!(value["properties"]["optional"]["type"], "string");
        assert_eq!(
            value["properties"]["null_only"]["type"],
            serde_json::json!(["null"])
        );
        assert_eq!(
            value["properties"]["null_union"]["anyOf"],
            serde_json::json!([{"type": "null"}])
        );
        assert_eq!(
            value["properties"]["reference"],
            serde_json::json!({"$ref": "#/$defs/Name", "description": "name"})
        );
        assert_eq!(
            value["properties"]["explicit"]["type"],
            serde_json::json!(["string", "null"])
        );
        assert_eq!(
            value["properties"]["items"]["items"]["type"],
            serde_json::json!(["string", "null"])
        );
        assert_eq!(
            value["$defs"]["NullableString"]["type"],
            serde_json::json!(["string", "null"])
        );
    }
}
