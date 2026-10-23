use super::*;
use crate::chat::Service;
use crate::chat::tests::Harness;
use crate::chat::tests::content;
use crate::transport::ConnectionOrigin;
use pretty_assertions::assert_eq;

fn temporary(h: &Harness) -> Arc<TemporarySession> {
    h.service
        .open_temporary(ConnectionOrigin::InProcess, None)
        .unwrap()
}

#[tokio::test]
async fn temporary_drafts_and_native_sessions_work_without_recovery_storage() {
    let h = Harness::new().await;
    h.db.close().await;
    let session = temporary(&h);
    let mut body = content();
    let marker = "TEMPORARY-NO-DISK-WILLOW-526";
    body.text = marker.into();
    let draft = session.draft_write(Uuid::new_v4(), None, body).unwrap();
    assert_eq!(
        session.draft_read(draft.id).unwrap().unwrap().content.text,
        marker
    );
    let native = session.native(draft.id).unwrap();
    assert!(native.check_current().is_ok());
    assert!(lock(&h.service.state).saved.is_none());
    // This fixture's only pre-existing files are managed dummy auth and SQLite
    // state. Temporary contents must not appear in either, including WAL files.
    let mut directories = vec![h.home.path().to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                directories.push(entry.path());
            } else if kind.is_file() {
                let bytes = std::fs::read(entry.path()).unwrap();
                assert!(
                    !bytes
                        .windows(marker.len())
                        .any(|part| part == marker.as_bytes())
                );
            }
        }
    }
    h.service.shutdown();
    assert!(native.check_current().is_err());
    let replacement = Service::new(Arc::clone(&h.auth), None, true);
    assert!(
        replacement
            .open_temporary(ConnectionOrigin::LocalSocket, None)
            .unwrap()
            .draft_list(None, 20)
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn saved_and_temporary_drafts_share_identity_but_never_content_or_storage() {
    let h = Harness::new().await;
    let saved = h.session();
    let transient = temporary(&h);
    assert_eq!(saved.scope_id(), transient.scope_id());
    assert!(Arc::ptr_eq(&saved.scope(), &transient.scope()));
    let id = Uuid::new_v4();
    let persisted = saved.draft_write(id, None, content()).await.unwrap();
    let mut body = content();
    body.text = "temporary ! @ $".into();
    let memory = transient.draft_write(id, None, body).unwrap();
    assert_eq!(
        transient.draft_read(id).unwrap().unwrap().content.text,
        "temporary ! @ $"
    );
    assert_eq!(
        saved.draft_read(id).await.unwrap().unwrap().content.text,
        persisted.content.text
    );
    transient.draft_delete(id, memory.revision).unwrap();
    assert_eq!(
        saved.draft_read(id).await.unwrap().unwrap().revision,
        persisted.revision
    );
    assert!(transient.draft_read(id).unwrap().is_none());
}

#[tokio::test]
async fn revisions_pagination_and_capacity_preserve_existing_temporary_drafts() {
    let h = Harness::new().await;
    let session = temporary(&h);
    let first = session
        .draft_write(Uuid::new_v4(), None, content())
        .unwrap();
    assert!(matches!(
        session.draft_write(first.id, None, content()),
        Err(ServiceError::Store(ChatStoreError::RevisionConflict))
    ));
    assert!(matches!(
        session.draft_delete(first.id, Uuid::new_v4()),
        Err(ServiceError::Store(ChatStoreError::RevisionConflict))
    ));
    for _ in 1..MAX_DRAFTS {
        session
            .draft_write(Uuid::new_v4(), None, content())
            .unwrap();
    }
    assert!(matches!(
        session.draft_write(Uuid::new_v4(), None, content()),
        Err(ServiceError::Capacity)
    ));
    let page = session.draft_list(None, 10).unwrap();
    assert_eq!(page.items.len(), 10);
    let second = session.draft_list(page.next, 100).unwrap();
    assert_eq!(second.items.len(), MAX_DRAFTS - 10);
    assert!(
        page.items
            .iter()
            .all(|a| second.items.iter().all(|b| a.id != b.id))
    );
    assert_eq!(
        session.draft_read(first.id).unwrap().unwrap().revision,
        first.revision
    );
    let mut large = content();
    large.text = "x".repeat(1024 * 1024);
    let ids: Vec<_> = session
        .draft_list(None, 100)
        .unwrap()
        .items
        .into_iter()
        .map(|d| (d.id, d.revision))
        .collect();
    let mut rejected = false;
    for (id, revision) in ids {
        match session.draft_write(id, Some(revision), large.clone()) {
            Ok(_) => {}
            Err(ServiceError::Capacity) => {
                assert_eq!(session.draft_read(id).unwrap().unwrap().revision, revision);
                rejected = true;
                break;
            }
            Err(error) => panic!("unexpected error: {error}"),
        }
    }
    assert!(rejected);
}

#[tokio::test]
async fn temporary_native_capacity_is_released_when_a_draft_is_removed() {
    let h = Harness::new().await;
    let session = temporary(&h);
    let mut first = None;
    for _ in 0..MAX_NATIVE_SESSIONS {
        let draft = session
            .draft_write(Uuid::new_v4(), None, content())
            .unwrap();
        let native = session.native(draft.id).unwrap();
        first.get_or_insert((draft, native));
    }
    let pending = session
        .draft_write(Uuid::new_v4(), None, content())
        .unwrap();
    assert!(matches!(
        session.native(pending.id),
        Err(ServiceError::Capacity)
    ));
    assert!(session.draft_read(pending.id).unwrap().is_some());
    let (draft, native) = first.unwrap();
    session.draft_delete(draft.id, draft.revision).unwrap();
    assert!(native.check_current().is_err());
    assert!(session.native(pending.id).unwrap().check_current().is_ok());
}

#[tokio::test]
async fn temporary_cannot_import_a_conversation_and_remote_origin_is_rejected_first() {
    let h = Harness::new().await;
    for origin in [ConnectionOrigin::WebSocket, ConnectionOrigin::RemoteControl] {
        assert!(matches!(
            h.service.open_temporary(origin, None),
            Err(ServiceError::RemoteConnection)
        ));
    }
    assert!(lock(&h.service.state).scope.is_none());
    let session = temporary(&h);
    let mut foreign = content();
    foreign.conversation_id = Some("foreign-saved-conversation".into());
    foreign.expected_parent_id = Some("foreign-parent".into());
    assert!(matches!(
        session.draft_write(Uuid::new_v4(), None, foreign),
        Err(ServiceError::Native(ChatError::BranchConflict))
    ));
    assert!(session.draft_list(None, 20).unwrap().items.is_empty());
}

#[tokio::test]
async fn disable_and_auth_refresh_revoke_temporary_state_even_without_a_saved_session() {
    let h = Harness::new().await;
    let first = temporary(&h);
    let draft = first.draft_write(Uuid::new_v4(), None, content()).unwrap();
    let native = first.native(draft.id).unwrap();
    h.service.set_enabled(false);
    h.service.set_enabled(true);
    assert_eq!(first.check(), Err(ServiceError::ScopeChanged));
    assert!(lock(&first.memory).drafts.is_empty());
    assert!(native.check_current().is_err());
    let second = temporary(&h);
    assert_ne!(first.scope_id(), second.scope_id());
    assert!(second.draft_read(draft.id).unwrap().is_none());
    let draft = second.draft_write(Uuid::new_v4(), None, content()).unwrap();
    crate::chat::tests::write_auth(
        h.home.path(),
        "fixture-account",
        "fixture-user",
        "new-temporary-token",
    );
    h.auth.reload().await;
    let replacement = temporary(&h);
    assert_eq!(second.check(), Err(ServiceError::ScopeChanged));
    assert!(lock(&second.memory).drafts.is_empty());
    assert!(replacement.draft_read(draft.id).unwrap().is_none());
    assert!(lock(&h.service.state).saved.is_none());
}

#[tokio::test]
async fn temporary_staging_keeps_clipboard_bytes_and_file_snapshots_out_of_saved_storage() {
    use crate::chat::UploadSelection;
    use codex_state::chat::ChatFileSelection;
    let h = Harness::new().await;
    h.db.close().await;
    let session = temporary(&h);
    let path = h.home.path().join("selected.txt");
    std::fs::write(&path, b"temporary attachment fixture").unwrap();
    let file_id = Uuid::new_v4();
    let clipboard_id = Uuid::new_v4();
    let mut body = content();
    body.selections = vec![
        ChatFileSelection {
            id: file_id,
            source: Some(path.clone()),
            name: "selected.txt".into(),
            snapshot: None,
        },
        ChatFileSelection {
            id: clipboard_id,
            source: None,
            name: "clipboard.png".into(),
            snapshot: None,
        },
    ];
    let draft = session.draft_write(Uuid::new_v4(), None, body).unwrap();
    let (staged_draft, files) = session
        .stage_selected(
            draft.id,
            draft.revision,
            vec![
                UploadSelection::File {
                    selection_id: file_id,
                },
                UploadSelection::Clipboard {
                    selection_id: clipboard_id,
                    bytes: include_bytes!(
                        "../../../../chatgpt-chat/fixtures/attachments/fixture.png"
                    )
                    .to_vec(),
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(files.len(), 2);
    assert_ne!(staged_draft.revision, draft.revision);
    assert!(
        staged_draft
            .content
            .selections
            .iter()
            .all(|s| s.snapshot.is_some())
    );
    assert!(staged_draft.content.selections[1].source.is_none());
    assert!(lock(&h.service.state).saved.is_none());
    assert!(lock(&session.memory).native.is_empty());
    assert!(lock(&session.uploads).is_empty());
    drop(files);
    // The snapshot remains stable; a changed file is rejected before creating
    // a native session, remote allocation, or an upload handle.
    let mut file_only = staged_draft.content;
    file_only.selections.pop();
    let updated = session
        .draft_write(draft.id, Some(staged_draft.revision), file_only)
        .unwrap();
    std::fs::write(&path, b"changed attachment bytes").unwrap();
    assert!(matches!(
        session
            .stage_selected(
                updated.id,
                updated.revision,
                vec![UploadSelection::File {
                    selection_id: file_id
                }]
            )
            .await,
        Err(ServiceError::Attachment(
            codex_chatgpt_chat::attachment::AttachmentError::Changed
        ))
    ));
    assert_eq!(
        session.draft_read(updated.id).unwrap().unwrap().revision,
        updated.revision
    );
    assert!(lock(&session.memory).native.is_empty());
}
