//! Chat input never dispatches shell escapes, repository mentions, skills, or
//! Codex tools. Only the explicit Chat commands below have special meaning.
use super::*;
use crate::key_hint::KeyBindingListExt;
use crate::keymap::RuntimeKeymap;
use crate::tui::TuiEvent;
use crossterm::event::KeyCode;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseEventKind;
use std::path::Path;

pub(super) enum InputAction {
    Stay,
    Codex,
    Quit,
}

impl NativeChat {
    pub(super) fn input(
        &mut self,
        event: &TuiEvent,
        keymap: &RuntimeKeymap,
        cwd: &Path,
        disable_paste_burst: bool,
        server: &AppServerSession,
        frame: &FrameRequester,
    ) -> InputAction {
        let handle = server.request_handle();
        if self.scope.is_none() {
            if let TuiEvent::Key(key) = event
                && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            {
                if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return InputAction::Quit;
                }
                if key.code == KeyCode::F(5) {
                    self.refresh(&handle, frame);
                }
            }
            frame.schedule_frame();
            return InputAction::Stay;
        }
        match event {
            TuiEvent::Paste(text) => {
                if let Some(session) = self.sessions.get_mut(self.current)
                    && session.editable()
                {
                    session.paste_burst.clear_after_explicit_paste();
                    if session.composer.text().len().saturating_add(text.len())
                        <= state::MAX_TEXT_BYTES
                    {
                        let cursor = session.composer.cursor();
                        session
                            .composer
                            .insert_str_at(cursor, &text.replace("\r\n", "\n").replace('\r', "\n"));
                        session.edited();
                    } else {
                        session.notice = "Paste exceeds the 256 KiB message limit.".into();
                    }
                }
            }
            TuiEvent::Mouse(mouse) => {
                if let Some(session) = self.sessions.get_mut(self.current) {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            session.scroll = session.scroll.saturating_add(3)
                        }
                        MouseEventKind::ScrollDown => {
                            session.scroll = session.scroll.saturating_sub(3)
                        }
                        _ => {}
                    }
                }
            }
            TuiEvent::Key(key)
                if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
            {
                if key.code == KeyCode::F(2) {
                    self.new_session(if key.modifiers.contains(KeyModifiers::SHIFT) {
                        rpc::ChatPersistence::Temporary
                    } else {
                        rpc::ChatPersistence::Saved
                    });
                    frame.schedule_frame();
                    return InputAction::Stay;
                }
                if key.code == KeyCode::F(3) {
                    self.picker = Some(Picker::Sessions);
                    self.selected = self.current;
                    frame.schedule_frame();
                    return InputAction::Stay;
                }
                if matches!(key.code, KeyCode::Char('s' | 'S'))
                    && key
                        .modifiers
                        .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                {
                    if self
                        .sessions
                        .get(self.current)
                        .is_some_and(|s| s.write_conflict && s.editable() && !s.save_in_flight)
                    {
                        self.release_upload(self.current, &handle, frame);
                        self.preserve_conflicted_draft();
                    }
                    frame.schedule_frame();
                    return InputAction::Stay;
                }
                if key.code == KeyCode::F(5) {
                    self.refresh(&handle, frame);
                    frame.schedule_frame();
                    return InputAction::Stay;
                }
                if self.picker.is_some() {
                    if keymap.list.cancel.is_pressed(*key) {
                        self.picker = None;
                    } else if keymap.list.move_up.is_pressed(*key) {
                        self.selected = self.selected.saturating_sub(1);
                    } else if keymap.list.move_down.is_pressed(*key) {
                        self.selected = self
                            .selected
                            .saturating_add(1)
                            .min(self.picker_len().saturating_sub(1));
                    } else if keymap.list.accept.is_pressed(*key) {
                        self.pick(&handle, frame);
                    }
                    return InputAction::Stay;
                }
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.cancel(&handle, frame);
                } else if key.code == KeyCode::Char('d')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && self
                        .sessions
                        .get(self.current)
                        .is_none_or(|s| s.composer.is_empty() && s.editable())
                {
                    return InputAction::Quit;
                } else if key.code == KeyCode::Char('v')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    self.capture_clipboard(frame);
                } else if key.code == KeyCode::PageUp {
                    if let Some(s) = self.sessions.get_mut(self.current) {
                        s.scroll = s.scroll.saturating_add(10);
                    }
                } else if key.code == KeyCode::PageDown {
                    if let Some(s) = self.sessions.get_mut(self.current) {
                        s.scroll = s.scroll.saturating_sub(10);
                    }
                } else if keymap.composer.submit.is_pressed(*key) {
                    if !disable_paste_burst
                        && key.code == KeyCode::Enter
                        && let Some(session) = self.sessions.get_mut(self.current)
                        && session.editable()
                        && session
                            .paste_burst
                            .direct_insert_newline_should_insert(Instant::now())
                    {
                        session.paste_burst.extend_window(Instant::now());
                        if session.composer.text().len() < state::MAX_TEXT_BYTES {
                            session
                                .composer
                                .insert_str_at(session.composer.cursor(), "\n");
                            session.edited();
                        }
                        frame.schedule_frame();
                        return InputAction::Stay;
                    }
                    let text = self
                        .sessions
                        .get(self.current)
                        .map(|s| s.composer.text().trim().to_owned())
                        .unwrap_or_default();
                    if text.starts_with('/') {
                        return self.command(&text, cwd, &handle, frame);
                    }
                    self.submit(&handle, frame);
                } else if let Some(session) = self.sessions.get_mut(self.current)
                    && session.editable()
                {
                    if !disable_paste_burst
                        && matches!(key.code, KeyCode::Char(_))
                        && !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        )
                    {
                        if session
                            .paste_burst
                            .on_plain_char_no_hold(Instant::now())
                            .is_some()
                        {
                            session.paste_burst.extend_window(Instant::now());
                        }
                    } else {
                        session.paste_burst.clear_after_explicit_paste();
                    }
                    session.composer.set_keymap_bindings(keymap);
                    let before = session.composer.text().to_owned();
                    session.composer.input_with_keymap(*key, &keymap.editor);
                    if session.composer.text().len() > state::MAX_TEXT_BYTES {
                        session.composer.set_text_clearing_elements(&before);
                        session.notice = "Message exceeds the 256 KiB limit.".into();
                    } else if session.composer.text() != before {
                        session.edited();
                    }
                }
            }
            _ => {}
        }
        frame.schedule_frame();
        InputAction::Stay
    }

    fn command(
        &mut self,
        text: &str,
        cwd: &Path,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) -> InputAction {
        let (command, argument) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let argument = argument.trim();
        // Commands may navigate during activity, but never rewrite its immutable
        // submitted snapshot. A command entered in an idle draft is consumed.
        if let Some(s) = self.sessions.get_mut(self.current)
            && s.editable()
        {
            s.composer = crate::bottom_pane::TextArea::new();
            s.edited();
        }
        match command {
            "/codex" => return InputAction::Codex,
            "/new" => self.new_session(rpc::ChatPersistence::Saved),
            "/temporary" => self.new_session(rpc::ChatPersistence::Temporary),
            "/history" => { self.picker = Some(Picker::History); self.selected = 0; self.load_history(None, handle, frame); }
            "/drafts" => { self.picker = Some(Picker::Drafts); self.selected = 0; self.load_drafts(handle, frame, None); }
            "/chats" => { self.picker = Some(Picker::Sessions); self.selected = 0; }
            "/model" => { self.picker = Some(Picker::Models); self.selected = 0; }
            "/attach" if !argument.is_empty() => {
                let path = Path::new(argument); let path = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
                if let Some(s) = self.sessions.get_mut(self.current) && s.editable() {
                    if s.content.selections.len() >= 20 { s.notice = "At most 20 attachments can be selected.".into(); }
                    else { let id = Uuid::new_v4().to_string(); s.content.selections.push(rpc::ChatFileSelection { id, name: path.file_name().unwrap_or_default().to_string_lossy().into_owned(), source: Some(path.to_string_lossy().into_owned()), snapshot: None }); s.edited(); s.notice = "File selected. Validation and upload start on Submit.".into(); }
                }
            }
            "/attach" => { self.picker = Some(Picker::Attachments); self.selected = 0; self.notice = "Use /attach <path> to select a file; Enter removes a selection.".into(); }
            "/refresh" => self.refresh(handle, frame),
            "/close" => {
                if let Some(s) = self.sessions.get_mut(self.current) {
                    if s.editable() && !s.write_conflict {
                        s.close_requested = true;
                        s.notice = "Saving this draft before closing…".into();
                        self.save(self.current, false, handle, frame);
                    } else { self.notice = "Resolve this draft's pending operation or save conflict before closing.".into(); }
                }
            }
            "/help" => self.notice = "/new · /temporary · /history · /drafts · /chats · /model · /attach <path> · /refresh · /close · /codex".into(),
            _ => { if let Some(s) = self.sessions.get_mut(self.current) && s.editable() { s.composer.set_text_clearing_elements(text); s.edited(); s.notice = "Unknown Chat command. Use /help for the available commands.".into(); } }
        }
        frame.schedule_frame();
        InputAction::Stay
    }

    fn preserve_conflicted_draft(&mut self) {
        let Some(session) = self.sessions.get_mut(self.current) else {
            return;
        };
        if !session.write_conflict || !session.editable() || session.save_in_flight {
            return;
        }
        // Keep the other client's stored revision intact. The local edit obtains
        // a distinct draft ID; no user message is transmitted by this action.
        session.id = Uuid::new_v4().to_string();
        session.read_in_flight = false;
        session.turn = None;
        session.revision = None;
        session.write_conflict = false;
        session.upload = None;
        session.upload_operation = None;
        session.saved_serial = 0;
        session.edit_serial = 1;
        session.edited_at = Instant::now();
        session.notice =
            "Saving your local edit as a separate draft. The other draft is unchanged.".into();
    }

    fn refresh(&mut self, handle: &AppServerRequestHandle, frame: &FrameRequester) {
        self.status(handle, frame);
        if !self.sessions.is_empty() {
            if matches!(
                self.sessions[self.current].work,
                Work::Saving | Work::Uploading | Work::Starting | Work::Streaming
            ) {
                return;
            }
            if self.sessions[self.current]
                .content
                .conversation_id
                .is_some()
            {
                self.read(self.current, false, handle, frame);
            } else if self.sessions[self.current].revision.is_some() {
                self.read_draft(self.current, handle, frame);
            }
        }
    }

    fn load_history(
        &mut self,
        offset: Option<u32>,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) {
        let Some(scope_id) = self.scope.clone() else {
            return;
        };
        self.request(
            handle,
            frame,
            None,
            Purpose::Browse,
            rpc::ClientRequest::ChatConversationList {
                request_id: request_id(),
                params: rpc::ChatConversationListParams {
                    scope_id,
                    offset,
                    limit: Some(32),
                },
            },
            Reply::History,
        );
    }

    pub(super) fn picker_len(&self) -> usize {
        match self.picker {
            Some(Picker::Models) => self.models.len() + 1,
            Some(Picker::History) => self.history.len() + usize::from(self.next_history.is_some()),
            Some(Picker::Drafts) => self.drafts.len() + usize::from(self.next_drafts.is_some()),
            Some(Picker::Sessions) => self.sessions.len(),
            Some(Picker::Attachments) => self
                .sessions
                .get(self.current)
                .map_or(0, |s| s.content.selections.len()),
            None => 0,
        }
    }

    fn pick(&mut self, handle: &AppServerRequestHandle, frame: &FrameRequester) {
        let Some(scope_id) = self.scope.clone() else {
            return;
        };
        match self.picker {
            Some(Picker::Models) => {
                if let Some(s) = self.sessions.get_mut(self.current)
                    && s.editable()
                {
                    s.content.requested_model = self
                        .selected
                        .checked_sub(1)
                        .and_then(|i| self.models.get(i))
                        .map(|m| m.id.clone());
                    s.edited();
                }
                self.picker = None;
            }
            Some(Picker::Sessions) if self.selected < self.sessions.len() => {
                self.current = self.selected;
                self.sessions[self.current].unread = false;
                self.picker = None;
            }
            Some(Picker::Attachments) => {
                if self
                    .sessions
                    .get(self.current)
                    .is_some_and(|s| s.editable() && self.selected < s.content.selections.len())
                {
                    self.release_upload(self.current, handle, frame);
                }
                if let Some(s) = self.sessions.get_mut(self.current)
                    && s.editable()
                    && self.selected < s.content.selections.len()
                {
                    let file = s.content.selections.remove(self.selected);
                    s.clipboard.remove(&file.id);
                    s.upload = None;
                    s.edited();
                    self.selected = self
                        .selected
                        .min(s.content.selections.len().saturating_sub(1));
                }
            }
            Some(Picker::History) => {
                if self.selected == self.history.len() {
                    self.load_history(self.next_history, handle, frame);
                    return;
                }
                let Some(item) = self.history.get(self.selected) else {
                    return;
                };
                let id = item.id.clone();
                if let Some(index) = self
                    .sessions
                    .iter()
                    .position(|s| s.content.conversation_id.as_ref() == Some(&id))
                {
                    self.current = index;
                } else if self.sessions.len() < state::MAX_DRAFTS {
                    self.new_session(rpc::ChatPersistence::Saved);
                    self.sessions[self.current].content.conversation_id = Some(id);
                } else {
                    self.notice = "Close an idle chat before opening another.".into();
                    return;
                }
                self.picker = None;
                self.read(self.current, false, handle, frame);
            }
            Some(Picker::Drafts) => {
                if self.selected == self.drafts.len() {
                    self.load_drafts(handle, frame, self.next_drafts.clone());
                    return;
                }
                let Some(draft) = self.drafts.get(self.selected) else {
                    return;
                };
                if let Some(index) = self.sessions.iter().position(|s| s.id == draft.id) {
                    self.current = index;
                    self.picker = None;
                    return;
                }
                self.request(
                    handle,
                    frame,
                    None,
                    Purpose::Browse,
                    rpc::ClientRequest::ChatDraftRead {
                        request_id: request_id(),
                        params: rpc::ChatDraftReadParams {
                            scope_id,
                            persistence: rpc::ChatPersistence::Saved,
                            draft_id: draft.id.clone(),
                        },
                    },
                    Reply::Draft,
                );
            }
            _ => {}
        }
    }

    fn cancel(&mut self, handle: &AppServerRequestHandle, frame: &FrameRequester) {
        let Some(session) = self.sessions.get_mut(self.current) else {
            return;
        };
        if session.submission.is_none() && session.work == Work::Idle {
            session.composer = crate::bottom_pane::TextArea::new();
            session.edited();
            return;
        }
        session.stop_requested = true;
        session.notice = "Local stop requested. Server cancellation is not confirmed.".into();
        self.stop_operation(self.current, handle, frame);
    }

    /// Release unused, scoped upload handles when a draft is changed or closed.
    /// Cancellation is idempotent and never sends a user message.
    pub(super) fn release_upload(
        &mut self,
        index: usize,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) {
        let Some(scope_id) = self.scope.clone() else {
            return;
        };
        let session = &mut self.sessions[index];
        let Some(operation_id) = session.upload_operation.take() else {
            return;
        };
        if let Some(upload) = &mut session.upload {
            for file in &mut upload.files {
                file.handle = None;
            }
        }
        self.request(
            handle,
            frame,
            None,
            Purpose::Release,
            rpc::ClientRequest::ChatAttachmentCancel {
                request_id: request_id(),
                params: rpc::ChatAttachmentCancelParams {
                    scope_id,
                    operation_id,
                },
            },
            Reply::CancelUpload,
        );
    }

    pub(super) fn close_saved_session(
        &mut self,
        index: usize,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) {
        let session = &self.sessions[index];
        if !session.close_requested || session.dirty() || !session.editable() {
            return;
        }
        self.release_upload(index, handle, frame);
        let session = &mut self.sessions[index];
        if session.persistence == rpc::ChatPersistence::Temporary {
            let (Some(scope_id), Some(expected_revision)) =
                (self.scope.clone(), session.revision.clone())
            else {
                return;
            };
            let draft_id = session.id.clone();
            session.work = Work::Saving;
            self.request(
                handle,
                frame,
                Some(draft_id.clone()),
                Purpose::Close,
                rpc::ClientRequest::ChatDraftDelete {
                    request_id: request_id(),
                    params: rpc::ChatDraftDeleteParams {
                        scope_id,
                        persistence: rpc::ChatPersistence::Temporary,
                        draft_id,
                        expected_revision,
                    },
                },
                Reply::Deleted,
            );
            return;
        }
        self.remove_session(index);
    }

    pub(super) fn remove_session(&mut self, index: usize) {
        self.sessions.remove(index);
        if self.current >= index {
            self.current = self.current.saturating_sub(1);
        }
        if self.sessions.is_empty() {
            self.new_session(rpc::ChatPersistence::Saved);
        }
    }

    pub(super) fn stop_operation(
        &mut self,
        index: usize,
        handle: &AppServerRequestHandle,
        frame: &FrameRequester,
    ) {
        let Some(scope_id) = self.scope.clone() else {
            return;
        };
        let s = &mut self.sessions[index];
        if s.cancel_in_flight {
            return;
        }
        let draft_id = s.id.clone();
        if let Some(turn) = &s.turn
            && turn.phase != rpc::ChatTurnPhase::Finished
        {
            let operation_id = turn.operation_id.clone();
            s.cancel_in_flight = true;
            self.request(
                handle,
                frame,
                Some(draft_id),
                Purpose::Cancel,
                rpc::ClientRequest::ChatTurnCancel {
                    request_id: request_id(),
                    params: rpc::ChatTurnCancelParams {
                        scope_id,
                        operation_id,
                    },
                },
                Reply::CancelTurn,
            );
        } else if let Some(operation_id) = s.upload_operation.clone() {
            s.cancel_in_flight = true;
            self.request(
                handle,
                frame,
                Some(draft_id),
                Purpose::Cancel,
                rpc::ClientRequest::ChatAttachmentCancel {
                    request_id: request_id(),
                    params: rpc::ChatAttachmentCancelParams {
                        scope_id,
                        operation_id,
                    },
                },
                Reply::CancelUpload,
            );
        }
    }

    fn capture_clipboard(&mut self, frame: &FrameRequester) {
        if self.clipboard_in_flight
            || self
                .sessions
                .get(self.current)
                .is_none_or(|s| !s.editable())
        {
            return;
        }
        let retained: usize = self
            .sessions
            .iter()
            .flat_map(|s| s.clipboard.values())
            .map(Vec::len)
            .sum();
        if retained > 128 * 1024 * 1024 - state::MAX_CLIPBOARD_BYTES {
            self.notice = "Clipboard staging is full. Remove an attachment and try again.".into();
            return;
        }
        self.clipboard_in_flight = true;
        let draft = Some(self.sessions[self.current].id.clone());
        let epoch = self.epoch;
        let sender = self.sender.clone();
        let frame = frame.clone();
        self.tasks.push(tokio::spawn(async move {
            let result =
                tokio::task::spawn_blocking(crate::clipboard_paste::paste_chat_image_as_png)
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .map(Reply::Clipboard)
                    .ok_or(rpc::ChatErrorCode::AttachmentInvalidFormat);
            if sender
                .send(Envelope {
                    epoch,
                    draft,
                    purpose: Purpose::Clipboard,
                    result,
                })
                .await
                .is_ok()
            {
                frame.schedule_frame();
            }
        }));
    }

    pub(super) fn stage_clipboard(&mut self, index: usize, bytes: Vec<u8>) {
        let s = &mut self.sessions[index];
        if !s.editable() {
            s.notice =
                "Clipboard read completed after Submit. Paste again when this draft is idle."
                    .into();
            return;
        }
        let retained: usize = s.clipboard.values().map(Vec::len).sum();
        if s.content.selections.len() >= 20
            || retained.saturating_add(bytes.len()) > state::MAX_CLIPBOARD_BYTES
        {
            s.notice = "Clipboard attachment exceeds the staging limit.".into();
            return;
        }
        let id = Uuid::new_v4().to_string();
        s.clipboard.insert(id.clone(), bytes);
        s.content.selections.push(rpc::ChatFileSelection {
            id,
            source: None,
            name: "clipboard.png".into(),
            snapshot: None,
        });
        s.edited();
        s.notice = "Clipboard image selected. It stays in memory until Submit.".into();
    }
}
