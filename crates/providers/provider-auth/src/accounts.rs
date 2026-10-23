//! Additive account index and recoverable mutations. Credentials keep their old keys.
//! Lock order is credential -> index/selection. Never acquire a credential while
//! holding an index, pool, or selection lock. No index lock spans network I/O.
use crate::AuthStore;
use crate::StoreLock;
use crate::TokenRecord;
use crate::digest;
use crate::oauth::auth_error;
use crate::oauth::credential_key;
use crate::oauth::selected_key;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_provider_api::AccountRef;
use codex_provider_api::ProviderError;
use codex_provider_api::Result;
use rand::Rng;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

fn nonce() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
fn index_key(provider: &str) -> String {
    serde_json::json!(("account-index", provider)).to_string()
}
fn control_key(provider: &str, label: &str) -> String {
    serde_json::json!(("account-operation", provider, label)).to_string()
}
fn reference(label: &str, record: &TokenRecord) -> AccountRef {
    AccountRef {
        label: label.into(),
        generation: record.metadata["codex_account_generation"]
            .as_str()
            .map(str::to_owned),
    }
}
fn validate_label(label: &str) -> Result<()> {
    if label.trim().is_empty() || label.len() > 128 || label.chars().any(char::is_control) {
        return Err(auth_error(
            "account label must contain 1–128 bytes without control characters",
        ));
    }
    Ok(())
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialCondition {
    Ready,
    Missing,
    Revoked,
    RefreshInterrupted,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedAccount {
    pub reference: AccountRef,
    pub condition: CredentialCondition,
    pub expires_at: Option<u64>,
    /// Hashed identities only. Identical keys always share a quota group.
    pub quota_identities: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct Index {
    version: u32,
    revision: u64,
    accounts: BTreeMap<String, Option<AccountRef>>,
}
impl Default for Index {
    fn default() -> Self {
        Self {
            version: 1,
            revision: 0,
            accounts: BTreeMap::new(),
        }
    }
}
#[derive(Default, Serialize, Deserialize)]
struct Control {
    operation: Option<String>,
    mutation: Option<Mutation>,
}
#[derive(Serialize, Deserialize)]
enum Mutation {
    Login { generation: String, select: bool },
    Logout,
}
/// A callback must own this operation to commit. No credential material.
#[derive(Debug)]
pub struct LoginOperation {
    pub provider: String,
    pub account: String,
    operation: String,
}
impl AuthStore {
    /// Invalidate only the rejected token. A concurrent refresh that already
    /// rotated it must not be expired again by a stale HTTP response.
    pub async fn expire_rejected(
        &self,
        provider: &str,
        account: &AccountRef,
        token: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool> {
        let lock = self
            .store
            .lock(&credential_key(provider, &account.label), cancellation)
            .await?;
        self.recover_account(provider, &account.label, &lock, cancellation)
            .await?;
        let mut record = lock
            .read::<TokenRecord>()?
            .ok_or_else(|| auth_error("account was logged out"))?;
        if reference(&account.label, &record) != *account {
            return Err(auth_error("account was replaced"));
        }
        if record.refresh_token.is_none() {
            return Ok(false);
        }
        if record.access_token == token {
            record.expires_at = Some(0);
            lock.write(&record)?;
        }
        Ok(true)
    }
    async fn index_account(
        &self,
        provider: &str,
        label: &str,
        account: Option<AccountRef>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let lock = self.store.lock(&index_key(provider), cancellation).await?;
        let mut index = lock.read::<Index>()?.unwrap_or_default();
        if index.version != 1 {
            return Err(auth_error("unsupported account index version"));
        }
        if index.accounts.get(label) != Some(&account) {
            index.accounts.insert(label.into(), account);
            index.revision += 1;
            lock.write(&index)?;
        }
        Ok(())
    }
    /// A commit intent is written before credentials change. Recovery checks the
    /// actual credential generation and never guesses whether a token was saved.
    pub(crate) async fn recover_account(
        &self,
        provider: &str,
        label: &str,
        credential: &StoreLock,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let control = self
            .store
            .lock(&control_key(provider, label), cancellation)
            .await?;
        let mut state = control.read::<Control>()?.unwrap_or_default();
        if let Some(mutation) = state.mutation.take() {
            match mutation {
                Mutation::Logout => {
                    credential.delete()?;
                    let selected = self
                        .store
                        .lock(&selected_key(provider), cancellation)
                        .await?;
                    if selected.read::<String>()?.as_deref() == Some(label) {
                        selected.delete()?;
                    }
                }
                Mutation::Login { generation, select } => {
                    if select
                        && credential.read::<TokenRecord>()?.is_some_and(|record| {
                            record.metadata["codex_account_generation"] == generation
                        })
                    {
                        self.store
                            .lock(&selected_key(provider), cancellation)
                            .await?
                            .write(&label)?;
                    }
                }
            }
            state.operation = None;
            let saved = credential
                .read::<TokenRecord>()?
                .map(|record| reference(label, &record));
            self.index_account(provider, label, saved, cancellation)
                .await?;
            control.write(&state)?;
        }
        Ok(())
    }
    pub fn selected_reference(&self, provider: &str) -> Result<AccountRef> {
        let label = self
            .selected(provider)?
            .ok_or_else(|| auth_error("log in to the selected provider"))?;
        self.account_reference(provider, &label)
    }
    pub fn account_reference(&self, provider: &str, label: &str) -> Result<AccountRef> {
        let record = self
            .store
            .read::<TokenRecord>(&credential_key(provider, label))?
            .ok_or_else(|| auth_error("provider credentials missing"))?;
        Ok(reference(label, &record))
    }
    pub async fn enroll(
        &self,
        provider: &str,
        label: &str,
        cancellation: &CancellationToken,
    ) -> Result<SavedAccount> {
        validate_label(label)?;
        let credential = self
            .store
            .lock(&credential_key(provider, label), cancellation)
            .await?;
        self.recover_account(provider, label, &credential, cancellation)
            .await?;
        let record = credential.read::<TokenRecord>()?.ok_or_else(|| {
            auth_error("no saved credentials for this account label; log in first")
        })?;
        let account = saved_account(label, record);
        self.index_account(
            provider,
            label,
            Some(account.reference.clone()),
            cancellation,
        )
        .await?;
        Ok(account)
    }
    pub async fn accounts(
        &self,
        provider: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<SavedAccount>> {
        let current = self
            .store
            .read::<Index>(&index_key(provider))?
            .unwrap_or_default();
        if let Some(label) = self.selected(provider)?
            && !current.accounts.contains_key(&label)
        {
            // Import only the known selected label; hashed filenames are opaque.
            let lock = self.store.lock(&index_key(provider), cancellation).await?;
            let mut index = lock.read::<Index>()?.unwrap_or_default();
            if index.version != 1 {
                return Err(auth_error("unsupported account index version"));
            }
            if !index.accounts.contains_key(&label)
                && let Some(record) = self
                    .store
                    .read::<TokenRecord>(&credential_key(provider, &label))?
            {
                index
                    .accounts
                    .insert(label.clone(), Some(reference(&label, &record)));
                index.revision += 1;
                lock.write(&index)?;
            }
        }
        let index = self
            .store
            .read::<Index>(&index_key(provider))?
            .unwrap_or_default();
        if index.version != 1 {
            return Err(auth_error("unsupported account index version"));
        }
        let mut accounts = Vec::new();
        for (label, previous) in index.accounts {
            // Read atomic snapshots; listing B does not wait for A's refresh.
            let record = self
                .store
                .read::<TokenRecord>(&credential_key(provider, &label))?;
            accounts.push(match record {
                Some(record) => saved_account(&label, record),
                None => SavedAccount {
                    reference: previous.unwrap_or(AccountRef {
                        label,
                        generation: None,
                    }),
                    condition: CredentialCondition::Missing,
                    expires_at: None,
                    quota_identities: vec![],
                },
            });
        }
        Ok(accounts)
    }
    pub async fn begin_login(
        &self,
        provider: &str,
        label: &str,
        cancellation: &CancellationToken,
    ) -> Result<LoginOperation> {
        validate_label(label)?;
        if provider.is_empty() {
            return Err(auth_error("empty provider"));
        }
        let credential = self
            .store
            .lock(&credential_key(provider, label), cancellation)
            .await?;
        self.recover_account(provider, label, &credential, cancellation)
            .await?;
        let operation = nonce();
        let lock = self
            .store
            .lock(&control_key(provider, label), cancellation)
            .await?;
        lock.write(&Control {
            operation: Some(operation.clone()),
            mutation: None,
        })?;
        Ok(LoginOperation {
            provider: provider.into(),
            account: label.into(),
            operation,
        })
    }
    pub async fn commit_login(
        &self,
        operation: &LoginOperation,
        record: TokenRecord,
        select: bool,
        cancellation: &CancellationToken,
    ) -> Result<AccountRef> {
        self.commit_login_with(operation, record, select, cancellation, |_| async {
            Ok(())
        })
        .await
    }
    /// The preparation hook runs under this account's lock after ownership was
    /// checked, before credentials commit. It must never acquire credential locks.
    pub async fn commit_login_with<F, Fut>(
        &self,
        operation: &LoginOperation,
        mut record: TokenRecord,
        select: bool,
        cancellation: &CancellationToken,
        prepare: F,
    ) -> Result<AccountRef>
    where
        F: FnOnce(AccountRef) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        if cancellation.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if record.access_token.trim().is_empty() {
            return Err(auth_error("empty provider token"));
        }
        let provider = &operation.provider;
        let label = &operation.account;
        let credential = self
            .store
            .lock(&credential_key(provider, label), cancellation)
            .await?;
        self.recover_account(provider, label, &credential, cancellation)
            .await?;
        let control = self
            .store
            .lock(&control_key(provider, label), cancellation)
            .await?;
        let state = control.read::<Control>()?.unwrap_or_default();
        if state.operation.as_ref() != Some(&operation.operation) {
            return Err(auth_error("login was cancelled, replaced, or logged out"));
        }
        let generation = nonce();
        if !record.metadata.is_object() {
            record.metadata = serde_json::json!({});
        }
        record.metadata["codex_account_generation"] = Value::String(generation.clone());
        record.metadata["codex_refresh_pending"] = Value::Bool(false);
        let account = reference(label, &record);
        // Register the label first so interrupted commits remain recoverable.
        self.index_account(
            provider,
            label,
            credential
                .read::<TokenRecord>()?
                .map(|r| reference(label, &r)),
            cancellation,
        )
        .await?;
        if cancellation.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        prepare(account.clone()).await?;
        control.write(&Control {
            operation: None,
            mutation: Some(Mutation::Login { generation, select }),
        })?;
        credential.write(&record)?;
        // Once credentials commit, complete the transaction despite caller cancellation.
        let durable = CancellationToken::new();
        self.index_account(provider, label, Some(account.clone()), &durable)
            .await?;
        if select {
            self.store
                .lock(&selected_key(provider), &durable)
                .await?
                .write(label)?;
        }
        control.write(&Control::default())?;
        Ok(account)
    }
    pub async fn logout_account(
        &self,
        provider: &str,
        label: &str,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.logout_account_checked(provider, label, cancellation, || async { Ok(()) })
            .await
    }
    pub async fn logout_account_checked<F, Fut>(
        &self,
        provider: &str,
        label: &str,
        cancellation: &CancellationToken,
        check: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        validate_label(label)?;
        let expected = self
            .store
            .read::<TokenRecord>(&credential_key(provider, label))?
            .map(|record| reference(label, &record));
        let credential = self
            .store
            .lock(&credential_key(provider, label), cancellation)
            .await?;
        self.recover_account(provider, label, &credential, cancellation)
            .await?;
        if credential
            .read::<TokenRecord>()?
            .map(|record| reference(label, &record))
            != expected
        {
            return Err(auth_error("account changed; reload before logging out"));
        }
        check().await?;
        let control = self
            .store
            .lock(&control_key(provider, label), cancellation)
            .await?;
        control.write(&Control {
            operation: None,
            mutation: Some(Mutation::Logout),
        })?;
        drop(control);
        self.recover_account(provider, label, &credential, &CancellationToken::new())
            .await
    }
    pub async fn select_account(
        &self,
        provider: &str,
        account: &AccountRef,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.select_account_with(provider, account, cancellation, || async { Ok(true) })
            .await
    }
    /// The callback may commit or verify an additive selection intent. Both the
    /// generation and selection lock are held, so a stale recovery cannot undo
    /// a newer explicit selection. It must not acquire another credential lock.
    pub async fn select_account_with<F, Fut>(
        &self,
        provider: &str,
        account: &AccountRef,
        cancellation: &CancellationToken,
        prepare: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<bool>>,
    {
        let credential = self
            .store
            .lock(&credential_key(provider, &account.label), cancellation)
            .await?;
        self.recover_account(provider, &account.label, &credential, cancellation)
            .await?;
        if self.account_reference(provider, &account.label)? != *account {
            return Err(auth_error("account generation changed"));
        }
        let selected = self
            .store
            .lock(&selected_key(provider), cancellation)
            .await?;
        if prepare().await? {
            selected.write(&account.label)?;
        }
        Ok(())
    }
}
fn saved_account(label: &str, record: TokenRecord) -> SavedAccount {
    let condition = if record.revoked {
        CredentialCondition::Revoked
    } else if record.metadata["codex_refresh_pending"] == true {
        CredentialCondition::RefreshInterrupted
    } else {
        CredentialCondition::Ready
    };
    let mut quota_identities = vec![
        format!("key:{}", digest(record.access_token.as_bytes())),
        format!(
            "generation:{}",
            digest(reference(label, &record).identity().as_bytes())
        ),
    ];
    for field in [
        "account_id",
        "organization_id",
        "workspace_id",
        "project_id",
    ] {
        if let Some(id) = record.metadata[field]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 512)
        {
            quota_identities.push(format!("{field}:{}", digest(id.as_bytes())));
        }
    }
    SavedAccount {
        reference: reference(label, &record),
        condition,
        expires_at: record.expires_at,
        quota_identities,
    }
}
