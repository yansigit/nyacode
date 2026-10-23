//! Temporary ownership has no StateRuntime, SavedChatStore, recovery journal,
//! generic composer history, or filesystem-backed transcript. Drafts disappear
//! when the ownership generation ends, including disablement and shutdown.

mod turns;
mod uploads;
pub use turns::StartTemporaryTurn;
pub use turns::TemporaryPending;

use super::Scope;
use super::ServiceError;
use super::lock;
use codex_chatgpt_chat::ChatError;
use codex_chatgpt_chat::temporary_session::TemporaryChatSession;
use codex_state::chat::ChatDraft;
use codex_state::chat::ChatDraftContent;
use codex_state::chat::ChatDraftPage;
use codex_state::chat::ChatDraftSummary;
use codex_state::chat::ChatPageCursor;
use codex_state::chat::ChatStoreError;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_DRAFTS: usize = 32;
const MAX_DRAFT_BYTES: usize = 8 * 1024 * 1024;
const MAX_NATIVE_SESSIONS: usize = 8;

pub struct TemporarySession {
    scope: Arc<Scope>,
    stopped: CancellationToken,
    stager: Arc<codex_chatgpt_chat::attachment::AttachmentStager>,
    uploads: Mutex<HashMap<Uuid, super::uploads::UploadEntry>>,
    memory: Mutex<Memory>,
}

#[derive(Default)]
struct Memory {
    drafts: HashMap<Uuid, ChatDraft>,
    native: HashMap<Uuid, Arc<TemporaryChatSession>>,
    turns: turns::Turns,
}

impl TemporarySession {
    pub(super) fn new(
        scope: Arc<Scope>,
        stager: Arc<codex_chatgpt_chat::attachment::AttachmentStager>,
    ) -> Self {
        Self {
            stopped: scope.stopped.child_token(),
            stager,
            uploads: Mutex::new(HashMap::new()),
            scope,
            memory: Mutex::new(Memory::default()),
        }
    }

    pub fn scope(&self) -> Arc<Scope> {
        Arc::clone(&self.scope)
    }
    pub fn scope_id(&self) -> Uuid {
        self.scope.id()
    }

    pub fn check(&self) -> Result<(), ServiceError> {
        self.scope.check()?;
        if self.stopped.is_cancelled() {
            return Err(ServiceError::ScopeChanged);
        }
        Ok(())
    }

    pub(super) fn stop(&self) {
        self.stopped.cancel();
        let mut memory = lock(&self.memory);
        for operation in memory.turns.operations.values() {
            operation.cancel.cancel();
        }
        for native in memory.native.values() {
            native.stop();
        }
        *memory = Memory::default();
        lock(&self.uploads).clear();
    }

    pub fn draft_read(&self, id: Uuid) -> Result<Option<ChatDraft>, ServiceError> {
        let memory = lock(&self.memory);
        self.check()?;
        Ok(memory.drafts.get(&id).cloned())
    }

    /// Revisions apply to temporary drafts too, including across local clients.
    /// A new draft cannot import a saved or foreign temporary conversation ID.
    pub fn draft_write(
        &self,
        id: Uuid,
        expected_revision: Option<Uuid>,
        content: ChatDraftContent,
    ) -> Result<ChatDraft, ServiceError> {
        content.validate()?;
        let mut memory = lock(&self.memory);
        self.check()?;
        let previous = memory.drafts.get(&id);
        if previous.map(|draft| draft.revision) != expected_revision {
            return Err(ChatStoreError::RevisionConflict.into());
        }
        if content.conversation_id.as_ref()
            != previous.and_then(|draft| draft.content.conversation_id.as_ref())
        {
            return Err(ChatError::BranchConflict.into());
        }
        if previous.is_none() && memory.drafts.len() >= MAX_DRAFTS {
            return Err(ServiceError::Capacity);
        }
        let used = memory
            .drafts
            .values()
            .filter(|draft| draft.id != id)
            .map(|draft| content_weight(&draft.content))
            .sum::<usize>();
        if used.saturating_add(content_weight(&content)) > MAX_DRAFT_BYTES {
            return Err(ServiceError::Capacity);
        }
        let draft = ChatDraft {
            id,
            revision: Uuid::new_v4(),
            content,
            updated_at_ms: previous
                .map_or(0, |draft| draft.updated_at_ms.saturating_add(1))
                .max(chrono::Utc::now().timestamp_millis()),
        };
        memory.drafts.insert(id, draft.clone());
        Ok(draft)
    }

    pub fn draft_delete(&self, id: Uuid, expected_revision: Uuid) -> Result<(), ServiceError> {
        let mut memory = lock(&self.memory);
        self.check()?;
        let draft = memory.drafts.get(&id).ok_or(ChatStoreError::NotFound)?;
        if draft.revision != expected_revision {
            return Err(ChatStoreError::RevisionConflict.into());
        }
        if memory.turns.operations.values().any(|operation| {
            let snapshot = operation.snapshot();
            snapshot.draft_id == id && snapshot.phase != super::OperationPhase::Finished
        }) {
            return Err(ServiceError::OverlappingTurn);
        }
        memory.turns.pending.remove(&id);
        memory.turns.conversations.remove(&id);
        memory
            .turns
            .operations
            .retain(|_, operation| operation.snapshot().draft_id != id);
        // Explicitly removing this local temporary draft also revokes its
        // native capabilities and stops any local use of its conduit.
        if let Some(native) = memory.native.remove(&id) {
            native.stop();
        }
        memory.drafts.remove(&id);
        lock(&self.uploads).retain(|_, entry| entry.draft_id != id);
        Ok(())
    }

    pub fn draft_list(
        &self,
        before: Option<ChatPageCursor>,
        limit: usize,
    ) -> Result<ChatDraftPage, ServiceError> {
        if !(1..=100).contains(&limit) {
            return Err(ChatStoreError::InvalidInput.into());
        }
        let memory = lock(&self.memory);
        self.check()?;
        let mut drafts: Vec<_> = memory
            .drafts
            .values()
            .filter(|draft| {
                before.is_none_or(|cursor| {
                    (draft.updated_at_ms, draft.id) < (cursor.updated_at_ms, cursor.id)
                })
            })
            .collect();
        drafts.sort_by_key(|draft| std::cmp::Reverse((draft.updated_at_ms, draft.id)));
        let more = drafts.len() > limit;
        let items: Vec<_> = drafts
            .into_iter()
            .take(limit)
            .map(|draft| ChatDraftSummary {
                id: draft.id,
                revision: draft.revision,
                conversation_id: draft.content.conversation_id.clone(),
                requested_model: draft.content.requested_model.clone(),
                preview: draft.content.text.chars().take(160).collect(),
                attachment_count: draft.content.selections.len(),
                updated_at_ms: draft.updated_at_ms,
            })
            .collect();
        let next = items.last().filter(|_| more).map(|draft| ChatPageCursor {
            id: draft.id,
            updated_at_ms: draft.updated_at_ms,
        });
        Ok(ChatDraftPage { items, next })
    }

    /// Native state is created only on submission/upload. It uses an ephemeral
    /// device and is bound to this exact draft and ownership generation.
    pub fn native(&self, draft: Uuid) -> Result<Arc<TemporaryChatSession>, ServiceError> {
        let mut memory = lock(&self.memory);
        self.check()?;
        if !memory.drafts.contains_key(&draft) {
            return Err(ChatStoreError::NotFound.into());
        }
        if let Some(native) = memory.native.get(&draft) {
            return Ok(Arc::clone(native));
        }
        if memory.native.len() >= MAX_NATIVE_SESSIONS {
            return Err(ServiceError::Capacity);
        }
        let native = Arc::new(TemporaryChatSession::new(
            Arc::clone(&self.scope.auth),
            true,
            &self.scope.account,
            draft,
        )?);
        self.check()?;
        memory.native.insert(draft, Arc::clone(&native));
        Ok(native)
    }
}

impl Drop for TemporarySession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn content_weight(content: &ChatDraftContent) -> usize {
    // A conservative bound for retained strings and metadata, independent of
    // attachment byte staging (which has its own shared semaphore).
    2048 + content.text.len()
        + content
            .selections
            .iter()
            .map(|selection| {
                1024 + selection.name.len()
                    + selection
                        .source
                        .as_ref()
                        .map_or(0, |path| path.as_os_str().len())
            })
            .sum::<usize>()
}

#[cfg(test)]
mod tests;
