//! Compatibility boundary between native providers and Codex host types.
//! Provider implementations and runtime never depend on host crates.
pub mod backend;
mod host_operations;
pub mod session;

pub use codex_provider_api as api;
pub use codex_provider_api::CompletionDisposition;
pub use codex_provider_api::HostCompletion;
pub use codex_provider_auth as auth;
pub use codex_provider_registry as registry;
pub use codex_provider_runtime::PoolStatus;
pub use codex_provider_runtime::PoolUpdate;

/// Host-side controls for a bidirectional inference session.
#[derive(Clone)]
pub struct SessionControl {
    pub results: tokio::sync::mpsc::Sender<HostCompletion>,
    pub capabilities: backend::SessionCapabilities,
}
