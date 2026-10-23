//! Experimental native Chat APIs. Chat scopes are server-issued generations.
//! All content-bearing Debug implementations are redacted, including nested
//! message parts and clipboard payloads. These types never select connection trust.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

macro_rules! redacted_debug {
    ($($ty:ident),* $(,)?) => { $(impl std::fmt::Debug for $ty {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct(stringify!($ty)).finish_non_exhaustive()
        }
    })* };
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ChatPersistence {
    Saved,
    Temporary,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ChatAvailability {
    Available,
    Disabled,
    LoginRequired,
    LocalConnectionRequired,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ChatAcceptance {
    Unknown,
    Accepted,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ChatTurnPhase {
    Preparing,
    Sending,
    Finished,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ChatGenerationOutcome {
    Incomplete,
    Completed,
    Failed,
    UnsupportedHandoff,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ChatContinuationBlocker {
    ReadOnly,
    CustomGpt,
    Temporary,
    UnsupportedMode,
    EmptyConversation,
    IncompleteGeneration,
    UnsupportedContent,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ChatAttachmentKind {
    Png,
    Jpeg,
    Webp,
    Pdf,
    Utf8,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ChatAttachmentStage {
    Selected,
    Allocating,
    Uploading,
    Processing,
    Ready,
    Failed,
    LocalStopped,
}

/// Closed, content-free errors. Unknown backend values never become diagnostics.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ChatErrorCode {
    Disabled,
    LoginRequired,
    Authentication,
    IdentityChanged,
    SessionChanged,
    AccountMismatch,
    CloudflareChallenge,
    UnsupportedChallenge,
    AccessDenied,
    RateLimited,
    PreparationCooldown,
    PreparationInProgress,
    NotFound,
    UnsupportedCapability,
    ProtocolChanged,
    ResponseTooLarge,
    NetworkPolicy,
    Transport,
    Timeout,
    LocalCancellation,
    InvalidInput,
    RecoveryUnavailable,
    PendingSubmission,
    BranchConflict,
    IncompleteStream,
    UploadFailed,
    ProcessingIncomplete,
    StaleUpload,
    RemoteConnection,
    ScopeChanged,
    OverlappingTurn,
    Capacity,
    RevisionConflict,
    DuplicateSubmission,
    LeaseLost,
    ReconcileOnly,
    InvalidTransition,
    ProfileMismatch,
    InvalidRecord,
    AttachmentUnreadable,
    AttachmentChanged,
    AttachmentInvalidFormat,
    AttachmentLimit,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatErrorDetails {
    pub code: ChatErrorCode,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatCapabilities {
    pub saved: bool,
    pub temporary: bool,
    pub history: bool,
    pub attachments: Vec<ChatAttachmentKind>,
    pub server_cancellation: bool,
    #[ts(type = "number")]
    pub max_submission_bytes: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatModel {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// Catalog visibility does not establish that the backend serves this model.
    pub send_verified: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatModelLimit {
    pub model: String,
    #[ts(type = "number | null")]
    pub resets_at_ms: Option<i64>,
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatStatusReadParams {}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatStatusSnapshot {
    #[ts(type = "number")]
    pub generation: u64,
    pub availability: ChatAvailability,
    pub scope_id: Option<String>,
    pub capabilities: ChatCapabilities,
    /// None means quota information has not been supplied by the backend.
    pub model_limits: Option<Vec<ChatModelLimit>>,
    pub turns: Vec<ChatTurnSnapshot>,
    pub uploads: Vec<ChatAttachmentSnapshot>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatStatusReadResponse {
    #[ts(type = "number")]
    pub generation: u64,
    pub availability: ChatAvailability,
    pub scope_id: Option<String>,
    pub capabilities: ChatCapabilities,
    /// None means quota information has not been supplied by the backend.
    pub model_limits: Option<Vec<ChatModelLimit>>,
    pub turns: Vec<ChatTurnSnapshot>,
    pub uploads: Vec<ChatAttachmentSnapshot>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatModelListParams {
    pub scope_id: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatModelListResponse {
    pub scope_id: String,
    pub models: Vec<ChatModel>,
    pub default_model: String,
    pub model_limits: Option<Vec<ChatModelLimit>>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatConversationListParams {
    pub scope_id: String,
    #[ts(optional = nullable)]
    pub offset: Option<u32>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatConversationSummary {
    pub id: String,
    pub title: String,
    #[ts(type = "number | null")]
    pub updated_at_ms: Option<i64>,
    pub archived: bool,
    pub custom_gpt: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatConversationListResponse {
    pub scope_id: String,
    pub items: Vec<ChatConversationSummary>,
    pub next_offset: Option<u32>,
    #[ts(type = "number | null")]
    pub total: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatConversationReadParams {
    pub scope_id: String,
    pub conversation_id: String,
    pub persistence: ChatPersistence,
    /// Explicit reconciliation of this stable saved submission, never a resend.
    #[ts(optional = nullable)]
    pub submission_id: Option<String>,
    /// Read the saved local display cache without contacting ChatGPT. Cached
    /// branches cannot establish acceptance or enable continuation.
    #[ts(optional = nullable)]
    pub cache_only: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ChatConversationSource {
    Authoritative,
    LocalCache,
    TemporaryMemory,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatAttachmentReference {
    pub file_id: String,
    pub name: Option<String>,
    pub mime_type: Option<String>,
    #[ts(type = "number | null")]
    pub size_bytes: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
#[ts(tag = "type", rename_all = "camelCase", export_to = "v2/")]
pub enum ChatMessagePart {
    Text {
        text: String,
    },
    Image {
        reference: ChatAttachmentReference,
    },
    Unsupported {
        #[serde(rename = "contentType")]
        #[ts(rename = "contentType")]
        content_type: String,
    },
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatCitation {
    pub marker: String,
    pub urls: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatMessage {
    pub id: String,
    pub role: String,
    pub parts: Vec<ChatMessagePart>,
    pub model: Option<String>,
    pub finished: bool,
    pub end_turn: bool,
    pub attachments: Vec<ChatAttachmentReference>,
    pub citations: Vec<ChatCitation>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatConversationNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub message: Option<ChatMessage>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatConversationReadResponse {
    pub scope_id: String,
    pub conversation_id: String,
    pub persistence: ChatPersistence,
    pub source: ChatConversationSource,
    /// Request start time for a local cached snapshot; null for other sources.
    #[ts(type = "number | null")]
    pub observed_at_ms: Option<i64>,
    pub title: String,
    #[ts(type = "number | null")]
    pub updated_at_ms: Option<i64>,
    pub current_node: Option<String>,
    pub nodes: Vec<ChatConversationNode>,
    pub other_branch_heads: Vec<String>,
    pub default_model: Option<String>,
    pub continuation_blocker: Option<ChatContinuationBlocker>,
    pub pending_submission: Option<ChatPendingSubmission>,
    pub recovery_available: bool,
    pub reconciliation_error: Option<ChatErrorCode>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatFileSnapshot {
    pub sha256: String,
    #[ts(type = "number")]
    pub size: u64,
    #[ts(type = "number | null")]
    pub modified_at_ms: Option<i64>,
    pub kind: ChatAttachmentKind,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatFileSelection {
    pub id: String,
    /// Explicit absolute local file path. None is a clipboard selection, which
    /// requires reattachment after restart; bytes never appear in draft storage.
    pub source: Option<String>,
    pub name: String,
    pub snapshot: Option<ChatFileSnapshot>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatDraftContent {
    pub conversation_id: Option<String>,
    pub expected_parent_id: Option<String>,
    pub requested_model: Option<String>,
    pub text: String,
    pub selections: Vec<ChatFileSelection>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatDraft {
    pub id: String,
    pub revision: String,
    pub persistence: ChatPersistence,
    pub content: ChatDraftContent,
    #[ts(type = "number")]
    pub updated_at_ms: i64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatDraftSummary {
    pub id: String,
    pub revision: String,
    pub persistence: ChatPersistence,
    pub conversation_id: Option<String>,
    pub requested_model: Option<String>,
    pub preview: String,
    pub attachment_count: u32,
    #[ts(type = "number")]
    pub updated_at_ms: i64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatDraftCursor {
    pub id: String,
    #[ts(type = "number")]
    pub updated_at_ms: i64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatDraftListParams {
    pub scope_id: String,
    pub persistence: ChatPersistence,
    #[ts(optional = nullable)]
    pub before: Option<ChatDraftCursor>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatDraftListResponse {
    pub scope_id: String,
    pub items: Vec<ChatDraftSummary>,
    pub next: Option<ChatDraftCursor>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatDraftReadParams {
    pub scope_id: String,
    pub persistence: ChatPersistence,
    pub draft_id: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatDraftReadResponse {
    pub scope_id: String,
    pub draft: Option<ChatDraft>,
    pub pending_submission: Option<ChatPendingSubmission>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatDraftWriteParams {
    pub scope_id: String,
    pub persistence: ChatPersistence,
    pub draft_id: String,
    /// None creates only; every subsequent edit must compare the current revision.
    pub expected_revision: Option<String>,
    pub content: ChatDraftContent,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatDraftWriteResponse {
    pub scope_id: String,
    pub draft: ChatDraft,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatDraftDeleteParams {
    pub scope_id: String,
    pub persistence: ChatPersistence,
    pub draft_id: String,
    pub expected_revision: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatDraftDeleteResponse {
    pub scope_id: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
#[ts(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    export_to = "v2/"
)]
pub enum ChatAttachmentSource {
    LocalFile {
        selection_id: String,
        path: String,
    },
    ClipboardImage {
        selection_id: String,
        name: String,
        image_base64: String,
    },
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatAttachmentUploadParams {
    pub scope_id: String,
    pub persistence: ChatPersistence,
    pub draft_id: String,
    pub expected_revision: String,
    /// The complete immutable selection for this submit, bounded to 32 MiB.
    pub selections: Vec<ChatAttachmentSource>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatAttachmentUploadResponse {
    pub scope_id: String,
    pub operation_id: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatAttachmentSnapshot {
    pub scope_id: String,
    pub operation_id: String,
    pub draft_id: String,
    pub persistence: ChatPersistence,
    #[ts(type = "number")]
    pub sequence: u64,
    pub draft_revision: Option<String>,
    pub files: Vec<ChatAttachmentProgress>,
    pub finished: bool,
    pub error: Option<ChatErrorCode>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatAttachmentProgress {
    pub selection_id: String,
    pub handle: Option<String>,
    pub stage: ChatAttachmentStage,
    pub percent: Option<u8>,
    pub metadata: Option<ChatFileSnapshot>,
    pub error: Option<ChatErrorCode>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatAttachmentCancelParams {
    pub scope_id: String,
    pub operation_id: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatAttachmentCancelResponse {
    pub scope_id: String,
    pub upload: Option<ChatAttachmentSnapshot>,
    pub local_stop_requested: bool,
    pub server_cancellation_confirmed: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatTurnStartParams {
    pub scope_id: String,
    pub persistence: ChatPersistence,
    pub draft_id: String,
    pub expected_revision: String,
    pub submission_id: String,
    pub client_message_id: String,
    pub expected_parent_id: String,
    /// Must match the immutable saved draft revision supplied with this submit.
    pub text: String,
    pub upload_handles: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatTurnSnapshot {
    pub scope_id: String,
    pub operation_id: String,
    pub draft_id: String,
    pub submission_id: String,
    pub client_message_id: String,
    pub conversation_id: Option<String>,
    pub persistence: ChatPersistence,
    #[ts(type = "number")]
    pub sequence: u64,
    pub phase: ChatTurnPhase,
    pub acceptance: ChatAcceptance,
    pub outcome: ChatGenerationOutcome,
    pub requested_model: Option<String>,
    pub resolved_model: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub error: Option<ChatErrorCode>,
    pub recovery_pending: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatPendingSubmission {
    pub submission_id: String,
    pub client_message_id: String,
    pub draft_id: String,
    pub conversation_id: Option<String>,
    pub expected_parent_id: String,
    pub transmission_started: bool,
    pub accepted: bool,
    pub active: bool,
    /// The original submitted snapshot is preserved separately from new edits.
    pub content: ChatDraftContent,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatTurnStartResponse {
    pub scope_id: String,
    pub turn: ChatTurnSnapshot,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatTurnCancelParams {
    pub scope_id: String,
    pub operation_id: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatTurnCancelResponse {
    pub scope_id: String,
    pub turn: Option<ChatTurnSnapshot>,
    pub local_stop_requested: bool,
    pub server_cancellation_confirmed: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatAvailabilityNotification {
    pub status: ChatStatusSnapshot,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatMessageNotification {
    pub turn: ChatTurnSnapshot,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatAttachmentNotification {
    pub upload: ChatAttachmentSnapshot,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatConversationNotification {
    pub scope_id: String,
    pub operation_id: String,
    pub draft_id: String,
    pub conversation_id: String,
    pub persistence: ChatPersistence,
    #[ts(type = "number")]
    pub sequence: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct ChatTurnCompletedNotification {
    pub turn: ChatTurnSnapshot,
}

redacted_debug!(
    ChatModel,
    ChatModelLimit,
    ChatStatusReadParams,
    ChatStatusReadResponse,
    ChatStatusSnapshot,
    ChatModelListParams,
    ChatModelListResponse,
    ChatConversationListParams,
    ChatConversationSummary,
    ChatConversationListResponse,
    ChatConversationReadParams,
    ChatAttachmentReference,
    ChatMessagePart,
    ChatCitation,
    ChatMessage,
    ChatConversationNode,
    ChatConversationReadResponse,
    ChatFileSnapshot,
    ChatFileSelection,
    ChatDraftContent,
    ChatDraft,
    ChatDraftSummary,
    ChatDraftCursor,
    ChatDraftListParams,
    ChatDraftListResponse,
    ChatDraftReadParams,
    ChatDraftReadResponse,
    ChatDraftWriteParams,
    ChatDraftWriteResponse,
    ChatDraftDeleteParams,
    ChatDraftDeleteResponse,
    ChatAttachmentSource,
    ChatAttachmentUploadParams,
    ChatAttachmentUploadResponse,
    ChatAttachmentSnapshot,
    ChatAttachmentProgress,
    ChatAttachmentCancelParams,
    ChatAttachmentCancelResponse,
    ChatTurnStartParams,
    ChatTurnSnapshot,
    ChatPendingSubmission,
    ChatTurnStartResponse,
    ChatTurnCancelParams,
    ChatTurnCancelResponse,
    ChatAvailabilityNotification,
    ChatMessageNotification,
    ChatAttachmentNotification,
    ChatConversationNotification,
    ChatTurnCompletedNotification,
);

#[cfg(test)]
pub(crate) const CHAT_EXPERIMENTAL_TYPES: &[&str] = &[
    "ChatPersistence",
    "ChatAvailability",
    "ChatAcceptance",
    "ChatTurnPhase",
    "ChatGenerationOutcome",
    "ChatContinuationBlocker",
    "ChatAttachmentKind",
    "ChatAttachmentStage",
    "ChatErrorCode",
    "ChatErrorDetails",
    "ChatCapabilities",
    "ChatModel",
    "ChatModelLimit",
    "ChatStatusReadParams",
    "ChatStatusReadResponse",
    "ChatStatusSnapshot",
    "ChatModelListParams",
    "ChatModelListResponse",
    "ChatConversationListParams",
    "ChatConversationSummary",
    "ChatConversationListResponse",
    "ChatConversationReadParams",
    "ChatConversationSource",
    "ChatAttachmentReference",
    "ChatMessagePart",
    "ChatCitation",
    "ChatMessage",
    "ChatConversationNode",
    "ChatConversationReadResponse",
    "ChatFileSnapshot",
    "ChatFileSelection",
    "ChatDraftContent",
    "ChatDraft",
    "ChatDraftSummary",
    "ChatDraftCursor",
    "ChatDraftListParams",
    "ChatDraftListResponse",
    "ChatDraftReadParams",
    "ChatDraftReadResponse",
    "ChatDraftWriteParams",
    "ChatDraftWriteResponse",
    "ChatDraftDeleteParams",
    "ChatDraftDeleteResponse",
    "ChatAttachmentSource",
    "ChatAttachmentUploadParams",
    "ChatAttachmentUploadResponse",
    "ChatAttachmentSnapshot",
    "ChatAttachmentProgress",
    "ChatAttachmentCancelParams",
    "ChatAttachmentCancelResponse",
    "ChatTurnStartParams",
    "ChatTurnSnapshot",
    "ChatPendingSubmission",
    "ChatTurnStartResponse",
    "ChatTurnCancelParams",
    "ChatTurnCancelResponse",
    "ChatAvailabilityNotification",
    "ChatMessageNotification",
    "ChatAttachmentNotification",
    "ChatConversationNotification",
    "ChatTurnCompletedNotification",
];

impl From<ChatStatusSnapshot> for ChatStatusReadResponse {
    fn from(status: ChatStatusSnapshot) -> Self {
        Self {
            generation: status.generation,
            availability: status.availability,
            scope_id: status.scope_id,
            capabilities: status.capabilities,
            model_limits: status.model_limits,
            turns: status.turns,
            uploads: status.uploads,
        }
    }
}

/// Chat notifications opt into both runtime negotiation and schema filtering.
#[cfg(test)]
pub(crate) const CHAT_EXPERIMENTAL_NOTIFICATION_METHODS: &[&str] = &[
    "chat/availability/changed",
    "chat/message/updated",
    "chat/attachment/progress",
    "chat/conversation/updated",
    "chat/turn/completed",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientRequest;
    use crate::JSONRPCRequest;
    use crate::RequestId;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn chat_request_debug_omits_private_content_and_clipboard_bytes() {
        for (method, params) in [
            (
                "chat/draft/write",
                json!({"scopeId":"PRIVATE","persistence":"saved","draftId":"PRIVATE","content":{"text":"PRIVATE","selections":[{"id":"PRIVATE","source":"/PRIVATE","name":"PRIVATE"}]}}),
            ),
            (
                "chat/attachment/upload",
                json!({"scopeId":"PRIVATE","persistence":"saved","draftId":"PRIVATE","expectedRevision":"PRIVATE","selections":[{"type":"clipboardImage","selectionId":"PRIVATE","name":"PRIVATE","imageBase64":"PRIVATE"}]}),
            ),
        ] {
            let request = ClientRequest::try_from(JSONRPCRequest {
                id: RequestId::Integer(1),
                method: method.into(),
                params: Some(params),
                trace: None,
            })
            .unwrap();
            assert!(!format!("{request:?}").contains("PRIVATE"));
        }
        let error = ChatErrorDetails {
            code: ChatErrorCode::ScopeChanged,
        };
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            json!({"code":"scopeChanged"})
        );
    }
}
