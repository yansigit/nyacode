//! Memory-only temporary conversation adapter. Each instance owns one draft,
//! native profile and conduit history. It has no journal, persistence, saved
//! history reader, or API for changing a conversation's persistence mode.

use crate::ChatError;
use crate::ChatProbe;
use crate::ProbeTransport;
use crate::SavedSendObservation;
use crate::attachment::StagedAttachment;
use crate::native_preparation::PreparationTarget;
use crate::native_preparation::PreparedTurn;
use crate::probe::valid_id;
use crate::stream::Acceptance;
use crate::stream::GenerationOutcome;
use crate::stream::Message;
use crate::stream::MessagePart;
use crate::stream::TurnDecoder;
use crate::stream::TurnState;
use crate::upload::ReadyUpload;
use crate::upload::UploadObservation;
use crate::upload::UploadStage;
use codex_login::AuthManager;
use http::Method;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_TURNS: usize = 256;

// Content and capabilities intentionally implement neither Debug nor Serialize.
pub struct TemporaryTurnRequest {
    pub submission_id: Uuid,
    pub client_message_id: Uuid,
    pub conversation_id: Option<String>,
    pub expected_parent_id: String,
    pub model: Option<String>,
    pub text: String,
    pub created_at_ms: i64,
}

/// Synchronous callbacks update only the service's in-memory operation. Unlike
/// saved TurnSink, temporary progress never requires a durable checkpoint.
pub trait TemporaryTurnSink: Send {
    fn before_transmission(&mut self, requested_model: &str) -> Result<(), ChatError>;
    fn checkpoint(&mut self, state: &TurnState) -> Result<(), ChatError>;
}

pub struct TemporaryTurnResult {
    pub submission_id: Uuid,
    pub client_message_id: Uuid,
    pub conversation_id: Option<String>,
    pub expected_parent_id: String,
    pub transmission_started: bool,
    pub acceptance: Acceptance,
    pub outcome: GenerationOutcome,
    pub messages: Vec<Message>,
    pub requested_model: String,
    pub resolved_model: Option<String>,
    /// Whether the conversation can attempt another continuation. Before
    /// transmission this may refer to its previous, unchanged completed turn.
    pub continuation_eligible: bool,
    pub error: Option<ChatError>,
}

#[derive(Default)]
struct Memory {
    receipt: Option<Receipt>,
    // Set before polling a message POST. Dropping a send future cannot make an
    // ambiguous submission eligible for replay, even if no decoder is retained.
    pending: bool,
    submissions: HashSet<Uuid>,
    message_ids: HashSet<Uuid>,
}

struct Receipt {
    prepared: PreparedTurn,
    state: TurnState,
    client_message_id: Uuid,
    refresh_attempted: bool,
}

/// One temporary draft in one managed account/authentication generation. The
/// service bounds the number of instances and serializes operations per draft.
/// Other instances, and saved conversations, may stream independently.
pub struct TemporaryChatSession {
    probe: ChatProbe,
    generation: u64,
    draft: Uuid,
    memory: Mutex<Memory>,
    sending: Semaphore,
}

impl TemporaryChatSession {
    pub fn new(
        auth: Arc<AuthManager>,
        enabled: bool,
        expected_account: &str,
        draft: Uuid,
    ) -> Result<Self, ChatError> {
        let generation = auth.auth_change_state_receiver().borrow().generation;
        let probe = ChatProbe::with_transport(
            auth,
            enabled,
            Some(expected_account),
            ProbeTransport::BrowserEmulated,
        )?;
        let session = Self {
            probe,
            generation,
            draft,
            memory: Mutex::new(Memory::default()),
            sending: Semaphore::new(1),
        };
        session.check_current()?;
        Ok(session)
    }

    pub fn check_current(&self) -> Result<(), ChatError> {
        self.probe.check_owner()?;
        if self
            .probe
            .auth
            .auth_change_state_receiver()
            .borrow()
            .generation
            != self.generation
        {
            return Err(ChatError::SessionChanged);
        }
        Ok(())
    }

    pub fn stop(&self) {
        self.probe.stop();
        *lock(&self.memory) = Memory::default();
        // A running operation is woken by probe.stop(), drops its live futures,
        // and clears memory before returning an ownership error.
    }

    pub async fn upload_attachment(
        &self,
        file: &StagedAttachment,
        cancel: &CancellationToken,
        progress: impl FnMut(UploadStage, Option<u8>),
    ) -> Result<(Option<ReadyUpload>, UploadObservation), ChatError> {
        self.check_current()?;
        let result = self
            .probe
            .upload_attachment(self.draft, true, file, cancel, progress)
            .await;
        self.check_current()?;
        Ok(result)
    }

    /// Sends at most once. Errors before transmission leave the submitted draft
    /// recoverable. Any ambiguous transmission blocks subsequent sends in this
    /// temporary conversation; it is never reopened through saved history.
    pub async fn send(
        &self,
        request: TemporaryTurnRequest,
        uploads: Vec<Arc<ReadyUpload>>,
        sink: &mut impl TemporaryTurnSink,
        cancel: &CancellationToken,
    ) -> Result<TemporaryTurnResult, ChatError> {
        // Keep native preparation and stream temporaries off callers' task
        // frames, including the unoptimized embedded-server build.
        Box::pin(self.send_inner(request, uploads, sink, cancel)).await
    }

    async fn send_inner(
        &self,
        request: TemporaryTurnRequest,
        uploads: Vec<Arc<ReadyUpload>>,
        sink: &mut impl TemporaryTurnSink,
        cancel: &CancellationToken,
    ) -> Result<TemporaryTurnResult, ChatError> {
        self.check_current()?;
        let _permit = self
            .sending
            .try_acquire()
            .map_err(|_| ChatError::PreparationInProgress)?;
        let mut memory = MemoryLease {
            session: self,
            value: std::mem::take(&mut *lock(&self.memory)),
        };
        validate_request(&request)?;
        validate_memory(&memory, &request)?;
        if request.text.is_empty() && uploads.is_empty() {
            return Err(ChatError::InvalidInput);
        }
        self.probe.attachment_message_refs(
            self.draft,
            true,
            uploads.iter().map(Arc::as_ref),
            &request.text,
        )?;
        let preparation = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ChatError::LocalCancellation),
            result = Box::pin(self.prepare(&request)) => result,
        };
        let prepared = match preparation {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Err(owner_error) = self.check_current() {
                    memory.value = Memory::default();
                    return Err(owner_error);
                }
                return Err(error);
            }
        };
        let mut decoder = TurnDecoder::new(request.client_message_id.to_string());
        let mut transmission_started = false;
        let error = tokio::select! {
            biased;
            _ = cancel.cancelled() => Some(ChatError::LocalCancellation),
            result = Box::pin(self.send_prepared(&prepared, &request, &uploads, &mut memory, sink, &mut decoder, &mut transmission_started)) => result.err(),
        };
        if let Err(error) = self.check_current() {
            memory.value = Memory::default();
            return Err(error);
        }
        let state = decoder.into_state();
        let continuation_eligible = if transmission_started {
            state.acceptance == Acceptance::Accepted
                && state.conversation_id.is_some()
                && completed_parent(&state).is_some()
                && state.resume_token().is_some()
                && error.is_none()
        } else {
            !memory.pending
                && memory.receipt.as_ref().is_some_and(|receipt| {
                    !receipt.refresh_attempted
                        && receipt.state.resume_token().is_some()
                        && completed_parent(&receipt.state).is_some()
                })
        };
        let result = TemporaryTurnResult {
            submission_id: request.submission_id,
            client_message_id: request.client_message_id,
            conversation_id: state.conversation_id.clone().or(request.conversation_id),
            expected_parent_id: request.expected_parent_id,
            transmission_started,
            acceptance: state.acceptance,
            outcome: state.outcome(),
            messages: state.messages.clone(),
            requested_model: prepared.model.clone(),
            resolved_model: state.resolved_model.clone(),
            continuation_eligible,
            error,
        };
        if transmission_started {
            memory.pending = !continuation_eligible;
            memory.receipt = Some(Receipt {
                prepared,
                state,
                client_message_id: request.client_message_id,
                refresh_attempted: false,
            });
        }
        Ok(result)
    }

    async fn prepare(&self, request: &TemporaryTurnRequest) -> Result<PreparedTurn, ChatError> {
        let target = PreparationTarget {
            parent: request.expected_parent_id.clone(),
            conversation: request.conversation_id.clone(),
            model: request.model.clone(),
            temporary: true,
        };
        let mut observation = SavedSendObservation::default();
        let prepared = self
            .probe
            .prepare_native(target, &mut observation.preparation)
            .await?;
        self.probe
            .observer_ping(&prepared, &mut observation)
            .await?;
        self.check_current()?;
        Ok(prepared)
    }

    #[expect(clippy::too_many_arguments)]
    async fn send_prepared(
        &self,
        prepared: &PreparedTurn,
        request: &TemporaryTurnRequest,
        uploads: &[Arc<ReadyUpload>],
        memory: &mut Memory,
        sink: &mut impl TemporaryTurnSink,
        decoder: &mut TurnDecoder,
        transmission_started: &mut bool,
    ) -> Result<(), ChatError> {
        self.check_current()?;
        if let Some(receipt) = memory.receipt.as_mut() {
            self.refresh_parent(receipt, &request.expected_parent_id)
                .await?;
        }
        let (content, metadata) = self.probe.attachment_message_refs(
            self.draft,
            true,
            uploads.iter().map(Arc::as_ref),
            &request.text,
        )?;
        let mut body = json!({
            "action":"next", "parent_message_id":request.expected_parent_id,
            "model":prepared.model, "history_and_training_disabled":true,
            "conversation_mode":{"kind":"primary_assistant"},
            "timezone_offset_min":0, "timezone":"UTC", "fork_from_shared_post":false,
            "system_hints":[], "supports_buffering":true, "supported_encodings":["v1"],
            "messages":[{"id":request.client_message_id.to_string(),"author":{"role":"user"},"create_time":request.created_at_ms as f64 / 1000.0,
                "content":content,"metadata":metadata}]
        });
        if let Some(id) = &request.conversation_id {
            body["conversation_id"] = json!(id);
        } else {
            body["temporary_chat_requests_personalization"] = json!(false);
        }
        let outgoing = self
            .probe
            .prepared_request(prepared, "f/conversation")
            .await?
            .header("accept", "text/event-stream")
            .header("x-conduit-token", &prepared.conduit)
            .json(&body);
        sink.before_transmission(&prepared.model)?;
        self.check_current()?;
        memory.pending = true;
        memory.submissions.insert(request.submission_id);
        memory.message_ids.insert(request.client_message_id);
        *transmission_started = true;
        self.probe
            .consume_stream(
                outgoing,
                prepared,
                decoder,
                &mut SavedSendObservation::default(),
                |state| {
                    if request.conversation_id.as_ref().is_some_and(|expected| {
                        state
                            .conversation_id
                            .as_ref()
                            .is_some_and(|id| id != expected)
                    }) {
                        return Err(ChatError::ProtocolChanged);
                    }
                    sink.checkpoint(state)
                },
            )
            .await?;
        if decoder.state().conversation_id.is_none() {
            return Err(ChatError::ProtocolChanged);
        }
        self.check_current()
    }

    async fn refresh_parent(
        &self,
        receipt: &mut Receipt,
        expected_parent: &str,
    ) -> Result<(), ChatError> {
        if receipt.refresh_attempted {
            // Resume capabilities are one-use. A failed/cancelled refresh is
            // not permission to replay it or to read a saved conversation.
            return Err(ChatError::UnsupportedCapability);
        }
        let parent = completed_parent(&receipt.state).ok_or(ChatError::IncompleteStream)?;
        if parent.id != expected_parent {
            return Err(ChatError::BranchConflict);
        }
        let id = receipt
            .state
            .conversation_id
            .as_deref()
            .ok_or(ChatError::ProtocolChanged)?;
        let token = receipt
            .state
            .resume_token()
            .ok_or(ChatError::UnsupportedCapability)?;
        receipt.refresh_attempted = true;
        let prepared = &receipt.prepared;
        let request = self
            .probe
            .request(
                Method::POST,
                "f/conversation/resume",
                "f/conversation/resume",
            )
            .await?
            .header("origin", &prepared.profile.bootstrap.origin)
            .header("referer", format!("{}/", prepared.profile.bootstrap.origin))
            .header("oai-device-id", &prepared.profile.device)
            .header("oai-client-version", &prepared.profile.bootstrap.build)
            .header("oai-language", "en-US")
            .header("accept", "text/event-stream")
            .header("x-conduit-token", token)
            .json(&json!({"conversation_id":id,"offset":0}));
        let mut decoder = TurnDecoder::new(receipt.client_message_id.to_string());
        self.probe
            .consume_stream(
                request,
                prepared,
                &mut decoder,
                &mut SavedSendObservation::default(),
                |_| Ok(()),
            )
            .await?;
        let refreshed = decoder.state();
        if refreshed.conversation_id.as_deref() != Some(id)
            || refreshed.acceptance != Acceptance::Accepted
            || completed_parent(refreshed).is_none_or(|current| {
                current.id != expected_parent || current.text() != parent.text()
            })
        {
            return Err(ChatError::BranchConflict);
        }
        self.check_current()
    }
}

// The per-conversation semaphore serializes sends. State is owned across awaits,
// never protected by a held mutex. Dropping a future restores its pending marker;
// revocation discards all state instead of putting live capabilities back.
struct MemoryLease<'a> {
    session: &'a TemporaryChatSession,
    value: Memory,
}
impl std::ops::Deref for MemoryLease<'_> {
    type Target = Memory;
    fn deref(&self) -> &Memory {
        &self.value
    }
}
impl std::ops::DerefMut for MemoryLease<'_> {
    fn deref_mut(&mut self) -> &mut Memory {
        &mut self.value
    }
}
impl Drop for MemoryLease<'_> {
    fn drop(&mut self) {
        let mut memory = lock(&self.session.memory);
        if self.session.check_current().is_ok() {
            *memory = std::mem::take(&mut self.value);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn completed_parent(state: &TurnState) -> Option<&Message> {
    if state.outcome() != GenerationOutcome::Completed {
        return None;
    }
    state
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "assistant")
        .filter(|message| {
            message.finished
                && message.end_turn
                && !message
                    .parts
                    .iter()
                    .any(|part| matches!(part, MessagePart::Unsupported { .. }))
        })
}

fn validate_memory(memory: &Memory, request: &TemporaryTurnRequest) -> Result<(), ChatError> {
    if memory.submissions.contains(&request.submission_id)
        || memory.message_ids.contains(&request.client_message_id)
    {
        return Err(ChatError::PendingSubmission);
    }
    if memory.pending {
        return Err(
            if memory.receipt.as_ref().is_some_and(|receipt| {
                receipt.state.acceptance == Acceptance::Accepted
                    && receipt.state.outcome() != GenerationOutcome::Incomplete
            }) {
                ChatError::UnsupportedCapability
            } else {
                ChatError::PendingSubmission
            },
        );
    }
    if memory.submissions.len() >= MAX_TURNS {
        return Err(ChatError::ResponseTooLarge);
    }
    match &memory.receipt {
        Some(receipt) => {
            if receipt.state.conversation_id != request.conversation_id
                || completed_parent(&receipt.state)
                    .is_none_or(|parent| parent.id != request.expected_parent_id)
            {
                return Err(ChatError::BranchConflict);
            }
            if receipt.refresh_attempted || receipt.state.resume_token().is_none() {
                return Err(ChatError::UnsupportedCapability);
            }
        }
        None if request.conversation_id.is_some() => return Err(ChatError::BranchConflict),
        None => {}
    }
    Ok(())
}

fn validate_request(request: &TemporaryTurnRequest) -> Result<(), ChatError> {
    if !valid_id(&request.expected_parent_id)
        || request
            .conversation_id
            .as_deref()
            .is_some_and(|id| !valid_id(id))
        || request.model.as_ref().is_some_and(|model| {
            model.is_empty()
                || model.len() > 256
                || !model
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        })
        || request.text.len() > 1024 * 1024
        || request.created_at_ms < 0
    {
        return Err(ChatError::InvalidInput);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
