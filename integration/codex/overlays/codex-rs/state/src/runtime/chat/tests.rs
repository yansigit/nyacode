use super::*;
use crate::SqliteConfig;
use crate::migrations::STATE_MIGRATOR;
use codex_utils_absolute_path::test_support::PathExt;
use sqlx::migrate::Migrator;
use std::borrow::Cow;
use std::path::PathBuf;

#[path = "cache/tests.rs"]
mod cache_tests;

struct TestDb {
    home: PathBuf,
    sqlite: SqliteConfig,
    pool: Arc<SqlitePool>,
}

impl TestDb {
    async fn new() -> Self {
        Self::through(i64::MAX).await
    }

    async fn through(version: i64) -> Self {
        let home = crate::runtime::test_support::unique_temp_dir();
        tokio::fs::create_dir_all(&home).await.unwrap();
        let sqlite = SqliteConfig::new_for_testing(home.as_path().abs());
        let pool = Arc::new(
            sqlite
                .open_read_write_pool(&sqlite.state_db_path())
                .await
                .unwrap(),
        );
        Migrator {
            migrations: Cow::Owned(
                STATE_MIGRATOR
                    .migrations
                    .iter()
                    .filter(|m| m.version <= version)
                    .cloned()
                    .collect(),
            ),
            ..Migrator::DEFAULT
        }
        .run(pool.as_ref())
        .await
        .unwrap();
        Self { home, sqlite, pool }
    }

    fn store(&self) -> SavedChatStore {
        SavedChatStore {
            pool: Arc::clone(&self.pool),
            scope: scope("principal", "account", None),
        }
    }

    async fn other_process(&self) -> SavedChatStore {
        let pool = self
            .sqlite
            .open_read_write_pool(&self.sqlite.state_db_path())
            .await
            .unwrap();
        SavedChatStore {
            pool: Arc::new(pool),
            scope: scope("principal", "account", None),
        }
    }

    async fn close(self) {
        self.pool.close().await;
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn scope(principal: &str, account: &str, workspace: Option<&str>) -> ChatScope {
    ChatScope::new(
        principal.into(),
        account.into(),
        workspace.map(str::to_owned),
    )
    .unwrap()
}

fn content(text: &str, conversation: Option<&str>) -> ChatDraftContent {
    ChatDraftContent {
        text: text.into(),
        conversation_id: conversation.map(str::to_owned),
        expected_parent_id: conversation.map(|_| "parent-fixture".into()),
        requested_model: None,
        selections: Vec::new(),
    }
}

async fn draft(store: &SavedChatStore, conversation: Option<&str>) -> ChatDraft {
    store
        .write_draft(
            Uuid::new_v4(),
            None,
            content("saved fixture text", conversation),
        )
        .await
        .unwrap()
}

fn request(draft: &ChatDraft) -> ChatSubmissionRequest {
    ChatSubmissionRequest {
        submission_id: Uuid::new_v4(),
        client_message_id: Uuid::new_v4(),
        draft_id: draft.id,
        expected_revision: draft.revision,
        expected_parent_id: draft
            .content
            .expected_parent_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        requested_model: "fixture-model".into(),
        files: Vec::new(),
    }
}

fn proof(submission: &ChatSubmission) -> ChatAcceptanceProof {
    ChatAcceptanceProof {
        conversation_id: submission
            .conversation_id
            .clone()
            .unwrap_or_else(|| "created-conversation".into()),
        client_message_id: submission.client_message_id,
        parent_message_id: submission.expected_parent_id.clone(),
    }
}

async fn expire(store: &SavedChatStore, id: Uuid) {
    sqlx::query("UPDATE chat_submissions SET lease_until_ms=0 WHERE submission_id=?")
        .bind(id.to_string())
        .execute(store.pool.as_ref())
        .await
        .unwrap();
}

#[tokio::test]
async fn migration_upgrades_without_changing_existing_codex_state() {
    let db = TestDb::through(61).await;
    sqlx::query("INSERT INTO thread_read_receipts(thread_id,revision) VALUES('existing-thread','original-revision')")
        .execute(db.pool.as_ref()).await.unwrap();
    STATE_MIGRATOR.run(db.pool.as_ref()).await.unwrap();
    let revision: String = sqlx::query_scalar(
        "SELECT revision FROM thread_read_receipts WHERE thread_id='existing-thread'",
    )
    .fetch_one(db.pool.as_ref())
    .await
    .unwrap();
    assert_eq!(revision, "original-revision");
    assert!(
        db.store()
            .list_drafts(None, 10)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let profile = db
        .store()
        .profile("chrome136-linux-native-v1")
        .await
        .unwrap();
    assert_eq!(profile.version, "chrome136-linux-native-v1");
    db.close().await;
}

#[tokio::test]
async fn profile_is_stable_across_pools_and_rejects_implicit_profile_changes() {
    let db = TestDb::new().await;
    let first = db.store();
    let second = db.other_process().await;
    let (a, b) = tokio::join!(first.profile("profile-v1"), second.profile("profile-v1"));
    assert_eq!(a.unwrap().device_id, b.unwrap().device_id);
    assert!(matches!(
        second.profile("profile-v2").await,
        Err(ChatStoreError::ProfileMismatch)
    ));
    second.pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn chat_writes_use_full_durability_on_the_actual_connection() {
    let db = TestDb::new().await;
    let store = db.store();
    let mut connection = store.write_connection().await.unwrap();
    let sync: i64 = sqlx::query_scalar("PRAGMA synchronous")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    let fullsync: i64 = sqlx::query_scalar("PRAGMA fullfsync")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    assert_eq!(sync, 2);
    assert_eq!(fullsync, 1);
    assert_eq!(mode, "wal");
    drop(connection);
    db.close().await;
}

#[tokio::test]
async fn draft_revision_conflicts_preserve_both_clients_input() {
    let db = TestDb::new().await;
    let first = db.store();
    let second = db.other_process().await;
    let original = draft(&first, None).await;
    let a = content("client A edits", None);
    let b = content("client B edits", None);
    let (ra, rb) = tokio::join!(
        first.write_draft(original.id, Some(original.revision), a.clone()),
        second.write_draft(original.id, Some(original.revision), b.clone()),
    );
    assert!(matches!(
        (&ra, &rb),
        (Ok(_), Err(ChatStoreError::RevisionConflict))
            | (Err(ChatStoreError::RevisionConflict), Ok(_))
    ));
    let winner = first.read_draft(original.id).await.unwrap().unwrap();
    assert_ne!(winner.revision, original.revision);
    assert!(winner.content == a || winner.content == b);
    assert_eq!(a.text, "client A edits");
    assert_eq!(b.text, "client B edits");
    assert_eq!(
        first.delete_draft(original.id, original.revision).await,
        Err(ChatStoreError::RevisionConflict)
    );
    first
        .delete_draft(original.id, winner.revision)
        .await
        .unwrap();
    let recreated = first.write_draft(original.id, None, a).await.unwrap();
    assert!(matches!(
        second
            .write_draft(original.id, Some(winner.revision), b)
            .await,
        Err(ChatStoreError::RevisionConflict)
    ));
    assert_ne!(recreated.revision, winner.revision);
    second.pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn drafts_profiles_and_submissions_are_isolated_by_all_scope_fields() {
    let db = TestDb::new().await;
    let first = db.store();
    let original = draft(&first, None).await;
    let profile = first.profile("profile-v1").await.unwrap();
    let (submission, lease) = first
        .begin_submission(request(&original), Uuid::new_v4())
        .await
        .unwrap();
    first.mark_transmitting(&lease).await.unwrap();
    first
        .record_conversation(&lease, "scoped-conversation")
        .await
        .unwrap();
    assert_eq!(
        first
            .pending_submission_for_draft(original.id)
            .await
            .unwrap()
            .unwrap()
            .submission_id,
        submission.submission_id
    );
    assert_eq!(
        first
            .pending_submission_for_conversation("scoped-conversation")
            .await
            .unwrap()
            .unwrap()
            .submission_id,
        submission.submission_id
    );
    for other_scope in [
        scope("other", "account", None),
        scope("principal", "other", None),
        scope("principal", "account", Some("workspace")),
    ] {
        let other = SavedChatStore {
            pool: Arc::clone(&db.pool),
            scope: other_scope,
        };
        assert!(other.read_draft(original.id).await.unwrap().is_none());
        assert!(
            other
                .read_submission(submission.submission_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(other.list_drafts(None, 100).await.unwrap().items.is_empty());
        assert!(other.pending_submission_ids().await.unwrap().is_empty());
        assert!(
            other
                .pending_submission_for_draft(original.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            other
                .pending_submission_for_conversation("scoped-conversation")
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            other.mark_transmitting(&lease).await,
            Err(ChatStoreError::NotFound)
        ));
        assert_ne!(
            other.profile("profile-v1").await.unwrap().device_id,
            profile.device_id
        );
        let independent = other
            .write_draft(original.id, None, content("other account text", None))
            .await
            .unwrap();
        let mut same_ids = request(&independent);
        same_ids.submission_id = submission.submission_id;
        same_ids.client_message_id = submission.client_message_id;
        other
            .begin_submission(same_ids, Uuid::new_v4())
            .await
            .unwrap();
    }
    assert_eq!(
        first
            .read_draft(original.id)
            .await
            .unwrap()
            .unwrap()
            .content
            .text,
        "saved fixture text"
    );
    db.close().await;
}

#[tokio::test]
async fn drafts_page_equal_timestamps_without_loading_full_bodies() {
    let db = TestDb::new().await;
    let store = db.store();
    let mut expected = Vec::new();
    for _ in 0..5 {
        let d = store
            .write_draft(Uuid::new_v4(), None, content(&"é".repeat(1000), None))
            .await
            .unwrap();
        expected.push(d.id);
    }
    sqlx::query("UPDATE chat_drafts SET updated_at_ms=42")
        .execute(db.pool.as_ref())
        .await
        .unwrap();
    expected.sort_unstable_by(|a, b| b.cmp(a));
    let mut actual = Vec::new();
    let mut cursor = None;
    loop {
        let page = store.list_drafts(cursor, 2).await.unwrap();
        for d in page.items {
            assert_eq!(d.preview, "é".repeat(160));
            actual.push(d.id);
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(actual, expected);
    assert!(matches!(
        store.list_drafts(None, 0).await,
        Err(ChatStoreError::InvalidInput)
    ));
    db.close().await;
}

#[tokio::test]
async fn concurrent_processes_serialize_a_conversation_but_not_other_conversations() {
    let db = TestDb::new().await;
    let first = db.store();
    let second = db.other_process().await;
    let a = draft(&first, Some("same-conversation")).await;
    let b = draft(&second, Some("same-conversation")).await;
    let (ra, rb) = tokio::join!(
        first.begin_submission(request(&a), Uuid::new_v4()),
        second.begin_submission(request(&b), Uuid::new_v4()),
    );
    assert!(matches!(
        (&ra, &rb),
        (Ok(_), Err(ChatStoreError::PendingSubmission))
            | (Err(ChatStoreError::PendingSubmission), Ok(_))
    ));
    assert_eq!(first.pending_submission_ids().await.unwrap().len(), 1);
    let other = draft(&second, Some("independent-conversation")).await;
    second
        .begin_submission(request(&other), Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(first.pending_submission_ids().await.unwrap().len(), 2);
    assert_eq!(
        first.read_draft(a.id).await.unwrap().unwrap().content.text,
        a.content.text
    );
    assert_eq!(
        second.read_draft(b.id).await.unwrap().unwrap().content.text,
        b.content.text
    );
    second.pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn transmission_is_single_use_and_unknown_acceptance_blocks_duplicate_retry() {
    let db = TestDb::new().await;
    let store = db.store();
    let d = draft(&store, Some("conversation")).await;
    let (s, lease) = store
        .begin_submission(request(&d), Uuid::new_v4())
        .await
        .unwrap();
    assert!(!s.transmission_started);
    assert!(!s.accepted);
    let permit = store.mark_transmitting(&lease).await.unwrap();
    assert_eq!(permit.submission_id(), s.submission_id);
    assert!(matches!(
        store.mark_transmitting(&lease).await,
        Err(ChatStoreError::ReconcileOnly)
    ));
    assert_eq!(
        store
            .finish_submission(&lease, ChatSubmissionOutcome::Completed, None)
            .await,
        Err(ChatStoreError::InvalidTransition)
    );
    store
        .finish_submission(&lease, ChatSubmissionOutcome::LocalStopped, None)
        .await
        .unwrap();
    let pending = store
        .read_submission(s.submission_id)
        .await
        .unwrap()
        .unwrap();
    assert!(pending.active);
    assert!(!pending.accepted);
    assert_eq!(pending.outcome, ChatSubmissionOutcome::LocalStopped);
    assert!(matches!(
        store.begin_submission(request(&d), Uuid::new_v4()).await,
        Err(ChatStoreError::PendingSubmission)
    ));
    assert_eq!(
        store.delete_draft(d.id, d.revision).await,
        Err(ChatStoreError::PendingSubmission)
    );
    let mut duplicate = request(&d);
    duplicate.submission_id = s.submission_id;
    assert!(matches!(
        store.begin_submission(duplicate, Uuid::new_v4()).await,
        Err(ChatStoreError::DuplicateSubmission)
    ));
    let mut duplicate = request(&d);
    duplicate.client_message_id = s.client_message_id;
    assert!(matches!(
        store.begin_submission(duplicate, Uuid::new_v4()).await,
        Err(ChatStoreError::DuplicateSubmission)
    ));
    db.close().await;
}

#[tokio::test]
async fn pre_transmission_failure_preserves_draft_and_never_reuses_submission_id() {
    let db = TestDb::new().await;
    let store = db.store();
    let d = draft(&store, None).await;
    let (s, lease) = store
        .begin_submission(request(&d), Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        store.record_acceptance(&lease, proof(&s)).await,
        Err(ChatStoreError::InvalidTransition)
    );
    store
        .finish_submission(&lease, ChatSubmissionOutcome::Failed, None)
        .await
        .unwrap();
    let ended = store
        .read_submission(s.submission_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!ended.active);
    assert!(!ended.transmission_started);
    assert_eq!(
        store.read_draft(d.id).await.unwrap().unwrap().content.text,
        d.content.text
    );
    assert!(matches!(
        store.mark_transmitting(&lease).await,
        Err(ChatStoreError::LeaseLost)
    ));
    let mut duplicate = request(&d);
    duplicate.submission_id = s.submission_id;
    assert!(matches!(
        store.begin_submission(duplicate, Uuid::new_v4()).await,
        Err(ChatStoreError::DuplicateSubmission)
    ));
    // A distinct explicit submission may proceed after known pre-send failure.
    store
        .begin_submission(request(&d), Uuid::new_v4())
        .await
        .unwrap();
    db.close().await;
}

#[tokio::test]
async fn acceptance_requires_exact_ids_and_survives_generation_failure_and_new_typing() {
    let db = TestDb::new().await;
    let store = db.store();
    let d = draft(&store, None).await;
    let (s, lease) = store
        .begin_submission(request(&d), Uuid::new_v4())
        .await
        .unwrap();
    store.mark_transmitting(&lease).await.unwrap();
    let new_draft = store
        .write_draft(d.id, Some(d.revision), content("next unsent thought", None))
        .await
        .unwrap();
    let mut wrong = proof(&s);
    wrong.client_message_id = Uuid::new_v4();
    assert_eq!(
        store.record_acceptance(&lease, wrong).await,
        Err(ChatStoreError::InvalidTransition)
    );
    let mut wrong = proof(&s);
    wrong.parent_message_id = "other-parent".into();
    assert_eq!(
        store.record_acceptance(&lease, wrong).await,
        Err(ChatStoreError::InvalidTransition)
    );
    store.record_acceptance(&lease, proof(&s)).await.unwrap();
    store
        .finish_submission(
            &lease,
            ChatSubmissionOutcome::Failed,
            Some("substituted-model"),
        )
        .await
        .unwrap();
    let accepted = store
        .read_submission(s.submission_id)
        .await
        .unwrap()
        .unwrap();
    assert!(accepted.accepted);
    assert!(!accepted.active);
    assert_eq!(accepted.snapshot.draft.text, "saved fixture text");
    assert_eq!(
        accepted.resolved_model.as_deref(),
        Some("substituted-model")
    );
    assert_eq!(accepted.snapshot.requested_model, "fixture-model");
    assert_eq!(
        store.read_draft(d.id).await.unwrap().unwrap().revision,
        new_draft.revision
    );
    assert_eq!(
        store.read_draft(d.id).await.unwrap().unwrap().content.text,
        "next unsent thought"
    );
    store.delete_draft(d.id, new_draft.revision).await.unwrap();
    assert_eq!(
        store
            .read_submission(s.submission_id)
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .draft
            .text,
        "saved fixture text"
    );
    db.close().await;
}

#[tokio::test]
async fn lease_takeover_is_fenced_and_always_reconciliation_only() {
    let db = TestDb::new().await;
    let first = db.store();
    let second = db.other_process().await;
    for transmitted in [false, true] {
        let d = draft(&first, None).await;
        let owner = Uuid::new_v4();
        let (s, old_lease) = first.begin_submission(request(&d), owner).await.unwrap();
        if transmitted {
            first.mark_transmitting(&old_lease).await.unwrap();
        }
        assert!(matches!(
            second
                .claim_reconciliation(s.submission_id, Uuid::new_v4())
                .await,
            Err(ChatStoreError::LeaseLost)
        ));
        first.renew_lease(&old_lease).await.unwrap();
        expire(&first, s.submission_id).await;
        let (a, b) = tokio::join!(
            first.claim_reconciliation(s.submission_id, Uuid::new_v4()),
            second.claim_reconciliation(s.submission_id, Uuid::new_v4()),
        );
        let lease = match (a, b) {
            (Ok((_, lease)), Err(ChatStoreError::LeaseLost))
            | (Err(ChatStoreError::LeaseLost), Ok((_, lease))) => lease,
            _ => panic!("exactly one process must acquire reconciliation ownership"),
        };
        assert_eq!(
            first.renew_lease(&old_lease).await,
            Err(ChatStoreError::LeaseLost)
        );
        assert!(matches!(
            first.mark_transmitting(&old_lease).await,
            Err(ChatStoreError::LeaseLost)
        ));
        assert!(matches!(
            second.mark_transmitting(&lease).await,
            Err(ChatStoreError::ReconcileOnly)
        ));
        if transmitted {
            second
                .finish_submission(&lease, ChatSubmissionOutcome::Failed, None)
                .await
                .unwrap();
            assert!(
                first
                    .read_submission(s.submission_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .active
            );
            second.record_acceptance(&lease, proof(&s)).await.unwrap();
            second
                .finish_submission(&lease, ChatSubmissionOutcome::Completed, None)
                .await
                .unwrap();
        } else {
            second
                .finish_submission(&lease, ChatSubmissionOutcome::Failed, None)
                .await
                .unwrap();
        }
        assert!(
            !first
                .read_submission(s.submission_id)
                .await
                .unwrap()
                .unwrap()
                .active
        );
    }
    second.pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn ownership_generation_release_invalidates_held_leases() {
    let db = TestDb::new().await;
    let store = db.store();
    let owner = Uuid::new_v4();
    let d = draft(&store, None).await;
    let (s, lease) = store.begin_submission(request(&d), owner).await.unwrap();
    store.release_owner(owner).await.unwrap();
    assert!(matches!(
        store.mark_transmitting(&lease).await,
        Err(ChatStoreError::LeaseLost)
    ));
    let (_, recovered) = store
        .claim_reconciliation(s.submission_id, Uuid::new_v4())
        .await
        .unwrap();
    assert!(matches!(
        store.mark_transmitting(&recovered).await,
        Err(ChatStoreError::ReconcileOnly)
    ));
    db.close().await;
}

#[tokio::test]
async fn conversation_assignment_is_durable_but_does_not_prove_acceptance() {
    let db = TestDb::new().await;
    let store = db.store();
    let d = draft(&store, None).await;
    let (s, lease) = store
        .begin_submission(request(&d), Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        store.record_conversation(&lease, "assigned").await,
        Err(ChatStoreError::InvalidTransition)
    );
    store.mark_transmitting(&lease).await.unwrap();
    store.record_conversation(&lease, "assigned").await.unwrap();
    assert_eq!(
        store.record_conversation(&lease, "different").await,
        Err(ChatStoreError::InvalidTransition)
    );
    let assigned = store
        .read_submission(s.submission_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(assigned.conversation_id.as_deref(), Some("assigned"));
    assert!(!assigned.accepted);
    let second = draft(&store, Some("assigned")).await;
    assert!(matches!(
        store
            .begin_submission(request(&second), Uuid::new_v4())
            .await,
        Err(ChatStoreError::PendingSubmission)
    ));
    store
        .finish_submission(&lease, ChatSubmissionOutcome::Failed, None)
        .await
        .unwrap();
    assert!(
        store
            .read_submission(s.submission_id)
            .await
            .unwrap()
            .unwrap()
            .active
    );
    db.close().await;
}

#[tokio::test]
async fn journal_unavailability_never_returns_a_transmission_permit() {
    let db = TestDb::new().await;
    let store = db.store();
    let d = draft(&store, None).await;
    let (s, lease) = store
        .begin_submission(request(&d), Uuid::new_v4())
        .await
        .unwrap();
    db.pool.close().await;
    assert!(matches!(
        store.mark_transmitting(&lease).await,
        Err(ChatStoreError::Unavailable)
    ));
    let reopened = db.other_process().await;
    assert!(
        !reopened
            .read_submission(s.submission_id)
            .await
            .unwrap()
            .unwrap()
            .transmission_started
    );
    reopened.pool.close().await;
    db.close().await;
}

#[tokio::test]
async fn selections_persist_only_paths_and_metadata_and_validate_size_before_journaling() {
    let db = TestDb::new().await;
    let store = db.store();
    let selection = ChatFileSelection {
        id: Uuid::new_v4(),
        source: None,
        name: "clipboard.png".into(),
        snapshot: Some(ChatFileSnapshot {
            sha256: "a".repeat(64),
            size: 42,
            modified_at_ms: None,
            kind: ChatFileKind::Png,
            dimensions: Some((10, 10)),
        }),
    };
    let mut body = content("attachment fixture", None);
    body.selections.push(selection.clone());
    let d = store
        .write_draft(Uuid::new_v4(), None, body.clone())
        .await
        .unwrap();
    let mut r = request(&d);
    r.files.push(ChatUploadedFile {
        selection_id: selection.id,
        server_file_id: "file-fixture".into(),
        snapshot: selection.snapshot.clone().unwrap(),
    });
    let (s, _) = store.begin_submission(r, Uuid::new_v4()).await.unwrap();
    let stored = store
        .read_submission(s.submission_id)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.snapshot.draft.selections[0].source.is_none());
    let json: String =
        sqlx::query_scalar("SELECT snapshot FROM chat_submissions WHERE submission_id=?")
            .bind(s.submission_id.to_string())
            .fetch_one(db.pool.as_ref())
            .await
            .unwrap();
    for unexpected in ["bytes", "https://", "cookie", "token", "authorization"] {
        assert!(!json.contains(unexpected));
    }
    let mut poisoned: serde_json::Value = serde_json::from_str(&json).unwrap();
    poisoned["files"][0]["signed_url"] = "https://storage.invalid/?signature=secret".into();
    assert!(types::decode::<ChatSubmissionSnapshot>(&poisoned.to_string()).is_err());
    body.selections[0].snapshot.as_mut().unwrap().size = MAX_CHAT_SUBMISSION_BYTES + 1;
    assert!(matches!(
        store.write_draft(Uuid::new_v4(), None, body).await,
        Err(ChatStoreError::InvalidInput)
    ));
    assert_eq!(store.list_drafts(None, 100).await.unwrap().items.len(), 1);
    db.close().await;
}

#[tokio::test]
async fn failed_journal_transaction_does_not_partially_create_submission() {
    let db = TestDb::new().await;
    let store = db.store();
    let d = draft(&store, None).await;
    sqlx::query("CREATE TRIGGER reject_chat_submission BEFORE INSERT ON chat_submissions BEGIN SELECT RAISE(ABORT, 'fixture failure'); END")
        .execute(db.pool.as_ref()).await.unwrap();
    let r = request(&d);
    let id = r.submission_id;
    assert!(matches!(
        store.begin_submission(r, Uuid::new_v4()).await,
        Err(ChatStoreError::Unavailable)
    ));
    assert!(store.read_submission(id).await.unwrap().is_none());
    assert_eq!(
        store.read_draft(d.id).await.unwrap().unwrap().revision,
        d.revision
    );
    db.close().await;
}

#[tokio::test]
async fn process_exit_around_transmission_preserves_recovery_without_replay() {
    for stage in ["prepared", "transmitting", "uncommitted"] {
        let db = TestDb::new().await;
        let store = db.store();
        let d = store
            .write_draft(Uuid::nil(), None, content("before crash", None))
            .await
            .unwrap();
        let executable = std::env::current_exe().unwrap();
        let directory = db.home.clone();
        let status = tokio::task::spawn_blocking(move || {
            std::process::Command::new(executable)
                .args([
                    "--exact",
                    "runtime::chat::tests::crash_worker",
                    "--nocapture",
                ])
                .env("NYACODE_CHAT_CRASH_TEST_HOME", directory)
                .env("NYACODE_CHAT_CRASH_TEST_STAGE", stage)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(
            status.status.code(),
            Some(73),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert_eq!(
            store.read_draft(d.id).await.unwrap().unwrap().content.text,
            "before crash"
        );
        let pending = store.pending_submission_ids().await.unwrap();
        if stage == "uncommitted" {
            assert!(pending.is_empty());
        } else {
            assert_eq!(pending.len(), 1);
            let recovered = store.read_submission(pending[0]).await.unwrap().unwrap();
            assert_eq!(recovered.transmission_started, stage == "transmitting");
            assert!(!recovered.accepted);
            expire(&store, pending[0]).await;
            let (_, lease) = store
                .claim_reconciliation(pending[0], Uuid::new_v4())
                .await
                .unwrap();
            assert!(matches!(
                store.mark_transmitting(&lease).await,
                Err(ChatStoreError::ReconcileOnly)
            ));
        }
        db.close().await;
    }
}

#[tokio::test]
async fn crash_worker() {
    let Some(directory) = std::env::var_os("NYACODE_CHAT_CRASH_TEST_HOME") else {
        return;
    };
    let directory = PathBuf::from(directory);
    let sqlite = SqliteConfig::new_for_testing(directory.as_path().abs());
    let pool = Arc::new(
        sqlite
            .open_read_write_pool(&sqlite.state_db_path())
            .await
            .unwrap(),
    );
    let store = SavedChatStore {
        pool,
        scope: scope("principal", "account", None),
    };
    let stage = std::env::var("NYACODE_CHAT_CRASH_TEST_STAGE").unwrap();
    if stage == "uncommitted" {
        let mut connection = store.write_connection().await.unwrap();
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query("UPDATE chat_drafts SET text='uncommitted fixture'")
            .execute(&mut *tx)
            .await
            .unwrap();
        std::process::exit(73);
    }
    let d = store.read_draft(Uuid::nil()).await.unwrap().unwrap();
    let (_, lease) = store
        .begin_submission(request(&d), Uuid::new_v4())
        .await
        .unwrap();
    if stage == "transmitting" {
        store.mark_transmitting(&lease).await.unwrap();
    }
    std::process::exit(73);
}

#[tokio::test]
async fn terminal_acceptance_advances_the_exact_saved_draft_and_retains_the_journal() {
    for outcome in [
        ChatSubmissionOutcome::Completed,
        ChatSubmissionOutcome::Failed,
    ] {
        let db = TestDb::new().await;
        let store = db.store();
        let mut body = content("submitted saved input", None);
        body.requested_model = Some("fixture-model".into());
        let original = store.write_draft(Uuid::new_v4(), None, body).await.unwrap();
        let (submission, lease) = store
            .begin_submission(request(&original), Uuid::new_v4())
            .await
            .unwrap();
        store.mark_transmitting(&lease).await.unwrap();
        store
            .record_acceptance(&lease, proof(&submission))
            .await
            .unwrap();
        store
            .finish_submission_and_advance(
                &lease,
                outcome,
                Some("reported-model"),
                Some("response-parent"),
            )
            .await
            .unwrap();
        let advanced = db
            .other_process()
            .await
            .read_draft(original.id)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(advanced.revision, original.revision);
        assert!(advanced.content.text.is_empty());
        assert!(advanced.content.selections.is_empty());
        assert_eq!(
            advanced.content.conversation_id.as_deref(),
            Some("created-conversation")
        );
        assert_eq!(
            advanced.content.expected_parent_id.as_deref(),
            Some("response-parent")
        );
        assert_eq!(
            advanced.content.requested_model.as_deref(),
            Some("fixture-model")
        );
        let recorded = store
            .read_submission(submission.submission_id)
            .await
            .unwrap()
            .unwrap();
        assert!(recorded.accepted && !recorded.active);
        assert!(recorded.snapshot.draft == original.content);
        assert_eq!(recorded.outcome, outcome);
        db.close().await;
    }
}

#[tokio::test]
async fn draft_advancement_preserves_a_concurrent_revision_even_with_identical_text() {
    let db = TestDb::new().await;
    let store = db.store();
    let original = draft(&store, None).await;
    let (submission, lease) = store
        .begin_submission(request(&original), Uuid::new_v4())
        .await
        .unwrap();
    store.mark_transmitting(&lease).await.unwrap();
    store
        .record_acceptance(&lease, proof(&submission))
        .await
        .unwrap();
    let mut edited = original.content.clone();
    edited.requested_model = Some("next-model".into());
    let newer = store
        .write_draft(original.id, Some(original.revision), edited)
        .await
        .unwrap();
    store
        .finish_submission_and_advance(
            &lease,
            ChatSubmissionOutcome::Completed,
            None,
            Some("response-parent"),
        )
        .await
        .unwrap();
    let retained = store.read_draft(original.id).await.unwrap().unwrap();
    assert_eq!(retained.revision, newer.revision);
    assert!(retained.content == newer.content);
    db.close().await;
}

#[tokio::test]
async fn unknown_acceptance_keeps_the_draft_and_reservation_for_reconciliation() {
    let db = TestDb::new().await;
    let store = db.store();
    let original = draft(&store, None).await;
    let (submission, lease) = store
        .begin_submission(request(&original), Uuid::new_v4())
        .await
        .unwrap();
    store.mark_transmitting(&lease).await.unwrap();
    store
        .finish_submission_and_advance(
            &lease,
            ChatSubmissionOutcome::Failed,
            None,
            Some("unproven-parent"),
        )
        .await
        .unwrap();
    let retained = store.read_draft(original.id).await.unwrap().unwrap();
    assert_eq!(retained.revision, original.revision);
    assert!(retained.content == original.content);
    assert!(
        store
            .read_submission(submission.submission_id)
            .await
            .unwrap()
            .unwrap()
            .active
    );
    db.close().await;
}

#[tokio::test]
async fn failed_draft_advancement_rolls_back_the_terminal_journal_checkpoint() {
    let db = TestDb::new().await;
    let store = db.store();
    let original = draft(&store, None).await;
    let (submission, lease) = store
        .begin_submission(request(&original), Uuid::new_v4())
        .await
        .unwrap();
    store.mark_transmitting(&lease).await.unwrap();
    store
        .record_acceptance(&lease, proof(&submission))
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER reject_draft_advance BEFORE UPDATE ON chat_drafts BEGIN SELECT RAISE(ABORT, 'fixture rollback'); END")
        .execute(db.pool.as_ref()).await.unwrap();
    assert!(
        store
            .finish_submission_and_advance(
                &lease,
                ChatSubmissionOutcome::Completed,
                None,
                Some("response-parent")
            )
            .await
            .is_err()
    );
    let retained = store.read_draft(original.id).await.unwrap().unwrap();
    assert_eq!(retained.revision, original.revision);
    assert!(retained.content == original.content);
    let recorded = store
        .read_submission(submission.submission_id)
        .await
        .unwrap()
        .unwrap();
    assert!(recorded.accepted && recorded.active);
    assert_eq!(recorded.outcome, ChatSubmissionOutcome::InProgress);
    db.close().await;
}
