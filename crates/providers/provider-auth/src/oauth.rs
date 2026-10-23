use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_provider_api::Credentials;
use codex_provider_api::ProviderError;
use codex_provider_api::Result;
use rand::Rng;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::Store;

/// Runtime OAuth configuration. Token parameter values are excluded from Debug.
#[derive(Clone)]
pub struct OAuthConfig {
    pub authorization_url: String,
    pub token_url: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    pub authorization_params: BTreeMap<String, String>,
    pub token_params: BTreeMap<String, String>,
    pub token_json: bool,
    pub exchange_state: bool,
}

impl std::fmt::Debug for OAuthConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthConfig")
            .field("authorization_url", &self.authorization_url)
            .field("token_url", &self.token_url)
            .field("redirect_uri", &self.redirect_uri)
            .field("scopes", &self.scopes)
            .field("token_json", &self.token_json)
            .field("exchange_state", &self.exchange_state)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TokenRecord {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<u64>,
    pub metadata: Value,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountInfo {
    pub provider: String,
    pub account: String,
    pub expires_at: Option<u64>,
    pub revoked: bool,
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub(crate) fn auth_error(message: &str) -> ProviderError {
    ProviderError::Authentication(message.into())
}
pub(crate) fn credential_key(provider: &str, account: &str) -> String {
    serde_json::json!(("credentials", provider, account)).to_string()
}
pub(crate) fn selected_key(provider: &str) -> String {
    serde_json::json!(("selected", provider)).to_string()
}

pub(crate) fn account_identity(account: &str, record: &TokenRecord) -> String {
    match record
        .metadata
        .get("codex_account_generation")
        .and_then(Value::as_str)
    {
        Some(generation) => format!("{account}:{generation}"),
        None => account.to_owned(),
    }
}

/// Explicit credential generations, with selected-account compatibility wrappers.
#[derive(Clone, Debug)]
pub struct AuthStore {
    pub(crate) store: Store,
}
impl AuthStore {
    pub fn new(codex_home: &Path) -> Self {
        Self {
            store: Store::new(codex_home),
        }
    }
    pub fn selected(&self, provider: &str) -> Result<Option<String>> {
        self.store.read(&selected_key(provider))
    }
    pub fn identity(&self, provider: &str) -> Result<Option<String>> {
        let Some(account) = self.selected(provider)? else {
            return Ok(None);
        };
        Ok(self
            .store
            .read::<TokenRecord>(&credential_key(provider, &account))?
            .map(|record| account_identity(&account, &record)))
    }
    pub fn status(&self, provider: &str) -> Result<Option<AccountInfo>> {
        let Some(account) = self.selected(provider)? else {
            return Ok(None);
        };
        Ok(self
            .store
            .read::<TokenRecord>(&credential_key(provider, &account))?
            .map(|record| AccountInfo {
                provider: provider.into(),
                account,
                expires_at: record.expires_at,
                revoked: record.revoked,
            }))
    }
    pub async fn login(
        &self,
        provider: &str,
        account: &str,
        record: TokenRecord,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let operation = self.begin_login(provider, account, cancellation).await?;
        self.commit_login(&operation, record, true, cancellation)
            .await
            .map(|_| ())
    }
    pub async fn logout(&self, provider: &str, cancellation: &CancellationToken) -> Result<()> {
        if let Some(account) = self.selected(provider)? {
            self.logout_account(provider, &account, cancellation)
                .await?;
        }
        Ok(())
    }
    pub async fn credentials(
        &self,
        provider: &str,
        config: Option<&OAuthConfig>,
        cancellation: &CancellationToken,
    ) -> Result<Credentials> {
        let account = self.selected_reference(provider)?;
        self.credentials_for(provider, &account, config, cancellation)
            .await
    }
    pub async fn credentials_for(
        &self,
        provider: &str,
        account: &codex_provider_api::AccountRef,
        config: Option<&OAuthConfig>,
        cancellation: &CancellationToken,
    ) -> Result<Credentials> {
        let config = config.cloned();
        self.credentials_for_with_refresh(
            provider,
            account,
            cancellation,
            move |record, refresh_cancel| async move {
                let config = config
                    .ok_or_else(|| auth_error("provider credentials expired; log in again"))?;
                let refresh = record
                    .refresh_token
                    .as_ref()
                    .ok_or_else(|| auth_error("refresh token missing; log in again"))?;
                let mut fields = config.token_params.clone();
                fields.insert("client_id".into(), config.client_id.clone());
                fields.insert("grant_type".into(), "refresh_token".into());
                fields.insert("refresh_token".into(), refresh.clone());
                let response = token_request(&config, fields, &refresh_cancel).await?;
                let mut updated = parse_token(response, record.metadata)?;
                if updated.refresh_token.is_none() {
                    updated.refresh_token = record.refresh_token;
                }
                Ok(updated)
            },
        )
        .await
    }

    /// A provider-specific refresh uses the same cross-process locking and writeback.
    pub async fn credentials_with_refresh<F, Fut>(
        &self,
        provider: &str,
        cancellation: &CancellationToken,
        refresh: F,
    ) -> Result<Credentials>
    where
        F: FnOnce(TokenRecord, CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<TokenRecord>> + Send + 'static,
    {
        let account = self.selected_reference(provider)?;
        self.credentials_for_with_refresh(provider, &account, cancellation, refresh)
            .await
    }

    pub async fn credentials_for_with_refresh<F, Fut>(
        &self,
        provider: &str,
        reference: &codex_provider_api::AccountRef,
        cancellation: &CancellationToken,
        refresh: F,
    ) -> Result<Credentials>
    where
        F: FnOnce(TokenRecord, CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<TokenRecord>> + Send + 'static,
    {
        let account = reference.label.clone();
        let lock = self
            .store
            .lock(&credential_key(provider, &account), cancellation)
            .await?;
        self.recover_account(provider, &account, &lock, cancellation)
            .await?;
        let mut record = lock
            .read::<TokenRecord>()?
            .ok_or_else(|| auth_error("provider credentials missing"))?;
        if account_identity(&account, &record) != reference.identity() {
            return Err(auth_error(
                "provider account was replaced; select its new generation",
            ));
        }
        if record.revoked {
            return Err(auth_error("provider credentials revoked; log in again"));
        }
        if record
            .metadata
            .get("codex_refresh_pending")
            .and_then(Value::as_bool)
            == Some(true)
        {
            return Err(auth_error(
                "a previous refresh was interrupted; log in again",
            ));
        }
        if record
            .expires_at
            .is_none_or(|expiry| expiry > now().saturating_add(60))
        {
            return Ok(Credentials {
                account_id: account_identity(&account, &record),
                access_token: record.access_token,
                metadata: record.metadata,
            });
        }
        if cancellation.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if !record.metadata.is_object() {
            record.metadata = serde_json::json!({});
        }
        // A process crash or ambiguous transport failure must not retry a possibly
        // consumed refresh token. Keep this marker until the rotation commits.
        record.metadata["codex_refresh_pending"] = Value::Bool(true);
        lock.write(&record)?;
        let refresh_task = tokio::spawn(async move {
            // Only this account's stable credential lock spans the network operation.
            // It outlives cancellation; logout/replacement serialize after its commit.
            match tokio::time::timeout(
                Duration::from_secs(120),
                refresh(record.clone(), CancellationToken::new()),
            )
            .await
            {
                Ok(Ok(mut updated)) => {
                    if !updated.metadata.is_object() {
                        updated.metadata = serde_json::json!({});
                    }
                    updated.metadata["codex_account_generation"] =
                        record.metadata["codex_account_generation"].clone();
                    updated.metadata["codex_refresh_pending"] = Value::Bool(false);
                    lock.write(&updated)?;
                    Ok(Credentials {
                        account_id: account_identity(&account, &updated),
                        access_token: updated.access_token,
                        metadata: updated.metadata,
                    })
                }
                Ok(Err(ProviderError::Authentication(message))) if message == "invalid_grant" => {
                    record.revoked = true;
                    record.metadata["codex_refresh_pending"] = Value::Bool(false);
                    lock.write(&record)?;
                    Err(auth_error("provider credentials revoked; log in again"))
                }
                Ok(Err(error)) => Err(error),
                Err(_) => Err(auth_error("refresh timed out; log in again")),
            }
        });
        tokio::select! {
            result = refresh_task => result.map_err(|_| auth_error("refresh task interrupted; log in again"))?,
            _ = cancellation.cancelled() => Err(ProviderError::Cancelled),
        }
    }
}

pub struct LoginFlow {
    config: OAuthConfig,
    verifier: String,
    state: String,
    started: std::time::Instant,
}
impl LoginFlow {
    pub fn begin(config: OAuthConfig) -> Result<(Self, String)> {
        validate_endpoint(&config.authorization_url)?;
        validate_endpoint(&config.token_url)?;
        let mut random = [0u8; 64];
        rand::rng().fill_bytes(&mut random);
        let verifier = URL_SAFE_NO_PAD.encode(&random[..32]);
        let state = URL_SAFE_NO_PAD.encode(&random[32..]);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = Url::parse(&config.authorization_url)
            .map_err(|_| auth_error("invalid authorization URL"))?;
        url.query_pairs_mut()
            .extend_pairs(&config.authorization_params)
            .append_pair("client_id", &config.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &config.redirect_uri)
            .append_pair("scope", &config.scopes.join(" "))
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);
        Ok((
            Self {
                config,
                verifier,
                state,
                started: std::time::Instant::now(),
            },
            url.into(),
        ))
    }
    pub fn redirect_uri(&self) -> &str {
        &self.config.redirect_uri
    }
    /// Consuming the flow makes callbacks single-use, including invalid callbacks.
    pub async fn finish(
        self,
        callback: &str,
        cancellation: &CancellationToken,
    ) -> Result<TokenRecord> {
        if cancellation.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if self.started.elapsed() > Duration::from_secs(600) {
            return Err(auth_error("login expired"));
        }
        let callback = Url::parse(callback).map_err(|_| auth_error("invalid OAuth callback"))?;
        let redirect = Url::parse(&self.config.redirect_uri)
            .map_err(|_| auth_error("invalid redirect URI"))?;
        if callback.origin() != redirect.origin() || callback.path() != redirect.path() {
            return Err(auth_error("OAuth redirect mismatch"));
        }
        let values: Vec<_> = callback.query_pairs().collect();
        let states: Vec<_> = values
            .iter()
            .filter(|(key, _)| key == "state")
            .map(|(_, value)| value.as_ref())
            .collect();
        if states != [self.state.as_str()] {
            return Err(auth_error("invalid OAuth state"));
        }
        if values.iter().any(|(key, _)| key == "error") {
            return Err(auth_error("authorization was denied"));
        }
        let codes: Vec<_> = values
            .iter()
            .filter(|(key, _)| key == "code")
            .map(|(_, value)| value.to_string())
            .collect();
        if codes.len() != 1 || codes[0].is_empty() {
            return Err(auth_error("missing or repeated authorization code"));
        }
        let mut fields = self.config.token_params.clone();
        fields.insert("client_id".into(), self.config.client_id.clone());
        fields.insert("grant_type".into(), "authorization_code".into());
        fields.insert("redirect_uri".into(), self.config.redirect_uri.clone());
        fields.insert("code_verifier".into(), self.verifier);
        fields.insert("code".into(), codes[0].clone());
        if self.config.exchange_state {
            fields.insert("state".into(), self.state);
        }
        parse_token(
            token_request(&self.config, fields, cancellation).await?,
            Value::Null,
        )
    }
}

fn validate_endpoint(endpoint: &str) -> Result<()> {
    let url = Url::parse(endpoint).map_err(|_| auth_error("invalid OAuth endpoint"))?;
    if url.scheme() != "https"
        && !(url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))
    {
        return Err(auth_error("OAuth endpoints require HTTPS"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(auth_error("OAuth URL must not contain credentials"));
    }
    Ok(())
}
async fn token_request(
    config: &OAuthConfig,
    fields: BTreeMap<String, String>,
    cancellation: &CancellationToken,
) -> Result<Value> {
    validate_endpoint(&config.token_url)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|_| auth_error("unable to create OAuth client"))?;
    let request = client.post(&config.token_url);
    let request = if config.token_json {
        request.json(&fields)
    } else {
        request.form(&fields)
    };
    tokio::select! {
        _ = cancellation.cancelled() => Err(ProviderError::Cancelled),
        result = async {
            let mut response = request.send().await.map_err(|_| auth_error("token endpoint request failed"))?;
            let status = response.status();
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| auth_error("invalid token response"))? {
                if body.len().saturating_add(chunk.len()) > 1024 * 1024 {
                    return Err(auth_error("token response exceeded 1 MiB"));
                }
                body.extend_from_slice(&chunk);
            }
            let value: Value = serde_json::from_slice(&body).map_err(|_| auth_error("invalid token response"))?;
            if !status.is_success() {
                return Err(auth_error(if value.get("error").and_then(Value::as_str) == Some("invalid_grant") { "invalid_grant" } else { "token endpoint rejected credentials" }));
            }
            Ok(value)
        } => result,
    }
}
fn parse_token(value: Value, mut metadata: Value) -> Result<TokenRecord> {
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| auth_error("token response missing access_token"))?
        .to_owned();
    let refresh_token = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let expires_at = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .map(|seconds| now().saturating_add(seconds));
    if !metadata.is_object() {
        metadata = serde_json::json!({});
    }
    for (field, source) in [
        ("account_id", &value["account"]["uuid"]),
        ("organization_id", &value["organization"]["uuid"]),
        ("workspace_id", &value["workspace_id"]),
        ("project_id", &value["project_id"]),
    ] {
        if let Some(id) = source
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control))
        {
            metadata[field] = Value::String(id.into());
        }
    }
    Ok(TokenRecord {
        access_token,
        refresh_token,
        expires_at,
        metadata,
        revoked: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    fn config(base: &str) -> OAuthConfig {
        OAuthConfig {
            authorization_url: format!("{base}/authorize"),
            token_url: format!("{base}/token"),
            client_id: "test".into(),
            redirect_uri: "http://127.0.0.1:53692/callback".into(),
            scopes: vec!["inference".into()],
            authorization_params: BTreeMap::new(),
            token_params: BTreeMap::new(),
            token_json: true,
            exchange_state: true,
        }
    }
    fn record(expired: bool) -> TokenRecord {
        TokenRecord {
            access_token: "old".into(),
            refresh_token: Some("refresh".into()),
            expires_at: expired.then_some(0),
            metadata: json!({}),
            revoked: false,
        }
    }
    #[test]
    fn oauth_debug_omits_application_credentials_and_parameter_values() {
        let mut config = config("https://example.test");
        config.client_id = "fixture-private-client".into();
        config
            .token_params
            .insert("client_secret".into(), "fixture-private-secret".into());
        config
            .authorization_params
            .insert("login_hint".into(), "fixture-private-hint".into());
        let debug = format!("{config:?}");
        assert!(!debug.contains("fixture-private"));
        assert!(debug.contains("OAuthConfig"));
    }

    #[tokio::test]
    async fn invalid_state_and_cancel_do_not_exchange_tokens() {
        let server = MockServer::start().await;
        let (flow, _) = LoginFlow::begin(config(&server.uri())).unwrap();
        assert!(
            flow.finish(
                "http://127.0.0.1:53692/callback?code=x&state=invalid",
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
        let (flow, _) = LoginFlow::begin(config(&server.uri())).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            flow.finish("invalid", &cancel).await,
            Err(ProviderError::Cancelled)
        ));
        assert!(server.received_requests().await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn concurrent_refresh_rotates_once_and_logout_is_isolated() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"access_token":"new","refresh_token":"rotated","expires_in":3600}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        let auth = AuthStore::new(root.path());
        let cancel = CancellationToken::new();
        auth.login("a", "one", record(true), &cancel).await.unwrap();
        auth.login("b", "one", record(false), &cancel)
            .await
            .unwrap();
        let cfg = config(&server.uri());
        let (first, second) = tokio::join!(
            auth.credentials("a", Some(&cfg), &cancel),
            auth.credentials("a", Some(&cfg), &cancel)
        );
        assert_eq!(first.unwrap().access_token, "new");
        assert_eq!(second.unwrap().access_token, "new");
        assert_eq!(
            auth.store
                .read::<TokenRecord>(&credential_key("a", "one"))
                .unwrap()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("rotated")
        );
        auth.logout("a", &cancel).await.unwrap();
        assert!(auth.status("a").unwrap().is_none());
        assert_eq!(
            auth.credentials("b", None, &cancel)
                .await
                .unwrap()
                .access_token,
            "old"
        );
    }
    #[tokio::test]
    async fn revoked_refresh_is_not_repeated() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(400).set_body_json(json!({"error":"invalid_grant"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let root = tempfile::tempdir().unwrap();
        let auth = AuthStore::new(root.path());
        let cancel = CancellationToken::new();
        auth.login("a", "one", record(true), &cancel).await.unwrap();
        let cfg = config(&server.uri());
        assert!(auth.credentials("a", Some(&cfg), &cancel).await.is_err());
        assert!(auth.credentials("a", Some(&cfg), &cancel).await.is_err());
        assert!(auth.status("a").unwrap().unwrap().revoked);
    }
}
