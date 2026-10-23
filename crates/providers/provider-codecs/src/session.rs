use crate::SseDecoder;
use crate::protocol;
use crate::replay;
use codex_provider_api::*;
use reqwest::Client;
use reqwest::Response;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::time::Duration;
use tokio::time::Instant;

pub struct WireRequest {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Value,
}

/// Each codec owns exactly one upstream HTTP round, including signed replay parts.
pub trait WireCodec: Send {
    fn request(&self, request: &Request, context: &Context) -> Result<WireRequest>;
    fn reset(&mut self);
    fn frame(&mut self, event: &str, data: &str) -> Result<Vec<Event>>;
    fn finished(&self) -> bool;
    /// Some upstreams send usage after the generation's terminal content frame.
    fn terminal_grace(&self) -> Duration {
        Duration::ZERO
    }
    fn assistant(&self, namespace: &Namespace) -> Result<Message>;
    fn response_id(&self) -> String;
}

pub struct HttpSession {
    client: Client,
    codec: Box<dyn WireCodec>,
    request: Request,
    context: Context,
    response: Option<Response>,
    decoder: SseDecoder,
    events: VecDeque<Event>,
    pending: BTreeMap<String, Option<ToolResult>>,
    completed: bool,
    pending_order: Vec<String>,
    round_bytes: usize,
    terminal_deadline: Option<Instant>,
    classifier: FailureClassifier,
    continuation_ready: bool,
}
impl HttpSession {
    pub async fn start(
        client: Client,
        codec: Box<dyn WireCodec>,
        request: Request,
        context: Context,
    ) -> Result<Self> {
        Self::start_with_policy(client, codec, request, context, |response| {
            response.unknown()
        })
        .await
    }
    pub async fn start_with_policy(
        client: Client,
        codec: Box<dyn WireCodec>,
        mut request: Request,
        context: Context,
        classifier: FailureClassifier,
    ) -> Result<Self> {
        if context.credentials.account_id != request.namespace.account {
            return Err(ProviderError::Authentication(
                "request account does not match credentials".into(),
            ));
        }
        if context.credentials.access_token.is_empty() {
            return Err(ProviderError::Authentication("missing access token".into()));
        }
        replay::restore_checkpoint(&mut request)?;
        let mut session = Self {
            client,
            codec,
            request,
            context,
            response: None,
            decoder: SseDecoder::default(),
            events: VecDeque::new(),
            pending: BTreeMap::new(),
            completed: false,
            pending_order: Vec::new(),
            round_bytes: 0,
            terminal_deadline: None,
            classifier,
            continuation_ready: false,
        };
        session.send().await?;
        Ok(session)
    }
    async fn send(&mut self) -> Result<()> {
        if self.context.cancellation.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let wire = self.codec.request(&self.request, &self.context)?;
        let mut builder = self
            .client
            .post(wire.url)
            .json(&wire.body)
            .header("accept", "text/event-stream");
        for (key, value) in wire.headers {
            builder = builder.header(key, value);
        }
        let response = tokio::select! {
            biased;
            _ = self.context.cancellation.cancelled() => return Err(ProviderError::Cancelled),
            response = tokio::time::timeout(Duration::from_secs(60), builder.send()) => response.map_err(|_| ProviderError::Transport("provider request timed out".into()))?.map_err(|_| ProviderError::Transport("provider request failed".into()))?,
        };
        if !response.status().is_success() {
            return Err(classify_response(
                response,
                &self.context,
                &self.request.namespace.model,
                self.classifier,
            )
            .await?);
        }
        self.codec.reset();
        self.round_bytes = 0;
        self.terminal_deadline = None;
        self.decoder = SseDecoder::default();
        self.response = Some(response);
        Ok(())
    }
    fn finish_round(&mut self) -> Result<()> {
        // A terminal event can share an HTTP chunk with a truncated trailing
        // frame. Validate retained SSE bytes before checkpointing or dispatch.
        self.decoder.finish()?;
        let assistant = self.codec.assistant(&self.request.namespace)?;
        let calls: Vec<ToolCall> = assistant
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect();
        let checkpoint = replay::checkpoint(&self.request, &assistant)?;
        self.events.push_back(Event::Checkpoint(checkpoint));
        self.request.messages.push(assistant);
        self.response = None;
        for call in calls {
            if self.pending.insert(call.id.clone(), None).is_some() {
                return Err(protocol("duplicate upstream tool call id"));
            }
            self.pending_order.push(call.id.clone());
            self.events.push_back(Event::ToolCall(call));
        }
        if self.pending.is_empty() {
            self.completed = true;
            self.events.push_back(Event::Completed {
                id: self.codec.response_id(),
            });
        }
        Ok(())
    }
    async fn next(&mut self) -> Result<Option<Event>> {
        loop {
            if self.context.cancellation.is_cancelled() {
                self.response = None;
                return Err(ProviderError::Cancelled);
            }
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
            if self.completed {
                return Ok(None);
            }
            let Some(response) = self.response.as_mut() else {
                if self.pending.is_empty() {
                    return Err(protocol("provider has no response or outstanding tools"));
                }
                // The driver polls this future alongside asynchronous host work.
                // It drops this borrow when a result is ready, then submits it.
                self.context.cancellation.cancelled().await;
                return Err(ProviderError::Cancelled);
            };
            let terminal_deadline = self.terminal_deadline;
            let chunk = tokio::select! {
                biased;
                _ = self.context.cancellation.cancelled() => { self.response = None; return Err(ProviderError::Cancelled); },
                _ = async {
                    if let Some(deadline) = terminal_deadline {
                        tokio::time::sleep_until(deadline).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    self.decoder.finish()?;
                    self.finish_round()?;
                    continue;
                },
                chunk = tokio::time::timeout(Duration::from_secs(90), response.chunk()) => chunk.map_err(|_| ProviderError::Transport("provider stream idle timeout".into()))?.map_err(|_| ProviderError::Transport("provider stream failed".into()))?,
            };
            match chunk {
                Some(chunk) => {
                    self.round_bytes = self.round_bytes.saturating_add(chunk.len());
                    if self.round_bytes > 64 * 1024 * 1024 {
                        return Err(protocol("provider stream exceeds 64 MiB"));
                    }
                    for (event, data) in self.decoder.push(&chunk)? {
                        let body = serde_json::from_str::<Value>(&data).ok();
                        if event == "error"
                            || body.as_ref().is_some_and(|value| {
                                value.get("error").is_some() || value["type"] == "response.failed"
                            })
                        {
                            return Err(ProviderError::Classified((self.classifier)(
                                FailureResponse {
                                    status: 200,
                                    body: data.as_bytes(),
                                    retry_after_seconds: None,
                                    model: &self.request.namespace.model,
                                    now: unix_time(),
                                    outcome: RequestOutcome::PotentiallyProcessed,
                                },
                            )));
                        }
                        self.events.extend(self.codec.frame(&event, &data)?);
                    }
                    if self.codec.finished() {
                        if self.codec.terminal_grace().is_zero() {
                            self.finish_round()?;
                        } else if self.terminal_deadline.is_none() {
                            self.terminal_deadline =
                                Some(Instant::now() + self.codec.terminal_grace());
                        }
                    }
                }
                None => {
                    self.decoder.finish()?;
                    if self.codec.finished() {
                        self.finish_round()?;
                    } else {
                        return Err(protocol("provider stream ended without a terminal event"));
                    }
                }
            }
        }
    }
}
impl ProviderSession for HttpSession {
    fn retry_authentication(&mut self, credentials: Credentials) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            if self.response.is_some()
                || !self.pending.is_empty()
                || self.completed
                || !self.events.is_empty()
                || credentials.account_id != self.request.namespace.account
            {
                return Err(protocol("no rejected authentication boundary"));
            }
            self.context.credentials = credentials;
            self.send().await
        })
    }
    fn continuation_request(&self) -> Result<Request> {
        if self.response.is_some() || !self.pending.is_empty() || self.completed {
            return Err(protocol("no completed HTTP continuation boundary"));
        }
        let mut request = self.request.clone();
        request.checkpoint = None;
        if let Some(metadata) = request.metadata.as_object_mut() {
            metadata.remove("host_checkpoint_prefix");
        }
        Ok(request)
    }
    fn next_event(&mut self) -> ProviderFuture<'_, Option<Event>> {
        Box::pin(self.next())
    }
    fn submit_tool_result(&mut self, result: ToolResult) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            if self.context.cancellation.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            let entry = self
                .pending
                .get_mut(&result.call_id)
                .ok_or_else(|| protocol("unknown tool result id"))?;
            if entry.is_some() {
                return Err(protocol("duplicate tool result"));
            }
            *entry = Some(result);
            if self.pending.values().all(Option::is_some) {
                let mut pending = std::mem::take(&mut self.pending);
                let results = std::mem::take(&mut self.pending_order)
                    .into_iter()
                    .filter_map(|id| pending.remove(&id).flatten())
                    .map(Content::ToolResult)
                    .collect();
                self.request.messages.push(Message {
                    role: Role::Tool,
                    content: results,
                });
                self.continuation_ready = true;
                self.events.push_back(Event::ContinuationReady);
            }
            Ok(())
        })
    }
    fn continue_round(&mut self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            if !self.continuation_ready
                || !self.pending.is_empty()
                || self.response.is_some()
                || !self.events.is_empty()
            {
                return Err(protocol("no acknowledged continuation boundary"));
            }
            self.continuation_ready = false;
            self.send().await
        })
    }
    fn cancel(&mut self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            self.context.cancellation.cancel();
            self.response = None;
            self.events.clear();
            Ok(())
        })
    }
}

pub async fn bounded_json(mut response: Response, context: &Context) -> Result<Value> {
    if !response.status().is_success() {
        return Err(ProviderError::Transport(format!(
            "HTTP {}",
            response.status().as_u16()
        )));
    }
    let operation = async {
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ProviderError::Transport("provider discovery failed".into()))?
        {
            if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
                return Err(protocol("provider JSON exceeds 8 MiB"));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| protocol("invalid provider JSON"))
    };
    tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => Err(ProviderError::Cancelled),
        result = tokio::time::timeout(Duration::from_secs(30), operation) => result.map_err(|_| ProviderError::Transport("provider discovery timeout".into()))?,
    }
}

fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
/// Read only bounded error evidence. Bodies are discarded after provider classification.
pub async fn classify_response(
    mut response: Response,
    context: &Context,
    model: &str,
    classifier: FailureClassifier,
) -> Result<ProviderError> {
    let status = response.status().as_u16();
    let retry_after_seconds = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| codex_provider_api::retry_after_seconds(value, unix_time()));
    let operation = async {
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| protocol("unable to read provider rejection"))?
        {
            if bytes.len() + chunk.len() > 64 * 1024 {
                return Ok(Vec::new());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok::<_, ProviderError>(bytes)
    };
    let bytes = tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => return Err(ProviderError::Cancelled),
        result = tokio::time::timeout(Duration::from_secs(30), operation) => result.map_err(|_| protocol("provider rejection read timed out"))??,
    };
    Ok(ProviderError::Classified(classifier(FailureResponse {
        status,
        body: &bytes,
        retry_after_seconds,
        model,
        now: unix_time(),
        outcome: RequestOutcome::Rejected,
    })))
}
