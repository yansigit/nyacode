use codex_app_server::chat::OperationPhase;
use codex_app_server::chat::Service;
use codex_app_server::chat::ServiceError;
use codex_app_server::chat::StartSavedTurn;
use codex_app_server_transport::ConnectionOrigin;
use codex_chatgpt_chat::ServiceObservation;
use codex_chatgpt_chat::stream::Acceptance;
use codex_chatgpt_chat::stream::GenerationOutcome;
use codex_login::AuthManager;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_state::chat::ChatDraftContent;
use codex_state::chat::ChatStoreError;
use codex_state::chat::ChatSubmissionOutcome;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

pub(super) async fn run(auth: Arc<AuthManager>, sqlite: SqliteConfig) -> ServiceObservation {
    let mut observation = ServiceObservation::default();
    if let Err(error) = exercise(auth, sqlite, &mut observation).await {
        observation.error = Some(error.to_string());
    }
    observation
}

async fn exercise(
    auth: Arc<AuthManager>,
    sqlite: SqliteConfig,
    observation: &mut ServiceObservation,
) -> Result<(), ServiceError> {
    let db = StateRuntime::init(sqlite, "chat-service-probe".into())
        .await
        .map_err(|_| ChatStoreError::Unavailable)?;
    let service = Service::new(auth, Some(db), true);
    observation.remote_rejected = matches!(
        service.open_saved(ConnectionOrigin::WebSocket, None),
        Err(ServiceError::RemoteConnection)
    );
    let session = service.open_saved(ConnectionOrigin::InProcess, None)?;
    let draft = session
        .draft_write(
            Uuid::new_v4(),
            None,
            ChatDraftContent {
                conversation_id: None,
                expected_parent_id: None,
                requested_model: None,
                text: "Reply with exactly SERVICE-CEDAR-731.".into(),
                selections: Vec::new(),
            },
        )
        .await?;
    let submission_id = Uuid::new_v4();
    let client_message_id = Uuid::new_v4();
    let operation = session
        .start_turn(StartSavedTurn {
            draft_id: draft.id,
            revision: draft.revision,
            submission_id,
            client_message_id,
            expected_parent_id: Uuid::new_v4().to_string(),
            upload_handles: Vec::new(),
        })
        .await?;
    let mut updates = operation.subscribe();
    let finished = tokio::time::timeout(
        Duration::from_secs(180),
        updates.wait_for(|state| state.phase == OperationPhase::Finished),
    )
    .await;
    if !matches!(finished, Ok(Ok(_))) {
        session.cancel_turn(operation.id())?;
        service.shutdown();
        return Err(codex_chatgpt_chat::ChatError::Timeout.into());
    }
    let result = operation.snapshot();
    observation.acceptance = result.acceptance;
    observation.generation = result.outcome;
    observation
        .requested_model
        .clone_from(&result.requested_model);
    observation
        .resolved_model
        .clone_from(&result.resolved_model);
    if let Some(error) = result.error {
        return Err(error);
    }
    let recorded = session
        .submission_read(submission_id)
        .await?
        .ok_or(ChatStoreError::NotFound)?;
    observation.saved_journal_accepted = recorded.transmission_started && recorded.accepted;
    observation.saved_journal_terminal =
        !recorded.active && recorded.outcome == ChatSubmissionOutcome::Completed;
    observation.fixture_response_matched = result.messages.iter().any(|message| {
        message.role == "assistant"
            && message.finished
            && message.end_turn
            && message.text().trim() == "SERVICE-CEDAR-731"
    });
    let conversation = recorded
        .conversation_id
        .as_deref()
        .ok_or(ChatStoreError::InvalidRecord)?;
    let authoritative = session.conversation_read(conversation).await?;
    observation.authoritative_retrieval = authoritative.branch.nodes.iter().any(|node| {
        node.message
            .as_ref()
            .is_some_and(|m| m.role == "user" && m.id == client_message_id.to_string())
    }) && authoritative.branch.nodes.iter().any(|node| {
        node.message.as_ref().is_some_and(|m| {
            m.role == "assistant"
                && m.finished
                && m.end_turn
                && m.text().trim() == "SERVICE-CEDAR-731"
        })
    });
    service.set_enabled(false);
    observation.feature_disable_revoked = session.check().is_err()
        && matches!(
            service.open_saved(ConnectionOrigin::InProcess, None),
            Err(ServiceError::Native(
                codex_chatgpt_chat::ChatError::Disabled
            ))
        );
    service.shutdown();
    if result.acceptance != Acceptance::Accepted
        || result.outcome != GenerationOutcome::Completed
        || !observation.authoritative_retrieval
        || !observation.fixture_response_matched
    {
        return Err(codex_chatgpt_chat::ChatError::IncompleteStream.into());
    }
    Ok(())
}
