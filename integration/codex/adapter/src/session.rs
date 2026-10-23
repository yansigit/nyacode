//! Translate host conversation items to provider-neutral sessions and events.
use crate::backend::NativeBackend;
use crate::host_operations;
use codex_api::ResponseEvent;
use codex_protocol::ResponseItemId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::plaintext_agent_message_content;
use codex_protocol::protocol::TokenUsage;
use codex_provider_api as api;
use codex_provider_runtime::ChannelHost;
use codex_provider_runtime::HostCall;
use codex_provider_runtime::TurnCheckpoint;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub struct Prompt {
    pub input: Vec<ResponseItem>,
    pub tools: Vec<Value>,
    pub parallel_tool_calls: bool,
    pub system: String,
    pub output_schema: Option<Value>,
    pub persistence_acknowledgements: bool,
}

pub struct SessionStream {
    pub results: mpsc::Sender<api::HostCompletion>,
    pub events: mpsc::Receiver<Result<ResponseEvent>>,
    pub cancellation: CancellationToken,
}

impl SessionStream {
    pub fn into_parts(
        mut self,
    ) -> (
        mpsc::Sender<api::HostCompletion>,
        mpsc::Receiver<Result<ResponseEvent>>,
        CancellationToken,
    ) {
        let (_, empty) = mpsc::channel(1);
        let events = std::mem::replace(&mut self.events, empty);
        let cancellation = std::mem::replace(&mut self.cancellation, CancellationToken::new());
        (self.results.clone(), events, cancellation)
    }
}

impl Drop for SessionStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

fn error(error: impl std::fmt::Display) -> CodexErr {
    CodexErr::Stream(error.to_string())
}

#[derive(Clone)]
struct ToolTarget {
    name: String,
    namespace: Option<String>,
    custom: bool,
    native: bool,
}

fn tool_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(namespace) => format!("{namespace}__{name}"),
        None => name.to_owned(),
    }
}

/// Native agent communication is plaintext over the provider's authenticated
/// transport. Do not request Responses-only encrypted arguments that a child on
/// another transport cannot decode. Preserve properties named `encrypted` and
/// literal enum/default data; only visit schema nodes.
fn plaintext_tool_schema(schema: &Value) -> Value {
    let mut schema = schema.clone();
    if let Some(object) = schema.as_object_mut() {
        object.remove("encrypted");
        for key in ["properties", "$defs", "definitions", "patternProperties"] {
            if let Some(children) = object.get_mut(key).and_then(Value::as_object_mut) {
                for child in children.values_mut() {
                    *child = plaintext_tool_schema(child);
                }
            }
        }
        for key in [
            "items",
            "additionalProperties",
            "not",
            "contains",
            "if",
            "then",
            "else",
            "propertyNames",
        ] {
            if let Some(child) = object.get_mut(key) {
                *child = plaintext_tool_schema(child);
            }
        }
        for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
            if let Some(children) = object.get_mut(key).and_then(Value::as_array_mut) {
                for child in children {
                    *child = plaintext_tool_schema(child);
                }
            }
        }
    }
    schema
}

fn tools(prompt: &Prompt) -> Result<(Vec<api::ToolDefinition>, BTreeMap<String, ToolTarget>)> {
    let mut definitions = vec![];
    let mut targets = BTreeMap::new();
    for spec in prompt.tools.iter() {
        let spec = spec.clone();
        let (namespace, entries) = if spec["type"] == "namespace" {
            (
                spec["name"].as_str(),
                spec["tools"].as_array().cloned().unwrap_or_default(),
            )
        } else {
            (None, vec![spec.clone()])
        };
        for entry in entries {
            let kind = entry["type"].as_str().unwrap_or("function");
            if kind != "function" && kind != "custom" {
                continue;
            }
            let Some(name) = entry["name"].as_str() else {
                return Err(error("native tool is missing a name"));
            };
            let mapped = tool_name(namespace, name);
            if targets
                .insert(
                    mapped.clone(),
                    ToolTarget {
                        name: name.into(),
                        namespace: namespace.map(str::to_owned),
                        custom: kind == "custom",
                        native: false,
                    },
                )
                .is_some()
            {
                return Err(error("native tool name collision"));
            }
            definitions.push(api::ToolDefinition {
                name: mapped,
                description: entry["description"].as_str().unwrap_or_default().into(),
                parameters: if kind == "custom" { json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"],"additionalProperties":false}) } else { plaintext_tool_schema(&entry["parameters"]) },
            });
        }
    }
    Ok((definitions, targets))
}

fn push(messages: &mut Vec<api::Message>, role: api::Role, content: Vec<api::Content>) {
    if content.is_empty() {
        return;
    }
    if let Some(previous) = messages.last_mut()
        && previous.role == role
    {
        previous.content.extend(content);
    } else {
        messages.push(api::Message { role, content });
    }
}

fn messages(input: &[ResponseItem]) -> Result<Vec<api::Message>> {
    let mut messages = vec![];
    for item in input {
        if let ResponseItem::AgentMessage {
            author,
            recipient,
            content,
            ..
        } = item
        {
            let text = plaintext_agent_message_content(content).ok_or_else(|| {
                error("native provider requires a nonempty plaintext agent message; encrypted agent messages cannot cross this transport")
            })?;
            // Agent input is conversational input, never a system instruction. Keep
            // attribution and all plaintext, including completion and follow-up messages.
            push(
                &mut messages,
                api::Role::User,
                vec![api::Content::Text {
                    text: json!({"type":"agent_message","author":author,"recipient":recipient,"text":text}).to_string(),
                }],
            );
            continue;
        }
        let failed = match item {
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => output.success == Some(false),
            _ => false,
        };
        let item = serde_json::to_value(item).map_err(error)?;
        match item["type"].as_str().unwrap_or_default() {
            "message" => {
                let role = match item["role"].as_str() {
                    Some("assistant") => api::Role::Assistant,
                    Some("system" | "developer") => api::Role::System,
                    Some("user") => api::Role::User,
                    _ => return Err(error("unsupported native message role")),
                };
                let mut content = vec![];
                for part in item["content"].as_array().into_iter().flatten() {
                    match part["type"].as_str() {
                        Some("input_text" | "output_text") => content.push(api::Content::Text {
                            text: part["text"].as_str().unwrap_or_default().into(),
                        }),
                        Some("input_image") => content.push(api::Content::Image {
                            url: part["image_url"].as_str().unwrap_or_default().into(),
                        }),
                        _ => {
                            return Err(error(
                                "native provider does not support this content type",
                            ));
                        }
                    }
                }
                push(&mut messages, role, content);
            }
            "function_call" | "custom_tool_call" => {
                let arguments = if item["type"] == "custom_tool_call" {
                    json!({"input":item["input"]})
                } else {
                    serde_json::from_str(item["arguments"].as_str().unwrap_or("{}"))
                        .map_err(error)?
                };
                push(
                    &mut messages,
                    api::Role::Assistant,
                    vec![api::Content::ToolCall(api::ToolCall {
                        id: item["call_id"].as_str().unwrap_or_default().into(),
                        name: tool_name(
                            item["namespace"].as_str(),
                            item["name"].as_str().unwrap_or_default(),
                        ),
                        arguments,
                    })],
                );
            }
            "function_call_output" | "custom_tool_call_output" => {
                push(
                    &mut messages,
                    api::Role::Tool,
                    vec![api::Content::ToolResult(api::ToolResult {
                        call_id: item["call_id"].as_str().unwrap_or_default().into(),
                        output: item["output"].clone(),
                        is_error: failed,
                    })],
                );
            }
            "reasoning" => {
                let text = item["summary"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<String>();
                if !text.is_empty() {
                    push(
                        &mut messages,
                        api::Role::Assistant,
                        vec![api::Content::Thinking {
                            text,
                            signature: None,
                        }],
                    );
                }
            }
            // Server-specific encrypted and bookkeeping items cannot cross provider boundaries.
            "compaction"
            | "context_compaction"
            | "configuration_update"
            | "compaction_trigger"
            | "additional_tools" => {}
            other => {
                return Err(error(format!(
                    "native history contains unsupported item: {other}"
                )));
            }
        }
    }
    Ok(messages)
}

fn call_item(
    call: api::ToolCall,
    targets: &BTreeMap<String, ToolTarget>,
    operations: Option<&dyn api::NativeOperations>,
) -> Result<ResponseItem> {
    let target = targets
        .get(&call.name)
        .ok_or_else(|| error("unadvertised native tool"))?;
    let arguments = if target.native {
        let operation = operations
            .ok_or_else(|| error("missing native operation mapping"))?
            .operation(&call)
            .map_err(error)?;
        host_operations::command(&operation).map_err(error)?
    } else {
        call.arguments
    };
    let value = if target.custom {
        json!({"type":"custom_tool_call","call_id":call.id,"name":target.name,"namespace":target.namespace,"input":arguments["input"].as_str().ok_or_else(|| error("custom tool requires input string"))?})
    } else {
        // Native transports return plaintext arguments. An explicit empty list
        // distinguishes that from Responses' absent/unknown encryption metadata.
        json!({"type":"function_call","call_id":call.id,"name":target.name,"namespace":target.namespace,"arguments":serde_json::to_string(&arguments).map_err(error)?,"encrypted_function_args":[]})
    };
    serde_json::from_value(value).map_err(error)
}

/// Called after the ordinary tool runtime has executed and persisted its output.
pub fn tool_result(item: &ResponseItem) -> Result<api::HostCompletion> {
    match item {
        ResponseItem::FunctionCallOutput {
            call_id: Some(id),
            output,
            ..
        }
        | ResponseItem::CustomToolCallOutput {
            call_id: id,
            output,
            ..
        } => Ok(api::HostCompletion {
            call_id: id.clone(),
            output: serde_json::to_value(&output.body).map_err(error)?,
            structured: None,
            disposition: if output.success == Some(false) {
                api::CompletionDisposition::Failure
            } else {
                api::CompletionDisposition::Success
            },
        }),
        _ => Err(error("native tool did not produce a matching result")),
    }
}

fn message(id: &str, text: String, thinking: bool) -> ResponseItem {
    if thinking {
        ResponseItem::Reasoning {
            id: Some(ResponseItemId::from_server(id.to_owned())),
            summary: vec![
                codex_protocol::models::ReasoningItemReasoningSummary::SummaryText { text },
            ],
            content: None,
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        }
    } else {
        ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: Some(ResponseItemId::from_server(id.to_owned())),
            role: "assistant".into(),
            content: vec![ContentItem::OutputText {
                text,
                annotations: None,
                logprobs: None,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }
}

async fn emit(
    tx: &mpsc::Sender<Result<ResponseEvent>>,
    event: ResponseEvent,
    cancellation: &CancellationToken,
) -> Result<()> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(CodexErr::TurnAborted),
        sent = tx.send(Ok(event)) => sent.map_err(|_| CodexErr::TurnAborted),
    }
}

async fn finish_item(
    tx: &mpsc::Sender<Result<ResponseEvent>>,
    active: &mut Option<(String, String, bool)>,
    history: &mut Vec<api::Message>,
    cancellation: &CancellationToken,
) -> Result<()> {
    if let Some((id, text, thinking)) = active.take() {
        let item = message(&id, text, thinking);
        for entry in messages(std::slice::from_ref(&item))? {
            push(history, entry.role, entry.content);
        }
        emit(tx, ResponseEvent::OutputItemDone(item), cancellation).await?;
    }
    Ok(())
}

async fn acknowledge_history(
    tx: &mpsc::Sender<Result<ResponseEvent>>,
    expected: &[api::Message],
    system: &str,
    scope: &str,
    cancellation: &CancellationToken,
) -> Result<Option<api::HistoryAcknowledgement>> {
    let (reply, receive) = oneshot::channel();
    emit(
        tx,
        ResponseEvent::HistoryPersistenceRequested { reply },
        cancellation,
    )
    .await?;
    let recorded = tokio::select! {
        _ = cancellation.cancelled() => return Err(CodexErr::TurnAborted),
        result = receive => result.map_err(error)?.map_err(error)?,
    };
    let Some(recorded) = recorded else {
        return Ok(None);
    };
    let recorded = messages(&recorded)?;
    if !recorded.ends_with(expected) {
        return Err(error(
            "recorded provider history differs from the acknowledged conversation; refusing account handoff",
        ));
    }
    Ok(Some(api::HistoryAcknowledgement {
        hash: codex_provider_runtime::lineage_hash(system, expected).map_err(error)?,
        length: expected.len(),
        checkpoint_scope: scope.into(),
    }))
}

#[allow(clippy::too_many_arguments)]
pub async fn stream(
    provider: Arc<NativeBackend>,
    prompt: &Prompt,
    model: &str,
    thread: String,
    reasoning_effort: Option<String>,
    checkpoint_scope: String,
    execution_purpose: &str,
) -> Result<SessionStream> {
    let cancellation = CancellationToken::new();
    let namespace = api::Namespace {
        provider: provider.id().into(),
        account: String::new(),
        model: model.into(),
        thread,
    };
    let home = provider
        .state_home()
        .ok_or_else(|| error("native provider requires local state storage"))?;
    let backend = provider.provider().map_err(error)?;
    let operations = backend.native_operations();
    let (mut definitions, mut targets) = tools(prompt)?;
    let poll_tool = targets
        .iter()
        .find(|(_, target)| target.name == "write_stdin")
        .map(|(name, _)| name.clone());
    if host_operations::available()
        && let Some(operations) = &operations
        && poll_tool.is_some()
        && let Some(executor) = targets
            .values()
            .find(|target| target.name == "exec_command" && !target.custom)
            .cloned()
    {
        for definition in operations.definitions() {
            let mut target = executor.clone();
            target.native = true;
            if targets.insert(definition.name.clone(), target).is_some() {
                return Err(error("reserved native tool name collision"));
            }
            definitions.push(definition);
        }
    }
    let request = api::Request {
        namespace,
        system: prompt.system.clone(),
        messages: messages(&prompt.input)?,
        tools: definitions,
        parallel_tool_calls: prompt.parallel_tool_calls,
        max_output_tokens: None,
        reasoning_effort,
        output_schema: prompt.output_schema.clone(),
        checkpoint: None,
        metadata: json!({}),
    };
    let started = codex_provider_runtime::start_turn(
        &home,
        provider.info().base_url.as_deref().unwrap_or(""),
        execution_purpose,
        &checkpoint_scope,
        request,
        backend,
        provider.clone(),
        cancellation.clone(),
    )
    .await
    .map_err(error)?;
    let request = started.request;
    let store = started.store;
    let execution = started.execution;
    let session = started.session;
    let handoff_journal = started.journal;
    let initial_switch = started.switched;
    let provider_id = request.namespace.provider.clone();
    let persistence_acknowledgements =
        prompt.persistence_acknowledgements && handoff_journal.is_some();
    let mut history = request.messages.clone();
    let system = request.system.clone();
    let final_store = store.clone();
    let checkpoint_state = Arc::new(tokio::sync::Mutex::new(TurnCheckpoint {
        namespace: request.namespace.clone(),
        latest: None,
        previous: request.checkpoint.clone(),
        tools_started: false,
        prior_journals: store.prior_journals(),
    }));
    let (calls, mut host_calls) = mpsc::channel::<HostCall>(128);
    let native_tools = targets
        .iter()
        .filter(|(_, target)| target.native)
        .map(|(name, _)| name.clone())
        .collect();
    let (history_tx, mut history_requests) =
        mpsc::channel::<codex_provider_runtime::HistoryRequest>(1);
    let host = Arc::new(ChannelHost {
        history_requests: persistence_acknowledgements.then_some(history_tx),
        store,
        calls,
        cancellation: cancellation.clone(),
        poll_tool,
        native_tools,
        operations: operations.clone(),
        checkpoint: checkpoint_state.clone(),
        execution: execution.clone(),
    });
    let (events, mut provider_events) = mpsc::channel(128);
    let driver_cancel = cancellation.clone();
    let error_events = events.clone();
    tokio::spawn(async move {
        if let Err(error) =
            codex_provider_runtime::drive_session(session, host, &request, driver_cancel, events)
                .await
        {
            let _ = error_events.send(Err(error)).await;
        }
    });
    let (tx, rx_event) = mpsc::channel(128);
    let (result_tx, mut results) = mpsc::channel::<api::HostCompletion>(128);
    let consumer_dropped = cancellation.clone();
    tokio::spawn(async move {
        let result = async {
            let mut replies = BTreeMap::new();
            let mut active = None;
            let mut counter = 0u64;
            let response_id = uuid::Uuid::new_v4();
            let mut usage = None;
            emit(&tx, ResponseEvent::Created { response_id: None }, &cancellation).await?;
            if let Some((previous, account)) = initial_switch { emit(&tx, ResponseEvent::ProviderAccountChanged { provider: provider_id, previous, account }, &cancellation).await?; }
            loop {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(CodexErr::TurnAborted),
                    result = results.recv() => {
                        let result = result.ok_or(CodexErr::TurnAborted)?;
                        let reply: oneshot::Sender<api::HostCompletion> = replies.remove(&result.call_id).ok_or_else(|| error("unexpected native tool result ID"))?;
                        push(&mut history, api::Role::Tool, vec![api::Content::ToolResult(result.to_tool_result())]);
                        let _ = reply.send(result);
                    }
                    event = provider_events.recv() => {
                        match event.ok_or_else(|| error("native stream closed without completion"))?.map_err(error)? {
                            event @ (api::Event::TextDelta(_) | api::Event::ThinkingDelta(_)) => {
                                let thinking = matches!(event, api::Event::ThinkingDelta(_));
                                let text = match event { api::Event::TextDelta(text) | api::Event::ThinkingDelta(text) => text, _ => unreachable!() };
                                if active.as_ref().is_some_and(|(_, _, kind)| *kind != thinking) { finish_item(&tx, &mut active, &mut history, &cancellation).await?; }
                                if active.is_none() {
                                    counter += 1;
                                    let id = format!("native-{response_id}-{counter}");
                                    emit(&tx, ResponseEvent::OutputItemAdded(message(&id, String::new(), thinking)), &cancellation).await?;
                                    active = Some((id, String::new(), thinking));
                                }
                                active.as_mut().ok_or_else(|| error("native message state missing"))?.1.push_str(&text);
                                emit(&tx, if thinking { ResponseEvent::ReasoningSummaryDelta { delta: text, summary_index: 0 } } else { ResponseEvent::OutputTextDelta(text) }, &cancellation).await?;
                            }
                            api::Event::ToolCall(_) => { finish_item(&tx, &mut active, &mut history, &cancellation).await?; }
                            api::Event::Checkpoint(_) | api::Event::ContinuationReady => {}
                            api::Event::AccountChanged { previous, account, namespace, .. } => {
                                emit(&tx, ResponseEvent::ProviderAccountChanged { provider: namespace.provider, previous, account }, &cancellation).await?;
                            }
                            api::Event::Usage(value) => { usage = Some(TokenUsage { input_tokens: value.input_tokens.min(i64::MAX as u64) as i64, cached_input_tokens: value.cached_input_tokens.min(i64::MAX as u64) as i64, output_tokens: value.output_tokens.min(i64::MAX as u64) as i64, reasoning_output_tokens: value.reasoning_tokens.min(i64::MAX as u64) as i64, total_tokens: value.input_tokens.saturating_add(value.output_tokens).min(i64::MAX as u64) as i64, ..TokenUsage::default() }); }
                            api::Event::Completed { id } => {
                                finish_item(&tx, &mut active, &mut history, &cancellation).await?;
                                // The driver has accepted terminal completion,
                                // so no checkpoint writer remains. Release the
                                // async lock before the durable final write.
                                if persistence_acknowledgements { acknowledge_history(&tx, &history, &system, &checkpoint_scope, &cancellation).await?; }
                                let checkpoint = checkpoint_state.lock().await.clone();
                                checkpoint.complete(&final_store, &execution, &system, &history, &cancellation).await.map_err(error)?;
                                if let Some(journal) = &handoff_journal { journal.complete(&cancellation).await.map_err(error)?; }
                                emit(&tx, ResponseEvent::Completed { response_id: id, token_usage: usage, usage_metadata: None, end_turn: Some(true) }, &cancellation).await?;
                                return Ok(());
                            }
                        }
                    }
                    boundary = history_requests.recv(), if !history_requests.is_closed() => {
                        if let Some(boundary) = boundary {
                            finish_item(&tx, &mut active, &mut history, &cancellation).await?;
                            let acknowledgement = acknowledge_history(&tx, &history, &system, &checkpoint_scope, &cancellation).await;
                            let _ = boundary.reply.send(acknowledgement.map_err(|error| api::ProviderError::Checkpoint(error.to_string())));
                        }
                    }
                    call = host_calls.recv(), if !host_calls.is_closed() => {
                        if let Some(call) = call {
                            finish_item(&tx, &mut active, &mut history, &cancellation).await?;
                            let id = call.call.id.clone();
                            let item = match call_item(call.call, &targets, operations.as_deref()) {
                                Ok(item) => item,
                                Err(error) => {
                                    let _ = call.reply.send(api::HostCompletion { call_id: id, output: json!({"error":error.to_string()}), structured: None, disposition: api::CompletionDisposition::Failure });
                                    continue;
                                }
                            };
                            for entry in messages(std::slice::from_ref(&item))? { push(&mut history, entry.role, entry.content); }
                            replies.insert(id, call.reply);
                            emit(&tx, ResponseEvent::OutputItemDone(item), &cancellation).await?;
                        }
                    }
                }
            }
        }.await;
        if let Err(error) = result {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {},
                _ = tx.send(Err(error)) => {},
            }
        }
        cancellation.cancel();
    });
    Ok(SessionStream {
        results: result_tx,
        events: rx_event,
        cancellation: consumer_dropped,
    })
}

#[cfg(test)]
mod tests {
    use super::NativeBackend;
    use super::Prompt;
    use super::ResponseEvent;
    use super::call_item;
    use super::messages;
    use super::tool_result;
    use super::tools;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ResponseItem;
    use codex_provider_api as api;
    use serde_json::Value;
    use serde_json::json;
    use std::sync::Arc;

    fn item(value: Value) -> ResponseItem {
        serde_json::from_value(value).expect("valid response item")
    }

    #[tokio::test]
    async fn pool_history_acknowledgement_requires_actual_persisted_content() {
        for (recorded, valid) in [
            (Some("exact"), true),
            (Some("changed"), false),
            (None, true),
        ] {
            let history = vec![item(
                json!({"type":"message", "role":"user", "content":[{"type":"input_text", "text":"exact"}]}),
            )];
            let expected = messages(&history).unwrap();
            let (send, mut receive) = tokio::sync::mpsc::channel(1);
            let host = tokio::spawn(async move {
                let Some(Ok(ResponseEvent::HistoryPersistenceRequested { reply })) =
                    receive.recv().await
                else {
                    panic!("history request");
                };
                let actual = recorded.map(|text| vec![item(json!({"type":"message", "role":"user", "content":[{"type":"input_text", "text":text}]}))]);
                let _ = reply.send(Ok(actual));
            });
            let result = super::acknowledge_history(
                &send,
                &expected,
                "system",
                "scope",
                &tokio_util::sync::CancellationToken::new(),
            )
            .await;
            assert_eq!(result.is_ok(), valid);
            if recorded.is_none() {
                assert!(result.unwrap().is_none());
            }
            host.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_full_host_event_channel() {
        let (events, _receive) = tokio::sync::mpsc::channel(1);
        events
            .send(Ok(ResponseEvent::Created { response_id: None }))
            .await
            .unwrap();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let cancelled = cancellation.clone();
        let emitting = tokio::spawn(async move {
            super::emit(
                &events,
                ResponseEvent::OutputTextDelta("blocked".into()),
                &cancelled,
            )
            .await
        });
        tokio::task::yield_now().await;
        cancellation.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(2), emitting)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            error.details(),
            codex_protocol::error::CodexErrorDetails::TurnAborted
        ));
    }

    #[test]
    fn moving_stream_parts_transfers_cancellation_ownership() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let (results, _) = tokio::sync::mpsc::channel(1);
        let (_, events) = tokio::sync::mpsc::channel(1);
        let stream = super::SessionStream {
            results,
            events,
            cancellation: cancellation.clone(),
        };
        let (_, _, owned) = stream.into_parts();
        assert!(!cancellation.is_cancelled());
        owned.cancel();
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn messages_map_roles_images_and_adjacent_assistant_items() {
        let input = vec![
            item(
                json!({"type":"message","role":"system","content":[{"type":"input_text","text":"rules"}]}),
            ),
            item(json!({"type":"message","role":"user","content":[
                {"type":"input_text","text":"look"},
                {"type":"input_image","image_url":"data:image/png;base64,AA=="}
            ]})),
            item(
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"checking"}]}),
            ),
            item(
                json!({"type":"reasoning","summary":[{"type":"summary_text","text":"thought"}],"encrypted_content":null}),
            ),
            item(
                json!({"type":"function_call","call_id":"c1","name":"read","arguments":"{\"path\":\"a\"}"}),
            ),
        ];

        let mapped = messages(&input).expect("messages convert");
        assert_eq!(mapped.len(), 3);
        assert_eq!(mapped[0].role, api::Role::System);
        assert_eq!(
            mapped[0].content,
            vec![api::Content::Text {
                text: "rules".into()
            }]
        );
        assert_eq!(mapped[1].role, api::Role::User);
        assert_eq!(
            mapped[1].content,
            vec![
                api::Content::Text {
                    text: "look".into()
                },
                api::Content::Image {
                    url: "data:image/png;base64,AA==".into()
                },
            ]
        );
        assert_eq!(mapped[2].role, api::Role::Assistant);
        assert_eq!(
            mapped[2].content,
            vec![
                api::Content::Text {
                    text: "checking".into()
                },
                api::Content::Thinking {
                    text: "thought".into(),
                    signature: None
                },
                api::Content::ToolCall(api::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: json!({"path":"a"}),
                }),
            ]
        );
    }

    #[test]
    fn native_tools_do_not_request_encrypted_agent_arguments() {
        let schema = json!({"type":"object","properties":{
            "message":{"type":"string","encrypted":true},
            "encrypted":{"type":"boolean"},
            "nested":{"anyOf":[{"$ref":"#/$defs/Message"}]}
        },"$defs":{"Message":{"type":"string","encrypted":true}},
        "default":{"encrypted":true},"required":["message"]});
        let prompt = Prompt {
            tools: vec![json!({"type":"function","name":"spawn_agent","parameters":schema})],
            ..Prompt::default()
        };
        let (definitions, _) = tools(&prompt).unwrap();
        let parameters = &definitions[0].parameters;
        assert!(
            parameters["properties"]["message"]
                .get("encrypted")
                .is_none()
        );
        assert!(parameters["$defs"]["Message"].get("encrypted").is_none());
        assert_eq!(
            parameters["properties"]["encrypted"],
            json!({"type":"boolean"})
        );
        assert_eq!(parameters["default"], json!({"encrypted":true}));
        assert_eq!(parameters["required"], json!(["message"]));
    }

    #[test]
    fn tools_map_namespaced_functions_and_custom_tools() {
        let prompt = Prompt {
            tools: vec![json!({"type":"namespace","name":"local","tools":[
                {"type":"function","name":"lookup","description":"find a thing","parameters":{"type":"object","properties":{}}},
                {"type":"custom","name":"script","description":"run script"}
            ]})],
            ..Prompt::default()
        };

        let (definitions, targets) = tools(&prompt).expect("tools convert");
        assert_eq!(
            definitions
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["local__lookup", "local__script"]
        );
        assert_eq!(targets["local__lookup"].namespace.as_deref(), Some("local"));
        assert!(!targets["local__lookup"].custom);
        assert!(targets["local__script"].custom);
        assert_eq!(
            definitions[1].parameters,
            json!({
                "type":"object", "properties":{"input":{"type":"string"}},
                "required":["input"], "additionalProperties":false
            })
        );

        let function_call = call_item(
            api::ToolCall {
                id: "c1".into(),
                name: "local__lookup".into(),
                arguments: json!({"query":"example"}),
            },
            &targets,
            None,
        )
        .expect("function call maps back with explicit plaintext metadata");
        assert!(matches!(function_call, ResponseItem::FunctionCall {
            encrypted_function_args: Some(arguments), ..
        } if arguments.is_empty()));

        let custom_call = call_item(
            api::ToolCall {
                id: "c2".into(),
                name: "local__script".into(),
                arguments: json!({"input":"print(1)"}),
            },
            &targets,
            None,
        )
        .expect("custom call maps back to the Responses item");
        assert_eq!(
            serde_json::to_value(custom_call).expect("serialize mapped call"),
            json!({
                "type":"custom_tool_call", "call_id":"c2", "name":"script",
                "namespace":"local", "input":"print(1)"
            })
        );
    }

    #[test]
    fn failed_tool_outputs_remain_marked_as_errors() {
        let failed = ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("c1".into()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("denied".into()),
                success: Some(false),
            },
            internal_chat_message_metadata_passthrough: None,
        };
        let converted = tool_result(&failed).expect("tool output converts");
        assert_eq!(converted.call_id, "c1");
        assert!(converted.is_error());
        assert_eq!(converted.output, json!("denied"));

        let mapped = messages(&[failed]).expect("history output converts");
        assert_eq!(mapped[0].role, api::Role::Tool);
        assert!(
            matches!(&mapped[0].content[0], api::Content::ToolResult(result) if result.is_error)
        );
    }

    #[test]
    fn plaintext_agent_messages_preserve_attribution_and_content() {
        let agent_message = item(
            json!({"type":"agent_message","author":"/root","recipient":"/root/worker","content":[{"type":"input_text","text":"first task"},{"type":"input_text","text":"second line"}]}),
        );
        let mapped = messages(&[agent_message]).unwrap();
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].role, api::Role::User);
        let api::Content::Text { text } = &mapped[0].content[0] else {
            panic!("expected attributed plaintext");
        };
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap(),
            json!({"type":"agent_message","author":"/root","recipient":"/root/worker","text":"first task\nsecond line"})
        );
    }

    #[test]
    fn unsupported_history_and_content_formats_are_rejected() {
        let audio = item(
            json!({"type":"message","role":"user","content":[{"type":"input_audio","audio_url":"data:audio/wav;base64,AA=="}]}),
        );
        assert!(messages(&[audio]).is_err());

        for content in [
            json!([{"type":"encrypted_content","encrypted_content":"opaque"}]),
            json!([{"type":"input_text","text":"visible"},{"type":"encrypted_content","encrypted_content":"opaque"}]),
        ] {
            let agent_message = item(
                json!({"type":"agent_message","author":"a","recipient":"b","content":content}),
            );
            assert!(messages(&[agent_message]).is_err());
        }

        let malformed_call = item(
            json!({"type":"function_call","call_id":"c1","name":"read","arguments":"not json"}),
        );
        assert!(messages(&[malformed_call]).is_err());
    }

    struct MockBackend {
        id: &'static str,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
        results: Arc<std::sync::Mutex<Vec<api::ToolResult>>>,
    }
    impl api::Provider for MockBackend {
        fn id(&self) -> &'static str {
            self.id
        }
        fn models(&self, _: api::Context) -> api::ProviderFuture<'_, Vec<api::Model>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn start(
            &self,
            request: api::Request,
            _: api::Context,
        ) -> api::ProviderFuture<'_, Box<dyn api::ProviderSession>> {
            Box::pin(async move {
                assert_eq!(request.namespace.provider, self.id);
                assert_eq!(request.tools[0].name, "lookup");
                let session = MockSession {
                    events: std::collections::VecDeque::from([
                        api::Event::TextDelta("before tool".into()),
                        api::Event::ToolCall(api::ToolCall {
                            id: "call-one".into(),
                            name: "lookup".into(),
                            arguments: json!({}),
                        }),
                    ]),
                    cancelled: self.cancelled.clone(),
                    results: self.results.clone(),
                };
                Ok(Box::new(session) as Box<dyn api::ProviderSession>)
            })
        }
    }
    struct MockSession {
        events: std::collections::VecDeque<api::Event>,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
        results: Arc<std::sync::Mutex<Vec<api::ToolResult>>>,
    }
    impl api::ProviderSession for MockSession {
        fn next_event(&mut self) -> api::ProviderFuture<'_, Option<api::Event>> {
            Box::pin(async {
                match self.events.pop_front() {
                    Some(event) => Ok(Some(event)),
                    None => std::future::pending().await,
                }
            })
        }
        fn submit_tool_result(&mut self, result: api::ToolResult) -> api::ProviderFuture<'_, ()> {
            Box::pin(async move {
                self.results.lock().unwrap().push(result);
                self.events
                    .push_back(api::Event::TextDelta("after tool".into()));
                self.events.push_back(api::Event::Completed {
                    id: "finished".into(),
                });
                Ok(())
            })
        }
        fn cancel(&mut self) -> api::ProviderFuture<'_, ()> {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    fn mock_native(home: &std::path::Path) -> (Arc<NativeBackend>, Arc<MockBackend>, Prompt) {
        let provider = Arc::new(MockBackend {
            id: "claude",
            cancelled: Arc::default(),
            results: Arc::default(),
        });
        let mut info = codex_model_provider_info::built_in_model_providers(None)
            .remove("claude")
            .unwrap();
        info.experimental_bearer_token = Some("fixture-token".into());
        let backend = Arc::new(NativeBackend::with_provider(
            info,
            Some(home.to_owned()),
            provider.clone(),
        ));
        let prompt = Prompt {
            tools: vec![
                json!({"type":"function","name":"lookup","description":"lookup","parameters":{"type":"object","properties":{}}}),
            ],
            ..Prompt::default()
        };
        (backend, provider, prompt)
    }

    fn fifth_factory(
        _: codex_provider_registry::TransportOptions<'_>,
    ) -> api::Result<Arc<dyn api::Provider>> {
        Ok(Arc::new(MockBackend {
            id: "fixture-fifth",
            cancelled: Arc::default(),
            results: Arc::default(),
        }))
    }

    static FIFTH: codex_provider_registry::Descriptor = codex_provider_registry::Descriptor {
        id: "fixture-fifth",
        display_name: "Fifth fixture provider",
        env_key: None,
        auxiliary_model: "fixture-model",
        legacy_wire_aliases: &[],
        authentication: codex_provider_registry::Authentication::ApiKey,
        capabilities: codex_provider_registry::Capabilities {
            image_generation: false,
            web_search: false,
            external_web_access: false,
        },
        recovery_policy: api::RecoveryPolicy::RestorePrevious,
        native_operations: false,
        factory: Some(fifth_factory),
        auth_driver: Some(&codex_provider_registry::AuthDriver::ApiKey),
    };

    #[tokio::test]
    async fn registered_fifth_provider_drives_host_tools_through_the_generic_adapter() {
        let home = tempfile::tempdir().unwrap();
        let (_, _, prompt) = mock_native(home.path());
        let mut registry = codex_provider_registry::Registry::new();
        registry.register(&FIFTH).unwrap();
        let descriptor = registry.get("fixture-fifth").unwrap();
        let mut info = codex_model_provider_info::built_in_model_providers(None)
            .remove("claude")
            .unwrap();
        info.wire_api = codex_model_provider_info::WireApi::Native;
        info.native_provider = Some(descriptor.id.into());
        info.experimental_bearer_token = Some("fixture-token".into());
        let backend = Arc::new(NativeBackend::with_descriptor(
            info,
            Some(home.path().to_owned()),
            descriptor,
        ));
        let mut stream = super::stream(
            backend,
            &prompt,
            "fixture-model",
            "fifth-thread".into(),
            None,
            "window".into(),
            "turn",
        )
        .await
        .unwrap();
        let mut called = false;
        let mut text = String::new();
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(2), stream.events.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            match event {
                ResponseEvent::OutputTextDelta(delta) => text.push_str(&delta),
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                    call_id, name, ..
                }) => {
                    assert!(!called);
                    assert_eq!(name, "lookup");
                    called = true;
                    stream
                        .results
                        .send(api::HostCompletion {
                            call_id,
                            output: json!("host result"),
                            structured: None,
                            disposition: api::CompletionDisposition::Success,
                        })
                        .await
                        .unwrap();
                }
                ResponseEvent::Completed { response_id, .. } => {
                    assert!(called);
                    assert_eq!(response_id, "finished");
                    assert_eq!(text, "before toolafter tool");
                    break;
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn native_stream_preserves_text_order_and_waits_for_host_result() {
        let home = tempfile::tempdir().unwrap();
        let (backend, provider, prompt) = mock_native(home.path());
        let model = "fixture-model";
        let mut stream = super::stream(
            backend.clone(),
            &prompt,
            model,
            "thread".into(),
            None,
            "window".into(),
            "turn",
        )
        .await
        .unwrap();
        let mut messages = vec![];
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(2), stream.events.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            match event {
                ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. }) => {
                    messages.extend(content)
                }
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, .. }) => {
                    assert_eq!(call_id, "call-one");
                    break;
                }
                ResponseEvent::Completed { .. } => panic!("completed before host result"),
                _ => {}
            }
        }
        assert_eq!(
            messages,
            vec![codex_protocol::models::ContentItem::OutputText {
                text: "before tool".into(),
                annotations: None,
                logprobs: None,
            }]
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), stream.events.recv())
                .await
                .is_err()
        );
        // Structured execution metadata remains outside the exact public history.
        stream
            .results
            .send(api::HostCompletion {
                call_id: "call-one".into(),
                output: json!("denied"),
                structured: Some(json!({"extra":"private runtime data"})),
                disposition: api::CompletionDisposition::Failure,
            })
            .await
            .unwrap();
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(2), stream.events.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            if let ResponseEvent::Completed {
                response_id,
                end_turn,
                ..
            } = event
            {
                assert_eq!(response_id, "finished");
                assert_eq!(end_turn, Some(true));
                break;
            }
        }
        let results = provider.results.lock().unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_error);
        assert_eq!(results[0].output, json!("denied"));
    }

    #[tokio::test]
    async fn dropping_native_stream_cancels_pending_host_and_upstream() {
        let home = tempfile::tempdir().unwrap();
        let (backend, provider, prompt) = mock_native(home.path());
        let model = "fixture-model";
        let mut stream = super::stream(
            backend.clone(),
            &prompt,
            model,
            "thread".into(),
            None,
            "window".into(),
            "turn",
        )
        .await
        .unwrap();
        loop {
            if matches!(
                stream.events.recv().await.unwrap().unwrap(),
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { .. })
            ) {
                break;
            }
        }
        drop(stream);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !provider.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(provider.results.lock().unwrap().is_empty());
        let changed_model = "other-model";
        let resumed = super::stream(
            backend,
            &prompt,
            changed_model,
            "thread".into(),
            None,
            "new-compacted-window".into(),
            "turn",
        )
        .await;
        let Err(error) = resumed else {
            panic!("model/window changes must not bypass interrupted execution");
        };
        assert!(error.to_string().contains("explicitly branch"));
    }
}
