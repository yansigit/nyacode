//! Explicitly invoked preparation research, independent of the agent and TUI.
//!
//! Candidate preparation request adapted from gpt2agent (MIT); see ../NOTICE.
//! A successful HTTP exchange is not proof that a conversation can be sent.

use crate::ChatError;
use crate::ChatProbe;
use crate::probe::bounded_body;
use crate::probe::classify_response;
use crate::probe::classify_status;
use crate::probe::transport_error;
use http::Method;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparationOperation {
    SessionBootstrap,
    ConversationInit,
    ConversationPrepare,
    /// Bootstrap without a browser fingerprint or an undistributed solver.
    SentinelRequirementsWithoutProof,
    SentinelPrepare,
    SentinelCompute,
    SentinelFinalize,
    SessionObserver,
    SessionObserverSnapshot,
}

/// Only an allowlist of structural observations may leave the transport. Neither
/// raw responses nor token/challenge values implement Debug or Serialize here.
#[derive(Debug, Default, Serialize)]
pub struct PreparationShape {
    pub default_model_present: bool,
    pub conduit_token_present: bool,
    pub requirements_token_present: bool,
    pub proof_of_work_required: Option<bool>,
    pub turnstile_required: Option<bool>,
    pub arkose_required: Option<bool>,
    pub turnstile_payload_present: bool,
    pub prepare_token_present: bool,
    pub observer_required: Option<bool>,
    pub observer_collector_present: bool,
    pub observer_snapshot_present: bool,
}

impl PreparationShape {
    pub(crate) fn from_value(value: &Value) -> Result<Self, ChatError> {
        if !value.is_object() {
            return Err(ChatError::ProtocolChanged);
        }
        Ok(Self {
            default_model_present: nonempty_string(value, "/default_model_slug"),
            conduit_token_present: nonempty_string(value, "/conduit_token"),
            requirements_token_present: nonempty_string(value, "/token"),
            proof_of_work_required: required(value, "/proofofwork/required")?,
            turnstile_required: required(value, "/turnstile/required")?,
            arkose_required: required(value, "/arkose/required")?,
            turnstile_payload_present: nonempty_string(value, "/turnstile/dx"),
            prepare_token_present: nonempty_string(value, "/prepare_token"),
            observer_required: required(value, "/so/required")?,
            observer_collector_present: nonempty_string(value, "/so/collector_dx"),
            observer_snapshot_present: nonempty_string(value, "/so/snapshot_dx"),
        })
    }

    fn requires_challenge(&self) -> bool {
        self.proof_of_work_required == Some(true)
            || self.turnstile_required == Some(true)
            || self.arkose_required == Some(true)
    }
}

fn nonempty_string(value: &Value, pointer: &str) -> bool {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
}

fn required(value: &Value, pointer: &str) -> Result<Option<bool>, ChatError> {
    value
        .pointer(pointer)
        .map(|value| value.as_bool().ok_or(ChatError::ProtocolChanged))
        .transpose()
}

#[derive(Debug, Serialize)]
pub struct PreparationObservation {
    pub operation: PreparationOperation,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    pub cloudflare_challenge: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<PreparationShape>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ChatError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_failure: Option<crate::SentinelFailure>,
}

impl ChatProbe {
    /// Diagnostics only: does not send a user message, solve challenges, or
    /// create a saved conversation. POSTs are never automatically replayed.
    pub async fn inspect_preparation(&self, temporary: bool) -> Vec<PreparationObservation> {
        let permit = self.preparation_permit.try_acquire();
        let preflight = match &permit {
            Ok(_) => self.check_session_limits(true),
            Err(_) => Err(ChatError::PreparationInProgress),
        };
        if let Err(error) = preflight {
            return vec![PreparationObservation {
                operation: PreparationOperation::SessionBootstrap,
                latency_ms: 0,
                http_status: None,
                cloudflare_challenge: false,
                response: None,
                error: Some(error),
                native_failure: None,
            }];
        }
        let mut observations = Vec::new();
        for (operation, path, body) in [
            (PreparationOperation::SessionBootstrap, "../", Value::Null),
            (
                PreparationOperation::ConversationInit,
                "conversation/init",
                json!({"conversation_mode_kind": "primary_assistant"}),
            ),
            (
                PreparationOperation::ConversationPrepare,
                "f/conversation/prepare",
                json!({
                    "action": "next",
                    "fork_from_shared_post": false,
                    "parent_message_id": "client-created-root",
                    "model": "auto",
                    "timezone_offset_min": 0,
                    "timezone": "UTC",
                    "history_and_training_disabled": temporary,
                    "conversation_mode": {"kind": "primary_assistant"},
                    "system_hints": [],
                    "supports_buffering": true,
                    "supported_encodings": ["v1"]
                }),
            ),
            (
                PreparationOperation::SentinelRequirementsWithoutProof,
                "sentinel/chat-requirements",
                json!({}),
            ),
        ] {
            let started = Instant::now();
            let mut observation = PreparationObservation {
                operation,
                latency_ms: 0,
                http_status: None,
                cloudflare_challenge: false,
                response: None,
                error: None,
                native_failure: None,
            };
            let result = self
                .guarded(async {
                    let mut request = if operation == PreparationOperation::SessionBootstrap {
                        self.bootstrap_request().await?
                    } else {
                        self.request(Method::POST, path, path).await?.json(&body)
                    };
                    if operation == PreparationOperation::ConversationPrepare {
                        request = request.header("x-conduit-token", "no-token");
                    }
                    let response = request.send().await.map_err(transport_error)?;
                    self.observe_response(&response);
                    observation.http_status = Some(response.status().as_u16());
                    observation.cloudflare_challenge = response
                        .headers()
                        .get("cf-mitigated")
                        .is_some_and(|value| value == "challenge");
                    if operation == PreparationOperation::SessionBootstrap {
                        classify_status(response.status(), response.headers())?;
                        // Retain only the shared client's allowlisted infrastructure
                        // cookies. Discard the bounded HTML body without inspecting it.
                        bounded_body(response).await?;
                        return Ok(());
                    }
                    classify_response(response.status(), response.headers())?;
                    let body = bounded_body(response).await?;
                    let value: Value =
                        serde_json::from_slice(&body).map_err(|_| ChatError::ProtocolChanged)?;
                    let shape = PreparationShape::from_value(&value)?;
                    let challenge = shape.requires_challenge();
                    let expected = match operation {
                        PreparationOperation::SessionBootstrap => false,
                        PreparationOperation::ConversationInit => shape.default_model_present,
                        PreparationOperation::ConversationPrepare => shape.conduit_token_present,
                        PreparationOperation::SentinelRequirementsWithoutProof => {
                            shape.requirements_token_present
                        }
                        _ => false,
                    };
                    observation.response = Some(shape);
                    if challenge {
                        return Err(ChatError::UnsupportedChallenge);
                    }
                    if !expected {
                        return Err(ChatError::ProtocolChanged);
                    }
                    Ok(())
                })
                .await;
            observation.error = result.err();
            observation.latency_ms =
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            // Stop on identity, access, or policy errors. Never hammer a refused
            // endpoint, refresh/replay a POST, or try another inference backend.
            let stop = observation.error.is_some();
            observations.push(observation);
            if stop {
                break;
            }
        }
        self.finish_preparation(observations.iter().find_map(|step| step.error));
        observations
    }
}
