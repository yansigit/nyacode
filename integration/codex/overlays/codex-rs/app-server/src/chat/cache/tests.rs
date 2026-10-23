use super::*;
use crate::chat::tests::Harness;
use pretty_assertions::assert_eq;

fn snapshot() -> ConversationSnapshot {
    ConversationSnapshot {
        id: "cache-fixture".into(),
        title: "saved cache fixture".into(),
        updated_at_ms: Some(42),
        default_model: Some("fixture-model".into()),
        continuation_blocker: None,
        other_branch_heads: Vec::new(),
        branch: ConversationBranch {
            current_node: Some("assistant".into()),
            nodes: vec![
                ConversationNode {
                    id: "root".into(),
                    parent_id: None,
                    message: None,
                },
                ConversationNode {
                    id: "assistant".into(),
                    parent_id: Some("root".into()),
                    message: Some(Message {
                        id: "assistant-message".into(),
                        role: "assistant".into(),
                        model: Some("fixture-model".into()),
                        finished: true,
                        end_turn: true,
                        parts: vec![
                            MessagePart::Text("literal ! @ $ cached fixture".into()),
                            MessagePart::Image(ImageReference {
                                file_id: "file-image".into(),
                                width: Some(8),
                                height: Some(6),
                                size_bytes: Some(512),
                            }),
                        ],
                        citations: vec![Citation {
                            marker: "\u{e200}cite\u{e202}turn0search0\u{e201}".into(),
                            urls: vec!["https://example.org/fixture".into()],
                        }],
                        attachments: vec![AttachmentReference {
                            file_id: "file-doc".into(),
                            name: "fixture.pdf".into(),
                            mime_type: Some("application/pdf".into()),
                            size_bytes: Some(128),
                            width: None,
                            height: None,
                        }],
                    }),
                },
            ],
        },
    }
}

#[tokio::test]
async fn cached_saved_snapshot_keeps_metadata_but_requires_authoritative_refresh() {
    let h = Harness::new().await;
    let session = h.session();
    let mut fixture = snapshot();
    session
        .cache_authoritative_conversation(&fixture, 100)
        .await;
    let (cached, observed) = session.cached_conversation_read(&fixture.id).await.unwrap();
    assert_eq!(observed, 100);
    assert!(matches!(
        cached.check_continuation("assistant"),
        Err(ChatError::UnsupportedCapability)
    ));
    fixture.continuation_blocker = Some(ContinuationBlocker::ReadOnly);
    assert_eq!(
        serde_json::to_value(&cached).unwrap(),
        serde_json::to_value(&fixture).unwrap()
    );
    h.service.set_enabled(false);
    assert!(matches!(
        session.cached_conversation_read(&fixture.id).await,
        Err(ServiceError::ScopeChanged)
    ));
    h.service.set_enabled(true);
    assert!(
        h.session()
            .cached_conversation_read(&fixture.id)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn temporary_snapshots_cannot_create_saved_cache_rows() {
    let h = Harness::new().await;
    let session = h.session();
    let mut fixture = snapshot();
    fixture.continuation_blocker = Some(ContinuationBlocker::Temporary);
    session
        .cache_authoritative_conversation(&fixture, 100)
        .await;
    assert!(matches!(
        session.cached_conversation_read(&fixture.id).await,
        Err(ServiceError::Store(ChatStoreError::NotFound))
    ));
}

#[tokio::test]
async fn display_cache_failure_does_not_create_a_native_session_or_panic() {
    let h = Harness::new().await;
    let session = h.session();
    h.db.close().await;
    session
        .cache_authoritative_conversation(&snapshot(), 100)
        .await;
    assert!(session.native.get().is_none());
    assert!(session.read_only_native.get().is_none());
    assert!(matches!(
        session.cached_conversation_read("cache-fixture").await,
        Err(ServiceError::Store(ChatStoreError::Unavailable))
    ));
}
