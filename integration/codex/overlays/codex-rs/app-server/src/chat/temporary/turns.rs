use super::*;
use crate::chat::Operation;
use crate::chat::OperationPhase;
use crate::chat::OperationSnapshot;
use crate::chat::Persistence;
use codex_chatgpt_chat::conversation::ContinuationBlocker;
use codex_chatgpt_chat::conversation::ConversationBranch;
use codex_chatgpt_chat::conversation::ConversationNode;
use codex_chatgpt_chat::conversation::ConversationSnapshot;
use codex_chatgpt_chat::stream::Acceptance;
use codex_chatgpt_chat::stream::GenerationOutcome;
use codex_chatgpt_chat::stream::Message;
use codex_chatgpt_chat::stream::TurnState;
use codex_chatgpt_chat::temporary_session::TemporaryTurnRequest;
use codex_chatgpt_chat::temporary_session::TemporaryTurnResult;
use codex_chatgpt_chat::temporary_session::TemporaryTurnSink;
use std::collections::HashSet;
use tokio::sync::watch;

const MAX_TRANSCRIPT_BYTES: usize = 32 * 1024 * 1024;
const TURN_CONTENT_RESERVATION: usize = 8 * 1024 * 1024;
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;
const MAX_SUBMISSIONS: usize = 8 * 256;

pub struct StartTemporaryTurn {
    pub draft_id: Uuid,
    pub revision: Uuid,
    pub submission_id: Uuid,
    pub client_message_id: Uuid,
    pub expected_parent_id: String,
    pub upload_handles: Vec<Uuid>,
}

#[derive(Clone)]
pub struct TemporaryPending {
    pub submission_id: Uuid,
    pub client_message_id: Uuid,
    pub draft_id: Uuid,
    pub conversation_id: Option<String>,
    pub expected_parent_id: String,
    pub transmission_started: bool,
    pub accepted: bool,
    pub active: bool,
    pub content: ChatDraftContent,
}

#[derive(Default)]
pub(super) struct Turns {
    pub(super) operations: HashMap<Uuid, Operation>,
    pub(super) pending: HashMap<Uuid, TemporaryPending>,
    pub(super) conversations: HashMap<Uuid, ConversationSnapshot>,
    submissions: HashSet<Uuid>,
    message_ids: HashSet<Uuid>,
}

impl TemporarySession {
    pub fn operation(&self, id: Uuid) -> Result<Option<Operation>, ServiceError> {
        let memory = lock(&self.memory);
        self.check()?;
        Ok(memory.turns.operations.get(&id).cloned())
    }

    pub fn operations(&self) -> Result<Vec<Arc<OperationSnapshot>>, ServiceError> {
        let memory = lock(&self.memory);
        self.check()?;
        Ok(memory
            .turns
            .operations
            .values()
            .map(Operation::snapshot)
            .collect())
    }

    pub fn cancel_turn(&self, id: Uuid) -> Result<Option<Arc<OperationSnapshot>>, ServiceError> {
        let memory = lock(&self.memory);
        self.check()?;
        let Some(operation) = memory.turns.operations.get(&id) else {
            return Ok(None);
        };
        operation.cancel.cancel();
        Ok(Some(operation.snapshot()))
    }

    pub fn pending_for_draft(&self, draft: Uuid) -> Result<Option<TemporaryPending>, ServiceError> {
        let memory = lock(&self.memory);
        self.check()?;
        Ok(memory.turns.pending.get(&draft).cloned())
    }

    /// A memory snapshot of observed stream messages. Only user-message parent
    /// IDs and the completed response head are known; other parent links stay
    /// absent. Native continuation always refreshes the authoritative conduit.
    pub fn conversation_read(&self, id: &str) -> Result<ConversationSnapshot, ServiceError> {
        let memory = lock(&self.memory);
        self.check()?;
        let cached = memory
            .turns
            .conversations
            .values()
            .find(|snapshot| snapshot.id == id)
            .cloned();
        for operation in memory.turns.operations.values() {
            let turn = operation.snapshot();
            if turn.phase == OperationPhase::Finished || turn.conversation_id.as_deref() != Some(id)
            {
                continue;
            }
            let Some(pending) = memory.turns.pending.get(&turn.draft_id) else {
                continue;
            };
            let mut snapshot = cached.unwrap_or_else(|| empty_conversation(id));
            merge_messages(
                &mut snapshot,
                &turn.messages,
                turn.client_message_id,
                &pending.expected_parent_id,
            );
            snapshot.branch.current_node = None;
            snapshot.continuation_blocker = Some(ContinuationBlocker::IncompleteGeneration);
            snapshot.default_model = turn.resolved_model.clone().or(turn.requested_model.clone());
            return Ok(snapshot);
        }
        cached.ok_or(ChatError::NotFound.into())
    }

    pub fn pending_for_conversation(
        &self,
        id: &str,
    ) -> Result<Option<TemporaryPending>, ServiceError> {
        let memory = lock(&self.memory);
        self.check()?;
        Ok(memory
            .turns
            .pending
            .values()
            .find(|pending| pending.conversation_id.as_deref() == Some(id))
            .cloned())
    }

    pub fn start_turn(
        self: &Arc<Self>,
        request: StartTemporaryTurn,
    ) -> Result<Operation, ServiceError> {
        self.check()?;
        let draft = self
            .draft_read(request.draft_id)?
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
        let ready = self.ready_uploads(&draft, &request.upload_handles)?;
        let operation = self.reserve(&draft, &request)?;
        let session = Arc::clone(self);
        let task_operation = operation.clone();
        tokio::spawn(async move {
            let result = async {
                let native = session.native(draft.id)?;
                let mut sink = MemorySink {
                    session: &session,
                    operation: &task_operation,
                };
                let result = native
                    .send(
                        TemporaryTurnRequest {
                            submission_id: request.submission_id,
                            client_message_id: request.client_message_id,
                            conversation_id: draft.content.conversation_id.clone(),
                            expected_parent_id: request.expected_parent_id,
                            model: draft.content.requested_model.clone(),
                            text: draft.content.text.clone(),
                            created_at_ms: chrono::Utc::now().timestamp_millis(),
                        },
                        ready,
                        &mut sink,
                        &task_operation.cancel,
                    )
                    .await?;
                session.finish(&draft, &task_operation, result)
            }
            .await;
            if session.check().is_ok() {
                if let Err(error) = result {
                    let mut memory = lock(&session.memory);
                    if memory
                        .turns
                        .pending
                        .get(&draft.id)
                        .is_some_and(|pending| !pending.transmission_started)
                    {
                        memory.turns.pending.remove(&draft.id);
                    }
                    task_operation.update(|snapshot| {
                        snapshot.error = Some(error);
                    });
                }
                if task_operation.snapshot().acceptance == Acceptance::Accepted {
                    for handle in request.upload_handles {
                        let _ = session.cancel_upload(handle);
                    }
                }
                if let Some(pending) = lock(&session.memory).turns.pending.get_mut(&draft.id) {
                    pending.active = false;
                }
                task_operation.update(|snapshot| {
                    snapshot.phase = OperationPhase::Finished;
                });
            }
        });
        Ok(operation)
    }

    fn reserve(
        &self,
        draft: &ChatDraft,
        request: &StartTemporaryTurn,
    ) -> Result<Operation, ServiceError> {
        let mut memory = lock(&self.memory);
        self.check()?;
        if memory.drafts.get(&draft.id).map(|draft| draft.revision) != Some(request.revision) {
            return Err(ChatStoreError::RevisionConflict.into());
        }
        let turns = &mut memory.turns;
        if turns.operations.values().any(|operation| {
            let snapshot = operation.snapshot();
            snapshot.draft_id == draft.id && snapshot.phase != OperationPhase::Finished
        }) {
            return Err(ServiceError::OverlappingTurn);
        }
        if turns.pending.contains_key(&draft.id) {
            return Err(ChatError::PendingSubmission.into());
        }
        if turns.submissions.contains(&request.submission_id)
            || turns.message_ids.contains(&request.client_message_id)
        {
            return Err(ChatStoreError::DuplicateSubmission.into());
        }
        if turns
            .conversations
            .get(&draft.id)
            .is_some_and(|snapshot| snapshot.continuation_blocker.is_some())
        {
            return Err(ChatError::UnsupportedCapability.into());
        }
        let active = turns
            .operations
            .values()
            .filter(|operation| operation.snapshot().phase != OperationPhase::Finished)
            .count();
        let retained = turns
            .conversations
            .values()
            .flat_map(|snapshot| &snapshot.branch.nodes)
            .filter_map(|node| node.message.as_ref())
            .map(Message::weight)
            .sum::<usize>();
        let pending = turns
            .pending
            .values()
            .map(|pending| content_weight(&pending.content))
            .sum::<usize>();
        if active >= 8
            || turns.submissions.len() >= MAX_SUBMISSIONS
            || retained.saturating_add((active + 1) * TURN_CONTENT_RESERVATION)
                > MAX_TRANSCRIPT_BYTES
            || pending.saturating_add(content_weight(&draft.content)) > MAX_PENDING_BYTES
        {
            return Err(ServiceError::Capacity);
        }
        if turns.operations.len() >= 32 {
            let evict = turns
                .operations
                .iter()
                .find(|(_, operation)| operation.snapshot().phase == OperationPhase::Finished)
                .map(|(id, _)| *id);
            if let Some(id) = evict {
                turns.operations.remove(&id);
            }
        }
        let operation = Operation {
            updates: Arc::new(
                watch::channel(Arc::new(OperationSnapshot {
                    scope_id: self.scope_id(),
                    persistence: Persistence::Temporary,
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
            target: format!("draft:{}", draft.id),
        };
        turns.pending.insert(
            draft.id,
            TemporaryPending {
                submission_id: request.submission_id,
                client_message_id: request.client_message_id,
                draft_id: draft.id,
                conversation_id: draft.content.conversation_id.clone(),
                expected_parent_id: request.expected_parent_id.clone(),
                transmission_started: false,
                accepted: false,
                active: true,
                content: draft.content.clone(),
            },
        );
        turns.submissions.insert(request.submission_id);
        turns.message_ids.insert(request.client_message_id);
        turns.operations.insert(operation.id(), operation.clone());
        Ok(operation)
    }

    fn finish(
        &self,
        submitted: &ChatDraft,
        operation: &Operation,
        result: TemporaryTurnResult,
    ) -> Result<(), ServiceError> {
        let mut memory = lock(&self.memory);
        self.check()?;
        if submitted
            .content
            .conversation_id
            .as_ref()
            .is_some_and(|expected| {
                result
                    .conversation_id
                    .as_ref()
                    .is_some_and(|id| id != expected)
            })
        {
            return Err(ChatError::ProtocolChanged.into());
        }
        let pending = memory
            .turns
            .pending
            .get_mut(&submitted.id)
            .ok_or(ChatError::PendingSubmission)?;
        pending.transmission_started |= result.transmission_started;
        pending.accepted |= result.acceptance == Acceptance::Accepted;
        pending.active = false;
        let accepted = pending.accepted;
        pending.conversation_id.clone_from(&result.conversation_id);
        let recovery_pending = pending.transmission_started
            && (!pending.accepted || result.outcome == GenerationOutcome::Incomplete);
        if !recovery_pending {
            memory.turns.pending.remove(&submitted.id);
        }
        let current = result
            .continuation_eligible
            .then(|| {
                result
                    .messages
                    .iter()
                    .rev()
                    .find(|message| {
                        message.role == "assistant" && message.finished && message.end_turn
                    })
                    .map(|message| message.id.clone())
            })
            .flatten();
        if result.transmission_started
            && let Some(id) = &result.conversation_id
        {
            let snapshot = memory
                .turns
                .conversations
                .entry(submitted.id)
                .or_insert_with(|| empty_conversation(id));
            if snapshot.id != *id {
                return Err(ChatError::ProtocolChanged.into());
            }
            merge_messages(
                snapshot,
                &result.messages,
                result.client_message_id,
                &result.expected_parent_id,
            );
            snapshot.branch.current_node.clone_from(&current);
            snapshot.default_model = result
                .resolved_model
                .clone()
                .or(Some(result.requested_model.clone()));
            snapshot.updated_at_ms = Some(chrono::Utc::now().timestamp_millis());
            snapshot.continuation_blocker = if result.continuation_eligible {
                None
            } else if result.outcome == GenerationOutcome::Incomplete {
                Some(ContinuationBlocker::IncompleteGeneration)
            } else {
                Some(ContinuationBlocker::UnsupportedContent)
            };
        } else if !result.continuation_eligible
            && let Some(snapshot) = memory.turns.conversations.get_mut(&submitted.id)
        {
            snapshot.continuation_blocker = Some(ContinuationBlocker::UnsupportedContent);
        }
        if accepted {
            let draft = memory
                .drafts
                .get_mut(&submitted.id)
                .ok_or(ChatStoreError::NotFound)?;
            if draft.revision == submitted.revision {
                draft.content.text.clear();
                draft.content.selections.clear();
            }
            draft
                .content
                .conversation_id
                .clone_from(&result.conversation_id);
            draft.content.expected_parent_id = current;
            draft.revision = Uuid::new_v4();
            draft.updated_at_ms = chrono::Utc::now()
                .timestamp_millis()
                .max(draft.updated_at_ms.saturating_add(1));
        }
        operation.update(|snapshot| {
            snapshot.conversation_id = result.conversation_id;
            snapshot.acceptance = if accepted {
                Acceptance::Accepted
            } else {
                result.acceptance
            };
            snapshot.outcome = result.outcome;
            snapshot.requested_model = Some(result.requested_model);
            snapshot.resolved_model = result.resolved_model;
            snapshot.messages = result.messages;
            snapshot.error = result.error.map(ServiceError::Native);
            snapshot.recovery_pending = recovery_pending;
        });
        Ok(())
    }
}

fn empty_conversation(id: &str) -> ConversationSnapshot {
    ConversationSnapshot {
        id: id.into(),
        title: "Temporary chat".into(),
        updated_at_ms: None,
        branch: ConversationBranch {
            current_node: None,
            nodes: Vec::new(),
        },
        other_branch_heads: Vec::new(),
        default_model: None,
        continuation_blocker: None,
    }
}

fn merge_messages(
    snapshot: &mut ConversationSnapshot,
    messages: &[Message],
    client_message_id: Uuid,
    expected_parent_id: &str,
) {
    for message in messages {
        let content =
            matches!(message.role.as_str(), "user" | "assistant").then(|| message.clone());
        if let Some(node) = snapshot
            .branch
            .nodes
            .iter_mut()
            .find(|node| node.id == message.id)
        {
            node.message = content;
        } else {
            snapshot.branch.nodes.push(ConversationNode {
                id: message.id.clone(),
                parent_id: (message.id == client_message_id.to_string())
                    .then(|| expected_parent_id.into()),
                message: content,
            });
        }
    }
}

struct MemorySink<'a> {
    session: &'a TemporarySession,
    operation: &'a Operation,
}
impl TemporaryTurnSink for MemorySink<'_> {
    fn before_transmission(&mut self, model: &str) -> Result<(), ChatError> {
        let mut memory = lock(&self.session.memory);
        self.session
            .check()
            .map_err(|_| ChatError::SessionChanged)?;
        if self.operation.cancel.is_cancelled() {
            return Err(ChatError::LocalCancellation);
        }
        let draft = self.operation.snapshot().draft_id;
        let pending = memory
            .turns
            .pending
            .get_mut(&draft)
            .ok_or(ChatError::PendingSubmission)?;
        pending.transmission_started = true;
        self.operation.update(|snapshot| {
            snapshot.phase = OperationPhase::Sending;
            snapshot.requested_model = Some(model.into());
            snapshot.recovery_pending = true;
        });
        Ok(())
    }
    fn checkpoint(&mut self, state: &TurnState) -> Result<(), ChatError> {
        let mut memory = lock(&self.session.memory);
        self.session
            .check()
            .map_err(|_| ChatError::SessionChanged)?;
        let draft = self.operation.snapshot().draft_id;
        let pending = memory
            .turns
            .pending
            .get_mut(&draft)
            .ok_or(ChatError::PendingSubmission)?;
        pending.accepted |= state.acceptance == Acceptance::Accepted;
        pending.conversation_id.clone_from(&state.conversation_id);
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

#[cfg(test)]
mod tests;
