use codex_provider_auth::*;
use serde_json::json;
use tokio_util::sync::CancellationToken;
fn token(value: &str) -> TokenRecord {
    TokenRecord {
        access_token: value.into(),
        refresh_token: Some("refresh".into()),
        expires_at: None,
        metadata: json!({}),
        revoked: false,
    }
}
#[tokio::test]
async fn add_preserves_selection_and_logout_fences_late_callbacks() {
    let home = tempfile::tempdir().unwrap();
    let auth = AuthStore::new(home.path());
    let cancel = CancellationToken::new();
    auth.login("p", "a", token("a"), &cancel).await.unwrap();
    let login = auth.begin_login("p", "b", &cancel).await.unwrap();
    let b = auth
        .commit_login(&login, token("b"), false, &cancel)
        .await
        .unwrap();
    assert_eq!(auth.selected("p").unwrap().as_deref(), Some("a"));
    assert_eq!(auth.accounts("p", &cancel).await.unwrap().len(), 2);
    let late = auth.begin_login("p", "b", &cancel).await.unwrap();
    auth.logout_account("p", "b", &cancel).await.unwrap();
    assert!(
        auth.commit_login(&late, token("late"), false, &cancel)
            .await
            .is_err()
    );
    assert!(auth.credentials_for("p", &b, None, &cancel).await.is_err());
}
#[tokio::test]
async fn replacement_and_cancellation_cannot_overwrite_new_generation() {
    let home = tempfile::tempdir().unwrap();
    let auth = AuthStore::new(home.path());
    let cancel = CancellationToken::new();
    let first = auth.begin_login("p", "a", &cancel).await.unwrap();
    let second = auth.begin_login("p", "a", &cancel).await.unwrap();
    let a = auth
        .commit_login(&second, token("new"), true, &cancel)
        .await
        .unwrap();
    assert!(
        auth.commit_login(&first, token("old"), true, &cancel)
            .await
            .is_err()
    );
    let cancelled = auth.begin_login("p", "a", &cancel).await.unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(
        auth.commit_login(&cancelled, token("cancelled"), true, &cancellation)
            .await
            .is_err()
    );
    assert_eq!(
        auth.credentials_for("p", &a, None, &cancel)
            .await
            .unwrap()
            .access_token,
        "new"
    );
}
#[tokio::test]
async fn refresh_does_not_block_other_account_or_selection_reads() {
    let home = tempfile::tempdir().unwrap();
    let auth = AuthStore::new(home.path());
    let cancel = CancellationToken::new();
    let mut expired = token("a");
    expired.expires_at = Some(0);
    auth.login("p", "a", expired, &cancel).await.unwrap();
    let a = auth.selected_reference("p").unwrap();
    let operation = auth.begin_login("p", "b", &cancel).await.unwrap();
    let b = auth
        .commit_login(&operation, token("b"), false, &cancel)
        .await
        .unwrap();
    let (entered, wait) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let refreshing = {
        let auth = auth.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            auth.credentials_for_with_refresh("p", &a, &cancel, |_, _| async move {
                entered.send(()).unwrap();
                released.await.unwrap();
                Ok(token("rotated"))
            })
            .await
        })
    };
    wait.await.unwrap();
    tokio::time::timeout(std::time::Duration::from_millis(500), async {
        assert_eq!(
            auth.credentials_for("p", &b, None, &cancel)
                .await
                .unwrap()
                .access_token,
            "b"
        );
        assert_eq!(auth.accounts("p", &cancel).await.unwrap().len(), 2);
        auth.select_account("p", &b, &cancel).await.unwrap();
    })
    .await
    .unwrap();
    release.send(()).unwrap();
    refreshing.await.unwrap().unwrap();
    assert_eq!(auth.selected("p").unwrap().as_deref(), Some("b"));
}
#[tokio::test]
async fn legacy_selected_and_known_unselected_labels_import_without_new_generation() {
    let home = tempfile::tempdir().unwrap();
    let store = Store::new(home.path());
    let auth = AuthStore::new(home.path());
    let cancel = CancellationToken::new();
    for label in ["selected", "known"] {
        store
            .lock(&json!(("credentials", "p", label)).to_string(), &cancel)
            .await
            .unwrap()
            .write(&token(label))
            .unwrap();
    }
    store
        .lock(&json!(("selected", "p")).to_string(), &cancel)
        .await
        .unwrap()
        .write(&"selected")
        .unwrap();
    let accounts = auth.accounts("p", &cancel).await.unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].reference.identity(), "selected");
    assert_eq!(
        auth.enroll("p", "known", &cancel)
            .await
            .unwrap()
            .reference
            .identity(),
        "known"
    );
    assert_eq!(auth.accounts("p", &cancel).await.unwrap().len(), 2);
}

#[tokio::test]
async fn legacy_import_remains_available_while_credential_refresh_holds_its_lock() {
    let home = tempfile::tempdir().unwrap();
    let store = Store::new(home.path());
    let cancel = CancellationToken::new();
    let credential = store
        .lock(&json!(("credentials", "p", "legacy")).to_string(), &cancel)
        .await
        .unwrap();
    credential.write(&token("legacy")).unwrap();
    store
        .lock(&json!(("selected", "p")).to_string(), &cancel)
        .await
        .unwrap()
        .write(&"legacy")
        .unwrap();
    let auth = AuthStore::new(home.path());
    let accounts = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        auth.accounts("p", &cancel),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(accounts[0].reference.identity(), "legacy");
    drop(credential);
}
