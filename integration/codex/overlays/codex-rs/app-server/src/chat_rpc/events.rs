use super::*;
use std::time::Duration;

impl ChatRpc {
    pub(super) fn watch_availability(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let mut changes = self.service.changes();
        let stopped = self.stopped.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! { biased; _ = stopped.cancelled() => break, result = changes.changed() => if result.is_err() { break; } }
                let revision = *changes.borrow_and_update();
                let Some(rpc) = weak.upgrade() else { break };
                // Revocation also releases operations when every client has disconnected.
                {
                    let mut uploads = lock(&rpc.uploads);
                    let scope = rpc
                        .service
                        .open_scope(ConnectionOrigin::InProcess, None)
                        .ok()
                        .map(|session| session.id());
                    uploads.retain(|_, upload| {
                        if Some(upload.scope_id) == scope {
                            true
                        } else {
                            upload.cancel.cancel();
                            false
                        }
                    });
                }
                let subscribers: Vec<_> = lock(&rpc.subscribers)
                    .iter()
                    .map(|(id, sub)| (*id, sub.clone()))
                    .collect();
                for (connection, subscriber) in subscribers {
                    if subscriber.gate.is_closed() {
                        rpc.disconnect(connection);
                        continue;
                    }
                    let Ok((status, session)) = rpc.status(subscriber.origin) else {
                        continue;
                    };
                    let scope = session.as_ref().map(|s| s.id());
                    if let Some(current) = lock(&rpc.subscribers).get_mut(&connection) {
                        current.scope = scope;
                    }
                    let check = rpc.service.changes();
                    rpc.outgoing
                        .send_chat_notification_checked(
                            connection,
                            rpc::ServerNotification::ChatAvailability(
                                rpc::ChatAvailabilityNotification { status },
                            ),
                            move || {
                                !subscriber.gate.is_closed()
                                    && *check.borrow() == revision
                                    && session.as_ref().is_none_or(|s| s.check().is_ok())
                            },
                        )
                        .await;
                }
            }
        });
    }

    pub(super) async fn notify_scope(
        &self,
        session: &Arc<Scope>,
        notification: rpc::ServerNotification,
    ) {
        if session.check().is_err() {
            return;
        }
        let targets: Vec<_> = lock(&self.subscribers)
            .iter()
            .filter(|(_, subscriber)| {
                subscriber.scope == Some(session.id()) && !subscriber.gate.is_closed()
            })
            .map(|(id, subscriber)| (*id, Arc::clone(&subscriber.gate)))
            .collect();
        for (connection, gate) in targets {
            let session = Arc::clone(session);
            self.outgoing
                .send_chat_notification_checked(connection, notification.clone(), move || {
                    !gate.is_closed() && session.check().is_ok()
                })
                .await;
        }
    }

    pub(super) fn watch_turn(
        self: &Arc<Self>,
        session: Arc<Scope>,
        operation: crate::chat::Operation,
    ) {
        let weak = Arc::downgrade(self);
        let mut updates = operation.subscribe();
        let mut changes = self.service.changes();
        let stopped = self.stopped.clone();
        tokio::spawn(async move {
            let mut conversation = None;
            loop {
                if session.check().is_err() {
                    break;
                }
                let current = Arc::clone(&updates.borrow_and_update());
                let Some(rpc) = weak.upgrade() else { break };
                if conversation != current.conversation_id {
                    conversation.clone_from(&current.conversation_id);
                    if let Some(id) = &conversation {
                        rpc.notify_scope(
                            &session,
                            rpc::ServerNotification::ChatConversation(
                                rpc::ChatConversationNotification {
                                    scope_id: current.scope_id.to_string(),
                                    operation_id: current.operation_id.to_string(),
                                    draft_id: current.draft_id.to_string(),
                                    conversation_id: id.clone(),
                                    persistence: match current.persistence {
                                        crate::chat::Persistence::Saved => {
                                            rpc::ChatPersistence::Saved
                                        }
                                        crate::chat::Persistence::Temporary => {
                                            rpc::ChatPersistence::Temporary
                                        }
                                    },
                                    sequence: current.sequence,
                                },
                            ),
                        )
                        .await;
                    }
                }
                let terminal = current.phase == crate::chat::OperationPhase::Finished;
                let turn = adapter::turn(&current);
                rpc.notify_scope(
                    &session,
                    if terminal {
                        rpc::ServerNotification::ChatTurnCompleted(
                            rpc::ChatTurnCompletedNotification { turn },
                        )
                    } else {
                        rpc::ServerNotification::ChatMessage(rpc::ChatMessageNotification { turn })
                    },
                )
                .await;
                drop(rpc);
                if terminal {
                    break;
                }
                tokio::select! { biased;
                    _ = stopped.cancelled() => break,
                    result = changes.changed() => { if result.is_err() || session.check().is_err() { break; } },
                    result = updates.changed() => if result.is_err() { break; },
                }
                // Rendering updates coalesce while journal checkpoints keep every
                // acceptance transition. Slow clients cannot stall generation.
                tokio::select! { biased; _ = stopped.cancelled() => break, _ = tokio::time::sleep(Duration::from_millis(50)) => {} }
            }
        });
    }
}
