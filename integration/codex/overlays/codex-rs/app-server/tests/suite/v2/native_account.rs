use anyhow::Result;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_provider_adapter::auth::AuthStore;
use codex_provider_adapter::auth::TokenRecord;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const READ_TIMEOUT: Duration = Duration::from_secs(30);
const API_KEY: &str = "dummyfixturetoken";

fn write_claude_config(codex_home: &std::path::Path) -> Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        r#"
model = "claude-haiku-5-5"
model_provider = "claude_native_fixture"
approval_policy = "never"
sandbox_mode = "danger-full-access"

[model_providers.claude_native_fixture]
name = "Claude native provider test"
wire_api = "claude"
env_key = "ANTHROPIC_API_KEY"
"#,
    )?;
    Ok(())
}

async fn login_claude_with_token(server: &mut TestAppServer) -> Result<()> {
    let request_id = server
        .send_login_account_request(json!({
            "type": "native",
            "provider": "claude",
            "accountId": "default",
            "apiKey": API_KEY,
        }))
        .await?;
    let response: Value = timeout(READ_TIMEOUT, server.read_response(request_id)).await??;
    assert_eq!(response["type"], "native");
    assert_eq!(response["provider"], "claude");
    assert!(response["loginId"].is_null());

    let completed = server
        .read_stream_until_notification_message("account/login/completed")
        .await?;
    let params = completed.params.expect("login completion params");
    assert_eq!(params["success"], true);
    assert!(params["loginId"].is_null());

    let updated = server
        .read_stream_until_notification_message("account/updated")
        .await?;
    let params = updated.params.expect("account update params");
    assert!(params["authMode"].is_null());
    Ok(())
}

fn seed_token(access_token: &str) -> TokenRecord {
    TokenRecord {
        access_token: access_token.into(),
        refresh_token: None,
        expires_at: None,
        metadata: json!({}),
        revoked: false,
    }
}

#[tokio::test]
async fn native_token_login_read_and_logout_are_provider_scoped() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_claude_config(codex_home.path())?;

    let auth_store = AuthStore::new(codex_home.path());
    auth_store
        .login(
            "opencode-go",
            "default",
            seed_token("go-dummyfixturetoken"),
            &CancellationToken::new(),
        )
        .await?;

    let mut server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("ANTHROPIC_API_KEY", None), ("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(READ_TIMEOUT)
        .await?;

    login_claude_with_token(&mut server).await?;

    let request_id = server
        .send_get_account_request(codex_app_server_protocol::GetAccountParams {
            refresh_token: false,
        })
        .await?;
    let account: Value = timeout(READ_TIMEOUT, server.read_response(request_id)).await??;
    assert_eq!(account["account"]["type"], "native");
    assert_eq!(account["account"]["provider"], "claude");
    assert_eq!(account["account"]["accountId"], "default");

    let request_id = server.send_logout_account_request().await?;
    let response: Value = timeout(READ_TIMEOUT, server.read_response(request_id)).await??;
    assert_eq!(response, json!({}));
    let updated = server
        .read_stream_until_notification_message("account/updated")
        .await?;
    assert!(updated.params.expect("account update params")["authMode"].is_null());

    assert_eq!(auth_store.status("claude")?, None);
    assert_eq!(
        auth_store
            .status("opencode-go")?
            .map(|status| status.account),
        Some("default".into())
    );
    Ok(())
}

#[tokio::test]
async fn environment_token_override_rejects_logout_without_deleting_stored_account() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_claude_config(codex_home.path())?;

    let auth_store = AuthStore::new(codex_home.path());
    auth_store
        .login(
            "claude",
            "default",
            seed_token("stored-dummyfixturetoken"),
            &CancellationToken::new(),
        )
        .await?;

    let mut server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[
            ("ANTHROPIC_API_KEY", Some(API_KEY)),
            ("OPENAI_API_KEY", None),
        ])
        .build_initialized_with_timeout(READ_TIMEOUT)
        .await?;

    let request_id = server.send_logout_account_request().await?;
    let error = timeout(
        READ_TIMEOUT,
        server.read_stream_until_error_message(codex_app_server_protocol::RequestId::Integer(
            request_id,
        )),
    )
    .await??;
    assert!(
        error
            .error
            .message
            .contains("configured by an environment variable")
    );
    assert_eq!(
        auth_store.status("claude")?.map(|status| status.account),
        Some("default".into())
    );
    Ok(())
}

#[tokio::test]
async fn native_provider_selection_survives_fork_and_cold_resume() -> Result<()> {
    let codex_home = TempDir::new()?;
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string("data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"saved\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"))
        .expect(1)
        .mount(&upstream).await;
    std::fs::write(
        codex_home.path().join("config.toml"),
        format!(
            "model = \"gpt-5.6-luna\"\n[model_providers.go_native_fixture]\nname = \"Native fixture\"\nwire_api = \"opencode-go\"\nbase_url = {:?}\n",
            upstream.uri()
        ),
    )?;
    AuthStore::new(codex_home.path())
        .login(
            "opencode-go",
            "default",
            seed_token("go-dummyfixturetoken"),
            &CancellationToken::new(),
        )
        .await?;

    let mut server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(READ_TIMEOUT)
        .await?;

    let started: ThreadStartResponse = timeout(
        READ_TIMEOUT,
        server.start_thread(ThreadStartParams {
            model: Some("glm-5.3".into()),
            model_provider: Some("go_native_fixture".into()),
            ..Default::default()
        }),
    )
    .await??;
    assert_eq!(started.model_provider, "go_native_fixture");
    assert_eq!(started.model, "glm-5.3");

    // Account management must use the thread's configured alias and endpoint,
    // even while the app-server's default provider is different.
    let accounts_id = server
        .send_request(
            "provider/accounts/list",
            Some(json!({"provider":"opencode-go", "threadId":started.thread.id})),
        )
        .await?;
    let scoped: Value = timeout(READ_TIMEOUT, server.read_response(accounts_id)).await??;
    assert_eq!(scoped["endpoint"], upstream.uri());

    // Empty threads have no durable rollout. Persist a real native turn before
    // exercising fork and a separate app-server process's cold resume.
    timeout(
        READ_TIMEOUT,
        server.start_turn_and_wait_for_completion(codex_app_server_protocol::TurnStartParams {
            thread_id: started.thread.id.clone(),
            input: vec![codex_app_server_protocol::UserInput::Text {
                text: "Save this turn".into(),
                text_elements: vec![],
            }],
            ..Default::default()
        }),
    )
    .await??;

    let fork_id = server
        .send_thread_fork_request(ThreadForkParams {
            thread_id: started.thread.id.clone(),
            ..Default::default()
        })
        .await?;
    let forked: ThreadForkResponse = timeout(READ_TIMEOUT, server.read_response(fork_id)).await??;
    assert_eq!(forked.model_provider, "go_native_fixture");
    assert_eq!(forked.model, "glm-5.3");

    server.shutdown_gracefully().await?;
    let mut restarted = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(READ_TIMEOUT)
        .await?;
    let resume_id = restarted
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: started.thread.id,
            ..Default::default()
        })
        .await?;
    let resumed: ThreadResumeResponse =
        timeout(READ_TIMEOUT, restarted.read_response(resume_id)).await??;
    assert_eq!(resumed.model_provider, "go_native_fixture");
    assert_eq!(resumed.model, "glm-5.3");
    Ok(())
}

#[cfg(not(windows))]
#[tokio::test]
async fn native_v2_default_full_fork_preserves_tools_across_child_cold_resume() -> Result<()> {
    let home = TempDir::new()?;
    let upstream = wiremock::MockServer::start().await;
    let executed = home.path().join("parent-tool-executions");
    let command = format!(
        "printf x >> '{}'; printf parent-tool-result",
        executed.display()
    );
    let child_requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let observed = child_requests.clone();
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let input = body["input"].as_array().unwrap();
            let child = input.iter().any(|item| item["role"] == "user" && item["content"].as_array().is_some_and(|parts| {
                parts.iter().any(|part| part["text"].as_str().is_some_and(|text| {
                    serde_json::from_str::<Value>(text).is_ok_and(|message| message["type"] == "agent_message" && message["recipient"] == "/root/worker")
                }))
            }));
            let has_output = |id: &str| input.iter().any(|item| {
                item["type"] == "function_call_output" && item["call_id"] == id
            });
            let tool = |id: &str, suffix: &str, arguments: Value| {
                let name = body["tools"].as_array().unwrap().iter()
                    .find_map(|tool| tool["name"].as_str().filter(|name| {
                        *name == suffix || name.ends_with(&format!("__{suffix}"))
                    })).expect("fixture tool must be advertised");
                json!({"type":"function_call","call_id":id,"name":name,"arguments":arguments.to_string()})
            };
            let resumed = input.iter().any(|item| item["content"].as_array().is_some_and(|parts| {
                parts.iter().any(|part| part["text"] == "resume-worker")
            }));
            let output = if child {
                observed.lock().unwrap().push(body.clone());
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"child finished"}]})
            } else if resumed && !has_output("resume-child") {
                tool("resume-child", "followup_task", json!({"target":"worker","message":"child-followup"}))
            } else if resumed && !has_output("wait-resumed-child") {
                tool("wait-resumed-child", "wait_agent", json!({}))
            } else if !resumed && !has_output("parent-action") {
                tool("parent-action", "exec_command", json!({"cmd":command,"max_output_tokens":100}))
            } else if !resumed && !has_output("spawn-worker") {
                tool("spawn-worker", "spawn_agent", json!({"task_name":"worker","message":"child-initial"}))
            } else if !resumed && !has_output("wait-child") {
                tool("wait-child", "wait_agent", json!({}))
            } else {
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"parent finished"}]})
            };
            let delta = if output["type"] == "message" {
                let event = json!({"type":"response.output_text.delta","delta":output["content"][0]["text"]});
                format!("event: response.output_text.delta\ndata: {event}\n\n")
            } else {
                String::new()
            };
            let response = json!({"type":"response.completed","response":{"id":"fixture","output":[output]}});
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(format!("{delta}event: response.completed\ndata: {response}\n\n"))
        })
        .mount(&upstream).await;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            r#"
model = "gpt-6-luna"
model_provider = "go_native_fixture"
approval_policy = "never"
sandbox_mode = "danger-full-access"
developer_instructions = "PARENT_PREFIX_MARKER"
[features.multi_agent_v2]
enabled = true
subagent_developer_instructions = "CHILD_PREFIX_MARKER"
[model_providers.go_native_fixture]
name = "Native fork fixture"
wire_api = "opencode-go"
base_url = {:?}
"#,
            upstream.uri()
        ),
    )?;
    AuthStore::new(home.path())
        .login(
            "opencode-go",
            "default",
            seed_token(API_KEY),
            &CancellationToken::new(),
        )
        .await?;
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(READ_TIMEOUT)
        .await?;
    let parent = timeout(
        READ_TIMEOUT,
        server.start_thread(ThreadStartParams::default()),
    )
    .await??;
    let turn_id = server
        .send_turn_start_request(codex_app_server_protocol::TurnStartParams {
            thread_id: parent.thread.id.clone(),
            input: vec![codex_app_server_protocol::UserInput::Text {
                text: "spawn-worker".into(),
                text_elements: vec![],
            }],
            ..Default::default()
        })
        .await?;
    let _: codex_app_server_protocol::TurnStartResponse =
        timeout(READ_TIMEOUT, server.read_response(turn_id)).await??;
    let spawned = timeout(
        READ_TIMEOUT,
        server.read_stream_until_matching_notification("native worker started", |notification| {
            notification.method == "item/completed"
                && notification.params.as_ref().is_some_and(|params| {
                    params["threadId"] == parent.thread.id
                        && params["item"]["type"] == "subAgentActivity"
                        && params["item"]["id"] == "spawn-worker"
                        && params["item"]["kind"] == "started"
                })
        }),
    )
    .await??;
    let child_id = spawned.params.unwrap()["item"]["agentThreadId"]
        .as_str()
        .unwrap()
        .to_owned();
    let complete = timeout(
        READ_TIMEOUT,
        server.read_stream_until_matching_notification("native parent completed", |notification| {
            notification.method == "turn/completed"
                && notification
                    .params
                    .as_ref()
                    .is_some_and(|params| params["threadId"] == parent.thread.id)
        }),
    )
    .await??;
    let complete: codex_app_server_protocol::TurnCompletedNotification =
        serde_json::from_value(complete.params.unwrap())?;
    assert_eq!(
        complete.turn.status,
        codex_app_server_protocol::TurnStatus::Completed
    );
    assert!(complete.turn.error.is_none(), "{:?}", complete.turn.error);
    timeout(
        READ_TIMEOUT,
        server.read_stream_until_matching_notification("parent assistant output", |notification| {
            notification.method == "item/completed"
                && notification.params.as_ref().is_some_and(|params| {
                    params["threadId"] == parent.thread.id
                        && params["item"]["type"] == "agentMessage"
                        && params["item"]["text"] == "parent finished"
                })
        }),
    )
    .await??;
    // Parent-scoped listing includes workers without a local user-message preview.
    let list_id = server
        .send_request(
            "thread/list",
            Some(json!({"sourceKinds":["subAgent"],"modelProviders":["go_native_fixture"],"parentThreadId":parent.thread.id})),
        )
        .await?;
    let listed: Value = timeout(READ_TIMEOUT, server.read_response(list_id)).await??;
    let children = listed["data"].as_array().unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["id"], child_id);
    assert_ne!(child_id, parent.thread.id);
    assert_eq!(children[0]["parentThreadId"], parent.thread.id);
    assert_eq!(children[0]["modelProvider"], "go_native_fixture");
    assert_eq!(children[0]["model"], "gpt-6-luna");
    timeout(READ_TIMEOUT, server.shutdown_gracefully()).await??;
    let mut restarted = TestAppServer::builder()
        .with_codex_home(home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(READ_TIMEOUT)
        .await?;
    let resume_id = restarted
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: parent.thread.id.clone(),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse =
        timeout(READ_TIMEOUT, restarted.read_response(resume_id)).await??;
    let complete = timeout(
        READ_TIMEOUT,
        restarted.start_turn_and_wait_for_completion(codex_app_server_protocol::TurnStartParams {
            thread_id: parent.thread.id,
            input: vec![codex_app_server_protocol::UserInput::Text {
                text: "resume-worker".into(),
                text_elements: vec![],
            }],
            ..Default::default()
        }),
    )
    .await??;
    assert_eq!(
        complete.turn.status,
        codex_app_server_protocol::TurnStatus::Completed
    );
    assert!(complete.turn.error.is_none(), "{:?}", complete.turn.error);
    let child_resume_id = restarted
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: child_id.clone(),
            ..Default::default()
        })
        .await?;
    let resumed: ThreadResumeResponse =
        timeout(READ_TIMEOUT, restarted.read_response(child_resume_id)).await??;
    assert_eq!(resumed.thread.id, child_id);
    assert_eq!(resumed.model_provider, "go_native_fixture");
    assert_eq!(resumed.model, "gpt-6-luna");
    let child_requests = child_requests.lock().unwrap();
    assert_eq!(child_requests.len(), 2);
    for request in child_requests.iter() {
        assert_eq!(request["model"], "gpt-6-luna");
        let input = request["input"].as_array().unwrap();
        for marker in ["PARENT_PREFIX_MARKER", "CHILD_PREFIX_MARKER"] {
            assert!(
                input.iter().any(|item| item["content"]
                    .as_array()
                    .is_some_and(|parts| { parts.iter().any(|part| part["text"] == marker) })),
                "missing inherited instruction {marker}"
            );
        }
        let calls = input
            .iter()
            .filter(|item| item["type"] == "function_call" && item["call_id"] == "parent-action")
            .count();
        let outputs = input
            .iter()
            .filter(|item| {
                item["type"] == "function_call_output" && item["call_id"] == "parent-action"
            })
            .collect::<Vec<_>>();
        assert_eq!(calls, 1);
        assert_eq!(outputs.len(), 1);
        assert!(
            outputs[0]["output"]
                .as_str()
                .unwrap()
                .contains("parent-tool-result")
        );
    }
    assert!(
        child_requests.last().unwrap()["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| {
                item["role"] == "assistant"
                    && item["content"].as_array().is_some_and(|parts| {
                        parts.iter().any(|part| part["text"] == "child finished")
                    })
            }),
        "cold-resumed child must retain its delivered assistant output"
    );
    assert_eq!(
        std::fs::read_to_string(executed)?,
        "x",
        "inherited tool must never replay"
    );
    Ok(())
}

#[tokio::test]
async fn native_account_pool_revision_redaction_and_named_logout() -> Result<()> {
    let home = TempDir::new()?;
    write_claude_config(home.path())?;
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_auto_env()
        .with_env_overrides(&[("ANTHROPIC_API_KEY", None), ("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(READ_TIMEOUT)
        .await?;
    let id = server
        .send_request(
            "provider/accounts/list",
            Some(json!({"provider":"opencode-go"})),
        )
        .await?;
    let other_provider: Value = timeout(READ_TIMEOUT, server.read_response(id)).await??;
    assert_eq!(other_provider["provider"], "opencode-go");
    assert_eq!(other_provider["endpoint"], "");
    login_claude_with_token(&mut server).await?;
    let id = server.send_login_account_request(json!({"type":"native","provider":"claude","accountId":"work","apiKey":"second-fixture-secret","select":false})).await?;
    let _: Value = timeout(READ_TIMEOUT, server.read_response(id)).await??;
    let id = server
        .send_request("provider/accounts/list", Some(json!({"provider":"claude"})))
        .await?;
    let status: Value = timeout(READ_TIMEOUT, server.read_response(id)).await??;
    assert_eq!(status["enabled"], true);
    assert_eq!(status["preferredAccount"], "default");
    assert_eq!(status["accounts"].as_array().unwrap().len(), 2);
    assert!(!status.to_string().contains(API_KEY));
    assert!(!status.to_string().contains("second-fixture-secret"));
    let revision = status["revision"].as_u64().unwrap();
    let id = server
        .send_request(
            "provider/pool/update",
            Some(json!({"provider":"claude","revision":revision,"update":{"action":"disable"}})),
        )
        .await?;
    let disabled: Value = timeout(READ_TIMEOUT, server.read_response(id)).await??;
    assert_eq!(disabled["enabled"], false);
    let id = server
        .send_request(
            "provider/pool/update",
            Some(json!({"provider":"claude","revision":revision,"update":{"action":"enable"}})),
        )
        .await?;
    let stale = timeout(
        READ_TIMEOUT,
        server.read_stream_until_error_message(codex_app_server_protocol::RequestId::Integer(id)),
    )
    .await??;
    assert!(stale.error.message.contains("reload"));
    let id = server
        .send_request(
            "provider/account/logout",
            Some(json!({"provider":"claude","account":"work","revision":disabled["revision"]})),
        )
        .await?;
    let after: Value = timeout(READ_TIMEOUT, server.read_response(id)).await??;
    assert_eq!(after["enabled"], false);
    let auth = AuthStore::new(home.path());
    assert!(auth.account_reference("claude", "work").is_err());
    assert_eq!(auth.selected("claude")?.as_deref(), Some("default"));
    Ok(())
}
