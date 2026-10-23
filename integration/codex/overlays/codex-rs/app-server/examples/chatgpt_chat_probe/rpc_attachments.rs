//! Explicit five-format upload acceptance through the app-server RPC boundary.
//! Fixture contents and remote/local IDs never enter the capability report.

use super::*;
use crate::fixtures::SPECIFICATIONS;
use codex_chatgpt_chat::RpcAttachmentObservation;
use codex_chatgpt_chat::attachment::AttachmentKind;
use codex_chatgpt_chat::attachment::AttachmentStager;
use std::path::Path;
use tokio_util::sync::CancellationToken;

pub(super) async fn run(
    client: &mut Client,
    scope: &str,
    directory: &Path,
    observation: &mut RpcObservation,
) -> Result<(), String> {
    let directory = directory.canonicalize().map_err(|_| "fixture_unreadable")?;
    let files = AttachmentStager::new()
        .snapshot(
            SPECIFICATIONS
                .iter()
                .map(|(name, ..)| directory.join(name))
                .collect(),
            CancellationToken::new(),
        )
        .await
        .map_err(|_| "fixture_invalid")?;
    if files
        .iter()
        .zip(SPECIFICATIONS)
        .any(|(file, (_, _, digest, kind, _))| {
            file.metadata().sha256 != digest || file.metadata().kind != kind
        })
    {
        return Err("fixture_digest_mismatch".into());
    }
    let snapshots: Vec<_> = files.iter().map(|file| {
        let metadata = file.metadata();
        json!({
            "sha256":metadata.sha256,"size":metadata.size,
            "modifiedAtMs":metadata.modified.and_then(|time|time.duration_since(std::time::UNIX_EPOCH).ok()).and_then(|duration|i64::try_from(duration.as_millis()).ok()),
            "kind":metadata.kind,"width":metadata.dimensions.map(|(width,_)|width),"height":metadata.dimensions.map(|(_,height)|height)
        })
    }).collect();
    drop(files);
    let mut branch = Branch {
        conversation: None,
        parent: Uuid::new_v4().to_string(),
    };
    for (index, (name, expected, digest, kind, _)) in SPECIFICATIONS.into_iter().enumerate() {
        let mut item = RpcAttachmentObservation {
            kind,
            upload_updates: 0,
            sequence_ordered: true,
            ready_metadata_matched: false,
            accepted: false,
            completed: false,
            fixture_response_matched: false,
            authoritative_retrieval: false,
            parent_matched: false,
            history_attachment_present: false,
            pending_cleared: false,
            error: None,
        };
        let result = turn(
            client,
            scope,
            Fixture {
                path: &directory.join(name),
                snapshot: &snapshots[index],
                expected,
                digest,
                index,
            },
            &mut branch,
            observation,
            &mut item,
        )
        .await;
        if let Err(error) = &result {
            item.error = Some(error.clone());
        }
        observation.attachments.push(item);
        result?;
    }
    Ok(())
}

struct Branch {
    conversation: Option<String>,
    parent: String,
}

struct Fixture<'a> {
    path: &'a Path,
    snapshot: &'a serde_json::Value,
    expected: &'a str,
    digest: &'a str,
    index: usize,
}

async fn turn(
    client: &mut Client,
    scope: &str,
    fixture: Fixture<'_>,
    branch: &mut Branch,
    observation: &mut RpcObservation,
    item: &mut RpcAttachmentObservation,
) -> Result<(), String> {
    let Fixture {
        path,
        snapshot,
        expected,
        digest,
        index,
    } = fixture;
    let Branch {
        conversation,
        parent,
    } = branch;
    let text = "Read only the file attached to this message. Reply with exactly its verification code and no other text.";
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("fixture_name_invalid")?;
    let path = path.to_str().ok_or("fixture_path_invalid")?;
    let draft_id = Uuid::new_v4().to_string();
    let selection_id = Uuid::new_v4().to_string();
    let client_message_id = Uuid::new_v4().to_string();
    let id = 100 + index as i64 * 10;
    let draft: rpc::ChatDraftWriteResponse = call(
        client,
        id,
        "chat/draft/write",
        json!({
            "scopeId":scope,"persistence":"saved","draftId":draft_id,
            "content":{"conversationId":conversation,"expectedParentId":parent,"text":text,
                "selections":[{"id":selection_id,"source":path,"name":name,"snapshot":snapshot}]}
        }),
    )
    .await?;
    let upload: rpc::ChatAttachmentUploadResponse = call(client, id+1, "chat/attachment/upload", json!({
        "scopeId":scope,"persistence":"saved","draftId":draft_id,"expectedRevision":draft.draft.revision,
        "selections":[{"type":"localFile","selectionId":selection_id,"path":path}]
    })).await?;
    let ready = wait_for_upload(client, scope, &draft_id, &upload.operation_id, item).await?;
    let (revision, handle) = ready_file(&ready, &selection_id, digest, item.kind)?;
    item.ready_metadata_matched = true;
    let start: rpc::ChatTurnStartResponse = call(client, id+2, "chat/turn/start", json!({
        "scopeId":scope,"persistence":"saved","draftId":draft_id,"expectedRevision":revision,
        "submissionId":Uuid::new_v4(),"clientMessageId":client_message_id,"expectedParentId":parent,
        "text":text,"uploadHandles":[handle]
    })).await?;
    let turn = wait_for_turn(client, scope, &start.turn.operation_id, observation).await?;
    if turn.persistence != rpc::ChatPersistence::Saved
        || turn.draft_id != draft_id
        || turn.client_message_id != client_message_id
    {
        return Err("turn_scope_mismatch".into());
    }
    item.accepted = turn.acceptance == rpc::ChatAcceptance::Accepted;
    item.completed = turn.outcome == rpc::ChatGenerationOutcome::Completed;
    item.fixture_response_matched = turn
        .messages
        .iter()
        .any(|message| fixture_matches(message, expected));
    observation
        .requested_model
        .clone_from(&turn.requested_model);
    observation.resolved_model.clone_from(&turn.resolved_model);
    observation.accepted = item.accepted;
    observation.completed = item.completed;
    observation.fixture_response_matched = item.fixture_response_matched;
    if let Some(error) = turn.error {
        return Err(error_code(error));
    }
    let conversation_id = turn.conversation_id.ok_or("conversation_id_missing")?;
    if conversation
        .as_ref()
        .is_some_and(|prior| prior != &conversation_id)
    {
        return Err("attachment_conversation_changed".into());
    }
    let read: rpc::ChatConversationReadResponse = call(
        client,
        id + 3,
        "chat/conversation/read",
        json!({
            "scopeId":scope,"persistence":"saved","conversationId":conversation_id
        }),
    )
    .await?;
    let user = read.nodes.iter().find(|node| {
        node.message
            .as_ref()
            .is_some_and(|message| message.role == "user" && message.id == client_message_id)
    });
    item.parent_matched =
        user.is_some_and(|node| node.parent_id.as_deref() == Some(parent.as_str()));
    item.history_attachment_present = user.and_then(|node| node.message.as_ref()).is_some_and(|message| {
        if item.kind.is_image() {
            message.parts.iter().any(|part| matches!(part, rpc::ChatMessagePart::Image { reference } if !reference.file_id.is_empty()))
        } else {
            message.attachments.iter().any(|file| file.name.as_deref()==Some(name) && !file.file_id.is_empty())
        }
    });
    item.authoritative_retrieval = read.source == rpc::ChatConversationSource::Authoritative
        && read.nodes.iter().any(|node| {
            node.message
                .as_ref()
                .is_some_and(|message| fixture_matches(message, expected))
        });
    item.pending_cleared = read.recovery_available && read.pending_submission.is_none();
    observation.authoritative_retrieval = item.authoritative_retrieval;
    observation.pending_cleared = item.pending_cleared;
    if !item.accepted
        || !item.completed
        || !item.fixture_response_matched
        || !item.authoritative_retrieval
        || !item.parent_matched
        || !item.history_attachment_present
        || !item.pending_cleared
        || !item.sequence_ordered
        || !observation.sequence_ordered
        || read.continuation_blocker.is_some()
    {
        return Err("rpc_attachment_acceptance_incomplete".into());
    }
    *parent = read.current_node.ok_or("attachment_parent_missing")?;
    *conversation = Some(conversation_id);
    Ok(())
}

fn ready_file<'a>(
    upload: &'a rpc::ChatAttachmentSnapshot,
    selection_id: &str,
    digest: &str,
    kind: AttachmentKind,
) -> Result<(&'a str, &'a str), String> {
    if let Some(error) = upload.error {
        return Err(error_code(error));
    }
    let file = upload
        .files
        .first()
        .filter(|_| upload.files.len() == 1)
        .ok_or("upload_files_mismatch")?;
    if !upload.finished
        || file.selection_id != selection_id
        || file.stage != rpc::ChatAttachmentStage::Ready
        || file.error.is_some()
    {
        return Err("upload_not_ready".into());
    }
    let metadata = file.metadata.as_ref().ok_or("upload_metadata_missing")?;
    let expected_kind = match kind {
        AttachmentKind::Png => rpc::ChatAttachmentKind::Png,
        AttachmentKind::Jpeg => rpc::ChatAttachmentKind::Jpeg,
        AttachmentKind::Webp => rpc::ChatAttachmentKind::Webp,
        AttachmentKind::Pdf => rpc::ChatAttachmentKind::Pdf,
        AttachmentKind::Utf8 => rpc::ChatAttachmentKind::Utf8,
    };
    if metadata.sha256 != digest || metadata.kind != expected_kind {
        return Err("upload_fixture_mismatch".into());
    }
    Ok((
        upload
            .draft_revision
            .as_deref()
            .ok_or("upload_revision_missing")?,
        file.handle.as_deref().ok_or("upload_handle_missing")?,
    ))
}

async fn wait_for_upload(
    client: &mut Client,
    scope: &str,
    draft: &str,
    operation: &str,
    item: &mut RpcAttachmentObservation,
) -> Result<rpc::ChatAttachmentSnapshot, String> {
    let mut sequence = None;
    loop {
        match client.next_event().await.ok_or("rpc_event_stream_closed")? {
            AppServerEvent::ServerNotification(notification) => {
                let rpc::ServerNotification::ChatAttachment(event) = *notification else {
                    continue;
                };
                let upload = event.upload;
                if upload.scope_id != scope || upload.operation_id != operation {
                    continue;
                }
                if upload.draft_id != draft || upload.persistence != rpc::ChatPersistence::Saved {
                    return Err("upload_scope_mismatch".into());
                }
                if sequence.is_some_and(|prior| upload.sequence <= prior) {
                    item.sequence_ordered = false;
                    return Err("upload_sequence_out_of_order".into());
                }
                sequence = Some(upload.sequence);
                item.upload_updates += 1;
                if upload.finished {
                    return Ok(upload);
                }
            }
            AppServerEvent::Disconnected { .. } => return Err("rpc_event_stream_closed".into()),
            AppServerEvent::Lagged { .. } => return Err("rpc_event_lagged".into()),
            AppServerEvent::ServerRequest(_) => return Err("unexpected_agent_request".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_requires_processed_digest_bound_file_and_new_revision() {
        let mut upload = rpc::ChatAttachmentSnapshot {
            scope_id: "scope".into(),
            operation_id: "operation".into(),
            draft_id: "draft".into(),
            persistence: rpc::ChatPersistence::Saved,
            sequence: 8,
            draft_revision: Some("new-revision".into()),
            finished: true,
            error: None,
            files: vec![rpc::ChatAttachmentProgress {
                selection_id: "selected".into(),
                handle: Some("handle".into()),
                stage: rpc::ChatAttachmentStage::Ready,
                percent: Some(100),
                error: None,
                metadata: Some(rpc::ChatFileSnapshot {
                    sha256: "digest".into(),
                    size: 100,
                    modified_at_ms: None,
                    kind: rpc::ChatAttachmentKind::Pdf,
                    width: None,
                    height: None,
                }),
            }],
        };
        assert_eq!(
            ready_file(&upload, "selected", "digest", AttachmentKind::Pdf).unwrap(),
            ("new-revision", "handle")
        );
        assert!(ready_file(&upload, "other", "digest", AttachmentKind::Pdf).is_err());
        assert!(ready_file(&upload, "selected", "changed", AttachmentKind::Pdf).is_err());
        assert!(ready_file(&upload, "selected", "digest", AttachmentKind::Png).is_err());
        upload.files[0].stage = rpc::ChatAttachmentStage::Processing;
        assert!(ready_file(&upload, "selected", "digest", AttachmentKind::Pdf).is_err());
        upload.files[0].stage = rpc::ChatAttachmentStage::Ready;
        upload.draft_revision = None;
        assert!(ready_file(&upload, "selected", "digest", AttachmentKind::Pdf).is_err());
    }
}
