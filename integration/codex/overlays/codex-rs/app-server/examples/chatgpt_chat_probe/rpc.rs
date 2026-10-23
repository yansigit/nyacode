use codex_app_server::in_process::EmbeddedNetworkPolicy;
use codex_app_server::in_process::InProcessStartArgs;
use codex_app_server::in_process::{self};
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol as rpc;
use codex_chatgpt_chat::RpcObservation;
use codex_chatgpt_chat::TemporaryRpcObservation;
use codex_core::config::Config;
use codex_state::StateRuntime;
use futures::TryStreamExt;
use serde::de::DeserializeOwned;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use sqlx::SqliteConnection;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

#[path = "rpc_attachments.rs"]
mod attachments;
#[path = "rpc_client.rs"]
mod client;
#[path = "rpc_lifecycle.rs"]
mod lifecycle;
#[path = "rpc_saved.rs"]
mod saved;
use client::Client;

pub(super) struct Target {
    pub socket: Option<std::path::PathBuf>,
    pub expected_account: String,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum Scenario {
    Read,
    Saved,
    ResumeSaved(String),
    Temporary,
    Lifecycle,
    Attachments(std::path::PathBuf),
}

pub(super) async fn run(
    config: Config,
    overrides: Vec<(String, toml::Value)>,
    policy: EmbeddedNetworkPolicy,
    scenario: Scenario,
    target: Target,
) -> RpcObservation {
    let mut observation = RpcObservation {
        local_socket: target.socket.is_some(),
        ..Default::default()
    };
    if let Err(error) = exercise(
        config,
        overrides,
        policy,
        scenario,
        target,
        &mut observation,
    )
    .await
    {
        observation.error = Some(error);
    }
    observation
}

async fn exercise(
    mut config: Config,
    mut cli_overrides: Vec<(String, toml::Value)>,
    embedded_network_policy: EmbeddedNetworkPolicy,
    scenario: Scenario,
    target: Target,
    observation: &mut RpcObservation,
) -> Result<(), String> {
    if scenario == Scenario::Lifecycle && target.socket.is_none() {
        // The explicit CLI flag authorized the live probe. Its owned server
        // needs runtime enablement so that the disablement RPC can take effect.
        // Preserve every user/admin override and fail if one prevents toggling.
        cli_overrides.retain(|(key, _)| key != "features.chatgpt_chat");
        config = codex_core::config::ConfigBuilder::default()
            .without_project_context()
            .codex_home(config.codex_home.to_path_buf())
            .cli_overrides(cli_overrides.clone())
            .build()
            .await
            .map_err(|_| "lifecycle_config_unavailable")?;
        embedded_network_policy.activate(&mut config);
    }
    let sqlite = config.sqlite_config().clone();
    let db = StateRuntime::init(config.sqlite_config().clone(), "chat-rpc-probe".into())
        .await
        .map_err(|_| "recovery_unavailable")?;
    let audit_pool = sqlite
        .open_read_only_pool(&sqlite.state_db_path(), None)
        .await
        .map_err(|_| "audit_unavailable")?;
    let environments = Arc::new(codex_exec_server::EnvironmentManager::without_environments(
        config
            .http_client_factory()
            .with_network_policy(config.application_network_policy.clone()),
    ));
    let mut client = if let Some(path) = &target.socket {
        Client::socket(path, config.codex_home.as_path()).await?
    } else {
        Client::Embedded(
            in_process::start(InProcessStartArgs {
                arg0_paths: Default::default(),
                config: Arc::new(config),
                cli_overrides,
                loader_overrides: Default::default(),
                strict_config: false,
                cloud_config_bundle: Default::default(),
                embedded_network_policy,
                thread_config_loader: Arc::new(codex_config::NoopThreadConfigLoader),
                feedback: codex_feedback::CodexFeedback::new(),
                log_db: None,
                state_db: Some(Arc::clone(&db)),
                environment_manager: environments,
                config_warnings: Vec::new(),
                session_source: codex_protocol::protocol::SessionSource::Cli,
                enable_codex_api_key_env: false,
                initialize: rpc::InitializeParams {
                    client_info: rpc::ClientInfo {
                        name: "nyacode-chat-rpc-probe".into(),
                        title: None,
                        version: "1".into(),
                    },
                    capabilities: Some(rpc::InitializeCapabilities {
                        experimental_api: true,
                        ..Default::default()
                    }),
                },
                channel_capacity: 64,
            })
            .await
            .map_err(|_| "embedded_start_failed")?,
        )
    };
    let timeout = if matches!(scenario, Scenario::Attachments(_)) {
        600
    } else {
        240
    };
    let result = tokio::time::timeout(Duration::from_secs(timeout), async {
        let mut audit = audit_pool
            .acquire()
            .await
            .map_err(|_| "audit_unavailable")?;
        let account: rpc::GetAccountResponse =
            call(&client, 0, "account/read", json!({"refreshToken":false})).await?;
        if !matches!(account.account, Some(rpc::Account::Chatgpt { .. }))
            || account
                .workspace_routing
                .as_ref()
                .is_none_or(|routing| routing.chatgpt_account_id != target.expected_account)
        {
            return Err("rpc_daemon_account_mismatch".into());
        }
        observation.account_matched = true;
        run_scenario(&mut client, scenario, observation, &mut audit).await
    })
    .await
    .map_err(|_| "rpc_timeout".to_owned())
    .and_then(|result| result);
    let shutdown = client
        .shutdown()
        .await
        .map_err(|_| "embedded_shutdown_failed".to_owned());
    audit_pool.close().await;
    db.close().await;
    result.and(shutdown)
}

async fn call<T: DeserializeOwned>(
    client: &Client,
    id: i64,
    method: &str,
    params: serde_json::Value,
) -> Result<T, String> {
    let request = rpc::ClientRequest::try_from(rpc::JSONRPCRequest {
        id: rpc::RequestId::Integer(id),
        method: method.into(),
        params: Some(params),
        trace: None,
    })
    .map_err(|_| "probe_request_invalid")?;
    let result = client
        .request(request)
        .await
        .map_err(|_| "rpc_transport_failed")?
        .map_err(|error| {
            error
                .data
                .and_then(|data| serde_json::from_value::<rpc::ChatErrorDetails>(data).ok())
                .and_then(|details| serde_json::to_value(details.code).ok())
                .and_then(|code| code.as_str().map(str::to_owned))
                .unwrap_or_else(|| "rpc_failed".into())
        })?;
    serde_json::from_value(result).map_err(|_| "rpc_response_invalid".into())
}

async fn run_scenario(
    client: &mut Client,
    scenario: Scenario,
    observation: &mut RpcObservation,
    audit: &mut SqliteConnection,
) -> Result<(), String> {
    if scenario == Scenario::Lifecycle {
        return lifecycle::run(client, observation).await;
    }
    let status: rpc::ChatStatusReadResponse =
        call(client, 1, "chat/status/read", json!({})).await?;
    observation.status_available = status.availability == rpc::ChatAvailability::Available;
    let scope = status.scope_id.ok_or("scope_unavailable")?;
    let models: rpc::ChatModelListResponse =
        call(client, 2, "chat/model/list", json!({"scopeId":scope})).await?;
    observation.model_count = models.models.len();
    observation.default_model = Some(models.default_model);
    let history: rpc::ChatConversationListResponse = call(
        client,
        3,
        "chat/conversation/list",
        json!({"scopeId":scope,"limit":20}),
    )
    .await?;
    observation.history_count = history.items.len();
    if scenario == Scenario::Temporary {
        return temporary(client, &scope, observation, audit).await;
    }
    if scenario == Scenario::Read {
        return Ok(());
    }
    if let Scenario::Attachments(directory) = scenario {
        return attachments::run(client, &scope, &directory, observation).await;
    }
    let resume = match scenario {
        Scenario::ResumeSaved(id) => Some(id),
        Scenario::Saved => None,
        _ => return Err("probe_scenario_invalid".into()),
    };
    saved::run(client, &scope, resume.as_deref(), observation).await
}

fn error_code(error: rpc::ChatErrorCode) -> String {
    serde_json::to_value(error)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "turn_failed".into())
}

async fn wait_for_turn(
    client: &mut Client,
    scope: &str,
    operation_id: &str,
    observation: &mut RpcObservation,
) -> Result<rpc::ChatTurnSnapshot, String> {
    let mut last_sequence = None;
    observation.sequence_ordered = true;
    let turn = loop {
        let Some(event) = client.next_event().await else {
            return Err("rpc_event_stream_closed".into());
        };
        match event {
            AppServerEvent::ServerNotification(notification) => {
                let turn = match *notification {
                    rpc::ServerNotification::ChatMessage(event) => Some(event.turn),
                    rpc::ServerNotification::ChatTurnCompleted(event) => Some(event.turn),
                    _ => None,
                };
                let Some(turn) = turn else { continue };
                if turn.scope_id != scope || turn.operation_id != operation_id {
                    continue;
                }
                if last_sequence.is_some_and(|prior| turn.sequence <= prior) {
                    observation.sequence_ordered = false;
                }
                last_sequence = Some(turn.sequence);
                observation.streamed_updates += 1;
                if turn.phase == rpc::ChatTurnPhase::Finished {
                    break turn;
                }
            }
            AppServerEvent::Disconnected { .. } => return Err("rpc_event_stream_closed".into()),
            AppServerEvent::Lagged { .. } => return Err("rpc_event_lagged".into()),
            AppServerEvent::ServerRequest(_) => return Err("unexpected_agent_request".into()),
        }
    };
    Ok(turn)
}

fn fixture_matches(message: &rpc::ChatMessage, expected: &str) -> bool {
    message.role == "assistant"
        && message.finished
        && message.end_turn
        && message
            .parts
            .iter()
            .filter_map(|part| match part {
                rpc::ChatMessagePart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>()
            .trim()
            == expected
}

async fn temporary(
    client: &mut Client,
    scope: &str,
    observation: &mut RpcObservation,
    audit: &mut SqliteConnection,
) -> Result<(), String> {
    let before = saved_tables_digest(audit).await?;
    let mut temporary = TemporaryRpcObservation::default();
    let result = temporary_turns(client, scope, observation, &mut temporary).await;
    let unchanged = saved_tables_digest(audit)
        .await
        .map(|after| after == before);
    temporary.saved_tables_unchanged = unchanged.as_ref().is_ok_and(|value| *value);
    observation.temporary = Some(temporary);
    result?;
    if !unchanged? {
        return Err("temporary_saved_state_changed".into());
    }
    Ok(())
}

async fn temporary_turns(
    client: &mut Client,
    scope: &str,
    observation: &mut RpcObservation,
    temporary: &mut TemporaryRpcObservation,
) -> Result<(), String> {
    let mut draft_id = Uuid::new_v4().to_string();
    let mut revision = None::<String>;
    let mut conversation_id = None::<String>;
    let mut parent = Uuid::new_v4().to_string();
    for (index, (text, expected)) in [
        (
            "Reply with exactly RPC-TEMP-ASPEN-953.",
            "RPC-TEMP-ASPEN-953",
        ),
        (
            "Repeat your previous answer, followed immediately by /FOLLOWUP, and nothing else.",
            "RPC-TEMP-ASPEN-953/FOLLOWUP",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let request_id = 10 + index as i64 * 10;
        let draft: rpc::ChatDraftWriteResponse = call(client, request_id, "chat/draft/write", json!({
            "scopeId":scope, "persistence":"temporary", "draftId":draft_id, "expectedRevision":revision,
            "content":{"conversationId":conversation_id,"expectedParentId":parent,"text":text,"selections":[]}
        })).await?;
        draft_id = draft.draft.id;
        let client_message_id = Uuid::new_v4().to_string();
        let start: rpc::ChatTurnStartResponse = call(client, request_id + 1, "chat/turn/start", json!({
            "scopeId":scope,"persistence":"temporary","draftId":draft_id,"expectedRevision":draft.draft.revision,
            "submissionId":Uuid::new_v4(),"clientMessageId":client_message_id,"expectedParentId":parent,"text":text,"uploadHandles":[]
        })).await?;
        let turn = wait_for_turn(client, scope, &start.turn.operation_id, observation).await?;
        observation.accepted = turn.acceptance == rpc::ChatAcceptance::Accepted;
        observation.completed = turn.outcome == rpc::ChatGenerationOutcome::Completed;
        observation
            .requested_model
            .clone_from(&turn.requested_model);
        observation.resolved_model.clone_from(&turn.resolved_model);
        observation.fixture_response_matched = turn
            .messages
            .iter()
            .any(|message| fixture_matches(message, expected));
        if let Some(error) = turn.error {
            return Err(error_code(error));
        }
        if !observation.accepted
            || !observation.completed
            || !observation.fixture_response_matched
            || !observation.sequence_ordered
            || turn.persistence != rpc::ChatPersistence::Temporary
        {
            return Err("temporary_rpc_turn_incomplete".into());
        }
        let id = turn.conversation_id.ok_or("conversation_id_missing")?;
        if conversation_id.as_ref().is_some_and(|prior| prior != &id) {
            return Err("temporary_conversation_changed".into());
        }
        let read: rpc::ChatConversationReadResponse = call(
            client,
            request_id + 2,
            "chat/conversation/read",
            json!({
                "scopeId":scope,"persistence":"temporary","conversationId":id
            }),
        )
        .await?;
        let matched = read.persistence == rpc::ChatPersistence::Temporary
            && read.source == rpc::ChatConversationSource::TemporaryMemory
            && read.nodes.iter().any(|node| {
                node.parent_id.as_deref() == Some(parent.as_str())
                    && node.message.as_ref().is_some_and(|message| {
                        message.id == client_message_id && message.role == "user"
                    })
            })
            && read.nodes.iter().any(|node| {
                node.message
                    .as_ref()
                    .is_some_and(|message| fixture_matches(message, expected))
            });
        observation.pending_cleared = read.pending_submission.is_none();
        temporary.recovery_unavailable = !read.recovery_available;
        temporary.memory_snapshot_matched = matched;
        if !matched
            || !temporary.recovery_unavailable
            || !observation.pending_cleared
            || read.continuation_blocker.is_some()
        {
            return Err("temporary_snapshot_incomplete".into());
        }
        parent = read.current_node.ok_or("temporary_parent_missing")?;
        conversation_id = Some(id);
        let current: rpc::ChatDraftReadResponse = call(
            client,
            request_id + 3,
            "chat/draft/read",
            json!({
                "scopeId":scope,"persistence":"temporary","draftId":draft_id
            }),
        )
        .await?;
        let current = current.draft.ok_or("temporary_draft_missing")?;
        if current.content.conversation_id != conversation_id
            || current.content.expected_parent_id.as_deref() != Some(parent.as_str())
            || !current.content.text.is_empty()
        {
            return Err("temporary_draft_not_advanced".into());
        }
        revision = Some(current.revision);
        temporary.completed_turns += 1;
    }
    temporary.continuation_verified = true;
    let saved: Result<rpc::ChatConversationReadResponse, _> = call(
        client,
        30,
        "chat/conversation/read",
        json!({
            "scopeId":scope,"persistence":"saved","conversationId":conversation_id
        }),
    )
    .await;
    let rejected = matches!(saved, Err(ref error) if error == "notFound");
    temporary.saved_read_rejected = rejected;
    if !rejected {
        return Err("temporary_saved_read_not_rejected".into());
    }
    let history: rpc::ChatConversationListResponse = call(
        client,
        31,
        "chat/conversation/list",
        json!({"scopeId":scope,"limit":100}),
    )
    .await?;
    let absent = !history
        .items
        .iter()
        .any(|item| Some(&item.id) == conversation_id.as_ref());
    temporary.absent_from_recent_history = absent;
    if !absent {
        return Err("temporary_present_in_saved_history".into());
    }
    Ok(())
}

/// Compare complete saved Chat rows in memory. No content or identifiers from
/// this audit are included in the diagnostic report or written to a file.
async fn saved_tables_digest(connection: &mut SqliteConnection) -> Result<[u8; 32], String> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name GLOB 'chat_*' ORDER BY name",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(|_| "audit_unavailable")?;
    let mut digest = Sha256::new();
    for table in tables {
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info(?) ORDER BY cid")
                .bind(&table)
                .fetch_all(&mut *connection)
                .await
                .map_err(|_| "audit_unavailable")?;
        if !std::iter::once(&table)
            .chain(columns.iter())
            .all(|identifier| {
                !identifier.is_empty()
                    && identifier
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
        {
            return Err("audit_schema_unsupported".into());
        }
        digest.update(table.as_bytes());
        let columns = columns
            .iter()
            .map(|column| format!("\"{column}\""))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT json_array({columns}) FROM \"{table}\" ORDER BY {columns}");
        // Every interpolated identifier was restricted to ASCII letters,
        // digits and underscores above, then quoted. No values enter this SQL.
        let mut rows = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql.as_str()))
            .fetch(&mut *connection);
        while let Some(row) = rows.try_next().await.map_err(|_| "audit_unavailable")? {
            digest.update((row.len() as u64).to_le_bytes());
            digest.update(row.as_bytes());
        }
    }
    Ok(digest.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Connection;

    #[tokio::test]
    async fn temporary_audit_detects_content_changes_and_ignores_other_tables() {
        let mut connection = SqliteConnection::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE chat_fixture (id TEXT PRIMARY KEY, text TEXT)")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE unrelated (text TEXT)")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("INSERT INTO chat_fixture VALUES ('one', 'original')")
            .execute(&mut connection)
            .await
            .unwrap();
        let before = saved_tables_digest(&mut connection).await.unwrap();
        sqlx::query("UPDATE chat_fixture SET text = 'changed'")
            .execute(&mut connection)
            .await
            .unwrap();
        assert_ne!(before, saved_tables_digest(&mut connection).await.unwrap());
        sqlx::query("UPDATE chat_fixture SET text = 'original'")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("INSERT INTO unrelated VALUES ('fixture')")
            .execute(&mut connection)
            .await
            .unwrap();
        assert_eq!(before, saved_tables_digest(&mut connection).await.unwrap());
    }
}
