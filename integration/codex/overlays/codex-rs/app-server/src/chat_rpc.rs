//! Experimental Chat dispatch. Origin checks precede any scope, path, or content
//! access. Ordinary Codex request handling and transcript history are not used.
mod adapter;
mod events;
mod session;
mod uploads;

use crate::chat::SavedSession;
use crate::chat::Scope;
use crate::chat::Service;
use crate::chat::ServiceError;
use crate::connection_rpc_gate::ConnectionRpcGate;
use crate::message_processor::ConnectionSessionState;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::ConnectionRequestId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::RequestContext;
use crate::transport::ConnectionOrigin;
use codex_app_server_protocol as rpc;
use codex_chatgpt_chat::ChatError;
use codex_state::chat::ChatStoreError;
use session::Session;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(crate) struct ChatRpc {
    service: Arc<Service>,
    outgoing: Arc<OutgoingMessageSender>,
    subscribers: Mutex<HashMap<ConnectionId, Subscriber>>,
    uploads: Mutex<HashMap<Uuid, uploads::UploadOperation>>,
    stopped: CancellationToken,
    encoded_memory: Arc<tokio::sync::Semaphore>,
}

#[derive(Clone)]
struct Subscriber {
    scope: Option<Uuid>,
    gate: Arc<ConnectionRpcGate>,
    origin: ConnectionOrigin,
}

impl ChatRpc {
    pub(crate) fn new(service: Arc<Service>, outgoing: Arc<OutgoingMessageSender>) -> Arc<Self> {
        let this = Arc::new(Self {
            service,
            outgoing,
            subscribers: Mutex::new(HashMap::new()),
            uploads: Mutex::new(HashMap::new()),
            stopped: CancellationToken::new(),
            encoded_memory: Arc::new(tokio::sync::Semaphore::new(128 * 1024 * 1024)),
        });
        this.watch_availability();
        this
    }

    pub(crate) fn disconnect(&self, connection: ConnectionId) {
        lock(&self.subscribers).remove(&connection);
    }

    pub(crate) fn shutdown(&self) {
        self.stopped.cancel();
        lock(&self.subscribers).clear();
        let mut uploads = lock(&self.uploads);
        for upload in uploads.values() {
            upload.cancel.cancel();
        }
        uploads.clear();
    }

    pub(crate) async fn handle(
        self: &Arc<Self>,
        request: rpc::ClientRequest,
        request_id: ConnectionRequestId,
        connection: Arc<ConnectionSessionState>,
        context: RequestContext,
    ) -> Result<(), rpc::JSONRPCErrorError> {
        check_origin(connection.origin).map_err(adapter::error)?;
        let changes = self.service.changes();
        let revision = *changes.borrow();
        // Subscribe before an operation can finish, including immediate errors.
        let (_, current) = self.status(connection.origin).map_err(adapter::error)?;
        lock(&self.subscribers).insert(
            request_id.connection_id,
            Subscriber {
                scope: current.as_ref().map(|s| s.id()),
                gate: Arc::clone(&connection.rpc_gate),
                origin: connection.origin,
            },
        );
        let (payload, scope) = self
            .dispatch(request, connection.origin)
            .await
            .map_err(adapter::error)?;
        if context.cancellation.is_cancelled() || connection.rpc_gate.is_closed() {
            return Err(adapter::error(ChatError::LocalCancellation.into()));
        }
        lock(&self.subscribers).insert(
            request_id.connection_id,
            Subscriber {
                scope: scope.as_ref().map(|s| s.id()),
                gate: Arc::clone(&connection.rpc_gate),
                origin: connection.origin,
            },
        );
        self.outgoing
            .send_response_as_checked(request_id, payload, move || {
                if context.cancellation.is_cancelled() || connection.rpc_gate.is_closed() {
                    return Err(adapter::error(ChatError::LocalCancellation.into()));
                }
                if *changes.borrow() != revision {
                    return Err(adapter::error(ServiceError::ScopeChanged));
                }
                if let Some(scope) = scope {
                    scope.check().map_err(adapter::error)?;
                }
                Ok(())
            })
            .await;
        Ok(())
    }

    fn saved(
        &self,
        origin: ConnectionOrigin,
        scope: &str,
        persistence: rpc::ChatPersistence,
    ) -> Result<Arc<SavedSession>, ServiceError> {
        check_origin(origin)?;
        if persistence != rpc::ChatPersistence::Saved {
            return Err(ChatError::UnsupportedCapability.into());
        }
        self.service.open_saved(origin, Some(adapter::id(scope)?))
    }

    fn session(
        &self,
        origin: ConnectionOrigin,
        scope: &str,
        persistence: rpc::ChatPersistence,
    ) -> Result<Session, ServiceError> {
        check_origin(origin)?;
        let scope = Some(adapter::id(scope)?);
        match persistence {
            rpc::ChatPersistence::Saved => {
                self.service.open_saved(origin, scope).map(Session::Saved)
            }
            rpc::ChatPersistence::Temporary => self
                .service
                .open_temporary(origin, scope)
                .map(Session::Temporary),
        }
    }

    fn status(
        &self,
        origin: ConnectionOrigin,
    ) -> Result<(rpc::ChatStatusSnapshot, Option<Arc<Scope>>), ServiceError> {
        check_origin(origin)?;
        let (availability, scope) = match self.service.open_scope(origin, None) {
            Ok(scope) => (rpc::ChatAvailability::Available, Some(scope)),
            Err(ServiceError::Native(ChatError::Disabled)) => {
                (rpc::ChatAvailability::Disabled, None)
            }
            Err(ServiceError::Native(ChatError::LoginRequired)) => {
                (rpc::ChatAvailability::LoginRequired, None)
            }
            Err(error) => return Err(error),
        };
        let saved = scope
            .as_ref()
            .map(|scope| self.service.saved_in_scope(scope))
            .transpose()?
            .flatten();
        let temporary = scope
            .as_ref()
            .map(|scope| self.service.temporary_in_scope(scope))
            .transpose()?
            .flatten();
        let model_limits = saved
            .as_ref()
            .map(|session| session.model_limits())
            .transpose()?
            .flatten();
        let mut operations = saved
            .as_ref()
            .map(|session| session.operations())
            .transpose()?
            .unwrap_or_default();
        if let Some(temporary) = temporary {
            operations.extend(temporary.operations()?);
        }
        let scope_id = scope.as_ref().map(|scope| scope.id());
        let uploads = lock(&self.uploads)
            .values()
            .filter(|upload| Some(upload.scope_id) == scope_id)
            .map(uploads::UploadOperation::snapshot)
            .collect();
        Ok((
            rpc::ChatStatusSnapshot {
                generation: *self.service.changes().borrow(),
                availability,
                scope_id: scope_id.map(|id| id.to_string()),
                capabilities: rpc::ChatCapabilities {
                    saved: scope.is_some() && self.service.recovery_configured(),
                    temporary: scope.is_some(),
                    history: scope.is_some(),
                    attachments: if scope.is_some() {
                        vec![
                            rpc::ChatAttachmentKind::Png,
                            rpc::ChatAttachmentKind::Jpeg,
                            rpc::ChatAttachmentKind::Webp,
                            rpc::ChatAttachmentKind::Pdf,
                            rpc::ChatAttachmentKind::Utf8,
                        ]
                    } else {
                        Vec::new()
                    },
                    server_cancellation: false,
                    max_submission_bytes: 32 * 1024 * 1024,
                },
                model_limits: adapter::limits(model_limits),
                turns: operations
                    .iter()
                    .map(|operation| adapter::turn(operation))
                    .collect(),
                uploads,
            },
            scope,
        ))
    }

    async fn dispatch(
        self: &Arc<Self>,
        request: rpc::ClientRequest,
        origin: ConnectionOrigin,
    ) -> Result<(rpc::ClientResponsePayload, Option<Arc<Scope>>), ServiceError> {
        use rpc::ClientRequest as R;
        match request {
            R::ChatStatusRead { .. } => {
                let (response, session) = self.status(origin)?;
                Ok((rpc::ChatStatusReadResponse::from(response).into(), session))
            }
            R::ChatModelList { params, .. } => {
                let session = self.saved(origin, &params.scope_id, rpc::ChatPersistence::Saved)?;
                let catalog = session.model_catalog().await?;
                let operations = session.operations()?;
                let response = rpc::ChatModelListResponse {
                    scope_id: params.scope_id,
                    default_model: catalog.default_model,
                    model_limits: adapter::limits(catalog.limits),
                    models: catalog
                        .models
                        .into_iter()
                        .map(|model| {
                            let send_verified = operations.iter().any(|turn| {
                                turn.acceptance == codex_chatgpt_chat::stream::Acceptance::Accepted
                                    && turn.outcome
                                        == codex_chatgpt_chat::stream::GenerationOutcome::Completed
                                    && turn.resolved_model.as_deref() == Some(model.id.as_str())
                            });
                            rpc::ChatModel {
                                id: model.id,
                                title: model.title,
                                description: model.description,
                                send_verified,
                            }
                        })
                        .collect(),
                };
                Ok((response.into(), Some(session.scope())))
            }
            R::ChatConversationList { params, .. } => {
                let session = self.saved(origin, &params.scope_id, rpc::ChatPersistence::Saved)?;
                let page = session
                    .conversation_list(params.offset.unwrap_or(0), params.limit.unwrap_or(20))
                    .await?;
                let response = rpc::ChatConversationListResponse {
                    scope_id: params.scope_id,
                    items: page
                        .items
                        .into_iter()
                        .map(|item| rpc::ChatConversationSummary {
                            id: item.id,
                            title: item.title,
                            updated_at_ms: item.updated_at_ms,
                            archived: item.archived,
                            custom_gpt: item.custom_gpt,
                        })
                        .collect(),
                    next_offset: page.next_offset,
                    total: page.total,
                };
                Ok((response.into(), Some(session.scope())))
            }
            R::ChatConversationRead { params, .. } => {
                let cache_only = params.cache_only.unwrap_or(false);
                if cache_only && params.submission_id.is_some() {
                    return Err(ChatError::InvalidInput.into());
                }
                if cache_only && params.persistence == rpc::ChatPersistence::Temporary {
                    return Err(ChatError::UnsupportedCapability.into());
                }
                let session = self.session(origin, &params.scope_id, params.persistence)?;
                let mut reconciliation_error = None;
                if let Some(submission) = params.submission_id {
                    let Session::Saved(saved) = &session else {
                        return Err(ChatError::UnsupportedCapability.into());
                    };
                    let id = adapter::id(&submission)?;
                    let recorded = saved
                        .submission_read(id)
                        .await?
                        .ok_or(ChatStoreError::NotFound)?;
                    if recorded.conversation_id.as_deref() != Some(params.conversation_id.as_str())
                    {
                        return Err(ChatError::InvalidInput.into());
                    }
                    if let Err(error) = saved.reconcile_submission(id).await {
                        reconciliation_error = Some(adapter::code(error));
                    }
                    session.check()?;
                }
                let (snapshot, source, observed_at_ms) = session
                    .conversation_read(&params.conversation_id, cache_only)
                    .await?;
                let (pending, recovery_available) = session
                    .pending_for_conversation(&params.conversation_id)
                    .await?;
                let response = rpc::ChatConversationReadResponse {
                    scope_id: params.scope_id,
                    conversation_id: snapshot.id,
                    persistence: params.persistence,
                    source,
                    observed_at_ms,
                    title: snapshot.title,
                    updated_at_ms: snapshot.updated_at_ms,
                    current_node: snapshot.branch.current_node,
                    nodes: snapshot
                        .branch
                        .nodes
                        .into_iter()
                        .map(|node| rpc::ChatConversationNode {
                            id: node.id,
                            parent_id: node.parent_id,
                            message: node.message.as_ref().map(adapter::message),
                        })
                        .collect(),
                    other_branch_heads: snapshot.other_branch_heads,
                    default_model: snapshot.default_model,
                    continuation_blocker: snapshot.continuation_blocker.map(adapter::blocker),
                    pending_submission: pending,
                    recovery_available,
                    reconciliation_error,
                };
                Ok((response.into(), Some(session.scope())))
            }
            R::ChatDraftList { params, .. } => {
                let session = self.session(origin, &params.scope_id, params.persistence)?;
                let before = params
                    .before
                    .map(|cursor| {
                        Ok::<_, ServiceError>(codex_state::chat::ChatPageCursor {
                            updated_at_ms: cursor.updated_at_ms,
                            id: adapter::id(&cursor.id)?,
                        })
                    })
                    .transpose()?;
                let page = session
                    .draft_list(before, params.limit.unwrap_or(20) as usize)
                    .await?;
                let response = rpc::ChatDraftListResponse {
                    scope_id: params.scope_id,
                    items: page
                        .items
                        .into_iter()
                        .map(|draft| rpc::ChatDraftSummary {
                            id: draft.id.to_string(),
                            revision: draft.revision.to_string(),
                            persistence: params.persistence,
                            conversation_id: draft.conversation_id,
                            requested_model: draft.requested_model,
                            preview: draft.preview,
                            attachment_count: draft.attachment_count as u32,
                            updated_at_ms: draft.updated_at_ms,
                        })
                        .collect(),
                    next: page.next.map(|cursor| rpc::ChatDraftCursor {
                        id: cursor.id.to_string(),
                        updated_at_ms: cursor.updated_at_ms,
                    }),
                };
                Ok((response.into(), Some(session.scope())))
            }
            R::ChatDraftRead { params, .. } => {
                let session = self.session(origin, &params.scope_id, params.persistence)?;
                let draft_id = adapter::id(&params.draft_id)?;
                let draft = session
                    .draft_read(draft_id)
                    .await?
                    .map(|draft| adapter::draft(draft, params.persistence));
                let pending_submission = session.pending_for_draft(draft_id).await?;
                Ok((
                    rpc::ChatDraftReadResponse {
                        scope_id: params.scope_id,
                        draft,
                        pending_submission,
                    }
                    .into(),
                    Some(session.scope()),
                ))
            }
            R::ChatDraftWrite { params, .. } => {
                let session = self.session(origin, &params.scope_id, params.persistence)?;
                let draft = session
                    .draft_write(
                        adapter::id(&params.draft_id)?,
                        params
                            .expected_revision
                            .as_deref()
                            .map(adapter::id)
                            .transpose()?,
                        adapter::draft_content(params.content)?,
                    )
                    .await?;
                Ok((
                    rpc::ChatDraftWriteResponse {
                        scope_id: params.scope_id,
                        draft: adapter::draft(draft, params.persistence),
                    }
                    .into(),
                    Some(session.scope()),
                ))
            }
            R::ChatDraftDelete { params, .. } => {
                let session = self.session(origin, &params.scope_id, params.persistence)?;
                session
                    .draft_delete(
                        adapter::id(&params.draft_id)?,
                        adapter::id(&params.expected_revision)?,
                    )
                    .await?;
                Ok((
                    rpc::ChatDraftDeleteResponse {
                        scope_id: params.scope_id,
                    }
                    .into(),
                    Some(session.scope()),
                ))
            }
            R::ChatTurnStart { params, .. } => {
                let session = self.session(origin, &params.scope_id, params.persistence)?;
                let draft_id = adapter::id(&params.draft_id)?;
                let revision = adapter::id(&params.expected_revision)?;
                if params.text.len() > codex_state::chat::MAX_CHAT_TEXT_BYTES {
                    return Err(ChatError::InvalidInput.into());
                }
                let draft = session
                    .draft_read(draft_id)
                    .await?
                    .ok_or(ChatStoreError::NotFound)?;
                if draft.revision != revision || draft.content.text != params.text {
                    return Err(ChatStoreError::RevisionConflict.into());
                }
                let operation = session
                    .start_turn(crate::chat::StartSavedTurn {
                        draft_id,
                        revision,
                        submission_id: adapter::id(&params.submission_id)?,
                        client_message_id: adapter::id(&params.client_message_id)?,
                        expected_parent_id: params.expected_parent_id,
                        upload_handles: params
                            .upload_handles
                            .iter()
                            .map(|id| adapter::id(id))
                            .collect::<Result<_, _>>()?,
                    })
                    .await?;
                let turn = adapter::turn(&operation.snapshot());
                self.watch_turn(session.scope(), operation);
                Ok((
                    rpc::ChatTurnStartResponse {
                        scope_id: params.scope_id,
                        turn,
                    }
                    .into(),
                    Some(session.scope()),
                ))
            }
            R::ChatTurnCancel { params, .. } => {
                let scope = self
                    .service
                    .open_scope(origin, Some(adapter::id(&params.scope_id)?))?;
                let id = adapter::id(&params.operation_id)?;
                let mut turn = None;
                if let Some(saved) = self.service.saved_in_scope(&scope)? {
                    turn = saved.cancel_turn(id)?;
                }
                if turn.is_none()
                    && let Some(temporary) = self.service.temporary_in_scope(&scope)?
                {
                    turn = temporary.cancel_turn(id)?;
                }
                let turn = turn.as_ref().map(|turn| adapter::turn(turn));
                let local_stop_requested = turn
                    .as_ref()
                    .is_some_and(|turn| turn.phase != rpc::ChatTurnPhase::Finished);
                Ok((
                    rpc::ChatTurnCancelResponse {
                        scope_id: params.scope_id,
                        turn,
                        local_stop_requested,
                        server_cancellation_confirmed: false,
                    }
                    .into(),
                    Some(scope),
                ))
            }
            R::ChatAttachmentUpload { params, .. } => {
                let session = self.session(origin, &params.scope_id, params.persistence)?;
                let scope_id = params.scope_id.clone();
                let operation_id = self.start_upload(session.clone(), params).await?;
                Ok((
                    rpc::ChatAttachmentUploadResponse {
                        scope_id,
                        operation_id: operation_id.to_string(),
                    }
                    .into(),
                    Some(session.scope()),
                ))
            }
            R::ChatAttachmentCancel { params, .. } => {
                let scope = self
                    .service
                    .open_scope(origin, Some(adapter::id(&params.scope_id)?))?;
                let id = adapter::id(&params.operation_id)?;
                let persistence = lock(&self.uploads)
                    .get(&id)
                    .filter(|upload| upload.scope_id == scope.id())
                    .map(|upload| upload.snapshot().persistence);
                let upload = if let Some(persistence) = persistence {
                    let session = self.session(origin, &params.scope_id, persistence)?;
                    self.cancel_upload(&session, id)?
                } else {
                    None
                };
                let local_stop_requested = upload.is_some();
                if let Some(upload) = &upload {
                    self.notify_scope(
                        &scope,
                        rpc::ServerNotification::ChatAttachment(rpc::ChatAttachmentNotification {
                            upload: upload.clone(),
                        }),
                    )
                    .await;
                }
                Ok((
                    rpc::ChatAttachmentCancelResponse {
                        scope_id: params.scope_id,
                        upload,
                        local_stop_requested,
                        server_cancellation_confirmed: false,
                    }
                    .into(),
                    Some(scope),
                ))
            }
            _ => Err(ChatError::UnsupportedCapability.into()),
        }
    }
}

impl Drop for ChatRpc {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(crate) fn check_origin(origin: ConnectionOrigin) -> Result<(), ServiceError> {
    match origin {
        ConnectionOrigin::Stdio | ConnectionOrigin::InProcess | ConnectionOrigin::LocalSocket => {
            Ok(())
        }
        ConnectionOrigin::WebSocket | ConnectionOrigin::RemoteControl => {
            Err(ServiceError::RemoteConnection)
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn rpc_error(error: ServiceError) -> rpc::JSONRPCErrorError {
    adapter::error(error)
}

#[cfg(test)]
mod tests;
