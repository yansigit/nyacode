use super::*;
use app_test_support::ChatGptAuthFixture;
use app_test_support::write_chatgpt_auth;
use codex_config::types::AuthCredentialsStoreMode;
use codex_core::config::ConfigBuilder;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_state::chat::ChatDraftContent;

pub(super) struct Harness {
    pub(super) home: tempfile::TempDir,
    pub(super) auth: Arc<AuthManager>,
    pub(super) db: Arc<StateRuntime>,
    pub(super) sqlite: SqliteConfig,
    pub(super) service: Arc<Service>,
}

impl Harness {
    pub(super) async fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        write_auth(
            home.path(),
            "fixture-account",
            "fixture-user",
            "fixture-token",
        );
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .build()
            .await
            .unwrap();
        let auth = AuthManager::shared_from_config(&config, false)
            .await
            .unwrap();
        let sqlite = config.sqlite_config().clone();
        let db = StateRuntime::init(sqlite.clone(), "test-provider".into())
            .await
            .unwrap();
        let service = Service::new(Arc::clone(&auth), Some(Arc::clone(&db)), true);
        Self {
            home,
            auth,
            db,
            sqlite,
            service,
        }
    }

    pub(super) fn session(&self) -> Arc<SavedSession> {
        self.service
            .open_saved(ConnectionOrigin::InProcess, None)
            .unwrap()
    }

    pub(super) async fn draft(&self) -> codex_state::chat::ChatDraft {
        self.session()
            .draft_write(Uuid::new_v4(), None, content())
            .await
            .unwrap()
    }
}

pub(super) fn write_auth(home: &std::path::Path, account: &str, user: &str, token: &str) {
    write_chatgpt_auth(
        home,
        ChatGptAuthFixture::new(token)
            .account_id(account)
            .chatgpt_account_id(account)
            .chatgpt_user_id(user)
            .plan_type("plus"),
        AuthCredentialsStoreMode::File,
    )
    .unwrap();
}

pub(super) fn content() -> ChatDraftContent {
    ChatDraftContent {
        conversation_id: None,
        expected_parent_id: None,
        requested_model: None,
        text: "literal ! @ $ and a private fixture".into(),
        selections: Vec::new(),
    }
}

#[tokio::test]
async fn scope_lookup_rejects_a_live_scope_from_another_service() {
    let h = Harness::new().await;
    let scope = h
        .service
        .open_scope(ConnectionOrigin::InProcess, None)
        .unwrap();
    let other = Service::new(Arc::clone(&h.auth), Some(Arc::clone(&h.db)), true);
    other.open_saved(ConnectionOrigin::InProcess, None).unwrap();
    other
        .open_temporary(ConnectionOrigin::InProcess, None)
        .unwrap();
    assert!(matches!(
        other.saved_in_scope(&scope),
        Err(ServiceError::ScopeChanged)
    ));
    assert!(matches!(
        other.temporary_in_scope(&scope),
        Err(ServiceError::ScopeChanged)
    ));
}

#[tokio::test]
async fn trusted_origins_share_one_scope_and_remote_origins_are_rejected_first() {
    let h = Harness::new().await;
    let original = h.session();
    for origin in [
        ConnectionOrigin::InProcess,
        ConnectionOrigin::Stdio,
        ConnectionOrigin::LocalSocket,
    ] {
        assert!(Arc::ptr_eq(
            &original,
            &h.service
                .open_saved(origin, Some(original.scope_id()))
                .unwrap()
        ));
    }
    h.service.set_enabled(false);
    for origin in [ConnectionOrigin::WebSocket, ConnectionOrigin::RemoteControl] {
        assert!(matches!(
            h.service.open_saved(origin, None),
            Err(ServiceError::RemoteConnection)
        ));
    }
    assert!(matches!(
        h.service.open_saved(ConnectionOrigin::Stdio, None),
        Err(ServiceError::Native(ChatError::Disabled))
    ));
}

#[tokio::test]
async fn disable_enable_revokes_live_scopes_without_erasing_saved_drafts() {
    let h = Harness::new().await;
    let original = h.session();
    let draft = h.draft().await;
    let old_native = original.native().await.unwrap();
    h.service.set_enabled(false);
    h.service.set_enabled(true);
    assert_eq!(original.check(), Err(ServiceError::ScopeChanged));
    assert!(old_native.check_current().is_err());
    assert!(matches!(
        h.service
            .open_saved(ConnectionOrigin::LocalSocket, Some(original.scope_id())),
        Err(ServiceError::ScopeChanged)
    ));
    let replacement = h.session();
    assert_ne!(original.scope_id(), replacement.scope_id());
    assert_eq!(
        replacement
            .draft_read(draft.id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        draft.revision
    );
    let old_profile = original
        .store
        .as_ref()
        .unwrap()
        .profile(NATIVE_PROFILE_VERSION)
        .await
        .unwrap();
    let new_profile = replacement
        .store()
        .unwrap()
        .profile(NATIVE_PROFILE_VERSION)
        .await
        .unwrap();
    assert_eq!(old_profile.device_id, new_profile.device_id);
}

#[tokio::test]
async fn auth_refresh_revokes_old_work_and_account_switch_isolates_saved_content() {
    let h = Harness::new().await;
    let original = h.session();
    let draft = h.draft().await;
    write_auth(
        h.home.path(),
        "fixture-account",
        "fixture-user",
        "refreshed-token",
    );
    let previous = h.auth.auth_change_state_receiver().borrow().generation;
    h.auth.reload().await;
    assert!(h.auth.auth_change_state_receiver().borrow().generation > previous);
    assert_eq!(original.check(), Err(ServiceError::ScopeChanged));
    let refreshed = h.session();
    assert!(refreshed.draft_read(draft.id).await.unwrap().is_some());
    write_auth(
        h.home.path(),
        "another-account",
        "fixture-user",
        "another-token",
    );
    let previous = h.auth.auth_change_state_receiver().borrow().generation;
    h.auth.reload().await;
    assert!(h.auth.auth_change_state_receiver().borrow().generation > previous);
    assert_eq!(refreshed.check(), Err(ServiceError::ScopeChanged));
    assert!(h.session().draft_read(draft.id).await.unwrap().is_none());
    write_auth(
        h.home.path(),
        "fixture-account",
        "another-user",
        "another-user-token",
    );
    let previous = h.auth.auth_change_state_receiver().borrow().generation;
    h.auth.reload().await;
    assert!(h.auth.auth_change_state_receiver().borrow().generation > previous);
    assert!(h.session().draft_read(draft.id).await.unwrap().is_none());
}

#[tokio::test]
async fn journal_failure_leaves_the_read_only_native_session_available() {
    let h = Harness::new().await;
    h.db.close().await;
    let session = h.session();
    assert!(matches!(
        session.native().await,
        Err(ServiceError::Store(ChatStoreError::Unavailable))
    ));
    assert!(session.reader().await.unwrap().check_current().is_ok());
    assert!(session.native.get().is_none());
    assert!(session.read_only_native.get().is_some());
    assert!(matches!(
        session.draft_write(Uuid::new_v4(), None, content()).await,
        Err(ServiceError::Store(ChatStoreError::Unavailable))
    ));
}

#[tokio::test]
async fn global_config_reload_revokes_chat_before_returning() {
    let h = Harness::new().await;
    let manager = crate::config_manager::ConfigManager::new(
        h.home.path().into(),
        Vec::new(),
        codex_config::LoaderOverrides::default(),
        false,
        codex_config::CloudConfigBundleLoader::default(),
        codex_arg0::Arg0DispatchPaths::default(),
        Arc::new(codex_config::NoopThreadConfigLoader),
    );
    manager.bind_chat_service(&h.service);
    tokio::fs::write(
        h.home.path().join("config.toml"),
        "[features]\nchatgpt_chat = true\n",
    )
    .await
    .unwrap();
    manager.refresh_chat_feature().await;
    let original = h.session();
    tokio::fs::write(
        h.home.path().join("config.toml"),
        "[features]\nchatgpt_chat = false\n",
    )
    .await
    .unwrap();
    manager.refresh_chat_feature().await;
    assert_eq!(original.check(), Err(ServiceError::ScopeChanged));
    assert!(matches!(
        h.service.open_saved(ConnectionOrigin::InProcess, None),
        Err(ServiceError::Native(ChatError::Disabled))
    ));
}

#[tokio::test]
async fn changed_and_missing_attachment_selections_fail_before_native_upload_or_profile_write() {
    let h = Harness::new().await;
    let session = h.session();
    let selection_id = Uuid::new_v4();
    let mut body = content();
    body.selections.push(codex_state::chat::ChatFileSelection {
        id: selection_id,
        source: Some(h.home.path().join("missing.txt")),
        name: "missing.txt".into(),
        snapshot: None,
    });
    let draft = session
        .draft_write(Uuid::new_v4(), None, body)
        .await
        .unwrap();
    let result = session
        .upload_selected(
            draft.id,
            draft.revision,
            vec![UploadSelection::File { selection_id }],
            |_, _, _, _| panic!("upload cannot start"),
            &CancellationToken::new(),
        )
        .await;
    assert!(matches!(
        result,
        Err(ServiceError::Attachment(AttachmentError::Unreadable))
    ));
    assert!(session.native.get().is_none());
    assert!(session.draft_read(draft.id).await.unwrap().is_some());
    assert!(lock(&session.uploads).is_empty());
    let result = session
        .upload_selected(
            draft.id,
            Uuid::new_v4(),
            vec![UploadSelection::File { selection_id }],
            |_, _, _, _| panic!("upload cannot start"),
            &CancellationToken::new(),
        )
        .await;
    assert!(matches!(
        result,
        Err(ServiceError::Store(ChatStoreError::RevisionConflict))
    ));
}

#[tokio::test]
async fn staging_persists_only_metadata_and_recovered_selections_detect_changed_files() {
    let h = Harness::new().await;
    let session = h.session();
    let path = h.home.path().join("fixture.txt");
    let bytes = "distinctive attachment contents stay outside the database";
    tokio::fs::write(&path, bytes).await.unwrap();
    let selection_id = Uuid::new_v4();
    let mut body = content();
    body.selections.push(codex_state::chat::ChatFileSelection {
        id: selection_id,
        source: Some(path.clone()),
        name: "fixture.txt".into(),
        snapshot: None,
    });
    let initial = session
        .draft_write(Uuid::new_v4(), None, body)
        .await
        .unwrap();
    let (recorded, staged) = session
        .stage_selected(
            initial.id,
            initial.revision,
            vec![UploadSelection::File { selection_id }],
        )
        .await
        .unwrap();
    assert_ne!(initial.revision, recorded.revision);
    assert!(recorded.content.selections[0].snapshot.is_some());
    assert_eq!(staged[0].1.bytes(), bytes.as_bytes());
    assert!(session.native.get().is_none());
    let pool = h
        .sqlite
        .open_read_write_pool(&h.sqlite.state_db_path())
        .await
        .unwrap();
    let persisted: String =
        sqlx::query_scalar("SELECT selections FROM chat_drafts WHERE draft_id=?")
            .bind(initial.id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!persisted.contains(bytes));
    assert!(persisted.contains("sha256"));
    drop(staged);
    h.service.set_enabled(false);
    h.service.set_enabled(true);
    let recovered = h.session();
    tokio::fs::write(path, "changed attachment contents")
        .await
        .unwrap();
    let result = recovered
        .stage_selected(
            initial.id,
            recorded.revision,
            vec![UploadSelection::File { selection_id }],
        )
        .await;
    assert!(matches!(
        result,
        Err(ServiceError::Attachment(AttachmentError::Changed))
    ));
    assert_eq!(
        recovered
            .draft_read(initial.id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        recorded.revision
    );
    assert!(recovered.native.get().is_none());
}

#[tokio::test]
async fn reads_and_sends_share_the_persisted_native_session_when_recovery_is_available() {
    let h = Harness::new().await;
    let session = h.session();
    let reader = session.reader().await.unwrap();
    let sender = session.native().await.unwrap();
    assert!(Arc::ptr_eq(&reader, &sender));
    assert!(session.read_only_native.get().is_none());
    let read_only = Service::new(Arc::clone(&h.auth), None, true);
    let read_only_session = read_only
        .open_saved(ConnectionOrigin::InProcess, None)
        .unwrap();
    assert!(read_only_session.reader().await.is_ok());
    assert!(matches!(
        read_only_session.native().await,
        Err(ServiceError::Store(ChatStoreError::Unavailable))
    ));
    read_only.shutdown();
    h.service.shutdown();
    h.db.close().await;
}
