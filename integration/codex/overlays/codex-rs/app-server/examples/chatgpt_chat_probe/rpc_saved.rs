//! Saved RPC acceptance, including a separately invoked fresh-process resume.
//! The runner privately records the newly created draft ID from durable state.
//! Resume never replays a pending submission or selects a personal conversation.

use super::*;

const INITIAL_TEXT: &str = "Reply with exactly RPC-BIRCH-842.";
const INITIAL_RESPONSE: &str = "RPC-BIRCH-842";
const FOLLOW_UP_TEXT: &str = "Reply with exactly RPC-CEDAR-419.";
const FOLLOW_UP_RESPONSE: &str = "RPC-CEDAR-419";

pub(super) async fn run(
    client: &mut Client,
    scope: &str,
    resume: Option<&str>,
    observation: &mut RpcObservation,
) -> Result<(), String> {
    let (draft_id, revision, conversation_id, parent, text, expected) = if let Some(id) = resume {
        let draft: rpc::ChatDraftReadResponse = call(
            client,
            4,
            "chat/draft/read",
            json!({"scopeId":scope,"persistence":"saved","draftId":id}),
        )
        .await?;
        if draft.pending_submission.is_some() {
            return Err("pending_submission_requires_reconciliation".into());
        }
        let draft = draft.draft.ok_or("resume_draft_missing")?;
        let conversation_id = draft
            .content
            .conversation_id
            .as_deref()
            .ok_or("resume_conversation_missing")?;
        let read: rpc::ChatConversationReadResponse = call(
            client,
            5,
            "chat/conversation/read",
            json!({"scopeId":scope,"persistence":"saved","conversationId":conversation_id}),
        )
        .await?;
        let parent = validate_resume(&draft, &read)?.to_owned();
        observation.resumed_saved_draft = true;
        (
            draft.id,
            Some(draft.revision),
            Some(conversation_id.to_owned()),
            parent,
            FOLLOW_UP_TEXT,
            FOLLOW_UP_RESPONSE,
        )
    } else {
        (
            Uuid::new_v4().to_string(),
            None,
            None,
            Uuid::new_v4().to_string(),
            INITIAL_TEXT,
            INITIAL_RESPONSE,
        )
    };
    let draft: rpc::ChatDraftWriteResponse = call(
        client,
        6,
        "chat/draft/write",
        json!({
            "scopeId":scope,"persistence":"saved","draftId":draft_id,
            "expectedRevision":revision,
            "content":{"conversationId":conversation_id,"expectedParentId":parent,"text":text,"selections":[]}
        }),
    )
    .await?;
    let client_message_id = Uuid::new_v4().to_string();
    let start: rpc::ChatTurnStartResponse = call(
        client,
        7,
        "chat/turn/start",
        json!({
            "scopeId":scope,"persistence":"saved","draftId":draft.draft.id,
            "expectedRevision":draft.draft.revision,"submissionId":Uuid::new_v4(),
            "clientMessageId":client_message_id,"expectedParentId":parent,"text":text,"uploadHandles":[]
        }),
    )
    .await?;
    let turn = wait_for_turn(client, scope, &start.turn.operation_id, observation).await?;
    observation.accepted = turn.acceptance == rpc::ChatAcceptance::Accepted;
    observation.completed = turn.outcome == rpc::ChatGenerationOutcome::Completed;
    observation.requested_model = turn.requested_model;
    observation.resolved_model = turn.resolved_model;
    observation.fixture_response_matched = turn
        .messages
        .iter()
        .any(|message| fixture_matches(message, expected));
    if let Some(error) = turn.error {
        return Err(error_code(error));
    }
    let actual_conversation = turn.conversation_id.ok_or("conversation_id_missing")?;
    if conversation_id.is_some_and(|expected| expected != actual_conversation) {
        return Err("resume_conversation_changed".into());
    }
    let conversation: rpc::ChatConversationReadResponse = call(
        client,
        8,
        "chat/conversation/read",
        json!({"scopeId":scope,"persistence":"saved","conversationId":actual_conversation}),
    )
    .await?;
    observation.pending_cleared =
        conversation.recovery_available && conversation.pending_submission.is_none();
    observation.parent_matched = conversation.nodes.iter().any(|node| {
        node.parent_id.as_deref() == Some(&parent)
            && node
                .message
                .as_ref()
                .is_some_and(|message| message.role == "user" && message.id == client_message_id)
    });
    observation.authoritative_retrieval = conversation.source
        == rpc::ChatConversationSource::Authoritative
        && conversation.nodes.iter().any(|node| {
            node.message
                .as_ref()
                .is_some_and(|message| message.role == "user" && message.id == client_message_id)
        })
        && conversation.nodes.iter().any(|node| {
            node.message
                .as_ref()
                .is_some_and(|message| fixture_matches(message, expected))
        });
    if !observation.accepted
        || !observation.completed
        || !observation.fixture_response_matched
        || !observation.authoritative_retrieval
        || !observation.pending_cleared
        || !observation.parent_matched
        || !observation.sequence_ordered
    {
        return Err("rpc_acceptance_incomplete".into());
    }
    // Mirror the terminal's post-acceptance composer flush. Saved drafts keep
    // their revision until a client explicitly clears the accepted snapshot;
    // generation notifications alone must never overwrite concurrent edits.
    let current = conversation.current_node.ok_or("rpc_parent_missing")?;
    let flushed: rpc::ChatDraftWriteResponse = call(
        client,
        9,
        "chat/draft/write",
        json!({
            "scopeId":scope,"persistence":"saved","draftId":draft.draft.id,
            "expectedRevision":draft.draft.revision,
            "content":{"conversationId":actual_conversation,"expectedParentId":current,"text":"","selections":[]}
        }),
    )
    .await?;
    if flushed.draft.content.conversation_id.as_deref() != Some(&actual_conversation)
        || flushed.draft.content.expected_parent_id.as_deref() != Some(&current)
        || !flushed.draft.content.text.is_empty()
        || !flushed.draft.content.selections.is_empty()
    {
        return Err("rpc_draft_flush_failed".into());
    }
    Ok(())
}

fn validate_resume<'a>(
    draft: &rpc::ChatDraft,
    read: &'a rpc::ChatConversationReadResponse,
) -> Result<&'a str, String> {
    if draft.persistence != rpc::ChatPersistence::Saved
        || read.persistence != rpc::ChatPersistence::Saved
        || read.source != rpc::ChatConversationSource::Authoritative
        || !read.recovery_available
        || read.pending_submission.is_some()
        || read.continuation_blocker.is_some()
        || !draft.content.text.is_empty()
        || !draft.content.selections.is_empty()
        || draft.content.conversation_id.as_deref() != Some(&read.conversation_id)
        || draft.content.expected_parent_id != read.current_node
    {
        return Err("resume_fixture_not_ready".into());
    }
    let parent = read
        .current_node
        .as_deref()
        .ok_or("resume_parent_missing")?;
    let assistant = read
        .nodes
        .iter()
        .find(|node| node.id == parent)
        .ok_or("resume_parent_missing")?;
    let user = preceding_user(read, assistant).ok_or("resume_fixture_missing")?;
    if !assistant
        .message
        .as_ref()
        .is_some_and(|message| fixture_matches(message, INITIAL_RESPONSE))
        || user.role != "user"
        || !user.attachments.is_empty()
        || !matches!(user.parts.as_slice(), [rpc::ChatMessagePart::Text { text }] if text == INITIAL_TEXT)
        || read
            .nodes
            .iter()
            .filter(|node| {
                node.message
                    .as_ref()
                    .is_some_and(|message| message.role == "user")
            })
            .count()
            != 1
    {
        return Err("resume_fixture_missing".into());
    }
    Ok(parent)
}

fn preceding_user<'a>(
    read: &'a rpc::ChatConversationReadResponse,
    assistant: &'a rpc::ChatConversationNode,
) -> Option<&'a rpc::ChatMessage> {
    let nodes: std::collections::BTreeMap<_, _> = read
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let mut parent = assistant.parent_id.as_deref();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = parent {
        if !seen.insert(id) || seen.len() > 8192 {
            return None;
        }
        let node = nodes.get(id)?;
        let message = node.message.as_ref()?;
        if message.role == "user" {
            return Some(message);
        }
        // A thinking response has a finished, nonterminal assistant node
        // between its user and final answer. Follow only this verified shape.
        if message.role != "assistant"
            || !message.finished
            || message.end_turn
            || !message.attachments.is_empty()
        {
            return None;
        }
        parent = node.parent_id.as_deref();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> (rpc::ChatDraft, rpc::ChatConversationReadResponse) {
        let draft = serde_json::from_value(json!({
            "id":"draft","revision":"revision","persistence":"saved","updatedAtMs":0,
            "content":{"conversationId":"conversation","expectedParentId":"assistant","text":"","selections":[]}
        })).unwrap();
        let read = serde_json::from_value(json!({
            "scopeId":"scope","conversationId":"conversation","persistence":"saved",
            "source":"authoritative","title":"fixture","currentNode":"assistant",
            "nodes":[
                {"id":"user","parentId":"root","message":{"id":"user","role":"user","parts":[{"type":"text","text":INITIAL_TEXT}],"finished":true,"endTurn":false,"attachments":[],"citations":[]}},
                {"id":"assistant","parentId":"user","message":{"id":"assistant","role":"assistant","parts":[{"type":"text","text":INITIAL_RESPONSE}],"finished":true,"endTurn":true,"attachments":[],"citations":[]}}
            ],"otherBranchHeads":[],"recoveryAvailable":true
        })).unwrap();
        (draft, read)
    }

    #[test]
    fn resume_requires_the_unchanged_completed_fixture_and_authoritative_parent() {
        let (draft, read) = ready();
        assert_eq!(validate_resume(&draft, &read), Ok("assistant"));
        let mut edited = draft.clone();
        edited.content.text = "personal draft".into();
        assert!(validate_resume(&edited, &read).is_err());
        let mut forked = read.clone();
        forked.current_node = Some("different".into());
        assert!(validate_resume(&draft, &forked).is_err());
        let mut cached = read.clone();
        cached.source = rpc::ChatConversationSource::LocalCache;
        assert!(validate_resume(&draft, &cached).is_err());
        let mut personal = read;
        personal.nodes[0].message.as_mut().unwrap().parts = vec![rpc::ChatMessagePart::Text {
            text: "personal message".into(),
        }];
        assert!(validate_resume(&draft, &personal).is_err());
    }

    #[test]
    fn resume_follows_finished_thinking_nodes_but_rejects_cycles_and_tool_branches() {
        let (draft, mut read) = ready();
        let mut thinking = read.nodes[1].clone();
        thinking.id = "thinking".into();
        thinking.message.as_mut().unwrap().id = "thinking".into();
        thinking.message.as_mut().unwrap().end_turn = false;
        read.nodes[1].parent_id = Some(thinking.id.clone());
        read.nodes.push(thinking);
        assert_eq!(validate_resume(&draft, &read), Ok("assistant"));
        read.nodes[2].parent_id = Some("thinking".into());
        assert!(validate_resume(&draft, &read).is_err());
        read.nodes[2].parent_id = Some("user".into());
        read.nodes[2].message.as_mut().unwrap().role = "tool".into();
        assert!(validate_resume(&draft, &read).is_err());
    }

    #[test]
    fn resume_rejects_ambiguous_pending_and_incomplete_generations() {
        let (draft, mut read) = ready();
        read.pending_submission = Some(serde_json::from_value(json!({
            "submissionId":"pending","clientMessageId":"message","draftId":"draft",
            "expectedParentId":"assistant","transmissionStarted":true,"accepted":false,"active":false,
            "content":draft.content
        })).unwrap());
        assert!(validate_resume(&draft, &read).is_err());
        read.pending_submission = None;
        read.nodes[1].message.as_mut().unwrap().finished = false;
        assert!(validate_resume(&draft, &read).is_err());
    }
}
