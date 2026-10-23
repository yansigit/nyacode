use super::*;
use codex_login::CodexAuth;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::sync::Mutex as StdMutex;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn session(server: &MockServer) -> TemporaryChatSession {
    let auth =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let generation = auth.auth_change_state_receiver().borrow().generation;
    let mut probe = ChatProbe::new(auth, true, Some("account_id")).unwrap();
    probe.set_test_backend(url::Url::parse(&format!("{}/backend-api/", server.uri())).unwrap());
    TemporaryChatSession {
        probe,
        generation,
        draft: Uuid::new_v4(),
        memory: Mutex::new(Memory::default()),
        sending: Semaphore::new(1),
    }
}

fn request(conversation: bool) -> TemporaryTurnRequest {
    TemporaryTurnRequest {
        submission_id: Uuid::new_v4(),
        client_message_id: Uuid::new_v4(),
        conversation_id: conversation.then(|| "temporary-fixture".into()),
        expected_parent_id: if conversation {
            "assistant-1".into()
        } else {
            Uuid::new_v4().to_string()
        },
        text: "literal ! @ $ temporary fixture".into(),
        model: None,
        created_at_ms: 1_700_000_000_000,
    }
}

#[derive(Default)]
struct Sink {
    before_error: Option<ChatError>,
    checkpoint_error: Option<ChatError>,
    accepted: bool,
}
impl TemporaryTurnSink for Sink {
    fn before_transmission(&mut self, model: &str) -> Result<(), ChatError> {
        assert_eq!(model, "fixture-model");
        self.before_error.map_or(Ok(()), Err)
    }
    fn checkpoint(&mut self, state: &TurnState) -> Result<(), ChatError> {
        self.accepted |= state.acceptance == Acceptance::Accepted;
        self.checkpoint_error.map_or(Ok(()), Err)
    }
}

async fn preparation(server: &MockServer) {
    Mock::given(method("GET")).and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"<html data-build="fixture"><script src="https://cdn.oaistatic.com/fixture.js"></script></html>"#, "text/html"))
        .mount(server).await;
    for (endpoint, value) in [
        (
            "conversation/init",
            json!({"default_model_slug":"fixture-model"}),
        ),
        (
            "sentinel/chat-requirements/prepare",
            json!({"prepare_token":"fixture-prepare","proofofwork":{"required":false},"turnstile":{"required":false},"so":{"required":false}}),
        ),
        (
            "sentinel/chat-requirements/finalize",
            json!({"token":"fixture-final"}),
        ),
    ] {
        Mock::given(method("POST"))
            .and(path(format!("/backend-api/{endpoint}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(value))
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/backend-api/f/conversation/prepare"))
        .respond_with(|r: &wiremock::Request| {
            assert_eq!(
                r.body_json::<Value>().unwrap()["history_and_training_disabled"],
                true
            );
            ResponseTemplate::new(200).set_body_json(json!({"conduit_token":"fixture-conduit"}))
        })
        .mount(server)
        .await;
}

#[derive(Default)]
struct Backend {
    turns: usize,
    resumes: usize,
    stream: String,
}

async fn conversation(
    server: &MockServer,
    status: u16,
    complete: bool,
    changed_parent: bool,
) -> Arc<StdMutex<Backend>> {
    let backend = Arc::new(StdMutex::new(Backend::default()));
    let sent = Arc::clone(&backend);
    Mock::given(method("POST")).and(path("/backend-api/f/conversation"))
        .respond_with(move |request: &wiremock::Request| {
            let mut state = sent.lock().unwrap();
            let body: Value = request.body_json().unwrap();
            assert_eq!(body["history_and_training_disabled"], true);
            assert_eq!(body["messages"][0]["content"]["parts"][0], "literal ! @ $ temporary fixture");
            assert_eq!(state.turns, state.resumes, "each continuation refreshes exactly once before sending");
            if state.turns == 0 {
                assert!(body.get("conversation_id").is_none());
                assert_eq!(body["temporary_chat_requests_personalization"], false);
            } else {
                assert_eq!(body["conversation_id"], "temporary-fixture");
                assert_eq!(body["parent_message_id"], format!("assistant-{}",state.turns));
            }
            state.turns += 1;
            let user = json!({"type":"input_message","conversation_id":"temporary-fixture","input_message":body["messages"][0]});
            let assistant = json!({"conversation_id":"temporary-fixture","message":{"id":format!("assistant-{}",state.turns),"author":{"role":"assistant"},"content":{"content_type":"text","parts":["temporary reply"]},"status":"finished_successfully","end_turn":true,"metadata":{"model_slug":"resolved-model"}}});
            state.stream = format!("data: {user}\n\n");
            if complete { state.stream.push_str(&format!("data: {assistant}\n\ndata: {{\"type\":\"message_stream_complete\"}}\n\ndata: [DONE]\n\n")); }
            ResponseTemplate::new(status).insert_header("x-conduit-token",format!("resume-{}",state.turns)).set_body_raw(state.stream.clone(),"text/event-stream")
        }).mount(server).await;
    let resumed = Arc::clone(&backend);
    Mock::given(method("POST"))
        .and(path("/backend-api/f/conversation/resume"))
        .respond_with(move |request: &wiremock::Request| {
            let mut state = resumed.lock().unwrap();
            assert_eq!(
                request.body_json::<Value>().unwrap(),
                json!({"conversation_id":"temporary-fixture","offset":0})
            );
            assert_eq!(
                request.headers["x-conduit-token"].to_str().unwrap(),
                format!("resume-{}", state.turns)
            );
            assert!(
                !request
                    .headers
                    .contains_key("openai-sentinel-chat-requirements-token")
            );
            assert_eq!(state.resumes + 1, state.turns, "resume is never replayed");
            state.resumes += 1;
            ResponseTemplate::new(200).set_body_raw(
                if changed_parent {
                    state.stream.replace("assistant-1", "foreign-parent")
                } else {
                    state.stream.clone()
                },
                "text/event-stream",
            )
        })
        .mount(server)
        .await;
    backend
}

#[tokio::test]
async fn two_temporary_turns_refresh_the_exact_parent_and_never_read_saved_history() {
    let server = MockServer::start().await;
    preparation(&server).await;
    let backend = conversation(&server, 200, true, false).await;
    let session = session(&server);
    let mut sink = Sink::default();
    for continuation in [false, true] {
        let result = session
            .send(
                request(continuation),
                Vec::new(),
                &mut sink,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.error, None);
        assert_eq!(result.acceptance, Acceptance::Accepted);
        assert_eq!(result.outcome, GenerationOutcome::Completed);
        assert_eq!(result.resolved_model.as_deref(), Some("resolved-model"));
        assert!(result.transmission_started && result.continuation_eligible);
    }
    assert!(sink.accepted);
    assert_eq!(backend.lock().unwrap().turns, 2);
    assert_eq!(backend.lock().unwrap().resumes, 1);
    assert!(server.received_requests().await.unwrap().iter().all(|r| {
        r.url.path() != "/backend-api/conversations"
            && !r.url.path().starts_with("/backend-api/conversation/")
            || r.url.path() == "/backend-api/conversation/init"
    }));
}

#[tokio::test]
async fn stale_parent_and_duplicate_identifiers_are_rejected_before_preparation() {
    let server = MockServer::start().await;
    preparation(&server).await;
    let _ = conversation(&server, 200, true, false).await;
    let session = session(&server);
    let first = request(false);
    let ids = (first.submission_id, first.client_message_id);
    session
        .send(
            first,
            Vec::new(),
            &mut Sink::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let count = server.received_requests().await.unwrap().len();
    for kind in 0..4 {
        let mut next = request(true);
        match kind {
            0 => next.expected_parent_id = "changed-parent".into(),
            1 => next.submission_id = ids.0,
            2 => next.client_message_id = ids.1,
            _ => next.conversation_id = None,
        }
        let error = session
            .send(
                next,
                Vec::new(),
                &mut Sink::default(),
                &CancellationToken::new(),
            )
            .await
            .err();
        assert_eq!(
            error,
            Some(if kind == 0 || kind == 3 {
                ChatError::BranchConflict
            } else {
                ChatError::PendingSubmission
            })
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), count);
}

#[tokio::test]
async fn authoritative_parent_conflict_never_posts_followup_or_reuses_resume_capability() {
    let server = MockServer::start().await;
    preparation(&server).await;
    let backend = conversation(&server, 200, true, true).await;
    let session = session(&server);
    session
        .send(
            request(false),
            Vec::new(),
            &mut Sink::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let result = session
        .send(
            request(true),
            Vec::new(),
            &mut Sink::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.error, Some(ChatError::BranchConflict));
    assert!(!result.transmission_started);
    let count = server.received_requests().await.unwrap().len();
    assert_eq!(
        session
            .send(
                request(true),
                Vec::new(),
                &mut Sink::default(),
                &CancellationToken::new()
            )
            .await
            .err(),
        Some(ChatError::UnsupportedCapability)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), count);
    assert_eq!(backend.lock().unwrap().turns, 1);
    assert_eq!(backend.lock().unwrap().resumes, 1);
}

#[tokio::test]
async fn incomplete_transmissions_keep_acceptance_and_prevent_duplicate_retry() {
    for (status, acceptance) in [(500, Acceptance::Unknown), (200, Acceptance::Accepted)] {
        let server = MockServer::start().await;
        preparation(&server).await;
        let backend = conversation(&server, status, false, false).await;
        let session = session(&server);
        let result = session
            .send(
                request(false),
                Vec::new(),
                &mut Sink::default(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(result.error.is_some());
        assert!(result.transmission_started);
        assert!(!result.continuation_eligible);
        assert_eq!(result.acceptance, acceptance);
        assert_eq!(result.outcome, GenerationOutcome::Incomplete);
        assert_eq!(
            session
                .send(
                    request(false),
                    Vec::new(),
                    &mut Sink::default(),
                    &CancellationToken::new()
                )
                .await
                .err(),
            Some(ChatError::PendingSubmission)
        );
        assert_eq!(backend.lock().unwrap().turns, 1);
        assert_eq!(backend.lock().unwrap().resumes, 0);
    }
}

#[tokio::test]
async fn local_cancellation_preserves_accepted_messages_and_never_claims_server_stop() {
    let server = MockServer::start().await;
    preparation(&server).await;
    let _ = conversation(&server, 200, false, false).await;
    let session = session(&server);
    let mut sink = Sink {
        checkpoint_error: Some(ChatError::LocalCancellation),
        ..Default::default()
    };
    let result = session
        .send(
            request(false),
            Vec::new(),
            &mut sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.error, Some(ChatError::LocalCancellation));
    assert_eq!(result.acceptance, Acceptance::Accepted);
    assert_eq!(result.messages[0].role, "user");
    assert!(lock(&session.memory).pending);
}

#[tokio::test]
async fn pre_send_cancellation_overlap_and_invalid_input_have_no_network_side_effects() {
    let server = MockServer::start().await;
    let session = session(&server);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        session
            .send(request(false), Vec::new(), &mut Sink::default(), &cancel)
            .await
            .err(),
        Some(ChatError::LocalCancellation)
    );
    let guard = session.sending.try_acquire().unwrap();
    assert_eq!(
        session
            .send(
                request(false),
                Vec::new(),
                &mut Sink::default(),
                &CancellationToken::new()
            )
            .await
            .err(),
        Some(ChatError::PreparationInProgress)
    );
    drop(guard);
    let mut invalid = request(false);
    invalid.model = Some("invalid/model".into());
    assert_eq!(
        session
            .send(
                invalid,
                Vec::new(),
                &mut Sink::default(),
                &CancellationToken::new()
            )
            .await
            .err(),
        Some(ChatError::InvalidInput)
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn stop_releases_temporary_receipt_tokens_and_revokes_the_session() {
    let server = MockServer::start().await;
    preparation(&server).await;
    let _ = conversation(&server, 200, true, false).await;
    let session = session(&server);
    session
        .send(
            request(false),
            Vec::new(),
            &mut Sink::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(lock(&session.memory).receipt.is_some());
    session.stop();
    let memory = lock(&session.memory);
    assert!(
        memory.receipt.is_none() && memory.submissions.is_empty() && memory.message_ids.is_empty()
    );
    drop(memory);
    assert_eq!(session.check_current(), Err(ChatError::LocalCancellation));
}

#[tokio::test]
async fn failed_before_transmission_cannot_replay_a_consumed_conduit() {
    let server = MockServer::start().await;
    preparation(&server).await;
    let backend = conversation(&server, 200, true, false).await;
    let session = session(&server);
    session
        .send(
            request(false),
            Vec::new(),
            &mut Sink::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut sink = Sink {
        before_error: Some(ChatError::LocalCancellation),
        ..Default::default()
    };
    let result = session
        .send(
            request(true),
            Vec::new(),
            &mut sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.error, Some(ChatError::LocalCancellation));
    assert!(!result.transmission_started);
    assert_eq!(
        session
            .send(
                request(true),
                Vec::new(),
                &mut Sink::default(),
                &CancellationToken::new()
            )
            .await
            .err(),
        Some(ChatError::UnsupportedCapability)
    );
    assert_eq!(backend.lock().unwrap().turns, 1);
    assert_eq!(backend.lock().unwrap().resumes, 1);
}

#[tokio::test]
async fn dropping_a_transmitting_future_keeps_its_reservation_and_never_replays() {
    let server = MockServer::start().await;
    preparation(&server).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let notification = Arc::clone(&entered);
    Mock::given(method("POST"))
        .and(path("/backend-api/f/conversation"))
        .respond_with(move |_: &wiremock::Request| {
            notification.notify_one();
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(30))
                .set_body_raw("data: [DONE]\n\n", "text/event-stream")
        })
        .expect(1)
        .mount(&server)
        .await;
    let session = Arc::new(session(&server));
    let task_session = Arc::clone(&session);
    let task = tokio::spawn(async move {
        task_session
            .send(
                request(false),
                Vec::new(),
                &mut Sink::default(),
                &CancellationToken::new(),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.err().unwrap().is_cancelled());
    let count = server.received_requests().await.unwrap().len();
    {
        let memory = lock(&session.memory);
        assert!(memory.pending && memory.receipt.is_none());
        assert_eq!(memory.submissions.len(), 1);
        assert_eq!(memory.message_ids.len(), 1);
    }
    assert_eq!(
        session
            .send(
                request(false),
                Vec::new(),
                &mut Sink::default(),
                &CancellationToken::new()
            )
            .await
            .err(),
        Some(ChatError::PendingSubmission)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), count);
}
