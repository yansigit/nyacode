//! Route explicit persistence modes to disjoint owners. Temporary calls never
//! acquire SavedSession or SavedChatStore, including when recovery is unavailable.
use super::*;
use crate::chat::Scope;
use crate::chat::TemporarySession;
use codex_chatgpt_chat::upload::UploadStage;
use codex_state::chat::ChatDraft;
use codex_state::chat::ChatDraftContent;
use codex_state::chat::ChatDraftPage;
use codex_state::chat::ChatPageCursor;

#[derive(Clone)]
pub(super) enum Session {
    Saved(Arc<SavedSession>),
    Temporary(Arc<TemporarySession>),
}

impl Session {
    pub(super) fn scope(&self) -> Arc<Scope> {
        match self {
            Self::Saved(session) => session.scope(),
            Self::Temporary(session) => session.scope(),
        }
    }
    pub(super) fn scope_id(&self) -> Uuid {
        self.scope().id()
    }
    pub(super) fn check(&self) -> Result<(), ServiceError> {
        match self {
            Self::Saved(session) => session.check(),
            Self::Temporary(session) => session.check(),
        }
    }
    pub(super) fn persistence(&self) -> rpc::ChatPersistence {
        match self {
            Self::Saved(_) => rpc::ChatPersistence::Saved,
            Self::Temporary(_) => rpc::ChatPersistence::Temporary,
        }
    }
    pub(super) async fn draft_read(&self, id: Uuid) -> Result<Option<ChatDraft>, ServiceError> {
        match self {
            Self::Saved(session) => session.draft_read(id).await,
            Self::Temporary(session) => session.draft_read(id),
        }
    }
    pub(super) async fn draft_write(
        &self,
        id: Uuid,
        revision: Option<Uuid>,
        content: ChatDraftContent,
    ) -> Result<ChatDraft, ServiceError> {
        match self {
            Self::Saved(session) => session.draft_write(id, revision, content).await,
            Self::Temporary(session) => session.draft_write(id, revision, content),
        }
    }
    pub(super) async fn draft_delete(&self, id: Uuid, revision: Uuid) -> Result<(), ServiceError> {
        match self {
            Self::Saved(session) => session.draft_delete(id, revision).await,
            Self::Temporary(session) => session.draft_delete(id, revision),
        }
    }
    pub(super) async fn draft_list(
        &self,
        before: Option<ChatPageCursor>,
        limit: usize,
    ) -> Result<ChatDraftPage, ServiceError> {
        match self {
            Self::Saved(session) => session.draft_list(before, limit).await,
            Self::Temporary(session) => session.draft_list(before, limit),
        }
    }
    pub(super) async fn conversation_read(
        &self,
        id: &str,
        cache_only: bool,
    ) -> Result<
        (
            codex_chatgpt_chat::conversation::ConversationSnapshot,
            rpc::ChatConversationSource,
            Option<i64>,
        ),
        ServiceError,
    > {
        match self {
            Self::Saved(session) if cache_only => {
                let (snapshot, observed_at_ms) = session.cached_conversation_read(id).await?;
                Ok((
                    snapshot,
                    rpc::ChatConversationSource::LocalCache,
                    Some(observed_at_ms),
                ))
            }
            Self::Saved(session) => Ok((
                session.conversation_read(id).await?,
                rpc::ChatConversationSource::Authoritative,
                None,
            )),
            Self::Temporary(_) if cache_only => Err(ChatError::UnsupportedCapability.into()),
            Self::Temporary(session) => Ok((
                session.conversation_read(id)?,
                rpc::ChatConversationSource::TemporaryMemory,
                None,
            )),
        }
    }
    pub(super) async fn pending_for_draft(
        &self,
        id: Uuid,
    ) -> Result<Option<rpc::ChatPendingSubmission>, ServiceError> {
        match self {
            Self::Saved(session) => session
                .pending_for_draft(id)
                .await
                .map(|pending| pending.map(adapter::pending)),
            Self::Temporary(session) => session
                .pending_for_draft(id)
                .map(|pending| pending.map(adapter::temporary_pending)),
        }
    }
    pub(super) async fn pending_for_conversation(
        &self,
        id: &str,
    ) -> Result<(Option<rpc::ChatPendingSubmission>, bool), ServiceError> {
        match self {
            Self::Saved(session) => match session.pending_for_conversation(id).await {
                Ok(pending) => Ok((pending.map(adapter::pending), true)),
                Err(ServiceError::Store(ChatStoreError::Unavailable)) => Ok((None, false)),
                Err(error) => Err(error),
            },
            Self::Temporary(session) => Ok((
                session
                    .pending_for_conversation(id)?
                    .map(adapter::temporary_pending),
                false,
            )),
        }
    }
    pub(super) async fn start_turn(
        &self,
        request: crate::chat::StartSavedTurn,
    ) -> Result<crate::chat::Operation, ServiceError> {
        match self {
            Self::Saved(session) => session.start_turn(request).await,
            Self::Temporary(session) => session.start_turn(crate::chat::StartTemporaryTurn {
                draft_id: request.draft_id,
                revision: request.revision,
                submission_id: request.submission_id,
                client_message_id: request.client_message_id,
                expected_parent_id: request.expected_parent_id,
                upload_handles: request.upload_handles,
            }),
        }
    }
    pub(super) async fn upload_selected(
        &self,
        draft: Uuid,
        revision: Uuid,
        selections: Vec<crate::chat::UploadSelection>,
        progress: impl FnMut(Uuid, Uuid, UploadStage, Option<u8>),
        cancel: &CancellationToken,
    ) -> Result<crate::chat::UploadBatch, ServiceError> {
        match self {
            Self::Saved(session) => {
                Box::pin(session.upload_selected(draft, revision, selections, progress, cancel))
                    .await
            }
            Self::Temporary(session) => {
                Box::pin(session.upload_selected(draft, revision, selections, progress, cancel))
                    .await
            }
        }
    }
    pub(super) fn cancel_upload(&self, id: Uuid) -> Result<bool, ServiceError> {
        match self {
            Self::Saved(session) => session.cancel_upload(id),
            Self::Temporary(session) => session.cancel_upload(id),
        }
    }
}
