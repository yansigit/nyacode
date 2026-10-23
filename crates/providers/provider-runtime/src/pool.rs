//! Private pool management, scoped health, priority selection, and conversation affinity.
//! Metadata locks never span credential access, network requests, or inference.
use codex_provider_api::*;
use codex_provider_auth::AuthStore;
use codex_provider_auth::CredentialCondition;
use codex_provider_auth::SavedAccount;
use codex_provider_auth::Store;
use codex_provider_auth::StoreLock;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use tokio_util::sync::CancellationToken;

fn error(message: &str) -> ProviderError {
    ProviderError::Checkpoint(message.into())
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolMember {
    pub label: String,
    pub enabled: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PendingEnrollment {
    account: AccountRef,
    select: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSettings {
    pub version: u32,
    pub revision: u64,
    pub preference: Option<bool>,
    pub activated: bool,
    pub imported_selected: bool,
    pub members: Vec<PoolMember>,
    #[serde(default)]
    pending_logins: BTreeMap<String, PendingEnrollment>,
    #[serde(default)]
    pending_selection: Option<AccountRef>,
}
impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            version: 1,
            revision: 0,
            preference: None,
            activated: false,
            imported_selected: false,
            members: vec![],
            pending_logins: BTreeMap::new(),
            pending_selection: None,
        }
    }
}
impl PoolSettings {
    pub fn enabled(&self) -> bool {
        self.preference.unwrap_or(self.activated)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PoolUpdate {
    Enable,
    Disable,
    Enroll { label: String },
    Remove { label: String },
    SetAccountEnabled { label: String, enabled: bool },
    Order { labels: Vec<String> },
    Prefer { label: String },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountStatus {
    pub account: SavedAccount,
    pub member: bool,
    pub enabled: bool,
    pub priority: Option<usize>,
    pub restrictions: Vec<Restriction>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolStatus {
    pub provider: String,
    pub endpoint: String,
    pub settings: PoolSettings,
    pub enabled: bool,
    pub configured_token: bool,
    pub selected: Option<String>,
    pub effective_account: Option<AccountRef>,
    pub accounts: Vec<AccountStatus>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restriction {
    pub failure: Failure,
    pub revision: u64,
    pub consecutive_failures: u32,
    pub next_attempt_at: Option<u64>,
}
#[derive(Serialize, Deserialize)]
struct Availability {
    version: u32,
    revision: u64,
    restrictions: BTreeMap<String, Restriction>,
    #[serde(default)]
    identities: BTreeMap<String, Vec<String>>,
}
impl Default for Availability {
    fn default() -> Self {
        Self {
            version: 1,
            revision: 0,
            restrictions: BTreeMap::new(),
            identities: BTreeMap::new(),
        }
    }
}
impl Availability {
    fn validate(self) -> Result<Self> {
        if self.version != 1 {
            return Err(error("unsupported account availability version"));
        }
        Ok(self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Affinity {
    pub version: u32,
    pub account: AccountRef,
}
#[derive(Clone)]
pub struct PoolStore {
    store: Store,
    auth: AuthStore,
    provider: String,
    endpoint: String,
}
/// Recovery probe locks live only for the real request, never a global queue.
pub struct AttemptPermit {
    pub account: SavedAccount,
    pub observation: u64,
    pub model: String,
    _probes: Vec<StoreLock>,
}
impl PoolStore {
    pub fn new(home: &Path, provider: &str, endpoint: &str) -> Self {
        Self {
            store: Store::new(home),
            auth: AuthStore::new(home),
            provider: provider.into(),
            endpoint: endpoint.into(),
        }
    }
    fn key(&self, kind: &str) -> String {
        serde_json::json!((kind, &self.provider, &self.endpoint)).to_string()
    }
    fn affinity_key(&self, thread: &str) -> String {
        serde_json::json!(("account-affinity", &self.provider, &self.endpoint, thread)).to_string()
    }
    /// Read the first configured account for lazy discovery without mutating selection.
    pub fn preferred_reference(&self) -> Result<Option<AccountRef>> {
        let Some(settings) = self.store.read::<PoolSettings>(&self.key("account-pool"))? else {
            return Ok(None);
        };
        if settings.version != 1 {
            return Err(error("unsupported provider pool version"));
        }
        if !settings.enabled() {
            return Ok(None);
        }
        for member in settings.members.into_iter().filter(|member| member.enabled) {
            if let Ok(account) = self.auth.account_reference(&self.provider, &member.label) {
                return Ok(Some(account));
            }
        }
        Ok(None)
    }
    pub async fn prepare_login(
        &self,
        account: AccountRef,
        select: bool,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let lock = self
            .store
            .lock(&self.key("account-pool"), cancellation)
            .await?;
        let mut settings = lock.read::<PoolSettings>()?.unwrap_or_default();
        if settings.version != 1 {
            return Err(error("unsupported provider pool version"));
        }
        if select {
            settings.pending_selection = None;
        }
        settings
            .pending_logins
            .insert(account.label.clone(), PendingEnrollment { account, select });
        lock.write(&settings)
    }
    async fn recover_selection(&self, cancellation: &CancellationToken) -> Result<()> {
        let Some(account) = self
            .store
            .read::<PoolSettings>(&self.key("account-pool"))?
            .and_then(|settings| settings.pending_selection)
        else {
            return Ok(());
        };
        let recovered = self
            .auth
            .select_account_with(&self.provider, &account, cancellation, || async {
                let lock = self
                    .store
                    .lock(&self.key("account-pool"), cancellation)
                    .await?;
                Ok(lock
                    .read::<PoolSettings>()?
                    .is_some_and(|settings| settings.pending_selection.as_ref() == Some(&account)))
            })
            .await;
        match recovered {
            Ok(()) | Err(ProviderError::Authentication(_)) => {}
            Err(error) => return Err(error),
        }
        let lock = self
            .store
            .lock(&self.key("account-pool"), cancellation)
            .await?;
        let mut settings = lock.read::<PoolSettings>()?.unwrap_or_default();
        if settings.pending_selection.as_ref() == Some(&account) {
            settings.pending_selection = None;
            lock.write(&settings)?;
        }
        Ok(())
    }
    pub async fn prefer(
        &self,
        revision: u64,
        label: String,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let account = self.auth.account_reference(&self.provider, &label)?;
        self.auth
            .select_account_with(&self.provider, &account, cancellation, || async {
                self.update_with_selection(
                    revision,
                    PoolUpdate::Prefer { label },
                    Some(account.clone()),
                    cancellation,
                )
                .await?;
                Ok(true)
            })
            .await
    }
    pub async fn settings(&self, cancellation: &CancellationToken) -> Result<PoolSettings> {
        self.recover_selection(cancellation).await?;
        let selected = self.auth.selected(&self.provider)?;
        // Import credentials before taking the pool lock.
        if let Some(label) = &selected {
            let current = self
                .store
                .read::<PoolSettings>(&self.key("account-pool"))?
                .unwrap_or_default();
            if !current.imported_selected
                && self.auth.account_reference(&self.provider, label).is_ok()
            {
                self.auth.accounts(&self.provider, cancellation).await?;
            }
        }
        let lock = self
            .store
            .lock(&self.key("account-pool"), cancellation)
            .await?;
        let mut settings = lock.read::<PoolSettings>()?.unwrap_or_default();
        if settings.version != 1 {
            return Err(error("unsupported provider pool version"));
        }
        let ready: Vec<_> = settings
            .pending_logins
            .values()
            .filter(|pending| {
                self.auth
                    .account_reference(&self.provider, &pending.account.label)
                    .is_ok_and(|current| current == pending.account)
            })
            .cloned()
            .collect();
        if !ready.is_empty() {
            for pending in ready {
                let label = pending.account.label;
                settings.pending_logins.remove(&label);
                if !settings.members.iter().any(|member| member.label == label) {
                    settings.members.push(PoolMember {
                        label: label.clone(),
                        enabled: true,
                    });
                }
                if pending.select
                    && let Some(position) = settings
                        .members
                        .iter()
                        .position(|member| member.label == label)
                {
                    let member = settings.members.remove(position);
                    settings.members.insert(0, member);
                }
            }
            if settings.members.len() >= 2 {
                settings.activated = true;
            }
            settings.revision += 1;
            lock.write(&settings)?;
        }
        if !settings.imported_selected {
            if let Some(label) = selected {
                settings.members.push(PoolMember {
                    label,
                    enabled: true,
                });
            }
            settings.imported_selected = true;
            settings.revision += 1;
            lock.write(&settings)?;
        }
        Ok(settings)
    }
    /// Login enrollment merges into the latest settings under one short lock.
    pub async fn enroll_login(
        &self,
        label: &str,
        select: bool,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.auth
            .enroll(&self.provider, label, cancellation)
            .await?;
        self.settings(cancellation).await?;
        let lock = self
            .store
            .lock(&self.key("account-pool"), cancellation)
            .await?;
        let mut settings = lock.read::<PoolSettings>()?.unwrap_or_default();
        if settings.version != 1 {
            return Err(error("unsupported provider pool version"));
        }
        if !settings.members.iter().any(|member| member.label == label) {
            settings.members.push(PoolMember {
                label: label.into(),
                enabled: true,
            });
        }
        if settings.members.len() >= 2 {
            settings.activated = true;
        }
        if select
            && let Some(index) = settings
                .members
                .iter()
                .position(|member| member.label == label)
        {
            let member = settings.members.remove(index);
            settings.members.insert(0, member);
        }
        settings.revision += 1;
        lock.write(&settings)
    }
    /// Used under a credential lock for generation-checked logout; no auth I/O.
    pub async fn touch(
        &self,
        expected_revision: u64,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let lock = self
            .store
            .lock(&self.key("account-pool"), cancellation)
            .await?;
        let mut settings = lock.read::<PoolSettings>()?.unwrap_or_default();
        if settings.version != 1 || settings.revision != expected_revision {
            return Err(error("provider pool changed; reload before updating"));
        }
        settings.revision += 1;
        lock.write(&settings)
    }
    pub async fn update(
        &self,
        expected_revision: u64,
        update: PoolUpdate,
        cancellation: &CancellationToken,
    ) -> Result<PoolSettings> {
        self.update_with_selection(expected_revision, update, None, cancellation)
            .await
    }
    async fn update_with_selection(
        &self,
        expected_revision: u64,
        update: PoolUpdate,
        selection: Option<AccountRef>,
        cancellation: &CancellationToken,
    ) -> Result<PoolSettings> {
        if let PoolUpdate::Enroll { label } = &update {
            self.auth
                .enroll(&self.provider, label, cancellation)
                .await?;
        }
        let lock = self
            .store
            .lock(&self.key("account-pool"), cancellation)
            .await?;
        let mut settings = lock.read::<PoolSettings>()?.unwrap_or_default();
        if settings.version != 1 {
            return Err(error("unsupported provider pool version"));
        }
        if settings.revision != expected_revision {
            return Err(error("provider pool changed; reload before updating"));
        }
        match update {
            PoolUpdate::Enable => settings.preference = Some(true),
            PoolUpdate::Disable => settings.preference = Some(false),
            PoolUpdate::Enroll { label } => {
                if !settings.members.iter().any(|member| member.label == label) {
                    settings.members.push(PoolMember {
                        label,
                        enabled: true,
                    });
                }
                if settings.members.len() >= 2 {
                    settings.activated = true;
                }
            }
            PoolUpdate::Remove { label } => settings.members.retain(|member| member.label != label),
            PoolUpdate::SetAccountEnabled { label, enabled } => {
                settings
                    .members
                    .iter_mut()
                    .find(|member| member.label == label)
                    .ok_or_else(|| error("account is not a pool member"))?
                    .enabled = enabled;
            }
            PoolUpdate::Prefer { label } => {
                let position = settings
                    .members
                    .iter()
                    .position(|member| member.label == label)
                    .ok_or_else(|| error("account is not a pool member"))?;
                let member = settings.members.remove(position);
                settings.members.insert(0, member);
            }
            PoolUpdate::Order { labels } => {
                let distinct: BTreeSet<_> = labels.iter().collect();
                if distinct.len() != labels.len()
                    || labels.len() != settings.members.len()
                    || settings
                        .members
                        .iter()
                        .any(|member| !distinct.contains(&member.label))
                {
                    return Err(error(
                        "priority order must contain every pool member exactly once",
                    ));
                }
                let mut members: BTreeMap<_, _> = settings
                    .members
                    .into_iter()
                    .map(|member| (member.label.clone(), member))
                    .collect();
                settings.members = labels
                    .into_iter()
                    .filter_map(|label| members.remove(&label))
                    .collect();
            }
        }
        if let Some(selection) = selection {
            settings.pending_selection = Some(selection);
        }
        settings.revision += 1;
        lock.write(&settings)?;
        Ok(settings)
    }
    pub fn affinity(&self, thread: &str) -> Result<Option<AccountRef>> {
        let affinity = self.store.read::<Affinity>(&self.affinity_key(thread))?;
        if affinity
            .as_ref()
            .is_some_and(|affinity| affinity.version != 1)
        {
            return Err(error("unsupported account affinity version"));
        }
        Ok(affinity.map(|affinity| affinity.account))
    }
    pub async fn accept(
        &self,
        thread: &str,
        account: &AccountRef,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.store
            .lock(&self.affinity_key(thread), cancellation)
            .await?
            .write(&Affinity {
                version: 1,
                account: account.clone(),
            })
    }
    pub async fn status(
        &self,
        thread: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<PoolStatus> {
        let settings = self.settings(cancellation).await?;
        let accounts = self.auth.accounts(&self.provider, cancellation).await?;
        let availability = self
            .store
            .read::<Availability>(&self.key("account-availability"))?
            .unwrap_or_default()
            .validate()?;
        let accounts = accounts
            .into_iter()
            .map(|mut account| {
                if let Some(identities) = availability.identities.get(&account.reference.identity())
                {
                    account.quota_identities.extend(identities.clone());
                }
                let priority = settings
                    .members
                    .iter()
                    .position(|member| member.label == account.reference.label);
                let enabled = priority.is_some_and(|i| settings.members[i].enabled);
                let restrictions = availability
                    .restrictions
                    .iter()
                    .filter(|(key, _)| restriction_matches(key, &account, None))
                    .map(|(_, value)| value.clone())
                    .collect();
                AccountStatus {
                    account,
                    member: priority.is_some(),
                    enabled,
                    priority,
                    restrictions,
                }
            })
            .collect();
        Ok(PoolStatus {
            provider: self.provider.clone(),
            endpoint: self.endpoint.clone(),
            enabled: settings.enabled(),
            configured_token: false,
            settings,
            selected: self.auth.selected(&self.provider)?,
            effective_account: thread
                .map(|thread| self.affinity(thread))
                .transpose()?
                .flatten(),
            accounts,
        })
    }
    /// Candidates are recomputed at each boundary so logout and manager edits
    /// affect future requests without cancelling another process's in-flight work.
    pub async fn candidates(
        &self,
        thread: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<SavedAccount>> {
        let status = self.status(Some(thread), cancellation).await?;
        if !status.enabled {
            return Ok(status
                .accounts
                .into_iter()
                .filter(|entry| {
                    entry.member
                        && entry.enabled
                        && Some(&entry.account.reference.label) == status.selected.as_ref()
                })
                .map(|entry| entry.account)
                .collect());
        }
        let mut entries: Vec<_> = status
            .accounts
            .into_iter()
            .filter(|entry| entry.member && entry.enabled)
            .collect();
        entries.sort_by_key(|entry| {
            (
                Some(&entry.account.reference) != status.effective_account.as_ref(),
                entry.priority,
            )
        });
        Ok(entries.into_iter().map(|entry| entry.account).collect())
    }
    pub async fn reserve(
        &self,
        mut account: SavedAccount,
        model: &str,
        now: u64,
        cancellation: &CancellationToken,
    ) -> Result<Option<AttemptPermit>> {
        if account.condition != CredentialCondition::Ready {
            return Ok(None);
        }
        let lock = self
            .store
            .lock(&self.key("account-availability"), cancellation)
            .await?;
        let mut availability = lock
            .read::<Availability>()?
            .unwrap_or_default()
            .validate()?;
        if let Some(identities) = availability.identities.get(&account.reference.identity()) {
            account.quota_identities.extend(identities.clone());
        }
        let mut probes = Vec::new();
        for (key, restriction) in &availability.restrictions {
            if !restriction_matches(key, &account, Some(model)) {
                continue;
            }
            if restriction
                .next_attempt_at
                .is_none_or(|deadline| deadline > now)
            {
                return Ok(None);
            }
            let Some(probe) = self
                .store
                .try_lock(&self.key(&format!("account-probe:{key}")))?
            else {
                return Ok(None);
            };
            probes.push(probe);
        }
        availability.revision += 1;
        lock.write(&availability)?;
        Ok(Some(AttemptPermit {
            account,
            observation: availability.revision,
            model: model.into(),
            _probes: probes,
        }))
    }
    pub async fn failed(
        &self,
        permit: &AttemptPermit,
        failure: &Failure,
        now: u64,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if !failure.another_account_may_help || matches!(failure.scope, FailureScope::Provider) {
            return Ok(());
        }
        if !matches!(
            failure.category,
            FailureCategory::TemporaryLimit
                | FailureCategory::QuotaExhausted
                | FailureCategory::Authentication
                | FailureCategory::LoginRequired
                | FailureCategory::ModelEntitlement
        ) {
            return Ok(());
        }
        let lock = self
            .store
            .lock(&self.key("account-availability"), cancellation)
            .await?;
        let mut availability = lock
            .read::<Availability>()?
            .unwrap_or_default()
            .validate()?;
        if let FailureScope::QuotaGroup(id) = &failure.scope {
            let identities = availability
                .identities
                .entry(permit.account.reference.identity())
                .or_default();
            if !identities.contains(id) {
                identities.push(id.clone());
            }
        }
        // A failure is observed now, after any requests already in flight took
        // their snapshots. Those requests cannot clear this new restriction.
        availability.revision += 1;
        let observed_revision = availability.revision;
        for key in restriction_keys(&permit.account, &failure.scope) {
            let previous = availability.restrictions.get(&key);
            let count = previous.map_or(1, |restriction| {
                restriction.consecutive_failures.saturating_add(1)
            });
            let fallback = match failure.category {
                FailureCategory::TemporaryLimit => Some(
                    60u64
                        .saturating_mul(1 << count.saturating_sub(1).min(10))
                        .min(900),
                ),
                FailureCategory::QuotaExhausted => Some(
                    900u64
                        .saturating_mul(1 << count.saturating_sub(1).min(10))
                        .min(21600),
                ),
                _ => None,
            };
            let next_attempt_at = failure
                .retry_at
                .into_iter()
                .chain(failure.reset_at)
                .max()
                .or_else(|| fallback.map(|seconds| now.saturating_add(seconds)));
            let mut observed = failure.clone();
            let mut next_attempt_at = if matches!(
                failure.category,
                FailureCategory::TemporaryLimit | FailureCategory::QuotaExhausted
            ) {
                next_attempt_at
            } else {
                None
            };
            if let Some(previous) = previous {
                // Concurrent failures may add restrictions, never shorten a
                // deadline or repair credentials without a successful probe.
                if previous.next_attempt_at.is_none() {
                    observed = previous.failure.clone();
                    next_attempt_at = None;
                } else if let Some(at) = next_attempt_at {
                    next_attempt_at = Some(at.max(previous.next_attempt_at.unwrap_or(at)));
                    if previous.failure.category == FailureCategory::QuotaExhausted
                        && observed.category == FailureCategory::TemporaryLimit
                    {
                        observed = previous.failure.clone();
                    }
                }
                observed.reset_at = failure
                    .reset_at
                    .into_iter()
                    .chain(previous.failure.reset_at)
                    .max();
            }
            availability.restrictions.insert(
                key,
                Restriction {
                    failure: observed,
                    revision: observed_revision,
                    consecutive_failures: count,
                    next_attempt_at,
                },
            );
        }
        lock.write(&availability)
    }
    pub async fn succeeded(
        &self,
        permit: &AttemptPermit,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let lock = self
            .store
            .lock(&self.key("account-availability"), cancellation)
            .await?;
        let mut availability = lock
            .read::<Availability>()?
            .unwrap_or_default()
            .validate()?;
        availability.restrictions.retain(|key, restriction| {
            restriction.revision > permit.observation
                || !restriction_matches(key, &permit.account, Some(&permit.model))
        });
        lock.write(&availability)
    }
    pub async fn exhausted(
        &self,
        model: &str,
        cancellation: &CancellationToken,
    ) -> Result<ProviderError> {
        let status = self.status(None, cancellation).await?;
        let earliest = status
            .accounts
            .iter()
            .filter(|entry| entry.member && entry.enabled)
            .flat_map(|entry| &entry.restrictions)
            .filter(|r| !matches!(&r.failure.scope, FailureScope::Model(id) if id != model))
            .filter_map(|restriction| restriction.next_attempt_at)
            .min();
        let mut reasons = Vec::new();
        if let Some(at) = earliest {
            reasons.push(format!(
                "earliest recorded retry deadline {at} (not a promised quota reset)"
            ));
        }
        for entry in status.accounts {
            let label = &entry.account.reference.label;
            let reason = if !entry.member {
                "removed from pool; enroll to use".into()
            } else if !entry.enabled {
                "disabled; enable to use".into()
            } else if entry.account.condition != CredentialCondition::Ready {
                format!("{:?}; log in again", entry.account.condition)
            } else {
                entry
                    .restrictions
                    .iter()
                    .filter(|r| !matches!(&r.failure.scope, FailureScope::Model(id) if id != model))
                    .map(|r| {
                        format!(
                            "{:?}{}{}",
                            r.failure.code,
                            r.failure
                                .reset_at
                                .map(|reset| format!("; known reset {reset}"))
                                .unwrap_or_default(),
                            r.next_attempt_at
                                .map(|at| format!("; next eligible attempt {at}"))
                                .unwrap_or_else(|| "; repair credentials or entitlement".into())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            reasons.push(format!(
                "{label}: {}",
                if reason.is_empty() {
                    "already attempted or recovery probe in progress"
                } else {
                    &reason
                }
            ));
        }
        Ok(ProviderError::Authentication(format!(
            "no eligible provider accounts for {model}; {}",
            reasons.join("; ")
        )))
    }
}
fn restriction_keys(account: &SavedAccount, scope: &FailureScope) -> Vec<String> {
    let identity = account.reference.identity();
    let groups = || {
        account
            .quota_identities
            .iter()
            .map(|id| serde_json::json!(("quota", id, "")).to_string())
            .collect()
    };
    match scope {
        FailureScope::Credential => {
            vec![serde_json::json!(("credential", identity, "")).to_string()]
        }
        FailureScope::Account => groups(),
        FailureScope::QuotaGroup(id) => vec![
            serde_json::json!(("quota", id, "")).to_string(),
            serde_json::json!(("credential", identity, "")).to_string(),
        ],
        FailureScope::Model(model) => account
            .quota_identities
            .iter()
            .map(|id| serde_json::json!(("quota", id, model)).to_string())
            .collect(),
        FailureScope::Provider => vec![],
    }
}
fn restriction_matches(key: &str, account: &SavedAccount, model: Option<&str>) -> bool {
    let Ok((kind, id, restricted_model)) = serde_json::from_str::<(String, String, String)>(key)
    else {
        return false;
    };
    (restricted_model.is_empty() || model.is_none_or(|model| model == restricted_model))
        && ((kind == "credential" && id == account.reference.identity())
            || (kind == "quota" && account.quota_identities.contains(&id)))
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use codex_provider_auth::TokenRecord;
    use serde_json::json;

    #[tokio::test]
    async fn preferred_selection_recovers_commit_and_never_selects_replacement_generation() {
        let home = tempfile::tempdir().unwrap();
        let pool = PoolStore::new(home.path(), "fifth", "endpoint");
        let cancellation = CancellationToken::new();
        for label in ["a", "b"] {
            pool.auth
                .login(
                    "fifth",
                    label,
                    TokenRecord {
                        access_token: label.into(),
                        refresh_token: None,
                        expires_at: None,
                        metadata: json!({}),
                        revoked: false,
                    },
                    &cancellation,
                )
                .await
                .unwrap();
            let settings = pool.settings(&cancellation).await.unwrap();
            pool.update(
                settings.revision,
                PoolUpdate::Enroll {
                    label: label.into(),
                },
                &cancellation,
            )
            .await
            .unwrap();
        }
        let a = pool.auth.account_reference("fifth", "a").unwrap();
        let settings = pool.settings(&cancellation).await.unwrap();
        // Crash immediately after the pool intent and before selection write.
        pool.update_with_selection(
            settings.revision,
            PoolUpdate::Prefer { label: "a".into() },
            Some(a.clone()),
            &cancellation,
        )
        .await
        .unwrap();
        assert_eq!(pool.auth.selected("fifth").unwrap().as_deref(), Some("b"));
        let recovered = pool.settings(&cancellation).await.unwrap();
        assert!(recovered.pending_selection.is_none());
        assert_eq!(pool.auth.selected("fifth").unwrap().as_deref(), Some("a"));
        pool.update_with_selection(
            recovered.revision,
            PoolUpdate::Prefer { label: "a".into() },
            Some(a),
            &cancellation,
        )
        .await
        .unwrap();
        let b = pool.auth.account_reference("fifth", "b").unwrap();
        pool.auth
            .select_account("fifth", &b, &cancellation)
            .await
            .unwrap();
        pool.auth
            .logout_account("fifth", "a", &cancellation)
            .await
            .unwrap();
        let login = pool
            .auth
            .begin_login("fifth", "a", &cancellation)
            .await
            .unwrap();
        pool.auth
            .commit_login(
                &login,
                TokenRecord {
                    access_token: "replacement".into(),
                    refresh_token: None,
                    expires_at: None,
                    metadata: json!({}),
                    revoked: false,
                },
                false,
                &cancellation,
            )
            .await
            .unwrap();
        assert!(
            pool.settings(&cancellation)
                .await
                .unwrap()
                .pending_selection
                .is_none()
        );
        assert_eq!(pool.auth.selected("fifth").unwrap().as_deref(), Some("b"));
    }
}
