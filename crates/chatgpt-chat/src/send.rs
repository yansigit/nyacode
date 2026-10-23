//! Explicit saved-run acceptance. POSTs execute at most once. The journal is
//! durable before transmission; only authoritative identifiers reconcile it.

use crate::ChatError;
use crate::ChatProbe;
use crate::PreparationObservation;
use crate::journal::JournalError;
use crate::journal::RunJournal;
use crate::journal::RunScope;
use crate::native_preparation::PreparationTarget;
use crate::native_preparation::PreparedTurn;
use crate::probe::bounded_body;
use crate::probe::classify_status;
use crate::probe::transport_error;
use crate::stream::Acceptance;
use crate::stream::GenerationOutcome;
use crate::stream::Message;
use crate::stream::StreamError;
use crate::stream::TurnDecoder;
use base64::Engine;
use base64::prelude::BASE64_URL_SAFE;
use codex_http_client::RequestBuilder;
use http::Method;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashSet;
use std::time::Duration;

const STREAM_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Serialize)]
pub struct SavedSendObservation {
    pub preparation: Vec<PreparationObservation>,
    pub observer_ping_status: Option<u16>,
    pub send_http_status: Option<u16>,
    pub error: Option<ChatError>,
    pub stream_error: Option<StreamError>,
    pub acceptance: Acceptance,
    pub generation: GenerationOutcome,
    pub authoritative_retrieval: bool,
    pub requested_model: Option<String>,
    pub resolved_model: Option<String>,
    pub assistant_characters: usize,
    pub unknown_metadata_events: usize,
    pub fixture_response_matched: Option<bool>,
}

impl Default for SavedSendObservation {
    fn default() -> Self {
        Self {
            preparation: Vec::new(),
            observer_ping_status: None,
            send_http_status: None,
            error: None,
            stream_error: None,
            acceptance: Acceptance::Unknown,
            generation: GenerationOutcome::Incomplete,
            authoritative_retrieval: false,
            requested_model: None,
            resolved_model: None,
            assistant_characters: 0,
            unknown_metadata_events: 0,
            fixture_response_matched: None,
        }
    }
}

impl ChatProbe {
    pub fn run_scope(&self) -> Result<RunScope, ChatError> {
        self.check_owner()?;
        let auth = self.auth.auth_cached().ok_or(ChatError::Authentication)?;
        let scope = RunScope {
            principal: auth
                .get_chatgpt_user_id()
                .ok_or(ChatError::RecoveryUnavailable)?,
            account: auth.get_account_id().ok_or(ChatError::Authentication)?,
        };
        self.check_owner()?;
        Ok(scope)
    }

    fn bind_journal(&self, journal: &RunJournal) -> Result<(), ChatError> {
        if self.run_scope()? != *journal.scope() {
            return Err(ChatError::AccountMismatch);
        }
        if let Some(device) = self.native_device.get() {
            if device != journal.device() {
                return Err(ChatError::SessionChanged);
            }
        } else {
            self.native_device
                .set(journal.device().into())
                .map_err(|_| ChatError::SessionChanged)?;
        }
        Ok(())
    }

    pub async fn send_saved(&self, journal: &mut RunJournal) -> SavedSendObservation {
        self.send_saved_with_attachments(journal, uuid::Uuid::nil(), &[])
            .await
    }

    pub async fn send_saved_with_attachments(
        &self,
        journal: &mut RunJournal,
        draft: uuid::Uuid,
        attachments: &[crate::upload::ReadyUpload],
    ) -> SavedSendObservation {
        let mut observation = SavedSendObservation::default();
        observation.error =
            Box::pin(self.send_saved_inner(journal, draft, attachments, &mut observation))
                .await
                .err();
        fill_observation(journal, &mut observation);
        observation
    }

    async fn send_saved_inner(
        &self,
        journal: &mut RunJournal,
        draft: uuid::Uuid,
        attachments: &[crate::upload::ReadyUpload],
        observation: &mut SavedSendObservation,
    ) -> Result<(), ChatError> {
        self.bind_journal(journal)?;
        let attempt = journal.current().ok_or(ChatError::InvalidInput)?;
        if attempt.transmission_started {
            return Err(ChatError::PendingSubmission);
        }
        if !attempt.attachments.is_empty() && attachments.is_empty() {
            return Err(ChatError::StaleUpload);
        }
        self.attachment_message(draft, false, attachments, &attempt.text)?;
        if !attachments.is_empty() {
            journal
                .attach(
                    attachments
                        .iter()
                        .map(crate::upload::ReadyUpload::journal_metadata)
                        .collect(),
                )
                .map_err(journal_error)?;
        }
        let attempt = journal.current().ok_or(ChatError::InvalidInput)?;
        let target = PreparationTarget {
            parent: attempt.parent_message_id.clone(),
            conversation: attempt.conversation_id.clone(),
            model: None,
            temporary: false,
        };
        self.verify_parent(&target).await?;
        let prepared = self
            .prepare_native(target, &mut observation.preparation)
            .await?;
        if prepared.profile.device != journal.device() {
            return Err(ChatError::SessionChanged);
        }
        observation.requested_model = Some(prepared.model.clone());
        self.observer_ping(&prepared, observation).await?;
        self.verify_parent(&prepared.target).await?;
        let attempt = journal.current().ok_or(ChatError::InvalidInput)?;
        let message_id = attempt.client_message_id.clone();
        let (content, metadata) =
            self.attachment_message(draft, false, attachments, &attempt.text)?;
        let mut body = json!({
            "action":"next", "parent_message_id":prepared.target.parent,
            "model":prepared.model, "history_and_training_disabled":false,
            "conversation_mode":{"kind":"primary_assistant"},
            "timezone_offset_min":0, "timezone":"UTC", "fork_from_shared_post":false,
            "system_hints":[], "supports_buffering":true, "supported_encodings":["v1"],
            "messages":[{"id":message_id,"author":{"role":"user"},"create_time":attempt.created_at,
                "content":content,"metadata":metadata}]
        });
        if let Some(id) = &prepared.target.conversation {
            body["conversation_id"] = json!(id);
        }
        let request = self
            .prepared_request(&prepared, "f/conversation")
            .await?
            .header("accept", "text/event-stream")
            .header("x-conduit-token", &prepared.conduit)
            .json(&body);
        // No network operation is polled until this durable write succeeds.
        journal
            .mark_transmitting(&prepared.model)
            .map_err(journal_error)?;
        let mut decoder = TurnDecoder::new(message_id);
        let stream_result = self
            .consume_stream(request, &prepared, &mut decoder, observation, |state| {
                journal.observe(state, false).map_err(journal_error)
            })
            .await;
        // Preserve accepted messages and partial content even after timeout,
        // cancellation, malformed output, or generation failure.
        journal
            .observe(decoder.state(), true)
            .map_err(journal_error)?;
        observation.unknown_metadata_events = decoder.state().unknown_metadata_events;
        if matches!(
            stream_result,
            Err(ChatError::CloudflareChallenge
                | ChatError::UnsupportedChallenge
                | ChatError::LocalCancellation
                | ChatError::IdentityChanged
                | ChatError::SessionChanged
                | ChatError::RateLimited)
        ) {
            return stream_result;
        }
        let reconciled = self.reconcile_saved(journal).await;
        stream_result?;
        reconciled
    }

    pub(crate) async fn consume_stream(
        &self,
        request: RequestBuilder,
        prepared: &PreparedTurn,
        decoder: &mut TurnDecoder,
        observation: &mut SavedSendObservation,
        checkpoint: impl FnMut(&crate::stream::TurnState) -> Result<(), ChatError> + Send,
    ) -> Result<(), ChatError> {
        self.consume_stream_observed(
            request,
            prepared,
            decoder,
            observation,
            &mut crate::session::SynchronousSink(checkpoint),
        )
        .await
    }

    pub(crate) async fn consume_stream_observed(
        &self,
        request: RequestBuilder,
        prepared: &PreparedTurn,
        decoder: &mut TurnDecoder,
        observation: &mut SavedSendObservation,
        checkpoint: &mut impl crate::session::TurnSink,
    ) -> Result<(), ChatError> {
        let mut response = self
            .guarded_generation(prepared.generation, async {
                self.check_profile(&prepared.profile)?;
                // The browser transport's request deadline includes its body.
                // Give streaming the same bounded lifetime as the decoder;
                // guarded_generation still limits headers and each idle read.
                let response = request
                    .timeout(STREAM_TIMEOUT)
                    .send()
                    .await
                    .map_err(transport_error)?;
                self.observe_response(&response);
                observation.send_http_status = Some(response.status().as_u16());
                classify_status(response.status(), response.headers())?;
                if let Some(token) = response.headers().get("x-conduit-token") {
                    let token = token.to_str().map_err(|_| ChatError::ProtocolChanged)?;
                    decoder
                        .response_resume_token(token)
                        .map_err(|_| ChatError::ProtocolChanged)?;
                }
                let content_type = response
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if !content_type
                    .split(';')
                    .next()
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
                {
                    return Err(ChatError::ProtocolChanged);
                }
                Ok(response)
            })
            .await?;
        let result = tokio::time::timeout(STREAM_TIMEOUT, async {
            let mut received = 0usize;
            loop {
                let chunk = self
                    .guarded_generation(prepared.generation, async {
                        self.check_profile(&prepared.profile)?;
                        response.chunk().await.map_err(transport_error)
                    })
                    .await?;
                let Some(chunk) = chunk else {
                    break;
                };
                received = received.saturating_add(chunk.len());
                if received > 32 * 1024 * 1024 {
                    return Err(ChatError::ResponseTooLarge);
                }
                let result = decoder.push(&chunk);
                self.guarded_generation(
                    prepared.generation,
                    checkpoint.checkpoint(decoder.state()),
                )
                .await?;
                result.map_err(|error| {
                    observation.stream_error = Some(error);
                    ChatError::IncompleteStream
                })?;
            }
            let outcome = decoder.finish().map_err(|error| {
                observation.stream_error = Some(error);
                ChatError::IncompleteStream
            })?;
            match outcome {
                GenerationOutcome::Completed => Ok(()),
                GenerationOutcome::UnsupportedHandoff => Err(ChatError::UnsupportedCapability),
                GenerationOutcome::Failed | GenerationOutcome::Incomplete => {
                    Err(ChatError::IncompleteStream)
                }
            }
        })
        .await;
        result.map_err(|_| ChatError::Timeout)?
    }

    async fn verify_parent(&self, target: &PreparationTarget) -> Result<(), ChatError> {
        if let Some(id) = &target.conversation {
            let path = format!("conversation/{id}");
            let conversation: Value = self.read(&path, &path).await?;
            if conversation.get("current_node").and_then(Value::as_str)
                != Some(target.parent.as_str())
            {
                return Err(ChatError::BranchConflict);
            }
        }
        Ok(())
    }

    pub(crate) async fn prepared_request(
        &self,
        prepared: &PreparedTurn,
        path: &str,
    ) -> Result<RequestBuilder, ChatError> {
        self.check_owner()?;
        if self.auth.auth_change_state_receiver().borrow().generation != prepared.generation {
            return Err(ChatError::SessionChanged);
        }
        self.check_profile(&prepared.profile)?;
        let mut request = self
            .request(Method::POST, path, path)
            .await?
            .header("origin", &prepared.profile.bootstrap.origin)
            .header("referer", format!("{}/", prepared.profile.bootstrap.origin))
            .header("oai-device-id", &prepared.profile.device)
            .header("oai-client-version", &prepared.profile.bootstrap.build)
            .header("oai-language", "en-US")
            .header(
                "OpenAI-Sentinel-Chat-Requirements-Token",
                &prepared.requirements,
            );
        if let Some(proof) = &prepared.proof {
            request = request.header("OpenAI-Sentinel-Proof-Token", proof);
        }
        if let Some(turn) = &prepared.turnstile {
            request = request.header("OpenAI-Sentinel-Turnstile-Token", turn);
        }
        // Building the routed request may have replaced the HTTP pool because
        // configuration changed without an authentication revision.
        self.check_profile(&prepared.profile)?;
        Ok(request)
    }

    pub(crate) async fn observer_ping(
        &self,
        prepared: &PreparedTurn,
        observation: &mut SavedSendObservation,
    ) -> Result<(), ChatError> {
        let Some(observer) = &prepared.observer else {
            return Ok(());
        };
        let mut metadata = json!({"v":1,"sequence_number":0,"signals":{
            "ping_source":"session_observer_background_submit", "so_token_present":"1",
            "turnstile_token_present":if prepared.turnstile.is_some() {"1"} else {"0"},
            "proof_token_present":if prepared.proof.is_some() {"1"} else {"0"},
            "prepare_token_present":"1", "chat_requirements_token_present":"1"
        }});
        if let Some(id) = &prepared.target.conversation {
            metadata["conversation_id"] = json!(id);
            metadata["last_message_id"] = json!(prepared.target.parent);
        }
        let extra = BASE64_URL_SAFE
            .encode(serde_json::to_vec(&metadata).map_err(|_| ChatError::ProtocolChanged)?);
        self.guarded_generation(prepared.generation, async {
            let response = self
                .prepared_request(prepared, "sentinel/ping")
                .await?
                .header(
                    "OpenAI-Sentinel-Chat-Requirements-Prepare-Token",
                    &prepared.prepare_token,
                )
                .header("OpenAI-Sentinel-SO-Token", observer)
                .header("OpenAI-Sentinel-Extra-Data", extra)
                .json(&json!({}))
                .send()
                .await
                .map_err(transport_error)?;
            self.observe_response(&response);
            observation.observer_ping_status = Some(response.status().as_u16());
            classify_status(response.status(), response.headers())?;
            let _ = bounded_body(response).await?;
            Ok(())
        })
        .await
    }

    /// A fresh process may call this without preparation or a POST. Absence from
    /// history cannot prove non-acceptance and never unlocks another attempt.
    pub async fn reconcile_saved(&self, journal: &mut RunJournal) -> Result<(), ChatError> {
        self.bind_journal(journal)?;
        let attempt = journal.current().ok_or(ChatError::InvalidInput)?;
        let id = attempt
            .conversation_id
            .as_ref()
            .ok_or(ChatError::PendingSubmission)?;
        let path = format!("conversation/{id}");
        let conversation: Value = self.read(&path, &path).await?;
        let (messages, outcome) = reconcile(
            &conversation,
            &attempt.client_message_id,
            &attempt.parent_message_id,
        )?;
        journal.reconciled(messages, outcome).map_err(journal_error)
    }

    pub async fn inspect_saved_recovery(&self, journal: &mut RunJournal) -> SavedSendObservation {
        let mut observation = SavedSendObservation {
            error: self.reconcile_saved(journal).await.err(),
            ..SavedSendObservation::default()
        };
        fill_observation(journal, &mut observation);
        observation
    }
}

fn fill_observation(journal: &RunJournal, observation: &mut SavedSendObservation) {
    if let Some(attempt) = journal.current() {
        observation.acceptance = attempt.acceptance;
        observation.generation = attempt.outcome;
        observation.authoritative_retrieval = attempt.authoritative;
        observation.requested_model = Some(attempt.requested_model.clone());
        observation
            .resolved_model
            .clone_from(&attempt.resolved_model);
        observation.assistant_characters = attempt
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .map(|m| m.text().chars().count())
            .sum();
    }
}

fn journal_error(error: JournalError) -> ChatError {
    match error {
        JournalError::PendingSubmission | JournalError::InUse => ChatError::PendingSubmission,
        _ => ChatError::RecoveryUnavailable,
    }
}

pub(crate) fn reconcile(
    conversation: &Value,
    message_id: &str,
    parent: &str,
) -> Result<(Vec<Message>, GenerationOutcome), ChatError> {
    let mapping = conversation
        .get("mapping")
        .and_then(Value::as_object)
        .ok_or(ChatError::ProtocolChanged)?;
    if mapping.len() > 8192 {
        return Err(ChatError::ResponseTooLarge);
    }
    let user = mapping
        .get(message_id)
        .ok_or(ChatError::PendingSubmission)?;
    if user.get("parent").and_then(Value::as_str) != Some(parent)
        || user.pointer("/message/id").and_then(Value::as_str) != Some(message_id)
        || user.pointer("/message/author/role").and_then(Value::as_str) != Some("user")
    {
        return Err(ChatError::ProtocolChanged);
    }
    let user_message = Message::parse(&user["message"]).map_err(|_| ChatError::ProtocolChanged)?;
    let mut current = conversation
        .get("current_node")
        .and_then(Value::as_str)
        .ok_or(ChatError::ProtocolChanged)?;
    let mut visited = HashSet::new();
    let mut messages = Vec::new();
    while current != message_id {
        if !visited.insert(current) || visited.len() > 8192 {
            return Err(ChatError::ProtocolChanged);
        }
        let node = mapping.get(current).ok_or(ChatError::ProtocolChanged)?;
        if let Some(message) = node.get("message").filter(|m| !m.is_null()) {
            messages.push(Message::parse(message).map_err(|_| ChatError::ProtocolChanged)?);
        }
        match node.get("parent").and_then(Value::as_str) {
            Some(id) => current = id,
            // The exact user is accepted on a different branch. Preserve it;
            // another branch's completed response is not this turn's outcome.
            None => return Ok((vec![user_message], GenerationOutcome::Incomplete)),
        }
    }
    messages.push(user_message);
    messages.reverse();
    // A later user turn may be on the same active branch. Its generation must
    // not establish completion of the submission being reconciled.
    if let Some(next_user) = messages.iter().skip(1).position(|m| m.role == "user") {
        messages.truncate(next_user + 1);
    }
    let completed = messages
        .iter()
        .any(|m| m.role == "assistant" && m.finished && m.end_turn);
    Ok((
        messages,
        if completed {
            GenerationOutcome::Completed
        } else {
            GenerationOutcome::Incomplete
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn conversation() -> Value {
        json!({"current_node":"assistant","mapping":{
            "root":{"parent":null,"message":null},
            "user":{"parent":"root","message":{"id":"user","author":{"role":"user"},"content":{"content_type":"text","parts":["fixture"]}}},
            "assistant":{"parent":"user","message":{"id":"assistant","author":{"role":"assistant"},"status":"finished_successfully","end_turn":true,"content":{"content_type":"text","parts":["reply"]},"metadata":{"model_slug":"resolved"}}}
        }})
    }

    #[test]
    fn exact_message_and_parent_are_required_for_positive_reconciliation() {
        let value = conversation();
        let (messages, outcome) = reconcile(&value, "user", "root").unwrap();
        assert_eq!(outcome, GenerationOutcome::Completed);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].model.as_deref(), Some("resolved"));
        assert!(matches!(
            reconcile(&value, "missing", "root"),
            Err(ChatError::PendingSubmission)
        ));
        assert!(matches!(
            reconcile(&value, "user", "wrong"),
            Err(ChatError::ProtocolChanged)
        ));
    }

    #[test]
    fn accepted_user_is_preserved_when_generation_is_incomplete_or_branch_changed() {
        let mut value = conversation();
        value["mapping"]["assistant"]["message"]["status"] = json!("in_progress");
        assert_eq!(
            reconcile(&value, "user", "root").unwrap().1,
            GenerationOutcome::Incomplete
        );
        value["current_node"] = json!("root");
        let (messages, outcome) = reconcile(&value, "user", "root").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(outcome, GenerationOutcome::Incomplete);
    }

    #[test]
    fn a_later_turn_cannot_complete_an_earlier_failed_generation() {
        let mut value = conversation();
        value["mapping"]["assistant"]["message"]["status"] = json!("in_progress");
        value["mapping"]["later-user"] = value["mapping"]["user"].clone();
        value["mapping"]["later-user"]["message"]["id"] = json!("later-user");
        value["mapping"]["later-user"]["parent"] = json!("assistant");
        value["mapping"]["later-assistant"] = value["mapping"]["assistant"].clone();
        value["mapping"]["later-assistant"]["message"]["id"] = json!("later-assistant");
        value["mapping"]["later-assistant"]["message"]["status"] = json!("finished_successfully");
        value["mapping"]["later-assistant"]["parent"] = json!("later-user");
        value["current_node"] = json!("later-assistant");
        let (messages, outcome) = reconcile(&value, "user", "root").unwrap();
        assert_eq!(outcome, GenerationOutcome::Incomplete);
        assert_eq!(messages.len(), 2);
    }
}
