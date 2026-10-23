//! App-server ownership of native Chat sessions. No content-bearing type derives
//! Debug, and no operation uses the Codex thread, prompt history, or tool path.
//!
//! The transport acceptor supplies ConnectionOrigin. RPC parameters must never
//! choose it. A scope UUID is an opaque generation, not an account identifier.

mod cache;
mod operation;
mod saved;
mod temporary;
mod uploads;

pub use operation::Operation;
pub use operation::OperationPhase;
pub use operation::OperationSnapshot;
pub use operation::Persistence;
pub use saved::StartSavedTurn;
pub use temporary::StartTemporaryTurn;
pub use temporary::TemporaryPending;
pub use temporary::TemporarySession;
pub use uploads::SelectedUpload;
pub use uploads::UploadBatch;
pub use uploads::UploadSelection;

use crate::transport::ConnectionOrigin;
use codex_chatgpt_chat::ChatError;
use codex_chatgpt_chat::attachment::AttachmentError;
use codex_chatgpt_chat::attachment::AttachmentStager;
use codex_chatgpt_chat::session::NATIVE_PROFILE_VERSION;
use codex_chatgpt_chat::session::SavedChatSession;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_rollout::StateDbHandle;
use codex_state::chat::ChatScope;
use codex_state::chat::ChatStoreError;
use codex_state::chat::SavedChatStore;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    #[error("Chat requires an embedded, stdio, or trusted local socket connection")]
    RemoteConnection,
    #[error("the Chat scope changed; refresh the session snapshot")]
    ScopeChanged,
    #[error("another turn is running in this draft or conversation")]
    OverlappingTurn,
    #[error("Chat is at capacity; retain the draft and try later")]
    Capacity,
    #[error(transparent)]
    Native(#[from] ChatError),
    #[error(transparent)]
    Store(#[from] ChatStoreError),
    #[error(transparent)]
    Attachment(#[from] AttachmentError),
}

/// Created once per app-server and owned alongside the shared AuthManager.
pub struct Service {
    auth: Arc<AuthManager>,
    state_db: Option<StateDbHandle>,
    stager: Arc<AttachmentStager>,
    state: Mutex<ServiceState>,
    shutdown: CancellationToken,
    changes: tokio::sync::watch::Sender<u64>,
}

struct ServiceState {
    enabled: bool,
    saved: Option<Arc<SavedSession>>,
    scope: Option<Arc<Scope>>,
    temporary: Option<Arc<TemporarySession>>,
}

impl Service {
    pub fn new(
        auth: Arc<AuthManager>,
        state_db: Option<StateDbHandle>,
        enabled: bool,
    ) -> Arc<Self> {
        let service = Arc::new(Self {
            auth,
            state_db,
            stager: Arc::new(AttachmentStager::new()),
            state: Mutex::new(ServiceState {
                enabled,
                saved: None,
                scope: None,
                temporary: None,
            }),
            shutdown: CancellationToken::new(),
            changes: tokio::sync::watch::channel(0).0,
        });
        let weak = Arc::downgrade(&service);
        let mut changes = service.auth.auth_change_state_receiver();
        let shutdown = service.shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    result = changes.changed() => if result.is_err() { break; },
                }
                let Some(service) = weak.upgrade() else { break };
                let generation = changes.borrow_and_update().generation;
                let mut state = lock(&service.state);
                if state
                    .scope
                    .as_ref()
                    .is_some_and(|scope| scope.generation != generation)
                {
                    state.revoke();
                }
                service.changes.send_modify(|revision| *revision += 1);
            }
        });
        service
    }

    /// Synchronous revocation: disable/enable cannot revive old operations even
    /// if a watcher coalesces both configuration changes into one notification.
    pub fn set_enabled(&self, enabled: bool) {
        let mut state = lock(&self.state);
        let previous = state.enabled;
        state.enabled = enabled && !self.shutdown.is_cancelled();
        if !state.enabled {
            state.revoke();
        }
        if state.enabled != previous {
            self.changes.send_modify(|revision| *revision += 1);
        }
    }

    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
        self.set_enabled(false);
    }

    /// Creates only an in-memory identity guard. Temporary operations can
    /// obtain it without creating a saved session or touching the state database.
    pub fn open_scope(
        &self,
        origin: ConnectionOrigin,
        expected_scope: Option<Uuid>,
    ) -> Result<Arc<Scope>, ServiceError> {
        check_origin(origin)?;
        let mut state = lock(&self.state);
        if !state.enabled || self.shutdown.is_cancelled() {
            return Err(ChatError::Disabled.into());
        }
        let generation = self.auth.auth_change_state_receiver().borrow().generation;
        if state
            .scope
            .as_ref()
            .is_some_and(|scope| scope.generation != generation)
        {
            state.revoke();
        }
        if let Some(scope) = &state.scope {
            scope.check()?;
            if expected_scope.is_some_and(|id| id != scope.id) {
                return Err(ServiceError::ScopeChanged);
            }
            return Ok(Arc::clone(scope));
        }
        if expected_scope.is_some() {
            return Err(ServiceError::ScopeChanged);
        }
        let auth = self.auth.auth_cached().ok_or(ChatError::LoginRequired)?;
        if !matches!(
            auth,
            CodexAuth::Chatgpt(_) | CodexAuth::ChatgptAuthTokens(_)
        ) {
            return Err(ChatError::LoginRequired.into());
        }
        let principal = auth.get_chatgpt_user_id().ok_or(ChatError::LoginRequired)?;
        let account = auth.get_account_id().ok_or(ChatError::LoginRequired)?;
        let scope = Arc::new(Scope {
            id: Uuid::new_v4(),
            generation,
            auth: Arc::clone(&self.auth),
            principal,
            workspace: auth.is_workspace_account().then(|| account.clone()),
            account,
            stopped: CancellationToken::new(),
        });
        scope.check()?;
        state.scope = Some(Arc::clone(&scope));
        Ok(scope)
    }

    /// Origin and current ownership are checked before constructing a saved store.
    pub fn open_saved(
        &self,
        origin: ConnectionOrigin,
        expected_scope: Option<Uuid>,
    ) -> Result<Arc<SavedSession>, ServiceError> {
        let scope = self.open_scope(origin, expected_scope)?;
        let mut state = lock(&self.state);
        scope.check()?;
        if let Some(session) = &state.saved {
            session.check()?;
            return Ok(Arc::clone(session));
        }
        let store_scope = ChatScope::new(
            scope.principal.clone(),
            scope.account.clone(),
            scope.workspace.clone(),
        )?;
        let session = Arc::new(SavedSession {
            scope_id: scope.id,
            generation: scope.generation,
            auth: Arc::clone(&self.auth),
            account: scope.account.clone(),
            store: self.state_db.as_ref().map(|db| db.saved_chat(store_scope)),
            native: OnceCell::new(),
            read_only_native: OnceCell::new(),
            stopped: scope.stopped.child_token(),
            owner: scope,
            stager: Arc::clone(&self.stager),
            uploads: Mutex::new(HashMap::new()),
            operations: Mutex::new(HashMap::new()),
            models: Mutex::new(None),
        });
        session.check()?;
        state.saved = Some(Arc::clone(&session));
        Ok(session)
    }

    pub fn open_temporary(
        &self,
        origin: ConnectionOrigin,
        expected_scope: Option<Uuid>,
    ) -> Result<Arc<TemporarySession>, ServiceError> {
        let scope = self.open_scope(origin, expected_scope)?;
        let mut state = lock(&self.state);
        scope.check()?;
        if let Some(session) = &state.temporary {
            session.check()?;
            return Ok(Arc::clone(session));
        }
        let session = Arc::new(TemporarySession::new(scope, Arc::clone(&self.stager)));
        state.temporary = Some(Arc::clone(&session));
        Ok(session)
    }

    pub(crate) fn recovery_configured(&self) -> bool {
        self.state_db.is_some()
    }

    pub(crate) fn saved_in_scope(
        &self,
        scope: &Scope,
    ) -> Result<Option<Arc<SavedSession>>, ServiceError> {
        let state = lock(&self.state);
        scope.check()?;
        if state.scope.as_ref().map(|current| current.id()) != Some(scope.id()) {
            return Err(ServiceError::ScopeChanged);
        }
        Ok(state.saved.clone())
    }

    pub(crate) fn temporary_in_scope(
        &self,
        scope: &Scope,
    ) -> Result<Option<Arc<TemporarySession>>, ServiceError> {
        let state = lock(&self.state);
        scope.check()?;
        if state.scope.as_ref().map(|current| current.id()) != Some(scope.id()) {
            return Err(ServiceError::ScopeChanged);
        }
        Ok(state.temporary.clone())
    }
}

impl ServiceState {
    fn revoke(&mut self) {
        // Stop owners before propagating their shared cancellation token so
        // each owner still runs its native-capability and lease cleanup.
        if let Some(saved) = self.saved.take() {
            saved.stop();
        }
        if let Some(temporary) = self.temporary.take() {
            temporary.stop();
        }
        if let Some(scope) = self.scope.take() {
            scope.stopped.cancel();
        }
    }
}

/// An account/workspace/auth-generation guard with no database or transcript.
/// Saved and temporary owners share its opaque ID, never their content stores.
pub struct Scope {
    id: Uuid,
    generation: u64,
    auth: Arc<AuthManager>,
    principal: String,
    account: String,
    workspace: Option<String>,
    stopped: CancellationToken,
}

impl Scope {
    pub fn id(&self) -> Uuid {
        self.id
    }
    pub fn check(&self) -> Result<(), ServiceError> {
        if self.stopped.is_cancelled()
            || self.auth.auth_change_state_receiver().borrow().generation != self.generation
        {
            return Err(ServiceError::ScopeChanged);
        }
        Ok(())
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Saved-only service state. Temporary operations use a distinct memory-only
/// owner and must not obtain this session or its store.
pub struct SavedSession {
    owner: Arc<Scope>,
    scope_id: Uuid,
    generation: u64,
    auth: Arc<AuthManager>,
    account: String,
    store: Option<SavedChatStore>,
    native: OnceCell<Arc<SavedChatSession>>,
    read_only_native: OnceCell<Arc<SavedChatSession>>,
    stopped: CancellationToken,
    stager: Arc<AttachmentStager>,
    uploads: Mutex<HashMap<Uuid, uploads::UploadEntry>>,
    operations: Mutex<HashMap<Uuid, Operation>>,
    models: Mutex<Option<codex_chatgpt_chat::models::ModelCatalog>>,
}

impl SavedSession {
    pub fn scope(&self) -> Arc<Scope> {
        Arc::clone(&self.owner)
    }

    pub fn scope_id(&self) -> Uuid {
        self.scope_id
    }

    pub fn check(&self) -> Result<(), ServiceError> {
        self.owner.check()?;
        if self.stopped.is_cancelled() {
            return Err(ServiceError::ScopeChanged);
        }
        if self.auth.auth_change_state_receiver().borrow().generation != self.generation {
            return Err(ServiceError::ScopeChanged);
        }
        Ok(())
    }

    fn stop(&self) {
        if self.stopped.is_cancelled() {
            return;
        }
        self.stopped.cancel();
        if let Some(native) = self.native.get() {
            native.stop();
        }
        if let Some(native) = self.read_only_native.get() {
            native.stop();
        }
        lock(&self.uploads).clear();
        lock(&self.operations).clear();
        lock(&self.models).take();
        if let Some(store) = self.store.clone() {
            let owner = self.scope_id;
            // Expire leases so a new local owner can reconcile. Even a crash
            // before this cleanup only delays reconciliation until lease expiry.
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = store.release_owner(owner).await;
                });
            }
        }
    }

    async fn guarded<T>(
        &self,
        work: impl Future<Output = Result<T, ServiceError>>,
    ) -> Result<T, ServiceError> {
        self.check()?;
        let mut changes = self.auth.auth_change_state_receiver();
        let result = tokio::select! {
            biased;
            _ = self.stopped.cancelled() => return Err(ServiceError::ScopeChanged),
            _ = changes.wait_for(|state| state.generation != self.generation) => return Err(ServiceError::ScopeChanged),
            result = work => result,
        };
        self.check()?;
        result
    }

    fn store(&self) -> Result<&SavedChatStore, ServiceError> {
        self.check()?;
        self.store
            .as_ref()
            .ok_or(ChatStoreError::Unavailable.into())
    }

    async fn native(&self) -> Result<Arc<SavedChatSession>, ServiceError> {
        self.guarded(async {
            self.native
                .get_or_try_init(|| async {
                    let profile = self.store()?.profile(NATIVE_PROFILE_VERSION).await?;
                    self.check()?;
                    Ok(Arc::new(SavedChatSession::new(
                        Arc::clone(&self.auth),
                        true,
                        &self.account,
                        profile.device_id,
                    )?))
                })
                .await
                .cloned()
        })
        .await
    }

    async fn reader(&self) -> Result<Arc<SavedChatSession>, ServiceError> {
        self.check()?;
        if let Some(native) = self.native.get() {
            return Ok(Arc::clone(native));
        }
        // Normal reads and writes share the persisted device and native pool.
        // A storage failure must still leave authoritative history readable.
        if self.store.is_some() {
            match self.native().await {
                Ok(native) => return Ok(native),
                Err(ServiceError::Store(_)) => self.check()?,
                Err(error) => return Err(error),
            }
        }
        // History remains available if recovery storage is unavailable. This
        // ephemeral device is never used by a send or attachment operation.
        self.guarded(async {
            self.read_only_native
                .get_or_try_init(|| async {
                    Ok(Arc::new(SavedChatSession::new(
                        Arc::clone(&self.auth),
                        true,
                        &self.account,
                        Uuid::new_v4(),
                    )?))
                })
                .await
                .cloned()
        })
        .await
    }

    pub async fn conversation_list(
        &self,
        offset: u32,
        limit: u32,
    ) -> Result<codex_chatgpt_chat::conversation::ConversationPage, ServiceError> {
        self.guarded(async {
            Ok(self
                .reader()
                .await?
                .conversation_list(offset, limit)
                .await?)
        })
        .await
    }

    pub fn recovery_configured(&self) -> bool {
        self.store.is_some()
    }

    pub fn model_limits(
        &self,
    ) -> Result<Option<Vec<codex_chatgpt_chat::models::ModelLimit>>, ServiceError> {
        self.check()?;
        Ok(lock(&self.models)
            .as_ref()
            .and_then(|catalog| catalog.limits.clone()))
    }

    pub async fn model_catalog(
        &self,
    ) -> Result<codex_chatgpt_chat::models::ModelCatalog, ServiceError> {
        let catalog = self
            .guarded(async { Ok(self.reader().await?.model_catalog().await?) })
            .await?;
        let mut models = lock(&self.models);
        self.check()?;
        *models = Some(catalog.clone());
        self.check()?;
        Ok(catalog)
    }

    pub async fn conversation_read(
        &self,
        id: &str,
    ) -> Result<codex_chatgpt_chat::conversation::ConversationSnapshot, ServiceError> {
        self.guarded(async {
            let observed_at_ms = chrono::Utc::now().timestamp_millis();
            let snapshot = self.reader().await?.conversation_read(id).await?;
            self.cache_authoritative_conversation(&snapshot, observed_at_ms)
                .await;
            Ok(snapshot)
        })
        .await
    }

    pub async fn draft_read(
        &self,
        id: Uuid,
    ) -> Result<Option<codex_state::chat::ChatDraft>, ServiceError> {
        self.guarded(async { Ok(self.store()?.read_draft(id).await?) })
            .await
    }

    pub async fn draft_write(
        &self,
        id: Uuid,
        revision: Option<Uuid>,
        content: codex_state::chat::ChatDraftContent,
    ) -> Result<codex_state::chat::ChatDraft, ServiceError> {
        self.guarded(async { Ok(self.store()?.write_draft(id, revision, content).await?) })
            .await
    }

    pub async fn draft_delete(&self, id: Uuid, revision: Uuid) -> Result<(), ServiceError> {
        self.guarded(async { Ok(self.store()?.delete_draft(id, revision).await?) })
            .await?;
        lock(&self.uploads).retain(|_, entry| entry.draft_id != id);
        Ok(())
    }

    pub async fn draft_list(
        &self,
        before: Option<codex_state::chat::ChatPageCursor>,
        limit: usize,
    ) -> Result<codex_state::chat::ChatDraftPage, ServiceError> {
        self.guarded(async { Ok(self.store()?.list_drafts(before, limit).await?) })
            .await
    }

    pub async fn pending_submission_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
        self.guarded(async { Ok(self.store()?.pending_submission_ids().await?) })
            .await
    }

    pub async fn pending_for_draft(
        &self,
        id: Uuid,
    ) -> Result<Option<codex_state::chat::ChatSubmission>, ServiceError> {
        self.guarded(async { Ok(self.store()?.pending_submission_for_draft(id).await?) })
            .await
    }

    pub async fn pending_for_conversation(
        &self,
        id: &str,
    ) -> Result<Option<codex_state::chat::ChatSubmission>, ServiceError> {
        self.guarded(async {
            Ok(self
                .store()?
                .pending_submission_for_conversation(id)
                .await?)
        })
        .await
    }

    pub async fn submission_read(
        &self,
        id: Uuid,
    ) -> Result<Option<codex_state::chat::ChatSubmission>, ServiceError> {
        self.guarded(async { Ok(self.store()?.read_submission(id).await?) })
            .await
    }
}

impl Drop for SavedSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn check_origin(origin: ConnectionOrigin) -> Result<(), ServiceError> {
    match origin {
        ConnectionOrigin::InProcess | ConnectionOrigin::Stdio | ConnectionOrigin::LocalSocket => {
            Ok(())
        }
        ConnectionOrigin::WebSocket | ConnectionOrigin::RemoteControl => {
            Err(ServiceError::RemoteConnection)
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
