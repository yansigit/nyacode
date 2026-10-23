//! Bounded native catalog metadata. Visibility is not proof of send entitlement.
use crate::ChatError;
use crate::ChatProbe;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Clone)]
pub struct NativeModel {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone)]
pub struct ModelLimit {
    pub model: String,
    pub resets_at_ms: Option<i64>,
}

#[derive(Clone)]
pub struct ModelCatalog {
    pub models: Vec<NativeModel>,
    pub default_model: String,
    /// None means the init response did not supply quota information. An empty
    /// list only means no model limit was reported, never unlimited usage.
    pub limits: Option<Vec<ModelLimit>>,
}

pub(crate) struct ModelPicker {
    pub models: Vec<NativeModel>,
    default_model: Option<String>,
}

impl ModelPicker {
    pub(crate) fn default_model(&self, preferred: &str) -> Result<String, ChatError> {
        [Some(preferred), self.default_model.as_deref()]
            .into_iter()
            .flatten()
            .find(|id| self.models.iter().any(|model| model.id == *id))
            .map(str::to_owned)
            .ok_or(ChatError::UnsupportedCapability)
    }
}

impl ChatProbe {
    pub async fn models(&self) -> Result<Vec<NativeModel>, ChatError> {
        Ok(self.model_picker().await?.models)
    }

    pub(crate) async fn model_picker(&self) -> Result<ModelPicker, ChatError> {
        let value: Value = self
            .read("models?history_and_training_disabled=false", "models")
            .await?;
        let result = ModelPicker {
            models: parse_models(&value)?,
            default_model: value
                .get("default_model_slug")
                .filter(|value| !value.is_null())
                .map(|value| model_id(Some(value)))
                .transpose()?,
        };
        self.check_owner()?;
        Ok(result)
    }
}

pub(crate) fn defaults(value: &Value) -> Result<(String, Option<Vec<ModelLimit>>), ChatError> {
    let model = model_id(value.get("default_model_slug"))?;
    let limits = match value.get("model_limits").filter(|v| !v.is_null()) {
        None => None,
        Some(value) => {
            let entries = value.as_array().ok_or(ChatError::ProtocolChanged)?;
            if entries.len() > 256 {
                return Err(ChatError::ResponseTooLarge);
            }
            let mut seen = HashSet::new();
            let mut limits = Vec::new();
            for entry in entries {
                let model = model_id(entry.get("model_slug"))?;
                if !seen.insert(model.clone()) {
                    return Err(ChatError::ProtocolChanged);
                }
                let resets_at_ms = match entry.get("resets_after").filter(|v| !v.is_null()) {
                    None => None,
                    Some(value) => {
                        let timestamp =
                            text(Some(value), 128)?.ok_or(ChatError::ProtocolChanged)?;
                        let parsed = chrono::DateTime::parse_from_rfc3339(&timestamp)
                            .map_err(|_| ChatError::ProtocolChanged)?
                            .timestamp_millis();
                        if parsed < 0 {
                            return Err(ChatError::ProtocolChanged);
                        }
                        Some(parsed)
                    }
                };
                limits.push(ModelLimit {
                    model,
                    resets_at_ms,
                });
            }
            Some(limits)
        }
    };
    Ok((model, limits))
}

fn parse_models(value: &Value) -> Result<Vec<NativeModel>, ChatError> {
    // The full catalog also includes Work models and internal routing targets.
    // Only picker categories advertise ordinary-chat choices. Missing picker
    // metadata must not silently broaden the list back to the full catalog.
    let mut selectable = HashSet::new();
    for category in entries(value.get("categories"))? {
        selectable.insert(model_id(category.get("default_model"))?);
        for model in entries(category.get("supported_models"))? {
            selectable.insert(model_id(Some(model))?);
        }
    }
    // A disabled version may still be described in picker metadata. An alias
    // shared with an enabled version remains eligible for that enabled version.
    let mut disabled = HashSet::new();
    let mut enabled = HashSet::new();
    if let Some(versions) = value.get("versions").filter(|value| !value.is_null()) {
        for version in entries(Some(versions))? {
            let ids = if version
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or(ChatError::ProtocolChanged)?
            {
                &mut enabled
            } else {
                &mut disabled
            };
            for model in entries(version.get("slugs"))? {
                ids.insert(model_id(Some(model))?);
            }
        }
    }
    selectable.retain(|id| !disabled.contains(id) || enabled.contains(id));
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    for entry in entries(value.get("models"))? {
        let id = model_id(entry.get("slug"))?;
        if !seen.insert(id.clone()) {
            return Err(ChatError::ProtocolChanged);
        }
        let work_mode = entry
            .get("is_work_mode_model")
            .map(|value| value.as_bool().ok_or(ChatError::ProtocolChanged))
            .transpose()?;
        let model = NativeModel {
            id,
            title: text(entry.get("title"), 1024)?,
            description: text(entry.get("description"), 4096)?,
        };
        if work_mode != Some(true) && selectable.contains(&model.id) {
            models.push(model);
        }
    }
    Ok(models)
}

fn entries(value: Option<&Value>) -> Result<&Vec<Value>, ChatError> {
    let entries = value
        .and_then(Value::as_array)
        .ok_or(ChatError::ProtocolChanged)?;
    if entries.len() > 256 {
        return Err(ChatError::ResponseTooLarge);
    }
    Ok(entries)
}

fn model_id(value: Option<&Value>) -> Result<String, ChatError> {
    let id = text(value, 256)?.ok_or(ChatError::ProtocolChanged)?;
    if id.is_empty()
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
    {
        return Err(ChatError::ProtocolChanged);
    }
    Ok(id)
}

fn text(value: Option<&Value>, max: usize) -> Result<Option<String>, ChatError> {
    value
        .filter(|value| !value.is_null())
        .map(|value| {
            let text = value
                .as_str()
                .filter(|text| text.len() <= max && !text.chars().any(char::is_control))
                .ok_or(ChatError::ProtocolChanged)?;
            Ok(text.to_owned())
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn catalog_is_metadata_only_and_does_not_imply_model_entitlement() {
        let value = json!({"categories":[{"default_model":"fixture-model","supported_models":[]}],"models":[{"slug":"fixture-model","title":"Fixture","description":"Metadata","capabilities":{"untrusted":"ignored"},"enabled_tools":["browser"]}]});
        let models = parse_models(&value).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "fixture-model");
        assert_eq!(models[0].title.as_deref(), Some("Fixture"));
        assert_eq!(models[0].description.as_deref(), Some("Metadata"));
        for mut bad in [
            json!({"models":[{"slug":"duplicate"},{"slug":"duplicate"}]}),
            json!({"models":[{"slug":""}]}),
            json!({"models":[{"slug":"okay","title":"control\u{1b}"}]}),
            json!({"models":[{"slug":"okay","description":"x".repeat(4097)}]}),
        ] {
            bad["categories"] = value["categories"].clone();
            assert!(parse_models(&bad).is_err());
        }
    }

    #[test]
    fn picker_excludes_work_models_internal_targets_and_disabled_versions() {
        let value = json!({
            "categories": [
                {"default_model":"chat", "supported_models":["legacy", "work", "disabled", "shared", "unlisted"]},
                {"default_model":"chat", "supported_models":["ordinary-wm"]}
            ],
            "versions": [
                {"enabled":false, "slugs":["disabled", "shared"]},
                {"enabled":true, "slugs":["chat", "shared"]}
            ],
            "models": [
                {"slug":"internal", "is_work_mode_model":false},
                {"slug":"chat", "is_work_mode_model":false},
                {"slug":"work", "is_work_mode_model":true},
                {"slug":"legacy"},
                {"slug":"disabled", "is_work_mode_model":false},
                {"slug":"shared", "is_work_mode_model":false},
                {"slug":"ordinary-wm", "is_work_mode_model":false}
            ]
        });
        assert_eq!(
            parse_models(&value)
                .unwrap()
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["chat", "legacy", "shared", "ordinary-wm"]
        );
    }

    #[test]
    fn absent_empty_or_malformed_picker_metadata_never_exposes_the_full_catalog() {
        let model = json!({"models":[{"slug":"chat"}]});
        for categories in [
            Value::Null,
            json!({}),
            json!([{"default_model":"chat", "supported_models":"all"}]),
            json!([{"default_model":"../invalid", "supported_models":[]}]),
        ] {
            let mut value = model.clone();
            value["categories"] = categories;
            assert!(parse_models(&value).is_err());
        }
        assert!(parse_models(&model).is_err());
        let mut value = model;
        value["categories"] = json!([]);
        assert!(parse_models(&value).unwrap().is_empty());
        value["categories"] = json!([{"default_model":"chat", "supported_models":[]}]);
        value["models"][0]["is_work_mode_model"] = json!("false");
        assert!(parse_models(&value).is_err());
        value["models"][0]["is_work_mode_model"] = json!(false);
        value["versions"] = json!([{"enabled":"false", "slugs":["chat"]}]);
        assert!(parse_models(&value).is_err());
        value["versions"] = json!([]);
        value["categories"] = json!(vec![
            json!({"default_model":"chat", "supported_models":[]});
            257
        ]);
        assert_eq!(
            parse_models(&value).err(),
            Some(ChatError::ResponseTooLarge)
        );
    }

    #[test]
    fn default_stays_inside_the_filtered_picker_without_replacing_explicit_drafts() {
        let picker = ModelPicker {
            models: vec![
                NativeModel {
                    id: "chat".into(),
                    title: None,
                    description: None,
                },
                NativeModel {
                    id: "thinking".into(),
                    title: None,
                    description: None,
                },
            ],
            default_model: Some("chat".into()),
        };
        assert_eq!(picker.default_model("thinking").unwrap(), "thinking");
        assert_eq!(picker.default_model("work").unwrap(), "chat");
        let picker = ModelPicker {
            default_model: Some("work".into()),
            ..picker
        };
        assert_eq!(
            picker.default_model("work").err(),
            Some(ChatError::UnsupportedCapability)
        );
    }

    #[test]
    fn init_defaults_preserve_unknown_quota_and_validate_reset_times() {
        let (default, limits) = defaults(&json!({"default_model_slug":"fixture-model"})).unwrap();
        assert_eq!(default, "fixture-model");
        assert!(limits.is_none());
        assert!(
            defaults(&json!({"default_model_slug":"fixture-model","model_limits":[]}))
                .unwrap()
                .1
                .unwrap()
                .is_empty()
        );
        let (_, limits) = defaults(&json!({"default_model_slug":"fixture-model","model_limits":[{"model_slug":"limited-model","resets_after":"2026-10-12T06:00:00Z"}]})).unwrap();
        assert_eq!(limits.as_ref().unwrap()[0].model, "limited-model");
        assert!(limits.unwrap()[0].resets_at_ms.is_some());
        assert!(defaults(&json!({"default_model_slug":"fixture-model","model_limits":[{"model_slug":"limited-model","resets_after":"private invalid value"}]})).is_err());
    }
}
