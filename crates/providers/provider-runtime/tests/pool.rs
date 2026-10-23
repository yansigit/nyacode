#![allow(clippy::expect_used)]
use codex_provider_api::*;
use codex_provider_auth::*;
use codex_provider_runtime::*;
use serde_json::json;
use tokio_util::sync::CancellationToken;
async fn accounts(home: &std::path::Path) -> (PoolStore, Vec<SavedAccount>) {
    let auth = AuthStore::new(home);
    let cancel = CancellationToken::new();
    for label in ["a", "b"] {
        auth.login(
            "fifth",
            label,
            TokenRecord {
                access_token: label.into(),
                refresh_token: None,
                expires_at: None,
                metadata: json!({}),
                revoked: false,
            },
            &cancel,
        )
        .await
        .expect("valid test fixture");
    }
    let pool = PoolStore::new(home, "fifth", "endpoint");
    edit(&pool, PoolUpdate::Enroll { label: "a".into() }).await;
    edit(&pool, PoolUpdate::Enroll { label: "b".into() }).await;
    edit(
        &pool,
        PoolUpdate::Order {
            labels: vec!["a".into(), "b".into()],
        },
    )
    .await;
    (
        pool,
        auth.accounts("fifth", &cancel)
            .await
            .expect("valid test fixture"),
    )
}
async fn edit(pool: &PoolStore, update: PoolUpdate) {
    let cancel = CancellationToken::new();
    let revision = pool
        .settings(&cancel)
        .await
        .expect("valid test fixture")
        .revision;
    pool.update(revision, update, &cancel)
        .await
        .expect("valid test fixture");
}
fn quota() -> Failure {
    Failure {
        category: FailureCategory::QuotaExhausted,
        code: FailureCode::QuotaExhausted,
        scope: FailureScope::Model("m".into()),
        reset_at: None,
        retry_at: None,
        outcome: RequestOutcome::Rejected,
        another_account_may_help: true,
    }
}
#[tokio::test]
async fn automatic_activation_disable_priority_affinity_and_remove_are_independent() {
    let home = tempfile::tempdir().expect("valid test fixture");
    let (pool, accounts) = accounts(home.path()).await;
    let cancel = CancellationToken::new();
    assert!(
        pool.settings(&cancel)
            .await
            .expect("valid test fixture")
            .enabled()
    );
    assert_eq!(
        pool.candidates("new", &cancel)
            .await
            .expect("valid test fixture")[0]
            .reference
            .label,
        "a"
    );
    pool.accept("existing", &accounts[1].reference, &cancel)
        .await
        .expect("valid test fixture");
    assert_eq!(
        pool.candidates("existing", &cancel)
            .await
            .expect("valid test fixture")[0]
            .reference
            .label,
        "b"
    );
    edit(&pool, PoolUpdate::Prefer { label: "a".into() }).await;
    assert_eq!(
        pool.candidates("existing", &cancel)
            .await
            .expect("valid test fixture")[0]
            .reference
            .label,
        "b"
    );
    edit(&pool, PoolUpdate::Remove { label: "b".into() }).await;
    assert!(
        pool.settings(&cancel)
            .await
            .expect("valid test fixture")
            .enabled()
    );
    assert_eq!(
        pool.candidates("existing", &cancel)
            .await
            .expect("valid test fixture")[0]
            .reference
            .label,
        "a"
    );
    edit(&pool, PoolUpdate::Disable).await;
    edit(&pool, PoolUpdate::Enroll { label: "b".into() }).await;
    assert!(
        !pool
            .settings(&cancel)
            .await
            .expect("valid test fixture")
            .enabled()
    );
    assert_eq!(
        AuthStore::new(home.path())
            .selected("fifth")
            .expect("valid test fixture")
            .as_deref(),
        Some("b")
    );
}
#[tokio::test]
async fn cooldown_model_scope_single_recovery_probe_and_stale_success() {
    let home = tempfile::tempdir().expect("valid test fixture");
    let (pool, accounts) = accounts(home.path()).await;
    let cancel = CancellationToken::new();
    let old = pool
        .reserve(accounts[0].clone(), "m", 100, &cancel)
        .await
        .expect("valid test fixture")
        .expect("valid test fixture");
    let recent = pool
        .reserve(accounts[0].clone(), "m", 101, &cancel)
        .await
        .expect("valid test fixture")
        .expect("valid test fixture");
    pool.failed(&recent, &quota(), 101, &cancel)
        .await
        .expect("valid test fixture");
    pool.succeeded(&old, &cancel)
        .await
        .expect("valid test fixture");
    assert!(
        pool.reserve(accounts[0].clone(), "m", 500, &cancel)
            .await
            .expect("valid test fixture")
            .is_none()
    );
    assert!(
        pool.reserve(accounts[0].clone(), "other-model", 500, &cancel)
            .await
            .expect("valid test fixture")
            .is_some()
    );
    let probe = pool
        .reserve(accounts[0].clone(), "m", 1001, &cancel)
        .await
        .expect("valid test fixture")
        .expect("valid test fixture");
    assert!(
        pool.reserve(accounts[0].clone(), "m", 1002, &cancel)
            .await
            .expect("valid test fixture")
            .is_none()
    );
    assert!(
        pool.reserve(accounts[1].clone(), "m", 1002, &cancel)
            .await
            .expect("valid test fixture")
            .is_some()
    );
    pool.succeeded(&probe, &cancel)
        .await
        .expect("valid test fixture");
    drop(probe);
    assert!(
        pool.reserve(accounts[0].clone(), "m", 1003, &cancel)
            .await
            .expect("valid test fixture")
            .is_some()
    );
}
#[tokio::test]
async fn duplicate_credentials_share_limits_but_endpoints_do_not() {
    let home = tempfile::tempdir().expect("valid test fixture");
    let (pool, accounts) = accounts(home.path()).await;
    let cancel = CancellationToken::new();
    let auth = AuthStore::new(home.path());
    auth.login(
        "fifth",
        "duplicate",
        TokenRecord {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: None,
            metadata: json!({}),
            revoked: false,
        },
        &cancel,
    )
    .await
    .expect("valid test fixture");
    let duplicate = auth
        .enroll("fifth", "duplicate", &cancel)
        .await
        .expect("valid test fixture");
    let attempt = pool
        .reserve(accounts[0].clone(), "m", 1, &cancel)
        .await
        .expect("valid test fixture")
        .expect("valid test fixture");
    pool.failed(&attempt, &quota(), 1, &cancel)
        .await
        .expect("valid test fixture");
    assert!(
        pool.reserve(duplicate.clone(), "m", 2, &cancel)
            .await
            .expect("valid test fixture")
            .is_none()
    );
    let other = PoolStore::new(home.path(), "fifth", "other-endpoint");
    assert!(
        other
            .reserve(duplicate, "m", 2, &cancel)
            .await
            .expect("valid test fixture")
            .is_some()
    );
}
#[tokio::test]
async fn edits_require_matching_revision_and_exact_membership_order() {
    let home = tempfile::tempdir().expect("valid test fixture");
    let (pool, _) = accounts(home.path()).await;
    let cancel = CancellationToken::new();
    let revision = pool
        .settings(&cancel)
        .await
        .expect("valid test fixture")
        .revision;
    edit(&pool, PoolUpdate::Disable).await;
    assert!(
        pool.update(revision, PoolUpdate::Enable, &cancel)
            .await
            .is_err()
    );
    let revision = pool
        .settings(&cancel)
        .await
        .expect("valid test fixture")
        .revision;
    assert!(
        pool.update(
            revision,
            PoolUpdate::Order {
                labels: vec!["a".into(), "a".into()]
            },
            &cancel
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn committed_login_enrollment_recovers_without_callback_or_selection_change() {
    let home = tempfile::tempdir().expect("valid test fixture");
    let (pool, _) = accounts(home.path()).await;
    let cancel = CancellationToken::new();
    edit(&pool, PoolUpdate::Disable).await;
    let auth = AuthStore::new(home.path());
    let login = auth
        .begin_login("fifth", "new", &cancel)
        .await
        .expect("valid test fixture");
    let account = auth
        .commit_login_with(
            &login,
            TokenRecord {
                access_token: "new".into(),
                refresh_token: None,
                expires_at: None,
                metadata: json!({}),
                revoked: false,
            },
            false,
            &cancel,
            |account| pool.prepare_login(account, false, &cancel),
        )
        .await
        .expect("valid test fixture");
    // Simulate process death after credentials commit, before the caller resumes.
    let restarted = PoolStore::new(home.path(), "fifth", "endpoint");
    let status = restarted
        .status(None, &cancel)
        .await
        .expect("valid test fixture");
    assert!(
        status
            .accounts
            .iter()
            .any(|entry| entry.member && entry.account.reference == account)
    );
    assert!(!status.enabled);
    assert_eq!(status.selected.as_deref(), Some("b"));
}

#[tokio::test]
async fn recovery_probe_subprocess() {
    let Some(home) = std::env::var_os("CODEX_POOL_PROBE_TEST_HOME") else {
        return;
    };
    let cancellation = CancellationToken::new();
    let auth = AuthStore::new(std::path::Path::new(&home));
    let account = auth
        .accounts("fifth", &cancellation)
        .await
        .unwrap()
        .remove(0);
    let pool = PoolStore::new(std::path::Path::new(&home), "fifth", "endpoint");
    let probe = pool
        .reserve(account, "m", 1001, &cancellation)
        .await
        .unwrap();
    assert_eq!(
        probe.is_some(),
        std::env::var("CODEX_POOL_PROBE_TEST_EXPECT").unwrap() == "available"
    );
}

#[tokio::test]
async fn recovery_probe_is_exclusive_across_processes_and_released_on_exit() {
    let home = tempfile::tempdir().unwrap();
    let (pool, accounts) = accounts(home.path()).await;
    let cancellation = CancellationToken::new();
    let initial = pool
        .reserve(accounts[0].clone(), "m", 100, &cancellation)
        .await
        .unwrap()
        .unwrap();
    pool.failed(&initial, &quota(), 100, &cancellation)
        .await
        .unwrap();
    drop(initial);
    let probe = pool
        .reserve(accounts[0].clone(), "m", 1001, &cancellation)
        .await
        .unwrap()
        .unwrap();
    let child = |expected| {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "recovery_probe_subprocess", "--nocapture"])
            .env("CODEX_POOL_PROBE_TEST_HOME", home.path())
            .env("CODEX_POOL_PROBE_TEST_EXPECT", expected)
            .output()
            .unwrap()
    };
    let blocked = child("blocked");
    assert!(
        blocked.status.success(),
        "{}",
        String::from_utf8_lossy(&blocked.stdout)
    );
    drop(probe);
    let available = child("available");
    assert!(
        available.status.success(),
        "{}",
        String::from_utf8_lossy(&available.stdout)
    );
    // The successful subprocess exited without recording success; its probe lock
    // must nevertheless be released for a later real request.
    assert!(
        pool.reserve(accounts[0].clone(), "m", 1001, &cancellation)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn in_flight_success_cannot_clear_a_failure_observed_after_its_dispatch() {
    let home = tempfile::tempdir().unwrap();
    let (pool, accounts) = accounts(home.path()).await;
    let cancel = CancellationToken::new();
    let rejected = pool
        .reserve(accounts[0].clone(), "m", 100, &cancel)
        .await
        .unwrap()
        .unwrap();
    let in_flight = pool
        .reserve(accounts[0].clone(), "m", 101, &cancel)
        .await
        .unwrap()
        .unwrap();
    pool.failed(&rejected, &quota(), 102, &cancel)
        .await
        .unwrap();
    pool.succeeded(&in_flight, &cancel).await.unwrap();
    assert!(
        pool.reserve(accounts[0].clone(), "m", 103, &cancel)
            .await
            .unwrap()
            .is_none()
    );
    let probe = pool
        .reserve(accounts[0].clone(), "m", 1003, &cancel)
        .await
        .unwrap()
        .unwrap();
    pool.succeeded(&probe, &cancel).await.unwrap();
    // A later failure is a new observation, even after a successful probe.
    pool.failed(&in_flight, &quota(), 1004, &cancel)
        .await
        .unwrap();
    drop(probe);
    assert!(
        pool.reserve(accounts[0].clone(), "m", 1005, &cancel)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn concurrent_failures_cannot_shorten_an_authoritative_deadline() {
    let home = tempfile::tempdir().unwrap();
    let (pool, accounts) = accounts(home.path()).await;
    let cancellation = CancellationToken::new();
    let first = pool
        .reserve(accounts[0].clone(), "m", 100, &cancellation)
        .await
        .unwrap()
        .unwrap();
    let second = pool
        .reserve(accounts[0].clone(), "m", 101, &cancellation)
        .await
        .unwrap()
        .unwrap();
    let mut exhausted = quota();
    exhausted.reset_at = Some(10000);
    pool.failed(&first, &exhausted, 102, &cancellation)
        .await
        .unwrap();
    let mut temporary = quota();
    temporary.category = FailureCategory::TemporaryLimit;
    temporary.code = FailureCode::RateLimit;
    pool.failed(&second, &temporary, 103, &cancellation)
        .await
        .unwrap();
    assert!(
        pool.reserve(accounts[0].clone(), "m", 9999, &cancellation)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        pool.reserve(accounts[0].clone(), "m", 10000, &cancellation)
            .await
            .unwrap()
            .is_some()
    );
}
