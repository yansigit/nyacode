use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;
use uuid::Uuid;

pub const MAX_CHAT_TEXT_BYTES: usize = 1024 * 1024;
pub const MAX_CHAT_SELECTIONS: usize = 20;
pub const MAX_CHAT_SUBMISSION_BYTES: u64 = 32 * 1024 * 1024;
pub(super) const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
pub(super) const LEASE_MS: i64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChatStoreError {
    #[error("saved Chat recovery storage is unavailable")]
    Unavailable,
    #[error("invalid saved Chat request")]
    InvalidInput,
    #[error("saved Chat recovery data is invalid or from an unsupported version")]
    InvalidRecord,
    #[error("the draft changed; retain the local draft and refresh its revision")]
    RevisionConflict,
    #[error("the saved draft or submission was not found in this account")]
    NotFound,
    #[error("this draft or conversation already has a pending submission")]
    PendingSubmission,
    #[error("this submission or client-message ID was already used; reconcile it instead")]
    DuplicateSubmission,
    #[error("another process owns the submission, or this operation's lease expired")]
    LeaseLost,
    #[error("the submission can only be reconciled; it cannot be transmitted again")]
    ReconcileOnly,
    #[error("the submission outcome conflicts with its recorded state")]
    InvalidTransition,
    #[error("the saved native session profile requires a compatible adapter")]
    ProfileMismatch,
    #[error("too many saved Chat submissions are pending reconciliation")]
    Capacity,
}

impl From<sqlx::Error> for ChatStoreError {
    fn from(_: sqlx::Error) -> Self {
        Self::Unavailable
    }
}

/// Construct only from managed authentication and the resolved workspace,
/// never from client-declared identity fields. Empty workspace denotes none.
#[derive(Clone, PartialEq, Eq)]
pub struct ChatScope {
    pub(super) principal: String,
    pub(super) account: String,
    pub(super) workspace: String,
}

impl ChatScope {
    pub fn new(
        principal: String,
        account: String,
        workspace: Option<String>,
    ) -> Result<Self, ChatStoreError> {
        for value in [&principal, &account].into_iter().chain(workspace.iter()) {
            if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
                return Err(ChatStoreError::InvalidInput);
            }
        }
        Ok(Self {
            principal,
            account,
            workspace: workspace.unwrap_or_default(),
        })
    }
}

pub struct ChatProfile {
    pub device_id: Uuid,
    pub version: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatFileKind {
    Png,
    Jpeg,
    Webp,
    Pdf,
    Utf8,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatFileSnapshot {
    pub sha256: String,
    pub size: u64,
    pub modified_at_ms: Option<i64>,
    pub kind: ChatFileKind,
    pub dimensions: Option<(u32, u32)>,
}

impl ChatFileSnapshot {
    pub(super) fn validate(&self) -> Result<(), ChatStoreError> {
        if self.sha256.len() != 64
            || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || self.size > MAX_CHAT_SUBMISSION_BYTES
            || self.dimensions.is_some_and(|(w, h)| w == 0 || h == 0)
        {
            return Err(ChatStoreError::InvalidInput);
        }
        Ok(())
    }
}

/// None source marks a clipboard selection that needs reattachment after a
/// restart. Neither clipboard bytes nor a temporary-file fallback are stored.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatFileSelection {
    pub id: Uuid,
    pub source: Option<PathBuf>,
    pub name: String,
    pub snapshot: Option<ChatFileSnapshot>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatDraftContent {
    pub conversation_id: Option<String>,
    pub expected_parent_id: Option<String>,
    pub requested_model: Option<String>,
    pub text: String,
    pub selections: Vec<ChatFileSelection>,
}

impl ChatDraftContent {
    /// Pure validation shared by saved storage and the separate memory-only
    /// temporary owner. This method performs no database or filesystem access.
    pub fn validate(&self) -> Result<(), ChatStoreError> {
        for id in [
            &self.conversation_id,
            &self.expected_parent_id,
            &self.requested_model,
        ]
        .into_iter()
        .flatten()
        {
            validate_id(id)?;
        }
        if self.conversation_id.is_some() && self.expected_parent_id.is_none() {
            return Err(ChatStoreError::InvalidInput);
        }
        if self.text.len() > MAX_CHAT_TEXT_BYTES || self.selections.len() > MAX_CHAT_SELECTIONS {
            return Err(ChatStoreError::InvalidInput);
        }
        let mut ids = HashSet::new();
        let mut size = 0;
        for selection in &self.selections {
            if !ids.insert(selection.id)
                || selection.name.is_empty()
                || selection.name.len() > 1024
                || selection.name.chars().any(char::is_control)
                || selection.source.as_ref().is_some_and(|path| {
                    !path.is_absolute()
                        || path.as_os_str().len() > 16 * 1024
                        || path.as_os_str().as_encoded_bytes().contains(&0)
                })
            {
                return Err(ChatStoreError::InvalidInput);
            }
            if let Some(snapshot) = &selection.snapshot {
                snapshot.validate()?;
                size += snapshot.size;
            }
        }
        if size > MAX_CHAT_SUBMISSION_BYTES {
            return Err(ChatStoreError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct ChatDraft {
    pub id: Uuid,
    pub revision: Uuid,
    pub content: ChatDraftContent,
    pub updated_at_ms: i64,
}

pub struct ChatDraftSummary {
    pub id: Uuid,
    pub revision: Uuid,
    pub conversation_id: Option<String>,
    pub requested_model: Option<String>,
    pub preview: String,
    pub attachment_count: usize,
    pub updated_at_ms: i64,
}

#[derive(Clone, Copy)]
pub struct ChatPageCursor {
    pub updated_at_ms: i64,
    pub id: Uuid,
}

pub struct ChatDraftPage {
    pub items: Vec<ChatDraftSummary>,
    pub next: Option<ChatPageCursor>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatUploadedFile {
    pub selection_id: Uuid,
    pub server_file_id: String,
    pub snapshot: ChatFileSnapshot,
}

pub struct ChatSubmissionRequest {
    pub submission_id: Uuid,
    pub client_message_id: Uuid,
    pub draft_id: Uuid,
    pub expected_revision: Uuid,
    pub expected_parent_id: String,
    /// The selected model, including the resolved backend default for a draft
    /// that did not explicitly select a model.
    pub requested_model: String,
    pub files: Vec<ChatUploadedFile>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatSubmissionSnapshot {
    pub(super) version: u32,
    pub draft: ChatDraftContent,
    pub requested_model: String,
    pub files: Vec<ChatUploadedFile>,
}

impl ChatSubmissionSnapshot {
    pub(super) fn validate(&self) -> Result<(), ChatStoreError> {
        self.draft.validate()?;
        validate_id(&self.requested_model)?;
        if self.version != 1
            || (self.draft.text.is_empty() && self.draft.selections.is_empty())
            || self.files.len() != self.draft.selections.len()
            || self
                .draft
                .requested_model
                .as_ref()
                .is_some_and(|m| m != &self.requested_model)
        {
            return Err(ChatStoreError::InvalidInput);
        }
        let mut files = HashSet::new();
        let mut selections = HashSet::new();
        let mut size = 0;
        for file in &self.files {
            validate_id(&file.server_file_id)?;
            file.snapshot.validate()?;
            if !files.insert(&file.server_file_id) || !selections.insert(file.selection_id) {
                return Err(ChatStoreError::InvalidInput);
            }
            let Some(selected) = self
                .draft
                .selections
                .iter()
                .find(|s| s.id == file.selection_id)
            else {
                return Err(ChatStoreError::InvalidInput);
            };
            if selected
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot != &file.snapshot)
            {
                return Err(ChatStoreError::InvalidInput);
            }
            size += file.snapshot.size;
        }
        if size > MAX_CHAT_SUBMISSION_BYTES {
            return Err(ChatStoreError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatSubmissionOutcome {
    InProgress,
    Completed,
    Failed,
    LocalStopped,
    UnsupportedHandoff,
}

impl ChatSubmissionOutcome {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::LocalStopped => "local_stopped",
            Self::UnsupportedHandoff => "unsupported_handoff",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, ChatStoreError> {
        match value {
            "in_progress" => Ok(Self::InProgress),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "local_stopped" => Ok(Self::LocalStopped),
            "unsupported_handoff" => Ok(Self::UnsupportedHandoff),
            _ => Err(ChatStoreError::InvalidRecord),
        }
    }
}

pub struct ChatSubmission {
    pub submission_id: Uuid,
    pub client_message_id: Uuid,
    pub draft_id: Uuid,
    pub draft_revision: Uuid,
    pub conversation_id: Option<String>,
    pub expected_parent_id: String,
    pub snapshot: ChatSubmissionSnapshot,
    pub transmission_started: bool,
    pub accepted: bool,
    pub resolved_model: Option<String>,
    pub outcome: ChatSubmissionOutcome,
    /// Unknown acceptance after transmission stays active, even after local
    /// cancellation or failure. Absence from history never clears this flag.
    pub active: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// A fresh fencing token for one owner/authentication generation. Do not derive
/// Clone or Serialize: this is an in-process operation capability, not an RPC.
pub struct ChatSubmissionLease {
    pub(super) submission_id: Uuid,
    pub(super) lease_id: Uuid,
}

impl ChatSubmissionLease {
    pub fn submission_id(&self) -> Uuid {
        self.submission_id
    }
}

/// Returned once, only after the transmission-started marker has durably
/// committed. The native send adapter consumes it before issuing its one POST.
pub struct ChatTransmissionPermit {
    pub(super) submission_id: Uuid,
}

impl ChatTransmissionPermit {
    pub fn submission_id(&self) -> Uuid {
        self.submission_id
    }
}

/// Exact backend message identifiers, obtained from the native stream or an
/// authoritative branch read. Similar text and history absence are not proof.
pub struct ChatAcceptanceProof {
    pub conversation_id: String,
    pub client_message_id: Uuid,
    pub parent_message_id: String,
}

pub(super) fn validate_id(value: &str) -> Result<(), ChatStoreError> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
    {
        return Err(ChatStoreError::InvalidInput);
    }
    Ok(())
}

pub(super) fn encode<T: Serialize>(value: &T) -> Result<String, ChatStoreError> {
    let encoded = serde_json::to_string(value).map_err(|_| ChatStoreError::InvalidInput)?;
    if encoded.len() > MAX_RECORD_BYTES {
        return Err(ChatStoreError::InvalidInput);
    }
    Ok(encoded)
}

pub(super) fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, ChatStoreError> {
    if value.len() > MAX_RECORD_BYTES {
        return Err(ChatStoreError::InvalidRecord);
    }
    serde_json::from_str(value).map_err(|_| ChatStoreError::InvalidRecord)
}
