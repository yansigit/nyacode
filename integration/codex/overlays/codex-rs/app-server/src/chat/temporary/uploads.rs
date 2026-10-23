use super::*;
use crate::chat::SelectedUpload;
use crate::chat::UploadBatch;
use crate::chat::UploadSelection;
use crate::chat::uploads::UploadEntry;
use crate::chat::uploads::stage_draft;
use crate::chat::uploads::validate_uploads;
use codex_chatgpt_chat::attachment::StagedAttachment;
use codex_chatgpt_chat::upload::ReadyUpload;
use codex_chatgpt_chat::upload::UploadStage;

struct ActiveUpload<'a> {
    session: &'a TemporarySession,
    handle: Uuid,
    retained: bool,
}
impl Drop for ActiveUpload<'_> {
    fn drop(&mut self) {
        if !self.retained {
            lock(&self.session.uploads).remove(&self.handle);
        }
    }
}

impl TemporarySession {
    pub(super) async fn stage_selected(
        &self,
        draft_id: Uuid,
        revision: Uuid,
        selections: Vec<UploadSelection>,
    ) -> Result<(ChatDraft, Vec<(Uuid, StagedAttachment)>), ServiceError> {
        let draft = self.draft_read(draft_id)?.ok_or(ChatStoreError::NotFound)?;
        if draft.revision != revision {
            return Err(ChatStoreError::RevisionConflict.into());
        }
        let cancel = self.stopped.child_token();
        let _cancel_guard = cancel.clone().drop_guard();
        let (recorded, staged) = stage_draft(&self.stager, &draft, selections, cancel).await?;
        let current = self.draft_read(draft_id)?.ok_or(ChatStoreError::NotFound)?;
        if current.revision != revision {
            return Err(ChatStoreError::RevisionConflict.into());
        }
        let current = if current.content == recorded {
            current
        } else {
            self.draft_write(draft_id, Some(revision), recorded)?
        };
        Ok((current, staged))
    }

    /// Validation and source snapshots stay in this memory-only owner. The native
    /// uploader marks temporary processing explicitly and binds ready handles to
    /// the per-draft temporary profile, preventing association with saved sends.
    pub async fn upload_selected(
        &self,
        draft_id: Uuid,
        revision: Uuid,
        selections: Vec<UploadSelection>,
        mut progress: impl FnMut(Uuid, Uuid, UploadStage, Option<u8>),
        cancel: &CancellationToken,
    ) -> Result<UploadBatch, ServiceError> {
        let work = async {
            let (current, staged) = self.stage_selected(draft_id, revision, selections).await?;
            let native = self.native(draft_id)?;
            let mut selected = Vec::new();
            for (selection_id, file) in staged {
                self.check()?;
                let handle = Uuid::new_v4();
                let upload_cancel = self.stopped.child_token();
                let _upload_guard = upload_cancel.clone().drop_guard();
                {
                    let mut uploads = lock(&self.uploads);
                    uploads.retain(|_, entry| {
                        (entry.draft_id != draft_id || entry.selection_id != selection_id)
                            && entry.ready.as_ref().is_none_or(|ready| !ready.is_expired())
                    });
                    if uploads.len() >= 160 {
                        return Err(ServiceError::Capacity);
                    }
                    uploads.insert(
                        handle,
                        UploadEntry {
                            draft_id,
                            selection_id,
                            cancel: upload_cancel.clone(),
                            ready: None,
                        },
                    );
                }
                let mut active = ActiveUpload {
                    session: self,
                    handle,
                    retained: false,
                };
                let result = native
                    .upload_attachment(&file, &upload_cancel, |stage, percent| {
                        if stage != UploadStage::Ready {
                            progress(selection_id, handle, stage, percent);
                        }
                    })
                    .await;
                self.check()?;
                let mut uploads = lock(&self.uploads);
                match result {
                    Ok((Some(ready), _)) => {
                        let entry = uploads
                            .get_mut(&handle)
                            .ok_or(ChatError::LocalCancellation)?;
                        if entry.cancel.is_cancelled() {
                            return Err(ChatError::LocalCancellation.into());
                        }
                        selected.push(SelectedUpload {
                            selection_id,
                            handle,
                            metadata: ready.metadata().clone(),
                        });
                        entry.ready = Some(Arc::new(ready));
                        drop(uploads);
                        active.retained = true;
                        progress(selection_id, handle, UploadStage::Ready, Some(100));
                    }
                    Ok((None, observation)) => {
                        uploads.remove(&handle);
                        return Err(observation.error.unwrap_or(ChatError::UploadFailed).into());
                    }
                    Err(error) => {
                        uploads.remove(&handle);
                        return Err(error.into());
                    }
                }
            }
            Ok(UploadBatch {
                draft: current,
                uploads: selected,
            })
        };
        tokio::select! { biased;
            _ = cancel.cancelled() => Err(ChatError::LocalCancellation.into()),
            _ = self.stopped.cancelled() => Err(ServiceError::ScopeChanged),
            result = work => result,
        }
    }

    pub fn cancel_upload(&self, handle: Uuid) -> Result<bool, ServiceError> {
        self.check()?;
        Ok(lock(&self.uploads).remove(&handle).is_some())
    }

    pub fn ready_uploads(
        &self,
        draft: &ChatDraft,
        handles: &[Uuid],
    ) -> Result<Vec<Arc<ReadyUpload>>, ServiceError> {
        self.check()?;
        Ok(validate_uploads(draft, handles, &lock(&self.uploads))?
            .into_iter()
            .map(|(_, ready)| ready)
            .collect())
    }
}
