use codex_provider_api::Credentials;
use codex_provider_api::Provider;
use codex_provider_api::ProviderError;
use codex_provider_api::ProviderFuture;
use codex_provider_api::Result;
use codex_provider_auth::AuthStore;
use codex_provider_auth::OAuthConfig;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct TransportOptions<'a> {
    pub base_url: Option<&'a str>,
    pub state_home: Option<&'a Path>,
}
pub type TransportFactory = fn(TransportOptions<'_>) -> Result<Arc<dyn Provider>>;

pub(crate) fn not_compiled(id: &str) -> ProviderError {
    ProviderError::Unsupported(format!("provider {id} was not compiled into this binary"))
}
impl crate::Descriptor {
    pub fn provider(&self, options: TransportOptions<'_>) -> Result<Arc<dyn Provider>> {
        self.factory.ok_or_else(|| not_compiled(self.id))?(options)
    }
    pub fn credentials<'a>(
        &'static self,
        store: &'a AuthStore,
        cancellation: &'a CancellationToken,
    ) -> ProviderFuture<'a, Credentials> {
        Box::pin(async move {
            let account = store.selected_reference(self.id)?;
            self.credentials_for(store, &account, cancellation).await
        })
    }
    pub fn credentials_for<'a>(
        &'static self,
        store: &'a AuthStore,
        account: &'a codex_provider_api::AccountRef,
        cancellation: &'a CancellationToken,
    ) -> ProviderFuture<'a, Credentials> {
        match self.auth_driver {
            Some(driver) => driver.credentials(self.id, store, account, cancellation),
            None => Box::pin(async move { Err(not_compiled(self.id)) }),
        }
    }
}

#[derive(Debug)]
pub enum AuthDriver {
    ApiKey,
    OAuth(fn() -> Result<OAuthConfig>),
    #[cfg(feature = "provider-cursor")]
    Cursor,
}
impl AuthDriver {
    fn oauth_config(&self) -> Result<Option<OAuthConfig>> {
        match self {
            Self::OAuth(config) => config().map(Some),
            _ => Ok(None),
        }
    }
    fn credentials<'a>(
        &self,
        id: &'static str,
        store: &'a AuthStore,
        account: &'a codex_provider_api::AccountRef,
        cancellation: &'a CancellationToken,
    ) -> ProviderFuture<'a, Credentials> {
        let config = self.oauth_config();
        #[cfg(feature = "provider-cursor")]
        if matches!(self, Self::Cursor) {
            return Box::pin(async move {
                store
                    .credentials_for_with_refresh(
                        id,
                        account,
                        cancellation,
                        |record, refresh_cancel| async move {
                            let refresh = record.refresh_token.ok_or_else(|| {
                                ProviderError::Authentication("Cursor refresh token missing".into())
                            })?;
                            let auth = codex_provider_cursor::auth::AuthClient::new(
                                "https://api2.cursor.sh",
                            )?
                            .refresh_auth(refresh, refresh_cancel)
                            .await?;
                            Ok(codex_provider_auth::TokenRecord {
                                expires_at: codex_provider_cursor::auth::token_expires_at(
                                    &auth.access_token,
                                ),
                                access_token: auth.access_token,
                                refresh_token: auth.refresh_token,
                                metadata: record.metadata,
                                revoked: false,
                            })
                        },
                    )
                    .await
            });
        }
        Box::pin(async move {
            store
                .credentials_for(id, account, config?.as_ref(), cancellation)
                .await
        })
    }
}

#[cfg(feature = "provider-cursor")]
pub static CURSOR_AUTH: AuthDriver = AuthDriver::Cursor;
#[cfg(feature = "provider-opencode")]
pub static OPENCODE_AUTH: AuthDriver = AuthDriver::ApiKey;
#[cfg(feature = "provider-claude")]
pub static CLAUDE_AUTH: AuthDriver = AuthDriver::OAuth(claude_oauth);
#[cfg(feature = "provider-antigravity")]
pub static ANTIGRAVITY_AUTH: AuthDriver = AuthDriver::OAuth(antigravity_oauth);

#[cfg(any(
    feature = "provider-opencode",
    feature = "provider-claude",
    feature = "provider-antigravity"
))]
fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| ProviderError::Transport("failed to create native HTTP client".into()))
}
#[cfg(feature = "provider-cursor")]
pub fn cursor_factory(options: TransportOptions<'_>) -> Result<Arc<dyn Provider>> {
    Ok(Arc::new(codex_provider_cursor::CursorProvider::new(
        codex_provider_cursor::Options {
            base_url: options.base_url.unwrap_or("https://api2.cursor.sh").into(),
            ..Default::default()
        },
    )?))
}
pub enum NativeLogin {
    OAuth(Box<codex_provider_auth::LoopbackLogin>),
    #[cfg(feature = "provider-cursor")]
    Cursor(codex_provider_cursor::auth::LoginChallenge),
}
impl NativeLogin {
    pub async fn begin(descriptor: &crate::Descriptor) -> codex_provider_api::Result<Self> {
        let id = descriptor.id;
        let driver = descriptor.auth_driver.ok_or_else(|| not_compiled(id))?;
        #[cfg(feature = "provider-cursor")]
        if matches!(driver, AuthDriver::Cursor) {
            return Ok(Self::Cursor(
                codex_provider_cursor::auth::LoginChallenge::new(),
            ));
        }
        let config = driver
            .oauth_config()?
            .ok_or_else(|| ProviderError::Unsupported(format!("{id} requires an API key")))?;
        Ok(Self::OAuth(Box::new(
            codex_provider_auth::LoopbackLogin::begin(config).await?,
        )))
    }
    pub fn url(&self) -> &str {
        match self {
            Self::OAuth(login) => &login.auth_url,
            #[cfg(feature = "provider-cursor")]
            Self::Cursor(login) => &login.url,
        }
    }
    pub async fn finish(
        self,
        cancellation: &CancellationToken,
    ) -> codex_provider_api::Result<codex_provider_auth::TokenRecord> {
        match self {
            Self::OAuth(login) => login.finish(cancellation).await,
            #[cfg(feature = "provider-cursor")]
            Self::Cursor(challenge) => {
                let auth = codex_provider_cursor::auth::AuthClient::new("https://api2.cursor.sh")?
                    .poll_auth(challenge, cancellation.clone())
                    .await?;
                Ok(codex_provider_auth::TokenRecord {
                    expires_at: codex_provider_cursor::auth::token_expires_at(&auth.access_token),
                    access_token: auth.access_token,
                    refresh_token: auth.refresh_token,
                    metadata: serde_json::Value::Null,
                    revoked: false,
                })
            }
        }
    }
}

#[cfg(feature = "provider-opencode")]
pub fn opencode_factory(options: TransportOptions<'_>) -> Result<Arc<dyn Provider>> {
    let client = client()?;
    let provider = match options.base_url {
        Some(url) => codex_provider_opencode::OpenCodeProvider::with_base_url(client, url.into())?,
        None => codex_provider_opencode::OpenCodeProvider::new(client),
    };
    Ok(Arc::new(provider))
}

#[cfg(feature = "provider-claude")]
pub fn claude_factory(options: TransportOptions<'_>) -> Result<Arc<dyn Provider>> {
    let client = client()?;
    let provider = match options.base_url {
        Some(url) => codex_provider_claude::ClaudeProvider::with_base_url(client, url.into())?,
        None => codex_provider_claude::ClaudeProvider::new(client),
    };
    Ok(Arc::new(provider))
}

#[cfg(feature = "provider-antigravity")]
pub fn antigravity_factory(options: TransportOptions<'_>) -> Result<Arc<dyn Provider>> {
    let client = client()?;
    let provider = match options.base_url {
        Some(url) => {
            codex_provider_antigravity::AntigravityProvider::with_base_url(client, url.into())?
        }
        None => codex_provider_antigravity::AntigravityProvider::new(client),
    };
    let home = options.state_home.ok_or_else(|| {
        ProviderError::Checkpoint("Antigravity onboarding requires local state storage".into())
    })?;
    let provider = provider.with_state_home(home.to_path_buf());
    Ok(Arc::new(provider))
}

#[cfg(feature = "provider-claude")]
fn claude_oauth() -> Result<OAuthConfig> {
    use std::collections::BTreeMap;
    Ok(OAuthConfig {
        authorization_url: codex_provider_claude::AUTHORIZE_URL.into(),
        token_url: codex_provider_claude::TOKEN_URL.into(),
        client_id: codex_provider_claude::CLIENT_ID.into(),
        redirect_uri: codex_provider_claude::REDIRECT_URI.into(),
        scopes: codex_provider_claude::SCOPES
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
        authorization_params: BTreeMap::from([("code".into(), "true".into())]),
        token_params: BTreeMap::new(),
        token_json: true,
        exchange_state: true,
    })
}

#[cfg(feature = "provider-antigravity")]
fn antigravity_oauth() -> Result<OAuthConfig> {
    antigravity_oauth_with_environment(|name| std::env::var(name).ok())
}
#[cfg(feature = "provider-antigravity")]
fn antigravity_oauth_with_environment(
    environment: impl Fn(&str) -> Option<String>,
) -> Result<OAuthConfig> {
    let required = |name: &str| {
        environment(name)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                ProviderError::Authentication(format!(
                    "set {name} to configure the Antigravity OAuth application"
                ))
            })
    };
    use std::collections::BTreeMap;
    Ok(OAuthConfig {
        authorization_url: codex_provider_antigravity::AUTHORIZE_URL.into(),
        token_url: codex_provider_antigravity::TOKEN_URL.into(),
        client_id: required(codex_provider_antigravity::CLIENT_ID_ENV)?,
        redirect_uri: codex_provider_antigravity::REDIRECT_URI.into(),
        scopes: codex_provider_antigravity::SCOPES
            .iter()
            .map(|value| (*value).into())
            .collect(),
        authorization_params: BTreeMap::from([
            ("access_type".into(), "offline".into()),
            ("prompt".into(), "consent".into()),
        ]),
        token_params: BTreeMap::from([(
            "client_secret".into(),
            required(codex_provider_antigravity::CLIENT_SECRET_ENV)?,
        )]),
        token_json: false,
        exchange_state: false,
    })
}

#[cfg(all(test, feature = "provider-antigravity"))]
mod tests {
    use super::*;
    #[cfg(feature = "provider-antigravity")]
    #[test]
    fn antigravity_oauth_requires_explicit_application_configuration() {
        for missing_secret in [None, Some("   ".to_owned())] {
            let error = antigravity_oauth_with_environment(|name| {
                if name == codex_provider_antigravity::CLIENT_ID_ENV {
                    Some("fixture-client".into())
                } else {
                    missing_secret.clone()
                }
            })
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("ANTIGRAVITY_OAUTH_CLIENT_SECRET")
            );
            assert!(!error.to_string().contains("fixture-client"));
        }
        assert!(
            antigravity_oauth_with_environment(|_| None)
                .unwrap_err()
                .to_string()
                .contains("ANTIGRAVITY_OAUTH_CLIENT_ID")
        );
        let config = antigravity_oauth_with_environment(|name| match name {
            codex_provider_antigravity::CLIENT_ID_ENV => Some("fixture-client".into()),
            codex_provider_antigravity::CLIENT_SECRET_ENV => Some("fixture-secret".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(config.client_id, "fixture-client");
        assert_eq!(config.token_params["client_secret"], "fixture-secret");
        assert!(!format!("{config:?}").contains("fixture-secret"));
    }
}
