use super::*;
use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingMessage;
use app_test_support::ChatGptAuthFixture;
use app_test_support::write_chatgpt_auth;
use codex_analytics::AnalyticsEventsClient;
use codex_config::types::AuthCredentialsStoreMode;
use codex_core::config::ConfigBuilder;
use codex_login::AuthManager;
use codex_state::StateRuntime;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::mpsc;

struct Harness {
    _home: tempfile::TempDir,
    rpc: Arc<ChatRpc>,
    db: Arc<StateRuntime>,
    messages: mpsc::Receiver<OutgoingEnvelope>,
}

impl Harness {
    async fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        write_chatgpt_auth(
            home.path(),
            ChatGptAuthFixture::new("fixture-token")
                .account_id("rpc-account")
                .chatgpt_account_id("rpc-account")
                .chatgpt_user_id("rpc-user")
                .plan_type("plus"),
            AuthCredentialsStoreMode::File,
        )
        .unwrap();
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_owned())
            .build()
            .await
            .unwrap();
        let auth = AuthManager::shared_from_config(&config, false)
            .await
            .unwrap();
        let db = StateRuntime::init(config.sqlite_config().clone(), "test".into())
            .await
            .unwrap();
        let service = Service::new(auth, Some(Arc::clone(&db)), true);
        let (sender, messages) = mpsc::channel(16);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            sender,
            AnalyticsEventsClient::disabled(),
        ));
        Self {
            _home: home,
            db,
            rpc: ChatRpc::new(service, outgoing),
            messages,
        }
    }
    fn scope(&self) -> String {
        self.rpc
            .service
            .open_scope(ConnectionOrigin::InProcess, None)
            .unwrap()
            .id()
            .to_string()
    }
    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ServiceError> {
        self.rpc
            .dispatch(request(method, params), ConnectionOrigin::InProcess)
            .await
            .map(|(result, _)| serde_json::to_value(result).unwrap())
    }
    async fn draft(&self, selections: serde_json::Value) -> serde_json::Value {
        self.call("chat/draft/write",json!({"scopeId":self.scope(), "persistence":"saved", "draftId":Uuid::new_v4(), "content":{"text":"literal ! @ $ PRIVATE-RPC-CONTENT", "selections":selections}})).await.unwrap()["draft"].clone()
    }
    async fn close(self) {
        self.rpc.shutdown();
        self.rpc.service.shutdown();
        self.db.close().await;
    }
}

fn request(method: &str, params: serde_json::Value) -> rpc::ClientRequest {
    rpc::ClientRequest::try_from(rpc::JSONRPCRequest {
        id: rpc::RequestId::Integer(1),
        method: method.into(),
        params: Some(params),
        trace: None,
    })
    .unwrap()
}

fn request_id() -> ConnectionRequestId {
    ConnectionRequestId {
        connection_id: ConnectionId(1),
        request_id: rpc::RequestId::Integer(1),
    }
}

fn context() -> RequestContext {
    RequestContext::new(
        ConnectionRequestId {
            connection_id: ConnectionId(1),
            request_id: rpc::RequestId::Integer(1),
        },
        "chat/status/read",
        tracing::Span::none(),
        None,
    )
}

#[tokio::test]
async fn explicit_cached_reads_are_labeled_local_and_cannot_reconcile_or_continue() {
    let h = Harness::new().await;
    let scope = h.scope();
    let store = h.db.saved_chat(
        codex_state::chat::ChatScope::new("rpc-user".into(), "rpc-account".into(), None).unwrap(),
    );
    store
        .cache_conversation(&codex_state::chat::ChatCachedConversation {
            id: "cached-fixture".into(),
            title: "local display fixture".into(),
            updated_at_ms: Some(40),
            observed_at_ms: 50,
            default_model: None,
            continuation_blocker: Some(codex_state::chat::ChatCacheBlocker::EmptyConversation),
            current_node: None,
            other_branch_heads: Vec::new(),
            nodes: Vec::new(),
        })
        .await
        .unwrap();
    let response = h.call("chat/conversation/read", json!({"scopeId":scope,"persistence":"saved","conversationId":"cached-fixture","cacheOnly":true})).await.unwrap();
    assert_eq!(response["source"], "localCache");
    assert_eq!(response["observedAtMs"], 50);
    assert_eq!(response["continuationBlocker"], "readOnly");
    assert_eq!(response["title"], "local display fixture");
    assert!(matches!(h.call("chat/conversation/read", json!({"scopeId":scope,"persistence":"saved","conversationId":"cached-fixture","cacheOnly":true,"submissionId":Uuid::new_v4()})).await, Err(ServiceError::Native(ChatError::InvalidInput))));
    assert!(matches!(h.call("chat/conversation/read", json!({"scopeId":scope,"persistence":"temporary","conversationId":"cached-fixture","cacheOnly":true})).await, Err(ServiceError::Native(ChatError::UnsupportedCapability))));
    h.close().await;
}

#[tokio::test]
async fn saved_drafts_round_trip_literals_and_detect_concurrent_edits() {
    let h = Harness::new().await;
    let draft = h.draft(json!([])).await;
    assert_eq!(
        draft["content"]["text"],
        "literal ! @ $ PRIVATE-RPC-CONTENT"
    );
    let params = json!({"scopeId":h.scope(),"persistence":"saved","draftId":draft["id"],"expectedRevision":draft["revision"],"content":{"text":"second edit","selections":[]}});
    let changed = h.call("chat/draft/write", params.clone()).await.unwrap();
    assert_ne!(draft["revision"], changed["draft"]["revision"]);
    assert!(matches!(
        h.call("chat/draft/write", params).await,
        Err(ServiceError::Store(ChatStoreError::RevisionConflict))
    ));
    let read = h
        .call(
            "chat/draft/read",
            json!({"scopeId":h.scope(),"persistence":"saved","draftId":draft["id"]}),
        )
        .await
        .unwrap();
    assert_eq!(read["draft"], changed["draft"]);
    assert!(read["pendingSubmission"].is_null());
    h.close().await;
}

#[tokio::test]
async fn temporary_rpc_drafts_work_with_closed_storage_and_never_open_saved_state() {
    let h = Harness::new().await;
    let status = h.call("chat/status/read", json!({})).await.unwrap();
    assert_eq!(status["capabilities"]["temporary"], true);
    let scope = h
        .rpc
        .service
        .open_scope(ConnectionOrigin::InProcess, None)
        .unwrap();
    assert!(h.rpc.service.saved_in_scope(&scope).unwrap().is_none());
    h.db.close().await;
    let response = h.call("chat/draft/write",json!({"scopeId":h.scope(),"persistence":"temporary","draftId":Uuid::new_v4(),"content":{"text":"TEMP-PRIVATE ! @ $","selections":[]}})).await.unwrap();
    let draft = &response["draft"];
    assert_eq!(draft["persistence"], "temporary");
    let read = h
        .call(
            "chat/draft/read",
            json!({"scopeId":h.scope(),"persistence":"temporary","draftId":draft["id"]}),
        )
        .await
        .unwrap();
    assert_eq!(&read["draft"], draft);
    assert!(h.rpc.service.saved_in_scope(&scope).unwrap().is_none());
    assert!(lock(&h.rpc.uploads).is_empty());
    h.close().await;
}

#[tokio::test]
async fn disabled_status_has_no_content_and_stale_scopes_cannot_read_saved_drafts() {
    let h = Harness::new().await;
    let scope = h.scope();
    let draft = h.draft(json!([])).await;
    h.rpc.service.set_enabled(false);
    let status = h.call("chat/status/read", json!({})).await.unwrap();
    assert_eq!(status["availability"], "disabled");
    assert!(status["scopeId"].is_null());
    assert_eq!(status["turns"], json!([]));
    h.rpc.service.set_enabled(true);
    assert!(matches!(
        h.call(
            "chat/draft/read",
            json!({"scopeId":scope,"persistence":"saved","draftId":draft["id"]})
        )
        .await,
        Err(ServiceError::ScopeChanged)
    ));
    h.close().await;
}

#[tokio::test]
async fn remote_origins_are_rejected_before_scope_or_file_validation() {
    let h = Harness::new().await;
    h.rpc.service.set_enabled(false);
    for origin in [ConnectionOrigin::WebSocket, ConnectionOrigin::RemoteControl] {
        let connection = Arc::new(ConnectionSessionState::new(origin));
        let result = h.rpc.handle(request("chat/attachment/upload",json!({"scopeId":"invalid","draftId":"invalid","expectedRevision":"invalid","persistence":"saved","selections":[{"type":"localFile","selectionId":"invalid","path":"/missing/private"}]})),request_id(),connection,context()).await.unwrap_err();
        assert_eq!(result.data, Some(json!({"code":"remoteConnection"})));
    }
    assert!(lock(&h.rpc.uploads).is_empty());
    assert!(lock(&h.rpc.subscribers).is_empty());
    h.close().await;
}

#[tokio::test]
async fn submit_rejects_text_or_revision_changes_before_starting_native_work() {
    let h = Harness::new().await;
    let draft = h.draft(json!([])).await;
    let params = json!({"scopeId":h.scope(),"persistence":"saved","draftId":draft["id"],"expectedRevision":draft["revision"],"submissionId":Uuid::new_v4(),"clientMessageId":Uuid::new_v4(),"expectedParentId":Uuid::new_v4(),"text":"changed after flush","uploadHandles":[]});
    assert!(matches!(
        h.call("chat/turn/start", params).await,
        Err(ServiceError::Store(ChatStoreError::RevisionConflict))
    ));
    assert!(
        h.rpc
            .service
            .open_saved(ConnectionOrigin::InProcess, None)
            .unwrap()
            .operations()
            .unwrap()
            .is_empty()
    );
    h.close().await;
}

#[tokio::test]
async fn unreadable_upload_finishes_locally_and_retains_the_selection() {
    let h = Harness::new().await;
    let id = Uuid::new_v4();
    let path = h._home.path().join("missing.txt");
    let draft = h
        .draft(json!([{"id":id,"source":path,"name":"missing.txt"}]))
        .await;
    let response = h.call("chat/attachment/upload",json!({"scopeId":h.scope(),"persistence":"saved","draftId":draft["id"],"expectedRevision":draft["revision"],"selections":[{"type":"localFile","selectionId":id,"path":path}]})).await.unwrap();
    let op = lock(&h.rpc.uploads)
        .get(&Uuid::parse_str(response["operationId"].as_str().unwrap()).unwrap())
        .unwrap()
        .clone();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !op.snapshot().finished {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = op.snapshot();
    assert_eq!(
        snapshot.error,
        Some(rpc::ChatErrorCode::AttachmentUnreadable)
    );
    assert_eq!(snapshot.files[0].stage, rpc::ChatAttachmentStage::Failed);
    assert!(snapshot.files[0].handle.is_none());
    let read = h
        .call(
            "chat/draft/read",
            json!({"scopeId":h.scope(),"persistence":"saved","draftId":draft["id"]}),
        )
        .await
        .unwrap();
    assert_eq!(
        read["draft"]["content"]["selections"],
        draft["content"]["selections"]
    );
    h.close().await;
}

#[tokio::test]
async fn upload_paths_must_match_the_selected_revision_before_file_access() {
    let h = Harness::new().await;
    let id = Uuid::new_v4();
    let draft = h
        .draft(json!([{"id":id,"source":"/explicit/a.txt","name":"a.txt"}]))
        .await;
    assert!(matches!(h.call("chat/attachment/upload",json!({"scopeId":h.scope(),"persistence":"saved","draftId":draft["id"],"expectedRevision":draft["revision"],"selections":[{"type":"localFile","selectionId":id,"path":"/substituted/a.txt"}]})).await,Err(ServiceError::Native(ChatError::InvalidInput))));
    assert!(lock(&h.rpc.uploads).is_empty());
    h.close().await;
}

#[tokio::test]
async fn queued_responses_recheck_scope_after_backpressure() {
    let mut h = Harness::new().await;
    for index in 0..16 {
        h.rpc
            .outgoing
            .send_response(
                ConnectionRequestId {
                    connection_id: ConnectionId(1),
                    request_id: rpc::RequestId::Integer(index),
                },
                rpc::ChatDraftDeleteResponse {
                    scope_id: "fixture".into(),
                },
            )
            .await;
    }
    let connection = Arc::new(ConnectionSessionState::new(ConnectionOrigin::LocalSocket));
    let rpc = Arc::clone(&h.rpc);
    let work = rpc.handle(
        request("chat/status/read", json!({})),
        request_id(),
        connection,
        context(),
    );
    let mut work = std::pin::pin!(work);
    assert!(futures::poll!(&mut work).is_pending());
    h.rpc.service.set_enabled(false);
    h.messages.recv().await.unwrap();
    work.await.unwrap();
    for _ in 1..16 {
        h.messages.recv().await.unwrap();
    }
    let OutgoingEnvelope::ToConnection {
        message: OutgoingMessage::Error(error),
        ..
    } = h.messages.recv().await.unwrap()
    else {
        panic!("revoked response must be an error")
    };
    assert_eq!(error.error.data, Some(json!({"code":"scopeChanged"})));
    h.close().await;
}

#[tokio::test]
async fn notifications_are_targeted_and_revocation_discards_queued_content() {
    let mut h = Harness::new().await;
    let session = h
        .rpc
        .service
        .open_saved(ConnectionOrigin::InProcess, None)
        .unwrap();
    let connection = ConnectionSessionState::new(ConnectionOrigin::LocalSocket);
    lock(&h.rpc.subscribers).insert(
        ConnectionId(7),
        Subscriber {
            scope: Some(session.scope_id()),
            gate: Arc::clone(&connection.rpc_gate),
            origin: connection.origin,
        },
    );
    let (status, _) = h.rpc.status(ConnectionOrigin::InProcess).unwrap();
    h.rpc
        .notify_scope(
            &session.scope(),
            rpc::ServerNotification::ChatAvailability(rpc::ChatAvailabilityNotification {
                status: status.clone(),
            }),
        )
        .await;
    assert!(matches!(
        h.messages.recv().await,
        Some(OutgoingEnvelope::ToConnection {
            connection_id: ConnectionId(7),
            ..
        })
    ));
    for index in 0..16 {
        h.rpc
            .outgoing
            .send_response(
                ConnectionRequestId {
                    connection_id: ConnectionId(1),
                    request_id: rpc::RequestId::Integer(index),
                },
                rpc::ChatDraftDeleteResponse {
                    scope_id: "fixture".into(),
                },
            )
            .await;
    }
    let rpc = Arc::clone(&h.rpc);
    let scope = session.scope();
    let work = rpc.notify_scope(
        &scope,
        rpc::ServerNotification::ChatAvailability(rpc::ChatAvailabilityNotification { status }),
    );
    let mut work = std::pin::pin!(work);
    assert!(futures::poll!(&mut work).is_pending());
    // Closing this subscriber must suppress the old content after queue reserve.
    connection.rpc_gate.close().await;
    h.messages.recv().await.unwrap();
    work.await;
    for _ in 1..16 {
        h.messages.recv().await.unwrap();
    }
    assert!(h.messages.try_recv().is_err());
    h.close().await;
}

#[tokio::test]
async fn clipboard_capacity_is_reserved_before_database_access() {
    let h = Harness::new().await;
    let session = h
        .rpc
        .service
        .open_saved(ConnectionOrigin::InProcess, None)
        .unwrap();
    let _held = Arc::clone(&h.rpc.encoded_memory)
        .acquire_many_owned(128 * 1024 * 1024)
        .await
        .unwrap();
    let result = h
        .rpc
        .start_upload(
            Session::Saved(session),
            rpc::ChatAttachmentUploadParams {
                scope_id: h.scope(),
                persistence: rpc::ChatPersistence::Saved,
                draft_id: Uuid::new_v4().to_string(),
                expected_revision: Uuid::new_v4().to_string(),
                selections: vec![rpc::ChatAttachmentSource::ClipboardImage {
                    selection_id: Uuid::new_v4().to_string(),
                    name: "clipboard.png".into(),
                    image_base64: "AAAA".into(),
                }],
            },
        )
        .await;
    assert!(matches!(result, Err(ServiceError::Capacity)));
    assert!(lock(&h.rpc.uploads).is_empty());
    h.close().await;
}

#[tokio::test]
async fn temporary_turns_and_cancel_use_memory_snapshots_without_saved_ownership() {
    let h = Harness::new().await;
    let response = h.call("chat/draft/write",json!({"scopeId":h.scope(),"persistence":"temporary","draftId":Uuid::new_v4(),"content":{"text":"temporary turn fixture","selections":[]}})).await.unwrap();
    let draft = &response["draft"];
    let started = h.call("chat/turn/start",json!({"scopeId":h.scope(),"persistence":"temporary","draftId":draft["id"],"expectedRevision":draft["revision"],"text":"temporary turn fixture","submissionId":Uuid::new_v4(),"clientMessageId":Uuid::new_v4(),"expectedParentId":Uuid::new_v4(),"uploadHandles":[]})).await.unwrap();
    assert_eq!(started["turn"]["persistence"], "temporary");
    assert_eq!(started["turn"]["acceptance"], "unknown");
    let cancelled = h
        .call(
            "chat/turn/cancel",
            json!({"scopeId":h.scope(),"operationId":started["turn"]["operationId"]}),
        )
        .await
        .unwrap();
    assert_eq!(cancelled["localStopRequested"], true);
    assert_eq!(cancelled["serverCancellationConfirmed"], false);
    let temporary = h
        .rpc
        .service
        .open_temporary(
            ConnectionOrigin::InProcess,
            Some(Uuid::parse_str(&h.scope()).unwrap()),
        )
        .unwrap();
    let operation = temporary
        .operation(Uuid::parse_str(started["turn"]["operationId"].as_str().unwrap()).unwrap())
        .unwrap()
        .unwrap();
    let mut updates = operation.subscribe();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if updates.borrow_and_update().phase == crate::chat::OperationPhase::Finished {
                break;
            }
            updates.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let status = h.call("chat/status/read", json!({})).await.unwrap();
    assert_eq!(status["turns"][0]["persistence"], "temporary");
    assert_eq!(status["turns"][0]["error"], "localCancellation");
    assert!(
        h.rpc
            .service
            .saved_in_scope(&temporary.scope())
            .unwrap()
            .is_none()
    );
    let read = h
        .call(
            "chat/draft/read",
            json!({"scopeId":h.scope(),"persistence":"temporary","draftId":draft["id"]}),
        )
        .await
        .unwrap();
    assert_eq!(&read["draft"], draft);
    assert!(read["pendingSubmission"].is_null());
    h.close().await;
}

#[tokio::test]
async fn temporary_attachment_failure_and_cancel_keep_mode_and_never_open_saved_storage() {
    let h = Harness::new().await;
    let path = h._home.path().join("missing-temporary.txt");
    let selection = Uuid::new_v4();
    let response = h.call("chat/draft/write",json!({"scopeId":h.scope(),"persistence":"temporary","draftId":Uuid::new_v4(),"content":{"text":"temporary upload fixture","selections":[{"id":selection,"source":path,"name":"missing-temporary.txt"}]}})).await.unwrap();
    let draft = &response["draft"];
    let start = h.call("chat/attachment/upload",json!({"scopeId":h.scope(),"persistence":"temporary","draftId":draft["id"],"expectedRevision":draft["revision"],"selections":[{"type":"localFile","selectionId":selection,"path":path}]})).await.unwrap();
    let operation_id = Uuid::parse_str(start["operationId"].as_str().unwrap()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if lock(&h.rpc.uploads)
                .get(&operation_id)
                .unwrap()
                .snapshot()
                .finished
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let snapshot = lock(&h.rpc.uploads).get(&operation_id).unwrap().snapshot();
    assert_eq!(snapshot.persistence, rpc::ChatPersistence::Temporary);
    assert_eq!(
        snapshot.error,
        Some(rpc::ChatErrorCode::AttachmentUnreadable)
    );
    let cancelled = h
        .call(
            "chat/attachment/cancel",
            json!({"scopeId":h.scope(),"operationId":operation_id}),
        )
        .await
        .unwrap();
    assert_eq!(cancelled["upload"]["persistence"], "temporary");
    assert_eq!(cancelled["serverCancellationConfirmed"], false);
    let scope = h
        .rpc
        .service
        .open_scope(ConnectionOrigin::InProcess, None)
        .unwrap();
    assert!(h.rpc.service.saved_in_scope(&scope).unwrap().is_none());
    h.close().await;
}

#[tokio::test]
async fn cancelling_unknown_operation_ids_does_not_create_either_content_owner() {
    let h = Harness::new().await;
    let scope = h
        .rpc
        .service
        .open_scope(ConnectionOrigin::InProcess, None)
        .unwrap();
    for method in ["chat/turn/cancel", "chat/attachment/cancel"] {
        let response = h
            .call(
                method,
                json!({"scopeId":h.scope(),"operationId":Uuid::new_v4()}),
            )
            .await
            .unwrap();
        assert_eq!(response["localStopRequested"], false);
    }
    assert!(h.rpc.service.saved_in_scope(&scope).unwrap().is_none());
    assert!(h.rpc.service.temporary_in_scope(&scope).unwrap().is_none());
    h.close().await;
}
