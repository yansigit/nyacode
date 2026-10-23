//! Bounded account attempts under the existing thread execution lease.
use crate::*;
use codex_provider_api::*;
use codex_provider_auth::Store;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn login_required() -> ProviderError {
    ProviderError::Classified(Failure {
        category: FailureCategory::LoginRequired,
        code: FailureCode::CredentialRevoked,
        scope: FailureScope::Credential,
        reset_at: None,
        retry_at: None,
        outcome: RequestOutcome::Rejected,
        another_account_may_help: true,
    })
}

pub trait AccountCredentials: Send + Sync {
    fn configured_token(&self) -> bool;
    fn namespace_account(&self, account: &AccountRef) -> String;
    fn credentials_for<'a>(
        &'a self,
        account: Option<&'a AccountRef>,
        cancellation: CancellationToken,
    ) -> ProviderFuture<'a, Credentials>;
    fn refresh_rejected<'a>(
        &'a self,
        _account: &'a AccountRef,
        _rejected_token: &'a str,
        _cancellation: CancellationToken,
    ) -> ProviderFuture<'a, Option<Credentials>> {
        Box::pin(async { Ok(None) })
    }
    fn validate_account(&self, account: &AccountRef) -> Result<()>;
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AttemptState {
    Prepared,
    Dispatched,
    Rejected,
    Accepted,
    Completed,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Handoff {
    version: u32,
    attempt: u64,
    history: HistoryAcknowledgement,
    namespace: Namespace,
    prior_journals: Vec<Namespace>,
    state: AttemptState,
}
#[derive(Clone)]
pub struct HandoffJournal {
    store: Store,
    key: String,
    fence: String,
}
impl HandoffJournal {
    fn new(home: &Path, provider: &str, endpoint: &str, thread: &str, purpose: &str) -> Self {
        Self {
            store: Store::new(home),
            fence: serde_json::json!(("account-handoff-inflight", thread, purpose)).to_string(),
            key: serde_json::json!(("account-handoff", provider, endpoint, thread, purpose))
                .to_string(),
        }
    }
    fn read(&self) -> Result<Option<Handoff>> {
        let record = self.store.read::<Handoff>(&self.key)?;
        if record.as_ref().is_some_and(|record| record.version != 1) {
            return Err(ProviderError::Checkpoint(
                "unsupported account handoff journal version".into(),
            ));
        }
        Ok(record)
    }
    async fn state(&self, state: AttemptState, cancellation: &CancellationToken) -> Result<()> {
        if state == AttemptState::Dispatched {
            self.store
                .lock(&self.fence, cancellation)
                .await?
                .write(&true)?;
        }
        let lock = self.store.lock(&self.key, cancellation).await?;
        if let Some(mut record) = lock.read::<Handoff>()? {
            record.state = state.clone();
            lock.write(&record)?;
        }
        if matches!(state, AttemptState::Rejected | AttemptState::Completed) {
            self.store
                .lock(&self.fence, cancellation)
                .await?
                .write(&false)?;
        }
        Ok(())
    }
    pub async fn complete(&self, cancellation: &CancellationToken) -> Result<()> {
        self.state(AttemptState::Completed, cancellation).await
    }
}
pub struct StartedTurn {
    pub request: Request,
    pub session: Box<dyn ProviderSession>,
    pub store: SessionStore,
    pub execution: Arc<ExecutionLease>,
    pub journal: Option<HandoffJournal>,
    pub switched: Option<(String, String)>,
}
fn export_handoff(
    provider: &dyn Provider,
    request: &Request,
    boundary: ContinuationBoundary,
) -> Result<Request> {
    provider.handoff_request(request, boundary).map_err(|error| match error {
        ProviderError::Unsupported(reason) => ProviderError::Unsupported(format!("{reason}; automatic continuation stopped. Inspect completed work and explicitly branch the thread to recover")),
        error => error,
    })
}

fn validate_handoff(original: &Request, exported: &Request) -> Result<()> {
    let mut metadata = original.metadata.clone();
    if let Some(metadata) = metadata.as_object_mut() {
        metadata.remove("host_checkpoint_prefix");
    }
    if metadata != exported.metadata
        || original.namespace.provider != exported.namespace.provider
        || original.namespace.model != exported.namespace.model
        || original.namespace.thread != exported.namespace.thread
        || original.system != exported.system
        || original.tools != exported.tools
        || original.parallel_tool_calls != exported.parallel_tool_calls
        || original.reasoning_effort != exported.reasoning_effort
        || original.max_output_tokens != exported.max_output_tokens
        || original.output_schema != exported.output_schema
    {
        return Err(ProviderError::Unsupported(
            "account handoff must preserve provider, model, tools, and request settings".into(),
        ));
    }
    if exported.checkpoint.is_some() {
        return Err(ProviderError::Checkpoint(
            "account handoff cannot relabel a checkpoint".into(),
        ));
    }
    Ok(())
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
#[allow(clippy::too_many_arguments)]
pub async fn start_turn(
    home: &Path,
    endpoint: &str,
    purpose: &str,
    checkpoint_scope: &str,
    mut request: Request,
    provider: Arc<dyn Provider>,
    credentials: Arc<dyn AccountCredentials>,
    cancellation: CancellationToken,
) -> Result<StartedTurn> {
    // Execution key depends only on thread/purpose, never the account or model.
    let execution = Arc::new(
        SessionStore::new(home, request.namespace.clone())
            .begin_execution(purpose, &cancellation)
            .await?,
    );
    let journal = HandoffJournal::new(
        home,
        &request.namespace.provider,
        endpoint,
        &request.namespace.thread,
        purpose,
    );
    let previous = journal.read()?;
    if previous.as_ref().is_some_and(|record| {
        matches!(
            record.state,
            AttemptState::Dispatched | AttemptState::Accepted
        )
    }) {
        return Err(ProviderError::Checkpoint("account attempt was interrupted after dispatch; outcome is uncertain. Inspect its effects and explicitly branch the thread before continuing".into()));
    }
    if credentials.configured_token() {
        let auth = credentials
            .credentials_for(None, cancellation.clone())
            .await?;
        request.namespace.account = auth.account_id.clone();
        let store = SessionStore::new(home, request.namespace.clone())
            .with_checkpoint_scope(checkpoint_scope.into());
        request.checkpoint = store.load()?;
        validate_lineage(&mut request, provider.recovery_policy())?;
        let session = provider
            .start(
                request.clone(),
                Context {
                    credentials: auth,
                    cancellation: cancellation.child_token(),
                },
            )
            .await?;
        return Ok(StartedTurn {
            request,
            session,
            store,
            execution,
            journal: None,
            switched: None,
        });
    }
    let pool = PoolStore::new(home, &request.namespace.provider, endpoint);
    let enabled = pool.settings(&cancellation).await?.enabled();
    let candidates = pool
        .candidates(&request.namespace.thread, &cancellation)
        .await?;
    let original = pool
        .affinity(&request.namespace.thread)?
        .or_else(|| candidates.first().map(|account| account.reference.clone()));
    if let Some(account) = &original {
        request.namespace.account = credentials.namespace_account(account);
        let store = SessionStore::new(home, request.namespace.clone())
            .with_checkpoint_scope(checkpoint_scope.into());
        request.checkpoint = store.load()?;
        validate_lineage(&mut request, provider.recovery_policy())?;
    }
    let mut prior_journals = previous
        .as_ref()
        .map(|record| record.prior_journals.clone())
        .unwrap_or_default();
    if let Some(record) = previous.as_ref()
        && !prior_journals.contains(&record.namespace)
    {
        prior_journals.push(record.namespace.clone());
    }
    let mut attempt = previous.map_or(0, |record| record.attempt);
    let mut attempted = BTreeSet::new();
    let mut last_failure = None;
    for account in candidates {
        if !attempted.insert(account.reference.clone()) {
            continue;
        }
        let reference = account.reference.clone();
        let Some(permit) = pool
            .reserve(account, &request.namespace.model, now(), &cancellation)
            .await?
        else {
            continue;
        };
        let mut candidate = if Some(&reference) != original.as_ref() {
            export_handoff(
                provider.as_ref(),
                &request,
                if last_failure.is_some() {
                    ContinuationBoundary::InitialRejection
                } else {
                    ContinuationBoundary::CompletedTurn
                },
            )?
        } else {
            request.clone()
        };
        if Some(&reference) != original.as_ref() {
            validate_handoff(&request, &candidate)?;
        }
        let attempt_cancellation = cancellation.child_token();
        let attempt_guard = attempt_cancellation.clone().drop_guard();
        let auth = match credentials
            .credentials_for(Some(&reference), attempt_cancellation.clone())
            .await
        {
            Ok(auth) => auth,
            Err(ProviderError::Authentication(_)) if enabled => {
                let failure = Failure {
                    category: FailureCategory::LoginRequired,
                    code: FailureCode::CredentialRevoked,
                    scope: FailureScope::Credential,
                    reset_at: None,
                    retry_at: None,
                    outcome: RequestOutcome::NotDispatched,
                    another_account_may_help: true,
                };
                pool.failed(&permit, &failure, now(), &cancellation).await?;
                last_failure = Some(failure);
                continue;
            }
            Err(error) => return Err(error),
        };
        candidate.namespace.account = auth.account_id.clone();
        if let Some(old) = &original
            && old != &reference
            && !prior_journals.contains(&request.namespace)
        {
            prior_journals.push(request.namespace.clone());
        }
        let store = SessionStore::new(home, candidate.namespace.clone())
            .with_checkpoint_scope(checkpoint_scope.into())
            .with_prior_journals(prior_journals.clone());
        if enabled {
            attempt += 1;
            let record = Handoff {
                version: 1,
                attempt,
                history: HistoryAcknowledgement {
                    hash: lineage_hash(&candidate.system, &candidate.messages)?,
                    length: candidate.messages.len(),
                    checkpoint_scope: checkpoint_scope.into(),
                },
                namespace: candidate.namespace.clone(),
                prior_journals: prior_journals.clone(),
                state: AttemptState::Prepared,
            };
            journal
                .store
                .lock(&journal.key, &cancellation)
                .await?
                .write(&record)?;
            journal
                .state(AttemptState::Dispatched, &cancellation)
                .await?;
        }
        let rejected_token = auth.access_token.clone();
        let mut active_token = rejected_token.clone();
        let mut refreshed_authentication = false;
        let mut result = provider
            .start(
                candidate.clone(),
                Context {
                    credentials: auth,
                    cancellation: attempt_cancellation.clone(),
                },
            )
            .await;
        if matches!(&result, Err(ProviderError::Classified(failure)) if failure.category == FailureCategory::Authentication && failure.outcome == RequestOutcome::Rejected)
        {
            refreshed_authentication = true;
            if enabled {
                journal.state(AttemptState::Rejected, &cancellation).await?;
            }
            match credentials
                .refresh_rejected(&reference, &rejected_token, attempt_cancellation.clone())
                .await
            {
                Ok(Some(refreshed)) => {
                    if enabled {
                        journal
                            .state(AttemptState::Dispatched, &cancellation)
                            .await?;
                    }
                    active_token = refreshed.access_token.clone();
                    result = provider
                        .start(
                            candidate.clone(),
                            Context {
                                credentials: refreshed,
                                cancellation: attempt_cancellation.clone(),
                            },
                        )
                        .await;
                }
                Ok(None) => {}
                Err(ProviderError::Authentication(_)) => result = Err(login_required()),
                Err(error) => return Err(error),
            }
        }
        match result {
            Ok(session) => {
                if enabled {
                    journal.state(AttemptState::Accepted, &cancellation).await?;
                }
                pool.accept(&candidate.namespace.thread, &reference, &cancellation)
                    .await?;
                let switched = original
                    .as_ref()
                    .filter(|original| *original != &reference)
                    .map(|original| (original.label.clone(), reference.label.clone()));
                let session = Box::new(ManagedSession {
                    inner: session,
                    attempt_guard,
                    active_token,
                    refreshed_authentication,
                    pool,
                    permit,
                    credentials,
                    cancellation: cancellation.clone(),
                    provider: provider.clone(),
                    journal: enabled.then_some(journal.clone()),
                    attempted,
                    queued: std::collections::VecDeque::new(),
                });
                return Ok(StartedTurn {
                    request: candidate,
                    session,
                    store,
                    execution,
                    journal: enabled.then_some(journal),
                    switched,
                });
            }
            Err(ProviderError::Classified(failure)) => {
                pool.failed(&permit, &failure, now(), &cancellation).await?;
                if enabled && failure.outcome != RequestOutcome::PotentiallyProcessed {
                    journal.state(AttemptState::Rejected, &cancellation).await?;
                }
                if !enabled || !failure.can_fail_over() {
                    return Err(ProviderError::Classified(failure));
                }
                last_failure = Some(failure);
            }
            Err(error) => return Err(error),
        }
    }
    Err(pool
        .exhausted(&request.namespace.model, &cancellation)
        .await?)
}
struct ManagedSession {
    attempt_guard: tokio_util::sync::DropGuard,
    active_token: String,
    refreshed_authentication: bool,
    inner: Box<dyn ProviderSession>,
    pool: PoolStore,
    permit: AttemptPermit,
    credentials: Arc<dyn AccountCredentials>,
    cancellation: CancellationToken,
    provider: Arc<dyn Provider>,
    journal: Option<HandoffJournal>,
    attempted: BTreeSet<AccountRef>,
    queued: std::collections::VecDeque<Event>,
}
impl ManagedSession {
    async fn prepare_attempt(
        &mut self,
        request: &Request,
        history: &HistoryAcknowledgement,
    ) -> Result<Vec<Namespace>> {
        let Some(journal) = &self.journal else {
            return Ok(vec![]);
        };
        let previous = journal
            .read()?
            .ok_or_else(|| ProviderError::Checkpoint("missing account attempt journal".into()))?;
        if previous.history.checkpoint_scope != history.checkpoint_scope {
            return Err(ProviderError::Checkpoint(
                "continuation history scope changed".into(),
            ));
        }
        let mut prior = previous.prior_journals;
        if previous.namespace != request.namespace && !prior.contains(&previous.namespace) {
            prior.push(previous.namespace);
        }
        let record = Handoff {
            version: 1,
            attempt: previous.attempt + 1,
            history: history.clone(),
            namespace: request.namespace.clone(),
            prior_journals: prior.clone(),
            state: AttemptState::Prepared,
        };
        journal
            .store
            .lock(&journal.key, &self.cancellation)
            .await?
            .write(&record)?;
        journal
            .state(AttemptState::Dispatched, &self.cancellation)
            .await?;
        Ok(prior)
    }
    async fn continue_authorized(&mut self, history: Option<HistoryAcknowledgement>) -> Result<()> {
        self.credentials
            .validate_account(&self.permit.account.reference)?;
        let request = self.inner.continuation_request()?;
        if let Some(history) = &history {
            self.prepare_attempt(&request, history).await?;
        }
        let mut result = self.inner.continue_round().await;
        if !self.refreshed_authentication
            && matches!(&result, Err(ProviderError::Classified(failure)) if failure.category == FailureCategory::Authentication && failure.outcome == RequestOutcome::Rejected)
        {
            self.refreshed_authentication = true;
            if let Some(journal) = &self.journal {
                journal
                    .state(AttemptState::Rejected, &self.cancellation)
                    .await?;
            }
            match self
                .credentials
                .refresh_rejected(
                    &self.permit.account.reference,
                    &self.active_token,
                    self.cancellation.clone(),
                )
                .await
            {
                Ok(Some(credentials)) => {
                    self.active_token = credentials.access_token.clone();
                    if let Some(history) = &history {
                        self.prepare_attempt(&request, history).await?;
                    }
                    result = self.inner.retry_authentication(credentials).await;
                }
                Ok(None) => {}
                Err(ProviderError::Authentication(_)) => result = Err(login_required()),
                Err(error) => return Err(error),
            }
        }
        let result = self.observe(result).await;
        let failure = match result {
            Ok(()) => {
                if let Some(journal) = &self.journal {
                    journal
                        .state(AttemptState::Accepted, &self.cancellation)
                        .await?;
                }
                return Ok(());
            }
            Err(ProviderError::Classified(failure)) if failure.can_fail_over() => failure,
            Err(error) => return Err(error),
        };
        if let Some(journal) = &self.journal {
            journal
                .state(AttemptState::Rejected, &self.cancellation)
                .await?;
        }
        if !self.pool.settings(&self.cancellation).await?.enabled() {
            return Err(ProviderError::Classified(failure));
        }
        let history = history.ok_or_else(|| ProviderError::Unsupported("account is unavailable, but the host has not acknowledged durable history; explicitly recover the conversation".into()))?;
        let mut exported = export_handoff(
            self.provider.as_ref(),
            &request,
            ContinuationBoundary::RecordedTools,
        )?;
        validate_handoff(&request, &exported)?;
        // Cancellation acknowledgement and dropping the old transport fence its events.
        tokio::time::timeout(std::time::Duration::from_secs(2), self.inner.cancel())
            .await
            .map_err(|_| {
                ProviderError::Checkpoint("old account attempt could not be fenced".into())
            })??;
        let previous_label = self.permit.account.reference.label.clone();
        for candidate in self
            .pool
            .candidates(&request.namespace.thread, &self.cancellation)
            .await?
        {
            if !self.attempted.insert(candidate.reference.clone()) {
                continue;
            }
            let reference = candidate.reference.clone();
            let Some(permit) = self
                .pool
                .reserve(
                    candidate,
                    &request.namespace.model,
                    now(),
                    &self.cancellation,
                )
                .await?
            else {
                continue;
            };
            let attempt_cancellation = self.cancellation.child_token();
            let attempt_guard = attempt_cancellation.clone().drop_guard();
            let auth = match self
                .credentials
                .credentials_for(Some(&reference), attempt_cancellation.clone())
                .await
            {
                Ok(auth) => auth,
                Err(ProviderError::Authentication(_)) => {
                    self.pool
                        .failed(
                            &permit,
                            &Failure {
                                category: FailureCategory::LoginRequired,
                                code: FailureCode::CredentialRevoked,
                                scope: FailureScope::Credential,
                                reset_at: None,
                                retry_at: None,
                                outcome: RequestOutcome::NotDispatched,
                                another_account_may_help: true,
                            },
                            now(),
                            &self.cancellation,
                        )
                        .await?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let mut active_token = auth.access_token.clone();
            exported.namespace.account = auth.account_id.clone();
            let prior_journals = self.prepare_attempt(&exported, &history).await?;
            let mut result = self
                .provider
                .start(
                    exported.clone(),
                    Context {
                        credentials: auth,
                        cancellation: attempt_cancellation.clone(),
                    },
                )
                .await;
            let mut refreshed_authentication = false;
            if matches!(&result, Err(ProviderError::Classified(failure)) if failure.category == FailureCategory::Authentication && failure.outcome == RequestOutcome::Rejected)
            {
                refreshed_authentication = true;
                if let Some(journal) = &self.journal {
                    journal
                        .state(AttemptState::Rejected, &self.cancellation)
                        .await?;
                }
                match self
                    .credentials
                    .refresh_rejected(&reference, &active_token, attempt_cancellation.clone())
                    .await
                {
                    Ok(Some(credentials)) => {
                        self.prepare_attempt(&exported, &history).await?;
                        active_token = credentials.access_token.clone();
                        result = self
                            .provider
                            .start(
                                exported.clone(),
                                Context {
                                    credentials,
                                    cancellation: attempt_cancellation.clone(),
                                },
                            )
                            .await;
                    }
                    Ok(None) => {}
                    Err(ProviderError::Authentication(_)) => result = Err(login_required()),
                    Err(error) => return Err(error),
                }
            }
            match result {
                Ok(inner) => {
                    if let Some(journal) = &self.journal {
                        journal
                            .state(AttemptState::Accepted, &self.cancellation)
                            .await?;
                    }
                    self.pool
                        .accept(&exported.namespace.thread, &reference, &self.cancellation)
                        .await?;
                    self.inner = inner;
                    self.attempt_guard = attempt_guard;
                    self.active_token = active_token;
                    self.refreshed_authentication = refreshed_authentication;
                    self.permit = permit;
                    self.queued.push_back(Event::AccountChanged {
                        previous: previous_label,
                        account: reference.label,
                        namespace: exported.namespace.clone(),
                        prior_journals,
                    });
                    return Ok(());
                }
                Err(ProviderError::Classified(failure)) => {
                    self.pool
                        .failed(&permit, &failure, now(), &self.cancellation)
                        .await?;
                    if failure.outcome != RequestOutcome::PotentiallyProcessed
                        && let Some(journal) = &self.journal
                    {
                        journal
                            .state(AttemptState::Rejected, &self.cancellation)
                            .await?;
                    }
                    if !failure.can_fail_over() {
                        return Err(ProviderError::Classified(failure));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(self
            .pool
            .exhausted(&request.namespace.model, &self.cancellation)
            .await?)
    }

    async fn observe<T: Send>(&mut self, result: Result<T>) -> Result<T> {
        if let Err(ProviderError::Classified(failure)) = &result {
            self.pool
                .failed(&self.permit, failure, now(), &self.cancellation)
                .await?;
        }
        result
    }
}
impl ProviderSession for ManagedSession {
    fn next_event(&mut self) -> ProviderFuture<'_, Option<Event>> {
        Box::pin(async move {
            if let Some(event) = self.queued.pop_front() {
                return Ok(Some(event));
            }
            let result = self.inner.next_event().await;
            let event = self.observe(result).await?;
            if matches!(event, Some(Event::Completed { .. })) {
                self.pool
                    .succeeded(&self.permit, &self.cancellation)
                    .await?;
            }
            Ok(event)
        })
    }
    fn submit_tool_result(&mut self, result: ToolResult) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            self.credentials
                .validate_account(&self.permit.account.reference)?;
            let result = self.inner.submit_tool_result(result).await;
            self.observe(result).await
        })
    }
    fn continue_round(&mut self) -> ProviderFuture<'_, ()> {
        Box::pin(self.continue_authorized(None))
    }
    fn authorize_continuation(
        &mut self,
        history: Option<HistoryAcknowledgement>,
    ) -> ProviderFuture<'_, ()> {
        Box::pin(self.continue_authorized(history))
    }
    fn cancel(&mut self) -> ProviderFuture<'_, ()> {
        self.inner.cancel()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handoff_preserves_billing_and_capability_metadata_except_the_old_history_binding() {
        let original = Request {
            namespace: Namespace {
                provider: "fifth".into(),
                account: "a".into(),
                model: "m".into(),
                thread: "t".into(),
            },
            system: "system".into(),
            messages: vec![],
            tools: vec![],
            parallel_tool_calls: true,
            max_output_tokens: Some(100),
            reasoning_effort: Some("high".into()),
            output_schema: None,
            checkpoint: None,
            metadata: serde_json::json!({"billing_mode":"configured", "required_capability":true, "host_checkpoint_prefix":3}),
        };
        let mut exported = original.clone();
        exported
            .metadata
            .as_object_mut()
            .unwrap()
            .remove("host_checkpoint_prefix");
        validate_handoff(&original, &exported).unwrap();
        exported.metadata["billing_mode"] = serde_json::json!("changed");
        assert!(validate_handoff(&original, &exported).is_err());
    }

    #[tokio::test]
    async fn handoff_crashes_preserve_uncertainty_across_provider_account_model_and_compaction() {
        for state in [
            AttemptState::Prepared,
            AttemptState::Dispatched,
            AttemptState::Rejected,
            AttemptState::Accepted,
            AttemptState::Completed,
        ] {
            let home = tempfile::tempdir().unwrap();
            let cancel = CancellationToken::new();
            let namespace = Namespace {
                provider: "fifth".into(),
                account: "a:generation".into(),
                model: "m".into(),
                thread: "thread".into(),
            };
            let journal = HandoffJournal::new(home.path(), "fifth", "endpoint", "thread", "turn");
            let record = Handoff {
                version: 1,
                attempt: 1,
                history: HistoryAcknowledgement {
                    hash: "persisted".into(),
                    length: 3,
                    checkpoint_scope: "window".into(),
                },
                namespace: namespace.clone(),
                prior_journals: vec![],
                state: AttemptState::Prepared,
            };
            journal
                .store
                .lock(&journal.key, &cancel)
                .await
                .unwrap()
                .write(&record)
                .unwrap();
            if state != AttemptState::Prepared {
                journal
                    .state(AttemptState::Dispatched, &cancel)
                    .await
                    .unwrap();
                journal.state(state.clone(), &cancel).await.unwrap();
            }
            let changed = Namespace {
                provider: "other".into(),
                account: "replacement".into(),
                model: "other".into(),
                ..namespace.clone()
            };
            let restarted =
                SessionStore::new(home.path(), changed).with_checkpoint_scope("compacted".into());
            assert_eq!(
                restarted.begin_execution("turn", &cancel).await.is_err(),
                matches!(state, AttemptState::Dispatched | AttemptState::Accepted),
                "{state:?}"
            );
            // A crash after writing the dispatch fence but before the attempt write is uncertain too.
            journal
                .store
                .lock(&journal.fence, &cancel)
                .await
                .unwrap()
                .write(&true)
                .unwrap();
            assert!(restarted.begin_execution("turn", &cancel).await.is_err());
            let branch = SessionStore::new(
                home.path(),
                Namespace {
                    thread: "explicit-branch".into(),
                    ..namespace
                },
            );
            assert!(branch.begin_execution("turn", &cancel).await.is_ok());
        }
    }
}
