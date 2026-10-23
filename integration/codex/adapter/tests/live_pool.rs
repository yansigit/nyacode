//! Opt-in acceptance around real transports. Injection exists only in this test
//! binary and conclusively rejects before dispatch; production has no hook.
#![allow(clippy::expect_used)]
use codex_api::ResponseEvent;
use codex_protocol::models::ResponseItem;
use codex_provider_adapter::backend::NativeBackend;
use codex_provider_adapter::session::Prompt;
use codex_provider_adapter::session::stream;
use codex_provider_api::*;
use codex_provider_runtime::AccountCredentials;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

struct RejectOnce {
    inner: Arc<dyn Provider>,
    account: String,
    injected: AtomicBool,
}
impl Provider for RejectOnce {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    fn recovery_policy(&self) -> RecoveryPolicy {
        self.inner.recovery_policy()
    }
    fn native_operations(&self) -> Option<Arc<dyn NativeOperations>> {
        self.inner.native_operations()
    }
    fn models(&self, context: Context) -> ProviderFuture<'_, Vec<Model>> {
        self.inner.models(context)
    }
    fn handoff_request(
        &self,
        request: &Request,
        boundary: ContinuationBoundary,
    ) -> Result<Request> {
        self.inner.handoff_request(request, boundary)
    }
    fn start(
        &self,
        request: Request,
        context: Context,
    ) -> ProviderFuture<'_, Box<dyn ProviderSession>> {
        Box::pin(async move {
            if request.namespace.account == self.account
                && !self.injected.swap(true, Ordering::SeqCst)
            {
                return Err(ProviderError::Classified(Failure {
                    category: FailureCategory::TemporaryLimit,
                    code: FailureCode::RateLimit,
                    scope: FailureScope::Credential,
                    reset_at: None,
                    retry_at: None,
                    outcome: RequestOutcome::NotDispatched,
                    another_account_may_help: true,
                }));
            }
            self.inner.start(request, context).await
        })
    }
}
fn user(text: &str) -> ResponseItem {
    serde_json::from_value(
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]}),
    )
    .expect("valid test fixture")
}
async fn turn(
    backend: Arc<NativeBackend>,
    model: &str,
    thread: &str,
    mut history: Vec<ResponseItem>,
) -> std::result::Result<(Vec<ResponseItem>, usize), String> {
    let prompt = Prompt {
        input: history.clone(),
        system:
            "You are a helpful assistant. Follow the user request exactly and keep the reply brief."
                .into(),
        tools: vec![
            json!({"type":"function","name":"lookup","description":"Look up a test value only when requested","parameters":{"type":"object","properties":{"key":{"type":"string"}},"required":["key"],"additionalProperties":false}}),
        ],
        persistence_acknowledgements: true,
        ..Prompt::default()
    };
    let mut session = stream(
        backend,
        &prompt,
        model,
        thread.into(),
        std::env::var("CODEX_POOL_ACCEPTANCE_REASONING").ok(),
        "live-window".into(),
        "turn",
    )
    .await
    .map_err(|error| error.to_string())?;
    let mut changes = 0;
    let persisted = tempfile::NamedTempFile::new().expect("valid test fixture");
    loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(120), session.events.recv())
                .await
                .map_err(|_| "live timeout")?
                .ok_or("stream closed")?
                .map_err(|error| error.to_string())?;
        match event {
            ResponseEvent::OutputItemDone(item) => {
                let call = match &item {
                    ResponseItem::FunctionCall { call_id, name, .. } => {
                        Some((call_id.clone(), name.clone()))
                    }
                    _ => None,
                };
                history.push(item);
                if let Some((call_id, name)) = call {
                    assert_eq!(
                        name, "lookup",
                        "only a fixed inert tool is authorized in this harness"
                    );
                    history.push(serde_json::from_value(json!({"type":"function_call_output", "call_id":call_id, "output":"LOOKUP_OK"})).expect("tool result fixture"));
                    std::fs::write(
                        persisted.path(),
                        serde_json::to_vec(&history).expect("history fixture"),
                    )
                    .expect("persist actual tool result");
                    persisted
                        .as_file()
                        .sync_all()
                        .expect("sync actual tool result");
                    session
                        .results
                        .send(HostCompletion {
                            call_id,
                            output: json!("LOOKUP_OK"),
                            structured: None,
                            disposition: CompletionDisposition::Success,
                        })
                        .await
                        .map_err(|_| "tool host closed")?;
                }
            }
            ResponseEvent::HistoryPersistenceRequested { reply } => {
                std::fs::write(
                    persisted.path(),
                    serde_json::to_vec(&history).expect("valid test fixture"),
                )
                .expect("valid test fixture");
                persisted.as_file().sync_all().expect("valid test fixture");
                let recorded = serde_json::from_slice(
                    &std::fs::read(persisted.path()).expect("valid test fixture"),
                )
                .expect("valid test fixture");
                let _ = reply.send(Ok(Some(recorded)));
            }
            ResponseEvent::ProviderAccountChanged { .. } => changes += 1,
            ResponseEvent::Completed { .. } => return Ok((history, changes)),
            _ => {}
        }
    }
}

#[tokio::test]
#[ignore = "requires authorized live accounts in a dedicated CODEX_POOL_ACCEPTANCE_HOME"]
async fn live_initial_rejection_switch_and_restart_affinity() {
    let home = std::path::PathBuf::from(
        std::env::var("CODEX_POOL_ACCEPTANCE_HOME").expect("dedicated acceptance home"),
    );
    let provider = std::env::var("CODEX_POOL_ACCEPTANCE_PROVIDER").expect("provider id");
    let model = std::env::var("CODEX_POOL_ACCEPTANCE_MODEL").expect("model id");
    let info = codex_model_provider_info::built_in_model_providers(None)
        .remove(&provider)
        .expect("valid test fixture");
    let backend = Arc::new(NativeBackend::new(info.clone(), Some(home.clone())));
    let cancel = CancellationToken::new();
    let status = backend
        .accounts(None, &cancel)
        .await
        .expect("valid test fixture");
    assert!(status.enabled && !status.configured_token);
    let members: Vec<_> = status
        .settings
        .members
        .iter()
        .filter(|entry| entry.enabled)
        .collect();
    assert_eq!(members.len(), 2, "use exactly two authorized accounts");
    let auth = codex_provider_auth::AuthStore::new(&home);
    let first = auth
        .account_reference(&provider, &members[0].label)
        .expect("valid test fixture");
    let second = auth
        .account_reference(&provider, &members[1].label)
        .expect("valid test fixture");
    let selected = auth.selected(&provider).expect("valid test fixture");
    let prefix = uuid::Uuid::new_v4().to_string();
    turn(
        backend.clone(),
        &model,
        &uuid::Uuid::new_v4().to_string(),
        vec![user("Reply with exactly POOL_OK.")],
    )
    .await
    .expect("first account live generation");
    let injected = Arc::new(RejectOnce {
        inner: backend.provider().expect("valid test fixture"),
        account: backend.namespace_account(&first),
        injected: AtomicBool::new(false),
    });
    let switched = Arc::new(NativeBackend::with_provider(
        info.clone(),
        Some(home.clone()),
        injected.clone(),
    ));
    let thread = prefix;
    let (mut history, changes) = turn(
        switched,
        &model,
        &thread,
        vec![user("Reply with exactly POOL_OK.")],
    )
    .await
    .expect("injected pre-dispatch failover");
    assert!(injected.injected.load(Ordering::SeqCst));
    assert_eq!(changes, 1);
    assert_eq!(
        backend
            .pool_store()
            .expect("valid test fixture")
            .affinity(&thread)
            .expect("valid test fixture"),
        Some(second.clone())
    );
    assert_eq!(
        auth.selected(&provider).expect("valid test fixture"),
        selected
    );
    history.push(user("Reply with exactly RESUME_OK."));
    let restarted = Arc::new(NativeBackend::new(info, Some(home)));
    let (_, changes) = turn(restarted.clone(), &model, &thread, history)
        .await
        .expect("restart and clean continuation");
    assert_eq!(changes, 0);
    assert_eq!(
        restarted
            .pool_store()
            .expect("valid test fixture")
            .affinity(&thread)
            .expect("valid test fixture"),
        Some(second)
    );
    eprintln!(
        "LIVE_POOL provider={provider} model={model} first=natural rejection=injected-before-dispatch second=natural restart=passed"
    );
}

struct RejectRound {
    inner: Arc<dyn Provider>,
    account: String,
    injected: Arc<AtomicBool>,
}
impl Provider for RejectRound {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    fn recovery_policy(&self) -> RecoveryPolicy {
        self.inner.recovery_policy()
    }
    fn models(&self, context: Context) -> ProviderFuture<'_, Vec<Model>> {
        self.inner.models(context)
    }
    fn handoff_request(
        &self,
        request: &Request,
        boundary: ContinuationBoundary,
    ) -> Result<Request> {
        self.inner.handoff_request(request, boundary)
    }
    fn start(
        &self,
        request: Request,
        context: Context,
    ) -> ProviderFuture<'_, Box<dyn ProviderSession>> {
        Box::pin(async move {
            let inject = request.namespace.account == self.account;
            let inner = self.inner.start(request, context).await?;
            Ok(Box::new(RejectRoundSession {
                inner,
                inject,
                injected: self.injected.clone(),
            }) as Box<dyn ProviderSession>)
        })
    }
}
struct RejectRoundSession {
    inner: Box<dyn ProviderSession>,
    inject: bool,
    injected: Arc<AtomicBool>,
}
impl ProviderSession for RejectRoundSession {
    fn next_event(&mut self) -> ProviderFuture<'_, Option<Event>> {
        self.inner.next_event()
    }
    fn submit_tool_result(&mut self, result: ToolResult) -> ProviderFuture<'_, ()> {
        self.inner.submit_tool_result(result)
    }
    fn cancel(&mut self) -> ProviderFuture<'_, ()> {
        self.inner.cancel()
    }
    fn continuation_request(&self) -> Result<Request> {
        self.inner.continuation_request()
    }
    fn continue_round(&mut self) -> ProviderFuture<'_, ()> {
        if self.inject && !self.injected.swap(true, Ordering::SeqCst) {
            Box::pin(async {
                Err(ProviderError::Classified(Failure {
                    category: FailureCategory::TemporaryLimit,
                    code: FailureCode::RateLimit,
                    scope: FailureScope::Credential,
                    reset_at: None,
                    retry_at: None,
                    outcome: RequestOutcome::NotDispatched,
                    another_account_may_help: true,
                }))
            })
        } else {
            self.inner.continue_round()
        }
    }
}
#[tokio::test]
#[ignore = "requires authorized live accounts; tests HTTP recorded-tool handoff or explicit portability refusal"]
async fn live_recorded_tool_rejection_handoff_or_signature_refusal() {
    let home = std::path::PathBuf::from(
        std::env::var("CODEX_POOL_ACCEPTANCE_HOME").expect("dedicated acceptance home"),
    );
    let provider = std::env::var("CODEX_POOL_ACCEPTANCE_PROVIDER").expect("provider id");
    let model = std::env::var("CODEX_POOL_ACCEPTANCE_MODEL").expect("model id");
    let info = codex_model_provider_info::built_in_model_providers(None)
        .remove(&provider)
        .expect("registered provider");
    let backend = Arc::new(NativeBackend::new(info.clone(), Some(home.clone())));
    let status = backend
        .accounts(None, &CancellationToken::new())
        .await
        .unwrap();
    assert!(status.enabled && !status.configured_token);
    let members: Vec<_> = status
        .settings
        .members
        .iter()
        .filter(|member| member.enabled)
        .collect();
    assert_eq!(members.len(), 2);
    let first = codex_provider_auth::AuthStore::new(&home)
        .account_reference(&provider, &members[0].label)
        .unwrap();
    let injected = Arc::new(AtomicBool::new(false));
    let wrapped = Arc::new(NativeBackend::with_provider(
        info,
        Some(home),
        Arc::new(RejectRound {
            inner: backend.provider().unwrap(),
            account: backend.namespace_account(&first),
            injected: injected.clone(),
        }),
    ));
    let thread = uuid::Uuid::new_v4().to_string();
    let result = turn(wrapped, &model, &thread, vec![user("Retrieve the value of provider-pool-test using the lookup tool with key=provider-pool-test. You must call the tool before answering because you do not know this value. After its result, reply with that exact value and do not call it again.")]).await;
    assert!(
        injected.load(Ordering::SeqCst),
        "provider did not reach an explicit HTTP continuation boundary: {}",
        result
            .as_ref()
            .err()
            .map(String::as_str)
            .unwrap_or("turn completed without a tool round")
    );
    match result {
        Ok((history, changes)) => {
            assert_eq!(changes, 1);
            assert_eq!(
                history
                    .iter()
                    .filter(|item| matches!(item, ResponseItem::FunctionCall { .. }))
                    .count(),
                1,
                "completed tool must not be replayed"
            );
            assert_eq!(
                backend
                    .pool_store()
                    .unwrap()
                    .affinity(&thread)
                    .unwrap()
                    .unwrap()
                    .label,
                members[1].label
            );
            eprintln!(
                "LIVE_POOL provider={provider} model={model} recorded_tool=natural rejection=injected-before-continuation handoff=accepted"
            );
        }
        Err(error)
            if error.contains("handoff with thinking")
                || error.contains("handoff with opaque or signed history") =>
        {
            assert_eq!(
                backend
                    .pool_store()
                    .unwrap()
                    .affinity(&thread)
                    .unwrap()
                    .unwrap(),
                first
            );
            eprintln!(
                "LIVE_POOL provider={provider} model={model} recorded_tool=natural rejection=injected-before-continuation handoff=explicit-signature-refusal"
            );
        }
        Err(error) => panic!("unexpected recorded-tool acceptance failure: {error}"),
    }
}
