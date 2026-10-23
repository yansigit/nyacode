use codex_provider_api::Checkpoint;
use codex_provider_api::Namespace;
use codex_provider_api::ToolCall;
use codex_provider_api::ToolResult;
use codex_provider_auth::AuthStore;
use codex_provider_auth::Store;
use codex_provider_auth::TokenRecord;
use codex_provider_auth::digest;
use codex_provider_runtime::SessionStore;
use codex_provider_runtime::ToolDisposition;
use serde_json::json;
use std::fs;
use std::path::Path;
use tokio_util::sync::CancellationToken;

const SELECTED_KEY: &str = "[\"selected\",\"fixture-provider\"]";
const CREDENTIAL_KEY: &str = "[\"credentials\",\"fixture-provider\",\"fixture-account\"]";

fn fixture(name: &str) -> Vec<u8> {
    let mut bytes = match name {
        "identities.json" => include_bytes!("fixtures/compatibility-v1/identities.json").as_slice(),
        "selected.json" => include_bytes!("fixtures/compatibility-v1/selected.json").as_slice(),
        "credentials.json" => {
            include_bytes!("fixtures/compatibility-v1/credentials.json").as_slice()
        }
        "checkpoint.json" => include_bytes!("fixtures/compatibility-v1/checkpoint.json").as_slice(),
        "journal.json" => include_bytes!("fixtures/compatibility-v1/journal.json").as_slice(),
        "execution.json" => include_bytes!("fixtures/compatibility-v1/execution.json").as_slice(),
        _ => unreachable!("unknown compatibility fixture"),
    }
    .to_vec();
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    bytes
}

fn namespace() -> Namespace {
    Namespace {
        provider: "fixture-provider".into(),
        account: "fixture-account".into(),
        model: "fixture-model".into(),
        thread: "fixture-thread".into(),
    }
}

fn baseline_key(value: serde_json::Value) -> String {
    let mut value = value;
    value.sort_all_objects();
    value.to_string()
}

fn install_fixture(root: &Path, key: &str, bytes: &[u8]) -> std::io::Result<()> {
    let directory = root.join("native-providers");
    fs::create_dir_all(&directory)?;
    fs::write(
        directory.join(format!("{}.json", digest(key.as_bytes()))),
        bytes,
    )
}

fn expected_identities() -> serde_json::Result<serde_json::Value> {
    serde_json::from_slice(&fixture("identities.json"))
}

#[tokio::test]
async fn baseline_persisted_state_fixtures_remain_readable_and_writable() {
    let read_root = tempfile::tempdir().unwrap();
    install_fixture(read_root.path(), SELECTED_KEY, &fixture("selected.json")).unwrap();
    install_fixture(
        read_root.path(),
        CREDENTIAL_KEY,
        &fixture("credentials.json"),
    )
    .unwrap();
    let auth = AuthStore::new(read_root.path());
    assert_eq!(
        auth.selected("fixture-provider").unwrap().as_deref(),
        Some("fixture-account")
    );
    assert_eq!(
        auth.identity("fixture-provider").unwrap().as_deref(),
        Some("fixture-account:fixture-generation")
    );
    assert_eq!(
        auth.status("fixture-provider").unwrap().unwrap().expires_at,
        Some(1_900_000_000)
    );

    let ns = namespace();
    let checkpoint_key = baseline_key(json!(("checkpoint", &ns, "latest")));
    install_fixture(
        read_root.path(),
        &checkpoint_key,
        &fixture("checkpoint.json"),
    )
    .unwrap();
    let session = SessionStore::new(read_root.path(), ns.clone());
    assert_eq!(
        session.load().unwrap().unwrap().context_hash,
        "fixture-context-digest"
    );
    assert_eq!(
        session.load().unwrap().unwrap().state["codex_host_lineage"]["length"],
        1
    );
    let tool_key = baseline_key(json!(("tool", &ns, "fixture-call-1")));
    install_fixture(read_root.path(), &tool_key, &fixture("journal.json")).unwrap();
    let call = ToolCall {
        id: "fixture-call-1".into(),
        name: "shell".into(),
        arguments: json!({"cmd":"true"}),
    };
    let ToolDisposition::Replay(result) = session
        .begin_tool(call, &CancellationToken::new())
        .await
        .unwrap()
    else {
        panic!("fixture tool result should replay");
    };
    assert_eq!(result.output, json!("fixture-result"));

    let writer_root = tempfile::tempdir().unwrap();
    let store = Store::new(writer_root.path());
    let cancellation = CancellationToken::new();
    let selected = store.lock(SELECTED_KEY, &cancellation).await.unwrap();
    selected.write(&"fixture-account").unwrap();
    drop(selected);
    let credentials = store.lock(CREDENTIAL_KEY, &cancellation).await.unwrap();
    credentials
        .write(&TokenRecord {
            access_token: "fixture-access-token-not-valid".into(),
            refresh_token: Some("fixture-refresh-token-not-valid".into()),
            expires_at: Some(1_900_000_000),
            metadata: json!({"codex_account_generation":"fixture-generation", "tenant":"fixture-tenant"}),
            revoked: false,
        })
        .unwrap();
    drop(credentials);

    let ns = namespace();
    let checkpoint = Checkpoint {
        version: 1,
        namespace: ns.clone(),
        context_hash: "fixture-context-digest".into(),
        state: json!({
            "codex_host_lineage": {
                "failed_results": {"fixture-call-1":"fixture-failed-output-digest"},
                "hash":"fixture-host-prefix-digest",
                "length":1
            },
            "codex_host_pending":false,
            "cursor":"fixture-cursor",
            "opaque_state":{"thinking_signature":"fixture-signature-not-valid"}
        }),
    };
    let session = SessionStore::new(writer_root.path(), ns.clone());
    session.save(&checkpoint, &cancellation).await.unwrap();
    let call = ToolCall {
        id: "fixture-call-1".into(),
        name: "shell".into(),
        arguments: json!({"cmd":"true"}),
    };
    let ToolDisposition::Execute(ticket) = session.begin_tool(call, &cancellation).await.unwrap()
    else {
        panic!("new fixture tool call should execute");
    };
    ticket
        .finish(ToolResult {
            call_id: "fixture-call-1".into(),
            output: json!("fixture-result"),
            is_error: false,
        })
        .unwrap();

    let output = writer_root.path().join("native-providers");
    assert_eq!(
        fs::read(output.join(format!(
            "{}.json",
            expected_identities().unwrap()["selected"].as_str().unwrap()
        )))
        .unwrap(),
        fixture("selected.json")
    );
    assert_eq!(
        fs::read(output.join(format!(
            "{}.json",
            expected_identities().unwrap()["credentials"].as_str().unwrap()
        )))
        .unwrap(),
        fixture("credentials.json")
    );
    assert_eq!(
        fs::read(output.join(format!(
            "{}.json",
            expected_identities().unwrap()["checkpoint"].as_str().unwrap()
        )))
        .unwrap(),
        fixture("checkpoint.json")
    );
    assert_eq!(
        fs::read(output.join(format!(
            "{}.json",
            expected_identities().unwrap()["tool"].as_str().unwrap()
        )))
        .unwrap(),
        fixture("journal.json")
    );

    let lock_names: Vec<_> = fs::read_dir(output)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".lock"))
        .collect();
    let mut expected_lock_names = ["selected", "credentials", "checkpoint", "tool"].map(|key| {
        format!(
            "{}.lock",
            expected_identities().unwrap()[key].as_str().unwrap()
        )
    });
    expected_lock_names.sort();
    let mut actual_lock_names = lock_names;
    actual_lock_names.sort();
    assert_eq!(actual_lock_names, expected_lock_names);
}

#[tokio::test]
async fn provider_namespaces_and_interrupted_execution_keep_baseline_identity() {
    let identities = expected_identities().unwrap();
    for provider in ["cursor", "opencode-go", "claude", "antigravity"] {
        let mut ns = namespace();
        ns.provider = provider.into();
        let provider_ids = &identities["providers"][provider];
        let keys = [
            (
                "selected",
                serde_json::json!(("selected", provider)).to_string(),
            ),
            (
                "credentials",
                serde_json::json!(("credentials", provider, "fixture-account")).to_string(),
            ),
            (
                "checkpoint",
                baseline_key(json!(("checkpoint", &ns, "latest"))),
            ),
            ("tool", baseline_key(json!(("tool", &ns, "fixture-call-1")))),
        ];
        for (kind, key) in keys {
            assert_eq!(
                digest(key.as_bytes()),
                provider_ids[kind].as_str().unwrap(),
                "{provider} {kind}"
            );
        }
    }

    let root = tempfile::tempdir().unwrap();
    let cancellation = CancellationToken::new();
    let store = SessionStore::new(root.path(), namespace());
    let lease = store.begin_execution("turn", &cancellation).await.unwrap();
    lease.mark_tools_started().unwrap();
    drop(lease);

    let execution_key = serde_json::json!(("execution", "fixture-thread", "turn")).to_string();
    assert_eq!(
        digest(execution_key.as_bytes()),
        identities["execution"].as_str().unwrap()
    );
    let execution_path = root.path().join("native-providers").join(format!(
        "{}.json",
        identities["execution"].as_str().unwrap()
    ));
    assert_eq!(fs::read(execution_path).unwrap(), fixture("execution.json"));

    let changed_provider = SessionStore::new(
        root.path(),
        Namespace {
            provider: "claude".into(),
            account: "another-account".into(),
            model: "another-model".into(),
            thread: "fixture-thread".into(),
        },
    );
    assert!(
        changed_provider
            .begin_execution("turn", &cancellation)
            .await
            .is_err()
    );
}
