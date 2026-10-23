use super::*;
use crate::chat::UploadSelection;
use codex_chatgpt_chat::attachment::AttachmentError;
use codex_chatgpt_chat::attachment::MAX_SUBMISSION_BYTES;
use codex_chatgpt_chat::upload::UploadStage;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use tokio::sync::watch;

const MAX_OPERATIONS: usize = 32;
const MAX_ACTIVE: usize = 8;
const MAX_ENCODED_IMAGE: usize = (20usize * 1024 * 1024).div_ceil(3) * 4;

#[derive(Clone)]
pub(super) struct UploadOperation {
    pub(super) scope_id: Uuid,
    pub(super) cancel: CancellationToken,
    updates: Arc<watch::Sender<rpc::ChatAttachmentSnapshot>>,
}

impl UploadOperation {
    pub(super) fn snapshot(&self) -> rpc::ChatAttachmentSnapshot {
        self.updates.borrow().clone()
    }

    fn update(&self, update: impl FnOnce(&mut rpc::ChatAttachmentSnapshot)) {
        self.updates.send_modify(|snapshot| {
            update(snapshot);
            snapshot.sequence += 1;
        });
    }
}

impl ChatRpc {
    pub(super) async fn start_upload(
        self: &Arc<Self>,
        session: Session,
        params: rpc::ChatAttachmentUploadParams,
    ) -> Result<Uuid, ServiceError> {
        let draft_id = adapter::id(&params.draft_id)?;
        let revision = adapter::id(&params.expected_revision)?;
        if params.selections.is_empty() || params.selections.len() > 20 {
            return Err(ChatError::InvalidInput.into());
        }
        let mut encoded_bytes = 0usize;
        for source in &params.selections {
            if let rpc::ChatAttachmentSource::ClipboardImage { image_base64, .. } = source {
                if image_base64.len() > MAX_ENCODED_IMAGE {
                    return Err(AttachmentError::Limit.into());
                }
                encoded_bytes = encoded_bytes
                    .checked_add(image_base64.len())
                    .ok_or(AttachmentError::Limit)?;
                if encoded_bytes > MAX_SUBMISSION_BYTES.div_ceil(3) * 4 + 20 * 4 {
                    return Err(AttachmentError::Limit.into());
                }
            }
        }
        // Reserve before the first await, so database waits cannot accumulate
        // unbounded encoded clipboard buffers. Native staging reserves decoded bytes.
        let encoded_lease = Arc::clone(&self.encoded_memory)
            .try_acquire_many_owned(encoded_bytes as u32)
            .map_err(|_| ServiceError::Capacity)?;
        let draft = session
            .draft_read(draft_id)
            .await?
            .ok_or(ChatStoreError::NotFound)?;
        if draft.revision != revision {
            return Err(ChatStoreError::RevisionConflict.into());
        }
        if draft.content.selections.len() != params.selections.len() {
            return Err(ChatError::InvalidInput.into());
        }
        let mut seen = HashSet::new();
        let mut selections = Vec::new();
        let mut files = Vec::new();
        for source in params.selections {
            let id = adapter::id(match &source {
                rpc::ChatAttachmentSource::LocalFile { selection_id, .. }
                | rpc::ChatAttachmentSource::ClipboardImage { selection_id, .. } => selection_id,
            })?;
            if !seen.insert(id) {
                return Err(ChatError::InvalidInput.into());
            }
            let selected = draft
                .content
                .selections
                .iter()
                .find(|selected| selected.id == id)
                .ok_or(ChatError::InvalidInput)?;
            selections.push(match source {
                rpc::ChatAttachmentSource::LocalFile { path, .. } => {
                    if selected.source.as_deref() != Some(Path::new(&path)) {
                        return Err(ChatError::InvalidInput.into());
                    }
                    UploadSelection::File { selection_id: id }
                }
                rpc::ChatAttachmentSource::ClipboardImage {
                    name, image_base64, ..
                } => {
                    if selected.source.is_some() || selected.name != name {
                        return Err(ChatError::InvalidInput.into());
                    }
                    UploadSelection::ClipboardBase64 {
                        selection_id: id,
                        encoded: image_base64,
                    }
                }
            });
            files.push(rpc::ChatAttachmentProgress {
                selection_id: id.to_string(),
                handle: None,
                stage: rpc::ChatAttachmentStage::Selected,
                percent: None,
                metadata: selected.snapshot.clone().map(adapter::file_snapshot),
                error: None,
            });
        }
        let operation_id = Uuid::new_v4();
        let operation = UploadOperation {
            scope_id: session.scope_id(),
            cancel: self.stopped.child_token(),
            updates: Arc::new(
                watch::channel(rpc::ChatAttachmentSnapshot {
                    scope_id: session.scope_id().to_string(),
                    operation_id: operation_id.to_string(),
                    draft_id: draft_id.to_string(),
                    persistence: session.persistence(),
                    sequence: 0,
                    draft_revision: Some(revision.to_string()),
                    files,
                    finished: false,
                    error: None,
                })
                .0,
            ),
        };
        {
            let mut uploads = lock(&self.uploads);
            session.check()?;
            let active = uploads
                .values()
                .filter(|upload| !upload.snapshot().finished)
                .count();
            if active >= MAX_ACTIVE
                || uploads.values().any(|upload| {
                    let s = upload.snapshot();
                    upload.scope_id == session.scope_id()
                        && s.persistence == session.persistence()
                        && s.draft_id == draft_id.to_string()
                        && !s.finished
                })
            {
                return Err(ServiceError::Capacity);
            }
            if uploads.len() >= MAX_OPERATIONS {
                let finished = uploads
                    .iter()
                    .find(|(_, upload)| upload.snapshot().finished)
                    .map(|(id, _)| *id)
                    .ok_or(ServiceError::Capacity)?;
                uploads.remove(&finished);
            }
            uploads.insert(operation_id, operation.clone());
        }
        self.watch_upload(session.clone(), operation.clone());
        tokio::spawn(async move {
            let _encoded_lease = encoded_lease;
            let result = session
                .upload_selected(
                    draft_id,
                    revision,
                    selections,
                    |selection, handle, stage, percent| {
                        operation.update(|snapshot| {
                            if let Some(file) = snapshot
                                .files
                                .iter_mut()
                                .find(|file| file.selection_id == selection.to_string())
                            {
                                file.handle = Some(handle.to_string());
                                file.stage = match stage {
                                    UploadStage::Allocating => rpc::ChatAttachmentStage::Allocating,
                                    UploadStage::Uploading => rpc::ChatAttachmentStage::Uploading,
                                    UploadStage::Processing => rpc::ChatAttachmentStage::Processing,
                                    UploadStage::Ready => rpc::ChatAttachmentStage::Ready,
                                };
                                file.percent = percent;
                            }
                        });
                    },
                    &operation.cancel,
                )
                .await;
            let (draft, error) = match result {
                Ok(batch) => (Some(batch.draft), None),
                Err(error) => (
                    session.draft_read(draft_id).await.ok().flatten(),
                    Some(adapter::code(error)),
                ),
            };
            let cancelled = operation.cancel.is_cancelled();
            if cancelled {
                for file in operation.snapshot().files {
                    if let Some(handle) = file.handle.and_then(|id| Uuid::parse_str(&id).ok()) {
                        let _ = session.cancel_upload(handle);
                    }
                }
            }
            operation.update(|snapshot| {
                snapshot.finished = true;
                snapshot.error = if cancelled {
                    Some(rpc::ChatErrorCode::LocalCancellation)
                } else {
                    error
                };
                if let Some(draft) = draft {
                    snapshot.draft_revision = Some(draft.revision.to_string());
                    for file in &mut snapshot.files {
                        if let Some(selected) = draft
                            .content
                            .selections
                            .iter()
                            .find(|s| s.id.to_string() == file.selection_id)
                        {
                            file.metadata = selected.snapshot.clone().map(adapter::file_snapshot);
                        }
                    }
                }
                for file in &mut snapshot.files {
                    if cancelled {
                        file.stage = rpc::ChatAttachmentStage::LocalStopped;
                        file.handle = None;
                        file.error = Some(rpc::ChatErrorCode::LocalCancellation);
                    } else if file.stage != rpc::ChatAttachmentStage::Ready {
                        file.stage = rpc::ChatAttachmentStage::Failed;
                        file.handle = None;
                        file.error = error;
                    }
                }
            });
        });
        Ok(operation_id)
    }

    pub(super) fn cancel_upload(
        &self,
        session: &Session,
        id: Uuid,
    ) -> Result<Option<rpc::ChatAttachmentSnapshot>, ServiceError> {
        session.check()?;
        let operation = lock(&self.uploads)
            .get(&id)
            .filter(|upload| upload.scope_id == session.scope_id())
            .cloned();
        let Some(operation) = operation else {
            return Ok(None);
        };
        operation.cancel.cancel();
        for file in operation.snapshot().files {
            if let Some(handle) = file.handle.and_then(|id| Uuid::parse_str(&id).ok()) {
                session.cancel_upload(handle)?;
            }
        }
        // Active transfers report stopping only after the worker observes it.
        if operation.snapshot().finished {
            operation.update(|snapshot| {
                for file in &mut snapshot.files {
                    if file.stage == rpc::ChatAttachmentStage::Ready {
                        file.stage = rpc::ChatAttachmentStage::LocalStopped;
                        file.handle = None;
                    }
                }
            });
        }
        Ok(Some(operation.snapshot()))
    }

    fn watch_upload(self: &Arc<Self>, session: Session, operation: UploadOperation) {
        let weak = Arc::downgrade(self);
        let mut updates = operation.updates.subscribe();
        let mut changes = self.service.changes();
        let stopped = self.stopped.clone();
        tokio::spawn(async move {
            loop {
                if session.check().is_err() {
                    operation.cancel.cancel();
                    break;
                }
                let upload = updates.borrow_and_update().clone();
                let terminal = upload.finished;
                let Some(rpc) = weak.upgrade() else {
                    operation.cancel.cancel();
                    break;
                };
                rpc.notify_scope(
                    &session.scope(),
                    rpc::ServerNotification::ChatAttachment(rpc::ChatAttachmentNotification {
                        upload,
                    }),
                )
                .await;
                drop(rpc);
                if terminal {
                    break;
                }
                tokio::select! { biased;
                    _ = stopped.cancelled() => { operation.cancel.cancel(); break; },
                    result = changes.changed() => { if result.is_err() || session.check().is_err() { operation.cancel.cancel(); break; } },
                    result = updates.changed() => if result.is_err() { break; },
                }
                tokio::select! { biased; _ = stopped.cancelled() => { operation.cancel.cancel(); break; }, _ = tokio::time::sleep(Duration::from_millis(50)) => {} }
            }
        });
    }
}
