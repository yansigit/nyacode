use crate::ChatError;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpClientFactory;
use codex_http_client::HttpError;
use codex_http_client::HttpResponse;
use codex_http_client::RequestBuilder;
use codex_http_client::RetryAfter;
use codex_http_client::RouteAwareClientPool;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::WorkspaceRoutingRequest;
use http::HeaderValue;
use http::Method;
use http::StatusCode;
use http::header::CONTENT_TYPE;
use serde::de::DeserializeOwned;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

const BACKEND: &str = "https://chatgpt.com/backend-api/";
const RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Harness-only comparison. Production Chat remains unavailable until a native
/// transport passes the live gate; this selector never provides a fallback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeTransport {
    #[default]
    Standard,
    BrowserEmulated,
}

impl std::str::FromStr for ProbeTransport {
    type Err = ChatError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "standard" => Ok(Self::Standard),
            "browser-emulated" => Ok(Self::BrowserEmulated),
            _ => Err(ChatError::InvalidInput),
        }
    }
}

/// Account-bound research client. Dropping/stopping it only cancels local work.
/// The owner must call `stop` when disabling the feature.
pub struct ChatProbe {
    pub(crate) auth: Arc<AuthManager>,
    account_id: String,
    owner_generation: u64,
    stopped: CancellationToken,
    backend: Url,
    pub(crate) transport: ProbeTransport,
    session: Arc<Mutex<Option<SessionClient>>>,
    session_watcher: OnceLock<tokio::task::JoinHandle<()>>,
    pub(crate) native_device: OnceLock<String>,
    pub(crate) preparation_permit: tokio::sync::Semaphore,
    limits: Mutex<SessionLimits>,
}

#[derive(Default)]
struct SessionLimits {
    challenge: Option<ChatError>,
    retry_after: Option<RetryAfter>,
    preparation_failures: u8,
    preparation_cooldown: Option<tokio::time::Instant>,
}

// Credentials are added to individual requests, never cached in pool defaults.
// One probe belongs to one account/ownership generation; routes within it are
// pooled by RouteAwareClientPool. A configuration/origin change drops the pool.
struct SessionClient {
    id: uuid::Uuid,
    factory: HttpClientFactory,
    origin: String,
    headers: http::HeaderMap,
    client: HttpClient,
    storage: HttpClient,
    generation: u64,
    profile: Option<Arc<crate::sentinel::Profile>>,
}

impl ChatProbe {
    pub(crate) fn transport_session_id(&self, generation: u64) -> Result<uuid::Uuid, ChatError> {
        self.check_owner()?;
        if self.auth.auth_change_state_receiver().borrow().generation != generation {
            return Err(ChatError::SessionChanged);
        }
        let session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        session
            .as_ref()
            .filter(|session| session.generation == generation)
            .map(|session| session.id)
            .ok_or(ChatError::SessionChanged)
    }

    pub(crate) fn check_profile(
        &self,
        profile: &crate::sentinel::Profile,
    ) -> Result<(), ChatError> {
        self.check_owner()?;
        let session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if session
            .as_ref()
            .and_then(|session| session.profile.as_ref())
            .is_some_and(|current| std::ptr::eq(current.as_ref(), profile))
        {
            Ok(())
        } else {
            Err(ChatError::SessionChanged)
        }
    }

    #[cfg(test)]
    pub(crate) fn set_test_backend(&mut self, backend: Url) {
        self.backend = backend;
    }

    pub(crate) fn native_profile(
        &self,
        bootstrap: crate::sentinel::Bootstrap,
        generation: u64,
    ) -> Result<Arc<crate::sentinel::Profile>, ChatError> {
        self.check_owner()?;
        if self.auth.auth_change_state_receiver().borrow().generation != generation {
            return Err(ChatError::SessionChanged);
        }
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = session.as_mut().ok_or(ChatError::SessionChanged)?;
        if current.generation != generation || current.origin != bootstrap.origin {
            return Err(ChatError::SessionChanged);
        }
        if current
            .profile
            .as_ref()
            .is_some_and(|p| p.bootstrap.build != bootstrap.build)
        {
            session.take();
            return Err(ChatError::SessionChanged);
        }
        Ok(Arc::clone(current.profile.get_or_insert_with(|| {
            let mut profile = crate::sentinel::Profile::new(bootstrap);
            if let Some(device) = self.native_device.get() {
                profile.device.clone_from(device);
            }
            Arc::new(profile)
        })))
    }

    /// Binds to the current managed login without loading credentials independently.
    /// `expected_account` is optional only when the caller explicitly selected the current login.
    pub fn new(
        auth: Arc<AuthManager>,
        enabled: bool,
        expected_account: Option<&str>,
    ) -> Result<Self, ChatError> {
        Self::with_transport(auth, enabled, expected_account, ProbeTransport::Standard)
    }

    pub fn with_transport(
        auth: Arc<AuthManager>,
        enabled: bool,
        expected_account: Option<&str>,
        transport: ProbeTransport,
    ) -> Result<Self, ChatError> {
        if !enabled {
            return Err(ChatError::Disabled);
        }
        let changes = auth.auth_change_state_receiver();
        let owner_generation = changes.borrow().owner_generation;
        let current = auth.auth_cached().ok_or(ChatError::LoginRequired)?;
        let account_id = chat_account(&current)?;
        if expected_account.is_some_and(|expected| expected != account_id) {
            return Err(ChatError::AccountMismatch);
        }
        if changes.borrow().owner_generation != owner_generation {
            return Err(ChatError::IdentityChanged);
        }
        Ok(Self {
            auth,
            account_id,
            owner_generation,
            stopped: CancellationToken::new(),
            backend: Url::parse(BACKEND).map_err(|_| ChatError::InvalidInput)?,
            transport,
            session: Arc::new(Mutex::new(None)),
            session_watcher: OnceLock::new(),
            native_device: OnceLock::new(),
            preparation_permit: tokio::sync::Semaphore::new(1),
            limits: Mutex::new(SessionLimits::default()),
        })
    }

    pub fn stop(&self) {
        self.stopped.cancel();
        self.clear_session();
    }

    fn clear_session(&self) {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    fn watch_session(&self) {
        self.session_watcher.get_or_init(|| {
            let mut changes = self.auth.auth_change_state_receiver();
            let owner = self.owner_generation;
            let session = Arc::clone(&self.session);
            let stopped = self.stopped.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = stopped.cancelled() => break,
                        result = changes.changed() => {
                            // Even a token refresh invalidates transport sessions;
                            // an ownership change also terminates this watcher.
                            let state = *changes.borrow_and_update();
                            let changed_owner = result.is_err() || state.owner_generation != owner;
                            let mut current = session.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            // A request may already have replaced the old pool
                            // before this notification was scheduled.
                            if changed_owner || current.as_ref().is_some_and(|s| s.generation != state.generation) {
                                current.take();
                            }
                            if changed_owner {
                                break;
                            }
                        }
                    }
                }
                session.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
            })
        });
    }

    pub(crate) fn check_session_limits(&self, preparation: bool) -> Result<(), ChatError> {
        let limits = self
            .limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(error) = limits.challenge {
            return Err(error);
        }
        if limits
            .retry_after
            .is_some_and(|retry| !retry.remaining_delay().is_zero())
        {
            return Err(ChatError::RateLimited);
        }
        if preparation
            && limits
                .preparation_cooldown
                .is_some_and(|until| until > tokio::time::Instant::now())
        {
            return Err(ChatError::PreparationCooldown);
        }
        Ok(())
    }

    pub(crate) fn finish_preparation(&self, error: Option<ChatError>) {
        let mut limits = self
            .limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if error.is_some() {
            limits.preparation_failures = limits.preparation_failures.saturating_add(1);
            if limits.preparation_failures >= 3 {
                limits.preparation_cooldown =
                    Some(tokio::time::Instant::now() + Duration::from_secs(600));
                limits.preparation_failures = 0;
            }
        } else {
            limits.preparation_failures = 0;
            limits.preparation_cooldown = None;
        }
        if matches!(
            error,
            Some(ChatError::CloudflareChallenge | ChatError::UnsupportedChallenge)
        ) {
            limits.challenge = error;
        }
    }

    pub(crate) fn observe_response(&self, response: &HttpResponse) {
        let mut limits = self
            .limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(retry) = RetryAfter::from_headers(response.headers()) {
            limits.retry_after = Some(limits.retry_after.map_or(retry, |old| {
                if old.deadline() > retry.deadline() {
                    old
                } else {
                    retry
                }
            }));
        }
        if response
            .headers()
            .get("cf-mitigated")
            .is_some_and(|value| value == "challenge")
        {
            limits.challenge = Some(ChatError::CloudflareChallenge);
        }
    }

    pub async fn model_count(&self) -> Result<usize, ChatError> {
        Ok(self.models().await?.len())
    }

    pub async fn history_count(&self, offset: u32, limit: u32) -> Result<usize, ChatError> {
        Ok(self.conversation_list(offset, limit).await?.items.len())
    }

    pub async fn active_message_count(&self, conversation_id: &str) -> Result<usize, ChatError> {
        Ok(self
            .conversation_read(conversation_id)
            .await?
            .branch
            .nodes
            .iter()
            .filter_map(|node| node.message.as_ref())
            .filter(|message| matches!(message.role.as_str(), "user" | "assistant" | "tool"))
            .count())
    }

    pub(crate) fn check_owner(&self) -> Result<(), ChatError> {
        if self.stopped.is_cancelled() {
            return Err(ChatError::LocalCancellation);
        }
        if self
            .auth
            .auth_change_state_receiver()
            .borrow()
            .owner_generation
            != self.owner_generation
        {
            self.clear_session();
            return Err(ChatError::IdentityChanged);
        }
        let current = self.auth.auth_cached().ok_or(ChatError::IdentityChanged)?;
        if chat_account(&current).as_deref() != Ok(self.account_id.as_str()) {
            self.clear_session();
            return Err(ChatError::IdentityChanged);
        }
        Ok(())
    }

    pub(crate) async fn guarded_generation<T>(
        &self,
        generation: u64,
        work: impl Future<Output = Result<T, ChatError>>,
    ) -> Result<T, ChatError> {
        let mut changes = self.auth.auth_change_state_receiver();
        if changes.borrow().generation != generation {
            return Err(ChatError::SessionChanged);
        }
        let result = self
            .guarded(async {
                tokio::select! {
                    biased;
                    _ = changes.changed() => Err(ChatError::SessionChanged),
                    result = work => result,
                }
            })
            .await;
        if changes.borrow().generation != generation {
            return Err(ChatError::SessionChanged);
        }
        result
    }

    pub(crate) async fn guarded<T>(
        &self,
        work: impl Future<Output = Result<T, ChatError>>,
    ) -> Result<T, ChatError> {
        let mut changes = self.auth.auth_change_state_receiver();
        self.check_owner()?;
        self.check_session_limits(false)?;
        let owner_changed = async {
            loop {
                if changes.borrow_and_update().owner_generation != self.owner_generation {
                    break;
                }
                if changes.changed().await.is_err() {
                    break;
                }
            }
        };
        tokio::select! {
            biased;
            _ = self.stopped.cancelled() => Err(ChatError::LocalCancellation),
            _ = owner_changed => {
                self.clear_session();
                Err(ChatError::IdentityChanged)
            },
            result = tokio::time::timeout(REQUEST_TIMEOUT, work) => {
                self.check_owner()?;
                result.map_err(|_| ChatError::Timeout)?
            }
        }
    }

    pub(crate) async fn read<T: DeserializeOwned>(
        &self,
        path: &str,
        target: &str,
    ) -> Result<T, ChatError> {
        self.guarded(self.read_inner(path, target)).await
    }

    async fn read_inner<T: DeserializeOwned>(
        &self,
        path: &str,
        target: &str,
    ) -> Result<T, ChatError> {
        let mut recovery = self.auth.unauthorized_recovery();
        loop {
            self.check_session_limits(false)?;
            let response = self
                .request(Method::GET, path, target)
                .await?
                .send()
                .await
                .map_err(transport_error)?;
            self.observe_response(&response);
            if response.status() == StatusCode::UNAUTHORIZED && recovery.has_next() {
                // Only GETs are replayed. Writes must reconcile server state.
                drop(response);
                recovery
                    .next()
                    .await
                    .map_err(|_| ChatError::Authentication)?;
                continue;
            }
            classify_response(response.status(), response.headers())?;
            let body = bounded_body(response).await?;
            // Parsing errors can quote response bytes, so never expose serde's error.
            return serde_json::from_slice(&body).map_err(|_| ChatError::ProtocolChanged);
        }
    }

    /// Builds a single request with the same managed auth, routing, cookies, and
    /// policy as reads. The caller owns replay policy and response interpretation.
    pub(crate) async fn request(
        &self,
        method: Method,
        path: &str,
        target: &str,
    ) -> Result<RequestBuilder, ChatError> {
        self.routed_request(method, path, Some(target)).await
    }

    /// Seed the routed origin's infrastructure cookies without bearer or account
    /// headers. Redirects remain disabled, as for authenticated backend requests.
    pub(crate) async fn bootstrap_request(&self) -> Result<RequestBuilder, ChatError> {
        self.routed_request(Method::GET, "../", None).await
    }

    async fn routed_request(
        &self,
        method: Method,
        path: &str,
        target: Option<&str>,
    ) -> Result<RequestBuilder, ChatError> {
        self.watch_session();
        self.check_owner()?;
        self.check_session_limits(false)?;
        let generation = self.auth.auth_change_state_receiver().borrow().generation;
        let (auth, factory) = self
            .auth
            .auth_with_http_client_factory()
            .await
            .ok_or(ChatError::Authentication)?;
        self.check_owner()?;
        if chat_account(&auth)? != self.account_id {
            return Err(ChatError::IdentityChanged);
        }
        let mut url = self
            .backend
            .join(path)
            .map_err(|_| ChatError::InvalidInput)?;
        let mut headers = codex_login::default_client::default_headers();
        let routing = self
            .auth
            .workspace_routing(
                &auth,
                WorkspaceRoutingRequest {
                    provider_base_url: self.backend.to_string(),
                    chatgpt_base_url: self.backend.to_string(),
                    previously_routed: false,
                    session: None,
                },
            )
            .await
            .map_err(|_| ChatError::UnsupportedCapability)?;
        if let Some(routing) = routing {
            if routing.chatgpt_account_id != self.account_id {
                return Err(ChatError::IdentityChanged);
            }
            let origin =
                Url::parse(&routing.backend_origin).map_err(|_| ChatError::ProtocolChanged)?;
            if origin.scheme() != "https"
                || origin.host_str().is_none()
                || !origin.username().is_empty()
                || origin.password().is_some()
                || origin.path() != "/"
                || origin.query().is_some()
                || origin.fragment().is_some()
            {
                return Err(ChatError::ProtocolChanged);
            }
            url.set_host(origin.host_str())
                .map_err(|_| ChatError::ProtocolChanged)?;
            url.set_port(origin.port())
                .map_err(|_| ChatError::ProtocolChanged)?;
            match routing.account_routing_override.as_str() {
                "NO_CONSTRAINT" => {}
                "us" | "us_cr" => {
                    headers.insert(
                        "x-openai-account-routing-override",
                        HeaderValue::from_str(&routing.account_routing_override)
                            .map_err(|_| ChatError::ProtocolChanged)?,
                    );
                }
                _ => return Err(ChatError::ProtocolChanged),
            }
        } else if auth.is_workspace_account() || auth.is_fedramp_account() {
            // A standalone probe has no app-server routing discovery owner. Never
            // silently send workspace traffic to the consumer origin in that case.
            return Err(ChatError::UnsupportedCapability);
        }
        if self.auth.auth_change_state_receiver().borrow().generation != generation {
            return Err(ChatError::SessionChanged);
        }
        let client = {
            let mut session = self
                .session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let origin = url.origin().ascii_serialization();
            if session.as_ref().is_none_or(|session| {
                session.factory != factory
                    || session.origin != origin
                    || session.headers != headers
                    || session.generation != generation
            }) {
                let builder = HttpClientBuilder::new()
                    .default_headers(headers.clone())
                    .with_chatgpt_cookies(&factory)
                    .without_redirects()
                    .without_automatic_retries()
                    .without_request_logging();
                let client = match self.transport {
                    ProbeTransport::Standard => RouteAwareClientPool::with_builder(
                        factory.clone(),
                        ClientRouteClass::Api,
                        builder,
                    )
                    .into_client(),
                    ProbeTransport::BrowserEmulated => factory
                        .browser_chat_client(&origin, headers.clone())
                        .map_err(transport_error)?,
                };
                *session = Some(SessionClient {
                    id: uuid::Uuid::new_v4(),
                    storage: RouteAwareClientPool::with_builder(
                        factory.clone().with_chatgpt_cookies([]),
                        ClientRouteClass::Other,
                        HttpClientBuilder::new()
                            .without_redirects()
                            .without_automatic_retries()
                            .without_request_logging(),
                    )
                    .for_current_account()
                    .into_client(),
                    client,
                    factory,
                    origin,
                    headers,
                    generation,
                    profile: None,
                });
            }
            session.as_ref().ok_or(ChatError::Transport)?.client.clone()
        };
        self.check_owner()?;
        let request = client.request(method, url);
        match target {
            Some(target) => Ok(request
                .bearer_auth(auth.get_token().map_err(|_| ChatError::Authentication)?)
                .header("chatgpt-account-id", &self.account_id)
                .header("x-openai-target-path", format!("/backend-api/{target}"))
                .header("accept", "application/json")),
            None => Ok(request.header("accept", "text/html")),
        }
    }

    /// Signed storage has its own pool and no ChatGPT default headers or cookie
    /// store. It still uses the application's proxy, CA, and revocable policy.
    pub(crate) fn storage_request(
        &self,
        url: Url,
        generation: u64,
    ) -> Result<RequestBuilder, ChatError> {
        self.check_owner()?;
        let session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session = session.as_ref().ok_or(ChatError::SessionChanged)?;
        if session.generation != generation {
            return Err(ChatError::SessionChanged);
        }
        crate::upload::validate_storage_url(&url)?;
        Ok(session.storage.request(Method::PUT, url))
    }
}

impl Drop for ChatProbe {
    fn drop(&mut self) {
        self.stop();
        if let Some(watcher) = self.session_watcher.get() {
            watcher.abort();
        }
    }
}

pub(crate) async fn bounded_body(mut response: HttpResponse) -> Result<Vec<u8>, ChatError> {
    if response
        .content_length()
        .is_some_and(|length| length > RESPONSE_LIMIT as u64)
    {
        return Err(ChatError::ResponseTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        if chunk.len() > RESPONSE_LIMIT - body.len() {
            return Err(ChatError::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn chat_account(auth: &CodexAuth) -> Result<String, ChatError> {
    if !matches!(
        auth,
        CodexAuth::Chatgpt(_) | CodexAuth::ChatgptAuthTokens(_)
    ) {
        return Err(ChatError::LoginRequired);
    }
    auth.get_account_id()
        .filter(|id| !id.trim().is_empty())
        .ok_or(ChatError::LoginRequired)
}

pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(crate) fn classify_response(
    status: StatusCode,
    headers: &http::HeaderMap,
) -> Result<(), ChatError> {
    classify_status(status, headers)?;
    let media_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if !media_type.is_some_and(|value| value.eq_ignore_ascii_case("application/json")) {
        return Err(ChatError::ProtocolChanged);
    }
    Ok(())
}

pub(crate) fn classify_status(
    status: StatusCode,
    headers: &http::HeaderMap,
) -> Result<(), ChatError> {
    if headers
        .get("cf-mitigated")
        .is_some_and(|value| value == "challenge")
    {
        return Err(ChatError::CloudflareChallenge);
    }
    match status {
        StatusCode::UNAUTHORIZED => return Err(ChatError::Authentication),
        StatusCode::FORBIDDEN => return Err(ChatError::AccessDenied),
        StatusCode::TOO_MANY_REQUESTS => return Err(ChatError::RateLimited),
        StatusCode::NOT_FOUND => return Err(ChatError::NotFound),
        _ if status.is_redirection() => return Err(ChatError::UnsupportedCapability),
        _ if !status.is_success() => return Err(ChatError::Transport),
        _ => {}
    }
    Ok(())
}

pub(crate) fn transport_error(error: HttpError) -> ChatError {
    if error.is_timeout() {
        return ChatError::Timeout;
    }
    match error {
        HttpError::Policy(_) => ChatError::NetworkPolicy,
        _ => ChatError::Transport,
    }
}

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "upload_tests.rs"]
mod upload_tests;
