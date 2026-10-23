use super::*;
use codex_chatgpt_chat::session::SavedTurnRequest;
use codex_chatgpt_chat::session::SavedTurnResult;
use codex_chatgpt_chat::session::TurnSink;
use codex_chatgpt_chat::stream::Acceptance;
use codex_chatgpt_chat::stream::GenerationOutcome;
use codex_chatgpt_chat::stream::TurnState;
use codex_state::chat::ChatAcceptanceProof;
use codex_state::chat::ChatDraft;
use codex_state::chat::ChatSubmission;
use codex_state::chat::ChatSubmissionLease;
use codex_state::chat::ChatSubmissionOutcome;
use codex_state::chat::ChatSubmissionRequest;
use std::time::Duration;
use tokio::sync::watch;

pub struct StartSavedTurn {
    pub draft_id: Uuid,
    pub revision: Uuid,
    pub submission_id: Uuid,
    pub client_message_id: Uuid,
    pub expected_parent_id: String,
    pub upload_handles: Vec<Uuid>,
}

impl SavedSession {
    pub fn operation(&self, id: Uuid) -> Result<Option<Operation>, ServiceError> {
        self.check()?;
        Ok(lock(&self.operations).get(&id).cloned())
    }

    pub fn operations(&self) -> Result<Vec<Arc<OperationSnapshot>>, ServiceError> {
        self.check()?;
        Ok(lock(&self.operations)
            .values()
            .map(Operation::snapshot)
            .collect())
    }

    /// Cancellation only stops local work. A finished operation's acceptance and
    /// outcome remain unchanged; native confirmed server cancellation is separate.
    pub fn cancel_turn(&self, id: Uuid) -> Result<Option<Arc<OperationSnapshot>>, ServiceError> {
        self.check()?;
        let operations = lock(&self.operations);
        let Some(operation) = operations.get(&id) else {
            return Ok(None);
        };
        operation.cancel.cancel();
        Ok(Some(operation.snapshot()))
    }

    /// RPC success means the operation was created, never backend acceptance.
    /// A task survives client disconnects and remains visible to other clients.
    pub async fn start_turn(
        self: &Arc<Self>,
        request: StartSavedTurn,
    ) -> Result<Operation, ServiceError> {
        self.check()?;
        let draft = self
            .draft_read(request.draft_id)
            .await?
            .ok_or(ChatStoreError::NotFound)?;
        if draft.revision != request.revision {
            return Err(ChatStoreError::RevisionConflict.into());
        }
        if draft
            .content
            .expected_parent_id
            .as_ref()
            .is_some_and(|parent| parent != &request.expected_parent_id)
        {
            return Err(ChatError::BranchConflict.into());
        }
        if self.submission_read(request.submission_id).await?.is_some() {
            return Err(ChatStoreError::DuplicateSubmission.into());
        }
        // Validate handles now and again inside native preparation. Do not move
        // or discard selections on overlap, stale-handle, or journal failure.
        let _ = self.ready_uploads(&draft, &request.upload_handles)?;
        let target = draft
            .content
            .conversation_id
            .clone()
            .unwrap_or_else(|| format!("draft:{}", draft.id));
        let operation = Operation {
            updates: Arc::new(
                watch::channel(Arc::new(OperationSnapshot {
                    scope_id: self.scope_id,
                    persistence: Persistence::Saved,
                    operation_id: Uuid::new_v4(),
                    draft_id: draft.id,
                    submission_id: request.submission_id,
                    client_message_id: request.client_message_id,
                    conversation_id: draft.content.conversation_id.clone(),
                    sequence: 0,
                    phase: OperationPhase::Preparing,
                    acceptance: Acceptance::Unknown,
                    outcome: GenerationOutcome::Incomplete,
                    requested_model: draft.content.requested_model.clone(),
                    resolved_model: None,
                    messages: Vec::new(),
                    error: None,
                    recovery_pending: false,
                }))
                .0,
            ),
            cancel: self.stopped.child_token(),
            target,
        };
        {
            let mut operations = lock(&self.operations);
            self.check()?;
            if operations.values().any(|op| {
                let snapshot = op.snapshot();
                snapshot.phase != OperationPhase::Finished
                    && (op.target == operation.target
                        || snapshot.draft_id == draft.id
                        || snapshot.submission_id == request.submission_id
                        || snapshot.client_message_id == request.client_message_id)
            }) {
                return Err(ServiceError::OverlappingTurn);
            }
            if operations
                .values()
                .filter(|op| op.snapshot().phase != OperationPhase::Finished)
                .count()
                >= 8
            {
                return Err(ServiceError::Capacity);
            }
            if operations.len() >= 32 {
                // Recovery of evicted snapshots uses durable pending submissions
                // plus authoritative reads, never a resend of their messages.
                let evict = operations
                    .iter()
                    .find(|(_, op)| op.snapshot().phase == OperationPhase::Finished)
                    .map(|(id, _)| *id);
                if let Some(id) = evict {
                    operations.remove(&id);
                }
            }
            operations.insert(operation.id(), operation.clone());
        }
        let session = Arc::clone(self);
        let task_operation = operation.clone();
        tokio::spawn(async move {
            let error = session
                .run_turn(draft, request, &task_operation)
                .await
                .err();
            if session.check().is_ok() {
                task_operation.update(|snapshot| {
                    snapshot.phase = OperationPhase::Finished;
                    if let Some(error) = error {
                        snapshot.error = Some(error);
                    }
                });
            }
        });
        Ok(operation)
    }

    async fn run_turn(
        &self,
        draft: ChatDraft,
        request: StartSavedTurn,
        operation: &Operation,
    ) -> Result<(), ServiceError> {
        let native = self.native().await?;
        let (uploads, files) = self.ready_uploads(&draft, &request.upload_handles)?;
        let prepared = native
            .prepare(
                SavedTurnRequest {
                    submission_id: request.submission_id,
                    client_message_id: request.client_message_id,
                    draft_id: draft.id,
                    conversation_id: draft.content.conversation_id.clone(),
                    expected_parent_id: request.expected_parent_id.clone(),
                    model: draft.content.requested_model.clone(),
                    text: draft.content.text.clone(),
                    created_at_ms: chrono::Utc::now().timestamp_millis(),
                },
                uploads,
                &operation.cancel,
            )
            .await?;
        let model = prepared.requested_model().to_owned();
        self.check()?;
        if operation.cancel.is_cancelled() {
            return Err(ChatError::LocalCancellation.into());
        }
        let store = self.store()?.clone();
        operation.update(|state| {
            state.requested_model = Some(model.clone());
            state.recovery_pending = true;
        });
        let reservation = store
            .begin_submission(
                ChatSubmissionRequest {
                    submission_id: request.submission_id,
                    client_message_id: request.client_message_id,
                    draft_id: draft.id,
                    expected_revision: request.revision,
                    expected_parent_id: request.expected_parent_id,
                    requested_model: model.clone(),
                    files,
                },
                self.scope_id,
            )
            .await;
        let (submission, lease) = match reservation {
            Ok(reservation) => reservation,
            Err(error) => {
                // A failed commit may still have created the durable record.
                // Keep recovery pending unless a successful read establishes
                // that no record was created; no HTTP send has started here.
                if let Ok(recorded) = store.read_submission(request.submission_id).await {
                    operation.update(|state| {
                        state.recovery_pending = recorded.is_some_and(|record| record.active)
                    });
                }
                return Err(error.into());
            }
        };
        // From here onward, only the durable record can decide whether an error
        // preceded transmission. Cancellation cannot discard this reservation.
        operation.update(|state| {
            state.requested_model = Some(model);
            state.recovery_pending = true;
        });
        let lease = Arc::new(lease);
        let mut sink = JournalSink::new(
            self,
            store.clone(),
            Arc::clone(&lease),
            submission,
            operation.clone(),
        );
        let heartbeat_stop = CancellationToken::new();
        let _heartbeat_guard = heartbeat_stop.clone().drop_guard();
        let heartbeat = renew_lease(
            store.clone(),
            Arc::clone(&lease),
            operation.cancel.clone(),
            heartbeat_stop,
        );
        let heartbeat = tokio::spawn(heartbeat);
        let result = native.send(prepared, &mut sink, &operation.cancel).await;
        let finish = sink.finish(result).await;
        heartbeat.abort();
        finish?;
        let final_state = operation.snapshot();
        if final_state.acceptance == Acceptance::Accepted {
            let mut uploads = lock(&self.uploads);
            for handle in &request.upload_handles {
                uploads.remove(handle);
            }
        }
        Ok(())
    }

    /// A new owner may claim an expired lease only for exact-ID reconciliation.
    /// No native preparation or user-message POST exists on this path.
    pub async fn reconcile_submission(&self, id: Uuid) -> Result<ChatSubmission, ServiceError> {
        self.guarded(async {
            let store = self.store()?;
            let existing = store
                .read_submission(id)
                .await?
                .ok_or(ChatStoreError::NotFound)?;
            if !existing.active {
                return Ok(existing);
            }
            let (submission, lease) = store.claim_reconciliation(id, self.scope_id).await?;
            let Some(conversation) = &submission.conversation_id else {
                return Ok(submission);
            };
            let reconciled = match self
                .reader()
                .await?
                .reconcile(
                    conversation,
                    submission.client_message_id,
                    &submission.expected_parent_id,
                )
                .await
            {
                Ok(reconciled) => reconciled,
                // Absence, including a currently invisible history entry, does
                // not establish rejection. The submission stays pending.
                Err(ChatError::PendingSubmission | ChatError::NotFound) => return Ok(submission),
                Err(error) => return Err(error.into()),
            };
            self.check()?;
            store
                .record_acceptance(
                    &lease,
                    ChatAcceptanceProof {
                        conversation_id: conversation.clone(),
                        client_message_id: submission.client_message_id,
                        parent_message_id: submission.expected_parent_id.clone(),
                    },
                )
                .await?;
            if let Some(outcome) = terminal_outcome(reconciled.outcome, None) {
                store
                    .finish_submission_and_advance(
                        &lease,
                        outcome,
                        reconciled
                            .messages
                            .iter()
                            .rev()
                            .find_map(|m| m.model.as_deref()),
                        reconciled.snapshot.branch.current_node.as_deref(),
                    )
                    .await?;
            }
            Ok(store
                .read_submission(id)
                .await?
                .ok_or(ChatStoreError::NotFound)?)
        })
        .await
    }
}

struct JournalSink<'a> {
    session: &'a SavedSession,
    store: SavedChatStore,
    lease: Arc<ChatSubmissionLease>,
    submission: ChatSubmission,
    operation: Operation,
    store_error: Option<ChatStoreError>,
}

impl<'a> JournalSink<'a> {
    fn new(
        session: &'a SavedSession,
        store: SavedChatStore,
        lease: Arc<ChatSubmissionLease>,
        submission: ChatSubmission,
        operation: Operation,
    ) -> Self {
        Self {
            session,
            store,
            lease,
            submission,
            operation,
            store_error: None,
        }
    }

    async fn persist(
        &mut self,
        conversation: Option<&str>,
        acceptance: Acceptance,
    ) -> Result<(), ChatError> {
        self.session
            .check()
            .map_err(|_| ChatError::SessionChanged)?;
        let result = async {
            if let Some(conversation) = conversation
                && self.submission.conversation_id.as_deref() != Some(conversation)
            {
                self.store
                    .record_conversation(&self.lease, conversation)
                    .await?;
                self.submission.conversation_id = Some(conversation.into());
            }
            if acceptance == Acceptance::Accepted && !self.submission.accepted {
                let conversation = conversation.ok_or(ChatStoreError::InvalidTransition)?;
                self.store
                    .record_acceptance(
                        &self.lease,
                        ChatAcceptanceProof {
                            conversation_id: conversation.into(),
                            client_message_id: self.submission.client_message_id,
                            parent_message_id: self.submission.expected_parent_id.clone(),
                        },
                    )
                    .await?;
                self.submission.accepted = true;
            }
            Ok(())
        }
        .await;
        self.map_store(result)?;
        self.session.check().map_err(|_| ChatError::SessionChanged)
    }

    fn map_store<T>(&mut self, result: Result<T, ChatStoreError>) -> Result<T, ChatError> {
        result.map_err(|error| {
            self.store_error = Some(error);
            ChatError::RecoveryUnavailable
        })
    }

    async fn finish(&mut self, result: SavedTurnResult) -> Result<(), ServiceError> {
        self.session.check()?;
        // Retry only local journal checkpoints, never the user-message POST.
        // If a cancelled callback committed before cancellation, exact reads and
        // idempotent acceptance recording preserve its accepted user message.
        let persisted = self
            .persist(result.conversation_id.as_deref(), result.acceptance)
            .await;
        let mut error = result.error.map(ServiceError::Native);
        if let Err(native_error) = persisted {
            error = Some(
                self.store_error
                    .map(ServiceError::Store)
                    .unwrap_or(native_error.into()),
            );
        }
        let mut recorded = self
            .store
            .read_submission(self.submission.submission_id)
            .await?
            .ok_or(ChatStoreError::NotFound)?;
        if persisted.is_ok() {
            let outcome = terminal_outcome(result.outcome, result.error)
                .unwrap_or(ChatSubmissionOutcome::Failed);
            let accepted_parent = result.client_message_id.to_string();
            if let Err(store_error) = self
                .store
                .finish_submission_and_advance(
                    &self.lease,
                    outcome,
                    result.resolved_model.as_deref(),
                    Some(
                        result
                            .messages
                            .last()
                            .map_or(accepted_parent.as_str(), |message| message.id.as_str()),
                    ),
                )
                .await
            {
                error = Some(store_error.into());
            }
            recorded = self
                .store
                .read_submission(self.submission.submission_id)
                .await?
                .ok_or(ChatStoreError::NotFound)?;
        }
        self.session.check()?;
        self.operation.update(|state| {
            state.conversation_id = result.conversation_id;
            state.acceptance = if recorded.accepted {
                Acceptance::Accepted
            } else {
                result.acceptance
            };
            state.outcome = result.outcome;
            state.messages = result.messages;
            state.resolved_model = result.resolved_model;
            state.error = error;
            state.recovery_pending = recorded.active;
        });
        Ok(())
    }
}

impl TurnSink for JournalSink<'_> {
    async fn before_transmission(
        &mut self,
        submission_id: Uuid,
        requested_model: &str,
    ) -> Result<(), ChatError> {
        self.session
            .check()
            .map_err(|_| ChatError::SessionChanged)?;
        if submission_id != self.submission.submission_id
            || requested_model != self.submission.snapshot.requested_model
        {
            return Err(ChatError::RecoveryUnavailable);
        }
        let result = self.store.mark_transmitting(&self.lease).await;
        let permit = self.map_store(result)?;
        if permit.submission_id() != submission_id {
            return Err(ChatError::RecoveryUnavailable);
        }
        self.session
            .check()
            .map_err(|_| ChatError::SessionChanged)?;
        self.operation
            .update(|state| state.phase = OperationPhase::Sending);
        Ok(())
    }

    async fn checkpoint(&mut self, state: &TurnState) -> Result<(), ChatError> {
        self.persist(state.conversation_id.as_deref(), state.acceptance)
            .await?;
        self.operation.update(|snapshot| {
            snapshot.conversation_id.clone_from(&state.conversation_id);
            snapshot.acceptance = state.acceptance;
            snapshot.outcome = state.outcome();
            snapshot.messages.clone_from(&state.messages);
            snapshot.resolved_model.clone_from(&state.resolved_model);
        });
        Ok(())
    }
}

async fn renew_lease(
    store: SavedChatStore,
    lease: Arc<ChatSubmissionLease>,
    cancel: CancellationToken,
    done: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = done.cancelled() => break,
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
        }
        if store.renew_lease(&lease).await.is_err() {
            cancel.cancel();
            break;
        }
    }
}

fn terminal_outcome(
    outcome: GenerationOutcome,
    error: Option<ChatError>,
) -> Option<ChatSubmissionOutcome> {
    if error == Some(ChatError::LocalCancellation) {
        return Some(ChatSubmissionOutcome::LocalStopped);
    }
    match outcome {
        GenerationOutcome::Completed => Some(ChatSubmissionOutcome::Completed),
        GenerationOutcome::Failed => Some(ChatSubmissionOutcome::Failed),
        GenerationOutcome::UnsupportedHandoff => Some(ChatSubmissionOutcome::UnsupportedHandoff),
        GenerationOutcome::Incomplete => error.map(|_| ChatSubmissionOutcome::Failed),
    }
}

#[cfg(test)]
mod tests;
