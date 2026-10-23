use super::*;
use crate::chat::tests::Harness;
use crate::chat::tests::content;
use crate::transport::ConnectionOrigin;
use codex_chatgpt_chat::stream::MessagePart;
use pretty_assertions::assert_eq;

fn setup(
    h: &Harness,
) -> (
    Arc<TemporarySession>,
    ChatDraft,
    StartTemporaryTurn,
    Operation,
) {
    let session = h
        .service
        .open_temporary(ConnectionOrigin::InProcess, None)
        .unwrap();
    let draft = session
        .draft_write(Uuid::new_v4(), None, content())
        .unwrap();
    let request = new_request(&draft);
    let operation = session.reserve(&draft, &request).unwrap();
    (session, draft, request, operation)
}
fn new_request(draft: &ChatDraft) -> StartTemporaryTurn {
    StartTemporaryTurn {
        draft_id: draft.id,
        revision: draft.revision,
        submission_id: Uuid::new_v4(),
        client_message_id: Uuid::new_v4(),
        expected_parent_id: Uuid::new_v4().to_string(),
        upload_handles: Vec::new(),
    }
}
fn message(id: String, role: &str, text: &str, finished: bool) -> Message {
    Message {
        id,
        role: role.into(),
        parts: vec![MessagePart::Text(text.into())],
        model: None,
        finished,
        end_turn: finished,
        attachments: Vec::new(),
        citations: Vec::new(),
    }
}
fn result(request: &StartTemporaryTurn, acceptance: Acceptance) -> TemporaryTurnResult {
    TemporaryTurnResult {
        submission_id: request.submission_id,
        client_message_id: request.client_message_id,
        conversation_id: (acceptance == Acceptance::Accepted).then(|| "temporary-fixture".into()),
        expected_parent_id: request.expected_parent_id.clone(),
        transmission_started: true,
        acceptance,
        outcome: GenerationOutcome::Incomplete,
        messages: if acceptance == Acceptance::Accepted {
            vec![message(
                request.client_message_id.to_string(),
                "user",
                "accepted fixture",
                false,
            )]
        } else {
            Vec::new()
        },
        requested_model: "fixture-model".into(),
        resolved_model: None,
        continuation_eligible: false,
        error: Some(ChatError::LocalCancellation),
    }
}

#[tokio::test]
async fn unknown_temporary_acceptance_keeps_the_original_snapshot_and_blocks_resend() {
    let h = Harness::new().await;
    let (session, draft, request, operation) = setup(&h);
    MemorySink {
        session: &session,
        operation: &operation,
    }
    .before_transmission("fixture-model")
    .unwrap();
    session
        .finish(&draft, &operation, result(&request, Acceptance::Unknown))
        .unwrap();
    operation.update(|snapshot| snapshot.phase = OperationPhase::Finished);
    let pending = session.pending_for_draft(draft.id).unwrap().unwrap();
    assert!(pending.transmission_started && !pending.active && !pending.accepted);
    assert_eq!(pending.content.text, draft.content.text);
    assert_eq!(
        session.draft_read(draft.id).unwrap().unwrap().revision,
        draft.revision
    );
    assert!(operation.snapshot().recovery_pending);
    assert!(matches!(
        session.reserve(&draft, &new_request(&draft)),
        Err(ServiceError::Native(ChatError::PendingSubmission))
    ));
    assert!(lock(&h.service.state).saved.is_none());
}

#[tokio::test]
async fn accepted_temporary_failure_retains_user_message_and_later_draft_edits() {
    let h = Harness::new().await;
    let (session, draft, request, operation) = setup(&h);
    MemorySink {
        session: &session,
        operation: &operation,
    }
    .before_transmission("fixture-model")
    .unwrap();
    let mut edit = draft.content.clone();
    edit.text = "later temporary draft".into();
    session
        .draft_write(draft.id, Some(draft.revision), edit)
        .unwrap();
    let mut response = result(&request, Acceptance::Accepted);
    response.outcome = GenerationOutcome::Failed;
    response.error = Some(ChatError::IncompleteStream);
    session.finish(&draft, &operation, response).unwrap();
    assert_eq!(operation.snapshot().acceptance, Acceptance::Accepted);
    assert_eq!(operation.snapshot().outcome, GenerationOutcome::Failed);
    assert_eq!(
        session.draft_read(draft.id).unwrap().unwrap().content.text,
        "later temporary draft"
    );
    let conversation = session.conversation_read("temporary-fixture").unwrap();
    assert_eq!(
        conversation.branch.nodes[0].message.as_ref().unwrap().id,
        request.client_message_id.to_string()
    );
    assert!(conversation.continuation_blocker.is_some());
    assert!(session.pending_for_draft(draft.id).unwrap().is_none());
}

#[tokio::test]
async fn successful_temporary_completion_clears_only_the_submitted_draft_revision() {
    let h = Harness::new().await;
    let (session, draft, request, operation) = setup(&h);
    let mut response = result(&request, Acceptance::Accepted);
    response.outcome = GenerationOutcome::Completed;
    response.continuation_eligible = true;
    response.error = None;
    response.messages.push(message(
        "private-system-node".into(),
        "system",
        "hidden fixture",
        false,
    ));
    response.messages.push(message(
        "assistant-fixture".into(),
        "assistant",
        "completed fixture",
        true,
    ));
    session.finish(&draft, &operation, response).unwrap();
    let updated = session.draft_read(draft.id).unwrap().unwrap();
    assert!(updated.content.text.is_empty());
    assert_eq!(
        updated.content.expected_parent_id.as_deref(),
        Some("assistant-fixture")
    );
    assert_eq!(
        updated.content.conversation_id.as_deref(),
        Some("temporary-fixture")
    );
    let conversation = session.conversation_read("temporary-fixture").unwrap();
    assert_eq!(
        conversation.branch.current_node.as_deref(),
        Some("assistant-fixture")
    );
    assert!(conversation.continuation_blocker.is_none());
    assert!(conversation.branch.nodes[1].message.is_none());
    assert_eq!(
        conversation.branch.nodes[0].parent_id.as_deref(),
        Some(request.expected_parent_id.as_str())
    );
    assert!(conversation.branch.nodes[2].parent_id.is_none());
    assert!(
        session
            .pending_for_conversation("temporary-fixture")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn temporary_overlap_and_memory_reservations_do_not_discard_drafts() {
    let h = Harness::new().await;
    let (session, draft, _, operation) = setup(&h);
    assert!(matches!(
        session.reserve(&draft, &new_request(&draft)),
        Err(ServiceError::OverlappingTurn)
    ));
    assert!(matches!(
        session.draft_delete(draft.id, draft.revision),
        Err(ServiceError::OverlappingTurn)
    ));
    for _ in 1..4 {
        let draft = session
            .draft_write(Uuid::new_v4(), None, content())
            .unwrap();
        session.reserve(&draft, &new_request(&draft)).unwrap();
    }
    let pending = session
        .draft_write(Uuid::new_v4(), None, content())
        .unwrap();
    assert!(matches!(
        session.reserve(&pending, &new_request(&pending)),
        Err(ServiceError::Capacity)
    ));
    assert_eq!(
        session.draft_read(pending.id).unwrap().unwrap().revision,
        pending.revision
    );
    h.service.set_enabled(false);
    assert!(operation.cancel.is_cancelled());
    let memory = lock(&session.memory);
    assert!(memory.turns.pending.is_empty() && memory.turns.operations.is_empty());
}

#[tokio::test]
async fn cancelling_before_the_native_task_runs_restores_the_literal_draft() {
    let h = Harness::new().await;
    let session = h
        .service
        .open_temporary(ConnectionOrigin::InProcess, None)
        .unwrap();
    let draft = session
        .draft_write(Uuid::new_v4(), None, content())
        .unwrap();
    let operation = session.start_turn(new_request(&draft)).unwrap();
    session.cancel_turn(operation.id()).unwrap();
    let mut updates = operation.subscribe();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if updates.borrow_and_update().phase == OperationPhase::Finished {
                break;
            }
            updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(
        operation.snapshot().error,
        Some(ServiceError::Native(ChatError::LocalCancellation))
    );
    assert_eq!(operation.snapshot().acceptance, Acceptance::Unknown);
    assert!(!operation.snapshot().recovery_pending);
    assert_eq!(
        session.draft_read(draft.id).unwrap().unwrap().revision,
        draft.revision
    );
    assert!(session.pending_for_draft(draft.id).unwrap().is_none());
}

#[tokio::test]
async fn temporary_reconnect_sees_partial_messages_without_inventing_a_completed_branch() {
    let h = Harness::new().await;
    let (session, draft, request, operation) = setup(&h);
    operation.update(|snapshot| {
        snapshot.phase = OperationPhase::Sending;
        snapshot.conversation_id = Some("temporary-fixture".into());
        snapshot.messages = vec![
            message(
                request.client_message_id.to_string(),
                "user",
                "fixture",
                false,
            ),
            message("assistant-fixture".into(), "assistant", "partial", false),
        ];
    });
    let first = session.conversation_read("temporary-fixture").unwrap();
    assert_eq!(first.branch.nodes.len(), 2);
    assert_eq!(
        first.branch.nodes[0].parent_id.as_deref(),
        Some(request.expected_parent_id.as_str())
    );
    assert!(first.branch.nodes[1].parent_id.is_none());
    assert!(first.branch.current_node.is_none());
    assert_eq!(
        first.continuation_blocker,
        Some(ContinuationBlocker::IncompleteGeneration)
    );
    assert!(lock(&session.memory).turns.conversations.is_empty());

    let mut completed = result(&request, Acceptance::Accepted);
    completed.outcome = GenerationOutcome::Completed;
    completed.continuation_eligible = true;
    completed.error = None;
    completed.messages = operation.snapshot().messages.clone();
    completed.messages[1].finished = true;
    completed.messages[1].end_turn = true;
    session.finish(&draft, &operation, completed).unwrap();
    operation.update(|snapshot| snapshot.phase = OperationPhase::Finished);
    let draft = session.draft_read(draft.id).unwrap().unwrap();
    let mut request = new_request(&draft);
    request.expected_parent_id = "assistant-fixture".into();
    let followup = session.reserve(&draft, &request).unwrap();
    followup.update(|snapshot| {
        snapshot.phase = OperationPhase::Sending;
        snapshot.messages = vec![message(
            request.client_message_id.to_string(),
            "user",
            "follow-up",
            false,
        )];
    });
    let current = session.conversation_read("temporary-fixture").unwrap();
    assert_eq!(current.branch.nodes.len(), 3);
    assert_eq!(
        current.branch.nodes[2].parent_id.as_deref(),
        Some("assistant-fixture")
    );
    assert!(current.branch.current_node.is_none());
    assert_eq!(
        current.continuation_blocker,
        Some(ContinuationBlocker::IncompleteGeneration)
    );
}

#[tokio::test]
async fn cancelling_a_followup_before_transmission_preserves_the_completed_temporary_branch() {
    let h = Harness::new().await;
    let (session, draft, request, operation) = setup(&h);
    let mut completed = result(&request, Acceptance::Accepted);
    completed.outcome = GenerationOutcome::Completed;
    completed.continuation_eligible = true;
    completed.error = None;
    completed.messages.push(message(
        "assistant-fixture".into(),
        "assistant",
        "completed",
        true,
    ));
    session.finish(&draft, &operation, completed).unwrap();
    operation.update(|snapshot| snapshot.phase = OperationPhase::Finished);
    let current = session.draft_read(draft.id).unwrap().unwrap();
    let mut content = current.content;
    content.text = "keep this follow-up draft".into();
    let draft = session
        .draft_write(current.id, Some(current.revision), content)
        .unwrap();
    let mut request = new_request(&draft);
    request.expected_parent_id = "assistant-fixture".into();
    let operation = session.reserve(&draft, &request).unwrap();
    let mut cancelled = result(&request, Acceptance::Unknown);
    cancelled.transmission_started = false;
    cancelled.conversation_id = Some("temporary-fixture".into());
    cancelled.continuation_eligible = true;
    session.finish(&draft, &operation, cancelled).unwrap();
    operation.update(|snapshot| snapshot.phase = OperationPhase::Finished);
    let retained = session.conversation_read("temporary-fixture").unwrap();
    assert_eq!(retained.branch.nodes.len(), 2);
    assert_eq!(
        retained.branch.current_node.as_deref(),
        Some("assistant-fixture")
    );
    assert!(retained.continuation_blocker.is_none());
    assert!(session.pending_for_draft(draft.id).unwrap().is_none());
    assert_eq!(
        session.draft_read(draft.id).unwrap().unwrap().revision,
        draft.revision
    );
}
