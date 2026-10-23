//! Native ChatGPT transport research. No Codex inference or browser fallback.
//!
//! Explicit probes keep integrated delivery gated until native conversation
//! preparation and every promised upload category pass live acceptance.

pub mod attachment;
pub mod content;
pub mod conversation;
pub mod journal;
pub mod models;
mod native_preparation;
mod pdf;
mod preparation;
mod probe;
mod report;
mod send;
mod sentinel;
pub mod session;
pub mod stream;
mod temporary;
pub mod temporary_session;
pub mod upload;

pub use native_preparation::PreparationProtocol;
pub use preparation::PreparationObservation;
pub use preparation::PreparationOperation;
pub use probe::ChatProbe;
pub use probe::ProbeTransport;
pub use report::AttachmentObservation;
pub use report::Capability;
pub use report::CapabilityStatus;
pub use report::LifecycleRpcObservation;
pub use report::ProbeReport;
pub use report::RpcAttachmentObservation;
pub use report::RpcObservation;
pub use report::ServiceObservation;
pub use report::TemporaryRpcObservation;
pub use report::WriteScenario;
pub use send::SavedSendObservation;
pub use sentinel::SentinelFailure;
pub use temporary::TemporaryObservation;
pub use temporary::TemporaryProtocol;

/// Errors contain no server response bodies, URLs, identities, or credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum ChatError {
    #[error("ChatGPT Chat is disabled")]
    Disabled,
    #[error("a managed ChatGPT login is required")]
    LoginRequired,
    #[error("ChatGPT authentication expired or refresh failed")]
    Authentication,
    #[error("the authenticated account or workspace changed")]
    IdentityChanged,
    #[error("the authentication generation changed; preparation must start in a new session")]
    SessionChanged,
    #[error("the selected account does not match the designated account")]
    AccountMismatch,
    #[error("Cloudflare requires a challenge; this native run has stopped")]
    CloudflareChallenge,
    #[error("the server requires an unsupported native Sentinel challenge")]
    UnsupportedChallenge,
    #[error("the server denied access")]
    AccessDenied,
    #[error("the server rate limited the operation")]
    RateLimited,
    #[error("preparation is cooling down after three consecutive failures")]
    PreparationCooldown,
    #[error("another preparation operation is running in this session")]
    PreparationInProgress,
    #[error("the conversation was not found")]
    NotFound,
    #[error("this capability is unsupported")]
    UnsupportedCapability,
    #[error("the native backend protocol did not match the expected shape")]
    ProtocolChanged,
    #[error("the server response exceeded the probe's size limit")]
    ResponseTooLarge,
    #[error("the application network policy denied the operation")]
    NetworkPolicy,
    #[error("the connection failed")]
    Transport,
    #[error("the operation timed out")]
    Timeout,
    #[error("the local operation stopped; server cancellation is not confirmed")]
    LocalCancellation,
    #[error("invalid probe input")]
    InvalidInput,
    #[error("private saved-run recovery storage is unavailable or inconsistent")]
    RecoveryUnavailable,
    #[error("the submission is pending reconciliation and cannot be resent")]
    PendingSubmission,
    #[error("the authoritative conversation parent changed")]
    BranchConflict,
    #[error("the response stream was malformed or incomplete")]
    IncompleteStream,
    #[error("the attachment upload or processing failed")]
    UploadFailed,
    #[error("the attachment did not finish processing")]
    ProcessingIncomplete,
    #[error("the upload handle is expired or belongs to a different draft or session")]
    StaleUpload,
}
