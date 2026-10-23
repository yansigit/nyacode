use super::*;
use codex_login::CodexAuth;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;
use tokio::sync::oneshot;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

async fn session(server: &MockServer) -> SavedChatSession {
    let auth =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let generation = auth.auth_change_state_receiver().borrow().generation;
    let mut probe = ChatProbe::new(auth, true, Some("account_id")).unwrap();
    probe.set_test_backend(url::Url::parse(&format!("{}/backend-api/", server.uri())).unwrap());
    probe.native_device.set(Uuid::new_v4().to_string()).unwrap();
    let _ = probe.bootstrap_request().await.unwrap();
    SavedChatSession {
        probe,
        id: Uuid::new_v4(),
        generation,
        preparation: Semaphore::new(1),
    }
}

fn prepared(session: &SavedChatSession, server: &MockServer) -> PreparedSavedTurn {
    let profile = session
        .probe
        .native_profile(
            crate::sentinel::Bootstrap {
                origin: server.uri(),
                build: "fixture-build".into(),
                scripts: vec!["https://example.invalid/fixture.js".into()],
                router: Err(crate::SentinelFailure::UnsupportedRouterContext),
            },
            session.generation,
        )
        .unwrap();
    let request = SavedTurnRequest {
        submission_id: Uuid::new_v4(),
        client_message_id: Uuid::new_v4(),
        draft_id: Uuid::new_v4(),
        conversation_id: None,
        expected_parent_id: Uuid::new_v4().to_string(),
        model: None,
        text: "literal ! @ $ fixture".into(),
        created_at_ms: 1_700_000_000_000,
    };
    let native = PreparedTurn {
        profile,
        generation: session.generation,
        target: PreparationTarget {
            parent: request.expected_parent_id.clone(),
            conversation: None,
            model: None,
            temporary: false,
        },
        model: "fixture-model".into(),
        conduit: "fixture-conduit".into(),
        requirements: "fixture-requirements".into(),
        prepare_token: "fixture-prepare".into(),
        proof: None,
        turnstile: None,
        observer: None,
    };
    PreparedSavedTurn {
        session_id: session.id,
        prepared: native,
        request,
        uploads: Vec::new(),
    }
}

struct Sink {
    committed: Arc<AtomicBool>,
    started: Arc<Notify>,
    before_gate: Option<oneshot::Receiver<()>>,
    before_error: Option<ChatError>,
    checkpoint_started: Arc<Notify>,
    checkpoint_gate: Option<oneshot::Receiver<()>>,
    seen_acceptance: Acceptance,
}

impl Default for Sink {
    fn default() -> Self {
        Self {
            committed: Arc::new(AtomicBool::new(false)),
            started: Arc::new(Notify::new()),
            before_gate: None,
            before_error: None,
            checkpoint_started: Arc::new(Notify::new()),
            checkpoint_gate: None,
            seen_acceptance: Acceptance::Unknown,
        }
    }
}

impl TurnSink for Sink {
    async fn before_transmission(&mut self, _: Uuid, model: &str) -> Result<(), ChatError> {
        assert_eq!(model, "fixture-model");
        self.started.notify_one();
        if let Some(gate) = self.before_gate.take() {
            gate.await.map_err(|_| ChatError::RecoveryUnavailable)?;
        }
        if let Some(error) = self.before_error {
            return Err(error);
        }
        self.committed.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn checkpoint(&mut self, state: &TurnState) -> Result<(), ChatError> {
        self.seen_acceptance = state.acceptance;
        self.checkpoint_started.notify_one();
        if let Some(gate) = self.checkpoint_gate.take() {
            gate.await.map_err(|_| ChatError::RecoveryUnavailable)?;
        }
        Ok(())
    }
}

async fn mount_stream(
    server: &MockServer,
    prepared: &PreparedSavedTurn,
    committed: Arc<AtomicBool>,
) {
    let message_id = prepared.request.client_message_id.to_string();
    let parent = prepared.request.expected_parent_id.clone();
    Mock::given(method("POST")).and(path("/backend-api/f/conversation"))
        .respond_with(move |request:&wiremock::Request| {
            assert!(committed.load(Ordering::SeqCst),"the asynchronous journal must commit before transmission");
            let body:Value=request.body_json().unwrap();
            assert_eq!(body["history_and_training_disabled"],false);
            assert_eq!(body["messages"][0]["id"],message_id);
            assert_eq!(body["messages"][0]["content"]["parts"][0],"literal ! @ $ fixture");
            assert_eq!(body["parent_message_id"],parent);
            let user=json!({"conversation_id":"fixture-conversation","message":{"id":message_id,"author":{"role":"user"},"content":{"content_type":"text","parts":["fixture"]}}});
            let assistant=json!({"conversation_id":"fixture-conversation","message":{"id":"fixture-assistant","author":{"role":"assistant"},"content":{"content_type":"text","parts":["response"]},"status":"finished_successfully","end_turn":true,"metadata":{"model_slug":"resolved-fixture"}}});
            ResponseTemplate::new(200).set_body_raw(format!("data: {user}\n\ndata: {assistant}\n\ndata: {{\"type\":\"message_stream_complete\"}}\n\ndata: [DONE]\n\n"),"text/event-stream")
        }).expect(1).mount(server).await;
}

#[tokio::test]
async fn send_waits_for_durable_async_checkpoint_and_keeps_literal_input() {
    let server = MockServer::start().await;
    let session = session(&server).await;
    let prepared = prepared(&session, &server);
    let (release, wait) = oneshot::channel();
    let mut sink = Sink {
        before_gate: Some(wait),
        ..Default::default()
    };
    let started = Arc::clone(&sink.started);
    mount_stream(&server, &prepared, Arc::clone(&sink.committed)).await;
    let cancel = CancellationToken::new();
    let sending = session.send(prepared, &mut sink, &cancel);
    let control = async {
        started.notified().await;
        assert!(server.received_requests().await.unwrap().is_empty());
        release.send(()).unwrap();
    };
    let (result, ()) = tokio::join!(sending, control);
    assert_eq!(result.error, None);
    assert!(result.transmission_started);
    assert_eq!(result.acceptance, Acceptance::Accepted);
    assert_eq!(result.outcome, GenerationOutcome::Completed);
    assert_eq!(result.messages.last().unwrap().text(), "response");
    assert_eq!(result.resolved_model.as_deref(), Some("resolved-fixture"));
    assert_eq!(sink.seen_acceptance, Acceptance::Accepted);
}

#[tokio::test]
async fn failed_journal_and_pre_send_cancellation_never_post() {
    let server = MockServer::start().await;
    let session = session(&server).await;
    let mut sink = Sink {
        before_error: Some(ChatError::RecoveryUnavailable),
        ..Default::default()
    };
    let result = session
        .send(
            prepared(&session, &server),
            &mut sink,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(result.error, Some(ChatError::RecoveryUnavailable));
    assert!(!result.transmission_started);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = session
        .send(prepared(&session, &server), &mut Sink::default(), &cancel)
        .await;
    assert_eq!(result.error, Some(ChatError::LocalCancellation));
    assert!(!result.transmission_started);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_during_async_checkpoint_preserves_accepted_state() {
    let server = MockServer::start().await;
    let session = session(&server).await;
    let prepared = prepared(&session, &server);
    let (_release, wait) = oneshot::channel();
    let mut sink = Sink {
        checkpoint_gate: Some(wait),
        ..Default::default()
    };
    let checkpoint_started = Arc::clone(&sink.checkpoint_started);
    mount_stream(&server, &prepared, Arc::clone(&sink.committed)).await;
    let cancel = CancellationToken::new();
    let (result, ()) = tokio::join!(session.send(prepared, &mut sink, &cancel), async {
        checkpoint_started.notified().await;
        cancel.cancel();
    });
    assert_eq!(result.error, Some(ChatError::LocalCancellation));
    assert_eq!(result.acceptance, Acceptance::Accepted);
    assert!(result.transmission_started);
    assert!(!result.messages.is_empty());
}

#[tokio::test]
async fn changed_parent_and_cross_session_preparation_cannot_reach_journal_or_post() {
    let server = MockServer::start().await;
    let first = session(&server).await;
    let second = session(&server).await;
    let mut sink = Sink::default();
    let result = second
        .send(
            prepared(&first, &server),
            &mut sink,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(result.error, Some(ChatError::SessionChanged));
    assert!(!sink.committed.load(Ordering::SeqCst));
    let mut turn = prepared(&first, &server);
    turn.request.conversation_id = Some("fixture-conversation".into());
    Mock::given(method("GET"))
        .and(path("/backend-api/conversation/fixture-conversation"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"current_node":"changed","mapping":{"changed":{"parent":null,"message":null}}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let result = first.send(turn, &mut sink, &CancellationToken::new()).await;
    assert_eq!(result.error, Some(ChatError::BranchConflict));
    assert!(!sink.committed.load(Ordering::SeqCst));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
