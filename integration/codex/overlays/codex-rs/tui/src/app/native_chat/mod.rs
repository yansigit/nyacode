//! Independent browserless Chat surface. Content stays in this controller and
//! typed Chat RPCs; it never enters AppEvent, Codex history, or transcript logs.
//! Draft writes compare revisions. Submission snapshots stay frozen until the
//! server establishes acceptance; transport failures never trigger a resend.

mod input;
mod render;
mod state;
#[cfg(test)]
mod tests;

use crate::app_server_session::AppServerSession;
use crate::tui::FrameRequester;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::TypedRequestError;
use codex_app_server_protocol as rpc;
use serde::de::DeserializeOwned;
use state::Session;
use state::Work;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Browse,
    Save(u64),
    SubmitSave(u64),
    Upload,
    Start,
    Read,
    BeforeSubmit,
    Cancel,
    Release,
    Close,
    Clipboard,
    Revision(u64),
}

// Intentionally no Debug: even transient temporary content remains private.
enum Reply {
    Status(rpc::ChatStatusReadResponse),
    Models(rpc::ChatModelListResponse),
    History(rpc::ChatConversationListResponse),
    Drafts(rpc::ChatDraftListResponse),
    Draft(rpc::ChatDraftReadResponse),
    Saved(rpc::ChatDraftWriteResponse),
    Deleted(rpc::ChatDraftDeleteResponse),
    Conversation(rpc::ChatConversationReadResponse),
    Upload(rpc::ChatAttachmentUploadResponse),
    Turn(rpc::ChatTurnStartResponse),
    CancelTurn(rpc::ChatTurnCancelResponse),
    CancelUpload(rpc::ChatAttachmentCancelResponse),
    Clipboard(Vec<u8>),
}
struct Envelope {
    epoch: u64,
    draft: Option<String>,
    purpose: Purpose,
    result: Result<Reply, rpc::ChatErrorCode>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Picker {
    History,
    Models,
    Drafts,
    Sessions,
    Attachments,
}

pub(super) struct NativeChat {
    pub(super) active: bool,
    pub(super) codex_unread: bool,
    initialized: bool,
    epoch: u64,
    generation: u64,
    scope: Option<String>,
    sessions: Vec<Session>,
    current: usize,
    models: Vec<rpc::ChatModel>,
    default_model: Option<String>,
    history: Vec<rpc::ChatConversationSummary>,
    next_history: Option<u32>,
    drafts: Vec<rpc::ChatDraftSummary>,
    next_drafts: Option<rpc::ChatDraftCursor>,
    picker: Option<Picker>,
    selected: usize,
    notice: String,
    sender: mpsc::Sender<Envelope>,
    receiver: mpsc::Receiver<Envelope>,
    tasks: Vec<JoinHandle<()>>,
    clipboard_in_flight: bool,
    last_status: Instant,
    status_in_flight: bool,
    reconnect_status_pending: bool,
}

impl Default for NativeChat {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel(32);
        Self {
            active: false,
            codex_unread: false,
            initialized: false,
            epoch: 0,
            generation: 0,
            scope: None,
            sessions: Vec::new(),
            current: 0,
            models: Vec::new(),
            default_model: None,
            history: Vec::new(),
            next_history: None,
            drafts: Vec::new(),
            next_drafts: None,
            picker: None,
            selected: 0,
            notice: String::new(),
            sender,
            receiver,
            tasks: Vec::new(),
            clipboard_in_flight: false,
            last_status: Instant::now(),
            status_in_flight: false,
            reconnect_status_pending: false,
        }
    }
}

impl Drop for NativeChat {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn request_id() -> rpc::RequestId {
    rpc::RequestId::String(format!("native-chat-{}", Uuid::new_v4()))
}

fn local_connection(target: &crate::AppServerTarget) -> bool {
    use crate::AppServerTarget;
    use codex_app_server_client::RemoteAppServerEndpoint;
    matches!(
        target,
        AppServerTarget::Embedded
            | AppServerTarget::LocalDaemon {
                endpoint: RemoteAppServerEndpoint::UnixSocket { .. },
                ..
            }
            | AppServerTarget::Remote {
                endpoint: RemoteAppServerEndpoint::UnixSocket { .. }
            }
    )
}
fn error_code(error: TypedRequestError) -> rpc::ChatErrorCode {
    match error {
        TypedRequestError::Server { source, .. } => source
            .data
            .and_then(|data| serde_json::from_value::<rpc::ChatErrorDetails>(data).ok())
            .map_or(rpc::ChatErrorCode::ProtocolChanged, |details| details.code),
        TypedRequestError::Transport { .. } => rpc::ChatErrorCode::Transport,
        TypedRequestError::Deserialize { .. } => rpc::ChatErrorCode::ProtocolChanged,
    }
}

impl NativeChat {
    fn request<T: DeserializeOwned + Send + 'static>(
        &mut self,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
        draft: Option<String>,
        purpose: Purpose,
        request: rpc::ClientRequest,
        map: impl FnOnce(T) -> Reply + Send + 'static,
    ) {
        let handle = handle.clone();
        let frame = frame.clone();
        let sender = self.sender.clone();
        let epoch = self.epoch;
        self.tasks.retain(|task| !task.is_finished());
        self.tasks.push(tokio::spawn(async move {
            let result = handle
                .request_typed(request)
                .await
                .map(map)
                .map_err(error_code);
            if sender
                .send(Envelope {
                    epoch,
                    draft,
                    purpose,
                    result,
                })
                .await
                .is_ok()
            {
                frame.schedule_frame();
            }
        }));
    }

    pub(super) fn open(&mut self, server: &AppServerSession, frame: &FrameRequester) {
        self.active = true;
        self.status(&server.request_handle(), frame);
        frame.schedule_frame();
    }

    fn status(&mut self, handle: &AppServerRequestHandle, frame: &FrameRequester) {
        if self.status_in_flight {
            return;
        }
        self.status_in_flight = true;
        self.last_status = Instant::now();
        self.request(
            handle,
            frame,
            None,
            Purpose::Browse,
            rpc::ClientRequest::ChatStatusRead {
                request_id: request_id(),
                params: rpc::ChatStatusReadParams::default(),
            },
            Reply::Status,
        );
    }

    fn clear_identity(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        for task in self.tasks.drain(..) {
            task.abort();
        }
        while self.receiver.try_recv().is_ok() {}
        self.sessions.clear();
        self.current = 0;
        self.models.clear();
        self.history.clear();
        self.drafts.clear();
        self.next_drafts = None;
        self.next_history = None;
        self.default_model = None;
        self.scope = None;
        self.initialized = false;
        self.picker = None;
        self.clipboard_in_flight = false;
        self.status_in_flight = false;
    }

    pub(super) fn disconnected(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        for task in self.tasks.drain(..) {
            task.abort();
        }
        while self.receiver.try_recv().is_ok() {}
        self.reconnect_status_pending = true;
        // A broken connection is not evidence of non-acceptance. Preserve the
        // snapshot and require status/authoritative reconciliation on reconnect.
        for session in &mut self.sessions {
            session.save_in_flight = false;
            session.read_in_flight = false;
            session.cancel_in_flight = false;
            if session.submission.is_some() {
                session.work = Work::Unknown;
                session.notice = "Disconnected. Pending submissions will not be resent.".into();
            } else if !session.write_conflict {
                session.notice = "Disconnected. Your draft is retained.".into();
            }
        }
        self.notice = "Disconnected. Pending submissions will not be resent.".into();
        self.status_in_flight = false;
        self.last_status = Instant::now() - Duration::from_secs(30);
    }

    pub(super) fn reconnected(&mut self, server: &AppServerSession, frame: &FrameRequester) {
        if !self.initialized && !self.active {
            self.reconnect_status_pending = false;
            return;
        }
        // Requests issued while reconnecting still belong to the dead handle.
        // Retire them before requesting the new owner's snapshot immediately.
        self.disconnected();
        self.status(&server.request_handle(), frame);
    }

    fn new_session(&mut self, persistence: rpc::ChatPersistence) {
        if self.sessions.len() >= state::MAX_DRAFTS {
            self.notice = "Close an idle chat before opening another (32 open chats).".into();
            return;
        }
        self.sessions.push(Session::new(persistence));
        self.current = self.sessions.len() - 1;
        self.picker = None;
    }

    fn apply_status(
        &mut self,
        status: rpc::ChatStatusSnapshot,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) {
        if status.generation < self.generation {
            return;
        }
        let changed = status.generation != self.generation || status.scope_id != self.scope;
        if changed {
            self.clear_identity();
        }
        self.generation = status.generation;
        self.scope = status.scope_id;
        self.status_in_flight = false;
        if status.availability != rpc::ChatAvailability::Available {
            self.clear_identity();
            self.notice = match status.availability {
                rpc::ChatAvailability::Disabled => {
                    "Chat is disabled. Enable features.chatgpt_chat to use this surface."
                }
                rpc::ChatAvailability::LoginRequired => {
                    "Sign in with your managed ChatGPT account to use Chat."
                }
                rpc::ChatAvailability::LocalConnectionRequired => {
                    "Chat requires an embedded server or trusted local daemon socket."
                }
                rpc::ChatAvailability::Available => unreachable!(),
            }
            .into();
            return;
        }
        if !self.initialized {
            self.initialized = true;
            self.notice = if changed {
                "Account session ready. Temporary chats from previous sessions are gone."
            } else {
                "Chat ready."
            }
            .into();
            if self.sessions.is_empty() {
                self.new_session(rpc::ChatPersistence::Saved);
            }
            if let Some(scope_id) = self.scope.clone() {
                self.request(
                    handle,
                    frame,
                    None,
                    Purpose::Browse,
                    rpc::ClientRequest::ChatModelList {
                        request_id: request_id(),
                        params: rpc::ChatModelListParams { scope_id },
                    },
                    Reply::Models,
                );
                self.load_drafts(handle, frame, None);
            }
        }
        for turn in status.turns {
            self.turn(turn, handle, frame);
        }
        for upload in status.uploads {
            self.upload(upload, handle, frame);
        }
    }

    pub(super) fn notification(
        &mut self,
        notification: &rpc::ServerNotification,
        server: &AppServerSession,
        frame: &FrameRequester,
    ) -> bool {
        let handle = server.request_handle();
        match notification {
            rpc::ServerNotification::ChatAvailability(value) => {
                if (self.initialized || self.active) && !self.reconnect_status_pending {
                    self.apply_status(value.status.clone(), &handle, frame);
                }
            }
            rpc::ServerNotification::ChatMessage(value) => {
                self.turn(value.turn.clone(), &handle, frame)
            }
            rpc::ServerNotification::ChatTurnCompleted(value) => {
                self.turn(value.turn.clone(), &handle, frame)
            }
            rpc::ServerNotification::ChatAttachment(value) => {
                self.upload(value.upload.clone(), &handle, frame)
            }
            rpc::ServerNotification::ChatConversation(_) => {}
            _ => return false,
        }
        frame.schedule_frame();
        true
    }

    pub(super) fn tick(&mut self, server: &AppServerSession, frame: &FrameRequester) {
        let handle = server.request_handle();
        while let Ok(reply) = self.receiver.try_recv() {
            if reply.epoch == self.epoch {
                self.reply(reply, &handle, frame);
            }
        }
        if !self.initialized {
            return;
        }
        if self.last_status.elapsed() >= Duration::from_secs(30) {
            self.status(&handle, frame);
        }
        let dirty: Vec<_> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                (s.dirty()
                    && !s.save_in_flight
                    && !s.write_conflict
                    && s.editable()
                    && s.edited_at.elapsed() >= Duration::from_millis(500))
                .then_some(i)
            })
            .collect();
        for index in dirty {
            self.save(index, false, &handle, frame);
        }
        frame.schedule_frame_in(Duration::from_millis(500));
    }

    pub(super) fn flush(&mut self, server: &AppServerSession, frame: &FrameRequester) {
        for index in 0..self.sessions.len() {
            let s = &self.sessions[index];
            if s.dirty() && !s.save_in_flight && !s.write_conflict && s.editable() {
                self.save(index, false, &server.request_handle(), frame);
            }
        }
    }

    /// Finish debounced writes before navigation can terminate the process.
    /// A revision or storage failure keeps the UI open with its draft intact.
    pub(super) async fn finish_writes(
        &mut self,
        server: &AppServerSession,
        frame: &FrameRequester,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            self.tick(server, frame);
            self.flush(server, frame);
            if self
                .sessions
                .iter()
                .all(|s| !s.save_in_flight && (!s.dirty() || !s.editable()))
            {
                return true;
            }
            if self.sessions.iter().any(|s| s.write_conflict && s.dirty()) {
                self.notice =
                    "A draft could not be saved. Resolve the conflict before exiting.".into();
                return false;
            }
            let reply = match tokio::time::timeout_at(deadline, self.receiver.recv()).await {
                Ok(Some(reply)) => reply,
                _ => {
                    self.notice = "Draft saving has not completed. Try exiting again after the connection recovers.".into();
                    return false;
                }
            };
            if reply.epoch == self.epoch {
                self.reply(reply, &server.request_handle(), frame);
            }
        }
    }

    fn save(
        &mut self,
        index: usize,
        submit: bool,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) {
        let Some(scope_id) = self.scope.clone() else {
            return;
        };
        let session = &mut self.sessions[index];
        if session.save_in_flight {
            return;
        }
        session.save_in_flight = true;
        let purpose = if submit {
            Purpose::SubmitSave(session.edit_serial)
        } else {
            Purpose::Save(session.edit_serial)
        };
        let params = rpc::ChatDraftWriteParams {
            scope_id,
            persistence: session.persistence,
            draft_id: session.id.clone(),
            expected_revision: session.revision.clone(),
            content: session.content.clone(),
        };
        self.request(
            handle,
            frame,
            Some(params.draft_id.clone()),
            purpose,
            rpc::ClientRequest::ChatDraftWrite {
                request_id: request_id(),
                params,
            },
            Reply::Saved,
        );
    }

    fn read(
        &mut self,
        index: usize,
        before_submit: bool,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) {
        let Some(scope_id) = self.scope.clone() else {
            return;
        };
        let session = &mut self.sessions[index];
        let Some(conversation_id) = session.content.conversation_id.clone() else {
            return;
        };
        if session.read_in_flight {
            return;
        }
        session.read_in_flight = true;
        let params = rpc::ChatConversationReadParams {
            scope_id,
            conversation_id,
            persistence: session.persistence,
            submission_id: if before_submit
                || session.persistence == rpc::ChatPersistence::Temporary
            {
                None
            } else {
                session
                    .pending
                    .as_ref()
                    .map(|p| p.submission_id.clone())
                    .or_else(|| session.submission.as_ref().map(|s| s.id.clone()))
            },
            cache_only: None,
        };
        let draft_id = session.id.clone();
        self.request(
            handle,
            frame,
            Some(draft_id),
            if before_submit {
                Purpose::BeforeSubmit
            } else {
                Purpose::Read
            },
            rpc::ClientRequest::ChatConversationRead {
                request_id: request_id(),
                params,
            },
            Reply::Conversation,
        );
    }

    fn submit(&mut self, handle: &AppServerRequestHandle, frame: &FrameRequester) {
        let index = self.current;
        let Some(session) = self.sessions.get_mut(index) else {
            return;
        };
        if !session.submission() {
            return;
        }
        self.release_upload(index, handle, frame);
        let session = &mut self.sessions[index];
        session.turn = None;
        session.upload = None;
        session.upload_operation = None;
        if session.content.conversation_id.is_some() {
            session.work = Work::Reading;
            self.read(index, true, handle, frame);
        } else {
            self.save(index, true, handle, frame);
        }
    }

    fn start_upload_or_turn(
        &mut self,
        index: usize,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) {
        let Some(scope_id) = self.scope.clone() else {
            return;
        };
        let session = &mut self.sessions[index];
        if session.submission.is_none() {
            return;
        }
        if session.stop_requested {
            session
                .restore_before_send("Stopped before transmission. Your draft is retained.".into());
            return;
        }
        if session.content.selections.is_empty() {
            self.start_turn(index, Vec::new(), handle, frame);
            return;
        }
        use base64::Engine;
        let selections = session
            .content
            .selections
            .iter()
            .map(|file| {
                if let Some(path) = &file.source {
                    rpc::ChatAttachmentSource::LocalFile {
                        selection_id: file.id.clone(),
                        path: path.clone(),
                    }
                } else {
                    rpc::ChatAttachmentSource::ClipboardImage {
                        selection_id: file.id.clone(),
                        name: file.name.clone(),
                        image_base64: base64::engine::general_purpose::STANDARD
                            .encode(&session.clipboard[&file.id]),
                    }
                }
            })
            .collect();
        session.work = Work::Uploading;
        session.notice = "Validating and uploading the selected files…".into();
        let params = rpc::ChatAttachmentUploadParams {
            scope_id,
            persistence: session.persistence,
            draft_id: session.id.clone(),
            expected_revision: session.revision.clone().unwrap_or_default(),
            selections,
        };
        self.request(
            handle,
            frame,
            Some(params.draft_id.clone()),
            Purpose::Upload,
            rpc::ClientRequest::ChatAttachmentUpload {
                request_id: request_id(),
                params,
            },
            Reply::Upload,
        );
    }

    fn start_turn(
        &mut self,
        index: usize,
        upload_handles: Vec<String>,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) {
        let Some(scope_id) = self.scope.clone() else {
            return;
        };
        let session = &mut self.sessions[index];
        let Some(submission) = &session.submission else {
            return;
        };
        if matches!(
            session.work,
            Work::Starting | Work::Streaming | Work::Unknown
        ) {
            return;
        }
        session.work = Work::Starting;
        session.notice = "Preparing the response…".into();
        let params = rpc::ChatTurnStartParams {
            scope_id,
            persistence: session.persistence,
            draft_id: session.id.clone(),
            expected_revision: session.revision.clone().unwrap_or_default(),
            submission_id: submission.id.clone(),
            client_message_id: submission.message_id.clone(),
            expected_parent_id: submission.parent.clone(),
            text: submission.text.clone(),
            upload_handles,
        };
        self.request(
            handle,
            frame,
            Some(params.draft_id.clone()),
            Purpose::Start,
            rpc::ClientRequest::ChatTurnStart {
                request_id: request_id(),
                params,
            },
            Reply::Turn,
        );
    }

    fn turn(
        &mut self,
        turn: rpc::ChatTurnSnapshot,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) {
        if Some(&turn.scope_id) != self.scope.as_ref() {
            return;
        }
        let Some(index) = self.sessions.iter().position(|s| s.id == turn.draft_id) else {
            return;
        };
        if self.sessions[index].apply_turn(turn, self.active && index == self.current) {
            self.release_upload(index, handle, frame);
            if self.sessions[index].content.conversation_id.is_some() {
                self.read(index, false, handle, frame);
            }
        }
        if self.sessions[index].stop_requested {
            self.stop_operation(index, handle, frame);
        }
    }

    fn upload(
        &mut self,
        upload: rpc::ChatAttachmentSnapshot,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) {
        if Some(&upload.scope_id) != self.scope.as_ref() {
            return;
        }
        let Some(index) = self.sessions.iter().position(|s| s.id == upload.draft_id) else {
            return;
        };
        let session = &mut self.sessions[index];
        if session.work != Work::Uploading
            || session
                .upload_operation
                .as_ref()
                .is_some_and(|id| id != &upload.operation_id)
            || session
                .upload
                .as_ref()
                .is_some_and(|old| old.sequence >= upload.sequence)
        {
            return;
        }
        session.upload_operation = Some(upload.operation_id.clone());
        if let Some(revision) = &upload.draft_revision {
            session.revision = Some(revision.clone());
        }
        for progress in &upload.files {
            if let Some(file) = session
                .content
                .selections
                .iter_mut()
                .find(|f| f.id == progress.selection_id)
            {
                // Keep the recovered digest until a new immutable snapshot is
                // validated. An early upload failure must not erase the data
                // needed to detect a changed file on the next explicit try.
                if progress.metadata.is_some() {
                    file.snapshot.clone_from(&progress.metadata);
                }
            }
        }
        let finished = upload.finished;
        let ready = finished
            && upload.error.is_none()
            && upload.files.len() == session.content.selections.len()
            && upload
                .files
                .iter()
                .all(|file| file.stage == rpc::ChatAttachmentStage::Ready && file.handle.is_some());
        let handles = upload
            .files
            .iter()
            .filter_map(|file| file.handle.clone())
            .collect();
        let error = upload.error;
        session.upload = Some(upload);
        if session.stop_requested {
            self.stop_operation(index, handle, frame);
            if finished {
                self.sessions[index]
                    .restore_before_send("Upload stopped locally. Selections retained.".into());
            }
            return;
        }
        if ready {
            self.start_turn(index, handles, handle, frame);
        } else if finished {
            session.restore_before_send(format!(
                "Upload failed: {:?}. Your selections remain available.",
                error.unwrap_or(rpc::ChatErrorCode::ProcessingIncomplete)
            ));
        }
    }

    fn load_drafts(
        &mut self,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
        before: Option<rpc::ChatDraftCursor>,
    ) {
        let Some(scope_id) = self.scope.clone() else {
            return;
        };
        self.request(
            handle,
            frame,
            None,
            Purpose::Browse,
            rpc::ClientRequest::ChatDraftList {
                request_id: request_id(),
                params: rpc::ChatDraftListParams {
                    scope_id,
                    persistence: rpc::ChatPersistence::Saved,
                    before,
                    limit: Some(32),
                },
            },
            Reply::Drafts,
        );
    }

    fn reply(
        &mut self,
        envelope: Envelope,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) {
        let index = envelope
            .draft
            .as_ref()
            .and_then(|id| self.sessions.iter().position(|s| &s.id == id));
        let purpose = envelope.purpose;
        if purpose == Purpose::Release {
            return;
        }
        if purpose == Purpose::Clipboard {
            self.clipboard_in_flight = false;
        }
        let reply = match envelope.result {
            Ok(reply) => reply,
            Err(error) => {
                if let Some(index) = index {
                    let session = &mut self.sessions[index];
                    if matches!(
                        purpose,
                        Purpose::Save(_) | Purpose::SubmitSave(_) | Purpose::Revision(_)
                    ) {
                        session.save_in_flight = false;
                    }
                    if matches!(purpose, Purpose::Read | Purpose::BeforeSubmit) {
                        session.read_in_flight = false;
                    }
                    if purpose == Purpose::Cancel {
                        session.cancel_in_flight = false;
                    }
                    if purpose == Purpose::Close {
                        session.work = Work::Idle;
                        session.close_requested = false;
                    }
                    session.write_conflict |= error == rpc::ChatErrorCode::RevisionConflict
                        || matches!(purpose, Purpose::Save(_));
                    if purpose == Purpose::Start
                        && matches!(
                            error,
                            rpc::ChatErrorCode::Transport
                                | rpc::ChatErrorCode::Timeout
                                | rpc::ChatErrorCode::ProtocolChanged
                                | rpc::ChatErrorCode::PendingSubmission
                                | rpc::ChatErrorCode::DuplicateSubmission
                                | rpc::ChatErrorCode::LeaseLost
                                | rpc::ChatErrorCode::ReconcileOnly
                        )
                    {
                        session.work = Work::Unknown;
                        session.notice = "Submission acceptance is unknown. Refresh to reconcile; do not resend.".into();
                    } else if matches!(
                        purpose,
                        Purpose::Start
                            | Purpose::Upload
                            | Purpose::SubmitSave(_)
                            | Purpose::BeforeSubmit
                    ) {
                        session.restore_before_send(format!(
                            "Chat could not submit: {error:?}. Draft retained."
                        ));
                    } else {
                        session.notice = if session.write_conflict {
                            "This draft conflicts with a saved revision or could not be saved. Ctrl-Shift-S preserves your edit as a separate draft.".into()
                        } else {
                            format!("Chat request failed: {error:?}.")
                        };
                    }
                    if session.submission.is_none()
                        && matches!(
                            purpose,
                            Purpose::Start
                                | Purpose::Upload
                                | Purpose::SubmitSave(_)
                                | Purpose::BeforeSubmit
                        )
                    {
                        self.release_upload(index, handle, frame);
                    }
                } else {
                    self.notice = format!("Chat request failed: {error:?}.");
                    self.status_in_flight = false;
                }
                return;
            }
        };
        match reply {
            Reply::Status(s) => {
                // A reconnect to the same owner must retain unsaved local
                // edits. A restarted owner has a new opaque scope and may
                // restart its generation counter; only the requested snapshot
                // can establish that new scope, never a queued notification.
                let reconnected = self.reconnect_status_pending;
                if reconnected && s.scope_id != self.scope {
                    self.generation = 0;
                }
                self.reconnect_status_pending = false;
                if reconnected && s.availability == rpc::ChatAvailability::Available {
                    self.notice = "Connection restored.".into();
                    for session in &mut self.sessions {
                        if session.work == Work::Idle && !session.write_conflict {
                            session.notice = "Connection restored. Your draft is retained.".into();
                        }
                    }
                }
                self.apply_status(
                    rpc::ChatStatusSnapshot {
                        generation: s.generation,
                        availability: s.availability,
                        scope_id: s.scope_id,
                        capabilities: s.capabilities,
                        model_limits: s.model_limits,
                        turns: s.turns,
                        uploads: s.uploads,
                    },
                    handle,
                    frame,
                );
            }
            Reply::Models(s) if Some(&s.scope_id) == self.scope.as_ref() => {
                self.models = s.models;
                self.default_model = Some(s.default_model);
            }
            Reply::History(s) if Some(&s.scope_id) == self.scope.as_ref() => {
                self.history = s.items;
                self.next_history = s.next_offset;
                self.selected = 0;
            }
            Reply::Drafts(s) if Some(&s.scope_id) == self.scope.as_ref() => {
                self.drafts = s.items;
                self.next_drafts = s.next;
                self.selected = 0;
            }
            Reply::Draft(s) if Some(&s.scope_id) == self.scope.as_ref() => {
                if let Some(index) = index {
                    if purpose == Purpose::Read {
                        self.sessions[index].read_in_flight = false;
                    } else if matches!(purpose, Purpose::Revision(_)) {
                        self.sessions[index].save_in_flight = false;
                    }
                }
                if let Some(draft) = s.draft {
                    if let Purpose::Revision(serial) = purpose {
                        if let Some(index) = index {
                            let session = &mut self.sessions[index];
                            session.revision = Some(draft.revision);
                            session.save_in_flight = false;
                            if session.edit_serial == serial {
                                session.saved_serial = serial;
                            }
                        }
                        return;
                    }
                    if let Some(index) = index
                        && self.sessions[index].dirty()
                        && self.sessions[index].editable()
                    {
                        self.sessions[index].notice =
                            "A newer local edit is still unsaved. Refresh did not replace it."
                                .into();
                        return;
                    }
                    let index = if let Some(index) = self
                        .sessions
                        .iter()
                        .position(|session| session.id == draft.id)
                    {
                        index
                    } else if self.sessions.len() < state::MAX_DRAFTS {
                        self.sessions.push(Session::new(draft.persistence));
                        self.sessions.len() - 1
                    } else {
                        return;
                    };
                    self.sessions[index].id.clone_from(&draft.id);
                    self.sessions[index].apply_draft(draft, s.pending_submission);
                    self.current = index;
                    self.picker = None;
                    self.read(index, false, handle, frame);
                }
            }
            Reply::Saved(s) if Some(&s.scope_id) == self.scope.as_ref() => {
                let Some(index) = index else {
                    return;
                };
                let session = &mut self.sessions[index];
                session.save_in_flight = false;
                session.revision = Some(s.draft.revision);
                if let Purpose::Save(serial) | Purpose::SubmitSave(serial) = purpose {
                    session.saved_serial = serial;
                }
                if matches!(purpose, Purpose::SubmitSave(_)) {
                    self.start_upload_or_turn(index, handle, frame);
                } else {
                    self.close_saved_session(index, handle, frame);
                }
            }
            Reply::Deleted(s) if Some(&s.scope_id) == self.scope.as_ref() => {
                if let Some(index) = index {
                    self.remove_session(index);
                }
            }
            Reply::Conversation(s) if Some(&s.scope_id) == self.scope.as_ref() => {
                let Some(index) = index else {
                    return;
                };
                let session = &mut self.sessions[index];
                session.read_in_flight = false;
                if purpose == Purpose::BeforeSubmit {
                    if s.current_node != session.content.expected_parent_id
                        || s.pending_submission.is_some()
                        || s.continuation_blocker.is_some()
                        || s.source == rpc::ChatConversationSource::LocalCache
                    {
                        session.apply_read(s);
                        session.restore_before_send("The branch changed or cannot continue. Review the refreshed conversation; your draft is retained.".into());
                    } else {
                        self.save(index, true, handle, frame);
                    }
                } else {
                    let accepted = session
                        .turn
                        .as_ref()
                        .is_some_and(|t| t.acceptance == rpc::ChatAcceptance::Accepted)
                        || s.pending_submission.as_ref().is_some_and(|p| p.accepted);
                    session.apply_read(s);
                    if accepted {
                        // Both services advance the submitted revision. Refresh it
                        // without overwriting a newer edit in the local composer.
                        self.read_advanced_revision(index, handle, frame);
                    }
                }
            }
            Reply::Upload(s) if Some(&s.scope_id) == self.scope.as_ref() => {
                if let Some(index) = index {
                    self.sessions[index].upload_operation = Some(s.operation_id);
                    if self.sessions[index].stop_requested {
                        self.stop_operation(index, handle, frame);
                    }
                }
            }
            Reply::Turn(s) => self.turn(s.turn, handle, frame),
            Reply::CancelTurn(s) => {
                if let Some(turn) = s.turn {
                    self.turn(turn, handle, frame);
                }
                if let Some(index) = index {
                    self.sessions[index].apply_cancel_result(
                        s.local_stop_requested,
                        s.server_cancellation_confirmed,
                    );
                }
            }
            Reply::CancelUpload(s) => {
                if let Some(upload) = s.upload {
                    self.upload(upload, handle, frame);
                }
                if let Some(index) = index {
                    self.sessions[index].notice =
                        "Local upload stop requested. Selections are retained.".into();
                }
            }
            Reply::Clipboard(bytes) => {
                if let Some(index) = index {
                    self.stage_clipboard(index, bytes);
                }
            }
            _ => {}
        }
    }

    fn read_advanced_revision(
        &mut self,
        index: usize,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) {
        let Some(scope_id) = self.scope.clone() else {
            return;
        };
        let session = &mut self.sessions[index];
        session.save_in_flight = true;
        let draft_id = session.id.clone();
        let purpose = Purpose::Revision(session.edit_serial);
        let params = rpc::ChatDraftReadParams {
            scope_id,
            persistence: session.persistence,
            draft_id: draft_id.clone(),
        };
        self.request(
            handle,
            frame,
            Some(draft_id),
            purpose,
            rpc::ClientRequest::ChatDraftRead {
                request_id: request_id(),
                params,
            },
            Reply::Draft,
        );
    }

    fn read_draft(
        &mut self,
        index: usize,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) {
        let Some(scope_id) = self.scope.clone() else {
            return;
        };
        let s = &mut self.sessions[index];
        if s.read_in_flight {
            return;
        }
        s.read_in_flight = true;
        let draft_id = s.id.clone();
        let persistence = s.persistence;
        self.request(
            handle,
            frame,
            Some(draft_id.clone()),
            Purpose::Read,
            rpc::ClientRequest::ChatDraftRead {
                request_id: request_id(),
                params: rpc::ChatDraftReadParams {
                    scope_id,
                    persistence,
                    draft_id,
                },
            },
            Reply::Draft,
        );
    }
}

impl super::App {
    pub(super) fn switch_native_surface(
        &mut self,
        chat: bool,
        tui: &mut crate::tui::Tui,
        server: &AppServerSession,
    ) {
        if !chat {
            self.native_chat.flush(server, &tui.frame_requester());
            self.native_chat.active = false;
            self.native_chat.codex_unread = false;
            self.cancel_pending_key_chord();
            tui.frame_requester().schedule_frame();
            return;
        }
        let error = if !self
            .config
            .features
            .enabled(codex_features::Feature::ChatgptChat)
        {
            Some(
                "Chat is experimental. Enable features.chatgpt_chat and restart before opening it.",
            )
        } else if !tui.is_owned_screen()
            || !tui.is_alt_screen_enabled()
            || !tui.is_alt_screen_active()
        {
            Some(
                "Chat requires the owned full-screen transcript and alternate screen. Change the TUI screen mode and restart.",
            )
        } else if !local_connection(&self.app_server_target) {
            Some(
                "Chat requires an embedded app-server or trusted local daemon socket. TCP connections are unsupported.",
            )
        } else {
            None
        };
        if let Some(error) = error {
            self.chat_widget.add_error_message(error.into());
            return;
        }
        self.cancel_pending_key_chord();
        self.native_chat.open(server, &tui.frame_requester());
    }

    pub(super) fn handle_native_chat_event(
        &mut self,
        tui: &mut crate::tui::Tui,
        server: &mut AppServerSession,
        event: &crate::tui::TuiEvent,
    ) -> color_eyre::eyre::Result<Option<super::AppRunControl>> {
        use crate::key_hint::KeyBindingListExt;
        use crate::tui::TuiEvent;
        self.native_chat.tick(server, &tui.frame_requester());
        let contexts =
            crate::keymap::KeymapContextSet::native_chat(self.native_chat.picker.is_some());
        let routed = if self.native_chat.active {
            self.key_chord_matcher.expire(contexts);
            if let TuiEvent::Key(key) = event {
                match self
                    .key_chord_matcher
                    .advance(*key, &self.keymap.chords, contexts)
                {
                    crate::keymap::KeyChordMatch::Completed(key) => Some(TuiEvent::Key(key)),
                    crate::keymap::KeyChordMatch::PassThrough => None,
                    _ => {
                        tui.frame_requester().schedule_frame();
                        return Ok(Some(super::AppRunControl::Continue));
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        let event = routed.as_ref().unwrap_or(event);
        if let TuiEvent::Key(key) = event
            && self.keymap.app.switch_surface.is_pressed(*key)
            && key.kind == crossterm::event::KeyEventKind::Press
            && (self.native_chat.active
                || self
                    .config
                    .features
                    .enabled(codex_features::Feature::ChatgptChat))
        {
            self.switch_native_surface(!self.native_chat.active, tui, server);
            return Ok(Some(super::AppRunControl::Continue));
        }
        if !self.native_chat.active {
            return Ok(None);
        }
        let action = self.native_chat.input(
            event,
            &self.keymap,
            self.config.cwd.as_path(),
            self.local_settings.tui.disable_paste_burst.unwrap_or(false),
            server,
            &tui.frame_requester(),
        );
        match action {
            input::InputAction::Codex => self.switch_native_surface(false, tui, server),
            input::InputAction::Quit => {
                self.native_chat.flush(server, &tui.frame_requester());
                return Ok(Some(super::AppRunControl::Exit(
                    super::ExitReason::UserRequested,
                )));
            }
            input::InputAction::Stay => {}
        }
        if matches!(
            event,
            TuiEvent::Draw | TuiEvent::Resize(_) | TuiEvent::Resume | TuiEvent::FocusGained
        ) {
            let size = tui.screen_size_for_event(event)?;
            self.chat_widget.pre_draw_tick();
            self.render_native_chat(tui, size)?;
        }
        Ok(Some(super::AppRunControl::Continue))
    }

    pub(super) fn native_surface_hint(&self) -> String {
        self.keymap
            .primary_hint(crate::keymap::KeymapContext::Global, "switch_surface")
            .map_or_else(
                || "/chat /codex".into(),
                crate::key_hint::ShortcutHint::display_label,
            )
    }

    pub(super) fn render_native_chat(
        &mut self,
        tui: &mut crate::tui::Tui,
        size: ratatui::layout::Size,
    ) -> color_eyre::eyre::Result<ratatui::layout::Rect> {
        use ratatui::widgets::Widget;
        let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
        let header = self.native_chat.header(
            self.chat_widget.is_agent_turn_running(),
            self.chat_widget.has_active_modal(),
            &self.native_surface_hint(),
        );
        tui.draw(size.height, |frame| {
            header.render(
                ratatui::layout::Rect::new(0, 0, size.width, size.height.min(1)),
                frame.buffer,
            );
            if let Some(cursor) = self.native_chat.render(
                ratatui::layout::Rect::new(
                    0,
                    1.min(size.height),
                    size.width,
                    size.height.saturating_sub(1),
                ),
                frame.buffer,
            ) {
                frame.set_cursor_position(cursor);
            }
        })?;
        Ok(area)
    }
}
