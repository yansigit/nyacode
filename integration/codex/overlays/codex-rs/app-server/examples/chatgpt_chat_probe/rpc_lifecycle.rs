//! Explicit live cancellation and feature revocation against new saved fixtures.
//! No submission is retried; account contents and identifiers stay out of reports.
use super::*;
use codex_chatgpt_chat::LifecycleRpcObservation;

pub(super) async fn run(
    client: &mut Client,
    observation: &mut RpcObservation,
) -> Result<(), String> {
    let mut lifecycle = LifecycleRpcObservation::default();
    let result = exercise(client, observation, &mut lifecycle).await;
    // The harness owns this embedded process or designated test socket. Leave
    // its runtime feature disabled even if a scenario failed partway through.
    let cleanup = set_enabled(client, false, 900).await;
    observation.lifecycle = Some(lifecycle);
    result.and(cleanup.map(|_| ()))
}

async fn set_enabled(
    client: &Client,
    enabled: bool,
    id: i64,
) -> Result<rpc::ChatStatusReadResponse, String> {
    let response: rpc::ExperimentalFeatureEnablementSetResponse = call(
        client,
        id,
        "experimentalFeature/enablement/set",
        json!({"enablement":{"chatgpt_chat":enabled}}),
    )
    .await?;
    let status: rpc::ChatStatusReadResponse =
        call(client, id + 1, "chat/status/read", json!({})).await?;
    let expected = if enabled {
        rpc::ChatAvailability::Available
    } else {
        rpc::ChatAvailability::Disabled
    };
    if response.enablement.get("chatgpt_chat") != Some(&enabled) || status.availability != expected
    {
        return Err("runtime_feature_override_prevents_lifecycle_probe".into());
    }
    Ok(status)
}

async fn exercise(
    client: &mut Client,
    observation: &mut RpcObservation,
    lifecycle: &mut LifecycleRpcObservation,
) -> Result<(), String> {
    set_enabled(client, false, 10).await?;
    let enabled = set_enabled(client, true, 12).await?;
    let scope = enabled.scope_id.ok_or("scope_unavailable")?;
    observation.status_available = true;
    lifecycle.runtime_enablement = true;
    let models: rpc::ChatModelListResponse =
        call(client, 14, "chat/model/list", json!({"scopeId":scope})).await?;
    observation.model_count = models.models.len();
    observation.default_model = Some(models.default_model);

    let cancelled = start_accepted(client, &scope, 20, observation).await?;
    lifecycle.cancel_after_acceptance = true;
    let cancel: rpc::ChatTurnCancelResponse = call(
        client,
        22,
        "chat/turn/cancel",
        json!({
            "scopeId":scope,"operationId":cancelled.operation_id
        }),
    )
    .await?;
    lifecycle.cancel_requested_locally = cancel.local_stop_requested;
    lifecycle.server_cancellation_confirmed = cancel.server_cancellation_confirmed;
    let stopped = wait_for_turn(client, &scope, &cancelled.operation_id, observation).await?;
    lifecycle.cancel_finished_locally = stopped.phase == rpc::ChatTurnPhase::Finished
        && stopped.error == Some(rpc::ChatErrorCode::LocalCancellation);
    lifecycle.cancel_acceptance_retained = stopped.acceptance == rpc::ChatAcceptance::Accepted;
    if !lifecycle.cancel_requested_locally
        || !lifecycle.cancel_finished_locally
        || !lifecycle.cancel_acceptance_retained
        || lifecycle.server_cancellation_confirmed
    {
        return Err("local_cancellation_not_established".into());
    }

    let temporary: rpc::ChatDraftWriteResponse = call(
        client,
        30,
        "chat/draft/write",
        json!({
            "scopeId":scope,"persistence":"temporary","draftId":Uuid::new_v4(),
            "content":{"text":"Temporary revocation fixture","selections":[]}
        }),
    )
    .await?;
    let active = start_accepted(client, &scope, 40, observation).await?;
    let before_disable: rpc::ChatStatusReadResponse =
        call(client, 401, "chat/status/read", json!({})).await?;
    if !before_disable.turns.iter().any(|turn| {
        turn.operation_id == active.operation_id
            && turn.acceptance == rpc::ChatAcceptance::Accepted
            && turn.phase != rpc::ChatTurnPhase::Finished
    }) {
        return Err("turn_finished_before_feature_disablement".into());
    }
    lifecycle.disable_after_acceptance = true;
    let disabled = set_enabled(client, false, 42).await?;
    lifecycle.disabled_has_no_capabilities =
        disabled.scope_id.is_none() && disabled.turns.is_empty() && disabled.uploads.is_empty();
    let old_read: Result<rpc::ChatDraftReadResponse, _> = call(
        client,
        44,
        "chat/draft/read",
        json!({
            "scopeId":scope,"persistence":"temporary","draftId":temporary.draft.id
        }),
    )
    .await;
    lifecycle.revoked_scope_rejected = matches!(old_read, Err(ref error) if error == "disabled");
    let enabled = set_enabled(client, true, 46).await?;
    let fresh_scope = enabled.scope_id.ok_or("scope_unavailable")?;
    lifecycle.fresh_scope = fresh_scope != scope && enabled.generation > disabled.generation;
    let temporary_read: rpc::ChatDraftReadResponse = call(
        client,
        48,
        "chat/draft/read",
        json!({
            "scopeId":fresh_scope,"persistence":"temporary","draftId":temporary.draft.id
        }),
    )
    .await?;
    lifecycle.temporary_draft_removed = temporary_read.draft.is_none();
    // Verify both previously accepted user IDs against ChatGPT itself. Reads
    // may reconcile a released lease, but can never prepare or resend a turn.
    for (index, turn) in [cancelled, active].into_iter().enumerate() {
        let conversation = turn.conversation_id.ok_or("conversation_id_missing")?;
        let read: rpc::ChatConversationReadResponse = call(
            client,
            50 + index as i64,
            "chat/conversation/read",
            json!({
                "scopeId":fresh_scope,"persistence":"saved","conversationId":conversation
            }),
        )
        .await?;
        if read.source != rpc::ChatConversationSource::Authoritative
            || !read.nodes.iter().any(|node| {
                node.message.as_ref().is_some_and(|message| {
                    message.role == "user" && message.id == turn.client_message_id
                })
            })
        {
            return Err("accepted_saved_message_not_retrieved_after_revocation".into());
        }
        lifecycle.saved_messages_retrieved += 1;
    }
    if !lifecycle.disabled_has_no_capabilities
        || !lifecycle.revoked_scope_rejected
        || !lifecycle.fresh_scope
        || !lifecycle.temporary_draft_removed
    {
        return Err("feature_revocation_incomplete".into());
    }
    Ok(())
}

async fn start_accepted(
    client: &mut Client,
    scope: &str,
    id: i64,
    observation: &mut RpcObservation,
) -> Result<rpc::ChatTurnSnapshot, String> {
    let text = "Write the integers from 1 to 5000, one per line, without skipping any. This is a harmless streaming cancellation fixture.";
    let parent = Uuid::new_v4().to_string();
    let draft: rpc::ChatDraftWriteResponse = call(
        client,
        id,
        "chat/draft/write",
        json!({
            "scopeId":scope,"persistence":"saved","draftId":Uuid::new_v4(),
            "content":{"expectedParentId":parent,"text":text,"selections":[]}
        }),
    )
    .await?;
    let start: rpc::ChatTurnStartResponse = call(client, id + 1, "chat/turn/start", json!({
        "scopeId":scope,"persistence":"saved","draftId":draft.draft.id,
        "expectedRevision":draft.draft.revision,"submissionId":Uuid::new_v4(),
        "clientMessageId":Uuid::new_v4(),"expectedParentId":parent,"text":text,"uploadHandles":[]
    })).await?;
    let operation = start.turn.operation_id;
    let mut last_sequence = None;
    loop {
        let Some(event) = client.next_event().await else {
            return Err("rpc_event_stream_closed".into());
        };
        let AppServerEvent::ServerNotification(notification) = event else {
            return Err("unexpected_rpc_event".into());
        };
        let turn = match *notification {
            rpc::ServerNotification::ChatMessage(event) => event.turn,
            rpc::ServerNotification::ChatTurnCompleted(event) => event.turn,
            _ => continue,
        };
        if turn.scope_id != scope || turn.operation_id != operation {
            continue;
        }
        if last_sequence.is_some_and(|prior| turn.sequence <= prior) {
            return Err("rpc_sequence_reordered".into());
        }
        last_sequence = Some(turn.sequence);
        observation.streamed_updates += 1;
        if turn.phase == rpc::ChatTurnPhase::Finished {
            return Err(turn
                .error
                .map_or_else(|| "turn_finished_before_cancellation".into(), error_code));
        }
        if turn.acceptance == rpc::ChatAcceptance::Accepted {
            observation.accepted = true;
            observation
                .requested_model
                .clone_from(&turn.requested_model);
            observation.resolved_model.clone_from(&turn.resolved_model);
            return Ok(turn);
        }
    }
}
