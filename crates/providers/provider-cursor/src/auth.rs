//! Cursor's native browser PKCE polling, API-key exchange, and OAuth refresh.
//! The host owns credential locking and durable storage.
//! No desktop credential database or third-party runtime is used.
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_provider_api::ProviderError;
use codex_provider_api::Result;
use rand::Rng;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use tokio_util::sync::CancellationToken;

use crate::connect::bounded_body;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredAuth {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

pub struct LoginChallenge {
    pub url: String,
    uuid: String,
    verifier: String,
}

impl LoginChallenge {
    pub fn new() -> Self {
        let mut random = [0u8; 32];
        rand::rng().fill_bytes(&mut random);
        let verifier = URL_SAFE_NO_PAD.encode(random);
        let challenge = URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
        let uuid = uuid::Uuid::new_v4().to_string();
        Self {
            url: format!(
                "https://cursor.com/loginDeepControl?challenge={challenge}&uuid={uuid}&mode=login&redirectTarget=cli"
            ),
            uuid,
            verifier,
        }
    }
}

impl Default for LoginChallenge {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct AuthClient {
    base_url: String,
    client: reqwest::Client,
}

impl AuthClient {
    pub fn new(base_url: &str) -> Result<Self> {
        Ok(Self {
            base_url: crate::validate_endpoint(base_url)?,
            client: crate::http_client()?,
        })
    }

    /// User API keys are exchanged for a short-lived AgentService bearer token.
    /// The API key itself is never sent to an inference RPC.
    pub async fn exchange_api_key(
        &self,
        api_key: &str,
        cancellation: CancellationToken,
    ) -> Result<StoredAuth> {
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ProviderError::Cancelled),
            response = self.client.post(format!("{}/auth/exchange_user_api_key", self.base_url))
                .bearer_auth(api_key).header("content-type", "application/json").body("{}").send() =>
                response.map_err(|_| ProviderError::Transport("Cursor API key exchange failed".into()))?,
        };
        if !response.status().is_success() {
            return Err(ProviderError::Authentication(format!(
                "Cursor API key exchange returned HTTP {}",
                response.status().as_u16()
            )));
        }
        parse_auth(&bounded_body(response, &cancellation).await?)
    }
    /// Finish the browser flow without persisting it, for a host that owns a
    /// shared account pool and credential store.
    pub async fn poll_auth(
        &self,
        challenge: LoginChallenge,
        cancellation: CancellationToken,
    ) -> Result<StoredAuth> {
        // Pinned CLI implementations poll up to 150 times with a ten-second
        // backoff ceiling. A ten-minute deadline can expire during browser login.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(25 * 60);
        let mut interval = Duration::from_secs(1);
        loop {
            let request = self
                .client
                .get(format!("{}/auth/poll", self.base_url))
                .query(&[("uuid", &challenge.uuid), ("verifier", &challenge.verifier)])
                .header("accept", "application/json")
                .header("content-type", "application/json")
                .timeout(Duration::from_secs(30));
            let response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ProviderError::Cancelled),
                response = request.send() => response.map_err(|_| ProviderError::Transport("Cursor login poll failed".into()))?,
            };
            if response.status().is_success() {
                let auth = parse_auth(&bounded_body(response, &cancellation).await?)?;
                return Ok(auth);
            }
            if response.status() != reqwest::StatusCode::NOT_FOUND {
                return Err(ProviderError::Authentication(format!(
                    "Cursor login failed with HTTP {}",
                    response.status().as_u16()
                )));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ProviderError::Authentication(
                    "Cursor login timed out".into(),
                ));
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ProviderError::Cancelled),
                _ = tokio::time::sleep(interval) => {},
            }
            interval = (interval.mul_f64(1.2)).min(Duration::from_secs(10));
        }
    }

    pub async fn refresh_auth(
        &self,
        refresh_token: String,
        cancellation: CancellationToken,
    ) -> Result<StoredAuth> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(ProviderError::Cancelled),
            result = refresh(&self.base_url, StoredAuth { access_token: String::new(), refresh_token: Some(refresh_token) }) => result,
        }
    }
}

/// An unverified expiry hint for proactive refresh; this never authenticates
/// a JWT locally. Upstream remains the authority for credential validity.
pub fn token_expires_at(token: &str) -> Option<u64> {
    let payload = URL_SAFE_NO_PAD.decode(token.split('.').nth(1)?).ok()?;
    serde_json::from_slice::<serde_json::Value>(&payload)
        .ok()?
        .get("exp")?
        .as_u64()
}

/// Native refresh for hosts that own credential locking and writeback. The
/// host must persist the returned record before exposing it to another turn.
pub async fn refresh(base_url: &str, previous: StoredAuth) -> Result<StoredAuth> {
    let base_url = crate::validate_endpoint(base_url)?;
    let token = previous.refresh_token.ok_or_else(|| {
        ProviderError::Authentication("Cursor refresh credentials missing".into())
    })?;
    let client = crate::http_client()?;
    let request = if token.starts_with("crsr_") {
        // User API keys returned as refresh credentials remain stable; exchange
        // them for the AgentService bearer as in the pinned OpenCodex client.
        client
            .post(format!("{base_url}/auth/exchange_user_api_key"))
            .bearer_auth(&token)
            .header("content-type", "application/json")
            .body("{}")
    } else {
        // Cursor's desktop OAuth registration is public client metadata.
        client.post(format!("{base_url}/oauth/token"))
            .json(&serde_json::json!({"grant_type":"refresh_token","client_id":"KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB","refresh_token":token}))
    };
    let response = request
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| ProviderError::Authentication("Cursor refresh failed".into()))?;
    if matches!(response.status().as_u16(), 401 | 403) {
        return Err(ProviderError::Authentication("invalid_grant".into()));
    }
    if !response.status().is_success() {
        return Err(ProviderError::Authentication(format!(
            "Cursor refresh returned HTTP {}",
            response.status().as_u16()
        )));
    }
    let bytes = bounded_body(response, &CancellationToken::new()).await?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| ProviderError::Authentication("invalid Cursor refresh response".into()))?;
    if value["shouldLogout"] == true
        || value["should_logout"] == true
        || value["error"] == "invalid_grant"
    {
        return Err(ProviderError::Authentication("invalid_grant".into()));
    }
    let mut auth = parse_auth(&bytes)?;
    if auth.refresh_token.is_none() {
        auth.refresh_token = Some(token);
    }
    Ok(auth)
}

fn parse_auth(bytes: &[u8]) -> Result<StoredAuth> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| ProviderError::Authentication("invalid Cursor credential response".into()))?;
    let access_token = value
        .get("accessToken")
        .or_else(|| value.get("access_token"))
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            ProviderError::Authentication("Cursor response missing access token".into())
        })?;
    let refresh_token = value
        .get("refreshToken")
        .or_else(|| value.get("refresh_token"))
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    Ok(StoredAuth {
        access_token: access_token.to_string(),
        refresh_token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_token_variants_without_debug_exposure() {
        let auth = parse_auth(br#"{"access_token":"access","refreshToken":"refresh"}"#).unwrap();
        assert_eq!(auth.access_token, "access");
        assert_eq!(auth.refresh_token.as_deref(), Some("refresh"));
        assert!(parse_auth(br#"{"accessToken":""}"#).is_err());
    }
    #[tokio::test]
    async fn api_key_exchange_and_refresh_preserve_original_refresh_token() {
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::header;
        use wiremock::matchers::method;
        use wiremock::matchers::path;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/exchange_user_api_key"))
            .and(header("authorization", "Bearer test-api-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"accessToken":"test-access"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(wiremock::matchers::body_json(serde_json::json!({"grant_type":"refresh_token","client_id":"KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB","refresh_token":"test-refresh"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"accessToken":"new-access"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = AuthClient::new(&server.uri()).unwrap();
        assert_eq!(
            client
                .exchange_api_key("test-api-key", CancellationToken::new())
                .await
                .unwrap()
                .access_token,
            "test-access"
        );
        let refreshed = client
            .refresh_auth("test-refresh".into(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("test-refresh"));
    }

    #[tokio::test]
    async fn cancelled_login_does_not_poll() {
        let client = AuthClient::new("http://127.0.0.1:1").unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            client.poll_auth(LoginChallenge::new(), cancellation).await,
            Err(ProviderError::Cancelled)
        ));
    }

    #[test]
    fn jwt_expiry_is_only_a_hint() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"exp":123}"#);
        assert_eq!(
            token_expires_at(&format!("e30.{payload}.signature")),
            Some(123)
        );
        assert_eq!(token_expires_at("invalid"), None);
    }
    #[tokio::test]
    async fn browser_login_uses_one_get_poll_with_matching_pkce_values() {
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::method;
        use wiremock::matchers::path;
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        let challenge = LoginChallenge::new();
        let url = url::Url::parse(&challenge.url).unwrap();
        let values: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            values["challenge"],
            URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(challenge.verifier.as_bytes()))
        );
        assert_eq!(values["uuid"], challenge.uuid);
        assert_eq!(values["redirectTarget"], "cli");
        Mock::given(method("GET"))
            .and(path("/auth/poll"))
            .and(query_param("uuid", &challenge.uuid))
            .and(query_param("verifier", &challenge.verifier))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"accessToken":"access","refreshToken":"refresh"}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let record = AuthClient::new(&server.uri())
            .unwrap()
            .poll_auth(challenge, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(record.access_token, "access");
    }

    #[tokio::test]
    async fn pending_browser_login_repolls_get_until_credentials_arrive() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::method;
        use wiremock::matchers::path;
        let server = MockServer::start().await;
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        Mock::given(method("GET"))
            .and(path("/auth/poll"))
            .respond_with(move |_: &wiremock::Request| {
                if requests.fetch_add(1, Ordering::Relaxed) == 0 {
                    ResponseTemplate::new(404).set_body_string("Not found")
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"accessToken":"authorized"}))
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let record = AuthClient::new(&server.uri())
            .unwrap()
            .poll_auth(LoginChallenge::new(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(record.access_token, "authorized");
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn oauth_logout_signal_is_revocation_and_key_refresh_uses_exchange() {
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::header;
        use wiremock::matchers::method;
        use wiremock::matchers::path;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"shouldLogout":true,"access_token":""})),
            )
            .mount(&server)
            .await;
        let client = AuthClient::new(&server.uri()).unwrap();
        assert!(
            matches!(client.refresh_auth("expired-refresh".into(),CancellationToken::new()).await,Err(ProviderError::Authentication(message)) if message=="invalid_grant")
        );
        Mock::given(method("POST"))
            .and(path("/auth/exchange_user_api_key"))
            .and(header("authorization", "Bearer crsr_test-refresh"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"accessToken":"fresh"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let record = client
            .refresh_auth("crsr_test-refresh".into(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(record.access_token, "fresh");
        assert_eq!(record.refresh_token.as_deref(), Some("crsr_test-refresh"));
    }
}
