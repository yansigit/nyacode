//! Thin model-provider facade for the subsystem's Codex-facing native backend.
use crate::ModelProvider;
use crate::ModelProviderFuture;
use crate::ProviderAccountResult;
use crate::ProviderAccountState;
use crate::ProviderCapabilities;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::RemoteCompactionSupport;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::openai_models::ModelsResponse;
use codex_provider_adapter::backend::NativeBackend;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct NativeModelProvider {
    backend: Arc<NativeBackend>,
}
impl NativeModelProvider {
    pub fn new(info: ModelProviderInfo, home: Option<PathBuf>) -> Self {
        Self {
            backend: Arc::new(NativeBackend::new(info, home)),
        }
    }
}
impl ModelProvider for NativeModelProvider {
    fn backend(&self) -> Option<Arc<NativeBackend>> {
        Some(Arc::clone(&self.backend))
    }
    fn info(&self) -> &ModelProviderInfo {
        self.backend.info()
    }
    fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        None
    }
    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>> {
        Box::pin(async { None })
    }
    fn account_state(&self) -> ProviderAccountResult {
        self.backend
            .account()
            .map(|account| ProviderAccountState {
                account,
                requires_openai_auth: false,
            })
            .map_err(|_| crate::ProviderAccountError::NativeStateUnavailable)
    }
    fn capabilities(&self) -> ProviderCapabilities {
        let capabilities = self
            .backend
            .descriptor()
            .map(|descriptor| descriptor.capabilities)
            .unwrap_or_default();
        ProviderCapabilities {
            image_generation: capabilities.image_generation,
            web_search: capabilities.web_search,
            external_web_access: capabilities.external_web_access,
            remote_compaction: RemoteCompactionSupport::Unsupported,
        }
    }
    fn approval_review_preferred_model(&self) -> &'static str {
        self.backend
            .descriptor()
            .map(|descriptor| descriptor.auxiliary_model)
            .unwrap_or(crate::provider::DEFAULT_APPROVAL_REVIEW_PREFERRED_MODEL)
    }
    fn memory_extraction_preferred_model(&self) -> &'static str {
        self.backend
            .descriptor()
            .map(|descriptor| descriptor.auxiliary_model)
            .unwrap_or(crate::provider::DEFAULT_MEMORY_EXTRACTION_PREFERRED_MODEL)
    }
    fn memory_consolidation_preferred_model(&self) -> &'static str {
        self.backend
            .descriptor()
            .map(|descriptor| descriptor.auxiliary_model)
            .unwrap_or(crate::provider::DEFAULT_MEMORY_CONSOLIDATION_PREFERRED_MODEL)
    }
    fn models_manager(
        &self,
        _home: PathBuf,
        catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        self.backend.models_manager(catalog)
    }
    fn models_manager_without_cache(&self, catalog: Option<ModelsResponse>) -> SharedModelsManager {
        self.backend.models_manager(catalog)
    }
}
