use crate::chat::ServiceError;
use codex_app_server_protocol as rpc;
use codex_chatgpt_chat::ChatError;
use codex_state::chat as state;
use uuid::Uuid;

pub(super) fn id(value: &str) -> Result<Uuid, ServiceError> {
    Uuid::parse_str(value).map_err(|_| ChatError::InvalidInput.into())
}

pub(super) fn error(error: ServiceError) -> rpc::JSONRPCErrorError {
    rpc::JSONRPCErrorError {
        code: -32002,
        message: error.to_string(),
        data: Some(serde_json::json!(rpc::ChatErrorDetails {
            code: code(error)
        })),
    }
}

pub(super) fn code(error: ServiceError) -> rpc::ChatErrorCode {
    use rpc::ChatErrorCode as C;
    match error {
        ServiceError::RemoteConnection => C::RemoteConnection,
        ServiceError::ScopeChanged => C::ScopeChanged,
        ServiceError::OverlappingTurn => C::OverlappingTurn,
        ServiceError::Capacity => C::Capacity,
        ServiceError::Native(error) => match error {
            ChatError::Disabled => C::Disabled,
            ChatError::LoginRequired => C::LoginRequired,
            ChatError::Authentication => C::Authentication,
            ChatError::IdentityChanged => C::IdentityChanged,
            ChatError::SessionChanged => C::SessionChanged,
            ChatError::AccountMismatch => C::AccountMismatch,
            ChatError::CloudflareChallenge => C::CloudflareChallenge,
            ChatError::UnsupportedChallenge => C::UnsupportedChallenge,
            ChatError::AccessDenied => C::AccessDenied,
            ChatError::RateLimited => C::RateLimited,
            ChatError::PreparationCooldown => C::PreparationCooldown,
            ChatError::PreparationInProgress => C::PreparationInProgress,
            ChatError::NotFound => C::NotFound,
            ChatError::UnsupportedCapability => C::UnsupportedCapability,
            ChatError::ProtocolChanged => C::ProtocolChanged,
            ChatError::ResponseTooLarge => C::ResponseTooLarge,
            ChatError::NetworkPolicy => C::NetworkPolicy,
            ChatError::Transport => C::Transport,
            ChatError::Timeout => C::Timeout,
            ChatError::LocalCancellation => C::LocalCancellation,
            ChatError::InvalidInput => C::InvalidInput,
            ChatError::RecoveryUnavailable => C::RecoveryUnavailable,
            ChatError::PendingSubmission => C::PendingSubmission,
            ChatError::BranchConflict => C::BranchConflict,
            ChatError::IncompleteStream => C::IncompleteStream,
            ChatError::UploadFailed => C::UploadFailed,
            ChatError::ProcessingIncomplete => C::ProcessingIncomplete,
            ChatError::StaleUpload => C::StaleUpload,
        },
        ServiceError::Store(error) => match error {
            state::ChatStoreError::Unavailable => C::RecoveryUnavailable,
            state::ChatStoreError::InvalidInput => C::InvalidInput,
            state::ChatStoreError::InvalidRecord => C::InvalidRecord,
            state::ChatStoreError::RevisionConflict => C::RevisionConflict,
            state::ChatStoreError::NotFound => C::NotFound,
            state::ChatStoreError::PendingSubmission => C::PendingSubmission,
            state::ChatStoreError::DuplicateSubmission => C::DuplicateSubmission,
            state::ChatStoreError::LeaseLost => C::LeaseLost,
            state::ChatStoreError::ReconcileOnly => C::ReconcileOnly,
            state::ChatStoreError::InvalidTransition => C::InvalidTransition,
            state::ChatStoreError::ProfileMismatch => C::ProfileMismatch,
            state::ChatStoreError::Capacity => C::Capacity,
        },
        ServiceError::Attachment(error) => match error {
            codex_chatgpt_chat::attachment::AttachmentError::Unreadable => C::AttachmentUnreadable,
            codex_chatgpt_chat::attachment::AttachmentError::Changed => C::AttachmentChanged,
            codex_chatgpt_chat::attachment::AttachmentError::InvalidFormat => {
                C::AttachmentInvalidFormat
            }
            codex_chatgpt_chat::attachment::AttachmentError::Limit => C::AttachmentLimit,
            codex_chatgpt_chat::attachment::AttachmentError::Capacity => C::Capacity,
            codex_chatgpt_chat::attachment::AttachmentError::Cancelled => C::LocalCancellation,
        },
    }
}

pub(super) fn draft_content(
    content: rpc::ChatDraftContent,
) -> Result<state::ChatDraftContent, ServiceError> {
    if content.text.len() > state::MAX_CHAT_TEXT_BYTES
        || content.selections.len() > state::MAX_CHAT_SELECTIONS
    {
        return Err(ChatError::InvalidInput.into());
    }
    Ok(state::ChatDraftContent {
        conversation_id: content.conversation_id,
        expected_parent_id: content.expected_parent_id,
        requested_model: content.requested_model,
        text: content.text,
        selections: content
            .selections
            .into_iter()
            .map(|selected| {
                Ok(state::ChatFileSelection {
                    id: id(&selected.id)?,
                    source: selected.source.map(std::path::PathBuf::from),
                    name: selected.name,
                    snapshot: selected
                        .snapshot
                        .map(|snapshot| {
                            let dimensions = match (snapshot.width, snapshot.height) {
                                (None, None) => None,
                                (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
                                _ => return Err(ChatError::InvalidInput),
                            };
                            Ok(state::ChatFileSnapshot {
                                sha256: snapshot.sha256,
                                size: snapshot.size,
                                modified_at_ms: snapshot.modified_at_ms,
                                kind: file_kind(snapshot.kind),
                                dimensions,
                            })
                        })
                        .transpose()?,
                })
            })
            .collect::<Result<_, ServiceError>>()?,
    })
}

pub(super) fn content(content: state::ChatDraftContent) -> rpc::ChatDraftContent {
    rpc::ChatDraftContent {
        conversation_id: content.conversation_id,
        expected_parent_id: content.expected_parent_id,
        requested_model: content.requested_model,
        text: content.text,
        selections: content
            .selections
            .into_iter()
            .map(|selected| rpc::ChatFileSelection {
                id: selected.id.to_string(),
                source: selected.source.map(|p| p.to_string_lossy().into_owned()),
                name: selected.name,
                snapshot: selected.snapshot.map(file_snapshot),
            })
            .collect(),
    }
}

pub(super) fn draft(draft: state::ChatDraft, persistence: rpc::ChatPersistence) -> rpc::ChatDraft {
    rpc::ChatDraft {
        id: draft.id.to_string(),
        revision: draft.revision.to_string(),
        persistence,
        content: content(draft.content),
        updated_at_ms: draft.updated_at_ms,
    }
}

fn file_kind(kind: rpc::ChatAttachmentKind) -> state::ChatFileKind {
    match kind {
        rpc::ChatAttachmentKind::Png => state::ChatFileKind::Png,
        rpc::ChatAttachmentKind::Jpeg => state::ChatFileKind::Jpeg,
        rpc::ChatAttachmentKind::Webp => state::ChatFileKind::Webp,
        rpc::ChatAttachmentKind::Pdf => state::ChatFileKind::Pdf,
        rpc::ChatAttachmentKind::Utf8 => state::ChatFileKind::Utf8,
    }
}

pub(super) fn file_snapshot(snapshot: state::ChatFileSnapshot) -> rpc::ChatFileSnapshot {
    rpc::ChatFileSnapshot {
        sha256: snapshot.sha256,
        size: snapshot.size,
        modified_at_ms: snapshot.modified_at_ms,
        kind: match snapshot.kind {
            state::ChatFileKind::Png => rpc::ChatAttachmentKind::Png,
            state::ChatFileKind::Jpeg => rpc::ChatAttachmentKind::Jpeg,
            state::ChatFileKind::Webp => rpc::ChatAttachmentKind::Webp,
            state::ChatFileKind::Pdf => rpc::ChatAttachmentKind::Pdf,
            state::ChatFileKind::Utf8 => rpc::ChatAttachmentKind::Utf8,
        },
        width: snapshot.dimensions.map(|(w, _)| w),
        height: snapshot.dimensions.map(|(_, h)| h),
    }
}

pub(super) fn temporary_pending(
    pending: crate::chat::TemporaryPending,
) -> rpc::ChatPendingSubmission {
    rpc::ChatPendingSubmission {
        submission_id: pending.submission_id.to_string(),
        client_message_id: pending.client_message_id.to_string(),
        draft_id: pending.draft_id.to_string(),
        conversation_id: pending.conversation_id,
        expected_parent_id: pending.expected_parent_id,
        transmission_started: pending.transmission_started,
        accepted: pending.accepted,
        active: pending.active,
        content: content(pending.content),
    }
}

pub(super) fn pending(submission: state::ChatSubmission) -> rpc::ChatPendingSubmission {
    rpc::ChatPendingSubmission {
        submission_id: submission.submission_id.to_string(),
        client_message_id: submission.client_message_id.to_string(),
        draft_id: submission.draft_id.to_string(),
        conversation_id: submission.conversation_id,
        expected_parent_id: submission.expected_parent_id,
        transmission_started: submission.transmission_started,
        accepted: submission.accepted,
        active: submission.active,
        content: content(submission.snapshot.draft),
    }
}

pub(super) fn message(message: &codex_chatgpt_chat::stream::Message) -> rpc::ChatMessage {
    rpc::ChatMessage {
        id: message.id.clone(),
        role: message.role.clone(),
        parts: message
            .parts
            .iter()
            .map(|part| match part {
                codex_chatgpt_chat::stream::MessagePart::Text(text) => {
                    rpc::ChatMessagePart::Text { text: text.clone() }
                }
                codex_chatgpt_chat::stream::MessagePart::Image(reference) => {
                    rpc::ChatMessagePart::Image {
                        reference: rpc::ChatAttachmentReference {
                            file_id: reference.file_id.clone(),
                            name: None,
                            mime_type: None,
                            size_bytes: reference.size_bytes,
                            width: reference.width,
                            height: reference.height,
                        },
                    }
                }
                codex_chatgpt_chat::stream::MessagePart::Unsupported { content_type } => {
                    rpc::ChatMessagePart::Unsupported {
                        content_type: content_type.clone(),
                    }
                }
            })
            .collect(),
        model: message.model.clone(),
        finished: message.finished,
        end_turn: message.end_turn,
        attachments: message
            .attachments
            .iter()
            .map(|reference| rpc::ChatAttachmentReference {
                file_id: reference.file_id.clone(),
                name: Some(reference.name.clone()),
                mime_type: reference.mime_type.clone(),
                size_bytes: reference.size_bytes,
                width: reference.width,
                height: reference.height,
            })
            .collect(),
        citations: message
            .citations
            .iter()
            .map(|citation| rpc::ChatCitation {
                marker: citation.marker.clone(),
                urls: citation.urls.clone(),
            })
            .collect(),
    }
}

pub(super) fn turn(snapshot: &crate::chat::OperationSnapshot) -> rpc::ChatTurnSnapshot {
    rpc::ChatTurnSnapshot {
        scope_id: snapshot.scope_id.to_string(),
        operation_id: snapshot.operation_id.to_string(),
        draft_id: snapshot.draft_id.to_string(),
        submission_id: snapshot.submission_id.to_string(),
        client_message_id: snapshot.client_message_id.to_string(),
        conversation_id: snapshot.conversation_id.clone(),
        persistence: match snapshot.persistence {
            crate::chat::Persistence::Saved => rpc::ChatPersistence::Saved,
            crate::chat::Persistence::Temporary => rpc::ChatPersistence::Temporary,
        },
        sequence: snapshot.sequence,
        phase: match snapshot.phase {
            crate::chat::OperationPhase::Preparing => rpc::ChatTurnPhase::Preparing,
            crate::chat::OperationPhase::Sending => rpc::ChatTurnPhase::Sending,
            crate::chat::OperationPhase::Finished => rpc::ChatTurnPhase::Finished,
        },
        acceptance: match snapshot.acceptance {
            codex_chatgpt_chat::stream::Acceptance::Unknown => rpc::ChatAcceptance::Unknown,
            codex_chatgpt_chat::stream::Acceptance::Accepted => rpc::ChatAcceptance::Accepted,
        },
        outcome: match snapshot.outcome {
            codex_chatgpt_chat::stream::GenerationOutcome::Incomplete => {
                rpc::ChatGenerationOutcome::Incomplete
            }
            codex_chatgpt_chat::stream::GenerationOutcome::Completed => {
                rpc::ChatGenerationOutcome::Completed
            }
            codex_chatgpt_chat::stream::GenerationOutcome::Failed => {
                rpc::ChatGenerationOutcome::Failed
            }
            codex_chatgpt_chat::stream::GenerationOutcome::UnsupportedHandoff => {
                rpc::ChatGenerationOutcome::UnsupportedHandoff
            }
        },
        requested_model: snapshot.requested_model.clone(),
        resolved_model: snapshot.resolved_model.clone(),
        messages: snapshot.messages.iter().map(message).collect(),
        error: snapshot.error.map(code),
        recovery_pending: snapshot.recovery_pending,
    }
}

pub(super) fn blocker(
    blocker: codex_chatgpt_chat::conversation::ContinuationBlocker,
) -> rpc::ChatContinuationBlocker {
    use codex_chatgpt_chat::conversation::ContinuationBlocker as B;
    match blocker {
        B::ReadOnly => rpc::ChatContinuationBlocker::ReadOnly,
        B::CustomGpt => rpc::ChatContinuationBlocker::CustomGpt,
        B::Temporary => rpc::ChatContinuationBlocker::Temporary,
        B::UnsupportedMode => rpc::ChatContinuationBlocker::UnsupportedMode,
        B::EmptyConversation => rpc::ChatContinuationBlocker::EmptyConversation,
        B::IncompleteGeneration => rpc::ChatContinuationBlocker::IncompleteGeneration,
        B::UnsupportedContent => rpc::ChatContinuationBlocker::UnsupportedContent,
    }
}

pub(super) fn limits(
    limits: Option<Vec<codex_chatgpt_chat::models::ModelLimit>>,
) -> Option<Vec<rpc::ChatModelLimit>> {
    limits.map(|limits| {
        limits
            .into_iter()
            .map(|limit| rpc::ChatModelLimit {
                model: limit.model,
                resets_at_ms: limit.resets_at_ms,
            })
            .collect()
    })
}
