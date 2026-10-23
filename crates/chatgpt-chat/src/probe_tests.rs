use super::*;
use crate::PreparationOperation;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

fn probe(server: &MockServer) -> ChatProbe {
    let auth =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let mut probe = ChatProbe::new(auth, true, Some("account_id")).unwrap();
    probe.backend = Url::parse(&format!("{}/backend-api/", server.uri())).unwrap();
    probe
}

#[test]
fn transport_timeouts_and_revocation_keep_distinct_error_categories() {
    assert_eq!(transport_error(HttpError::Timeout), ChatError::Timeout);
    assert_eq!(
        transport_error(HttpError::Policy(
            codex_http_client::NetworkPolicyDenied::Revoked
        )),
        ChatError::NetworkPolicy
    );
    assert_eq!(
        transport_error(HttpError::Build("redacted".into())),
        ChatError::Transport
    );
}

#[tokio::test]
async fn normalized_reads_reject_temporary_snapshots_and_invalid_paths() {
    let server = MockServer::start().await;
    let probe = probe(&server);
    Mock::given(method("GET"))
        .and(path("/backend-api/conversations"))
        .and(query_param("offset", "5"))
        .and(query_param("limit", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"offset":5,"total":6,"items":[{"id":"saved","title":"Fixture title"}]}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/backend-api/conversation/saved"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"conversation_id":"saved","current_node":null,"mapping":{}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/backend-api/conversation/temporary"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"is_temporary_chat":true,"title":"private fixture"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let page = probe.conversation_list(5, 2).await.unwrap();
    assert_eq!(page.items[0].id, "saved");
    assert_eq!(page.next_offset, None);
    assert_eq!(probe.conversation_read("saved").await.unwrap().id, "saved");
    assert!(matches!(
        probe.conversation_read("temporary").await,
        Err(ChatError::UnsupportedCapability)
    ));
    assert!(matches!(
        probe.conversation_read("../untrusted").await,
        Err(ChatError::InvalidInput)
    ));
    assert!(matches!(
        probe.conversation_list(0, 101).await,
        Err(ChatError::InvalidInput)
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[cfg(unix)]
#[tokio::test]
async fn saved_send_is_journaled_before_post_and_ambiguous_failures_never_replay() {
    use crate::journal::RunJournal;
    use crate::stream::Acceptance;
    use crate::stream::GenerationOutcome;
    use base64::Engine;
    for status in [200, 500] {
        let home =
            std::env::temp_dir().join(format!("chat-saved-fixture-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&home).unwrap();
        let claims = base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(
            json!({"https://api.openai.com/auth":{"chatgpt_user_id":"fixture-user"}}).to_string(),
        );
        let auth = CodexAuth::from_external_chatgpt_tokens(
            &format!("header.{claims}.signature"),
            "account_id",
            None,
        )
        .unwrap();
        let auth = AuthManager::from_auth_for_testing_with_home(auth, home.clone());
        let server = MockServer::start().await;
        let mut probe = ChatProbe::new(auth.clone(), true, None).unwrap();
        probe.backend = Url::parse(&format!("{}/backend-api/", server.uri())).unwrap();
        let mut journal = RunJournal::create(&home, probe.run_scope().unwrap()).unwrap();
        let run_id = journal.run_id().to_owned();
        journal
            .begin(None, "fixture-parent".into(), "fixture-text".into())
            .unwrap();
        let id = journal.current().unwrap().client_message_id.clone();
        native_bootstrap(&server).await;
        Mock::given(method("POST"))
            .and(path("/backend-api/f/conversation/prepare"))
            .respond_with(|request: &wiremock::Request| {
                let body: serde_json::Value = request.body_json().unwrap();
                assert_eq!(body["parent_message_id"], "fixture-parent");
                assert_eq!(body["history_and_training_disabled"], false);
                ResponseTemplate::new(200).set_body_json(json!({"conduit_token":"private-conduit"}))
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/sentinel/chat-requirements/prepare"))
            .respond_with(|request: &wiremock::Request| {
                native_challenge(request, &json!([[3, "fixture-turnstile"]]))
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/sentinel/chat-requirements/finalize"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"token":"private-final-token"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let user = json!({"id":id,"author":{"role":"user"},"content":{"content_type":"text","parts":["fixture-text"]}});
        let assistant = json!({"id":"fixture-assistant","author":{"role":"assistant"},"status":"finished_successfully","end_turn":true,"content":{"content_type":"text","parts":["fixture-reply"]},"metadata":{"model_slug":"fixture-model"}});
        let stream = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"type":"input_message","conversation_id":"fixture-conversation","input_message":user}),
            json!({"conversation_id":"fixture-conversation","message":assistant}),
            json!({"type":"message_stream_complete"})
        );
        let journal_path = home
            .join("chat-probe-runs")
            .join(&run_id)
            .join("journal.json");
        let expected_id = id.clone();
        Mock::given(method("POST"))
            .and(path("/backend-api/f/conversation"))
            .and(header(
                "OpenAI-Sentinel-Chat-Requirements-Token",
                "private-final-token",
            ))
            .and(header("x-conduit-token", "private-conduit"))
            .respond_with(move |request: &wiremock::Request| {
                let record: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
                assert_eq!(record["attempts"][0]["transmission_started"], true);
                let body: serde_json::Value = request.body_json().unwrap();
                assert_eq!(body["parent_message_id"], "fixture-parent");
                assert_eq!(body["messages"][0]["id"], expected_id);
                assert_eq!(body["history_and_training_disabled"], false);
                ResponseTemplate::new(status).set_body_raw(stream.clone(), "text/event-stream")
            })
            .expect(1)
            .mount(&server)
            .await;
        if status == 200 {
            Mock::given(method("GET"))
                .and(path("/backend-api/conversation/fixture-conversation"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    json!({"current_node":"fixture-assistant","mapping":{
                        "fixture-parent":{"parent":null,"message":null},
                        (id.clone()):{"parent":"fixture-parent","message":user},
                        "fixture-assistant":{"parent":id,"message":assistant}
                    }}),
                ))
                .expect(2)
                .mount(&server)
                .await;
        }
        let observation = probe.send_saved(&mut journal).await;
        if status == 200 {
            assert_eq!(observation.error, None);
            assert_eq!(observation.acceptance, Acceptance::Accepted);
            assert_eq!(observation.generation, GenerationOutcome::Completed);
            assert!(observation.authoritative_retrieval);
        } else {
            assert_eq!(observation.error, Some(ChatError::Transport));
            assert_eq!(observation.acceptance, Acceptance::Unknown);
        }
        let diagnostic = serde_json::to_string(&observation).unwrap();
        for content in [
            "private-",
            "fixture-text",
            "fixture-reply",
            "fixture-conversation",
            "fixture-assistant",
        ] {
            assert!(!diagnostic.contains(content));
        }
        drop(journal);
        drop(probe);
        let mut probe = ChatProbe::new(auth, true, None).unwrap();
        probe.backend = Url::parse(&format!("{}/backend-api/", server.uri())).unwrap();
        let mut journal = RunJournal::resume(&home, &run_id, &probe.run_scope().unwrap()).unwrap();
        assert_eq!(
            probe.send_saved(&mut journal).await.error,
            Some(ChatError::PendingSubmission)
        );
        assert_eq!(
            probe.reconcile_saved(&mut journal).await,
            if status == 200 {
                Ok(())
            } else {
                Err(ChatError::PendingSubmission)
            }
        );
        drop(journal);
        drop(probe);
        std::fs::remove_dir_all(home).unwrap();
    }
}

#[tokio::test]
async fn temporary_continuation_requires_authoritative_temporary_parent_and_never_saves() {
    use crate::TemporaryProtocol;
    for (temporary, protocol) in [
        (true, TemporaryProtocol::HistoryRead),
        (false, TemporaryProtocol::HistoryRead),
        (true, TemporaryProtocol::ConduitRefreshV1),
        (false, TemporaryProtocol::ConduitRefreshV1),
    ] {
        let conduit = protocol == TemporaryProtocol::ConduitRefreshV1;
        let server = MockServer::start().await;
        let turns = if temporary { 2 } else { 1 };
        Mock::given(method("GET")).and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(r#"<html data-build="fixture"><script src="https://cdn.oaistatic.com/fixture.js"></script></html>"#, "text/html"))
            .expect(turns).mount(&server).await;
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
                .expect(turns)
                .mount(&server)
                .await;
        }
        Mock::given(method("POST")).and(path("/backend-api/f/conversation/prepare"))
            .respond_with(|request: &wiremock::Request| {
                assert_eq!(request.body_json::<serde_json::Value>().unwrap()["history_and_training_disabled"], true);
                ResponseTemplate::new(200).set_body_json(json!({"conduit_token":"fixture-conduit"}))
            }).expect(turns).mount(&server).await;
        let state = Arc::new(Mutex::new((
            json!({"is_temporary_chat":temporary,"mapping":{}}),
            String::new(),
            0,
            String::new(),
        )));
        let sent_state = state.clone();
        Mock::given(method("POST")).and(path("/backend-api/f/conversation"))
            .respond_with(move |request: &wiremock::Request| {
                let body: serde_json::Value = request.body_json().unwrap();
                assert_eq!(body["history_and_training_disabled"], true);
                let mut state = sent_state.lock().unwrap();
                state.2 += 1;
                let user = body["messages"][0].clone();
                let id = user["id"].as_str().unwrap();
                let parent = body["parent_message_id"].as_str().unwrap();
                if state.2 == 1 {
                    state.1 = user["content"]["parts"][0].as_str().unwrap().strip_prefix("Reply with exactly this marker and no other text: ").unwrap().into();
                    assert_eq!(body["temporary_chat_requests_personalization"], false);
                    state.0["mapping"][parent] = json!({"parent":null,"message":null});
                } else {
                    assert_eq!(body["conversation_id"], "fixture-temporary");
                    assert_eq!(state.0["current_node"], parent);
                }
                let assistant_id = format!("fixture-assistant-{}", state.2);
                let assistant = json!({"id":assistant_id,"author":{"role":"assistant"},"content":{"content_type":"text","parts":[state.1]},"status":"finished_successfully","end_turn":true,"metadata":{"model_slug":"fixture-model"}});
                state.0["mapping"][id] = json!({"parent":parent,"message":user});
                state.0["mapping"][&assistant_id] = json!({"parent":id,"message":assistant});
                state.0["current_node"] = json!(assistant_id);
                let stream = format!("data: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n", json!({"type":"resume_conversation_token","conversation_id":"fixture-temporary","token":"fixture-resume-token"}), json!({"type":"input_message","conversation_id":"fixture-temporary","input_message":user}), json!({"conversation_id":"fixture-temporary","message":assistant}), json!({"type":"message_stream_complete"}));
                state.3 = stream.clone();
                ResponseTemplate::new(200).set_body_raw(stream,"text/event-stream")
            }).expect(turns).mount(&server).await;
        let read_state = state.clone();
        Mock::given(method("GET"))
            .and(path("/backend-api/conversation/fixture-temporary"))
            .respond_with(move |_: &wiremock::Request| {
                if conduit {
                    ResponseTemplate::new(404)
                } else {
                    ResponseTemplate::new(200).set_body_json(read_state.lock().unwrap().0.clone())
                }
            })
            .expect(if conduit {
                if temporary { 2 } else { 0 }
            } else if temporary {
                4
            } else {
                1
            })
            .mount(&server)
            .await;
        if conduit {
            let resume_state = state.clone();
            Mock::given(method("POST"))
                .and(path("/backend-api/f/conversation/resume"))
                .and(header("x-conduit-token", "fixture-resume-token"))
                .respond_with(move |request: &wiremock::Request| {
                    assert_eq!(
                        request.body_json::<serde_json::Value>().unwrap(),
                        json!({"conversation_id":"fixture-temporary","offset":0})
                    );
                    assert!(
                        !request
                            .headers
                            .contains_key("OpenAI-Sentinel-Chat-Requirements-Token")
                    );
                    let stream = resume_state.lock().unwrap().3.clone();
                    ResponseTemplate::new(200).set_body_raw(
                        if temporary {
                            stream
                        } else {
                            stream.replace("fixture-assistant-1", "changed-assistant")
                        },
                        "text/event-stream",
                    )
                })
                .expect(turns)
                .mount(&server)
                .await;
        }
        if temporary {
            Mock::given(method("GET"))
                .and(path("/backend-api/conversations"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[]})))
                .expect(1)
                .mount(&server)
                .await;
        }
        let observation = probe(&server).inspect_temporary(protocol).await;
        assert_eq!(
            observation.error,
            if temporary {
                None
            } else if conduit {
                Some(ChatError::ProtocolChanged)
            } else {
                Some(ChatError::UnsupportedCapability)
            }
        );
        assert_eq!(
            observation.server_temporary,
            if conduit { None } else { Some(temporary) }
        );
        assert_eq!(observation.turns.len(), turns as usize);
        let diagnostic = serde_json::to_string(&observation).unwrap();
        for text in [
            "NYACODE_NATIVE_TEMPORARY_",
            "fixture-temporary",
            "fixture-assistant",
            "fixture-resume-token",
        ] {
            assert!(!diagnostic.contains(text));
        }
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| !request.url.path().ends_with("/save"))
        );
    }
}

#[tokio::test]
async fn native_profile_and_device_are_scoped_to_the_session_generation() {
    let server = MockServer::start().await;
    let probe = probe(&server);
    let _request = probe.bootstrap_request().await.unwrap();
    let bootstrap = || crate::sentinel::Bootstrap {
        origin: server.uri(),
        build: "fixture".into(),
        scripts: vec!["https://cdn.oaistatic.com/fixture.js".into()],
        router: Err(crate::SentinelFailure::UnsupportedRouterContext),
    };
    let generation = probe.auth.auth_change_state_receiver().borrow().generation;
    let first = probe.native_profile(bootstrap(), generation).unwrap();
    let second = probe.native_profile(bootstrap(), generation).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(probe.check_profile(&first), Ok(()));
    assert!(matches!(
        probe.native_profile(bootstrap(), generation + 1),
        Err(ChatError::SessionChanged)
    ));
    let previous_session = probe.transport_session_id(generation).unwrap();
    probe.clear_session();
    let _request = probe.bootstrap_request().await.unwrap();
    let replacement = probe.native_profile(bootstrap(), generation).unwrap();
    assert_ne!(
        probe.transport_session_id(generation).unwrap(),
        previous_session
    );
    assert_eq!(probe.check_profile(&first), Err(ChatError::SessionChanged));
    assert_eq!(probe.check_profile(&replacement), Ok(()));
    let weak = Arc::downgrade(&replacement);
    drop(replacement);
    drop(first);
    drop(second);
    probe.stop();
    assert!(weak.upgrade().is_none());
    assert!(server.received_requests().await.unwrap().is_empty());
}

struct FixtureExternalAuth(CodexAuth);

impl codex_login::ExternalAuth for FixtureExternalAuth {
    fn resolve(&self) -> codex_login::ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(self.0.clone()) })
    }

    fn refresh(
        &self,
        _: codex_login::ExternalAuthRefreshContext,
    ) -> codex_login::ExternalAuthFuture<'_, CodexAuth> {
        self.resolve()
    }
}

#[tokio::test]
async fn refresh_cancels_preparation_without_changing_account_ownership() {
    use base64::Engine;
    let auth_value = |revision| {
        let claims = json!({"jti":revision,"https://api.openai.com/auth":{"chatgpt_user_id":"fixture-user"}});
        let claims = base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(claims.to_string());
        CodexAuth::from_external_chatgpt_tokens(
            &format!("header.{claims}.signature"),
            "account_id",
            None,
        )
        .unwrap()
    };
    let auth = AuthManager::from_auth_for_testing_with_home(
        auth_value(1),
        std::env::temp_dir().join(format!("native-chat-auth-fixture-{}", uuid::Uuid::new_v4())),
    );
    let server = MockServer::start().await;
    let mut probe = ChatProbe::new(auth, true, None).unwrap();
    probe.backend = Url::parse(&format!("{}/backend-api/", server.uri())).unwrap();
    let _request = probe.bootstrap_request().await.unwrap();
    let before = *probe.auth.auth_change_state_receiver().borrow();
    let worker_cancel = CancellationToken::new();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (result, ()) = tokio::join!(
        probe.guarded_generation(before.generation, async {
            let _cancel_on_drop = worker_cancel.clone().drop_guard();
            started.send(()).unwrap();
            std::future::pending::<Result<(), ChatError>>().await
        }),
        async {
            ready.await.unwrap();
            probe
                .auth
                .set_external_auth(Arc::new(FixtureExternalAuth(auth_value(2))))
                .await
                .unwrap();
        }
    );
    assert_eq!(result, Err(ChatError::SessionChanged));
    assert!(worker_cancel.is_cancelled());
    let after = *probe.auth.auth_change_state_receiver().borrow();
    assert_eq!(after.owner_generation, before.owner_generation);
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(probe.check_owner(), Ok(()));
    let _request = probe.bootstrap_request().await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(
        probe.session.lock().unwrap().as_ref().unwrap().generation,
        after.generation
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

async fn native_bootstrap(server: &MockServer) {
    Mock::given(method("GET")).and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"<html data-build="fixture-build"><script src="https://cdn.oaistatic.com/fixture.js"></script></html>"#,
            "text/html",
        )).expect(1).mount(server).await;
    Mock::given(method("POST"))
        .and(path("/backend-api/conversation/init"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"default_model_slug":"fixture-model"})),
        )
        .expect(1)
        .mount(server)
        .await;
}

fn native_challenge(request: &wiremock::Request, program: &serde_json::Value) -> ResponseTemplate {
    use base64::Engine;
    let body: serde_json::Value = request.body_json().unwrap();
    let p = body["p"].as_str().unwrap();
    assert!(p.starts_with("gAAAAAC"));
    let dx: Vec<_> = program
        .to_string()
        .bytes()
        .enumerate()
        .map(|(i, b)| b ^ p.as_bytes()[i % p.len()])
        .collect();
    ResponseTemplate::new(200).set_body_json(json!({
        "prepare_token":"private-prepare-token",
        "proofofwork":{"required":true,"seed":"fixture-seed","difficulty":"f"},
        "turnstile":{"required":true,"dx":base64::prelude::BASE64_STANDARD.encode(dx)},
        "so":{"required":false}
    }))
}

#[tokio::test]
async fn native_preparation_binds_challenges_and_never_sends_a_message() {
    let server = MockServer::start().await;
    native_bootstrap(&server).await;
    Mock::given(method("POST"))
        .and(path("/backend-api/sentinel/chat-requirements/prepare"))
        .respond_with(|request: &wiremock::Request| {
            native_challenge(request, &json!([[3, "fixture-turnstile"]]))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/sentinel/chat-requirements/finalize"))
        .respond_with(|request: &wiremock::Request| {
            use base64::Engine;
            let body: serde_json::Value = request.body_json().unwrap();
            assert_eq!(body["prepare_token"], "private-prepare-token");
            assert!(body["proofofwork"].as_str().unwrap().starts_with("gAAAAAB"));
            assert_eq!(
                body["turnstile"],
                base64::prelude::BASE64_STANDARD.encode("fixture-turnstile")
            );
            ResponseTemplate::new(200)
                .set_body_json(json!({"token":"private-final-token","expire_after":120}))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/f/conversation/prepare"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap();
            assert_eq!(body["model"], "fixture-model");
            assert_eq!(body["history_and_training_disabled"], true);
            assert!(uuid::Uuid::parse_str(body["parent_message_id"].as_str().unwrap()).is_ok());
            ResponseTemplate::new(200).set_body_json(json!({"conduit_token":"private-conduit"}))
        })
        .expect(1)
        .mount(&server)
        .await;
    let steps = probe(&server).inspect_native_preparation(true).await;
    assert_eq!(steps.len(), 6);
    assert_eq!(
        steps[2].operation,
        PreparationOperation::ConversationPrepare
    );
    assert_eq!(steps[3].operation, PreparationOperation::SentinelPrepare);
    assert!(steps.iter().all(|s| s.error.is_none()), "{steps:?}");
    assert!(!serde_json::to_string(&steps).unwrap().contains("private"));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 5);
    assert!(
        requests
            .iter()
            .all(|r| r.url.path() != "/backend-api/f/conversation")
    );
    let device = requests[1].headers.get("oai-device-id").unwrap();
    assert!(
        requests[1..]
            .iter()
            .all(|r| r.headers.get("oai-device-id") == Some(device))
    );
    assert!(!requests[0].headers.contains_key("authorization"));
}

#[tokio::test]
async fn native_interpreter_failure_is_redacted_and_stops_finalization() {
    let server = MockServer::start().await;
    native_bootstrap(&server).await;
    Mock::given(method("POST"))
        .and(path("/backend-api/f/conversation/prepare"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"conduit_token":"private-conduit"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/sentinel/chat-requirements/prepare"))
        .respond_with(|request: &wiremock::Request| {
            native_challenge(
                request,
                &json!([[99, "private-operand"], [3, "fake-token"]]),
            )
        })
        .expect(1)
        .mount(&server)
        .await;
    let probe = probe(&server);
    let steps = probe.inspect_native_preparation(false).await;
    assert_eq!(
        steps.last().unwrap().native_failure,
        Some(crate::SentinelFailure::UnknownInstruction)
    );
    assert_eq!(
        steps.last().unwrap().error,
        Some(ChatError::UnsupportedChallenge)
    );
    assert!(!serde_json::to_string(&steps).unwrap().contains("private"));
    assert_eq!(
        probe.inspect_native_preparation(false).await[0].error,
        Some(ChatError::UnsupportedChallenge)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn unknown_or_incomplete_observer_never_reaches_finalization() {
    for (required, expected) in [
        (json!(true), ChatError::ProtocolChanged),
        (json!(null), ChatError::ProtocolChanged),
    ] {
        let server = MockServer::start().await;
        native_bootstrap(&server).await;
        Mock::given(method("POST"))
            .and(path("/backend-api/f/conversation/prepare"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"conduit_token":"fixture"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/sentinel/chat-requirements/prepare"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "prepare_token":"private",
                "proofofwork":{"required":false},
                "turnstile":{"required":false},
                "so":{"required":required}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let steps = probe(&server).inspect_native_preparation(false).await;
        assert_eq!(steps.last().unwrap().error, Some(expected));
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }
}

#[tokio::test]
async fn native_observer_collects_before_finalize_and_snapshots_without_exposing_tokens() {
    for malformed_snapshot in [false, true] {
        let server = MockServer::start().await;
        native_bootstrap(&server).await;
        Mock::given(method("POST"))
            .and(path("/backend-api/f/conversation/prepare"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"conduit_token":"private"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/sentinel/chat-requirements/prepare"))
            .respond_with(move |request: &wiremock::Request| {
                use base64::Engine;
                let body: serde_json::Value = request.body_json().unwrap();
                let p = body["p"].as_str().unwrap();
                let encode = |program: serde_json::Value| {
                    let bytes: Vec<_> = program
                        .to_string()
                        .bytes()
                        .enumerate()
                        .map(|(i, b)| b ^ p.as_bytes()[i % p.len()])
                        .collect();
                    base64::prelude::BASE64_STANDARD.encode(bytes)
                };
                let collector = encode(json!([[3, "private-collector"], [2, 50, "private-state"]]));
                let snapshot = encode(if malformed_snapshot {
                    json!([[999, "private"]])
                } else {
                    // Resolves only when the post-resolution collector write
                    // survived; fresh VM state would leave this unresolved.
                    json!([
                        [2, 51, "private-state"],
                        [20, 50, 51, 3, "private-snapshot"]
                    ])
                });
                ResponseTemplate::new(200).set_body_json(json!({
                    "prepare_token":"private-prepare",
                    "proofofwork":{"required":false},"turnstile":{"required":false},
                    "so":{"required":true,"collector_dx":collector,"snapshot_dx":snapshot}
                }))
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/backend-api/sentinel/chat-requirements/finalize"))
            .respond_with(|request: &wiremock::Request| {
                assert_eq!(
                    request.body_json::<serde_json::Value>().unwrap(),
                    json!({"prepare_token":"private-prepare"})
                );
                ResponseTemplate::new(200).set_body_json(json!({"token":"private-final"}))
            })
            .expect(1)
            .mount(&server)
            .await;
        let steps = probe(&server).inspect_native_preparation(false).await;
        assert_eq!(
            steps.iter().map(|s| s.operation).collect::<Vec<_>>(),
            vec![
                PreparationOperation::SessionBootstrap,
                PreparationOperation::ConversationInit,
                PreparationOperation::ConversationPrepare,
                PreparationOperation::SentinelPrepare,
                PreparationOperation::SentinelCompute,
                PreparationOperation::SessionObserver,
                PreparationOperation::SentinelFinalize,
                PreparationOperation::SessionObserverSnapshot
            ]
        );
        assert!(steps[..7].iter().all(|s| s.error.is_none()), "{steps:?}");
        assert_eq!(
            steps[7].native_failure,
            malformed_snapshot.then_some(crate::SentinelFailure::UnknownInstruction)
        );
        assert!(!serde_json::to_string(&steps).unwrap().contains("private"));
        assert_eq!(server.received_requests().await.unwrap().len(), 5);
    }
}

#[tokio::test]
async fn disabled_and_api_key_sessions_do_not_make_requests() {
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("secret"));
    assert!(matches!(
        ChatProbe::new(Arc::clone(&auth), false, None),
        Err(ChatError::Disabled)
    ));
    assert!(matches!(
        ChatProbe::new(auth, true, None),
        Err(ChatError::LoginRequired)
    ));
}

#[tokio::test]
async fn designated_account_must_match() {
    let auth =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    assert!(matches!(
        ChatProbe::new(auth, true, Some("other")),
        Err(ChatError::AccountMismatch)
    ));
}

#[tokio::test]
async fn model_probe_uses_managed_auth_and_account_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/models"))
        .and(query_param("history_and_training_disabled", "false"))
        .and(header("authorization", "Bearer Access Token"))
        .and(header("chatgpt-account-id", "account_id"))
        .and(header("x-openai-target-path", "/backend-api/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"categories":[{"default_model":"fixture-model","supported_models":[]}],"models": [{"slug": "fixture-model"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(probe(&server).model_count().await, Ok(1));
}

#[tokio::test]
async fn history_probe_paginates_without_retaining_titles() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/conversations"))
        .and(query_param("offset", "20"))
        .and(query_param("limit", "10"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"items": [{"id": "conversation-a", "title": "private title"}]}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(probe(&server).history_count(20, 10).await, Ok(1));
}

#[tokio::test]
async fn conversation_probe_validates_the_active_branch() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/conversation/conversation-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "current_node": "leaf",
            "mapping": {"leaf": {"parent": null, "message": {"id": "server-id", "author": {"role": "assistant"}}}}
        }))).expect(1).mount(&server).await;
    assert_eq!(
        probe(&server).active_message_count("conversation-a").await,
        Ok(1)
    );
}

#[tokio::test]
async fn invalid_input_is_rejected_before_network_io() {
    let server = MockServer::start().await;
    let probe = probe(&server);
    for id in ["", "../models", "a?token=secret", "a/b", "a%2fb"] {
        assert_eq!(
            probe.active_message_count(id).await,
            Err(ChatError::InvalidInput)
        );
    }
    assert_eq!(
        probe.history_count(0, 0).await,
        Err(ChatError::InvalidInput)
    );
    assert_eq!(
        probe.history_count(0, 101).await,
        Err(ChatError::InvalidInput)
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn transport_errors_never_include_response_contents() {
    for (status, content_type, body, expected) in [
        (
            403,
            "text/html",
            "Bearer private-token",
            ChatError::AccessDenied,
        ),
        (
            429,
            "application/json",
            "private message",
            ChatError::RateLimited,
        ),
        (
            200,
            "application/json",
            "{private-token",
            ChatError::ProtocolChanged,
        ),
        (200, "application/json", "{}", ChatError::ProtocolChanged),
        (
            200,
            "text/html",
            "<html>private-token</html>",
            ChatError::ProtocolChanged,
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_raw(body, content_type))
            .expect(1)
            .mount(&server)
            .await;
        let error = probe(&server).model_count().await.unwrap_err();
        assert_eq!(error, expected);
        assert!(!format!("{error:?} {error}").contains("private"));
    }
}

#[test]
fn challenges_are_distinct_from_denied_access_and_authentication() {
    let mut headers = http::HeaderMap::new();
    headers.insert("cf-mitigated", HeaderValue::from_static("challenge"));
    assert_eq!(
        classify_response(StatusCode::FORBIDDEN, &headers),
        Err(ChatError::CloudflareChallenge)
    );
    headers.clear();
    assert_eq!(
        classify_response(StatusCode::FORBIDDEN, &headers),
        Err(ChatError::AccessDenied)
    );
    assert_eq!(
        classify_response(StatusCode::UNAUTHORIZED, &headers),
        Err(ChatError::Authentication)
    );
}

#[tokio::test]
async fn redirects_do_not_forward_credentials() {
    let server = MockServer::start().await;
    let destination = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", destination.uri()))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        probe(&server).model_count().await,
        Err(ChatError::UnsupportedCapability)
    );
    assert!(destination.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn oversized_responses_are_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(vec![b' '; RESPONSE_LIMIT + 1], "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        probe(&server).model_count().await,
        Err(ChatError::ResponseTooLarge)
    );
}

#[tokio::test]
async fn logout_cancels_local_work_and_rejects_stale_results() {
    let server = MockServer::start().await;
    let probe = probe(&server);
    let (started, ready) = tokio::sync::oneshot::channel();
    let (result, ()) = tokio::join!(
        probe.guarded(async {
            started.send(()).unwrap();
            std::future::pending::<Result<(), ChatError>>().await
        }),
        async {
            ready.await.unwrap();
            probe.auth.logout().await.unwrap();
        }
    );
    assert_eq!(result, Err(ChatError::IdentityChanged));
    assert_eq!(probe.model_count().await, Err(ChatError::IdentityChanged));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn stopping_a_probe_cancels_work_without_claiming_server_cancellation() {
    let server = MockServer::start().await;
    let probe = probe(&server);
    let (started, ready) = tokio::sync::oneshot::channel();
    let (result, ()) = tokio::join!(
        probe.guarded(async {
            started.send(()).unwrap();
            std::future::pending::<Result<(), ChatError>>().await
        }),
        async {
            ready.await.unwrap();
            probe.stop();
        }
    );
    assert_eq!(result, Err(ChatError::LocalCancellation));
    assert_eq!(probe.model_count().await, Err(ChatError::LocalCancellation));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn preparation_uses_managed_auth_and_redacts_all_response_values() {
    let server = MockServer::start().await;
    mount_bootstrap(&server).await;
    for (endpoint, response) in [
        (
            "conversation/init",
            json!({"default_model_slug": "private-model", "unknown": "private-data"}),
        ),
        (
            "f/conversation/prepare",
            json!({"conduit_token": "private-conduit"}),
        ),
        (
            "sentinel/chat-requirements",
            json!({
                "token": "private-requirements",
                "proofofwork": {"required": false},
                "turnstile": {"required": true, "dx": "private-challenge"},
                "arkose": {"required": false},
                "unknown": {"private-id": "https://private-url/"}
            }),
        ),
    ] {
        Mock::given(method("POST"))
            .and(path(format!("/backend-api/{endpoint}")))
            .and(header("authorization", "Bearer Access Token"))
            .and(header("chatgpt-account-id", "account_id"))
            .and(header(
                "x-openai-target-path",
                format!("/backend-api/{endpoint}"),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(1)
            .mount(&server)
            .await;
    }
    let observations = probe(&server).inspect_preparation(false).await;
    assert_eq!(observations.len(), 4);
    assert_eq!(observations[0].error, None);
    assert_eq!(observations[1].error, None);
    assert_eq!(observations[2].error, None);
    assert_eq!(observations[3].error, Some(ChatError::UnsupportedChallenge));
    let serialized = serde_json::to_string(&observations).unwrap();
    for secret in ["private", "Access Token", "account_id", "https://"] {
        assert!(!serialized.contains(secret));
        assert!(!format!("{observations:?}").contains(secret));
    }
    let requests = server.received_requests().await.unwrap();
    assert!(!requests[0].headers.contains_key("authorization"));
    assert!(!requests[0].headers.contains_key("chatgpt-account-id"));
    assert!(!requests[0].headers.contains_key("x-openai-target-path"));
    let prepare: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(prepare["history_and_training_disabled"], false);
    assert_eq!(prepare["model"], "auto");
    assert!(prepare.get("messages").is_none());
    assert_eq!(requests[2].headers["x-conduit-token"], "no-token");
    let requirements: serde_json::Value = serde_json::from_slice(&requests[3].body).unwrap();
    assert_eq!(requirements, json!({}));
}

#[tokio::test]
async fn preparation_rejections_stop_without_replaying_posts() {
    for (status, error) in [
        (401, ChatError::Authentication),
        (403, ChatError::AccessDenied),
        (429, ChatError::RateLimited),
    ] {
        let server = MockServer::start().await;
        mount_bootstrap(&server).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_string("private body"))
            .expect(1)
            .mount(&server)
            .await;
        let observations = probe(&server).inspect_preparation(false).await;
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[1].http_status, Some(status));
        assert_eq!(observations[1].error, Some(error));
        assert!(observations[1].response.is_none());
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}

#[tokio::test]
async fn temporary_preparation_never_enables_saved_history() {
    let server = MockServer::start().await;
    mount_bootstrap(&server).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "default_model_slug": "auto", "conduit_token": "token", "token": "token"
        })))
        .expect(3)
        .mount(&server)
        .await;
    let observations = probe(&server).inspect_preparation(true).await;
    assert!(observations.iter().all(|step| step.error.is_none()));
    let requests = server.received_requests().await.unwrap();
    let prepare: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(prepare["history_and_training_disabled"], true);
}

#[tokio::test]
async fn preparation_does_not_forward_credentials_on_redirect() {
    let server = MockServer::start().await;
    let destination = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", destination.uri()))
        .mount(&server)
        .await;
    let observations = probe(&server).inspect_preparation(false).await;
    assert!(
        observations
            .iter()
            .all(|step| step.error == Some(ChatError::UnsupportedCapability))
    );
    assert!(destination.received_requests().await.unwrap().is_empty());
}

async fn mount_bootstrap(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("<html>private body</html>", "text/html"),
        )
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn cloudflare_challenge_stops_preparation_before_any_post() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("cf-mitigated", "challenge")
                .set_body_raw("<html>private challenge</html>", "text/html"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let observations = probe(&server).inspect_preparation(false).await;
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].http_status, Some(403));
    assert_eq!(observations[0].error, Some(ChatError::CloudflareChallenge));
    assert!(observations[0].cloudflare_challenge);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_challenge_stops_the_entire_session_without_further_requests() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).insert_header("cf-mitigated", "challenge"))
        .expect(1)
        .mount(&server)
        .await;
    let probe = probe(&server);
    assert_eq!(
        probe.model_count().await,
        Err(ChatError::CloudflareChallenge)
    );
    assert_eq!(
        probe.history_count(0, 20).await,
        Err(ChatError::CloudflareChallenge)
    );
    let observations = probe.inspect_preparation(false).await;
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].http_status, None);
    assert_eq!(observations[0].error, Some(ChatError::CloudflareChallenge));
}

#[tokio::test]
async fn three_failed_preparations_cool_down_for_ten_minutes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .expect(4)
        .mount(&server)
        .await;
    let probe = probe(&server);
    for _ in 0..3 {
        assert_eq!(
            probe.inspect_preparation(false).await[0].error,
            Some(ChatError::Transport)
        );
    }
    assert_eq!(
        probe.inspect_preparation(false).await[0].error,
        Some(ChatError::PreparationCooldown)
    );
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(599)).await;
    assert_eq!(
        probe.inspect_preparation(false).await[0].error,
        Some(ChatError::PreparationCooldown)
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    assert_eq!(
        probe.inspect_preparation(false).await[0].error,
        Some(ChatError::Transport)
    );
}

#[tokio::test]
async fn retry_after_blocks_other_operations_until_the_original_deadline() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .expect(2)
        .mount(&server)
        .await;
    let probe = probe(&server);
    assert_eq!(probe.model_count().await, Err(ChatError::RateLimited));
    assert_eq!(
        probe.history_count(0, 20).await,
        Err(ChatError::RateLimited)
    );
    assert_eq!(probe.inspect_preparation(false).await[0].http_status, None);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(120)).await;
    tokio::time::resume();
    assert_eq!(probe.model_count().await, Err(ChatError::RateLimited));
}

#[tokio::test]
async fn preparation_overlap_is_rejected_before_network_io() {
    let server = MockServer::start().await;
    let probe = probe(&server);
    let _guard = probe.preparation_permit.acquire().await.unwrap();
    assert_eq!(
        probe.inspect_preparation(false).await[0].error,
        Some(ChatError::PreparationInProgress)
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn stopping_and_idle_logout_release_the_session_pool() {
    let server = MockServer::start().await;
    let probe = probe(&server);
    let _ = probe.bootstrap_request().await.unwrap();
    assert!(probe.session.lock().unwrap().is_some());
    probe.stop();
    assert!(probe.session.lock().unwrap().is_none());

    let probe = super::tests::probe(&server);
    let _ = probe.bootstrap_request().await.unwrap();
    assert!(probe.session.lock().unwrap().is_some());
    probe.auth.logout().await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if probe.session.lock().unwrap().is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(probe.model_count().await, Err(ChatError::IdentityChanged));
}
