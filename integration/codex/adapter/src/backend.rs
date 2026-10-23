//! Codex-facing native backend. Provider transport and authentication live in the registry.
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::ModelsEndpointClient;
use codex_models_manager::manager::ModelsEndpointFuture;
use codex_models_manager::manager::ModelsEndpointResponse;
use codex_models_manager::manager::OpenAiModelsManager;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::error::CodexErr;
use codex_protocol::openai_models::ModelsResponse;
use codex_provider_api::Context;
use codex_provider_api::Credentials;
use codex_provider_api::Provider;
use codex_provider_api::ProviderError;
use codex_provider_api::ProviderFuture;
use codex_provider_auth::AccountInfo;
use codex_provider_auth::AuthStore;
use codex_provider_auth::TokenRecord;
use codex_provider_registry::Descriptor;
pub use codex_provider_registry::NativeLogin;
use codex_provider_registry::TransportOptions;
use codex_provider_registry::registry;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use tokio_util::sync::CancellationToken;

type ActiveAccountSessions =
    std::collections::BTreeMap<(PathBuf, String, String), Vec<CancellationToken>>;
static ACTIVE_ACCOUNT_SESSIONS: OnceLock<std::sync::Mutex<ActiveAccountSessions>> = OnceLock::new();
fn account_sessions() -> &'static std::sync::Mutex<ActiveAccountSessions> {
    ACTIVE_ACCOUNT_SESSIONS.get_or_init(Default::default)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCapabilities {
    pub automatic_inference_retry: bool,
    pub terminal_completion: bool,
    pub structured_host_completion: bool,
    /// Agent mail must wait until the session has durably completed. Dropping a
    /// live native stream cannot authorize another request with uncertain history.
    pub defer_mailbox_preemption: bool,
}
#[derive(Clone)]
pub struct NativeBackend {
    info: ModelProviderInfo,
    descriptor: Option<&'static Descriptor>,
    home: Option<PathBuf>,
    transport: Arc<OnceLock<Arc<dyn Provider>>>,
    discovery_thread: Arc<std::sync::Mutex<Option<String>>>,
}
impl std::fmt::Debug for NativeBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeBackend")
            .field("info", &self.info)
            .field("home", &self.home)
            .finish_non_exhaustive()
    }
}
impl NativeBackend {
    pub fn new(info: ModelProviderInfo, home: Option<PathBuf>) -> Self {
        Self {
            descriptor: info.native_provider_id().and_then(|id| registry().get(id)),
            info,
            home,
            transport: Arc::new(OnceLock::new()),
            discovery_thread: Arc::new(std::sync::Mutex::new(None)),
        }
    }
    pub fn id(&self) -> &str {
        self.info
            .native_provider_id()
            .unwrap_or("unknown-native-provider")
    }
    pub fn info(&self) -> &ModelProviderInfo {
        &self.info
    }
    pub fn state_home(&self) -> Option<PathBuf> {
        self.home.clone()
    }
    pub fn descriptor(&self) -> codex_provider_api::Result<&'static Descriptor> {
        let descriptor = self.descriptor.ok_or_else(|| {
            ProviderError::Unsupported(format!("unknown native provider: {}", self.id()))
        })?;
        if descriptor.id != self.id() {
            return Err(ProviderError::Protocol(
                "native backend descriptor conflicts with provider configuration".into(),
            ));
        }
        Ok(descriptor)
    }
    pub fn session_capabilities(&self) -> SessionCapabilities {
        SessionCapabilities {
            automatic_inference_retry: false,
            terminal_completion: true,
            structured_host_completion: true,
            defer_mailbox_preemption: true,
        }
    }
    pub fn with_provider(
        info: ModelProviderInfo,
        home: Option<PathBuf>,
        provider: Arc<dyn Provider>,
    ) -> Self {
        let backend = Self::new(info, home);
        let _ = backend.transport.set(provider);
        backend
    }
    pub fn with_descriptor(
        info: ModelProviderInfo,
        home: Option<PathBuf>,
        descriptor: &'static Descriptor,
    ) -> Self {
        Self {
            info,
            home,
            descriptor: Some(descriptor),
            transport: Arc::new(OnceLock::new()),
            discovery_thread: Arc::new(std::sync::Mutex::new(None)),
        }
    }
    fn auth_store(&self) -> codex_provider_api::Result<AuthStore> {
        self.home.as_deref().map(AuthStore::new).ok_or_else(|| {
            ProviderError::Authentication("native provider requires a configured Codex home".into())
        })
    }
    pub fn models_manager(&self, catalog: Option<ModelsResponse>) -> SharedModelsManager {
        if let Some(catalog) = catalog {
            return Arc::new(StaticModelsManager::new(None, catalog));
        }
        let mut endpoint = self.clone();
        endpoint.discovery_thread = Arc::new(std::sync::Mutex::new(None));
        Arc::new(
            OpenAiModelsManager::new_without_cache(Arc::new(endpoint), None)
                .with_provider_catalog(),
        )
    }
    fn environment_token(&self) -> Option<String> {
        self.info
            .experimental_bearer_token
            .as_ref()
            .map(|token| token.as_str().to_owned())
            .or_else(|| {
                self.info
                    .env_key
                    .as_ref()
                    .and_then(|key| std::env::var(key).ok())
            })
            .filter(|token| !token.trim().is_empty())
    }
    /// Whether authentication is supplied by configuration rather than the account store.
    pub fn has_configured_token(&self) -> bool {
        self.environment_token().is_some()
    }
    fn scope_account(&self, account: String) -> String {
        match &self.info.base_url {
            Some(url) => format!("{account}@{}", codex_provider_auth::digest(url.as_bytes())),
            None => account,
        }
    }
    fn scope_credentials(&self, mut credentials: Credentials) -> Credentials {
        credentials.account_id = self.scope_account(credentials.account_id);
        credentials
    }
}

impl NativeBackend {
    pub fn provider(&self) -> codex_provider_api::Result<Arc<dyn Provider>> {
        let descriptor = self.descriptor()?;
        if let Some(provider) = self.transport.get() {
            return Ok(Arc::clone(provider));
        }
        let provider = descriptor.provider(TransportOptions {
            base_url: self.info.base_url.as_deref(),
            state_home: self.home.as_deref(),
        })?;
        let _ = self.transport.set(Arc::clone(&provider));
        Ok(self.transport.get().cloned().unwrap_or(provider))
    }
    pub fn credentials(&self, cancellation: CancellationToken) -> ProviderFuture<'_, Credentials> {
        Box::pin(async move {
            let descriptor = self.descriptor()?;
            let token = self.environment_token();
            if let Some(access_token) = token {
                return Ok(self.scope_credentials(Credentials {
                    account_id: format!(
                        "token-{}",
                        codex_provider_auth::digest(access_token.as_bytes())
                    ),
                    access_token,
                    metadata: serde_json::Value::Null,
                }));
            }
            descriptor
                .credentials(&self.auth_store()?, &cancellation)
                .await
                .map(|credentials| self.scope_credentials(credentials))
        })
    }
    fn ensure_stored_auth(&self) -> codex_provider_api::Result<()> {
        self.descriptor()?;
        if self.has_configured_token() {
            return Err(ProviderError::Authentication("remove the configured environment or bearer token override before changing stored login".into()));
        }
        Ok(())
    }
    pub async fn begin_login(&self) -> codex_provider_api::Result<NativeLogin> {
        self.ensure_stored_auth()?;
        NativeLogin::begin(self.descriptor()?).await
    }
    fn register_session(
        &self,
        account: &codex_provider_api::AccountRef,
        cancellation: CancellationToken,
    ) -> codex_provider_api::Result<()> {
        let Some(home) = &self.home else {
            return Ok(());
        };
        let mut sessions = account_sessions().lock().map_err(|_| {
            ProviderError::Authentication("account session registry unavailable".into())
        })?;
        sessions.retain(|_, values| {
            values.retain(|token| !token.is_cancelled());
            !values.is_empty()
        });
        sessions
            .entry((home.clone(), self.id().into(), account.identity()))
            .or_default()
            .push(cancellation);
        Ok(())
    }
    fn cancel_account_sessions(&self, account: &codex_provider_api::AccountRef) {
        if let Some(home) = &self.home
            && let Ok(mut sessions) = account_sessions().lock()
            && let Some(tokens) =
                sessions.remove(&(home.clone(), self.id().into(), account.identity()))
        {
            for token in tokens {
                token.cancel();
            }
        }
    }
    pub fn pool_store(&self) -> codex_provider_api::Result<codex_provider_runtime::PoolStore> {
        self.descriptor()?;
        let home = self.home.as_deref().ok_or_else(|| {
            ProviderError::Checkpoint("native provider requires local state storage".into())
        })?;
        Ok(codex_provider_runtime::PoolStore::new(
            home,
            self.id(),
            self.info.base_url.as_deref().unwrap_or(""),
        ))
    }
    pub async fn accounts(
        &self,
        thread: Option<&str>,
        cancellation: &CancellationToken,
    ) -> codex_provider_api::Result<codex_provider_runtime::PoolStatus> {
        let mut status = self.pool_store()?.status(thread, cancellation).await?;
        status.configured_token = self.has_configured_token();
        Ok(status)
    }
    pub async fn update_pool(
        &self,
        revision: u64,
        update: codex_provider_runtime::PoolUpdate,
        cancellation: &CancellationToken,
    ) -> codex_provider_api::Result<codex_provider_runtime::PoolStatus> {
        self.ensure_stored_auth()?;
        let pool = self.pool_store()?;
        match update {
            codex_provider_runtime::PoolUpdate::Prefer { label } => {
                pool.prefer(revision, label, cancellation).await?
            }
            update => {
                pool.update(revision, update, cancellation).await?;
            }
        }
        self.accounts(None, cancellation).await
    }
    pub async fn begin_account_login(
        &self,
        account: &str,
        cancellation: &CancellationToken,
    ) -> codex_provider_api::Result<codex_provider_auth::LoginOperation> {
        self.ensure_stored_auth()?;
        self.pool_store()?.settings(cancellation).await?;
        self.auth_store()?
            .begin_login(self.id(), account, cancellation)
            .await
    }
    pub async fn finish_account_login(
        &self,
        operation: &codex_provider_auth::LoginOperation,
        record: TokenRecord,
        select: bool,
        cancellation: &CancellationToken,
    ) -> codex_provider_api::Result<()> {
        self.ensure_stored_auth()?;
        if operation.provider != self.id() {
            return Err(ProviderError::Authentication(
                "login provider mismatch".into(),
            ));
        }
        let previous = self
            .auth_store()?
            .account_reference(self.id(), &operation.account)
            .ok();
        let pool = self.pool_store()?;
        self.auth_store()?
            .commit_login_with(operation, record, select, cancellation, |account| {
                pool.prepare_login(account, select, cancellation)
            })
            .await?;
        if let Some(previous) = previous {
            self.cancel_account_sessions(&previous);
        }
        pool.settings(&CancellationToken::new()).await.map(|_| ())
    }
    pub async fn commit_login(
        &self,
        account: &str,
        record: TokenRecord,
        cancellation: &CancellationToken,
    ) -> codex_provider_api::Result<()> {
        let operation = self.begin_account_login(account, cancellation).await?;
        self.finish_account_login(&operation, record, true, cancellation)
            .await
    }
    pub async fn login_token_with_selection(
        &self,
        account: &str,
        token: String,
        select: bool,
        cancellation: &CancellationToken,
    ) -> codex_provider_api::Result<()> {
        let operation = self.begin_account_login(account, cancellation).await?;
        self.finish_account_login(
            &operation,
            TokenRecord {
                access_token: token,
                refresh_token: None,
                expires_at: None,
                metadata: serde_json::Value::Null,
                revoked: false,
            },
            select,
            cancellation,
        )
        .await
    }
    pub async fn login_token(
        &self,
        account: &str,
        token: String,
        cancellation: &CancellationToken,
    ) -> codex_provider_api::Result<()> {
        self.login_token_with_selection(account, token, true, cancellation)
            .await
    }
    pub async fn logout_account_at_revision(
        &self,
        account: &str,
        revision: u64,
        cancellation: &CancellationToken,
    ) -> codex_provider_api::Result<()> {
        self.ensure_stored_auth()?;
        let previous = self
            .auth_store()?
            .account_reference(self.id(), account)
            .ok();
        let pool = self.pool_store()?;
        self.auth_store()?
            .logout_account_checked(self.id(), account, cancellation, || {
                pool.touch(revision, cancellation)
            })
            .await?;
        if let Some(previous) = previous {
            self.cancel_account_sessions(&previous);
        }
        Ok(())
    }
    pub async fn logout_account(
        &self,
        account: &str,
        cancellation: &CancellationToken,
    ) -> codex_provider_api::Result<()> {
        self.ensure_stored_auth()?;
        let previous = self
            .auth_store()?
            .account_reference(self.id(), account)
            .ok();
        self.auth_store()?
            .logout_account(self.id(), account, cancellation)
            .await?;
        if let Some(previous) = previous {
            self.cancel_account_sessions(&previous);
        }
        Ok(())
    }
    pub async fn logout(&self, cancellation: &CancellationToken) -> codex_provider_api::Result<()> {
        self.ensure_stored_auth()?;
        if let Some(account) = self.auth_store()?.selected(self.id())? {
            self.logout_account(&account, cancellation).await?;
        }
        Ok(())
    }
    pub fn status(&self) -> codex_provider_api::Result<Option<AccountInfo>> {
        self.descriptor()?;
        if self.has_configured_token() {
            return Ok(Some(AccountInfo {
                provider: self.id().into(),
                account: "configured token".into(),
                expires_at: None,
                revoked: false,
            }));
        }
        self.auth_store()?.status(self.id())
    }
    pub fn account(
        &self,
    ) -> codex_provider_api::Result<Option<codex_protocol::account::ProviderAccount>> {
        self.descriptor()?;
        if self.environment_token().is_some() {
            return Ok(Some(codex_protocol::account::ProviderAccount::Native {
                provider: self.id().into(),
                account_id: "configured token".into(),
            }));
        }
        let status = self.status()?;
        Ok(status.filter(|status| !status.revoked).map(|status| {
            codex_protocol::account::ProviderAccount::Native {
                provider: status.provider,
                account_id: status.account,
            }
        }))
    }
}
impl codex_provider_runtime::AccountCredentials for NativeBackend {
    fn configured_token(&self) -> bool {
        self.has_configured_token()
    }
    fn namespace_account(&self, account: &codex_provider_api::AccountRef) -> String {
        self.scope_account(account.identity())
    }
    fn credentials_for<'a>(
        &'a self,
        account: Option<&'a codex_provider_api::AccountRef>,
        cancellation: CancellationToken,
    ) -> ProviderFuture<'a, Credentials> {
        Box::pin(async move {
            let Some(account) = account.filter(|_| !self.has_configured_token()) else {
                return self.credentials(cancellation).await;
            };
            let credentials = self
                .descriptor()?
                .credentials_for(&self.auth_store()?, account, &cancellation)
                .await?;
            self.register_session(account, cancellation)?;
            codex_provider_runtime::AccountCredentials::validate_account(self, account)?;
            Ok(self.scope_credentials(credentials))
        })
    }
    fn refresh_rejected<'a>(
        &'a self,
        account: &'a codex_provider_api::AccountRef,
        token: &'a str,
        cancellation: CancellationToken,
    ) -> ProviderFuture<'a, Option<Credentials>> {
        Box::pin(async move {
            if self.has_configured_token()
                || self.descriptor()?.authentication
                    == codex_provider_registry::Authentication::ApiKey
            {
                return Ok(None);
            }
            if !self
                .auth_store()?
                .expire_rejected(self.id(), account, token, &cancellation)
                .await?
            {
                return Ok(None);
            }
            self.descriptor()?
                .credentials_for(&self.auth_store()?, account, &cancellation)
                .await
                .map(|credentials| Some(self.scope_credentials(credentials)))
        })
    }
    fn validate_account(
        &self,
        account: &codex_provider_api::AccountRef,
    ) -> codex_provider_api::Result<()> {
        if self
            .auth_store()?
            .account_reference(self.id(), &account.label)?
            != *account
        {
            return Err(ProviderError::Authentication(
                "account credentials were replaced; stop this attempt".into(),
            ));
        }
        Ok(())
    }
}
impl NativeBackend {
    fn discovery_account(&self) -> codex_provider_api::Result<codex_provider_api::AccountRef> {
        let thread = self
            .discovery_thread
            .lock()
            .map_err(|_| ProviderError::Checkpoint("model discovery context unavailable".into()))?
            .clone();
        if let Some(thread) = thread
            && let Some(account) = self.pool_store()?.affinity(&thread)?
            && self
                .auth_store()?
                .account_reference(self.id(), &account.label)
                .is_ok_and(|current| current == account)
        {
            return Ok(account);
        }
        self.pool_store()?
            .preferred_reference()?
            .map(Ok)
            .unwrap_or_else(|| self.auth_store()?.selected_reference(self.id()))
    }
}
impl ModelsEndpointClient for NativeBackend {
    fn bind_thread(&self, thread: &str) {
        if let Ok(mut current) = self.discovery_thread.lock() {
            *current = Some(thread.into());
        }
    }
    fn identity(&self) -> Option<String> {
        self.descriptor().ok()?;
        let account = match self.environment_token() {
            Some(token) => format!("token-{}", codex_provider_auth::digest(token.as_bytes())),
            None => self.discovery_account().ok()?.identity(),
        };
        let account = self.scope_account(account);
        Some(codex_provider_auth::digest(
            format!("{}:{account}:{:?}", self.id(), self.info.base_url).as_bytes(),
        ))
    }
    fn has_command_auth(&self) -> bool {
        true
    }
    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool> {
        Box::pin(async { false })
    }
    fn list_models<'a>(
        &'a self,
        _: &'a str,
        _: codex_http_client::HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, codex_protocol::error::Result<ModelsEndpointResponse>> {
        Box::pin(async move {
            let cancellation = CancellationToken::new();
            let account = if self.has_configured_token() {
                None
            } else {
                Some(
                    self.discovery_account()
                        .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?,
                )
            };
            let _guard = cancellation.clone().drop_guard();
            let credentials = codex_provider_runtime::AccountCredentials::credentials_for(
                self,
                account.as_ref(),
                cancellation.clone(),
            )
            .await
            .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
            let identity = codex_provider_auth::digest(
                format!(
                    "{}:{}:{:?}",
                    self.id(),
                    credentials.account_id,
                    self.info.base_url
                )
                .as_bytes(),
            );
            let models = self
                .provider()
                .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?
                .models(Context {
                    credentials,
                    cancellation,
                })
                .await
                .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
            let models = models
                .into_iter()
                .map(|model| {
                    let mut info =
                        codex_models_manager::model_info::model_info_from_slug(&model.id);
                    info.display_name = model.name;
                    info.context_window =
                        model.context_window.and_then(|value| value.try_into().ok());
                    info.max_context_window = info.context_window;
                    if let Some(supports_images) = model.supports_images {
                        info.input_modalities =
                            vec![codex_protocol::openai_models::InputModality::Text];
                        if supports_images {
                            info.input_modalities
                                .push(codex_protocol::openai_models::InputModality::Image);
                        }
                    }
                    info.supported_in_api = true;
                    info.visibility = codex_protocol::openai_models::ModelVisibility::List;
                    info
                })
                .collect();
            Ok(ModelsEndpointResponse {
                models,
                etag: None,
                identity,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_provider_api::Checkpoint;
    use codex_provider_api::Namespace;
    use codex_provider_runtime::SessionStore;
    use serde_json::json;
    #[tokio::test]
    async fn changing_native_endpoint_cannot_restore_another_endpoints_checkpoint() {
        let home = tempfile::tempdir().unwrap();
        let mut info = codex_model_provider_info::built_in_model_providers(None)
            .remove("opencode-go")
            .unwrap();
        info.experimental_bearer_token = Some("fixture-token".into());
        info.base_url = Some("https://first.example.test".into());
        let first = NativeBackend::new(info.clone(), Some(home.path().into()));
        info.base_url = Some("https://second.example.test".into());
        let second = NativeBackend::new(info, Some(home.path().into()));
        let cancellation = CancellationToken::new();
        let credentials = first.credentials(cancellation.clone()).await.unwrap();
        let namespace = Namespace {
            provider: "opencode-go".into(),
            account: credentials.account_id,
            model: "fixture".into(),
            thread: "thread".into(),
        };
        SessionStore::new(home.path(), namespace.clone())
            .save(
                &Checkpoint {
                    version: 1,
                    namespace: namespace.clone(),
                    context_hash: "history".into(),
                    state: json!({"private_continuation":"first endpoint"}),
                },
                &cancellation,
            )
            .await
            .unwrap();
        let credentials = second.credentials(cancellation).await.unwrap();
        assert_ne!(credentials.account_id, namespace.account);
        assert_ne!(first.identity(), second.identity());
        let other = Namespace {
            account: credentials.account_id,
            ..namespace
        };
        assert!(
            SessionStore::new(home.path(), other)
                .load()
                .unwrap()
                .is_none()
        );
    }
    struct FifthProvider;
    impl Provider for FifthProvider {
        fn id(&self) -> &'static str {
            "fifth-provider"
        }
        fn models(&self, context: Context) -> ProviderFuture<'_, Vec<codex_provider_api::Model>> {
            assert_eq!(context.credentials.access_token, "fixture-token");
            Box::pin(async {
                Ok(vec![codex_provider_api::Model {
                    id: "fifth-model".into(),
                    name: "Fifth model".into(),
                    context_window: Some(120_000),
                    supports_images: Some(true),
                }])
            })
        }
        fn start(
            &self,
            _request: codex_provider_api::Request,
            _context: Context,
        ) -> ProviderFuture<'_, Box<dyn codex_provider_api::ProviderSession>> {
            Box::pin(async {
                Err(ProviderError::Unsupported(
                    "catalog fixture does not infer".into(),
                ))
            })
        }
    }
    fn fifth_factory(
        options: TransportOptions<'_>,
    ) -> codex_provider_api::Result<Arc<dyn Provider>> {
        assert_eq!(options.base_url, Some("https://fifth.example.test"));
        assert!(options.state_home.is_some());
        Ok(Arc::new(FifthProvider))
    }
    static FIFTH: Descriptor = Descriptor {
        id: "fifth-provider",
        display_name: "Fifth provider",
        env_key: None,
        auxiliary_model: "fifth-model",
        legacy_wire_aliases: &[],
        factory: Some(fifth_factory),
        auth_driver: Some(&codex_provider_registry::AuthDriver::ApiKey),
        ..codex_provider_registry::OPENCODE
    };

    #[tokio::test]
    async fn registered_fifth_provider_uses_factory_catalog_credentials_and_account() {
        let mut registry = registry().clone();
        registry.register(&FIFTH).unwrap();
        let home = tempfile::tempdir().unwrap();
        let info = ModelProviderInfo {
            name: "Fifth".into(),
            wire_api: codex_model_provider_info::WireApi::Native,
            native_provider: Some("fifth-provider".into()),
            base_url: Some("https://fifth.example.test".into()),
            ..Default::default()
        };
        info.validate_with_registry(&registry).unwrap();
        let descriptor = registry.get(info.native_provider_id().unwrap()).unwrap();
        let backend = NativeBackend::with_descriptor(info, Some(home.path().into()), descriptor);
        let cancellation = CancellationToken::new();
        backend
            .login_token("account-five", "fixture-token".into(), &cancellation)
            .await
            .unwrap();
        assert_eq!(backend.id(), "fifth-provider");
        assert_eq!(backend.descriptor().unwrap().auxiliary_model, "fifth-model");
        assert!(Arc::ptr_eq(
            &backend.provider().unwrap(),
            &backend.provider().unwrap()
        ));
        assert_eq!(
            backend.account().unwrap(),
            Some(codex_protocol::account::ProviderAccount::Native {
                provider: "fifth-provider".into(),
                account_id: "account-five".into()
            })
        );
        let factory = codex_http_client::HttpClientFactory::new(
            codex_http_client::OutboundProxyPolicy::ReqwestDefault,
        );
        let response = backend.list_models("fixture", factory).await.unwrap();
        assert_eq!(response.models[0].slug, "fifth-model");
        assert_eq!(response.models[0].display_name, "Fifth model");
        assert_eq!(response.models[0].context_window, Some(120_000));
        assert!(
            response.models[0]
                .input_modalities
                .contains(&codex_protocol::openai_models::InputModality::Image)
        );
        assert_eq!(
            backend.identity().as_deref(),
            Some(response.identity.as_str())
        );
        backend.logout(&cancellation).await.unwrap();
        assert_eq!(backend.account().unwrap(), None);
    }

    #[tokio::test]
    async fn fifth_provider_pool_add_affinity_discovery_and_logout_are_isolated() {
        use codex_provider_runtime::AccountCredentials;
        let home = tempfile::tempdir().unwrap();
        let info = ModelProviderInfo {
            name: "Fifth".into(),
            wire_api: codex_model_provider_info::WireApi::Native,
            native_provider: Some("fifth-provider".into()),
            base_url: Some("https://fifth.example.test".into()),
            ..Default::default()
        };
        let backend = NativeBackend::with_descriptor(info, Some(home.path().into()), &FIFTH);
        let cancel = CancellationToken::new();
        backend
            .login_token("a", "fixture-token".into(), &cancel)
            .await
            .unwrap();
        backend
            .login_token_with_selection("b", "second-token".into(), false, &cancel)
            .await
            .unwrap();
        let status = backend.accounts(None, &cancel).await.unwrap();
        assert!(status.enabled);
        assert_eq!(status.selected.as_deref(), Some("a"));
        let b = backend
            .auth_store()
            .unwrap()
            .account_reference(backend.id(), "b")
            .unwrap();
        backend
            .pool_store()
            .unwrap()
            .accept("thread-b", &b, &cancel)
            .await
            .unwrap();
        let a_identity = backend.identity().unwrap();
        let mut thread_backend = backend.clone();
        thread_backend.discovery_thread = Arc::new(std::sync::Mutex::new(None));
        thread_backend.bind_thread("thread-b");
        assert_ne!(thread_backend.identity().unwrap(), a_identity);
        assert_eq!(backend.identity().unwrap(), a_identity);
        let a = backend
            .auth_store()
            .unwrap()
            .account_reference(backend.id(), "a")
            .unwrap();
        let a_cancel = cancel.child_token();
        let b_cancel = cancel.child_token();
        AccountCredentials::credentials_for(&backend, Some(&a), a_cancel.clone())
            .await
            .unwrap();
        AccountCredentials::credentials_for(&backend, Some(&b), b_cancel.clone())
            .await
            .unwrap();
        backend.logout_account("a", &cancel).await.unwrap();
        assert!(a_cancel.is_cancelled());
        assert!(!b_cancel.is_cancelled());
        assert!(!cancel.is_cancelled());
        assert!(backend.accounts(None, &cancel).await.unwrap().enabled);
        let status = backend.accounts(None, &cancel).await.unwrap();
        backend
            .update_pool(
                status.settings.revision,
                codex_provider_runtime::PoolUpdate::Disable,
                &cancel,
            )
            .await
            .unwrap();
        backend
            .login_token_with_selection("c", "third".into(), false, &cancel)
            .await
            .unwrap();
        assert!(!backend.accounts(None, &cancel).await.unwrap().enabled);
        let json = serde_json::to_string(&backend.accounts(None, &cancel).await.unwrap()).unwrap();
        assert!(!json.contains("fixture-token"));
        assert!(!json.contains("second-token"));
    }

    #[tokio::test]
    async fn unknown_native_provider_operations_return_errors_without_panicking() {
        let home = tempfile::tempdir().unwrap();
        let info = ModelProviderInfo {
            wire_api: codex_model_provider_info::WireApi::Native,
            native_provider: Some("unknown".into()),
            experimental_bearer_token: Some("configured-token".into()),
            ..Default::default()
        };
        let backend = NativeBackend::new(info, Some(home.path().into()));
        let cancellation = CancellationToken::new();
        assert!(backend.descriptor().is_err());
        assert!(backend.provider().is_err());
        assert!(backend.status().is_err());
        assert!(backend.account().is_err());
        assert!(backend.credentials(cancellation.clone()).await.is_err());
        assert!(backend.begin_login().await.is_err());
        assert!(
            backend
                .login_token("account", "token".into(), &cancellation)
                .await
                .is_err()
        );
        assert!(backend.logout(&cancellation).await.is_err());
    }
}
