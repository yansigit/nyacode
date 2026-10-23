//! Turn reduction is independent of framing and backend acceptance. A terminal
//! marker cannot establish acceptance of the submitted user message.

use super::DeltaDecoder;
use super::SseDecoder;
use super::SseEvent;
use super::StreamError;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Acceptance {
    #[default]
    Unknown,
    Accepted,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationOutcome {
    #[default]
    Incomplete,
    Completed,
    Failed,
    UnsupportedHandoff,
}

// Content types deliberately do not implement Debug. Serialization is for the
// private saved-run journal, never generic request or diagnostic logging.
#[derive(Clone, Serialize, Deserialize)]
pub enum MessagePart {
    Text(String),
    Image(crate::content::ImageReference),
    Unsupported { content_type: String },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: String,
    pub parts: Vec<MessagePart>,
    pub model: Option<String>,
    pub finished: bool,
    pub end_turn: bool,
    #[serde(default)]
    pub attachments: Vec<crate::content::AttachmentReference>,
    #[serde(default)]
    pub citations: Vec<crate::content::Citation>,
}

impl Message {
    /// Apply the same presentation boundary to live turns and saved history.
    /// Internal context must never enter client snapshots or submission journals.
    pub(crate) fn is_visible(value: &Value) -> Result<bool, StreamError> {
        let role = short_string(value.pointer("/author/role"), 64)?;
        let hidden = match value.pointer("/metadata/is_visually_hidden_from_conversation") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(hidden)) => *hidden,
            _ => return Err(StreamError::InvalidProtocol),
        };
        let internal = matches!(
            value
                .pointer("/content/content_type")
                .and_then(Value::as_str),
            Some("model_editable_context" | "user_editable_context")
        );
        Ok(!hidden && !internal && !matches!(role, "system" | "developer"))
    }

    pub(crate) fn parse(value: &Value) -> Result<Self, StreamError> {
        let id = identifier(value.get("id"))?.to_owned();
        let role = short_string(value.pointer("/author/role"), 64)?.to_owned();
        let mut parts = Vec::new();
        let kind = value
            .pointer("/content/content_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if matches!(kind, "text" | "multimodal_text") {
            let entries = value
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .ok_or(StreamError::InvalidProtocol)?;
            if entries.len() > 256 {
                return Err(StreamError::ContentLimit);
            }
            for entry in entries {
                let part = if let Some(text) = entry.as_str() {
                    MessagePart::Text(text.to_owned())
                } else if kind == "multimodal_text"
                    && let Some(reference) = crate::content::ImageReference::parse(entry)
                {
                    MessagePart::Image(reference)
                } else {
                    MessagePart::Unsupported {
                        content_type: "unknown_text_part".into(),
                    }
                };
                parts.push(part);
            }
        } else {
            // Unknown content is never rendered as arbitrary JSON, executed, or
            // interpreted as an attachment URL. Known categories are added only
            // after their wire contracts have been verified.
            parts.push(MessagePart::Unsupported {
                content_type: short_string(Some(&Value::String(kind.into())), 128)?.into(),
            });
        }
        let model = value
            .pointer("/metadata/model_slug")
            .filter(|value| !value.is_null())
            .map(|v| short_string(Some(v), 256).map(str::to_owned))
            .transpose()?;
        let (attachments, unsupported) = crate::content::attachments(value)?;
        if unsupported {
            parts.push(MessagePart::Unsupported {
                content_type: "attachment_metadata".into(),
            });
        }
        let (citations, unsupported) = crate::content::citations(value)?;
        if unsupported {
            parts.push(MessagePart::Unsupported {
                content_type: "citation_metadata".into(),
            });
        }
        Ok(Self {
            id,
            role,
            parts,
            model,
            finished: value.get("status").and_then(Value::as_str) == Some("finished_successfully"),
            end_turn: value.get("end_turn").and_then(Value::as_bool) == Some(true),
            attachments,
            citations,
        })
    }

    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Conservative retained-content size used for transcript capacity checks.
    pub fn weight(&self) -> usize {
        1024 + self
            .parts
            .iter()
            .map(|part| match part {
                MessagePart::Text(text) => text.len(),
                MessagePart::Unsupported { content_type } => content_type.len(),
                MessagePart::Image(reference) => 128 + reference.file_id.len(),
            })
            .sum::<usize>()
            + self
                .attachments
                .iter()
                .map(crate::content::AttachmentReference::weight)
                .sum::<usize>()
            + self
                .citations
                .iter()
                .map(crate::content::Citation::weight)
                .sum::<usize>()
    }
}

pub struct TurnState {
    pub conversation_id: Option<String>,
    pub acceptance: Acceptance,
    pub messages: Vec<Message>,
    pub resolved_model: Option<String>,
    pub unknown_metadata_events: usize,
    expected_message_id: String,
    done: bool,
    terminal: bool,
    failed: bool,
    handoff: bool,
    resume_token: Option<String>,
}

impl TurnState {
    pub(crate) fn resume_token(&self) -> Option<&str> {
        self.resume_token.as_deref()
    }

    pub(crate) fn set_resume_token(&mut self, token: &str) -> Result<(), StreamError> {
        if token.is_empty() || token.len() > 512 * 1024 {
            return Err(StreamError::InvalidProtocol);
        }
        self.resume_token = Some(token.into());
        Ok(())
    }
    pub fn outcome(&self) -> GenerationOutcome {
        if self.handoff {
            return GenerationOutcome::UnsupportedHandoff;
        }
        if self.failed {
            return GenerationOutcome::Failed;
        }
        if self.done
            && self.terminal
            && self
                .messages
                .iter()
                .any(|m| m.role == "assistant" && m.finished && m.end_turn)
        {
            GenerationOutcome::Completed
        } else {
            GenerationOutcome::Incomplete
        }
    }

    fn reduce(&mut self, value: &Value) -> Result<(), StreamError> {
        let object = value.as_object().ok_or(StreamError::InvalidProtocol)?;
        if let Some(id) = object.get("conversation_id").filter(|v| !v.is_null()) {
            let id = identifier(Some(id))?;
            if self.conversation_id.as_deref().is_some_and(|old| old != id) {
                return Err(StreamError::InvalidProtocol);
            }
            self.conversation_id.get_or_insert_with(|| id.to_owned());
        }
        if object.get("error").is_some_and(|v| !v.is_null()) {
            self.failed = true;
            return Ok(());
        }
        let kind = object.get("type").and_then(Value::as_str);
        let message = match kind {
            Some("input_message") => object.get("input_message"),
            Some("message_stream_complete") => {
                self.terminal = true;
                None
            }
            Some("stream_handoff") => {
                self.handoff = true;
                None
            }
            Some("resume_conversation_token") => {
                self.set_resume_token(short_string(object.get("token"), 512 * 1024)?)?;
                None
            }
            Some("server_ste_metadata") => {
                if let Some(model) = value.pointer("/metadata/model_slug") {
                    self.resolved_model = Some(short_string(Some(model), 256)?.into());
                }
                None
            }
            Some("streaming_parent_patch") => return Err(StreamError::InvalidProtocol),
            Some(_) => {
                self.unknown_metadata_events = self.unknown_metadata_events.saturating_add(1);
                None
            }
            None => object.get("message"),
        };
        if let Some(message) = message.filter(|m| !m.is_null()) {
            let visible = Message::is_visible(message)?;
            let message = Message::parse(message)?;
            if message.role == "user" && message.id == self.expected_message_id {
                self.acceptance = Acceptance::Accepted;
            }
            if !visible {
                self.messages.retain(|old| old.id != message.id);
                return Ok(());
            }
            if message.role == "assistant"
                && let Some(model) = &message.model
            {
                self.resolved_model = Some(model.clone());
            }
            if let Some(old) = self.messages.iter_mut().find(|m| m.id == message.id) {
                *old = message;
            } else {
                if self.messages.len() >= 128 {
                    return Err(StreamError::ContentLimit);
                }
                self.messages.push(message);
            }
            if self.messages.iter().map(Message::weight).sum::<usize>() > 8 * 1024 * 1024 {
                return Err(StreamError::ContentLimit);
            }
        }
        Ok(())
    }
}

pub struct TurnDecoder {
    sse: SseDecoder,
    delta: DeltaDecoder,
    v1: bool,
    state: TurnState,
}

impl TurnDecoder {
    pub(crate) fn into_state(self) -> TurnState {
        self.state
    }

    pub fn new(expected_message_id: String) -> Self {
        Self {
            sse: SseDecoder::new(),
            delta: DeltaDecoder::new(),
            v1: false,
            state: TurnState {
                conversation_id: None,
                acceptance: Acceptance::Unknown,
                messages: Vec::new(),
                resolved_model: None,
                unknown_metadata_events: 0,
                expected_message_id,
                done: false,
                terminal: false,
                failed: false,
                handoff: false,
                resume_token: None,
            },
        }
    }

    pub fn state(&self) -> &TurnState {
        &self.state
    }

    pub(crate) fn response_resume_token(&mut self, token: &str) -> Result<(), StreamError> {
        self.state.set_resume_token(token)
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<(), StreamError> {
        let Self {
            sse,
            delta,
            v1,
            state,
        } = self;
        sse.push(chunk, |event| apply_event(event, delta, v1, state))
    }

    pub fn finish(&mut self) -> Result<GenerationOutcome, StreamError> {
        self.sse.finish()?;
        Ok(self.state.outcome())
    }
}

fn apply_event(
    event: SseEvent,
    delta: &mut DeltaDecoder,
    v1: &mut bool,
    state: &mut TurnState,
) -> Result<(), StreamError> {
    if state.done {
        return Err(StreamError::InvalidProtocol);
    }
    if event.data == "[DONE]" {
        if event.event != "message" {
            return Err(StreamError::InvalidProtocol);
        }
        state.done = true;
        return Ok(());
    }
    match event.event.as_str() {
        "delta_encoding"
            if !*v1
                && (event.data == "v1"
                    || serde_json::from_str::<String>(&event.data)
                        .is_ok_and(|value| value == "v1")) =>
        {
            *v1 = true;
            Ok(())
        }
        "delta" if *v1 => {
            let value =
                serde_json::from_str(&event.data).map_err(|_| StreamError::InvalidProtocol)?;
            if let Some(value) = delta.apply(value)? {
                state.reduce(value)?;
            }
            Ok(())
        }
        "message" => {
            let value =
                serde_json::from_str(&event.data).map_err(|_| StreamError::InvalidProtocol)?;
            state.reduce(&value)
        }
        _ => Err(StreamError::InvalidProtocol),
    }
}

fn short_string(value: Option<&Value>, limit: usize) -> Result<&str, StreamError> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= limit)
        .ok_or(StreamError::InvalidProtocol)
}

pub(crate) fn identifier(value: Option<&Value>) -> Result<&str, StreamError> {
    let id = short_string(value, 128)?;
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(StreamError::InvalidProtocol);
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn multimodal_text_keeps_text_and_safe_attachment_metadata_without_download_urls() {
        let value = serde_json::json!({"id":"user","author":{"role":"user"},"content":{"content_type":"multimodal_text","parts":[
            {"content_type":"image_asset_pointer","asset_pointer":"file-service://file-picture","width":12,"height":8,"size_bytes":99},
            "What is shown?",
            {"content_type":"arbitrary_asset_pointer","asset_pointer":"https://storage.invalid/?sig=private"}
        ]},"metadata":{"attachments":[{"id":"file-picture","name":"fixture.png","mime_type":"image/png","size":99,"download_url":"https://storage.invalid/?sig=private"},{"unknown":"secret"}]}});
        let message = Message::parse(&value).unwrap();
        assert_eq!(message.text(), "What is shown?");
        assert!(
            matches!(&message.parts[0], MessagePart::Image(image) if image.file_id == "file-picture" && image.width == Some(12))
        );
        assert_eq!(message.attachments.len(), 1);
        assert_eq!(message.attachments[0].name, "fixture.png");
        let encoded = serde_json::to_string(&message).unwrap();
        for forbidden in ["storage.invalid", "private", "secret", "download_url"] {
            assert!(!encoded.contains(forbidden));
        }
    }
    fn event(decoder: &mut TurnDecoder, value: Value) {
        decoder
            .push(format!("data: {value}\n\n").as_bytes())
            .unwrap();
    }

    fn message(id: &str, role: &str) -> Value {
        json!({"id":id,"author":{"role":role},"content":{"content_type":"text","parts":["fixture"]},"status":"finished_successfully","end_turn":true})
    }

    #[test]
    fn acceptance_and_completion_are_independent_and_eof_is_incomplete() {
        let mut decoder = TurnDecoder::new("user".into());
        event(
            &mut decoder,
            json!({"type":"input_message","conversation_id":"conversation","input_message":message("other", "user")}),
        );
        assert_eq!(decoder.state().acceptance, Acceptance::Unknown);
        event(
            &mut decoder,
            json!({"message":message("assistant", "assistant")}),
        );
        event(&mut decoder, json!({"type":"message_stream_complete"}));
        assert_eq!(decoder.finish().unwrap(), GenerationOutcome::Incomplete);
        decoder.push(b"data: [DONE]\n\n").unwrap();
        assert_eq!(decoder.finish().unwrap(), GenerationOutcome::Completed);
        assert_eq!(decoder.state().acceptance, Acceptance::Unknown);
        assert!(decoder.push(b"data: {}\n\n").is_err());
    }

    #[test]
    fn accepted_user_survives_generation_error_and_handoff_is_never_complete() {
        for handoff in [false, true] {
            let mut decoder = TurnDecoder::new("user".into());
            event(
                &mut decoder,
                json!({"type":"input_message","input_message":message("user", "user")}),
            );
            event(
                &mut decoder,
                if handoff {
                    json!({"type":"stream_handoff"})
                } else {
                    json!({"error":{"message":"private failure"}})
                },
            );
            decoder.push(b"data: [DONE]\n\n").unwrap();
            assert_eq!(decoder.state().acceptance, Acceptance::Accepted);
            assert_eq!(
                decoder.finish().unwrap(),
                if handoff {
                    GenerationOutcome::UnsupportedHandoff
                } else {
                    GenerationOutcome::Failed
                }
            );
        }
    }

    #[test]
    fn fragmented_delta_updates_replace_text_and_preserve_model_substitution() {
        let mut decoder = TurnDecoder::new("user".into());
        let stream = format!(
            "event: delta_encoding\ndata: v1\n\nevent: delta\ndata: {}\n\nevent: delta\ndata: {}\n\ndata: {}\n\n",
            json!({"v":{"message":message("assistant", "assistant")}}),
            json!({"p":"/message","o":"patch","v":[{"p":"/content/parts/0","o":"replace","v":"é😀"},{"p":"/metadata/model_slug","o":"add","v":"resolved-model"}]}),
            json!({"type":"message_stream_complete"})
        );
        for byte in stream.as_bytes().chunks(1) {
            decoder.push(byte).unwrap();
        }
        assert_eq!(decoder.state().messages[0].text(), "é😀");
        assert_eq!(
            decoder.state().resolved_model.as_deref(),
            Some("resolved-model")
        );
        assert_eq!(decoder.finish().unwrap(), GenerationOutcome::Incomplete);
    }

    #[test]
    fn malformed_frames_changed_conversations_and_unnegotiated_encoding_fail_closed() {
        for input in [
            "event: delta\ndata: {\"v\":{}}\n\n",
            "event: delta_encoding\ndata: v2\n\n",
            "data: {bad}\n\n",
            "data: {\"conversation_id\":\"one\"}\n\ndata: {\"conversation_id\":\"two\"}\n\n",
        ] {
            assert!(
                TurnDecoder::new("user".into())
                    .push(input.as_bytes())
                    .is_err()
            );
        }
        let mut decoder = TurnDecoder::new("user".into());
        decoder
            .push(b"event: delta_encoding\ndata: \"v1\"\n\nevent: delta\ndata: {\"v\":{}}\n\n")
            .unwrap();
    }
}

#[cfg(test)]
mod visibility_tests {
    use super::*;
    use serde_json::json;

    fn message(id: &str, role: &str, kind: &str, hidden: bool, model: &str) -> Value {
        json!({"id":id,"author":{"role":role},
            "content":{"content_type":kind,"parts":["fixture text"]},
            "metadata":{"is_visually_hidden_from_conversation":hidden,"model_slug":model},
            "status":"finished_successfully","end_turn":true})
    }

    #[test]
    fn live_internal_messages_do_not_pollute_transcripts_or_response_model() {
        let mut decoder = TurnDecoder::new("user".into());
        let state = &mut decoder.state;
        state.reduce(&json!({"type":"input_message","input_message":message("user","user","text",false,"input-model")})).unwrap();
        state
            .reduce(
                &json!({"message":message("assistant","assistant","text",false,"response-model")}),
            )
            .unwrap();
        for (id, role, kind, hidden) in [
            ("system", "system", "text", false),
            ("developer", "developer", "text", false),
            ("context", "assistant", "model_editable_context", false),
            ("profile", "user", "user_editable_context", false),
            ("hidden", "assistant", "text", true),
        ] {
            state
                .reduce(&json!({"message":message(id,role,kind,hidden,"internal-model")}))
                .unwrap();
        }
        state
            .reduce(&json!({"type":"message_stream_complete"}))
            .unwrap();
        state.done = true;
        assert_eq!(state.acceptance, Acceptance::Accepted);
        assert_eq!(state.outcome(), GenerationOutcome::Completed);
        assert_eq!(state.resolved_model.as_deref(), Some("response-model"));
        assert_eq!(
            state
                .messages
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["user", "assistant"]
        );
        // Visible unknown content must still have an explicit placeholder.
        state
            .reduce(&json!({"message":message("audio","assistant","audio",false,"response-model")}))
            .unwrap();
        assert!(matches!(
            state.messages.last().unwrap().parts[0],
            MessagePart::Unsupported { .. }
        ));
    }

    #[test]
    fn hidden_input_can_establish_acceptance_without_becoming_transcript_content() {
        let mut decoder = TurnDecoder::new("user".into());
        decoder.state.reduce(&json!({"type":"input_message","input_message":message("user","user","text",true,"input-model")})).unwrap();
        assert_eq!(decoder.state.acceptance, Acceptance::Accepted);
        assert!(decoder.state.messages.is_empty());
    }

    #[test]
    fn a_visibility_update_removes_an_existing_message_and_invalid_flags_fail_closed() {
        let mut decoder = TurnDecoder::new("user".into());
        decoder
            .state
            .reduce(&json!({"message":message("assistant","assistant","text",false,"model")}))
            .unwrap();
        decoder
            .state
            .reduce(&json!({"message":message("assistant","assistant","text",true,"model")}))
            .unwrap();
        assert!(decoder.state.messages.is_empty());
        let mut invalid = message("other", "assistant", "text", false, "model");
        invalid["metadata"]["is_visually_hidden_from_conversation"] = json!("yes");
        assert!(Message::is_visible(&invalid).is_err());
    }
}
