//! Temporary acceptance owns only memory. There is no journal, file path,
//! generic transcript logger, history writer, or transition into saved mode.

use crate::ChatError;
use crate::ChatProbe;
use crate::SavedSendObservation;
use crate::native_preparation::PreparationTarget;
use crate::native_preparation::PreparedTurn;
use crate::send::reconcile;
use crate::stream::Acceptance;
use crate::stream::GenerationOutcome;
use crate::stream::TurnDecoder;
use http::Method;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TemporaryProtocol {
    #[default]
    HistoryRead,
    ConduitRefreshV1,
}

impl std::str::FromStr for TemporaryProtocol {
    type Err = ChatError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "history-read" => Ok(Self::HistoryRead),
            "conduit-refresh-v1" => Ok(Self::ConduitRefreshV1),
            _ => Err(ChatError::InvalidInput),
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct TemporaryObservation {
    pub protocol: TemporaryProtocol,
    pub turns: Vec<SavedSendObservation>,
    pub conduit_refreshes: Vec<SavedSendObservation>,
    pub resume_token_present: bool,
    pub history_read_error: Option<ChatError>,
    pub server_temporary: Option<bool>,
    pub absent_from_recent_history: Option<bool>,
    pub error: Option<ChatError>,
}

struct TemporarySubmission<'a> {
    conversation: Option<&'a str>,
    parent: &'a str,
    message_id: &'a str,
    text: &'a str,
    marker: &'a str,
}

impl ChatProbe {
    /// Harness-only two-turn fixture. Identifiers, prompts and decoded messages
    /// exist only in this invocation and never enter the returned observation.
    pub async fn inspect_temporary(&self, protocol: TemporaryProtocol) -> TemporaryObservation {
        let mut observation = TemporaryObservation {
            protocol,
            ..Default::default()
        };
        observation.error = Box::pin(self.temporary_inner(&mut observation)).await.err();
        observation
    }

    async fn temporary_inner(
        &self,
        observation: &mut TemporaryObservation,
    ) -> Result<(), ChatError> {
        let marker = format!("NYACODE_NATIVE_TEMPORARY_{}", Uuid::new_v4());
        let mut conversation = None;
        let mut parent = Uuid::new_v4().to_string();
        for turn in 0..2 {
            let text = if turn == 0 {
                format!("Reply with exactly this marker and no other text: {marker}")
            } else {
                "Repeat the exact marker from my first message in this conversation, and nothing else.".into()
            };
            let message_id = Uuid::new_v4().to_string();
            let mut result = SavedSendObservation::default();
            let submission = TemporarySubmission {
                conversation: conversation.as_deref(),
                parent: &parent,
                message_id: &message_id,
                text: &text,
                marker: &marker,
            };
            let turn_result = self
                .temporary_turn(submission, &mut result, observation)
                .await;
            result.error = turn_result.as_ref().err().copied();
            observation.turns.push(result);
            let (id, current_parent) = turn_result?;
            conversation = Some(id);
            parent = current_parent;
        }
        let id = conversation.ok_or(ChatError::ProtocolChanged)?;
        // Absence is only supplemental evidence. The authoritative response
        // must explicitly mark the conversation temporary before continuation.
        let history: Value = self
            .read(
                "conversations?offset=0&limit=100&order=updated",
                "conversations",
            )
            .await?;
        let entries = history
            .get("items")
            .and_then(Value::as_array)
            .ok_or(ChatError::ProtocolChanged)?;
        if entries.len() > 100 {
            return Err(ChatError::ResponseTooLarge);
        }
        let absent = !entries
            .iter()
            .any(|entry| entry.get("id").and_then(Value::as_str) == Some(id.as_str()));
        observation.absent_from_recent_history = Some(absent);
        if !absent {
            return Err(ChatError::ProtocolChanged);
        }
        Ok(())
    }

    async fn temporary_turn(
        &self,
        submission: TemporarySubmission<'_>,
        observation: &mut SavedSendObservation,
        temporary: &mut TemporaryObservation,
    ) -> Result<(String, String), ChatError> {
        let TemporarySubmission {
            conversation,
            parent,
            message_id,
            text,
            marker,
        } = submission;
        if let Some(id) = &conversation
            && temporary.protocol == TemporaryProtocol::HistoryRead
        {
            let snapshot = self.read_temporary(id, temporary).await?;
            if snapshot.get("current_node").and_then(Value::as_str) != Some(parent) {
                return Err(ChatError::BranchConflict);
            }
        }
        let target = PreparationTarget {
            parent: parent.into(),
            conversation: conversation.map(str::to_owned),
            model: None,
            temporary: true,
        };
        let prepared = self
            .prepare_native(target, &mut observation.preparation)
            .await?;
        observation.requested_model = Some(prepared.model.clone());
        self.observer_ping(&prepared, observation).await?;
        if let Some(id) = &conversation
            && temporary.protocol == TemporaryProtocol::HistoryRead
        {
            let snapshot = self.read_temporary(id, temporary).await?;
            if snapshot.get("current_node").and_then(Value::as_str) != Some(parent) {
                return Err(ChatError::BranchConflict);
            }
        }
        let mut body = json!({
            "action":"next", "parent_message_id":parent, "model":prepared.model,
            "history_and_training_disabled":true,
            "conversation_mode":{"kind":"primary_assistant"},
            "timezone_offset_min":0, "timezone":"UTC", "fork_from_shared_post":false,
            "system_hints":[], "supports_buffering":true, "supported_encodings":["v1"],
            "messages":[{"id":message_id,"author":{"role":"user"},"create_time":chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
                "content":{"content_type":"text","parts":[text]},"metadata":{}}]
        });
        if let Some(id) = &conversation {
            body["conversation_id"] = json!(id);
        } else {
            body["temporary_chat_requests_personalization"] = json!(false);
        }
        let request = self
            .prepared_request(&prepared, "f/conversation")
            .await?
            .header("accept", "text/event-stream")
            .header("x-conduit-token", &prepared.conduit)
            .json(&body);
        let mut decoder = TurnDecoder::new(message_id.into());
        let stream_result = self
            .consume_stream(request, &prepared, &mut decoder, observation, |_| Ok(()))
            .await;
        observation.acceptance = decoder.state().acceptance;
        observation.generation = decoder.state().outcome();
        observation
            .resolved_model
            .clone_from(&decoder.state().resolved_model);
        observation.unknown_metadata_events = decoder.state().unknown_metadata_events;
        observation.assistant_characters = decoder
            .state()
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .map(|m| m.text().chars().count())
            .sum();
        observation.fixture_response_matched = Some(
            decoder
                .state()
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "assistant" && m.end_turn)
                .is_some_and(|m| m.text().trim() == marker),
        );
        stream_result?;
        let id = decoder
            .state()
            .conversation_id
            .as_ref()
            .ok_or(ChatError::ProtocolChanged)?;
        if temporary.protocol == TemporaryProtocol::ConduitRefreshV1 {
            let parent = self
                .refresh_temporary_conduit(&prepared, &decoder, message_id, marker, temporary)
                .await?;
            observation.authoritative_retrieval = true;
            if observation.fixture_response_matched != Some(true) {
                return Err(ChatError::IncompleteStream);
            }
            // A normal saved-history read must either explicitly mark this
            // temporary or remain unavailable. This is never used to retry a
            // user message or convert the conversation into saved mode.
            match self.read_temporary(id, temporary).await {
                Ok(_) => {}
                Err(ChatError::NotFound) => {
                    temporary.history_read_error = Some(ChatError::NotFound)
                }
                Err(error) => return Err(error),
            }
            return Ok((id.clone(), parent));
        }
        let snapshot = self.read_temporary(id, temporary).await?;
        let (messages, outcome) = reconcile(&snapshot, message_id, parent)?;
        observation.acceptance = Acceptance::Accepted;
        observation.generation = outcome;
        observation.authoritative_retrieval = true;
        if outcome != GenerationOutcome::Completed
            || observation.fixture_response_matched != Some(true)
            || messages
                .iter()
                .rev()
                .find(|m| m.role == "assistant" && m.end_turn)
                .is_none_or(|m| m.text().trim() != marker)
        {
            return Err(ChatError::IncompleteStream);
        }
        let current = snapshot
            .get("current_node")
            .and_then(Value::as_str)
            .ok_or(ChatError::ProtocolChanged)?;
        Ok((id.clone(), current.into()))
    }

    async fn read_temporary(
        &self,
        id: &str,
        observation: &mut TemporaryObservation,
    ) -> Result<Value, ChatError> {
        let path = format!("conversation/{id}");
        let snapshot: Value = self.read(&path, &path).await?;
        // Observed in the official runtime's snapshot/cache exclusion contract.
        // Missing metadata is not permission to assume temporary semantics.
        observation.server_temporary = snapshot.get("is_temporary_chat").and_then(Value::as_bool);
        if observation.server_temporary != Some(true) {
            return Err(ChatError::UnsupportedCapability);
        }
        Ok(snapshot)
    }

    async fn refresh_temporary_conduit(
        &self,
        prepared: &PreparedTurn,
        original: &TurnDecoder,
        message_id: &str,
        marker: &str,
        observation: &mut TemporaryObservation,
    ) -> Result<String, ChatError> {
        let state = original.state();
        let id = state
            .conversation_id
            .as_deref()
            .ok_or(ChatError::ProtocolChanged)?;
        observation.resume_token_present = state.resume_token().is_some();
        let token = state
            .resume_token()
            .ok_or(ChatError::UnsupportedCapability)?;
        let expected_parent = state
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant" && m.finished && m.end_turn)
            .map(|m| m.id.as_str())
            .ok_or(ChatError::IncompleteStream)?;
        let request = self
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
        let mut decoder = TurnDecoder::new(message_id.into());
        let mut result = SavedSendObservation::default();
        let read_result = self
            .consume_stream(request, prepared, &mut decoder, &mut result, |_| Ok(()))
            .await;
        result.error = read_result.as_ref().err().copied();
        result.acceptance = decoder.state().acceptance;
        result.generation = decoder.state().outcome();
        result.requested_model = Some(prepared.model.clone());
        result
            .resolved_model
            .clone_from(&decoder.state().resolved_model);
        result.unknown_metadata_events = decoder.state().unknown_metadata_events;
        result.assistant_characters = decoder
            .state()
            .messages
            .iter()
            .filter(|message| message.role == "assistant")
            .map(|message| message.text().chars().count())
            .sum();
        let parent = decoder
            .state()
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant" && m.finished && m.end_turn);
        result.fixture_response_matched =
            Some(parent.is_some_and(|m| m.id == expected_parent && m.text().trim() == marker));
        let valid = read_result.is_ok()
            && result.acceptance == Acceptance::Accepted
            && result.fixture_response_matched == Some(true)
            && decoder.state().conversation_id.as_deref() == Some(id);
        if !valid && result.error.is_none() {
            result.error = Some(ChatError::ProtocolChanged);
        }
        result.authoritative_retrieval = valid;
        observation.conduit_refreshes.push(result);
        read_result?;
        if !valid {
            return Err(ChatError::ProtocolChanged);
        }
        Ok(expected_parent.into())
    }
}
