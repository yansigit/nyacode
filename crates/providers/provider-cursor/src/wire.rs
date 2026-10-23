//! Wire subset from agent.v1; nested checkpoint messages stay opaque so new
//! server fields survive persistence and resume without a lossy decode/re-encode.
use std::collections::HashMap;

use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ClientMessage {
    #[prost(message, optional, tag = "1")]
    pub run: Option<RunRequest>,
    #[prost(message, optional, tag = "2")]
    pub exec: Option<ExecClient>,
    #[prost(message, optional, tag = "3")]
    pub kv: Option<KvClient>,
    #[prost(bytes = "vec", optional, tag = "4")]
    pub action: Option<Vec<u8>>,
    #[prost(message, optional, tag = "5")]
    pub exec_control: Option<ExecClientControl>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ExecClientControl {
    #[prost(message, optional, tag = "1")]
    pub stream_close: Option<ExecStreamClose>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ExecStreamClose {
    #[prost(uint32, tag = "1")]
    pub id: u32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ServerMessage {
    #[prost(message, optional, tag = "1")]
    pub interaction: Option<Interaction>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub exec: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub checkpoint: Option<Vec<u8>>,
    #[prost(message, optional, tag = "4")]
    pub kv: Option<KvServer>,
    #[prost(bytes = "vec", optional, tag = "5")]
    pub control: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub query: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RunRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub state: Vec<u8>,
    #[prost(message, optional, tag = "2")]
    pub action: Option<Action>,
    #[prost(message, optional, tag = "3")]
    pub model: Option<ModelDetails>,
    #[prost(message, optional, tag = "4")]
    pub tools: Option<McpTools>,
    #[prost(string, optional, tag = "5")]
    pub conversation_id: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub system: Option<String>,
    #[prost(bool, optional, tag = "12")]
    pub exclude_workspace_context: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct State {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub roots: Vec<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Action {
    #[prost(message, optional, tag = "1")]
    pub user: Option<UserAction>,
    #[prost(message, optional, tag = "2")]
    pub resume: Option<Resume>,
    #[prost(message, optional, tag = "3")]
    pub cancel: Option<Cancel>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct UserAction {
    #[prost(message, optional, tag = "1")]
    pub message: Option<UserMessage>,
    #[prost(message, optional, tag = "2")]
    pub context: Option<RequestContext>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct UserMessage {
    #[prost(string, tag = "1")]
    pub text: String,
    #[prost(string, tag = "2")]
    pub id: String,
    #[prost(message, optional, tag = "3")]
    pub selected_context: Option<SelectedContext>,
    #[prost(int32, tag = "4")]
    pub mode: i32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Resume {
    #[prost(message, optional, tag = "2")]
    pub context: Option<RequestContext>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Cancel {
    #[prost(string, tag = "1")]
    pub reason: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ModelDetails {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "3")]
    pub display_id: String,
    #[prost(string, tag = "4")]
    pub name: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ModelsResponse {
    #[prost(message, repeated, tag = "1")]
    pub models: Vec<ModelDetails>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct McpTools {
    #[prost(message, repeated, tag = "1")]
    pub tools: Vec<McpTool>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct McpTool {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub description: String,
    #[prost(bytes = "vec", tag = "3")]
    pub schema: Vec<u8>,
    #[prost(string, tag = "4")]
    pub provider: String,
    #[prost(string, tag = "5")]
    pub tool_name: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct RequestContext {
    #[prost(message, optional, tag = "4")]
    pub env: Option<Environment>,
    #[prost(message, repeated, tag = "7")]
    pub tools: Vec<McpTool>,
    #[prost(bool, optional, tag = "17")]
    pub web_search: Option<bool>,
    #[prost(bool, optional, tag = "24")]
    pub web_fetch: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Environment {
    #[prost(string, tag = "1")]
    pub os: String,
    #[prost(string, repeated, tag = "2")]
    pub workspaces: Vec<String>,
    #[prost(string, tag = "3")]
    pub shell: String,
    #[prost(bool, tag = "5")]
    pub sandbox: bool,
    #[prost(string, tag = "10")]
    pub timezone: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Interaction {
    #[prost(message, optional, tag = "1")]
    pub text: Option<Text>,
    #[prost(message, optional, tag = "4")]
    pub thinking: Option<Text>,
    #[prost(message, optional, tag = "14")]
    pub ended: Option<TurnEnded>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Text {
    #[prost(string, tag = "1")]
    pub text: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct TurnEnded {
    #[prost(int64, optional, tag = "1")]
    pub input: Option<i64>,
    #[prost(int64, optional, tag = "2")]
    pub output: Option<i64>,
    #[prost(int64, optional, tag = "3")]
    pub cache_read: Option<i64>,
    #[prost(int64, optional, tag = "5")]
    pub reasoning: Option<i64>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ExecServer {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(message, optional, tag = "2")]
    pub shell: Option<ShellArgs>,
    #[prost(message, optional, tag = "3")]
    pub write: Option<WriteArgs>,
    #[prost(message, optional, tag = "4")]
    pub delete: Option<PathArgs>,
    #[prost(message, optional, tag = "5")]
    pub grep: Option<GrepArgs>,
    #[prost(message, optional, tag = "8")]
    pub ls: Option<LsArgs>,
    #[prost(message, optional, tag = "7")]
    pub read: Option<ReadArgs>,
    #[prost(bytes = "vec", optional, tag = "10")]
    pub request_context: Option<Vec<u8>>,
    #[prost(message, optional, tag = "11")]
    pub mcp: Option<McpArgs>,
    #[prost(message, optional, tag = "14")]
    pub shell_stream: Option<ShellArgs>,
    #[prost(string, tag = "15")]
    pub exec_id: String,
    #[prost(message, optional, tag = "36")]
    pub mcp_state: Option<McpStateArgs>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct McpArgs {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(map = "string, bytes", tag = "2")]
    pub args: HashMap<String, Vec<u8>>,
    #[prost(string, tag = "3")]
    pub call_id: String,
    #[prost(string, tag = "4")]
    pub provider: String,
    #[prost(string, tag = "5")]
    pub tool_name: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ExecClient {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub shell: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub write: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "4")]
    pub delete: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "5")]
    pub grep: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "8")]
    pub ls: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub read: Option<Vec<u8>>,
    #[prost(message, optional, tag = "10")]
    pub request_context: Option<ContextResult>,
    #[prost(message, optional, tag = "11")]
    pub mcp: Option<McpResult>,
    #[prost(bytes = "vec", optional, tag = "14")]
    pub shell_stream: Option<Vec<u8>>,
    #[prost(string, tag = "15")]
    pub exec_id: String,
    #[prost(message, optional, tag = "36")]
    pub mcp_state: Option<McpStateResult>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ContextResult {
    #[prost(message, optional, tag = "1")]
    pub success: Option<ContextSuccess>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ContextSuccess {
    #[prost(message, optional, tag = "1")]
    pub context: Option<RequestContext>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct McpResult {
    #[prost(message, optional, tag = "1")]
    pub success: Option<McpSuccess>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct McpSuccess {
    #[prost(message, repeated, tag = "1")]
    pub content: Vec<McpContent>,
    #[prost(bool, tag = "2")]
    pub is_error: bool,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct McpContent {
    #[prost(message, optional, tag = "1")]
    pub text: Option<Text>,
    #[prost(message, optional, tag = "2")]
    pub image: Option<McpImage>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct McpImage {
    #[prost(bytes = "vec", tag = "1")]
    pub data: Vec<u8>,
    #[prost(string, tag = "2")]
    pub mime: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct KvServer {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(message, optional, tag = "2")]
    pub get: Option<GetBlob>,
    #[prost(message, optional, tag = "3")]
    pub set: Option<SetBlob>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct GetBlob {
    #[prost(bytes = "vec", tag = "1")]
    pub id: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct SetBlob {
    #[prost(bytes = "vec", tag = "1")]
    pub id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub data: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct KvClient {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(message, optional, tag = "2")]
    pub get: Option<BlobResult>,
    #[prost(message, optional, tag = "3")]
    pub set: Option<BlobAck>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct BlobResult {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub data: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct BlobAck {}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct McpStateArgs {
    #[prost(string, repeated, tag = "1")]
    pub identifiers: Vec<String>,
    #[prost(bool, tag = "2")]
    pub kick_only: bool,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct McpStateResult {
    #[prost(message, optional, tag = "1")]
    pub success: Option<McpStateSuccess>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct McpStateSuccess {
    #[prost(message, repeated, tag = "1")]
    pub servers: Vec<McpStateServer>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct McpStateServer {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub identifier: String,
    #[prost(message, repeated, tag = "5")]
    pub tools: Vec<McpTool>,
    #[prost(string, optional, tag = "7")]
    pub status: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct SelectedContext {
    #[prost(message, repeated, tag = "1")]
    pub images: Vec<SelectedImage>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct SelectedImage {
    #[prost(string, tag = "2")]
    pub uuid: String,
    #[prost(string, tag = "3")]
    pub path: String,
    #[prost(string, tag = "7")]
    pub mime: String,
    #[prost(message, optional, tag = "9")]
    pub blob: Option<ImageBlob>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ImageBlob {
    #[prost(bytes = "vec", tag = "1")]
    pub id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub data: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ShellArgs {
    #[prost(string, tag = "1")]
    pub command: String,
    #[prost(string, tag = "2")]
    pub cwd: String,
    #[prost(int32, tag = "3")]
    pub timeout: i32,
    #[prost(string, tag = "4")]
    pub call_id: String,
    #[prost(bool, tag = "11")]
    pub background: bool,
    #[prost(int32, optional, tag = "14")]
    pub hard_timeout: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct PathArgs {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub call_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ReadArgs {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub call_id: String,
    #[prost(int32, optional, tag = "4")]
    pub offset: Option<i32>,
    #[prost(uint32, optional, tag = "5")]
    pub limit: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct WriteArgs {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub text: String,
    #[prost(string, tag = "3")]
    pub call_id: String,
    #[prost(bytes = "vec", tag = "5")]
    pub bytes: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct GrepArgs {
    #[prost(string, tag = "1")]
    pub pattern: String,
    #[prost(string, optional, tag = "2")]
    pub path: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub glob: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub output_mode: Option<String>,
    #[prost(int32, optional, tag = "5")]
    pub context_before: Option<i32>,
    #[prost(int32, optional, tag = "6")]
    pub context_after: Option<i32>,
    #[prost(int32, optional, tag = "7")]
    pub context: Option<i32>,
    #[prost(bool, optional, tag = "8")]
    pub case_insensitive: Option<bool>,
    #[prost(string, optional, tag = "9")]
    pub file_type: Option<String>,
    #[prost(int32, optional, tag = "10")]
    pub head_limit: Option<i32>,
    #[prost(bool, optional, tag = "11")]
    pub multiline: Option<bool>,
    #[prost(string, optional, tag = "12")]
    pub sort: Option<String>,
    #[prost(bool, optional, tag = "13")]
    pub sort_ascending: Option<bool>,
    #[prost(string, tag = "14")]
    pub call_id: String,
    #[prost(int32, optional, tag = "16")]
    pub offset: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct LsArgs {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, repeated, tag = "2")]
    pub ignore: Vec<String>,
    #[prost(string, tag = "3")]
    pub call_id: String,
}
