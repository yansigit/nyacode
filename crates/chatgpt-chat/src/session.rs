//! Native saved-session adapter for the local Chat service. The compatibility
//! harness keeps its private file journal; the service supplies asynchronous
//! durable checkpoints through TurnSink. There is no transport selector here.

use crate::ChatError;
use crate::ChatProbe;
use crate::ProbeTransport;
use crate::SavedSendObservation;
use crate::attachment::StagedAttachment;
use crate::conversation::ConversationPage;
use crate::conversation::ConversationSnapshot;
use crate::native_preparation::PreparationTarget;
use crate::native_preparation::PreparedTurn;
use crate::probe::valid_id;
use crate::stream::Acceptance;
use crate::stream::GenerationOutcome;
use crate::stream::Message;
use crate::stream::TurnDecoder;
use crate::stream::TurnState;
use crate::upload::ReadyUpload;
use crate::upload::UploadObservation;
use crate::upload::UploadStage;
use codex_login::AuthManager;
use serde_json::Value;
use serde_json::json;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const NATIVE_PROFILE_VERSION: &str = "chrome136-linux-native-v1";

/// No Debug/Serialize: immutable prompt contents never enter generic diagnostics.
pub struct SavedTurnRequest {
    pub submission_id: Uuid,
    pub client_message_id: Uuid,
    pub draft_id: Uuid,
    pub conversation_id: Option<String>,
    pub expected_parent_id: String,
    pub model: Option<String>,
    pub text: String,
    pub created_at_ms: i64,
}

/// Implemented by the service's durable submission owner. The first callback
/// must commit the transmission marker; failure prevents the message POST.
/// Checkpoints may persist new IDs/acceptance and publish scoped stream events.
pub trait TurnSink: Send {
    fn before_transmission(
        &mut self,
        submission_id: Uuid,
        requested_model: &str,
    ) -> impl Future<Output = Result<(), ChatError>> + Send;

    fn checkpoint(
        &mut self,
        state: &TurnState,
    ) -> impl Future<Output = Result<(), ChatError>> + Send;
}

pub(crate) struct SynchronousSink<F>(pub(crate) F);

impl<F> TurnSink for SynchronousSink<F>
where
    F: FnMut(&TurnState) -> Result<(), ChatError> + Send,
{
    async fn before_transmission(&mut self, _: Uuid, _: &str) -> Result<(), ChatError> {
        // Harness callers have already committed their own transmission marker.
        Err(ChatError::RecoveryUnavailable)
    }

    fn checkpoint(
        &mut self,
        state: &TurnState,
    ) -> impl Future<Output = Result<(), ChatError>> + Send {
        std::future::ready((self.0)(state))
    }
}

/// Opaque, session-bound, one-use preparation. The service can inspect the
/// selected model and immutable request before journaling, but cannot alter it.
pub struct PreparedSavedTurn {
    session_id: Uuid,
    prepared: PreparedTurn,
    request: SavedTurnRequest,
    uploads: Vec<Arc<ReadyUpload>>,
}

impl PreparedSavedTurn {
    pub fn request(&self) -> &SavedTurnRequest {
        &self.request
    }

    pub fn requested_model(&self) -> &str {
        &self.prepared.model
    }
}

pub struct SavedTurnResult {
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
    pub error: Option<ChatError>,
}

pub struct ReconciledSavedTurn {
    pub messages: Vec<Message>,
    pub outcome: GenerationOutcome,
    pub snapshot: ConversationSnapshot,
}

/// One managed account/authentication generation and one persisted device.
/// Temporary conversations use a separate adapter and can never enter this API.
pub struct SavedChatSession {
    probe: ChatProbe,
    id: Uuid,
    generation: u64,
    preparation: Semaphore,
}

impl SavedChatSession {
    pub fn new(
        auth: Arc<AuthManager>,
        enabled: bool,
        expected_account: &str,
        device: Uuid,
    ) -> Result<Self, ChatError> {
        let generation = auth.auth_change_state_receiver().borrow().generation;
        let probe = ChatProbe::with_transport(
            auth,
            enabled,
            Some(expected_account),
            ProbeTransport::BrowserEmulated,
        )?;
        probe
            .native_device
            .set(device.to_string())
            .map_err(|_| ChatError::SessionChanged)?;
        let session = Self {
            probe,
            id: Uuid::new_v4(),
            generation,
            preparation: Semaphore::new(1),
        };
        session.check_current()?;
        Ok(session)
    }

    pub fn stop(&self) {
        self.probe.stop();
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

    pub async fn conversation_list(
        &self,
        offset: u32,
        limit: u32,
    ) -> Result<ConversationPage, ChatError> {
        self.check_current()?;
        let page = self.probe.conversation_list(offset, limit).await?;
        self.check_current()?;
        Ok(page)
    }

    pub async fn model_catalog(&self) -> Result<crate::models::ModelCatalog, ChatError> {
        self.check_current()?;
        let _permit = self
            .preparation
            .acquire()
            .await
            .map_err(|_| ChatError::LocalCancellation)?;
        let init = self.probe.native_model_defaults().await?;
        let (default_model, limits) = crate::models::defaults(&init)?;
        let picker = self.probe.model_picker().await?;
        let default_model = picker.default_model(&default_model)?;
        self.check_current()?;
        Ok(crate::models::ModelCatalog {
            models: picker.models,
            default_model,
            limits,
        })
    }

    pub async fn conversation_read(&self, id: &str) -> Result<ConversationSnapshot, ChatError> {
        self.check_current()?;
        let snapshot = self.probe.conversation_read(id).await?;
        self.check_current()?;
        Ok(snapshot)
    }

    pub async fn upload_attachment(
        &self,
        draft: Uuid,
        file: &StagedAttachment,
        cancel: &CancellationToken,
        progress: impl FnMut(UploadStage, Option<u8>),
    ) -> Result<(Option<ReadyUpload>, UploadObservation), ChatError> {
        self.check_current()?;
        let result = self
            .probe
            .upload_attachment(draft, false, file, cancel, progress)
            .await;
        self.check_current()?;
        Ok(result)
    }

    /// Preparation may run before the durable reservation, but it never sends
    /// user content. The service must reserve the immutable draft revision and
    /// submission IDs before calling send; a losing process discards this token.
    pub async fn prepare(
        &self,
        request: SavedTurnRequest,
        uploads: Vec<Arc<ReadyUpload>>,
        cancel: &CancellationToken,
    ) -> Result<PreparedSavedTurn, ChatError> {
        self.check_current()?;
        validate_request(&request)?;
        if request.text.is_empty() && uploads.is_empty() {
            return Err(ChatError::InvalidInput);
        }
        self.probe.attachment_message_refs(
            request.draft_id,
            false,
            uploads.iter().map(Arc::as_ref),
            &request.text,
        )?;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ChatError::LocalCancellation),
            result = self.prepare_inner(request, uploads) => result,
        }
    }

    async fn prepare_inner(
        &self,
        request: SavedTurnRequest,
        uploads: Vec<Arc<ReadyUpload>>,
    ) -> Result<PreparedSavedTurn, ChatError> {
        // Independent conversations may stream concurrently. Only native
        // preparation uses this short, cancellable session-wide queue.
        let _permit = self
            .preparation
            .acquire()
            .await
            .map_err(|_| ChatError::LocalCancellation)?;
        self.verify_parent(&request).await?;
        let target = PreparationTarget {
            parent: request.expected_parent_id.clone(),
            conversation: request.conversation_id.clone(),
            model: request.model.clone(),
            temporary: false,
        };
        let mut observations = SavedSendObservation::default();
        let prepared = self
            .probe
            .prepare_native(target, &mut observations.preparation)
            .await?;
        self.probe
            .observer_ping(&prepared, &mut observations)
            .await?;
        self.verify_parent(&request).await?;
        self.check_current()?;
        Ok(PreparedSavedTurn {
            session_id: self.id,
            prepared,
            request,
            uploads,
        })
    }

    pub async fn send(
        &self,
        prepared: PreparedSavedTurn,
        sink: &mut impl TurnSink,
        cancel: &CancellationToken,
    ) -> SavedTurnResult {
        let mut decoder = TurnDecoder::new(prepared.request.client_message_id.to_string());
        let mut observation = SavedSendObservation::default();
        let mut transmission_started = false;
        let error = tokio::select! {
            biased;
            _ = cancel.cancelled() => Some(ChatError::LocalCancellation),
            result = self.send_inner(&prepared, sink, &mut decoder, &mut observation, &mut transmission_started) => result.err(),
        };
        let state = decoder.into_state();
        let outcome = state.outcome();
        SavedTurnResult {
            submission_id: prepared.request.submission_id,
            client_message_id: prepared.request.client_message_id,
            conversation_id: state.conversation_id.or(prepared.request.conversation_id),
            expected_parent_id: prepared.request.expected_parent_id,
            transmission_started,
            acceptance: state.acceptance,
            outcome,
            messages: state.messages,
            requested_model: prepared.prepared.model,
            resolved_model: state.resolved_model,
            error,
        }
    }

    async fn send_inner(
        &self,
        prepared: &PreparedSavedTurn,
        sink: &mut impl TurnSink,
        decoder: &mut TurnDecoder,
        observation: &mut SavedSendObservation,
        transmission_started: &mut bool,
    ) -> Result<(), ChatError> {
        self.check_current()?;
        if prepared.session_id != self.id {
            return Err(ChatError::SessionChanged);
        }
        self.verify_parent(&prepared.request).await?;
        let request = &prepared.request;
        let (content, metadata) = self.probe.attachment_message_refs(
            request.draft_id,
            false,
            prepared.uploads.iter().map(Arc::as_ref),
            &request.text,
        )?;
        let mut body = json!({
            "action":"next", "parent_message_id":request.expected_parent_id,
            "model":prepared.prepared.model, "history_and_training_disabled":false,
            "conversation_mode":{"kind":"primary_assistant"},
            "timezone_offset_min":0, "timezone":"UTC", "fork_from_shared_post":false,
            "system_hints":[], "supports_buffering":true, "supported_encodings":["v1"],
            "messages":[{"id":request.client_message_id.to_string(),"author":{"role":"user"},"create_time":request.created_at_ms as f64 / 1000.0,
                "content":content,"metadata":metadata}]
        });
        if let Some(id) = &request.conversation_id {
            body["conversation_id"] = json!(id);
        }
        let outgoing = self
            .probe
            .prepared_request(&prepared.prepared, "f/conversation")
            .await?
            .header("accept", "text/event-stream")
            .header("x-conduit-token", &prepared.prepared.conduit)
            .json(&body);
        self.probe
            .guarded_generation(
                self.generation,
                sink.before_transmission(request.submission_id, &prepared.prepared.model),
            )
            .await?;
        // If the callback's commit is ambiguous, no POST is polled. The durable
        // record, rather than this local flag, decides whether a retry is safe.
        *transmission_started = true;
        self.check_current()?;
        self.probe
            .consume_stream_observed(outgoing, &prepared.prepared, decoder, observation, sink)
            .await
    }

    async fn verify_parent(&self, request: &SavedTurnRequest) -> Result<(), ChatError> {
        self.check_current()?;
        if let Some(id) = &request.conversation_id {
            self.probe
                .conversation_read(id)
                .await?
                .check_continuation(&request.expected_parent_id)?;
        }
        Ok(())
    }

    /// Authoritative exact-ID reconciliation. An absent user message remains
    /// pending; this method never prepares, submits, or retries a user message.
    pub async fn reconcile(
        &self,
        conversation_id: &str,
        client_message_id: Uuid,
        expected_parent_id: &str,
    ) -> Result<ReconciledSavedTurn, ChatError> {
        self.check_current()?;
        if !valid_id(conversation_id) || !valid_id(expected_parent_id) {
            return Err(ChatError::InvalidInput);
        }
        let path = format!("conversation/{conversation_id}");
        let value: Value = self.probe.read(&path, &path).await?;
        if value.get("is_temporary_chat").and_then(Value::as_bool) == Some(true)
            || value.get("is_do_not_remember").and_then(Value::as_bool) == Some(true)
        {
            return Err(ChatError::UnsupportedCapability);
        }
        let snapshot = ConversationSnapshot::parse(conversation_id, &value)?;
        if snapshot.continuation_blocker
            == Some(crate::conversation::ContinuationBlocker::Temporary)
        {
            return Err(ChatError::UnsupportedCapability);
        }
        let (messages, outcome) =
            crate::send::reconcile(&value, &client_message_id.to_string(), expected_parent_id)?;
        self.check_current()?;
        Ok(ReconciledSavedTurn {
            messages,
            outcome,
            snapshot,
        })
    }
}

fn validate_request(request: &SavedTurnRequest) -> Result<(), ChatError> {
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
