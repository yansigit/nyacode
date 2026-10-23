use codex_provider_api::Checkpoint;
use codex_provider_api::Namespace;
use codex_provider_api::ProviderError;
use codex_provider_api::ToolCall;
use codex_provider_api::ToolResult;
use codex_provider_runtime::SessionStore;
use codex_provider_runtime::ToolDisposition;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn namespace() -> Namespace {
    Namespace {
        provider: "provider-a".into(),
        account: "account-a".into(),
        model: "model-a".into(),
        thread: "thread-a".into(),
    }
}

fn tool_call() -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: "shell".into(),
        arguments: json!({"cmd":"true"}),
    }
}

#[tokio::test]
async fn checkpoint_and_tool_journal_are_isolated_by_full_namespace() {
    let root = tempfile::tempdir().unwrap();
    let cancellation = CancellationToken::new();
    let original = namespace();
    let session = SessionStore::new(root.path(), original.clone());
    let checkpoint = Checkpoint {
        version: 1,
        namespace: original.clone(),
        context_hash: "exact-prefix-digest".into(),
        state: json!({"cursor":"opaque"}),
    };
    session.save(&checkpoint, &cancellation).await.unwrap();
    assert_eq!(session.load().unwrap(), Some(checkpoint.clone()));

    for mutate in [
        |ns: &mut Namespace| ns.provider.push_str("-other"),
        |ns: &mut Namespace| ns.account.push_str("-other"),
        |ns: &mut Namespace| ns.model.push_str("-other"),
        |ns: &mut Namespace| ns.thread.push_str("-other"),
    ] {
        let mut other = original.clone();
        mutate(&mut other);
        let other_session = SessionStore::new(root.path(), other.clone());
        assert!(other_session.load().unwrap().is_none());
        assert!(matches!(
            other_session.save(&checkpoint, &cancellation).await,
            Err(ProviderError::Checkpoint(_))
        ));
        assert!(matches!(
            other_session
                .begin_tool(tool_call(), &cancellation)
                .await
                .unwrap(),
            ToolDisposition::Execute(_)
        ));
    }

    let ToolDisposition::Execute(ticket) = session
        .begin_tool(tool_call(), &cancellation)
        .await
        .unwrap()
    else {
        panic!("a new tool call should execute in its namespace");
    };
    ticket
        .finish(ToolResult {
            call_id: "call-1".into(),
            output: json!("done"),
            is_error: false,
        })
        .unwrap();
    assert!(matches!(
        session
            .begin_tool(tool_call(), &cancellation)
            .await
            .unwrap(),
        ToolDisposition::Replay(_)
    ));
}

#[tokio::test]
async fn checkpoint_scope_changes_at_compaction_but_interrupted_tool_journal_is_shared() {
    let root = tempfile::tempdir().unwrap();
    let cancellation = CancellationToken::new();
    let namespace = namespace();
    let before_compaction = SessionStore::new(root.path(), namespace.clone())
        .with_checkpoint_scope("window-before-compaction".into());
    let checkpoint = Checkpoint {
        version: 1,
        namespace: namespace.clone(),
        context_hash: "pre-compaction-context".into(),
        state: json!({"cursor":"before"}),
    };
    before_compaction
        .save(&checkpoint, &cancellation)
        .await
        .unwrap();

    let ToolDisposition::Execute(ticket) = before_compaction
        .begin_tool(tool_call(), &cancellation)
        .await
        .unwrap()
    else {
        panic!("first tool execution should start");
    };
    drop(ticket); // Simulate interruption before the result is committed.

    let after_compaction = SessionStore::new(root.path(), namespace)
        .with_checkpoint_scope("window-after-compaction".into());
    assert!(after_compaction.load().unwrap().is_none());
    assert!(matches!(
        after_compaction
            .begin_tool(tool_call(), &cancellation)
            .await,
        Err(ProviderError::Checkpoint(message)) if message.contains("refusing automatic replay")
    ));
}

#[tokio::test]
async fn account_handoff_replays_prior_call_ids_but_never_command_text() {
    let home = tempfile::tempdir().unwrap();
    let cancellation = CancellationToken::new();
    let old = SessionStore::new(home.path(), namespace());
    let ToolDisposition::Execute(ticket) =
        old.begin_tool(tool_call(), &cancellation).await.unwrap()
    else {
        panic!("new call");
    };
    ticket
        .finish(ToolResult {
            call_id: "call-1".into(),
            output: json!("denied"),
            is_error: true,
        })
        .unwrap();
    let new = SessionStore::new(
        home.path(),
        Namespace {
            account: "replacement".into(),
            ..namespace()
        },
    )
    .with_prior_journals(vec![namespace()]);
    assert!(matches!(
        new.begin_tool(tool_call(), &cancellation).await.unwrap(),
        ToolDisposition::Replay(ToolResult { is_error: true, .. })
    ));
    let mut conflict = tool_call();
    conflict.arguments = json!({"cmd":"different"});
    assert!(new.begin_tool(conflict, &cancellation).await.is_err());
    let mut distinct = tool_call();
    distinct.id = "call-2".into();
    assert!(matches!(
        new.begin_tool(distinct, &cancellation).await.unwrap(),
        ToolDisposition::Execute(_)
    ));
}
