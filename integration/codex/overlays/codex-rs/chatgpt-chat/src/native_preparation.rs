//! Explicit, versioned native preparation candidate. No automatic protocol
//! fallback and no capability promotion from successful HTTP preparation alone.

use crate::ChatError;
use crate::ChatProbe;
use crate::PreparationObservation;
use crate::PreparationOperation;
use crate::ProbeTransport;
use crate::SentinelFailure;
use crate::preparation::PreparationShape;
use crate::probe::bounded_body;
use crate::probe::classify_response;
use crate::probe::classify_status;
use crate::probe::transport_error;
use crate::sentinel;
use http::Method;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreparationProtocol {
    #[default]
    Diagnostic,
    TwoPhaseV1,
}

impl std::str::FromStr for PreparationProtocol {
    type Err = ChatError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "diagnostic" => Ok(Self::Diagnostic),
            "two-phase-v1" => Ok(Self::TwoPhaseV1),
            _ => Err(ChatError::InvalidInput),
        }
    }
}

#[derive(Clone, Copy)]
struct PreparationContext<'a> {
    profile: &'a sentinel::Profile,
    generation: u64,
}

pub(crate) struct PreparationTarget {
    pub parent: String,
    pub conversation: Option<String>,
    pub model: Option<String>,
    pub temporary: bool,
}

// Opaque, one-use, in-memory preparation. No Clone/Debug/Serialize: live tokens
// cannot enter a journal, diagnostics, or another session's send operation.
pub(crate) struct PreparedTurn {
    pub profile: Arc<sentinel::Profile>,
    pub generation: u64,
    pub target: PreparationTarget,
    pub model: String,
    pub conduit: String,
    pub requirements: String,
    pub prepare_token: String,
    pub proof: Option<String>,
    pub turnstile: Option<String>,
    pub observer: Option<String>,
}

fn observation(operation: PreparationOperation) -> PreparationObservation {
    PreparationObservation {
        operation,
        latency_ms: 0,
        http_status: None,
        cloudflare_challenge: false,
        response: None,
        error: None,
        native_failure: None,
    }
}

fn elapsed(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn token(value: &Value, pointer: &str) -> Result<String, ChatError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 512 * 1024)
        .map(str::to_owned)
        .ok_or(ChatError::ProtocolChanged)
}

fn flag(value: &Value, pointer: &str) -> Result<bool, ChatError> {
    value
        .pointer(pointer)
        .and_then(Value::as_bool)
        .ok_or(ChatError::ProtocolChanged)
}

impl ChatProbe {
    pub async fn inspect_native_preparation(&self, temporary: bool) -> Vec<PreparationObservation> {
        let mut observations = Vec::new();
        let target = PreparationTarget {
            parent: uuid::Uuid::new_v4().to_string(),
            conversation: None,
            model: None,
            temporary,
        };
        let _ = self.prepare_native(target, &mut observations).await;
        observations
    }

    pub(crate) async fn prepare_native(
        &self,
        target: PreparationTarget,
        observations: &mut Vec<PreparationObservation>,
    ) -> Result<PreparedTurn, ChatError> {
        let permit = self.preparation_permit.try_acquire();
        let preflight = match &permit {
            Ok(_) if self.transport != ProbeTransport::BrowserEmulated && !cfg!(test) => {
                Err(ChatError::UnsupportedCapability)
            }
            Ok(_) => self.check_session_limits(true),
            Err(_) => Err(ChatError::PreparationInProgress),
        };
        if let Err(error) = preflight {
            let mut step = observation(PreparationOperation::SessionBootstrap);
            step.error = Some(error);
            observations.push(step);
            return Err(error);
        }
        let generation = self.auth.auth_change_state_receiver().borrow().generation;
        // The bounded interpreter carries substantial state across awaits.
        // Keep this future off the caller's stack, including debug/test builds.
        let result =
            Box::pin(self.native_preparation_inner(target, generation, observations)).await;
        if let Err(error) = &result
            && !observations.iter().any(|s| s.error.is_some())
            && let Some(last) = observations.last_mut()
        {
            last.error = Some(*error);
        }
        if result.is_err() {
            // Only bounded structural enums and timings enter diagnostics.
            // Never log target IDs, tokens, programs, operands, or contents.
            for step in observations.iter() {
                tracing::warn!(
                    target: "codex_chatgpt_chat::preparation",
                    operation = ?step.operation,
                    latency_ms = step.latency_ms,
                    http_status = step.http_status,
                    error = ?step.error,
                    native_failure = ?step.native_failure,
                    "native Chat preparation failed"
                );
            }
        }
        self.finish_preparation(result.as_ref().err().copied());
        result
    }

    async fn native_initialize(
        &self,
        generation: u64,
        observations: &mut Vec<PreparationObservation>,
    ) -> Result<(Arc<sentinel::Profile>, Value), ChatError> {
        let mut step = observation(PreparationOperation::SessionBootstrap);
        let start = Instant::now();
        let result = self
            .guarded_generation(generation, async {
                let response = self
                    .bootstrap_request()
                    .await?
                    .send()
                    .await
                    .map_err(transport_error)?;
                self.observe_response(&response);
                step.http_status = Some(response.status().as_u16());
                step.cloudflare_challenge = response
                    .headers()
                    .get("cf-mitigated")
                    .is_some_and(|v| v == "challenge");
                classify_status(response.status(), response.headers())?;
                let origin = response.url().origin().ascii_serialization();
                let body = bounded_body(response).await?;
                sentinel::Bootstrap::parse(&body, &origin).map_err(|failure| {
                    step.native_failure = Some(failure);
                    ChatError::ProtocolChanged
                })
            })
            .await;
        step.latency_ms = elapsed(start);
        step.error = result.as_ref().err().copied();
        observations.push(step);
        let profile = self.native_profile(result?, generation)?;
        let context = PreparationContext {
            profile: &profile,
            generation,
        };
        let init = self
            .native_json_step(
                PreparationOperation::ConversationInit,
                "conversation/init",
                json!({"conversation_mode_kind":"primary_assistant"}),
                context,
                observations,
            )
            .await?;
        Ok((profile, init))
    }

    /// Uses the same observed bootstrap/init ordering as preparation, without
    /// obtaining requirements/conduit tokens or transmitting a user message.
    pub(crate) async fn native_model_defaults(&self) -> Result<Value, ChatError> {
        self.check_owner()?;
        let generation = self.auth.auth_change_state_receiver().borrow().generation;
        Ok(self.native_initialize(generation, &mut Vec::new()).await?.1)
    }

    async fn native_preparation_inner(
        &self,
        target: PreparationTarget,
        generation: u64,
        observations: &mut Vec<PreparationObservation>,
    ) -> Result<PreparedTurn, ChatError> {
        let (profile, init) = self.native_initialize(generation, observations).await?;
        let context = PreparationContext {
            profile: &profile,
            generation,
        };
        let model = target
            .model
            .clone()
            .unwrap_or(token(&init, "/default_model_slug")?);
        let mut preparation_body = json!({
            "action":"next", "fork_from_shared_post":false,
            "parent_message_id":target.parent, "model":model,
            "timezone_offset_min":0, "timezone":"UTC",
            "history_and_training_disabled":target.temporary,
            "conversation_mode":{"kind":"primary_assistant"},
            "system_hints":[], "supports_buffering":true, "supported_encodings":["v1"]
        });
        if let Some(id) = &target.conversation {
            preparation_body["conversation_id"] = json!(id);
        }
        let conduit = self
            .native_json_step(
                PreparationOperation::ConversationPrepare,
                "f/conversation/prepare",
                preparation_body,
                context,
                observations,
            )
            .await?;
        let conduit = token(&conduit, "/conduit_token")?;
        let (config, p) = profile
            .requirements()
            .map_err(|_| ChatError::ProtocolChanged)?;
        let prepared = self
            .native_json_step(
                PreparationOperation::SentinelPrepare,
                "sentinel/chat-requirements/prepare",
                json!({"p":p}),
                context,
                observations,
            )
            .await?;

        let cancel = CancellationToken::new();
        // The same cancellation lifetime covers all CPU work and the observer
        // state retained across finalization. Dropping this attempt stops it.
        let _cancel_on_drop = cancel.clone().drop_guard();
        let mut step = observation(PreparationOperation::SentinelCompute);
        let start = Instant::now();
        let result = self
            .guarded_generation(generation, async {
                let prepare_token = token(&prepared, "/prepare_token")?;
                let proof_required = flag(&prepared, "/proofofwork/required")?;
                let turn_required = flag(&prepared, "/turnstile/required")?;
                if prepared
                    .pointer("/arkose/required")
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    step.native_failure = Some(SentinelFailure::InteractiveChallenge);
                    return Err(ChatError::UnsupportedChallenge);
                }
                let pow = if proof_required {
                    Some((
                        token(&prepared, "/proofofwork/seed")?,
                        token(&prepared, "/proofofwork/difficulty")?,
                    ))
                } else {
                    None
                };
                let dx = if turn_required {
                    Some(token(&prepared, "/turnstile/dx")?)
                } else {
                    None
                };
                let cancel = cancel.clone();
                let worker_p = p.clone();
                let worker_profile = Arc::clone(&profile);
                let computed = tokio::task::spawn_blocking(move || {
                    let proof = pow
                        .map(|(seed, difficulty)| {
                            sentinel::solve_proof(&seed, &difficulty, config, cancel.clone())
                        })
                        .transpose()?;
                    let turn = dx
                        .map(|dx| sentinel::solve(&dx, &worker_p, &worker_profile, cancel))
                        .transpose()?;
                    Ok::<_, SentinelFailure>((proof, turn))
                })
                .await
                .map_err(|_| ChatError::UnsupportedChallenge)?;
                let (proof, turn) = computed.map_err(|failure| {
                    step.native_failure = Some(failure);
                    ChatError::UnsupportedChallenge
                })?;
                let mut finalize = json!({"prepare_token":prepare_token});
                if let Some(proof) = proof {
                    finalize["proofofwork"] = json!(proof);
                }
                if let Some(turn) = turn {
                    finalize["turnstile"] = json!(turn);
                }
                Ok(finalize)
            })
            .await;
        step.latency_ms = elapsed(start);
        step.error = result.as_ref().err().copied();
        observations.push(step);
        let finalize = result?;
        let prepare_token = token(&finalize, "/prepare_token")?;
        let proof = finalize
            .get("proofofwork")
            .map(|_| token(&finalize, "/proofofwork"))
            .transpose()?;
        let turnstile = finalize
            .get("turnstile")
            .map(|_| token(&finalize, "/turnstile"))
            .transpose()?;
        let observer = if prepared.pointer("/so/required").and_then(Value::as_bool) == Some(true) {
            let mut step = observation(PreparationOperation::SessionObserver);
            let start = Instant::now();
            let result = self
                .guarded_generation(generation, async {
                    let collector = token(&prepared, "/so/collector_dx")?;
                    let snapshot = token(&prepared, "/so/snapshot_dx")?;
                    let profile = Arc::clone(&profile);
                    let cancel = cancel.clone();
                    let observer = tokio::task::spawn_blocking(move || {
                        sentinel::Observer::start(&collector, &p, profile, cancel)
                    })
                    .await
                    .map_err(|_| ChatError::UnsupportedChallenge)?
                    .map_err(|failure| {
                        step.native_failure = Some(failure);
                        ChatError::UnsupportedChallenge
                    })?;
                    Ok((observer, snapshot))
                })
                .await;
            step.latency_ms = elapsed(start);
            step.error = result.as_ref().err().copied();
            observations.push(step);
            Some(result?)
        } else {
            None
        };
        let finalized = self
            .native_json_step(
                PreparationOperation::SentinelFinalize,
                "sentinel/chat-requirements/finalize",
                finalize,
                context,
                observations,
            )
            .await?;
        let requirements = token(&finalized, "/token")?;
        let observer = if let Some((observer, snapshot)) = observer {
            let mut step = observation(PreparationOperation::SessionObserverSnapshot);
            let start = Instant::now();
            let result = self
                .guarded_generation(generation, async {
                    tokio::task::spawn_blocking(move || observer.snapshot(&snapshot))
                        .await
                        .map_err(|_| ChatError::UnsupportedChallenge)?
                        .map_err(|failure| {
                            step.native_failure = Some(failure);
                            ChatError::UnsupportedChallenge
                        })
                })
                .await;
            step.latency_ms = elapsed(start);
            step.error = result.as_ref().err().copied();
            observations.push(step);
            Some(result?)
        } else {
            None
        };
        Ok(PreparedTurn {
            profile,
            generation,
            target,
            model,
            conduit,
            requirements,
            prepare_token,
            proof,
            turnstile,
            observer,
        })
    }

    async fn native_json_step(
        &self,
        operation: PreparationOperation,
        path: &str,
        body: Value,
        context: PreparationContext<'_>,
        observations: &mut Vec<PreparationObservation>,
    ) -> Result<Value, ChatError> {
        let PreparationContext {
            profile,
            generation,
        } = context;
        let mut step = observation(operation);
        let start = Instant::now();
        let result = self
            .guarded_generation(generation, async {
                let request = self
                    .request(Method::POST, path, path)
                    .await?
                    .header("origin", &profile.bootstrap.origin)
                    .header("referer", format!("{}/", profile.bootstrap.origin))
                    .header("oai-device-id", &profile.device)
                    .header("oai-client-version", &profile.bootstrap.build)
                    .header("oai-language", "en-US")
                    .json(&body);
                self.check_profile(profile)?;
                let response = request.send().await.map_err(transport_error)?;
                self.observe_response(&response);
                step.http_status = Some(response.status().as_u16());
                step.cloudflare_challenge = response
                    .headers()
                    .get("cf-mitigated")
                    .is_some_and(|v| v == "challenge");
                classify_response(response.status(), response.headers())?;
                let body = bounded_body(response).await?;
                let value: Value =
                    serde_json::from_slice(&body).map_err(|_| ChatError::ProtocolChanged)?;
                let shape = PreparationShape::from_value(&value)?;
                let valid = match operation {
                    PreparationOperation::ConversationInit => shape.default_model_present,
                    PreparationOperation::SentinelPrepare => {
                        shape.prepare_token_present
                            && shape.proof_of_work_required.is_some()
                            && shape.turnstile_required.is_some()
                            && shape.observer_required.is_some()
                    }
                    PreparationOperation::SentinelFinalize => shape.requirements_token_present,
                    PreparationOperation::ConversationPrepare => shape.conduit_token_present,
                    _ => false,
                };
                step.response = Some(shape);
                if !valid {
                    return Err(ChatError::ProtocolChanged);
                }
                Ok(value)
            })
            .await;
        step.latency_ms = elapsed(start);
        step.error = result.as_ref().err().copied();
        observations.push(step);
        result
    }
}
