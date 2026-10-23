//! Reuses the initialized dispatcher fixture; Chat never invokes its verification provider.
use super::test_support::Harness;
use crate::outgoing_message::OutgoingMessage;
use crate::transport::ConnectionOrigin;
use pretty_assertions::assert_eq;
use serde_json::json;
use uuid::Uuid;

#[test]
fn chat_dispatch_runs_on_a_default_size_worker_stack() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_stack_size(2 * 1024 * 1024)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // Configuration construction uses the repository's normal test stack;
        // actual queued RPC dispatch runs on the explicitly smaller worker.
        let mut h = Harness::with_chat(ConnectionOrigin::InProcess)
            .await
            .unwrap();
        h.initialize("chat-small-stack-fixture", true).await;
        tokio::spawn(async move {
            h.send(1, "chat/status/read", json!({})).await;
            let OutgoingMessage::Response(response) = h.response().await else {
                panic!("local status")
            };
            let status = serde_json::to_value(response.result).unwrap();
            h.send(
                2,
                "chat/draft/write",
                json!({
                    "scopeId": status["scopeId"], "persistence": "saved",
                    "draftId": Uuid::new_v4(),
                    "content": {"text": "small stack fixture", "selections": []}
                }),
            )
            .await;
            assert!(matches!(h.response().await, OutgoingMessage::Response(_)));
            h.shutdown().await;
        })
        .await
        .unwrap();
    });
}

#[tokio::test]
async fn chat_requires_experimental_negotiation_even_on_trusted_connections() {
    let mut h = Harness::with_chat(ConnectionOrigin::InProcess)
        .await
        .unwrap();
    h.initialize("chat-fixture", false).await;
    h.send(1, "chat/status/read", json!({})).await;
    let OutgoingMessage::Error(error) = h.response().await else {
        panic!("negotiation required")
    };
    assert!(error.error.message.contains("experimental"));
    h.shutdown().await;
}

#[tokio::test]
async fn malformed_remote_chat_is_rejected_before_deserializing_private_params() {
    for origin in [ConnectionOrigin::WebSocket, ConnectionOrigin::RemoteControl] {
        let mut h = Harness::new(origin, || true).await.unwrap();
        h.initialize("codex-tui", true).await;
        h.send(1,"chat/attachment/upload",json!({"persistence":"PRIVATE-SECRET-ENUM","path":"/private/path","origin":"inProcess"})).await;
        let OutgoingMessage::Error(error) = h.response().await else {
            panic!("remote Chat rejected")
        };
        assert_eq!(error.error.data, Some(json!({"code":"remoteConnection"})));
        assert!(!error.error.message.contains("PRIVATE"));
        h.shutdown().await;
    }
}

#[tokio::test]
async fn malformed_local_chat_never_echoes_private_params_in_errors() {
    let mut h = Harness::with_chat(ConnectionOrigin::Stdio).await.unwrap();
    h.initialize("chat-fixture", true).await;
    h.send(
        1,
        "chat/draft/write",
        json!({"scopeId":"fixture","draftId":"fixture","persistence":"PRIVATE-SECRET-ENUM"}),
    )
    .await;
    let OutgoingMessage::Error(error) = h.response().await else {
        panic!("invalid params rejected")
    };
    assert_eq!(error.error.message, "Invalid Chat request");
    assert!(error.error.data.is_none());
    h.shutdown().await;
}

#[tokio::test]
async fn all_trusted_origins_can_use_revision_checked_saved_drafts() {
    for origin in [
        ConnectionOrigin::InProcess,
        ConnectionOrigin::Stdio,
        ConnectionOrigin::LocalSocket,
    ] {
        let mut h = Harness::with_chat(origin).await.unwrap();
        h.initialize("chat-fixture", true).await;
        h.send(1, "chat/status/read", json!({})).await;
        let OutgoingMessage::Response(response) = h.response().await else {
            panic!("local status")
        };
        let status = serde_json::to_value(response.result).unwrap();
        assert_eq!(status["availability"], "available");
        assert_eq!(status["capabilities"]["saved"], true);
        let scope = status["scopeId"].clone();
        h.send(2,"chat/draft/write",json!({"scopeId":scope,"persistence":"saved","draftId":Uuid::new_v4(),"content":{"text":"literal ! @ $","selections":[]}})).await;
        let OutgoingMessage::Response(response) = h.response().await else {
            panic!("local draft")
        };
        let response = serde_json::to_value(response.result).unwrap();
        assert_eq!(response["draft"]["content"]["text"], "literal ! @ $");
        h.send(
            3,
            "chat/draft/read",
            json!({"scopeId":scope,"persistence":"saved","draftId":response["draft"]["id"]}),
        )
        .await;
        let OutgoingMessage::Response(read) = h.response().await else {
            panic!("local read")
        };
        assert_eq!(
            serde_json::to_value(read.result).unwrap()["draft"],
            response["draft"]
        );
        h.shutdown().await;
    }
}

#[tokio::test]
async fn feature_disabled_status_reports_no_chat_capabilities() {
    let mut h = Harness::new(ConnectionOrigin::Stdio, || true)
        .await
        .unwrap();
    h.initialize("chat-fixture", true).await;
    h.send(1, "chat/status/read", json!({})).await;
    let OutgoingMessage::Response(response) = h.response().await else {
        panic!("status available while disabled")
    };
    let status = serde_json::to_value(response.result).unwrap();
    assert_eq!(status["availability"], "disabled");
    assert_eq!(status["capabilities"]["saved"], false);
    assert_eq!(status["capabilities"]["temporary"], false);
    assert!(status["scopeId"].is_null());
    h.shutdown().await;
}
