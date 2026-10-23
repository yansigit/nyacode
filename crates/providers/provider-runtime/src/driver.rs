use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use codex_provider_api::*;

/// Drives a bidirectional session without closing the transport while tools run.
/// Dropping the consumer cancels both upstream work and host operations.
pub async fn drive_session(
    mut session: Box<dyn ProviderSession>,
    host: Arc<dyn Host>,
    request: &Request,
    cancellation: CancellationToken,
    events: mpsc::Sender<Result<Event>>,
) -> Result<()> {
    let calls_cancel = cancellation.child_token();
    let mut pending = FuturesUnordered::new();
    let mut queued = VecDeque::<ToolCall>::new();
    let mut active_ids = HashSet::new();
    let mut completed = false;
    let mut namespace = request.namespace.clone();
    let result = async {
        loop {
            while (request.parallel_tool_calls || pending.is_empty()) && let Some(call) = queued.pop_front() {
                let host = Arc::clone(&host);
                let token = calls_cancel.child_token();
                pending.push(async move {
                    let id = call.id.clone();
                    let result = host.execute(call, token).await;
                    (id, result)
                });
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ProviderError::Cancelled),
                _ = events.closed() => return Err(ProviderError::Cancelled),
                result = pending.next(), if !pending.is_empty() => {
                    let Some((id, result)): Option<(String, Result<ToolResult>)> = result else { continue };
                    active_ids.remove(&id);
                    let result = result?;
                    if result.call_id != id {
                        return Err(ProviderError::Host("tool result ID does not match call".into()));
                    }
                    tokio::select! {
                        _ = cancellation.cancelled() => return Err(ProviderError::Cancelled),
                        _ = events.closed() => return Err(ProviderError::Cancelled),
                        result = session.submit_tool_result(result) => result?,
                    }
                }
                event = session.next_event() => {
                    let event = event?.ok_or_else(|| ProviderError::Protocol("stream ended without a terminal event".into()))?;
                    match &event {
                        Event::ToolCall(call) => {
                            if call.id.is_empty() || !request.tools.iter().any(|tool| tool.name == call.name) {
                                return Err(ProviderError::Protocol("provider requested an unadvertised tool or empty call ID".into()));
                            }
                            if !active_ids.insert(call.id.clone()) || active_ids.len() > 128 {
                                return Err(ProviderError::Protocol("duplicate active tool ID or too many pending tools".into()));
                            }
                            queued.push_back(call.clone());
                        }
                        Event::Checkpoint(checkpoint) => {
                            if checkpoint.version != 1 || checkpoint.namespace != namespace {
                                return Err(ProviderError::Checkpoint("checkpoint namespace/version mismatch".into()));
                            }
                            tokio::select! {
                                _ = cancellation.cancelled() => return Err(ProviderError::Cancelled),
                                _ = events.closed() => return Err(ProviderError::Cancelled),
                                result = host.checkpoint(checkpoint.clone()) => result?,
                            }
                        }
                        Event::ContinuationReady => {
                            if !pending.is_empty() || !queued.is_empty() || !active_ids.is_empty() {
                                return Err(ProviderError::Protocol("continuation while host work is pending".into()));
                            }
                            let history = host.persisted_history().await?;
                            session.authorize_continuation(history).await?;
                            continue;
                        }
                        Event::AccountChanged { namespace: next, prior_journals, .. } => {
                            if !pending.is_empty() || !queued.is_empty() || next.provider != namespace.provider || next.model != namespace.model || next.thread != namespace.thread {
                                return Err(ProviderError::Checkpoint("invalid account handoff boundary".into()));
                            }
                            host.account_changed(next.clone(), prior_journals.clone()).await?;
                            namespace = next.clone();
                        }
                        Event::Completed { .. } => {
                            if !pending.is_empty() || !queued.is_empty() {
                                return Err(ProviderError::Protocol("terminal event while tools are pending".into()));
                            }
                            completed = true;
                        }
                        _ => {}
                    }
                    tokio::select! {
                        _ = cancellation.cancelled() => return Err(ProviderError::Cancelled),
                        sent = events.send(Ok(event)) => sent.map_err(|_| ProviderError::Cancelled)?,
                    }
                    if completed { return Ok(()); }
                }
            }
        }
    }.await;
    calls_cancel.cancel();
    if !completed || result.is_err() {
        // A misbehaving remote must not make cancellation wait indefinitely.
        let _ = tokio::time::timeout(Duration::from_secs(2), session.cancel()).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use tokio::sync::Mutex;

    struct TestHost;
    impl Host for TestHost {
        fn execute(&self, call: ToolCall, _: CancellationToken) -> ProviderFuture<'_, ToolResult> {
            Box::pin(async move {
                Ok(ToolResult {
                    call_id: call.id,
                    output: json!(42),
                    is_error: false,
                })
            })
        }
        fn checkpoint(&self, _: Checkpoint) -> ProviderFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }
    struct Session {
        receiver: mpsc::Receiver<Event>,
        sent: mpsc::Sender<Event>,
        results: Arc<Mutex<Vec<ToolResult>>>,
        cancelled: Arc<AtomicBool>,
    }
    impl ProviderSession for Session {
        fn next_event(&mut self) -> ProviderFuture<'_, Option<Event>> {
            Box::pin(async { Ok(self.receiver.recv().await) })
        }
        fn submit_tool_result(&mut self, result: ToolResult) -> ProviderFuture<'_, ()> {
            Box::pin(async move {
                self.results.lock().await.push(result);
                self.sent
                    .send(Event::TextDelta("continued".into()))
                    .await
                    .unwrap();
                self.sent
                    .send(Event::Completed {
                        id: "response".into(),
                    })
                    .await
                    .unwrap();
                Ok(())
            })
        }
        fn cancel(&mut self) -> ProviderFuture<'_, ()> {
            self.cancelled.store(true, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }
    fn request() -> Request {
        Request {
            namespace: Namespace {
                provider: "mock".into(),
                account: "a".into(),
                model: "m".into(),
                thread: "t".into(),
            },
            system: String::new(),
            messages: vec![],
            tools: vec![ToolDefinition {
                name: "test".into(),
                description: String::new(),
                parameters: json!({}),
            }],
            parallel_tool_calls: true,
            max_output_tokens: None,
            reasoning_effort: None,
            output_schema: None,
            checkpoint: None,
            metadata: json!({}),
        }
    }
    #[tokio::test]
    async fn tools_return_on_the_same_session_before_continuation() {
        let (sent, receiver) = mpsc::channel(8);
        sent.send(Event::ToolCall(ToolCall {
            id: "call".into(),
            name: "test".into(),
            arguments: json!({}),
        }))
        .await
        .unwrap();
        let results = Arc::new(Mutex::new(vec![]));
        let cancelled = Arc::new(AtomicBool::new(false));
        let session = Session {
            receiver,
            sent,
            results: results.clone(),
            cancelled: cancelled.clone(),
        };
        let (events, mut output) = mpsc::channel(8);
        drive_session(
            Box::new(session),
            Arc::new(TestHost),
            &request(),
            CancellationToken::new(),
            events,
        )
        .await
        .unwrap();
        assert!(matches!(
            output.recv().await.unwrap().unwrap(),
            Event::ToolCall(_)
        ));
        assert_eq!(
            output.recv().await.unwrap().unwrap(),
            Event::TextDelta("continued".into())
        );
        assert!(matches!(
            output.recv().await.unwrap().unwrap(),
            Event::Completed { .. }
        ));
        assert_eq!(results.lock().await.len(), 1);
        assert!(!cancelled.load(Ordering::SeqCst));
    }
    #[tokio::test]
    async fn cancellation_closes_a_waiting_session() {
        let (sent, receiver) = mpsc::channel(8);
        let cancelled = Arc::new(AtomicBool::new(false));
        let session = Session {
            receiver,
            sent,
            results: Arc::new(Mutex::new(vec![])),
            cancelled: cancelled.clone(),
        };
        let token = CancellationToken::new();
        token.cancel();
        let (events, _output) = mpsc::channel(8);
        assert!(matches!(
            drive_session(
                Box::new(session),
                Arc::new(TestHost),
                &request(),
                token,
                events
            )
            .await,
            Err(ProviderError::Cancelled)
        ));
        assert!(cancelled.load(Ordering::SeqCst));
    }
    #[tokio::test]
    async fn unadvertised_tools_never_reach_host() {
        let (sent, receiver) = mpsc::channel(8);
        sent.send(Event::ToolCall(ToolCall {
            id: "call".into(),
            name: "unavailable".into(),
            arguments: json!({}),
        }))
        .await
        .unwrap();
        let session = Session {
            receiver,
            sent,
            results: Arc::new(Mutex::new(vec![])),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let (events, _output) = mpsc::channel(8);
        assert!(matches!(
            drive_session(
                Box::new(session),
                Arc::new(TestHost),
                &request(),
                CancellationToken::new(),
                events
            )
            .await,
            Err(ProviderError::Protocol(_))
        ));
    }
}
