use super::*;
use crate::chat::tests::Harness;
use codex_chatgpt_chat::stream::TurnDecoder;
use serde_json::json;

async fn reserve(session: &SavedSession) -> (ChatSubmission, Arc<ChatSubmissionLease>, Operation) {
    let draft = session
        .draft_write(Uuid::new_v4(), None, crate::chat::tests::content())
        .await
        .unwrap();
    let (submission, lease) = session
        .store()
        .unwrap()
        .begin_submission(
            ChatSubmissionRequest {
                submission_id: Uuid::new_v4(),
                client_message_id: Uuid::new_v4(),
                draft_id: draft.id,
                expected_revision: draft.revision,
                expected_parent_id: Uuid::new_v4().to_string(),
                requested_model: "fixture-model".into(),
                files: Vec::new(),
            },
            session.scope_id,
        )
        .await
        .unwrap();
    let operation = Operation {
        updates: Arc::new(
            watch::channel(Arc::new(OperationSnapshot {
                scope_id: session.scope_id,
                persistence: Persistence::Saved,
                operation_id: Uuid::new_v4(),
                draft_id: draft.id,
                submission_id: submission.submission_id,
                client_message_id: submission.client_message_id,
                conversation_id: None,
                sequence: 0,
                phase: OperationPhase::Preparing,
                acceptance: Acceptance::Unknown,
                outcome: GenerationOutcome::Incomplete,
                requested_model: Some("fixture-model".into()),
                resolved_model: None,
                messages: Vec::new(),
                error: None,
                recovery_pending: true,
            }))
            .0,
        ),
        cancel: session.stopped.child_token(),
        target: format!("draft:{}", draft.id),
    };
    (submission, Arc::new(lease), operation)
}

fn result(sink: &JournalSink<'_>, acceptance: Acceptance, error: ChatError) -> SavedTurnResult {
    SavedTurnResult {
        submission_id: sink.submission.submission_id,
        client_message_id: sink.submission.client_message_id,
        conversation_id: sink.submission.conversation_id.clone(),
        expected_parent_id: sink.submission.expected_parent_id.clone(),
        // Deliberately false: a dropped before-transmission callback may have
        // committed its marker even when the native caller did not observe it.
        transmission_started: false,
        acceptance,
        outcome: GenerationOutcome::Incomplete,
        messages: Vec::new(),
        requested_model: "fixture-model".into(),
        resolved_model: None,
        error: Some(error),
    }
}

#[tokio::test]
async fn durable_marker_is_committed_once_and_local_cancel_cannot_clear_unknown_acceptance() {
    let h = Harness::new().await;
    let session = h.session();
    let (submission, lease, operation) = reserve(&session).await;
    let id = submission.submission_id;
    let mut sink = JournalSink::new(
        &session,
        session.store().unwrap().clone(),
        lease,
        submission,
        operation.clone(),
    );
    sink.before_transmission(id, "fixture-model").await.unwrap();
    assert!(
        session
            .submission_read(id)
            .await
            .unwrap()
            .unwrap()
            .transmission_started
    );
    assert_eq!(
        sink.before_transmission(id, "fixture-model").await,
        Err(ChatError::RecoveryUnavailable)
    );
    sink.finish(result(
        &sink,
        Acceptance::Unknown,
        ChatError::LocalCancellation,
    ))
    .await
    .unwrap();
    let recorded = session.submission_read(id).await.unwrap().unwrap();
    assert!(recorded.active && recorded.transmission_started && !recorded.accepted);
    assert_eq!(recorded.outcome, ChatSubmissionOutcome::LocalStopped);
    assert!(operation.snapshot().recovery_pending);
    assert_eq!(
        operation.snapshot().error,
        Some(ServiceError::Native(ChatError::LocalCancellation))
    );
    assert_eq!(
        recorded.snapshot.draft.text,
        crate::chat::tests::content().text
    );
}

#[tokio::test]
async fn accepted_user_checkpoint_precedes_display_and_survives_generation_failure_and_draft_edits()
{
    let h = Harness::new().await;
    let session = h.session();
    let (submission, lease, operation) = reserve(&session).await;
    let id = submission.submission_id;
    let message = submission.client_message_id;
    let mut sink = JournalSink::new(
        &session,
        session.store().unwrap().clone(),
        lease,
        submission,
        operation.clone(),
    );
    sink.before_transmission(id, "fixture-model").await.unwrap();
    let mut decoder = TurnDecoder::new(message.to_string());
    let value = json!({"conversation_id":"fixture-conversation","message":{"id":message,"author":{"role":"user"},"content":{"content_type":"text","parts":["private fixture"]}}});
    decoder
        .push(format!("data: {value}\n\n").as_bytes())
        .unwrap();
    sink.checkpoint(decoder.state()).await.unwrap();
    assert!(session.submission_read(id).await.unwrap().unwrap().accepted);
    assert_eq!(operation.snapshot().acceptance, Acceptance::Accepted);
    let draft = session
        .draft_read(sink.submission.draft_id)
        .await
        .unwrap()
        .unwrap();
    let mut replacement = draft.content.clone();
    replacement.text = "next unsent draft".into();
    session
        .draft_write(draft.id, Some(draft.revision), replacement)
        .await
        .unwrap();
    let mut terminal = result(&sink, Acceptance::Accepted, ChatError::Transport);
    terminal.messages = decoder.state().messages.clone();
    terminal.outcome = GenerationOutcome::Failed;
    sink.finish(terminal).await.unwrap();
    let recorded = session.submission_read(id).await.unwrap().unwrap();
    assert!(recorded.accepted && !recorded.active);
    assert_eq!(recorded.outcome, ChatSubmissionOutcome::Failed);
    assert_eq!(
        recorded.snapshot.draft.text,
        crate::chat::tests::content().text
    );
    assert_eq!(
        session
            .draft_read(draft.id)
            .await
            .unwrap()
            .unwrap()
            .content
            .text,
        "next unsent draft"
    );
    assert_eq!(operation.snapshot().messages[0].id, message.to_string());
    assert!(!operation.snapshot().recovery_pending);
}

#[tokio::test]
async fn pre_transmission_failure_preserves_draft_and_releases_reservation_without_resending() {
    let h = Harness::new().await;
    let session = h.session();
    let (submission, lease, operation) = reserve(&session).await;
    let id = submission.submission_id;
    let mut sink = JournalSink::new(
        &session,
        session.store().unwrap().clone(),
        lease,
        submission,
        operation.clone(),
    );
    sink.finish(result(
        &sink,
        Acceptance::Unknown,
        ChatError::BranchConflict,
    ))
    .await
    .unwrap();
    let recorded = session.submission_read(id).await.unwrap().unwrap();
    assert!(!recorded.active && !recorded.transmission_started && !recorded.accepted);
    assert!(
        session
            .draft_read(recorded.draft_id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(!operation.snapshot().recovery_pending);
}

#[tokio::test]
async fn failed_journal_commit_and_revoked_owner_never_grant_transmission() {
    let h = Harness::new().await;
    let session = h.session();
    let (submission, lease, operation) = reserve(&session).await;
    let id = submission.submission_id;
    let pool = h
        .sqlite
        .open_read_write_pool(&h.sqlite.state_db_path())
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_chat_marker BEFORE UPDATE OF transmission_started ON chat_submissions BEGIN SELECT RAISE(ABORT, 'synthetic-private-error'); END").execute(&pool).await.unwrap();
    let mut sink = JournalSink::new(
        &session,
        session.store().unwrap().clone(),
        lease,
        submission,
        operation,
    );
    assert_eq!(
        sink.before_transmission(id, "fixture-model").await,
        Err(ChatError::RecoveryUnavailable)
    );
    assert_eq!(sink.store_error, Some(ChatStoreError::Unavailable));
    assert!(
        !session
            .submission_read(id)
            .await
            .unwrap()
            .unwrap()
            .transmission_started
    );
    assert!(!format!("{:?}", sink.store_error).contains("synthetic-private-error"));
    h.service.set_enabled(false);
    assert_eq!(
        sink.before_transmission(id, "fixture-model").await,
        Err(ChatError::SessionChanged)
    );
}

#[tokio::test]
async fn independent_owner_lease_takeover_fences_old_callback_and_cannot_transmit() {
    let h = Harness::new().await;
    let session = h.session();
    let (submission, lease, operation) = reserve(&session).await;
    let id = submission.submission_id;
    let pool = h
        .sqlite
        .open_read_write_pool(&h.sqlite.state_db_path())
        .await
        .unwrap();
    sqlx::query("UPDATE chat_submissions SET lease_until_ms=0 WHERE submission_id=?")
        .bind(id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let (_, replacement) = session
        .store()
        .unwrap()
        .claim_reconciliation(id, Uuid::new_v4())
        .await
        .unwrap();
    let mut sink = JournalSink::new(
        &session,
        session.store().unwrap().clone(),
        lease,
        submission,
        operation,
    );
    assert_eq!(
        sink.before_transmission(id, "fixture-model").await,
        Err(ChatError::RecoveryUnavailable)
    );
    assert_eq!(sink.store_error, Some(ChatStoreError::LeaseLost));
    assert!(matches!(
        session
            .store()
            .unwrap()
            .mark_transmitting(&replacement)
            .await,
        Err(ChatStoreError::ReconcileOnly)
    ));
}

#[tokio::test]
async fn overlapping_local_clients_keep_drafts_and_other_conversations_remain_cancellable() {
    let h = Harness::new().await;
    let session = h.session();
    let (submission, _lease, operation) = reserve(&session).await;
    lock(&session.operations).insert(operation.id(), operation.clone());
    let request = StartSavedTurn {
        draft_id: submission.draft_id,
        revision: submission.draft_revision,
        submission_id: Uuid::new_v4(),
        client_message_id: Uuid::new_v4(),
        expected_parent_id: submission.expected_parent_id.clone(),
        upload_handles: Vec::new(),
    };
    assert!(matches!(
        session.start_turn(request).await,
        Err(ServiceError::OverlappingTurn)
    ));
    assert!(
        session
            .draft_read(submission.draft_id)
            .await
            .unwrap()
            .is_some()
    );
    let other = h.draft().await;
    let independent = session
        .start_turn(StartSavedTurn {
            draft_id: other.id,
            revision: other.revision,
            submission_id: Uuid::new_v4(),
            client_message_id: Uuid::new_v4(),
            expected_parent_id: Uuid::new_v4().to_string(),
            upload_handles: Vec::new(),
        })
        .await
        .unwrap();
    let mut updates = independent.subscribe();
    session.cancel_turn(independent.id()).unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        updates.wait_for(|state| state.phase == OperationPhase::Finished),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        independent.snapshot().error,
        Some(ServiceError::Native(ChatError::LocalCancellation))
    );
    assert_eq!(operation.snapshot().phase, OperationPhase::Preparing);
    assert!(
        session
            .submission_read(independent.snapshot().submission_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn completed_saved_turn_advances_its_draft_before_publishing_the_terminal_snapshot() {
    let h = Harness::new().await;
    let session = h.session();
    let (submission, lease, operation) = reserve(&session).await;
    let id = submission.submission_id;
    let draft_id = submission.draft_id;
    let message = submission.client_message_id;
    let mut sink = JournalSink::new(
        &session,
        session.store().unwrap().clone(),
        lease,
        submission,
        operation.clone(),
    );
    sink.before_transmission(id, "fixture-model").await.unwrap();
    let mut decoder = TurnDecoder::new(message.to_string());
    for (id, role, text) in [
        (message.to_string(), "user", "private fixture"),
        (
            "assistant-response".into(),
            "assistant",
            "completed response",
        ),
    ] {
        let value = json!({"conversation_id":"fixture-conversation","message":{"id":id,"author":{"role":role},"content":{"content_type":"text","parts":[text]}}});
        decoder
            .push(format!("data: {value}\n\n").as_bytes())
            .unwrap();
    }
    let mut terminal = result(&sink, Acceptance::Accepted, ChatError::Transport);
    terminal.conversation_id = Some("fixture-conversation".into());
    terminal.messages = decoder.state().messages.clone();
    terminal.outcome = GenerationOutcome::Completed;
    terminal.error = None;
    sink.finish(terminal).await.unwrap();
    let advanced = session.draft_read(draft_id).await.unwrap().unwrap();
    assert!(advanced.content.text.is_empty());
    assert_eq!(
        advanced.content.conversation_id.as_deref(),
        Some("fixture-conversation")
    );
    assert_eq!(
        advanced.content.expected_parent_id.as_deref(),
        Some("assistant-response")
    );
    assert!(!operation.snapshot().recovery_pending);
    assert_eq!(operation.snapshot().outcome, GenerationOutcome::Completed);
}
