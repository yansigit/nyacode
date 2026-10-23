//! Cursor's AgentService protocol implemented directly in Rust.
//! Tool execution is exclusively provided by the host's executable catalog.
pub mod auth;
mod connect;
mod failure;
pub mod native;
pub mod operations;
mod state;
mod wire;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use codex_provider_api::Content;
use codex_provider_api::Context;
use codex_provider_api::Event;
use codex_provider_api::Model;
use codex_provider_api::Namespace;
use codex_provider_api::Provider;
use codex_provider_api::ProviderError;
use codex_provider_api::ProviderFuture;
use codex_provider_api::ProviderSession;
use codex_provider_api::Request;
use codex_provider_api::Result;
use codex_provider_api::Role;
use codex_provider_api::ToolCall;
use codex_provider_api::ToolResult;
use codex_provider_api::Usage;
use prost::Message;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

const MCP_PROVIDER: &str = "codex-native-host";
const MAX_TOOLS: usize = 330;
// Local serialization bound. The full host collaboration catalog can exceed
// 120 KB; retain its schemas and instructions instead of truncating tools.
const MAX_TOOL_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug)]
pub struct Options {
    pub base_url: String,
    pub client_version: String,
    pub connect_timeout: Duration,
    pub idle_timeout: Duration,
    /// Drain trailing metadata after TurnEnded when the server holds HTTP/2 open.
    /// The default matches pinned OpenCodex; later metadata requires a larger grace.
    pub completion_grace: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            base_url: "https://api2.cursor.sh".into(),
            client_version: "cli-2026.07.08-0c04a8a".into(),
            connect_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(90),
            completion_grace: Duration::from_millis(500),
        }
    }
}

pub struct CursorProvider {
    options: Options,
    client: reqwest::Client,
}

pub(crate) fn validate_endpoint(endpoint: &str) -> Result<String> {
    let url = url::Url::parse(endpoint)
        .map_err(|_| ProviderError::Protocol("invalid Cursor endpoint".into()))?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if (url.scheme() != "https" && !(local && url.scheme() == "http"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::Protocol(
            "Cursor endpoint must use HTTPS without credentials, query or fragment".into(),
        ));
    }
    Ok(endpoint.trim_end_matches('/').into())
}

pub(crate) fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|_| ProviderError::Transport("cannot create Cursor HTTP client".into()))
}

impl CursorProvider {
    pub fn new(mut options: Options) -> Result<Self> {
        options.base_url = validate_endpoint(&options.base_url)?;
        if options.connect_timeout.is_zero()
            || options.idle_timeout.is_zero()
            || options.completion_grace.is_zero()
        {
            return Err(ProviderError::Protocol(
                "Cursor timeouts must be positive".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(options.connect_timeout)
            .http2_prior_knowledge()
            .build()
            .map_err(|_| ProviderError::Transport("cannot create Cursor HTTP/2 client".into()))?;
        Ok(Self { options, client })
    }

    fn rpc(&self, method: &str, token: &str, streaming: bool) -> reqwest::RequestBuilder {
        self.client
            .post(format!(
                "{}/agent.v1.AgentService/{method}",
                self.options.base_url
            ))
            .bearer_auth(token)
            .header(
                "content-type",
                if streaming {
                    "application/connect+proto"
                } else {
                    "application/proto"
                },
            )
            .header("connect-protocol-version", "1")
            .header("connect-accept-encoding", "gzip")
            .header("x-cursor-client-type", "cli")
            .header("x-cursor-client-version", &self.options.client_version)
    }

    async fn bearer(&self, context: &Context) -> Result<String> {
        if context.credentials.access_token.starts_with("crsr_") {
            Ok(auth::AuthClient::new(&self.options.base_url)?
                .exchange_api_key(
                    &context.credentials.access_token,
                    context.cancellation.clone(),
                )
                .await?
                .access_token)
        } else {
            Ok(context.credentials.access_token.clone())
        }
    }

    async fn start_session(&self, request: Request, context: Context) -> Result<Session> {
        validate_request(&request, &context)?;
        let mut state = state::State::restore(&request)?;
        let (run, request_context) = prepare(&request, &mut state)?;
        state.message_count = request.messages.len();
        let context_hash = state::hash_messages(&request.messages)?;
        let bearer = self.bearer(&context).await?;
        let (writer, receiver) = mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(16);
        writer
            .try_send(Ok(connect::encode(
                &wire::ClientMessage {
                    run: Some(run),
                    ..Default::default()
                }
                .encode_to_vec(),
            )?))
            .map_err(|_| ProviderError::Transport("Cursor request channel failed".into()))?;
        let response = tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => return Err(ProviderError::Cancelled),
            response = tokio::time::timeout(self.options.connect_timeout,
                self.rpc("Run", &bearer, true)
                    .body(reqwest::Body::wrap_stream(ReceiverStream::new(receiver))).send()) =>
                response.map_err(|_| ProviderError::Transport("Cursor connect timed out".into()))?
                    .map_err(|_| ProviderError::Transport("Cursor HTTP/2 connection failed".into()))?,
        };
        let response = validate_status(response, &context).await?;
        if response.version() != reqwest::Version::HTTP_2 {
            return Err(ProviderError::Protocol(
                "Cursor bidirectional Run requires HTTP/2".into(),
            ));
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !content_type.starts_with("application/connect+proto") {
            return Err(ProviderError::Protocol(
                "Cursor Run returned an unexpected content type".into(),
            ));
        }
        let initial = Event::Checkpoint(state.snapshot(&request.namespace, &context_hash)?);
        Ok(Session {
            reader: Some(connect::Reader::new(response)),
            writer: Some(writer),
            cancellation: context.cancellation,
            namespace: request.namespace,
            context_hash,
            state,
            request_context,
            tools: request.tools.into_iter().map(|tool| tool.name).collect(),
            events: VecDeque::from([initial]),
            deferred: VecDeque::new(),
            pending: BTreeMap::new(),
            ended: None,
            completed: false,
            options: self.options.clone(),
        })
    }
}

impl Provider for CursorProvider {
    fn recovery_policy(&self) -> codex_provider_api::RecoveryPolicy {
        codex_provider_api::RecoveryPolicy::RebuildFromHistory
    }

    fn native_operations(
        &self,
    ) -> Option<std::sync::Arc<dyn codex_provider_api::NativeOperations>> {
        Some(std::sync::Arc::new(operations::CursorOperations))
    }

    fn id(&self) -> &'static str {
        "cursor"
    }

    fn handoff_request(
        &self,
        request: &Request,
        boundary: codex_provider_api::ContinuationBoundary,
    ) -> Result<Request> {
        if boundary == codex_provider_api::ContinuationBoundary::RecordedTools
            || request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|content| {
                    matches!(
                        content,
                        codex_provider_api::Content::Thinking { .. }
                            | codex_provider_api::Content::Opaque { .. }
                    )
                })
        {
            return Err(ProviderError::Unsupported("Cursor account handoff is only verified before generation or between completed ordinary turns".into()));
        }
        let mut exported = request.clone();
        exported.checkpoint = None;
        if let Some(metadata) = exported.metadata.as_object_mut() {
            metadata.remove("host_checkpoint_prefix");
        }
        Ok(exported)
    }
    fn models(&self, context: Context) -> ProviderFuture<'_, Vec<Model>> {
        Box::pin(async move {
            let bearer = self.bearer(&context).await?;
            if context.credentials.access_token.is_empty() {
                return Err(ProviderError::Authentication(
                    "Cursor login required".into(),
                ));
            }
            let response = tokio::select! {
                biased;
                _ = context.cancellation.cancelled() => return Err(ProviderError::Cancelled),
                response = tokio::time::timeout(self.options.connect_timeout, self.rpc("GetUsableModels", &bearer, false).body(Vec::<u8>::new()).send()) =>
                    response.map_err(|_| ProviderError::Transport("Cursor discovery timed out".into()))?
                        .map_err(|_| ProviderError::Transport("Cursor discovery connection failed".into()))?,
            };
            let response = validate_status(response, &context).await?;
            let body = tokio::time::timeout(
                self.options.idle_timeout,
                connect::bounded_body(response, &context.cancellation),
            )
            .await
            .map_err(|_| {
                ProviderError::Transport("Cursor discovery response timed out".into())
            })??;
            let response = wire::ModelsResponse::decode(body.as_slice()).map_err(|_| {
                ProviderError::Protocol("invalid Cursor model catalog protobuf".into())
            })?;
            let mut seen = BTreeSet::new();
            Ok(response
                .models
                .into_iter()
                .filter(|model| !model.id.is_empty() && seen.insert(model.id.clone()))
                .map(|model| Model {
                    name: if model.name.is_empty() {
                        model.id.clone()
                    } else {
                        model.name
                    },
                    id: model.id,
                    context_window: None,
                    supports_images: None,
                })
                .collect())
        })
    }

    fn start(
        &self,
        request: Request,
        context: Context,
    ) -> ProviderFuture<'_, Box<dyn ProviderSession>> {
        Box::pin(async move {
            Ok(Box::new(self.start_session(request, context).await?) as Box<dyn ProviderSession>)
        })
    }
}

fn validate_request(request: &Request, context: &Context) -> Result<()> {
    if request.namespace.provider != "cursor"
        || request.namespace.account != context.credentials.account_id
    {
        return Err(ProviderError::Authentication(
            "Cursor request account identity mismatch".into(),
        ));
    }
    if context.credentials.access_token.is_empty() {
        return Err(ProviderError::Authentication(
            "Cursor login required".into(),
        ));
    }
    if request.namespace.model.is_empty() || request.namespace.thread.is_empty() {
        return Err(ProviderError::Protocol(
            "Cursor model and thread identities are required".into(),
        ));
    }
    if request.max_output_tokens.is_some()
        || request.output_schema.is_some()
        || request.reasoning_effort.is_some()
    {
        return Err(ProviderError::Unsupported("Cursor output limits, structured output and reasoning controls require model-specific discovery parameters".into()));
    }
    if request.tools.len() > MAX_TOOLS {
        return Err(ProviderError::Unsupported(
            "Cursor tool catalog exceeds 330 tools".into(),
        ));
    }
    let mut names = BTreeSet::new();
    for tool in &request.tools {
        if tool.name.is_empty() || !names.insert(&tool.name) {
            return Err(ProviderError::Protocol(
                "Cursor tool names must be unique and nonempty".into(),
            ));
        }
    }
    for message in &request.messages {
        for content in &message.content {
            if let Content::Opaque { namespace, .. } = content
                && namespace != &request.namespace
            {
                return Err(ProviderError::Checkpoint(
                    "foreign provider replay content in Cursor request".into(),
                ));
            }
        }
    }
    Ok(())
}

fn prepare(
    request: &Request,
    state: &mut state::State,
) -> Result<(wire::RunRequest, wire::RequestContext)> {
    let tools: Vec<_> = request
        .tools
        .iter()
        .map(|tool| wire::McpTool {
            name: tool.name.clone(),
            tool_name: tool.name.clone(),
            provider: MCP_PROVIDER.into(),
            description: tool.description.clone(),
            schema: json_value(&tool.parameters).encode_to_vec(),
        })
        .collect();
    let mcp = wire::McpTools {
        tools: tools.clone(),
    };
    if mcp.encoded_len() > MAX_TOOL_BYTES {
        return Err(ProviderError::Unsupported(format!(
            "Cursor tool catalog exceeds the {MAX_TOOL_BYTES}-byte local limit"
        )));
    }
    let request_context = wire::RequestContext {
        tools,
        web_search: Some(false),
        web_fetch: Some(false),
        env: Some(wire::Environment {
            os: std::env::consts::OS.into(),
            workspaces: request
                .metadata
                .get("workspace_paths")
                .and_then(|v| v.as_array())
                .map(|paths| {
                    paths
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            shell: request
                .metadata
                .get("shell")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .into(),
            sandbox: request
                .metadata
                .get("sandbox_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            timezone: request
                .metadata
                .get("time_zone")
                .and_then(|v| v.as_str())
                .unwrap_or("UTC")
                .into(),
        }),
    };
    let active = request
        .messages
        .iter()
        .rposition(|message| message.role == Role::User);
    let new_user =
        active.filter(|index| request.checkpoint.is_none() || *index >= state.message_count);
    let mut images = Vec::new();
    let mut historical_attachments = Vec::new();
    let mut action = if let Some(index) = new_user {
        let message = &request.messages[index];
        let mut text = Vec::new();
        for content in &message.content {
            match content {
                Content::Text { text: value } => text.push(value.clone()),
                Content::Image { url } => {
                    for part in
                        native::mcp_content(&json!([{"type":"input_image","image_url":url}]))?
                    {
                        let image = part.image.ok_or_else(|| {
                            ProviderError::Protocol("Cursor image content missing image".into())
                        })?;
                        attach_image(state, &mut images, image)?;
                    }
                }
                _ => {
                    return Err(ProviderError::Unsupported(
                        "Cursor active user message contains unsupported content".into(),
                    ));
                }
            }
        }
        wire::Action {
            user: Some(wire::UserAction {
                message: Some(wire::UserMessage {
                    text: text.join("\n"),
                    id: uuid::Uuid::new_v4().to_string(),
                    mode: 1,
                    selected_context: Some(wire::SelectedContext { images: Vec::new() }),
                }),
                context: Some(request_context.clone()),
            }),
            ..Default::default()
        }
    } else {
        wire::Action {
            resume: Some(wire::Resume {
                context: Some(request_context.clone()),
            }),
            ..Default::default()
        }
    };
    let mut encoded_state = STANDARD
        .decode(&state.checkpoint)
        .map_err(|_| ProviderError::Checkpoint("invalid Cursor checkpoint encoding".into()))?;
    if encoded_state.is_empty() {
        let mut roots = Vec::new();
        if !request.system.is_empty() {
            roots.push(state.put_json_blob(json!({"role":"system","content":request.system}))?);
        }
        // Bootstrap blobs are content addressed. Their tool text must use the
        // same sorted embedded JSON as the baseline blob representation.
        for (index, message) in state::canonical_messages(&request.messages)
            .iter()
            .enumerate()
        {
            if Some(index) == new_user {
                continue;
            }
            let role = match message.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant | Role::Tool => "assistant",
            };
            let mut text = Vec::new();
            for content in &message.content {
                match content {
                    Content::Text { text: value } => text.push(value.clone()),
                    Content::ToolCall(call) => text.push(format!(
                        "[Tool Call {} {}] {}",
                        call.id, call.name, call.arguments
                    )),
                    Content::ToolResult(result) => {
                        let mut output = Vec::new();
                        for part in native::mcp_content(&result.output)? {
                            if let Some(value) = part.text {
                                output.push(value.text);
                            }
                            if let Some(image) = part.image {
                                let path = attach_image(state, &mut images, image)?;
                                let reference = format!(
                                    "History message {} tool result {} image: {path}",
                                    index + 1,
                                    result.call_id
                                );
                                historical_attachments.push(reference.clone());
                                output.push(format!("[Image attachment: {reference}]"));
                            }
                        }
                        text.push(format!(
                            "[Tool {} {}] {}",
                            if result.is_error { "Error" } else { "Result" },
                            result.call_id,
                            output.join("\n")
                        ));
                    }
                    Content::Thinking { .. } => {}
                    Content::Image { url } => {
                        for part in
                            native::mcp_content(&json!([{"type":"input_image","image_url":url}]))?
                        {
                            let image = part.image.ok_or_else(|| {
                                ProviderError::Protocol(
                                    "Cursor history content missing image".into(),
                                )
                            })?;
                            let path = attach_image(state, &mut images, image)?;
                            let reference =
                                format!("History message {} {role} image: {path}", index + 1);
                            historical_attachments.push(reference.clone());
                            text.push(format!("[Image attachment: {reference}]"));
                        }
                    }
                    Content::Opaque { .. } => {}
                }
            }
            if !text.is_empty() {
                roots.push(state.put_json_blob(
                    json!({"role":role,"content":[{"type":"text","text":text.join("\n")}]}),
                )?);
            }
        }
        if roots.len() > 192 {
            return Err(ProviderError::Unsupported(
                "Cursor history exceeds native root envelope limit".into(),
            ));
        }
        encoded_state = wire::State { roots }.encode_to_vec();
    }
    if !images.is_empty() {
        let user = action
            .user
            .as_mut()
            .and_then(|user| user.message.as_mut())
            .ok_or_else(|| {
                ProviderError::Unsupported(
                    "Cursor image bootstrap requires a new user message".into(),
                )
            })?;
        if !historical_attachments.is_empty() {
            user.text.push_str("\n\nThe following images belong to earlier history messages and tool results, at the attachment paths referenced in that history. Preserve those original associations:\n");
            user.text.push_str(&historical_attachments.join("\n"));
        }
        user.selected_context = Some(wire::SelectedContext { images });
    }
    Ok((
        wire::RunRequest {
            state: encoded_state,
            action: Some(action),
            model: Some(wire::ModelDetails {
                id: request.namespace.model.clone(),
                display_id: request.namespace.model.clone(),
                name: request.namespace.model.clone(),
            }),
            tools: Some(mcp),
            conversation_id: Some(state.conversation.clone()),
            system: None,
            exclude_workspace_context: None,
        },
        request_context,
    ))
}

fn attach_image(
    state: &mut state::State,
    images: &mut Vec<wire::SelectedImage>,
    image: wire::McpImage,
) -> Result<String> {
    if images.len() >= 16 {
        return Err(ProviderError::Unsupported(
            "Cursor image input exceeds 16 attachments".into(),
        ));
    }
    let id = state.put_blob(&image.data)?;
    let uuid = uuid::Uuid::new_v4().to_string();
    let path = format!("/attachments/{uuid}");
    images.push(wire::SelectedImage {
        path: path.clone(),
        uuid,
        mime: image.mime,
        blob: Some(wire::ImageBlob {
            id,
            data: image.data,
        }),
    });
    Ok(path)
}

async fn validate_status(
    mut response: reqwest::Response,
    context: &Context,
) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let retry_after_seconds = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            codex_provider_api::retry_after_seconds(
                value,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            )
        });
    let read = async {
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ProviderError::Transport("Cursor rejection read failed".into()))?
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
        result = tokio::time::timeout(std::time::Duration::from_secs(30), read) => result.map_err(|_| ProviderError::Transport("Cursor rejection timeout".into()))??,
    };
    Err(ProviderError::Classified(failure::classify(
        codex_provider_api::FailureResponse {
            status,
            body: &bytes,
            retry_after_seconds,
            model: "",
            now: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            outcome: codex_provider_api::RequestOutcome::Rejected,
        },
    )))
}

fn json_value(value: &Value) -> prost_types::Value {
    use prost_types::value::Kind;
    prost_types::Value {
        kind: Some(match value {
            Value::Null => Kind::NullValue(0),
            Value::Bool(value) => Kind::BoolValue(*value),
            Value::Number(value) => Kind::NumberValue(value.as_f64().unwrap_or(0.0)),
            Value::String(value) => Kind::StringValue(value.clone()),
            Value::Array(values) => Kind::ListValue(prost_types::ListValue {
                values: values.iter().map(json_value).collect(),
            }),
            Value::Object(values) => Kind::StructValue(prost_types::Struct {
                fields: values
                    .iter()
                    .map(|(key, value)| (key.clone(), json_value(value)))
                    .collect(),
            }),
        }),
    }
}

fn from_value(value: prost_types::Value) -> Result<Value> {
    from_value_at(value, true)
}

fn from_value_at(value: prost_types::Value, top_level: bool) -> Result<Value> {
    use prost_types::value::Kind;
    Ok(match value.kind {
        Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::BoolValue(value)) => Value::Bool(value),
        Some(Kind::NumberValue(value)) => {
            // Protobuf Value stores every number as a double. Preserve integer
            // JSON syntax for exactly representable values so host tools with
            // integer fields accept the same values as the reference JS codec.
            if value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0 {
                Value::Number(serde_json::Number::from(value as i64))
            } else {
                serde_json::Number::from_f64(value)
                    .map(Value::Number)
                    .ok_or_else(|| {
                        ProviderError::Protocol("nonfinite Cursor tool argument".into())
                    })?
            }
        }
        Some(Kind::StringValue(value)) => {
            if top_level {
                serde_json::from_str(&value).unwrap_or(Value::String(value))
            } else {
                Value::String(value)
            }
        }
        Some(Kind::ListValue(values)) => Value::Array(
            values
                .values
                .into_iter()
                .map(|value| from_value_at(value, false))
                .collect::<Result<_>>()?,
        ),
        Some(Kind::StructValue(values)) => Value::Object(
            values
                .fields
                .into_iter()
                .map(|(key, value)| Ok((key, from_value_at(value, false)?)))
                .collect::<Result<_>>()?,
        ),
        None => {
            return Err(ProviderError::Protocol(
                "Cursor tool argument lacks a value".into(),
            ));
        }
    })
}

fn decode_args(args: std::collections::HashMap<String, Vec<u8>>) -> Result<Value> {
    args.into_iter()
        .map(|(key, bytes)| {
            let value = if let Ok(value) = prost_types::Value::decode(bytes.as_slice()) {
                if value.encode_to_vec() == bytes {
                    from_value(value)?
                } else {
                    decode_text_arg(&bytes)?
                }
            } else {
                decode_text_arg(&bytes)?
            };
            Ok((key, value))
        })
        .collect::<Result<serde_json::Map<_, _>>>()
        .map(Value::Object)
}

fn decode_text_arg(bytes: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ProviderError::Protocol("invalid Cursor tool argument encoding".into()))?;
    Ok(serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.into())))
}

#[derive(Clone)]
struct Pending {
    id: u32,
    exec_id: String,
    operation: native::Operation,
}

struct Session {
    reader: Option<connect::Reader>,
    writer: Option<mpsc::Sender<std::result::Result<Bytes, std::io::Error>>>,
    cancellation: CancellationToken,
    namespace: Namespace,
    context_hash: String,
    state: state::State,
    request_context: wire::RequestContext,
    tools: BTreeSet<String>,
    events: VecDeque<Event>,
    deferred: VecDeque<wire::ClientMessage>,
    pending: BTreeMap<String, Pending>,
    ended: Option<tokio::time::Instant>,
    completed: bool,
    options: Options,
}

impl Session {
    fn snapshot(&mut self) -> Result<()> {
        self.events.push_back(Event::Checkpoint(
            self.state.snapshot(&self.namespace, &self.context_hash)?,
        ));
        Ok(())
    }

    async fn send(&mut self, message: wire::ClientMessage) -> Result<()> {
        let writer = self
            .writer
            .as_ref()
            .ok_or_else(|| ProviderError::Transport("Cursor request stream closed".into()))?;
        let payload = connect::encode(&message.encode_to_vec())?;
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(ProviderError::Cancelled),
            sent = tokio::time::timeout(self.options.idle_timeout, writer.send(Ok(payload))) =>
                sent.map_err(|_| ProviderError::Transport("Cursor request backpressure timeout".into()))?
                    .map_err(|_| ProviderError::Transport("Cursor request stream closed".into())),
        }
    }

    fn on_message(&mut self, message: wire::ServerMessage) -> Result<()> {
        if let Some(checkpoint) = message.checkpoint {
            self.state.checkpoint = STANDARD.encode(checkpoint);
            self.snapshot()?;
        }
        if let Some(kv) = message.kv {
            if let Some(set) = kv.set {
                self.state.set_blob(&set.id, &set.data)?;
                self.snapshot()?;
                self.deferred.push_back(wire::ClientMessage {
                    kv: Some(wire::KvClient {
                        id: kv.id,
                        set: Some(wire::BlobAck {}),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            } else if let Some(get) = kv.get {
                self.deferred.push_back(wire::ClientMessage {
                    kv: Some(wire::KvClient {
                        id: kv.id,
                        get: Some(wire::BlobResult {
                            data: self.state.blob(&get.id)?,
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            } else {
                return Err(ProviderError::Protocol(
                    "unknown Cursor blob operation".into(),
                ));
            }
        }
        if let Some(exec) = message.exec {
            let fields = wire_fields(&exec);
            let exec = wire::ExecServer::decode(exec.as_slice())
                .map_err(|_| ProviderError::Protocol("invalid Cursor execution protobuf".into()))?;
            if let Some(query) = &exec.mcp_state {
                let include = query.identifiers.is_empty()
                    || query.identifiers.iter().any(|id| id == MCP_PROVIDER);
                let servers = if include && !self.request_context.tools.is_empty() {
                    vec![wire::McpStateServer {
                        name: MCP_PROVIDER.into(),
                        identifier: MCP_PROVIDER.into(),
                        tools: self.request_context.tools.clone(),
                        status: Some("ready".into()),
                    }]
                } else {
                    Vec::new()
                };
                self.deferred.push_back(wire::ClientMessage {
                    exec: Some(wire::ExecClient {
                        id: exec.id,
                        exec_id: exec.exec_id,
                        mcp_state: Some(wire::McpStateResult {
                            success: Some(wire::McpStateSuccess { servers }),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            } else if exec.request_context.is_some() {
                self.deferred.push_back(wire::ClientMessage {
                    exec: Some(wire::ExecClient {
                        id: exec.id,
                        exec_id: exec.exec_id,
                        request_context: Some(wire::ContextResult {
                            success: Some(wire::ContextSuccess {
                                context: Some(self.request_context.clone()),
                            }),
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            } else {
                let (call, operation) = if let Some(mcp) = &exec.mcp {
                    if !mcp.provider.is_empty() && mcp.provider != MCP_PROVIDER {
                        return Err(ProviderError::Unsupported(
                            "Cursor requested an unavailable MCP server".into(),
                        ));
                    }
                    let name = if mcp.tool_name.is_empty() {
                        mcp.name.clone()
                    } else {
                        mcp.tool_name.clone()
                    };
                    (
                        ToolCall {
                            id: mcp.call_id.clone(),
                            name,
                            arguments: decode_args(mcp.args.clone())?,
                        },
                        native::Operation::Mcp,
                    )
                } else {
                    native::call(&exec)?.ok_or_else(|| {
                        ProviderError::Unsupported(
                            format!("Cursor requested an unadvertised native execution operation (fields {fields:?})"),
                        )
                    })?
                };
                if call.id.is_empty() || !self.tools.contains(&call.name) {
                    return Err(ProviderError::Unsupported(
                        "Cursor requested a tool absent from the executable host catalog".into(),
                    ));
                }
                let pending = Pending {
                    id: exec.id,
                    exec_id: exec.exec_id,
                    operation,
                };
                if let Some(previous) = self.state.executions.get(&call.id) {
                    if previous.call != call {
                        return Err(ProviderError::Protocol(
                            "Cursor reused a tool call ID with different arguments".into(),
                        ));
                    }
                    if let Some(result) = &previous.result {
                        self.deferred.extend(native::response(
                            pending.id,
                            &pending.exec_id,
                            &pending.operation,
                            result,
                        )?);
                    } else if !self.pending.contains_key(&call.id) {
                        return Err(ProviderError::Checkpoint(
                            "Cursor tool execution has an unknown outcome".into(),
                        ));
                    }
                } else {
                    self.state.executions.insert(
                        call.id.clone(),
                        state::Execution {
                            call: call.clone(),
                            result: None,
                        },
                    );
                    self.pending.insert(call.id.clone(), pending);
                    self.snapshot()?;
                    self.events.push_back(Event::ToolCall(call));
                }
            }
        }
        if message.query.is_some() || message.control.is_some() {
            return Err(ProviderError::Unsupported(
                "Cursor requested an unavailable interaction/control operation".into(),
            ));
        }
        if let Some(interaction) = message.interaction {
            if let Some(text) = interaction.text {
                self.events.push_back(Event::TextDelta(text.text));
            }
            if let Some(thinking) = interaction.thinking {
                self.events.push_back(Event::ThinkingDelta(thinking.text));
            }
            if let Some(ended) = interaction.ended {
                if !self.pending.is_empty() {
                    return Err(ProviderError::Protocol(
                        "Cursor ended while host tools are pending".into(),
                    ));
                }
                let tokens = |value: Option<i64>| {
                    u64::try_from(value.unwrap_or(0))
                        .map_err(|_| ProviderError::Protocol("negative Cursor usage".into()))
                };
                self.events.push_back(Event::Usage(Usage {
                    input_tokens: tokens(ended.input)?,
                    output_tokens: tokens(ended.output)?,
                    cached_input_tokens: tokens(ended.cache_read)?,
                    reasoning_tokens: tokens(ended.reasoning)?,
                }));
                self.ended = Some(tokio::time::Instant::now() + self.options.completion_grace);
            }
        }
        Ok(())
    }

    fn complete(&mut self) -> Result<Option<Event>> {
        if self.ended.is_none() {
            return Err(ProviderError::Protocol(
                "Cursor stream ended without turn completion".into(),
            ));
        }
        if let Some(reader) = &self.reader {
            reader.check_buffer_complete()?;
        }
        self.reader.take();
        self.writer.take();
        self.completed = true;
        Ok(Some(Event::Completed {
            id: self.state.conversation.clone(),
        }))
    }

    async fn poll(&mut self) -> Result<Option<Event>> {
        loop {
            if self.cancellation.is_cancelled() {
                self.reader.take();
                self.writer.take();
                return Err(ProviderError::Cancelled);
            }
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
            if self.completed {
                return Ok(None);
            }
            // Checkpoint events must be persisted by the host before it polls
            // again. Only then acknowledge KV and tool results upstream.
            if let Some(message) = self.deferred.front().cloned() {
                self.send(message).await?;
                self.deferred.pop_front();
                continue;
            }
            let timeout = self
                .ended
                .map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
                .unwrap_or(self.options.idle_timeout);
            let reader = self
                .reader
                .as_mut()
                .ok_or_else(|| ProviderError::Transport("Cursor response stream closed".into()))?;
            let payload = tokio::time::timeout(timeout, reader.next(&self.cancellation)).await;
            match payload {
                Err(_) if self.ended.is_some() => return self.complete(),
                Err(_) => {
                    return Err(ProviderError::Transport(
                        "Cursor response stalled before completion".into(),
                    ));
                }
                Ok(Ok(Some(payload))) => {
                    let message = wire::ServerMessage::decode(payload).map_err(|_| {
                        ProviderError::Protocol("invalid Cursor server protobuf".into())
                    })?;
                    self.on_message(message)?;
                }
                Ok(Ok(None)) => return self.complete(),
                Ok(Err(error)) => return Err(error),
            }
        }
    }
}

impl ProviderSession for Session {
    fn next_event(&mut self) -> ProviderFuture<'_, Option<Event>> {
        Box::pin(self.poll())
    }

    fn submit_tool_result(&mut self, result: ToolResult) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            if self.cancellation.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            let pending = self
                .pending
                .get(&result.call_id)
                .ok_or_else(|| {
                    ProviderError::Protocol(
                        "Cursor result does not match a pending host call".into(),
                    )
                })?
                .clone();
            let messages =
                native::response(pending.id, &pending.exec_id, &pending.operation, &result)?;
            let entry = self
                .state
                .executions
                .get_mut(&result.call_id)
                .ok_or_else(|| {
                    ProviderError::Protocol("Cursor execution journal entry missing".into())
                })?;
            entry.result = Some(result.clone());
            self.snapshot()?;
            self.deferred.extend(messages);
            self.pending.remove(&result.call_id);
            Ok(())
        })
    }

    fn cancel(&mut self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            let message = wire::ClientMessage {
                action: Some(
                    wire::Action {
                        cancel: Some(wire::Cancel {
                            reason: "Host cancelled the turn".into(),
                        }),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                ),
                ..Default::default()
            };
            if let (Some(writer), Ok(payload)) =
                (&self.writer, connect::encode(&message.encode_to_vec()))
            {
                let _ = writer.try_send(Ok(payload));
            }
            self.cancellation.cancel();
            self.reader.take();
            self.writer.take();
            self.events.clear();
            self.deferred.clear();
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;

fn wire_fields(mut payload: &[u8]) -> Vec<u32> {
    let mut fields = Vec::new();
    while !payload.is_empty() {
        let Ok((tag, wire_type)) = prost::encoding::decode_key(&mut payload) else {
            break;
        };
        fields.push(tag);
        if prost::encoding::skip_field(
            wire_type,
            tag,
            &mut payload,
            prost::encoding::DecodeContext::default(),
        )
        .is_err()
        {
            break;
        }
    }
    fields
}
