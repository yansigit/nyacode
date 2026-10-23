use super::*;
use codex_chatgpt_chat::attachment::AttachmentKind;
use codex_chatgpt_chat::attachment::AttachmentMetadata;
use codex_chatgpt_chat::attachment::MAX_SUBMISSION_BYTES;
use codex_chatgpt_chat::attachment::StagedAttachment;
use codex_chatgpt_chat::upload::ReadyUpload;
use codex_chatgpt_chat::upload::UploadStage;
use codex_state::chat::ChatFileKind;
use codex_state::chat::ChatFileSnapshot;
use codex_state::chat::ChatUploadedFile;
use std::collections::HashSet;
use std::time::UNIX_EPOCH;

pub enum UploadSelection {
    ClipboardBase64 {
        selection_id: Uuid,
        encoded: String,
    },
    /// Resolves the explicit local path retained in this saved draft.
    File {
        selection_id: Uuid,
    },
    /// Bounded image bytes from the in-memory clipboard reader. No file fallback.
    Clipboard {
        selection_id: Uuid,
        bytes: Vec<u8>,
    },
}

impl UploadSelection {
    fn id(&self) -> Uuid {
        match self {
            Self::File { selection_id }
            | Self::Clipboard { selection_id, .. }
            | Self::ClipboardBase64 { selection_id, .. } => *selection_id,
        }
    }
}

pub struct SelectedUpload {
    pub selection_id: Uuid,
    pub handle: Uuid,
    pub metadata: AttachmentMetadata,
}

pub struct UploadBatch {
    /// Revision retaining validated digests: durable for saved drafts, memory-only for temporary.
    pub draft: codex_state::chat::ChatDraft,
    pub uploads: Vec<SelectedUpload>,
}

pub(super) struct UploadEntry {
    pub(super) draft_id: Uuid,
    pub(super) selection_id: Uuid,
    pub(super) cancel: CancellationToken,
    pub(super) ready: Option<Arc<ReadyUpload>>,
}

impl Drop for UploadEntry {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

// Dropping an in-flight transfer releases its capability even when cancellation
// drops the future between allocation and the next progress callback.
struct ActiveUpload<'a> {
    session: &'a SavedSession,
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

impl SavedSession {
    pub(super) async fn stage_selected(
        &self,
        draft_id: Uuid,
        revision: Uuid,
        selections: Vec<UploadSelection>,
    ) -> Result<(codex_state::chat::ChatDraft, Vec<(Uuid, StagedAttachment)>), ServiceError> {
        let draft = self
            .draft_read(draft_id)
            .await?
            .ok_or(ChatStoreError::NotFound)?;
        if draft.revision != revision {
            return Err(ChatStoreError::RevisionConflict.into());
        }
        let staging_cancel = self.stopped.child_token();
        let _staging_guard = staging_cancel.clone().drop_guard();
        let (recorded, staged) =
            stage_draft(&self.stager, &draft, selections, staging_cancel).await?;
        let current = self
            .draft_read(draft_id)
            .await?
            .ok_or(ChatStoreError::NotFound)?;
        if current.revision != revision {
            return Err(ChatStoreError::RevisionConflict.into());
        }
        let current = if recorded == current.content {
            current
        } else {
            self.draft_write(draft_id, Some(revision), recorded).await?
        };
        Ok((current, staged))
    }

    /// Snapshots the entire selection before allocating the first remote file.
    /// Source paths come only from the revision-checked, local saved draft.
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
            let native = self.native().await?;
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
                    .upload_attachment(draft_id, &file, &upload_cancel, |stage, percent| {
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
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ChatError::LocalCancellation.into()),
            result = self.guarded(work) => result,
        }
    }

    /// Stops local transfer/processing or removes a ready selection. No native
    /// storage deletion or confirmed server cancellation is implied.
    pub fn cancel_upload(&self, handle: Uuid) -> Result<bool, ServiceError> {
        self.check()?;
        Ok(lock(&self.uploads).remove(&handle).is_some())
    }

    pub(super) fn ready_uploads(
        &self,
        draft: &codex_state::chat::ChatDraft,
        handles: &[Uuid],
    ) -> Result<(Vec<Arc<ReadyUpload>>, Vec<ChatUploadedFile>), ServiceError> {
        self.check()?;
        let entries = validate_uploads(draft, handles, &lock(&self.uploads))?;
        let mut ready = Vec::with_capacity(entries.len());
        let mut files = Vec::with_capacity(entries.len());
        for (selection_id, upload) in entries {
            let metadata = upload.recovery_metadata();
            files.push(ChatUploadedFile {
                selection_id,
                server_file_id: metadata.server_file_id,
                snapshot: snapshot(&metadata.metadata),
            });
            ready.push(upload);
        }
        Ok((ready, files))
    }
}

pub(super) fn validate_uploads(
    draft: &codex_state::chat::ChatDraft,
    handles: &[Uuid],
    uploads: &HashMap<Uuid, UploadEntry>,
) -> Result<Vec<(Uuid, Arc<ReadyUpload>)>, ServiceError> {
    if handles.len() > 20 || handles.len() != draft.content.selections.len() {
        return Err(ChatStoreError::InvalidInput.into());
    }
    let mut seen = HashSet::new();
    let mut selected = HashSet::new();
    let mut ready = Vec::new();
    for handle in handles {
        if !seen.insert(handle) {
            return Err(ChatError::StaleUpload.into());
        }
        let entry = uploads
            .get(handle)
            .filter(|entry| entry.draft_id == draft.id)
            .ok_or(ChatError::StaleUpload)?;
        if !selected.insert(entry.selection_id) {
            return Err(ChatError::StaleUpload.into());
        }
        let upload = entry
            .ready
            .as_ref()
            .ok_or(ChatError::ProcessingIncomplete)?;
        if upload.is_expired() {
            return Err(ChatError::StaleUpload.into());
        }
        let metadata = upload.metadata();
        let selection = draft
            .content
            .selections
            .iter()
            .find(|selected| selected.id == entry.selection_id)
            .ok_or(ChatError::StaleUpload)?;
        if selection.source != metadata.source
            || selection.name != metadata.name
            || selection.snapshot.as_ref() != Some(&snapshot(metadata))
        {
            return Err(ChatError::StaleUpload.into());
        }
        ready.push((entry.selection_id, Arc::clone(upload)));
    }
    Ok(ready)
}

/// Snapshots and validates a complete submission using only immutable draft
/// data and bounded staging memory. It never reads or writes saved storage.
pub(super) async fn stage_draft(
    stager: &AttachmentStager,
    draft: &codex_state::chat::ChatDraft,
    selections: Vec<UploadSelection>,
    staging_cancel: CancellationToken,
) -> Result<
    (
        codex_state::chat::ChatDraftContent,
        Vec<(Uuid, StagedAttachment)>,
    ),
    ServiceError,
> {
    if selections.is_empty()
        || selections.len() > 20
        || selections.len() != draft.content.selections.len()
    {
        return Err(ChatStoreError::InvalidInput.into());
    }
    let mut ids = HashSet::new();
    let mut paths = Vec::new();
    let mut path_ids = Vec::new();
    let mut clipboard = Vec::new();
    for selection in selections {
        let id = selection.id();
        if !ids.insert(id) {
            return Err(ChatStoreError::InvalidInput.into());
        }
        let selected = draft
            .content
            .selections
            .iter()
            .find(|s| s.id == id)
            .ok_or(ChatStoreError::InvalidInput)?;
        match (selection, &selected.source) {
            (UploadSelection::File { .. }, Some(path)) => {
                paths.push(path.clone());
                path_ids.push(id);
            }
            (
                selection @ (UploadSelection::Clipboard { .. }
                | UploadSelection::ClipboardBase64 { .. }),
                None,
            ) => clipboard.push((id, selected.name.clone(), selection)),
            _ => return Err(ChatStoreError::InvalidInput.into()),
        }
    }
    let mut staged: Vec<(Uuid, StagedAttachment)> = if paths.is_empty() {
        Vec::new()
    } else {
        path_ids
            .into_iter()
            .zip(stager.snapshot(paths, staging_cancel.clone()).await?)
            .collect()
    };
    for (id, name, selection) in clipboard {
        let file = match selection {
            UploadSelection::Clipboard { bytes, .. } => {
                stager
                    .snapshot_clipboard(name, bytes, staging_cancel.clone())
                    .await?
            }
            UploadSelection::ClipboardBase64 { encoded, .. } => {
                stager
                    .snapshot_clipboard_base64(name, encoded, staging_cancel.clone())
                    .await?
            }
            UploadSelection::File { .. } => return Err(ChatStoreError::InvalidInput.into()),
        };
        staged.push((id, file));
    }
    if staged
        .iter()
        .map(|(_, file)| file.metadata().size)
        .sum::<u64>()
        > MAX_SUBMISSION_BYTES as u64
    {
        return Err(AttachmentError::Limit.into());
    }
    for (id, file) in &staged {
        let selection = draft
            .content
            .selections
            .iter()
            .find(|s| s.id == *id)
            .ok_or(ChatStoreError::InvalidInput)?;
        if selection.name != file.metadata().name
            || selection
                .snapshot
                .as_ref()
                .is_some_and(|prior| *prior != snapshot(file.metadata()))
        {
            return Err(AttachmentError::Changed.into());
        }
    }
    let mut recorded = draft.content.clone();
    for (id, file) in &staged {
        let selection = recorded
            .selections
            .iter_mut()
            .find(|selection| selection.id == *id)
            .ok_or(ChatStoreError::InvalidInput)?;
        selection.snapshot = Some(snapshot(file.metadata()));
    }
    Ok((recorded, staged))
}

pub(super) fn snapshot(metadata: &AttachmentMetadata) -> ChatFileSnapshot {
    ChatFileSnapshot {
        sha256: metadata.sha256.clone(),
        size: metadata.size,
        modified_at_ms: metadata
            .modified
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .and_then(|duration| i64::try_from(duration.as_millis()).ok()),
        kind: match metadata.kind {
            AttachmentKind::Png => ChatFileKind::Png,
            AttachmentKind::Jpeg => ChatFileKind::Jpeg,
            AttachmentKind::Webp => ChatFileKind::Webp,
            AttachmentKind::Pdf => ChatFileKind::Pdf,
            AttachmentKind::Utf8 => ChatFileKind::Utf8,
        },
        dimensions: metadata.dimensions,
    }
}
