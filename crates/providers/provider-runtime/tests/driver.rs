use codex_provider_runtime::drive_session;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_provider_api::Checkpoint;
use codex_provider_api::Event;
use codex_provider_api::Host;
use codex_provider_api::Namespace;
use codex_provider_api::ProviderError;
use codex_provider_api::ProviderFuture;
use codex_provider_api::ProviderSession;
use codex_provider_api::Request;
use codex_provider_api::ToolCall;
use codex_provider_api::ToolDefinition;
use codex_provider_api::ToolResult;
use serde_json::Value;
use serde_json::json;
use tokio::sync::Barrier;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Session {
    incoming: mpsc::Receiver<Event>,
    results: mpsc::UnboundedSender<ToolResult>,
    cancelled: Arc<AtomicBool>,
}

impl ProviderSession for Session {
    fn next_event(&mut self) -> ProviderFuture<'_, Option<Event>> {
        Box::pin(async move { Ok(self.incoming.recv().await) })
    }

    fn submit_tool_result(&mut self, result: ToolResult) -> ProviderFuture<'_, ()> {
        Box::pin(async move {
            self.results
                .send(result)
                .map_err(|_| ProviderError::Transport("result receiver closed".into()))
        })
    }

    fn cancel(&mut self) -> ProviderFuture<'_, ()> {
        self.cancelled.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

struct ParallelHost {
    both_started: Arc<Barrier>,
    started: Arc<Mutex<Vec<String>>>,
}

impl Host for ParallelHost {
    fn execute(
        &self,
        call: ToolCall,
        cancellation: CancellationToken,
    ) -> ProviderFuture<'_, ToolResult> {
        let both_started = Arc::clone(&self.both_started);
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            started.lock().await.push(call.id.clone());
            both_started.wait().await;
            if cancellation.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            Ok(ToolResult {
                call_id: call.id,
                output: json!("ok"),
                is_error: false,
            })
        })
    }

    fn checkpoint(&self, _: Checkpoint) -> ProviderFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

struct WaitingHost {
    started: Arc<AtomicBool>,
    checkpoint_started: Arc<AtomicBool>,
}

impl Host for WaitingHost {
    fn execute(
        &self,
        _: ToolCall,
        cancellation: CancellationToken,
    ) -> ProviderFuture<'_, ToolResult> {
        let started = Arc::clone(&self.started);
        Box::pin(async move {
            started.store(true, Ordering::SeqCst);
            cancellation.cancelled().await;
            Err(ProviderError::Cancelled)
        })
    }

    fn checkpoint(&self, _: Checkpoint) -> ProviderFuture<'_, ()> {
        let started = Arc::clone(&self.checkpoint_started);
        Box::pin(async move {
            started.store(true, Ordering::SeqCst);
            std::future::pending().await
        })
    }
}

fn namespace() -> Namespace {
    Namespace {
        provider: "test-provider".into(),
        account: "account-a".into(),
        model: "model-a".into(),
        thread: "thread-a".into(),
    }
}

fn request() -> Request {
    Request {
        namespace: namespace(),
        system: String::new(),
        messages: vec![],
        tools: vec![ToolDefinition {
            name: "test_tool".into(),
            description: String::new(),
            parameters: json!({}),
        }],
        parallel_tool_calls: true,
        max_output_tokens: None,
        reasoning_effort: None,
        output_schema: None,
        checkpoint: None,
        metadata: Value::Null,
    }
}

fn call(id: &str) -> Event {
    Event::ToolCall(ToolCall {
        id: id.into(),
        name: "test_tool".into(),
        arguments: json!({"id": id}),
    })
}

fn session_pair() -> (
    Session,
    mpsc::Sender<Event>,
    mpsc::UnboundedReceiver<ToolResult>,
    Arc<AtomicBool>,
) {
    let (events_tx, events_rx) = mpsc::channel(8);
    let (results_tx, results_rx) = mpsc::unbounded_channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    (
        Session {
            incoming: events_rx,
            results: results_tx,
            cancelled: Arc::clone(&cancelled),
        },
        events_tx,
        results_rx,
        cancelled,
    )
}

struct SerialHost {
    started: mpsc::UnboundedSender<String>,
    release_first: Arc<Notify>,
}

impl Host for SerialHost {
    fn execute(&self, call: ToolCall, _: CancellationToken) -> ProviderFuture<'_, ToolResult> {
        let started = self.started.clone();
        let release_first = Arc::clone(&self.release_first);
        Box::pin(async move {
            started
                .send(call.id.clone())
                .map_err(|_| ProviderError::Host("test host observer closed".into()))?;
            if call.id == "one" {
                release_first.notified().await;
            }
            Ok(ToolResult {
                call_id: call.id,
                output: json!("ok"),
                is_error: false,
            })
        })
    }

    fn checkpoint(&self, _: Checkpoint) -> ProviderFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

struct FastHost;

impl Host for FastHost {
    fn execute(&self, call: ToolCall, _: CancellationToken) -> ProviderFuture<'_, ToolResult> {
        Box::pin(async move {
            Ok(ToolResult {
                call_id: call.id,
                output: json!("ok"),
                is_error: false,
            })
        })
    }

    fn checkpoint(&self, _: Checkpoint) -> ProviderFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

struct HangingSubmitSession {
    incoming: mpsc::Receiver<Event>,
    submit_started: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl ProviderSession for HangingSubmitSession {
    fn next_event(&mut self) -> ProviderFuture<'_, Option<Event>> {
        Box::pin(async move { Ok(self.incoming.recv().await) })
    }

    fn submit_tool_result(&mut self, _: ToolResult) -> ProviderFuture<'_, ()> {
        let started = Arc::clone(&self.submit_started);
        Box::pin(async move {
            started.store(true, Ordering::SeqCst);
            std::future::pending().await
        })
    }

    fn cancel(&mut self) -> ProviderFuture<'_, ()> {
        self.cancelled.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn eof_without_completed_is_a_protocol_error() {
    let (session, events_tx, _results, cancelled) = session_pair();
    drop(events_tx);
    let (output_tx, _output_rx) = mpsc::channel(8);
    let result = drive_session(
        Box::new(session),
        Arc::new(WaitingHost {
            started: Arc::new(AtomicBool::new(false)),
            checkpoint_started: Arc::new(AtomicBool::new(false)),
        }),
        &request(),
        CancellationToken::new(),
        output_tx,
    )
    .await;
    assert!(
        matches!(result, Err(ProviderError::Protocol(message)) if message.contains("without a terminal event"))
    );
    assert!(cancelled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn terminal_event_while_tool_is_pending_is_rejected_and_cancels_tool() {
    let (session, events_tx, _results, cancelled) = session_pair();
    let host_started = Arc::new(AtomicBool::new(false));
    let host = Arc::new(WaitingHost {
        started: Arc::clone(&host_started),
        checkpoint_started: Arc::new(AtomicBool::new(false)),
    });
    let (output_tx, mut output_rx) = mpsc::channel(8);
    let drive = tokio::spawn(async move {
        drive_session(
            Box::new(session),
            host,
            &request(),
            CancellationToken::new(),
            output_tx,
        )
        .await
    });
    events_tx.send(call("one")).await.unwrap();
    assert!(matches!(
        output_rx.recv().await.unwrap().unwrap(),
        Event::ToolCall(_)
    ));
    tokio::time::timeout(Duration::from_secs(1), async {
        while !host_started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    events_tx
        .send(Event::Completed { id: "early".into() })
        .await
        .unwrap();
    assert!(
        matches!(drive.await.unwrap(), Err(ProviderError::Protocol(message)) if message.contains("tools are pending"))
    );
    assert!(cancelled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn parallel_calls_execute_concurrently_and_return_individual_results() {
    let (session, events_tx, mut results_rx, cancelled) = session_pair();
    events_tx.send(call("one")).await.unwrap();
    events_tx.send(call("two")).await.unwrap();
    let host_started = Arc::new(Mutex::new(Vec::new()));
    let host = Arc::new(ParallelHost {
        both_started: Arc::new(Barrier::new(2)),
        started: Arc::clone(&host_started),
    });
    let (output_tx, mut output_rx) = mpsc::channel(8);
    let cancellation = CancellationToken::new();
    let drive = tokio::spawn(async move {
        drive_session(Box::new(session), host, &request(), cancellation, output_tx).await
    });
    let first = tokio::time::timeout(Duration::from_secs(2), results_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(2), results_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let mut ids = vec![first.call_id, second.call_id];
    ids.sort();
    assert_eq!(ids, ["one", "two"]);
    events_tx
        .send(Event::Completed { id: "done".into() })
        .await
        .unwrap();
    assert!(matches!(
        output_rx.recv().await.unwrap().unwrap(),
        Event::ToolCall(_)
    ));
    assert!(matches!(
        output_rx.recv().await.unwrap().unwrap(),
        Event::ToolCall(_)
    ));
    assert_eq!(
        output_rx.recv().await.unwrap().unwrap(),
        Event::Completed { id: "done".into() }
    );
    drive.await.unwrap().unwrap();
    assert_eq!(host_started.lock().await.len(), 2);
    assert!(!cancelled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn parallel_disabled_serializes_host_calls_until_first_result_is_submitted() {
    let (session, events_tx, mut results_rx, cancelled) = session_pair();
    events_tx.send(call("one")).await.unwrap();
    events_tx.send(call("two")).await.unwrap();
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let release_first = Arc::new(Notify::new());
    let host = Arc::new(SerialHost {
        started: started_tx,
        release_first: Arc::clone(&release_first),
    });
    let (output_tx, _output_rx) = mpsc::channel(8);
    let mut serial_request = request();
    serial_request.parallel_tool_calls = false;
    let drive = tokio::spawn(async move {
        drive_session(
            Box::new(session),
            host,
            &serial_request,
            CancellationToken::new(),
            output_tx,
        )
        .await
    });

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        "one"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), started_rx.recv())
            .await
            .is_err()
    );

    release_first.notify_one();
    assert_eq!(results_rx.recv().await.unwrap().call_id, "one");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        "two"
    );
    assert_eq!(results_rx.recv().await.unwrap().call_id, "two");
    events_tx
        .send(Event::Completed { id: "done".into() })
        .await
        .unwrap();
    drive.await.unwrap().unwrap();
    assert!(!cancelled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn dropping_event_consumer_cancels_a_hanging_tool_result_submission() {
    let (events_tx, incoming) = mpsc::channel(8);
    let submit_started = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));
    let session = HangingSubmitSession {
        incoming,
        submit_started: Arc::clone(&submit_started),
        cancelled: Arc::clone(&cancelled),
    };
    let (output_tx, mut output_rx) = mpsc::channel(8);
    let drive = tokio::spawn(async move {
        drive_session(
            Box::new(session),
            Arc::new(FastHost),
            &request(),
            CancellationToken::new(),
            output_tx,
        )
        .await
    });
    events_tx.send(call("one")).await.unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), output_rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Event::ToolCall(_)
    ));
    tokio::time::timeout(Duration::from_secs(1), async {
        while !submit_started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(output_rx);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), drive)
            .await
            .unwrap()
            .unwrap(),
        Err(ProviderError::Cancelled)
    ));
    assert!(cancelled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn cancellation_while_host_waits_for_tool_cancels_session_and_work() {
    let (session, events_tx, _results, cancelled) = session_pair();
    let host_started = Arc::new(AtomicBool::new(false));
    let host = Arc::new(WaitingHost {
        started: Arc::clone(&host_started),
        checkpoint_started: Arc::new(AtomicBool::new(false)),
    });
    let (output_tx, mut output_rx) = mpsc::channel(8);
    let cancellation = CancellationToken::new();
    let driver_cancel = cancellation.clone();
    let drive = tokio::spawn(async move {
        drive_session(
            Box::new(session),
            host,
            &request(),
            driver_cancel,
            output_tx,
        )
        .await
    });
    events_tx.send(call("one")).await.unwrap();
    assert!(matches!(
        output_rx.recv().await.unwrap().unwrap(),
        Event::ToolCall(_)
    ));
    tokio::time::timeout(Duration::from_secs(1), async {
        while !host_started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancellation.cancel();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), drive)
            .await
            .unwrap()
            .unwrap(),
        Err(ProviderError::Cancelled)
    ));
    assert!(cancelled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn cancellation_interrupts_a_waiting_checkpoint_persist() {
    let (session, events_tx, _results, cancelled) = session_pair();
    let checkpoint_started = Arc::new(AtomicBool::new(false));
    let host = Arc::new(WaitingHost {
        started: Arc::new(AtomicBool::new(false)),
        checkpoint_started: Arc::clone(&checkpoint_started),
    });
    let (output_tx, _output_rx) = mpsc::channel(8);
    let cancellation = CancellationToken::new();
    let driver_cancel = cancellation.clone();
    let mut drive = tokio::spawn(async move {
        drive_session(
            Box::new(session),
            host,
            &request(),
            driver_cancel,
            output_tx,
        )
        .await
    });
    events_tx
        .send(Event::Checkpoint(Checkpoint {
            version: 1,
            namespace: namespace(),
            context_hash: "prefix".into(),
            state: json!({"opaque": true}),
        }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !checkpoint_started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancellation.cancel();
    let result = tokio::time::timeout(Duration::from_millis(200), &mut drive).await;
    if result.is_err() {
        drive.abort();
    }
    assert!(matches!(result, Ok(Ok(Err(ProviderError::Cancelled)))));
    assert!(cancelled.load(Ordering::SeqCst));
}
