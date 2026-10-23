#![allow(clippy::expect_used)]
use codex_provider_api::*;
use codex_provider_auth::*;
use codex_provider_runtime::*;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;
struct CredentialsSource(AuthStore);
impl AccountCredentials for CredentialsSource {
    fn configured_token(&self) -> bool {
        false
    }
    fn namespace_account(&self, account: &AccountRef) -> String {
        account.identity()
    }
    fn credentials_for<'a>(
        &'a self,
        account: Option<&'a AccountRef>,
        cancel: CancellationToken,
    ) -> ProviderFuture<'a, Credentials> {
        Box::pin(async move {
            self.0
                .credentials_for("fifth", account.expect("valid test fixture"), None, &cancel)
                .await
        })
    }
    fn refresh_rejected<'a>(
        &'a self,
        _: &'a AccountRef,
        _: &'a str,
        _: CancellationToken,
    ) -> ProviderFuture<'a, Option<Credentials>> {
        Box::pin(async {
            Err(ProviderError::Authentication(
                "refresh credential revoked".into(),
            ))
        })
    }
    fn validate_account(&self, account: &AccountRef) -> Result<()> {
        assert_eq!(self.0.account_reference("fifth", &account.label)?, *account);
        Ok(())
    }
}
struct Fifth {
    starts: Arc<Mutex<Vec<String>>>,
    mid_turn: bool,
    ambiguous: bool,
    authentication: bool,
}
fn quota(outcome: RequestOutcome) -> ProviderError {
    ProviderError::Classified(Failure {
        category: FailureCategory::QuotaExhausted,
        code: FailureCode::QuotaExhausted,
        scope: FailureScope::Account,
        reset_at: None,
        retry_at: None,
        outcome,
        another_account_may_help: true,
    })
}
impl Provider for Fifth {
    fn id(&self) -> &'static str {
        "fifth"
    }
    fn models(&self, _: Context) -> ProviderFuture<'_, Vec<Model>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn handoff_request(&self, request: &Request, _: ContinuationBoundary) -> Result<Request> {
        let mut request = request.clone();
        request.checkpoint = None;
        Ok(request)
    }
    fn start(&self, request: Request, _: Context) -> ProviderFuture<'_, Box<dyn ProviderSession>> {
        Box::pin(async move {
            self.starts
                .lock()
                .expect("valid test fixture")
                .push(request.namespace.account.clone());
            assert_eq!(request.namespace.model, "exact-model");
            assert_eq!(request.reasoning_effort.as_deref(), Some("high"));
            assert_eq!(request.tools[0].name, "shell");
            if request.namespace.account.starts_with("a:") && !self.mid_turn {
                if self.authentication {
                    return Err(authentication());
                }
                return Err(quota(if self.ambiguous {
                    RequestOutcome::PotentiallyProcessed
                } else {
                    RequestOutcome::Rejected
                }));
            }
            Ok(Box::new(Session {
                request,
                mid_turn: self.mid_turn,
                authentication: self.authentication,
            }) as Box<dyn ProviderSession>)
        })
    }
}
struct Session {
    request: Request,
    mid_turn: bool,
    authentication: bool,
}
impl ProviderSession for Session {
    fn next_event(&mut self) -> ProviderFuture<'_, Option<Event>> {
        Box::pin(async { Ok(Some(Event::Completed { id: "done".into() })) })
    }
    fn submit_tool_result(&mut self, result: ToolResult) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            self.request.messages.push(Message {
                role: Role::Tool,
                content: vec![Content::ToolResult(result)],
            });
            Ok(())
        })
    }
    fn cancel(&mut self) -> ProviderFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn continuation_request(&self) -> Result<Request> {
        Ok(self.request.clone())
    }
    fn continue_round(&mut self) -> ProviderFuture<'_, ()> {
        Box::pin(async {
            if self.mid_turn && self.authentication {
                Err(authentication())
            } else if self.mid_turn {
                Err(quota(RequestOutcome::Rejected))
            } else {
                Ok(())
            }
        })
    }
}
fn request(thread: &str) -> Request {
    Request {
        namespace: Namespace {
            provider: "fifth".into(),
            account: String::new(),
            model: "exact-model".into(),
            thread: thread.into(),
        },
        system: "system".into(),
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "hello".into(),
            }],
        }],
        tools: vec![ToolDefinition {
            name: "shell".into(),
            description: String::new(),
            parameters: json!({}),
        }],
        parallel_tool_calls: true,
        max_output_tokens: Some(123),
        reasoning_effort: Some("high".into()),
        output_schema: None,
        checkpoint: None,
        metadata: json!({}),
    }
}
async fn setup(home: &std::path::Path) -> Arc<dyn AccountCredentials> {
    let auth = AuthStore::new(home);
    let cancel = CancellationToken::new();
    let pool = PoolStore::new(home, "fifth", "endpoint");
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
        let settings = pool.settings(&cancel).await.expect("valid test fixture");
        pool.update(
            settings.revision,
            PoolUpdate::Enroll {
                label: label.into(),
            },
            &cancel,
        )
        .await
        .expect("valid test fixture");
    }
    Arc::new(CredentialsSource(auth))
}
#[tokio::test]
async fn fifth_provider_failover_preserves_request_and_affinity_without_selection_mutation() {
    let home = tempfile::tempdir().expect("valid test fixture");
    let credentials = setup(home.path()).await;
    let auth = AuthStore::new(home.path());
    auth.select_account(
        "fifth",
        &auth.account_reference("fifth", "a").unwrap(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let starts = Arc::new(Mutex::new(vec![]));
    let cancel = CancellationToken::new();
    let provider = Arc::new(Fifth {
        starts: starts.clone(),
        mid_turn: false,
        ambiguous: false,
        authentication: false,
    });
    let turn = start_turn(
        home.path(),
        "endpoint",
        "turn",
        "window",
        request("thread"),
        provider.clone(),
        credentials.clone(),
        cancel.clone(),
    )
    .await
    .expect("valid test fixture");
    assert!(turn.request.namespace.account.starts_with("b:"));
    assert_eq!(starts.lock().expect("valid test fixture").len(), 2);
    turn.journal
        .as_ref()
        .expect("valid test fixture")
        .complete(&cancel)
        .await
        .expect("valid test fixture");
    drop(turn);
    let turn = start_turn(
        home.path(),
        "endpoint",
        "turn",
        "window",
        request("thread"),
        provider,
        credentials,
        cancel,
    )
    .await
    .expect("valid test fixture");
    assert!(turn.request.namespace.account.starts_with("b:"));
    assert_eq!(starts.lock().expect("valid test fixture").len(), 3);
    assert_eq!(
        AuthStore::new(home.path())
            .selected("fifth")
            .expect("valid test fixture")
            .as_deref(),
        Some("a")
    );
}
#[tokio::test]
async fn uncertain_attempt_stops_and_fences_restart() {
    let home = tempfile::tempdir().expect("valid test fixture");
    let credentials = setup(home.path()).await;
    let starts = Arc::new(Mutex::new(vec![]));
    let cancel = CancellationToken::new();
    let provider = Arc::new(Fifth {
        starts: starts.clone(),
        mid_turn: false,
        ambiguous: true,
        authentication: false,
    });
    assert!(
        start_turn(
            home.path(),
            "endpoint",
            "turn",
            "window",
            request("thread"),
            provider.clone(),
            credentials.clone(),
            cancel.clone()
        )
        .await
        .is_err()
    );
    assert_eq!(starts.lock().expect("valid test fixture").len(), 1);
    assert!(
        start_turn(
            home.path(),
            "endpoint",
            "turn",
            "new-window",
            request("thread"),
            provider,
            credentials,
            cancel
        )
        .await
        .is_err()
    );
    assert_eq!(starts.lock().expect("valid test fixture").len(), 1);
}
#[tokio::test]
async fn completed_tools_require_host_acknowledgement_and_retain_journal_namespaces() {
    let home = tempfile::tempdir().expect("valid test fixture");
    let credentials = setup(home.path()).await;
    let starts = Arc::new(Mutex::new(vec![]));
    let cancel = CancellationToken::new();
    let provider = Arc::new(Fifth {
        starts: starts.clone(),
        mid_turn: true,
        ambiguous: false,
        authentication: false,
    });
    let mut turn = start_turn(
        home.path(),
        "endpoint",
        "turn",
        "window",
        request("thread"),
        provider,
        credentials,
        cancel.clone(),
    )
    .await
    .expect("valid test fixture");
    let first = turn.request.namespace.clone();
    let result = ToolResult {
        call_id: "recorded".into(),
        output: json!("denied"),
        is_error: true,
    };
    turn.session
        .submit_tool_result(result)
        .await
        .expect("valid test fixture");
    turn.session
        .authorize_continuation(Some(HistoryAcknowledgement {
            hash: "host-verified-history".into(),
            length: 2,
            checkpoint_scope: "window".into(),
        }))
        .await
        .expect("valid test fixture");
    let Some(Event::AccountChanged {
        namespace,
        prior_journals,
        ..
    }) = turn.session.next_event().await.expect("valid test fixture")
    else {
        panic!("missing account acceptance event");
    };
    assert!(namespace.account.starts_with("b:"));
    assert!(prior_journals.contains(&first));
    assert_eq!(starts.lock().expect("valid test fixture").len(), 2);
}

fn authentication() -> ProviderError {
    ProviderError::Classified(Failure {
        category: FailureCategory::Authentication,
        code: FailureCode::AuthenticationRejected,
        scope: FailureScope::Credential,
        reset_at: None,
        retry_at: None,
        outcome: RequestOutcome::Rejected,
        another_account_may_help: true,
    })
}
#[tokio::test]
async fn revoked_refresh_after_conclusive_rejection_uses_next_account_at_both_boundaries() {
    for mid_turn in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let credentials = setup(home.path()).await;
        let starts = Arc::new(Mutex::new(vec![]));
        let cancellation = CancellationToken::new();
        let mut turn = start_turn(
            home.path(),
            "endpoint",
            "turn",
            "window",
            request("thread"),
            Arc::new(Fifth {
                starts: starts.clone(),
                mid_turn,
                ambiguous: false,
                authentication: true,
            }),
            credentials,
            cancellation.clone(),
        )
        .await
        .unwrap();
        if mid_turn {
            turn.session
                .authorize_continuation(Some(HistoryAcknowledgement {
                    hash: "persisted-history".into(),
                    length: 1,
                    checkpoint_scope: "window".into(),
                }))
                .await
                .unwrap();
            assert!(
                matches!(turn.session.next_event().await.unwrap(), Some(Event::AccountChanged { account, .. }) if account == "b")
            );
        } else {
            assert!(turn.request.namespace.account.starts_with("b:"));
        }
        assert_eq!(starts.lock().unwrap().len(), 2);
        let pool = PoolStore::new(home.path(), "fifth", "endpoint");
        assert_eq!(pool.affinity("thread").unwrap().unwrap().label, "b");
        let status = pool.status(None, &cancellation).await.unwrap();
        assert!(
            status
                .accounts
                .iter()
                .find(|entry| entry.account.reference.label == "a")
                .unwrap()
                .restrictions
                .iter()
                .any(|restriction| restriction.failure.category == FailureCategory::LoginRequired)
        );
    }
}

#[tokio::test]
async fn ephemeral_history_does_not_authorize_tool_round_handoff() {
    let home = tempfile::tempdir().unwrap();
    let credentials = setup(home.path()).await;
    let starts = Arc::new(Mutex::new(vec![]));
    let mut turn = start_turn(
        home.path(),
        "endpoint",
        "turn",
        "window",
        request("ephemeral"),
        Arc::new(Fifth {
            starts: starts.clone(),
            mid_turn: true,
            ambiguous: false,
            authentication: false,
        }),
        credentials,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let error = turn.session.authorize_continuation(None).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("has not acknowledged durable history")
    );
    assert_eq!(starts.lock().unwrap().len(), 1);
    assert_eq!(
        PoolStore::new(home.path(), "fifth", "endpoint")
            .affinity("ephemeral")
            .unwrap()
            .unwrap()
            .label,
        "a"
    );
}

struct ParallelRoundProvider;

fn parallel_call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "shell".into(),
        arguments: json!({"cmd":"same command"}),
    }
}

impl Provider for ParallelRoundProvider {
    fn id(&self) -> &'static str {
        "fifth"
    }
    fn models(&self, _: Context) -> ProviderFuture<'_, Vec<Model>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn handoff_request(&self, request: &Request, _: ContinuationBoundary) -> Result<Request> {
        let mut copy = request.clone();
        copy.checkpoint = None;
        Ok(copy)
    }
    fn start(&self, request: Request, _: Context) -> ProviderFuture<'_, Box<dyn ProviderSession>> {
        Box::pin(async move {
            let first = request.namespace.account.starts_with("a:");
            if !first {
                let results: Vec<_> = request
                    .messages
                    .iter()
                    .flat_map(|m| &m.content)
                    .filter_map(|c| {
                        if let Content::ToolResult(r) = c {
                            Some(r)
                        } else {
                            None
                        }
                    })
                    .collect();
                assert_eq!(results.len(), 2);
                assert_eq!(results[0].call_id, "denied");
                assert!(results[0].is_error);
                assert_eq!(results[1].call_id, "ok");
            }
            let ids = if first {
                vec!["ok", "denied"]
            } else {
                vec!["ok", "denied", "new"]
            };
            Ok(Box::new(ParallelRoundSession {
                pending: ids.len(),
                events: ids
                    .into_iter()
                    .map(|id| Event::ToolCall(parallel_call(id)))
                    .collect(),
                request,
                first,
            }) as Box<dyn ProviderSession>)
        })
    }
}

struct ParallelRoundSession {
    request: Request,
    events: std::collections::VecDeque<Event>,
    pending: usize,
    first: bool,
}
impl ProviderSession for ParallelRoundSession {
    fn next_event(&mut self) -> ProviderFuture<'_, Option<Event>> {
        Box::pin(async move {
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
            std::future::pending().await
        })
    }
    fn submit_tool_result(&mut self, result: ToolResult) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            self.request.messages.push(Message {
                role: Role::Tool,
                content: vec![Content::ToolResult(result)],
            });
            self.pending -= 1;
            if self.pending == 0 {
                self.events.push_back(Event::ContinuationReady);
            }
            Ok(())
        })
    }
    fn continuation_request(&self) -> Result<Request> {
        Ok(self.request.clone())
    }
    fn continue_round(&mut self) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            if self.first {
                return Err(quota(RequestOutcome::Rejected));
            }
            self.events
                .push_back(Event::Completed { id: "done".into() });
            Ok(())
        })
    }
    fn cancel(&mut self) -> ProviderFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

struct JournaledParallelHost {
    home: std::path::PathBuf,
    store: Mutex<SessionStore>,
    executed: Mutex<Vec<String>>,
    results: Mutex<Vec<ToolResult>>,
    both_started: tokio::sync::Barrier,
    denied_finished: tokio::sync::Notify,
}
impl Host for JournaledParallelHost {
    fn execute(
        &self,
        call: ToolCall,
        cancellation: CancellationToken,
    ) -> ProviderFuture<'_, ToolResult> {
        Box::pin(async move {
            let store = self.store.lock().expect("journal store lock").clone();
            let ticket = match store.begin_tool(call.clone(), &cancellation).await? {
                ToolDisposition::Replay(result) => return Ok(result),
                ToolDisposition::Execute(ticket) => ticket,
            };
            self.executed
                .lock()
                .expect("execution log lock")
                .push(call.id.clone());
            if call.id != "new" {
                self.both_started.wait().await;
            }
            if call.id == "ok" {
                self.denied_finished.notified().await;
            }
            let result = ticket.finish(ToolResult {
                call_id: call.id.clone(),
                output: json!({"id":call.id}),
                is_error: call.id == "denied",
            })?;
            self.results
                .lock()
                .expect("recorded results lock")
                .push(result.clone());
            if call.id == "denied" {
                self.denied_finished.notify_one();
            }
            Ok(result)
        })
    }
    fn checkpoint(&self, _: Checkpoint) -> ProviderFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn persisted_history(&self) -> ProviderFuture<'_, Option<HistoryAcknowledgement>> {
        Box::pin(async move {
            let results = self.results.lock().expect("recorded results lock").clone();
            Store::new(&self.home)
                .lock("complex-history", &CancellationToken::new())
                .await?
                .write(&results)?;
            Ok(Some(HistoryAcknowledgement {
                hash: digest(&serde_json::to_vec(&results).expect("serialize recorded history")),
                length: results.len(),
                checkpoint_scope: "window".into(),
            }))
        })
    }
    fn account_changed(
        &self,
        namespace: Namespace,
        prior: Vec<Namespace>,
    ) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            let mut store = self.store.lock().expect("journal store lock");
            *store = store.for_namespace(namespace).with_prior_journals(prior);
            Ok(())
        })
    }
}

#[tokio::test]
async fn parallel_denial_and_success_handoff_replays_ids_but_executes_new_identical_command() {
    let home = tempfile::tempdir().unwrap();
    let credentials = setup(home.path()).await;
    let cancel = CancellationToken::new();
    let turn = start_turn(
        home.path(),
        "endpoint",
        "turn",
        "window",
        request("complex"),
        Arc::new(ParallelRoundProvider),
        credentials,
        cancel.clone(),
    )
    .await
    .unwrap();
    turn.execution.mark_tools_started().unwrap();
    let host = Arc::new(JournaledParallelHost {
        home: home.path().to_path_buf(),
        store: Mutex::new(turn.store),
        executed: Mutex::new(vec![]),
        results: Mutex::new(vec![]),
        both_started: tokio::sync::Barrier::new(2),
        denied_finished: tokio::sync::Notify::new(),
    });
    let (events, mut received) = tokio::sync::mpsc::channel(32);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        drive_session(turn.session, host.clone(), &turn.request, cancel, events),
    )
    .await
    .expect("parallel tools must not deadlock")
    .unwrap();
    turn.execution.complete().unwrap();
    let mut calls = host.executed.lock().unwrap().clone();
    calls.sort();
    assert_eq!(calls, vec!["denied", "new", "ok"]);
    let mut changes = 0;
    while let Some(event) = received.recv().await {
        if matches!(event.unwrap(), Event::AccountChanged { .. }) {
            changes += 1;
        }
    }
    assert_eq!(changes, 1);
    assert_eq!(
        PoolStore::new(home.path(), "fifth", "endpoint")
            .affinity("complex")
            .unwrap()
            .unwrap()
            .label,
        "b"
    );
}
