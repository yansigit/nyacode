use super::ServiceError;
use codex_chatgpt_chat::stream::Acceptance;
use codex_chatgpt_chat::stream::GenerationOutcome;
use codex_chatgpt_chat::stream::Message;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Persistence {
    Saved,
    Temporary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationPhase {
    Preparing,
    Sending,
    Finished,
}

/// A bounded latest-value stream. Consumers use scope/operation/sequence to
/// resynchronize after reconnect; intermediate rendering updates may coalesce.
#[derive(Clone)]
pub struct OperationSnapshot {
    pub scope_id: Uuid,
    pub persistence: Persistence,
    pub operation_id: Uuid,
    pub draft_id: Uuid,
    pub submission_id: Uuid,
    pub client_message_id: Uuid,
    pub conversation_id: Option<String>,
    pub sequence: u64,
    pub phase: OperationPhase,
    pub acceptance: Acceptance,
    pub outcome: GenerationOutcome,
    pub requested_model: Option<String>,
    pub resolved_model: Option<String>,
    pub messages: Vec<Message>,
    pub error: Option<ServiceError>,
    pub recovery_pending: bool,
}

#[derive(Clone)]
pub struct Operation {
    pub(super) updates: Arc<watch::Sender<Arc<OperationSnapshot>>>,
    pub(super) cancel: CancellationToken,
    pub(super) target: String,
}

impl Operation {
    pub fn id(&self) -> Uuid {
        self.updates.borrow().operation_id
    }
    /// Delivery must recheck the owning session, including after queue waits.
    pub fn subscribe(&self) -> watch::Receiver<Arc<OperationSnapshot>> {
        self.updates.subscribe()
    }
    pub fn snapshot(&self) -> Arc<OperationSnapshot> {
        Arc::clone(&self.updates.borrow())
    }

    pub(super) fn update(&self, change: impl FnOnce(&mut OperationSnapshot)) {
        self.updates.send_modify(|current| {
            let snapshot = Arc::make_mut(current);
            snapshot.sequence += 1;
            change(snapshot);
        });
    }
}
