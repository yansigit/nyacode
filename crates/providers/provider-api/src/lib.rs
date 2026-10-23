//! Native inference boundary. Providers never execute host operations themselves.
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

mod pool;
pub use pool::*;

pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
pub type Result<T> = std::result::Result<T, ProviderError>;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("{0}")]
    Classified(Failure),
    #[error("provider authentication required: {0}")]
    Authentication(String),
    #[error("provider protocol error: {0}")]
    Protocol(String),
    #[error("provider transport error: {0}")]
    Transport(String),
    #[error("unsupported provider operation: {0}")]
    Unsupported(String),
    #[error("provider operation cancelled")]
    Cancelled,
    #[error("provider checkpoint error: {0}")]
    Checkpoint(String),
    #[error("host operation failed: {0}")]
    Host(String),
}

/// These identities must all match before opaque state may be replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Namespace {
    pub provider: String,
    pub account: String,
    pub model: String,
    pub thread: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub version: u32,
    pub namespace: Namespace,
    /// Hash of the exact conversation prefix to which this state belongs.
    pub context_hash: String,
    pub state: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
    },
    Image {
        url: String,
    },
    Thinking {
        text: String,
        signature: Option<String>,
    },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    /// Provider-specific replay material, never forwarded to another provider.
    Opaque {
        namespace: Namespace,
        value: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub output: Value,
    pub is_error: bool,
}

/// Outcome supplied by the host after its ordinary persistence path completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionDisposition {
    Success,
    Failure,
    Denied,
}

/// Internal completion envelope. The recorded history output is independent of
/// structured execution metadata and cannot collide with provider output keys.
#[derive(Debug, Clone, PartialEq)]
pub struct HostCompletion {
    pub call_id: String,
    pub output: Value,
    pub structured: Option<Value>,
    pub disposition: CompletionDisposition,
}

impl HostCompletion {
    pub fn is_error(&self) -> bool {
        self.disposition != CompletionDisposition::Success
    }

    pub fn to_tool_result(&self) -> ToolResult {
        ToolResult {
            call_id: self.call_id.clone(),
            output: self.output.clone(),
            is_error: self.is_error(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RecoveryPolicy {
    #[default]
    RestorePrevious,
    RebuildFromHistory,
}

/// Provider-neutral operations translated by the host's sandboxed tool adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum HostOperation {
    Shell(Value),
    ReadFile(Value),
    WriteFile(Value),
    DeleteFile(Value),
    ListDirectory(Value),
    Search(Value),
}

impl HostOperation {
    pub fn arguments(&self) -> &Value {
        match self {
            Self::Shell(args)
            | Self::ReadFile(args)
            | Self::WriteFile(args)
            | Self::DeleteFile(args)
            | Self::ListDirectory(args)
            | Self::Search(args) => args,
        }
    }
}

/// A provider owns protocol operation names and result shapes. This mapping
/// performs no host I/O; execution always returns through the host boundary.
pub trait NativeOperations: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn operation(&self, call: &ToolCall) -> Result<HostOperation>;
    fn result(&self, call: &ToolCall, structured: &Value) -> Result<Value>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub namespace: Namespace,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub parallel_tool_calls: bool,
    pub max_output_tokens: Option<u64>,
    pub reasoning_effort: Option<String>,
    pub output_schema: Option<Value>,
    pub checkpoint: Option<Checkpoint>,
    /// Host extension data. `host_checkpoint_prefix`, when present, is the count
    /// of messages the host verified against its durable checkpoint history.
    /// Only the trusted host may set it; providers may then restore their exact
    /// native transcript despite differences in tool presentation/order.
    pub metadata: Value,
}

/// Credential material deliberately has no Debug/Serialize implementation.
#[derive(Clone)]
pub struct Credentials {
    pub access_token: String,
    pub account_id: String,
    pub metadata: Value,
}

#[derive(Clone)]
pub struct Context {
    pub credentials: Credentials,
    pub cancellation: CancellationToken,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub context_window: Option<u64>,
    /// A known capability; `None` retains the host's fallback model metadata.
    #[serde(default)]
    pub supports_images: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    TextDelta(String),
    ThinkingDelta(String),
    ToolCall(ToolCall),
    /// Replay material associated with generated output (including signatures).
    Checkpoint(Checkpoint),
    Usage(Usage),
    /// All results have been accepted locally; no next HTTP request was sent.
    ContinuationReady,
    AccountChanged {
        previous: String,
        account: String,
        namespace: Namespace,
        prior_journals: Vec<Namespace>,
    },
    Completed {
        id: String,
    },
}

/// A session remains alive across tool requests and tool-result submission.
/// `next_event` must be cancellation-safe. EOF without Completed is an error.
pub trait ProviderSession: Send {
    fn next_event(&mut self) -> ProviderFuture<'_, Option<Event>>;
    fn submit_tool_result(&mut self, result: ToolResult) -> ProviderFuture<'_, ()>;
    fn cancel(&mut self) -> ProviderFuture<'_, ()>;
    /// HTTP rounds may only dispatch after the runtime authorizes continuation.
    fn continue_round(&mut self) -> ProviderFuture<'_, ()> {
        Box::pin(async { Err(ProviderError::Unsupported("explicit continuation".into())) })
    }
    /// Only a conclusively rejected request may install rotated credentials and retry.
    fn retry_authentication(&mut self, _credentials: Credentials) -> ProviderFuture<'_, ()> {
        Box::pin(async {
            Err(ProviderError::Unsupported(
                "authentication retry at this boundary".into(),
            ))
        })
    }
    fn continuation_request(&self) -> Result<Request> {
        Err(ProviderError::Unsupported(
            "provider has no portable continuation boundary".into(),
        ))
    }
    fn authorize_continuation(
        &mut self,
        _history: Option<HistoryAcknowledgement>,
    ) -> ProviderFuture<'_, ()> {
        self.continue_round()
    }
}

pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy::RestorePrevious
    }
    fn native_operations(&self) -> Option<Arc<dyn NativeOperations>> {
        None
    }
    /// Default deny: registration of a transport does not establish portability.
    fn handoff_request(
        &self,
        _request: &Request,
        _boundary: ContinuationBoundary,
    ) -> Result<Request> {
        Err(ProviderError::Unsupported(
            "account handoff has no verified history boundary".into(),
        ))
    }
    fn models(&self, context: Context) -> ProviderFuture<'_, Vec<Model>>;
    fn start(
        &self,
        request: Request,
        context: Context,
    ) -> ProviderFuture<'_, Box<dyn ProviderSession>>;
}

/// Host execution boundary. This is the only route to filesystem, shell and MCP.
/// Execution must preserve approvals, sandbox, hooks and normal tool presentation.
pub trait Host: Send + Sync {
    /// Durably journal the call before execution and its result before returning.
    /// Repeated IDs with identical arguments return the stored result. An ID
    /// left pending by interrupted execution must fail instead of running again.
    fn execute(
        &self,
        call: ToolCall,
        cancellation: CancellationToken,
    ) -> ProviderFuture<'_, ToolResult>;
    /// Persist a checkpoint before acknowledging its receipt.
    fn checkpoint(&self, checkpoint: Checkpoint) -> ProviderFuture<'_, ()>;
    fn persisted_history(&self) -> ProviderFuture<'_, Option<HistoryAcknowledgement>> {
        Box::pin(async { Ok(None) })
    }
    fn account_changed(
        &self,
        _namespace: Namespace,
        _prior_journals: Vec<Namespace>,
    ) -> ProviderFuture<'_, ()> {
        Box::pin(async {
            Err(ProviderError::Unsupported(
                "host cannot change account namespace".into(),
            ))
        })
    }
}
