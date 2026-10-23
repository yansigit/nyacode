//! Chat composer and operation state. Content has no Debug implementation and
//! never enters Codex's composer, generic prompt history, or transcript recorder.

use crate::bottom_pane::TextArea;
use codex_app_server_protocol as rpc;
use std::collections::BTreeMap;
use std::time::Instant;
use uuid::Uuid;

pub(super) const MAX_DRAFTS: usize = 32;
pub(super) const MAX_TEXT_BYTES: usize = 256 * 1024;
pub(super) const MAX_CLIPBOARD_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Work {
    Idle,
    Saving,
    Uploading,
    Starting,
    Streaming,
    Reading,
    Unknown,
}

pub(super) struct Submission {
    pub id: String,
    pub message_id: String,
    pub text: String,
    pub parent: String,
}

pub(super) struct Session {
    pub id: String,
    pub persistence: rpc::ChatPersistence,
    pub revision: Option<String>,
    pub title: String,
    pub composer: TextArea,
    pub editor_state: crate::bottom_pane::TextAreaState,
    pub paste_burst: crate::bottom_pane::PasteBurst,
    pub content: rpc::ChatDraftContent,
    pub clipboard: BTreeMap<String, Vec<u8>>,
    pub edit_serial: u64,
    pub saved_serial: u64,
    pub edited_at: Instant,
    pub save_in_flight: bool,
    pub write_conflict: bool,
    pub close_requested: bool,
    pub work: Work,
    pub submission: Option<Submission>,
    pub turn: Option<rpc::ChatTurnSnapshot>,
    pub upload: Option<rpc::ChatAttachmentSnapshot>,
    pub messages: Vec<rpc::ChatMessage>,
    pub pending: Option<rpc::ChatPendingSubmission>,
    pub blocker: Option<rpc::ChatContinuationBlocker>,
    pub notice: String,
    pub scroll: usize,
    pub unread: bool,
    pub read_in_flight: bool,
    pub upload_operation: Option<String>,
    pub stop_requested: bool,
    pub cancel_in_flight: bool,
}

impl Session {
    pub(super) fn new(persistence: rpc::ChatPersistence) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            persistence,
            revision: None,
            title: if persistence == rpc::ChatPersistence::Temporary {
                "Temporary chat"
            } else {
                "New chat"
            }
            .into(),
            composer: TextArea::new(),
            editor_state: Default::default(),
            paste_burst: Default::default(),
            content: rpc::ChatDraftContent {
                conversation_id: None,
                expected_parent_id: None,
                requested_model: None,
                text: String::new(),
                selections: Vec::new(),
            },
            clipboard: BTreeMap::new(),
            edit_serial: 0,
            saved_serial: 0,
            edited_at: Instant::now(),
            save_in_flight: false,
            write_conflict: false,
            close_requested: false,
            work: Work::Idle,
            submission: None,
            turn: None,
            upload: None,
            messages: Vec::new(),
            pending: None,
            blocker: None,
            notice: String::new(),
            scroll: 0,
            unread: false,
            read_in_flight: false,
            upload_operation: None,
            stop_requested: false,
            cancel_in_flight: false,
        }
    }

    pub(super) fn dirty(&self) -> bool {
        self.edit_serial != self.saved_serial
    }

    pub(super) fn editable(&self) -> bool {
        matches!(self.work, Work::Idle) && self.submission.is_none() && self.pending.is_none()
    }

    pub(super) fn edited(&mut self) {
        self.close_requested = false;
        self.content.text = self.composer.text().to_owned();
        self.edit_serial = self.edit_serial.saturating_add(1);
        self.edited_at = Instant::now();
    }

    pub(super) fn submission(&mut self) -> bool {
        if !self.editable()
            || self.write_conflict
            || self.blocker.is_some()
            || self.save_in_flight
            || self.read_in_flight
        {
            self.notice = "This draft is busy or requires recovery before sending.".into();
            return false;
        }
        if self.composer.text().trim().is_empty() && self.content.selections.is_empty() {
            return false;
        }
        if self.composer.text().len() > MAX_TEXT_BYTES {
            self.notice = "The message exceeds the 256 KiB limit.".into();
            return false;
        }
        if self
            .content
            .selections
            .iter()
            .any(|file| file.source.is_none() && !self.clipboard.contains_key(&file.id))
        {
            self.notice =
                "A clipboard selection was lost across restart. Remove it and attach it again."
                    .into();
            return false;
        }
        self.content.text = self.composer.text().to_owned();
        let parent = self
            .content
            .expected_parent_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        self.content.expected_parent_id = Some(parent.clone());
        self.edited();
        self.stop_requested = false;
        self.cancel_in_flight = false;
        self.submission = Some(Submission {
            id: Uuid::new_v4().to_string(),
            message_id: Uuid::new_v4().to_string(),
            text: self.content.text.clone(),
            parent,
        });
        self.work = Work::Saving;
        self.notice = "Saving the submitted draft…".into();
        true
    }

    pub(super) fn restore_before_send(&mut self, notice: String) {
        self.work = Work::Idle;
        self.submission = None;
        self.notice = notice;
    }

    pub(super) fn apply_draft(
        &mut self,
        draft: rpc::ChatDraft,
        pending: Option<rpc::ChatPendingSubmission>,
    ) {
        if self.submission.is_some() {
            // An absent pending record does not prove that an ambiguous send
            // was rejected. Keep its immutable IDs and composer until exact
            // server-message reconciliation supplies positive evidence.
            if self.work != Work::Unknown {
                return;
            }
            if let Some(pending) = pending
                && self.submission.as_ref().is_some_and(|submission| {
                    submission.id == pending.submission_id
                        && submission.message_id == pending.client_message_id
                })
            {
                self.content
                    .conversation_id
                    .clone_from(&pending.conversation_id);
                self.pending = Some(pending);
            }
            self.notice = "Submission acceptance is unresolved. It will not be resent.".into();
            return;
        }
        self.revision = Some(draft.revision);
        self.persistence = draft.persistence;
        self.content = draft.content;
        self.composer.set_text_clearing_elements(&self.content.text);
        self.saved_serial = self.edit_serial;
        self.write_conflict = false;
        if let Some(pending) = &pending
            && self.content.conversation_id.is_none()
        {
            self.content
                .conversation_id
                .clone_from(&pending.conversation_id);
        }
        self.pending = pending;
        self.work = if self.pending.is_some() {
            Work::Unknown
        } else {
            Work::Idle
        };
        if self.pending.is_some() {
            self.notice =
                "A saved submission requires reconciliation. It will not be resent.".into();
        }
    }

    pub(super) fn apply_turn(&mut self, turn: rpc::ChatTurnSnapshot, visible: bool) -> bool {
        if turn.draft_id != self.id || turn.persistence != self.persistence {
            return false;
        }
        if self.turn.as_ref().is_some_and(|previous| {
            previous.operation_id == turn.operation_id && previous.sequence >= turn.sequence
        }) {
            return false;
        }
        if let Some(submission) = &self.submission {
            if turn.submission_id != submission.id
                || turn.client_message_id != submission.message_id
            {
                return false;
            }
        } else if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.submission_id != turn.submission_id)
            || self
                .turn
                .as_ref()
                .is_some_and(|previous| previous.operation_id != turn.operation_id)
        {
            return false;
        }
        if turn.conversation_id.is_some() {
            self.content
                .conversation_id
                .clone_from(&turn.conversation_id);
        }
        self.unread |= !visible;
        for message in &turn.messages {
            if let Some(old) = self.messages.iter_mut().find(|old| old.id == message.id) {
                *old = message.clone();
            } else {
                self.messages.push(message.clone());
            }
        }
        let accepted = turn.acceptance == rpc::ChatAcceptance::Accepted;
        if accepted {
            // The server established acceptance. Later generation failure must
            // not restore the user message to a sendable composer.
            // Drop the editor too: its kill buffer must not retain an accepted
            // temporary submission behind an apparently empty composer.
            self.composer = TextArea::new();
            self.content.text.clear();
            self.content.selections.clear();
            self.clipboard.clear();
        }
        let finished = turn.phase == rpc::ChatTurnPhase::Finished;
        self.work = if !finished {
            Work::Streaming
        } else if turn.recovery_pending {
            Work::Unknown
        } else if accepted {
            Work::Reading
        } else {
            Work::Idle
        };
        self.notice = if !finished && self.stop_requested {
            "Local stop requested. Server cancellation is not confirmed.".into()
        } else if !finished && !accepted {
            "Awaiting message acceptance…".into()
        } else if !finished {
            "Receiving response…".into()
        } else if turn.error == Some(rpc::ChatErrorCode::LocalCancellation) {
            if turn.recovery_pending && !accepted {
                "Stopped locally; server cancellation unconfirmed. Acceptance unresolved; no resend."
                    .into()
            } else {
                "Local stream stopped. Server cancellation is not confirmed.".into()
            }
        } else if turn.recovery_pending && !accepted {
            "Acceptance is unresolved. This submission will not be resent.".into()
        } else if turn.recovery_pending {
            "Message accepted; generation outcome is unresolved. This message will not be resent."
                .into()
        } else if let Some(error) = turn.error {
            format!("Turn ended: {error:?}")
        } else if turn.outcome == rpc::ChatGenerationOutcome::Completed {
            "Response completed.".into()
        } else {
            "The response ended before completion.".into()
        };
        if finished && !accepted && !turn.recovery_pending {
            self.submission = None;
        }
        self.turn = Some(turn);
        finished
    }

    pub(super) fn apply_cancel_result(&mut self, requested: bool, confirmed: bool) {
        if confirmed {
            self.notice = "Server cancellation confirmed.".into();
        } else if requested
            && self
                .turn
                .as_ref()
                .is_none_or(|turn| turn.phase != rpc::ChatTurnPhase::Finished)
        {
            self.notice = "Local stop requested. Server cancellation is not confirmed.".into();
        }
    }

    pub(super) fn apply_read(&mut self, read: rpc::ChatConversationReadResponse) -> bool {
        if read.persistence != self.persistence
            || self.content.conversation_id.as_deref() != Some(&read.conversation_id)
        {
            return false;
        }
        // A read can finish after a newer local send starts. Only its terminal
        // operation snapshot may release that submission; a conversation GET
        // is not evidence that generation has finished.
        if matches!(
            self.work,
            Work::Saving | Work::Uploading | Work::Starting | Work::Streaming
        ) {
            return false;
        }
        let mut ordered = Vec::new();
        if read.source == rpc::ChatConversationSource::TemporaryMemory {
            // The temporary service retains the sole observed stream in order.
            // Assistant parent links are intentionally unknown; walking them
            // would discard earlier turns. Do not invent branch relationships.
            if self.persistence != rpc::ChatPersistence::Temporary || read.nodes.len() > 8192 {
                self.notice = "Invalid temporary conversation snapshot.".into();
                return false;
            }
            ordered.extend(read.nodes.iter().filter_map(|node| node.message.clone()));
        } else {
            // Saved reads follow the authoritative selected branch. Unknown
            // side branches stay metadata and never enter the transcript.
            let nodes: BTreeMap<_, _> = read
                .nodes
                .iter()
                .map(|node| (node.id.as_str(), node))
                .collect();
            let mut id = read.current_node.as_deref();
            let mut seen = std::collections::BTreeSet::new();
            while let Some(current) = id {
                if !seen.insert(current) || seen.len() > 8192 {
                    self.notice = "Invalid conversation branch.".into();
                    return false;
                }
                let Some(node) = nodes.get(current) else {
                    self.notice = "Incomplete conversation branch.".into();
                    return false;
                };
                if let Some(message) = &node.message {
                    ordered.push(message.clone());
                }
                id = node.parent_id.as_deref();
            }
            ordered.reverse();
        }
        self.messages = ordered;
        self.title = read.title;
        // A refresh before Submit must not silently move the caller's expected
        // branch. Surface conflicts while keeping the draft available.
        let branch_conflict = self.submission.as_ref().is_some_and(|submission| {
            self.work == Work::Reading
                && self.turn.is_none()
                && read.current_node.as_deref() != Some(&submission.parent)
        });
        self.content.expected_parent_id = read.current_node;
        self.blocker = read.continuation_blocker;
        let message_id = self
            .submission
            .as_ref()
            .map(|s| s.message_id.as_str())
            .or_else(|| self.pending.as_ref().map(|p| p.client_message_id.as_str()));
        let confirmed = read.source != rpc::ChatConversationSource::LocalCache
            && message_id.is_some_and(|id| {
                self.messages
                    .iter()
                    .any(|message| message.id == id && message.role == "user")
            });
        let unresolved = self.work == Work::Unknown && !confirmed;
        if confirmed {
            // A recovered pending snapshot may differ from a concurrently edited
            // draft. Clear only the submitted version, retaining newer edits.
            let submitted = self
                .submission
                .as_ref()
                .map(|s| s.text.as_str())
                .or_else(|| self.pending.as_ref().map(|p| p.content.text.as_str()));
            if submitted == Some(self.composer.text()) {
                self.composer = TextArea::new();
                self.content.text.clear();
                self.content.selections.clear();
                self.clipboard.clear();
            }
        }
        if read.pending_submission.is_some() || !unresolved {
            self.pending = read.pending_submission;
        }
        self.work = if self.pending.is_some() || unresolved {
            Work::Unknown
        } else {
            Work::Idle
        };
        if self.pending.is_none() && !unresolved {
            self.submission = None;
        }
        if read.source == rpc::ChatConversationSource::LocalCache {
            self.blocker = Some(rpc::ChatContinuationBlocker::ReadOnly);
            self.notice = "Showing a local cache. Refresh from ChatGPT before continuing.".into();
        }
        if branch_conflict {
            self.notice =
                "The conversation changed. Review its latest response before submitting again."
                    .into();
        }
        self.edited();
        true
    }
}
