//! Durable execution fences, journaling, and exact checkpoint history binding.
use crate::ExecutionLease;
use crate::SessionStore;
use crate::ToolDisposition;
use codex_provider_api as api;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

fn error(message: impl std::fmt::Display) -> api::ProviderError {
    api::ProviderError::Checkpoint(message.to_string())
}

pub struct HistoryRequest {
    pub reply: oneshot::Sender<api::Result<Option<api::HistoryAcknowledgement>>>,
}

pub struct HostCall {
    pub call: api::ToolCall,
    pub reply: oneshot::Sender<api::HostCompletion>,
}
#[derive(Clone)]
pub struct TurnCheckpoint {
    pub namespace: api::Namespace,
    pub latest: Option<api::Checkpoint>,
    pub previous: Option<api::Checkpoint>,
    pub tools_started: bool,
    pub prior_journals: Vec<api::Namespace>,
}
impl TurnCheckpoint {
    fn pending(&self, mut checkpoint: api::Checkpoint) -> api::Checkpoint {
        checkpoint.state["codex_host_pending"] = json!(true);
        checkpoint.state["codex_host_tools_started"] = json!(self.tools_started);
        checkpoint.state["codex_host_previous"] = json!(self.previous);
        checkpoint
    }
}
impl TurnCheckpoint {
    pub async fn complete(
        &self,
        store: &SessionStore,
        execution: &ExecutionLease,
        system: &str,
        history: &[api::Message],
        cancellation: &CancellationToken,
    ) -> api::Result<()> {
        if let Some(mut checkpoint) = self.latest.clone() {
            checkpoint.state["codex_host_pending"] = json!(false);
            if let Some(state) = checkpoint.state.as_object_mut() {
                state.remove("codex_host_previous");
            }
            checkpoint.state["codex_host_lineage"] = json!({
                "length":history.len(), "hash":lineage_hash(system, history)?,
                "failed_results":failed_result_bindings(history)?
            });
            store
                .for_namespace(self.namespace.clone())
                .save(&checkpoint, cancellation)
                .await?;
        }
        execution.complete()
    }
}

pub struct ChannelHost {
    pub store: SessionStore,
    pub calls: mpsc::Sender<HostCall>,
    pub history_requests: Option<mpsc::Sender<HistoryRequest>>,
    pub cancellation: CancellationToken,
    pub poll_tool: Option<String>,
    pub native_tools: std::collections::BTreeSet<String>,
    pub operations: Option<Arc<dyn api::NativeOperations>>,
    pub checkpoint: Arc<tokio::sync::Mutex<TurnCheckpoint>>,
    pub execution: Arc<ExecutionLease>,
}
impl ChannelHost {
    async fn dispatch(
        &self,
        call: api::ToolCall,
        cancellation: &CancellationToken,
    ) -> api::Result<api::HostCompletion> {
        let (reply, receive) = oneshot::channel();
        tokio::select! {
            _ = cancellation.cancelled() => return Err(api::ProviderError::Cancelled),
            sent = self.calls.send(HostCall { call, reply }) => sent.map_err(|_| api::ProviderError::Cancelled)?,
        }
        tokio::select! {
            _ = cancellation.cancelled() => Err(api::ProviderError::Cancelled),
            result = receive => result.map_err(|_| api::ProviderError::Cancelled),
        }
    }
}
impl api::Host for ChannelHost {
    fn persisted_history(&self) -> api::ProviderFuture<'_, Option<api::HistoryAcknowledgement>> {
        Box::pin(async move {
            let Some(requests) = &self.history_requests else {
                return Ok(None);
            };
            let (reply, receive) = oneshot::channel();
            tokio::select! {
                _ = self.cancellation.cancelled() => Err(api::ProviderError::Cancelled),
                result = async {
                    requests.send(HistoryRequest { reply }).await.map_err(|_| api::ProviderError::Cancelled)?;
                    receive.await.map_err(|_| api::ProviderError::Cancelled)?
                } => result,
            }
        })
    }
    fn account_changed(
        &self,
        namespace: api::Namespace,
        prior_journals: Vec<api::Namespace>,
    ) -> api::ProviderFuture<'_, ()> {
        Box::pin(async move {
            let mut state = self.checkpoint.lock().await;
            state.namespace = namespace;
            state.prior_journals = prior_journals;
            state.latest = None;
            state.previous = None;
            // The execution fence remains set until terminal host persistence.
            Ok(())
        })
    }
    // The lock serializes durable interruption fences with checkpoint writes.
    #[allow(clippy::await_holding_invalid_type)]
    fn execute(
        &self,
        call: api::ToolCall,
        cancellation: CancellationToken,
    ) -> api::ProviderFuture<'_, api::ToolResult> {
        Box::pin(async move {
            // Fence interruption before any operation can run, even if the
            // provider has not emitted its first checkpoint yet. Serialize this
            // with checkpoint writes so a later snapshot cannot clear the fence.
            {
                let mut state = self.checkpoint.lock().await;
                if !state.tools_started {
                    self.execution.mark_tools_started()?;
                    state.tools_started = true;
                    let checkpoint =
                        state.pending(state.latest.clone().unwrap_or_else(|| api::Checkpoint {
                            version: 1,
                            namespace: state.namespace.clone(),
                            context_hash: String::new(),
                            state: json!({}),
                        }));
                    self.store
                        .for_namespace(state.namespace.clone())
                        .save(&checkpoint, &cancellation)
                        .await?;
                    state.latest = Some(checkpoint);
                }
            }
            let store = {
                let state = self.checkpoint.lock().await;
                self.store
                    .for_namespace(state.namespace.clone())
                    .with_prior_journals(state.prior_journals.clone())
            };
            let ticket = match store.begin_tool(call.clone(), &cancellation).await? {
                ToolDisposition::Replay(result) => return Ok(result),
                ToolDisposition::Execute(ticket) => ticket,
            };
            let completion = self.dispatch(call.clone(), &cancellation).await?;
            let mut result = completion.to_tool_result();
            if self.native_tools.contains(&call.name) {
                if completion.disposition == api::CompletionDisposition::Denied {
                    result.output = completion
                        .structured
                        .clone()
                        .unwrap_or_else(|| json!({"rejected":true}));
                }
                if !result.is_error {
                    let mut structured = completion.structured.clone().ok_or_else(|| {
                        api::ProviderError::Host(
                            "missing structured native operation output".into(),
                        )
                    })?;
                    let mut output = structured["output"].as_str().unwrap_or_default().to_owned();
                    let mut output_exceeded = output.len() > 2 * 1024 * 1024;
                    if output_exceeded {
                        output.clear();
                    }
                    let mut polls = 0;
                    while let Some(session_id) = structured["session_id"].as_i64() {
                        polls += 1;
                        let poll_tool = self.poll_tool.clone().ok_or_else(|| {
                            api::ProviderError::Host(
                                "native shell requires write_stdin continuation".into(),
                            )
                        })?;
                        let polled = self.dispatch(api::ToolCall { id: format!("{}:poll:{polls}", call.id), name: poll_tool, arguments: json!({"session_id":session_id,"chars":"","yield_time_ms":1000,"max_output_tokens":30000}) }, &cancellation).await?;
                        if polled.is_error() {
                            result.is_error = true;
                            result.output = polled.output;
                            break;
                        }
                        structured = polled.structured.ok_or_else(|| {
                            api::ProviderError::Host(
                                "missing structured native continuation output".into(),
                            )
                        })?;
                        if !output_exceeded {
                            let chunk = structured["output"].as_str().unwrap_or_default();
                            if chunk.len() > 2 * 1024 * 1024 - output.len() {
                                output_exceeded = true;
                                output.clear();
                            } else {
                                output.push_str(chunk);
                            }
                        }
                        // Drain a foreground operation through its exit (or
                        // normal cancellation/timeout) even after the cap. An
                        // early error would leave its child running unnoticed.
                    }
                    if !result.is_error && output_exceeded {
                        result.is_error = true;
                        result.output = json!("native operation output exceeded 2 MiB");
                    } else if !result.is_error {
                        structured["output"] = json!(output);
                        match self
                            .operations
                            .as_ref()
                            .ok_or_else(|| {
                                api::ProviderError::Host("missing native operation mapping".into())
                            })?
                            .result(&call, &structured)
                        {
                            Ok(output) => result.output = output,
                            Err(error) => {
                                result.is_error = true;
                                result.output = json!({"error":error.to_string()});
                            }
                        }
                    }
                }
            }
            ticket.finish(result)
        })
    }
    #[allow(clippy::await_holding_invalid_type)]
    fn checkpoint(&self, checkpoint: api::Checkpoint) -> api::ProviderFuture<'_, ()> {
        Box::pin(async move {
            if !checkpoint.state.is_object() {
                return Err(api::ProviderError::Checkpoint(
                    "native checkpoint must be an object".into(),
                ));
            }
            let mut state = self.checkpoint.lock().await;
            let checkpoint = state.pending(checkpoint);
            self.store
                .for_namespace(state.namespace.clone())
                .save(&checkpoint, &self.cancellation)
                .await?;
            state.latest = Some(checkpoint);
            Ok(())
        })
    }
}

// Keep the original sorted JSON representation independent of serde_json's
// preserve_order feature, which other workspace targets can enable globally.
fn hash_value(mut value: serde_json::Value) -> api::Result<String> {
    value.sort_all_objects();
    Ok(codex_provider_auth::digest(
        &serde_json::to_vec(&value)
            .map_err(|error| api::ProviderError::Checkpoint(error.to_string()))?,
    ))
}

pub fn lineage_hash(system: &str, history: &[api::Message]) -> api::Result<String> {
    hash_value(json!({"system":system,"messages":history}))
}

pub fn failed_result_bindings(history: &[api::Message]) -> api::Result<BTreeMap<String, String>> {
    history
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            api::Content::ToolResult(result) if result.is_error => Some(result),
            _ => None,
        })
        .map(|result| Ok((result.call_id.clone(), hash_value(result.output.clone())?)))
        .collect()
}

pub fn validate_lineage(
    request: &mut api::Request,
    policy: api::RecoveryPolicy,
) -> api::Result<()> {
    if request.checkpoint.as_ref().is_some_and(|checkpoint| {
        checkpoint.version != 1 || checkpoint.namespace != request.namespace
    }) {
        return Err(error("native checkpoint identity or version mismatch"));
    }
    if let Some(checkpoint) = request.checkpoint.as_ref()
        && checkpoint.state["codex_host_pending"] == true
    {
        if checkpoint.state["codex_host_tools_started"] != false {
            return Err(error(
                "native generation was interrupted after tool execution started; inspect its effects and explicitly branch the thread before continuing",
            ));
        }
        // No host operation started. Retain the last completed signed transcript
        // and append the ordinary persisted interruption/new-user history.
        let previous = checkpoint
            .state
            .get("codex_host_previous")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        request.checkpoint = if policy == api::RecoveryPolicy::RebuildFromHistory {
            // The provider rebuilds interrupted text turns from verified public
            // history instead of restoring a transcript missing partial output.
            None
        } else {
            serde_json::from_value(previous)
                .map_err(|error| api::ProviderError::Checkpoint(error.to_string()))?
        };
    }
    let Some(checkpoint) = &request.checkpoint else {
        return Ok(());
    };
    if checkpoint.state["codex_host_pending"] != false {
        return Err(error(
            "native checkpoint has an incomplete or unverified host history; refusing automatic replay",
        ));
    }
    let lineage = &checkpoint.state["codex_host_lineage"];
    let length = lineage["length"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| error("missing native checkpoint history binding"))?;
    // ResponseItem's failure flag is not serialized in ordinary rollout history.
    // Restore it only for the same call ID and exact saved output bytes.
    if let Some(failed) = lineage["failed_results"].as_object() {
        for message in request.messages.iter_mut().take(length) {
            for content in &mut message.content {
                if let api::Content::ToolResult(result) = content
                    && let Some(hash) = failed
                        .get(&result.call_id)
                        .and_then(serde_json::Value::as_str)
                    && hash == hash_value(result.output.clone())?
                {
                    result.is_error = true;
                }
            }
        }
    }
    if length > request.messages.len()
        || lineage["hash"].as_str()
            != Some(&lineage_hash(&request.system, &request.messages[..length])?)
    {
        return Err(error(
            "native checkpoint history changed; refusing to resume stale upstream state",
        ));
    }
    request.metadata["host_checkpoint_prefix"] = json!(length);
    Ok(())
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
