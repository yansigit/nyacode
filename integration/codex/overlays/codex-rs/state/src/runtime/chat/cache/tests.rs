use super::*;

fn snapshot(id: &str, time: i64) -> ChatCachedConversation {
    ChatCachedConversation {
        id: id.into(),
        title: "saved fixture".into(),
        updated_at_ms: Some(time),
        observed_at_ms: time,
        default_model: Some("fixture-model".into()),
        continuation_blocker: None,
        current_node: Some("assistant".into()),
        other_branch_heads: vec!["other-head".into()],
        nodes: vec![
            ChatCachedNode {
                id: "root".into(),
                parent_id: None,
                message: None,
            },
            ChatCachedNode {
                id: "user".into(),
                parent_id: Some("root".into()),
                message: Some(ChatCachedMessage {
                    id: "user-message".into(),
                    role: ChatCacheRole::User,
                    parts: vec![
                        ChatCachedPart::Text("literal ! @ $ fixture".into()),
                        ChatCachedPart::Image {
                            file_id: "file-image".into(),
                            width: Some(4),
                            height: Some(3),
                            size_bytes: Some(64),
                        },
                    ],
                    model: None,
                    finished: true,
                    end_turn: false,
                    citations: Vec::new(),
                    attachments: vec![ChatCachedAttachment {
                        file_id: "file-doc".into(),
                        name: "fixture.pdf".into(),
                        mime_type: Some("application/pdf".into()),
                        size_bytes: Some(128),
                        width: None,
                        height: None,
                    }],
                }),
            },
            ChatCachedNode {
                id: "assistant".into(),
                parent_id: Some("user".into()),
                message: Some(ChatCachedMessage {
                    id: "assistant-message".into(),
                    role: ChatCacheRole::Assistant,
                    parts: vec![ChatCachedPart::Text("completed fixture".into())],
                    model: Some("fixture-model".into()),
                    finished: true,
                    end_turn: true,
                    attachments: Vec::new(),
                    citations: vec![ChatCachedCitation {
                        marker: "\u{e200}cite\u{e202}turn0search0\u{e201}".into(),
                        urls: vec!["https://example.org/fixture".into()],
                    }],
                }),
            },
        ],
    }
}

fn equal(actual: &ChatCachedConversation, expected: &ChatCachedConversation) {
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[tokio::test]
async fn cache_migration_preserves_saved_drafts_and_pending_submissions() {
    let db = TestDb::through(62).await;
    let store = db.store();
    let draft = draft(&store, None).await;
    let request = request(&draft);
    let (pending, _) = store
        .begin_submission(request, Uuid::new_v4())
        .await
        .unwrap();
    STATE_MIGRATOR.run(db.pool.as_ref()).await.unwrap();
    assert_eq!(
        store.read_draft(draft.id).await.unwrap().unwrap().revision,
        draft.revision
    );
    assert!(
        store
            .read_submission(pending.submission_id)
            .await
            .unwrap()
            .unwrap()
            .active
    );
    assert!(
        store
            .cached_conversation("fixture")
            .await
            .unwrap()
            .is_none()
    );
    db.close().await;
}

#[tokio::test]
async fn cache_round_trip_reopen_and_account_workspace_isolation() {
    let db = TestDb::new().await;
    let store = db.store();
    let fixture = snapshot("fixture", 100);
    assert!(store.cache_conversation(&fixture).await.unwrap());
    let other = db.other_process().await;
    equal(
        &other.cached_conversation("fixture").await.unwrap().unwrap(),
        &fixture,
    );
    let attached: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_attachments")
        .fetch_one(db.pool.as_ref())
        .await
        .unwrap();
    assert_eq!(attached, 1);
    let message: String =
        sqlx::query_scalar("SELECT message FROM chat_messages WHERE node_id='user'")
            .fetch_one(db.pool.as_ref())
            .await
            .unwrap();
    assert!(!message.contains("fixture.pdf"));
    for scope in [
        scope("other", "account", None),
        scope("principal", "other", None),
        scope("principal", "account", Some("workspace")),
    ] {
        let isolated = SavedChatStore {
            pool: Arc::clone(&db.pool),
            scope,
        };
        assert!(
            isolated
                .cached_conversation("fixture")
                .await
                .unwrap()
                .is_none()
        );
    }
    other.pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn late_read_cannot_replace_a_newer_snapshot_and_replacement_removes_old_children() {
    let db = TestDb::new().await;
    let store = db.store();
    let first = snapshot("fixture", 100);
    store.cache_conversation(&first).await.unwrap();
    let mut latest = snapshot("fixture", 300);
    latest.nodes[1]
        .message
        .as_mut()
        .unwrap()
        .attachments
        .clear();
    latest.other_branch_heads.clear();
    store.cache_conversation(&latest).await.unwrap();
    let mut stale = snapshot("fixture", 200);
    stale.title = "stale response".into();
    let other = db.other_process().await;
    assert!(!other.cache_conversation(&stale).await.unwrap());
    equal(
        &store.cached_conversation("fixture").await.unwrap().unwrap(),
        &latest,
    );
    let attached: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_attachments")
        .fetch_one(db.pool.as_ref())
        .await
        .unwrap();
    assert_eq!(attached, 0);
    other.pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn failed_cache_replacement_rolls_back_complete_previous_branch() {
    let db = TestDb::new().await;
    let store = db.store();
    let first = snapshot("fixture", 100);
    store.cache_conversation(&first).await.unwrap();
    sqlx::query("CREATE TRIGGER interrupt_cache BEFORE INSERT ON chat_messages WHEN NEW.node_id='assistant' BEGIN SELECT RAISE(ABORT, 'fixture interruption'); END").execute(db.pool.as_ref()).await.unwrap();
    assert!(matches!(
        store.cache_conversation(&snapshot("fixture", 200)).await,
        Err(ChatStoreError::Unavailable)
    ));
    equal(
        &store.cached_conversation("fixture").await.unwrap().unwrap(),
        &first,
    );
    db.close().await;
}

#[tokio::test]
async fn cache_rejects_untyped_metadata_signed_urls_and_invalid_branches() {
    let db = TestDb::new().await;
    let store = db.store();
    for url in [
        "file:///private",
        "https://storage.invalid/?sig=secret",
        "https://u:p@example.org/",
    ] {
        let mut invalid = snapshot("fixture", 100);
        invalid.nodes[2].message.as_mut().unwrap().citations[0].urls = vec![url.into()];
        assert!(matches!(
            store.cache_conversation(&invalid).await,
            Err(ChatStoreError::InvalidInput)
        ));
    }
    let mut invalid = snapshot("fixture", 100);
    invalid.nodes[2].parent_id = Some("root".into());
    assert!(matches!(
        store.cache_conversation(&invalid).await,
        Err(ChatStoreError::InvalidInput)
    ));
    store
        .cache_conversation(&snapshot("fixture", 100))
        .await
        .unwrap();
    sqlx::query("UPDATE chat_attachments SET metadata=json_set(metadata,'$.download_url','https://storage.invalid/?sig=secret')").execute(db.pool.as_ref()).await.unwrap();
    assert!(matches!(
        store.cached_conversation("fixture").await,
        Err(ChatStoreError::InvalidRecord)
    ));
    db.close().await;
}

#[tokio::test]
async fn corrupt_oversized_cache_is_rejected_before_message_decoding() {
    let db = TestDb::new().await;
    let store = db.store();
    store
        .cache_conversation(&snapshot("fixture", 100))
        .await
        .unwrap();
    sqlx::query("UPDATE chat_messages SET message=json_object('unknown',printf('%.*c',8388609,'x')) WHERE node_id='user'").execute(db.pool.as_ref()).await.unwrap();
    assert!(matches!(
        store.cached_conversation("fixture").await,
        Err(ChatStoreError::InvalidRecord)
    ));
    db.close().await;
}

#[tokio::test]
async fn display_cache_eviction_keeps_drafts_and_pending_submissions() {
    let db = TestDb::new().await;
    let store = db.store();
    let draft = draft(&store, Some("fixture-0")).await;
    let (pending, _) = store
        .begin_submission(request(&draft), Uuid::new_v4())
        .await
        .unwrap();
    for index in 0..129 {
        store
            .cache_conversation(&snapshot(&format!("fixture-{index}"), index))
            .await
            .unwrap();
    }
    assert!(
        store
            .cached_conversation("fixture-0")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .cached_conversation("fixture-128")
            .await
            .unwrap()
            .is_some()
    );
    assert!(store.read_draft(draft.id).await.unwrap().is_some());
    assert!(
        store
            .read_submission(pending.submission_id)
            .await
            .unwrap()
            .unwrap()
            .active
    );
    let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_messages")
        .fetch_one(db.pool.as_ref())
        .await
        .unwrap();
    assert_eq!(messages, 128 * 3);
    db.close().await;
}

#[tokio::test]
async fn cache_byte_budget_evicts_oldest_before_the_count_limit() {
    let db = TestDb::new().await;
    let store = db.store();
    for index in 0..8 {
        store
            .cache_conversation(&snapshot(&format!("fixture-{index}"), index))
            .await
            .unwrap();
    }
    // Account for eight maximum-size snapshots without retaining 64 MiB of
    // duplicate fixture text in this capacity test.
    sqlx::query("UPDATE chat_conversations SET content_bytes=8388608")
        .execute(db.pool.as_ref())
        .await
        .unwrap();
    store
        .cache_conversation(&snapshot("newest", 10))
        .await
        .unwrap();
    assert!(
        store
            .cached_conversation("fixture-0")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .cached_conversation("fixture-1")
            .await
            .unwrap()
            .is_some()
    );
    assert!(store.cached_conversation("newest").await.unwrap().is_some());
    db.close().await;
}
