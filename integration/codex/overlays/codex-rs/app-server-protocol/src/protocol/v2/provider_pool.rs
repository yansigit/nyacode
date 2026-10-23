use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ProviderAccountsParams {
    pub provider: String,
    #[serde(default)]
    #[ts(optional)]
    pub thread_id: Option<String>,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "action", rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum ProviderPoolUpdate {
    Enable,
    Disable,
    Enroll { label: String },
    Remove { label: String },
    SetAccountEnabled { label: String, enabled: bool },
    Order { labels: Vec<String> },
    Prefer { label: String },
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ProviderPoolUpdateParams {
    pub provider: String,
    #[serde(default)]
    #[ts(optional)]
    pub thread_id: Option<String>,
    pub revision: u64,
    pub update: ProviderPoolUpdate,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ProviderAccountLogoutParams {
    pub provider: String,
    pub account: String,
    #[serde(default)]
    #[ts(optional)]
    pub thread_id: Option<String>,
    pub revision: u64,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ProviderAccountAvailability {
    pub reason: String,
    pub model: Option<String>,
    pub known_reset_at: Option<u64>,
    pub next_attempt_at: Option<u64>,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ProviderAccountStatus {
    pub label: String,
    pub enrolled: bool,
    pub enabled: bool,
    pub priority: Option<u32>,
    pub credential_condition: String,
    pub availability: Vec<ProviderAccountAvailability>,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ProviderAccountsResponse {
    pub provider: String,
    pub endpoint: String,
    pub revision: u64,
    pub enabled: bool,
    pub explicitly_enabled: Option<bool>,
    pub configured_token: bool,
    pub preferred_account: Option<String>,
    pub effective_account: Option<String>,
    pub accounts: Vec<ProviderAccountStatus>,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ProviderPoolChangedNotification {
    pub provider: String,
    pub endpoint: String,
    pub revision: u64,
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadProviderAccountChangedNotification {
    pub thread_id: String,
    pub provider: String,
    pub previous_account: String,
    pub account: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LoginAccountParams;
    use serde_json::json;

    #[test]
    fn provider_pool_native_login_preserves_existing_client_defaults() {
        let old: LoginAccountParams = serde_json::from_value(
            json!({"type":"native", "provider":"fifth", "accountId":"work"}),
        )
        .unwrap();
        assert!(matches!(
            old,
            LoginAccountParams::Native {
                select: None,
                thread_id: None,
                ..
            }
        ));
        let added: LoginAccountParams = serde_json::from_value(json!({"type":"native", "provider":"fifth", "accountId":"work", "select":false, "threadId":"thread"})).unwrap();
        assert!(matches!(
            added,
            LoginAccountParams::Native {
                select: Some(false),
                thread_id: Some(_),
                ..
            }
        ));
    }
    #[test]
    fn provider_pool_updates_require_a_revision_and_generic_provider() {
        let value = json!({"provider":"fifth", "revision":7, "update":{"action":"setAccountEnabled","label":"work","enabled":false}});
        let request: ProviderPoolUpdateParams = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(request.revision, 7);
        assert_eq!(
            serde_json::to_value(request).unwrap()["update"],
            value["update"]
        );
        assert!(
            serde_json::from_value::<ProviderPoolUpdateParams>(
                json!({"provider":"fifth", "update":{"action":"disable"}})
            )
            .is_err()
        );
    }
}
