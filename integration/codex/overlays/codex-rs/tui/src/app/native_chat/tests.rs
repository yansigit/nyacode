use super::*;

#[test]
fn explicit_local_sockets_are_allowed_but_loopback_tcp_is_not() {
    use crate::AppServerTarget;
    use codex_app_server_client::RemoteAppServerEndpoint;
    use codex_utils_absolute_path::AbsolutePathBuf;
    let socket_path =
        AbsolutePathBuf::from_absolute_path_checked(std::env::temp_dir().join("native-chat.sock"))
            .unwrap();
    assert!(local_connection(&AppServerTarget::Embedded));
    assert!(local_connection(&AppServerTarget::Remote {
        endpoint: RemoteAppServerEndpoint::UnixSocket {
            socket_path: socket_path.clone()
        },
    }));
    assert!(local_connection(&AppServerTarget::LocalDaemon {
        endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
        allow_embedded_fallback: false,
    }));
    assert!(!local_connection(&AppServerTarget::Remote {
        endpoint: RemoteAppServerEndpoint::WebSocket {
            websocket_url: "ws://127.0.0.1:8000".into(),
            auth_token: None,
        },
    }));
}

#[test]
fn separate_drafts_keep_independent_text_and_scroll() {
    let mut chat = NativeChat::default();
    chat.new_session(rpc::ChatPersistence::Saved);
    chat.sessions[0]
        .composer
        .set_text_clearing_elements("!shell @file $skill stay literal");
    chat.sessions[0].edited();
    chat.sessions[0].scroll = 9;
    chat.new_session(rpc::ChatPersistence::Temporary);
    assert!(chat.sessions[1].composer.is_empty());
    assert_eq!(
        chat.sessions[0].content.text,
        "!shell @file $skill stay literal"
    );
    assert_eq!(chat.sessions[0].scroll, 9);
    assert_eq!(chat.sessions[1].scroll, 0);
}

#[test]
fn identity_change_drops_temporary_content_and_clipboard() {
    let mut chat = NativeChat::default();
    chat.new_session(rpc::ChatPersistence::Temporary);
    chat.sessions[0]
        .composer
        .set_text_clearing_elements("private");
    chat.sessions[0]
        .clipboard
        .insert("selection".into(), vec![1, 2, 3]);
    chat.clear_identity();
    assert!(chat.sessions.is_empty());
    assert!(chat.scope.is_none());
}

#[test]
fn disconnect_keeps_scope_generation_and_unsaved_drafts_until_resynchronization() {
    let mut chat = NativeChat::default();
    chat.scope = Some("owner-scope".into());
    chat.generation = 7;
    chat.new_session(rpc::ChatPersistence::Saved);
    chat.sessions[0]
        .composer
        .set_text_clearing_elements("unsaved edit");
    chat.sessions[0].edited();
    chat.disconnected();
    assert_eq!(chat.scope.as_deref(), Some("owner-scope"));
    assert_eq!(chat.generation, 7);
    assert!(chat.reconnect_status_pending);
    assert_eq!(chat.sessions[0].composer.text(), "unsaved edit");
    assert!(chat.sessions[0].dirty());
    assert_eq!(
        chat.sessions[0].notice,
        "Disconnected. Your draft is retained."
    );
}

#[test]
fn disconnect_replaces_streaming_notice_and_keeps_submission_identity() {
    let mut chat = NativeChat::default();
    chat.new_session(rpc::ChatPersistence::Saved);
    let session = &mut chat.sessions[0];
    session.composer.set_text_clearing_elements("submitted");
    assert!(session.submission());
    let id = session.submission.as_ref().unwrap().id.clone();
    session.work = Work::Streaming;
    session.notice = "Receiving response…".into();
    session.cancel_in_flight = true;
    chat.disconnected();
    let session = &chat.sessions[0];
    assert!(session.work == Work::Unknown);
    assert_eq!(session.submission.as_ref().unwrap().id, id);
    assert!(!session.cancel_in_flight);
    assert_eq!(
        session.notice,
        "Disconnected. Pending submissions will not be resent."
    );
}

#[test]
fn submission_freezes_draft_and_rejects_duplicate_submit() {
    let mut session = Session::new(rpc::ChatPersistence::Saved);
    session.composer.set_text_clearing_elements("hello");
    session.edited();
    assert!(session.submission());
    let id = session.submission.as_ref().unwrap().id.clone();
    assert!(!session.editable());
    assert!(!session.submission());
    assert_eq!(session.submission.as_ref().unwrap().id, id);
    assert_eq!(session.composer.text(), "hello");
}

fn turn(session: &Session, accepted: bool, pending: bool) -> rpc::ChatTurnSnapshot {
    let submission = session.submission.as_ref().unwrap();
    rpc::ChatTurnSnapshot {
        scope_id: "scope".into(),
        operation_id: "operation".into(),
        draft_id: session.id.clone(),
        submission_id: submission.id.clone(),
        client_message_id: submission.message_id.clone(),
        conversation_id: Some("conversation".into()),
        persistence: session.persistence,
        sequence: 1,
        phase: rpc::ChatTurnPhase::Finished,
        acceptance: if accepted {
            rpc::ChatAcceptance::Accepted
        } else {
            rpc::ChatAcceptance::Unknown
        },
        outcome: rpc::ChatGenerationOutcome::Failed,
        requested_model: None,
        resolved_model: None,
        messages: Vec::new(),
        error: Some(rpc::ChatErrorCode::Transport),
        recovery_pending: pending,
    }
}

fn conversation() -> rpc::ChatConversationReadResponse {
    rpc::ChatConversationReadResponse {
        scope_id: "scope".into(),
        conversation_id: "conversation".into(),
        persistence: rpc::ChatPersistence::Saved,
        source: rpc::ChatConversationSource::Authoritative,
        observed_at_ms: None,
        title: "Fixture".into(),
        updated_at_ms: None,
        current_node: None,
        nodes: Vec::new(),
        other_branch_heads: Vec::new(),
        default_model: None,
        continuation_blocker: None,
        pending_submission: None,
        recovery_available: true,
        reconciliation_error: None,
    }
}

#[test]
fn absent_authoritative_message_does_not_unlock_unknown_submission() {
    let mut session = Session::new(rpc::ChatPersistence::Saved);
    session.composer.set_text_clearing_elements("submitted");
    session.edited();
    assert!(session.submission());
    let snapshot = turn(&session, false, true);
    assert!(session.apply_turn(snapshot, true));
    assert!(session.apply_read(conversation()));
    assert!(session.submission.is_some());
    assert!(!session.editable());
    assert_eq!(session.composer.text(), "submitted");
}

#[test]
fn absent_pending_draft_record_does_not_unlock_unknown_submission() {
    let mut session = Session::new(rpc::ChatPersistence::Saved);
    session.composer.set_text_clearing_elements("submitted");
    assert!(session.submission());
    session.work = Work::Unknown;
    let message_id = session.submission.as_ref().unwrap().message_id.clone();
    let mut content = session.content.clone();
    content.text = "older stored draft".into();
    session.apply_draft(
        rpc::ChatDraft {
            id: session.id.clone(),
            revision: "stored-revision".into(),
            persistence: rpc::ChatPersistence::Saved,
            content,
            updated_at_ms: 0,
        },
        None,
    );
    assert!(session.work == Work::Unknown);
    assert!(!session.editable());
    assert_eq!(session.composer.text(), "submitted");
    assert_eq!(session.submission.as_ref().unwrap().message_id, message_id);
}

#[test]
fn refresh_cannot_unlock_an_active_temporary_submission() {
    for work in [
        Work::Saving,
        Work::Uploading,
        Work::Starting,
        Work::Streaming,
    ] {
        let mut session = Session::new(rpc::ChatPersistence::Temporary);
        session.composer.set_text_clearing_elements("submitted");
        assert!(session.submission());
        session.content.conversation_id = Some("conversation".into());
        session.work = work;
        let mut read = conversation();
        read.persistence = rpc::ChatPersistence::Temporary;
        assert!(!session.apply_read(read));
        assert!(session.work == work);
        assert!(session.submission.is_some());
        assert!(!session.editable());
    }
}

#[test]
fn editing_cancels_a_pending_close_until_the_new_draft_is_saved() {
    let mut session = Session::new(rpc::ChatPersistence::Saved);
    session.close_requested = true;
    session
        .composer
        .set_text_clearing_elements("keep this edit");
    session.edited();
    assert!(!session.close_requested);
    assert!(session.dirty());
}

#[test]
fn accepted_generation_failure_does_not_restore_submitted_text() {
    let mut session = Session::new(rpc::ChatPersistence::Saved);
    session.composer.set_text_clearing_elements("submitted");
    session.edited();
    assert!(session.submission());
    let snapshot = turn(&session, true, false);
    assert!(session.apply_turn(snapshot.clone(), false));
    assert!(session.composer.is_empty());
    assert!(session.unread);
    assert!(
        !session.apply_turn(snapshot, true),
        "duplicate notification must not advance state"
    );
}

#[test]
fn response_status_requires_confirmed_message_acceptance() {
    let mut session = Session::new(rpc::ChatPersistence::Temporary);
    session.composer.set_text_clearing_elements("submitted");
    assert!(session.submission());
    let mut snapshot = turn(&session, false, true);
    snapshot.phase = rpc::ChatTurnPhase::Sending;
    snapshot.error = None;
    session.apply_turn(snapshot.clone(), true);
    assert_eq!(session.notice, "Awaiting message acceptance…");
    snapshot.sequence += 1;
    snapshot.acceptance = rpc::ChatAcceptance::Accepted;
    session.apply_turn(snapshot, true);
    assert_eq!(session.notice, "Receiving response…");
}

#[test]
fn cancellation_notice_survives_late_reply_and_preserves_unknown_acceptance() {
    for accepted in [false, true] {
        let mut session = Session::new(rpc::ChatPersistence::Temporary);
        session.composer.set_text_clearing_elements("submitted");
        assert!(session.submission());
        session.stop_requested = true;
        let mut snapshot = turn(&session, accepted, true);
        snapshot.error = Some(rpc::ChatErrorCode::LocalCancellation);
        session.apply_turn(snapshot, true);
        let notice = session.notice.clone();
        session.apply_cancel_result(true, false);
        assert_eq!(session.notice, notice);
        assert!(
            session.notice.contains("server cancellation unconfirmed")
                || session
                    .notice
                    .contains("Server cancellation is not confirmed")
        );
        assert!(!session.editable());
        assert_eq!(session.composer.is_empty(), accepted);
        assert!(session.work == Work::Unknown);
        assert_eq!(session.notice.contains("Acceptance unresolved"), !accepted);
    }
}

#[test]
fn cancellation_acknowledgement_does_not_claim_local_completion() {
    let mut session = Session::new(rpc::ChatPersistence::Temporary);
    session.composer.set_text_clearing_elements("submitted");
    assert!(session.submission());
    let mut snapshot = turn(&session, true, false);
    snapshot.phase = rpc::ChatTurnPhase::Sending;
    session.apply_turn(snapshot, true);
    session.apply_cancel_result(true, false);
    assert_eq!(
        session.notice,
        "Local stop requested. Server cancellation is not confirmed."
    );
}

#[test]
fn authoritative_exact_user_id_reconciles_a_recovered_submission() {
    let mut session = Session::new(rpc::ChatPersistence::Saved);
    session.composer.set_text_clearing_elements("submitted");
    session.edited();
    assert!(session.submission());
    let message_id = session.submission.as_ref().unwrap().message_id.clone();
    let snapshot = turn(&session, false, true);
    session.apply_turn(snapshot, true);
    let mut read = conversation();
    read.current_node = Some(message_id.clone());
    read.nodes.push(rpc::ChatConversationNode {
        id: message_id.clone(),
        parent_id: None,
        message: Some(rpc::ChatMessage {
            id: message_id,
            role: "user".into(),
            parts: vec![rpc::ChatMessagePart::Text {
                text: "submitted".into(),
            }],
            model: None,
            finished: true,
            end_turn: true,
            attachments: Vec::new(),
            citations: Vec::new(),
        }),
    });
    assert!(session.apply_read(read));
    assert!(session.composer.is_empty());
    assert!(session.submission.is_none());
    assert!(session.editable());
}

#[test]
fn temporary_memory_refresh_keeps_prior_turns_without_inventing_parent_links() {
    let mut session = Session::new(rpc::ChatPersistence::Temporary);
    session.content.conversation_id = Some("conversation".into());
    let mut read = conversation();
    read.persistence = rpc::ChatPersistence::Temporary;
    read.source = rpc::ChatConversationSource::TemporaryMemory;
    read.current_node = Some("second-reply".into());
    for (id, role, parent) in [
        ("first-user", "user", Some("root")),
        ("first-reply", "assistant", None),
        ("second-user", "user", Some("first-reply")),
        ("second-reply", "assistant", None),
    ] {
        read.nodes.push(rpc::ChatConversationNode {
            id: id.into(),
            parent_id: parent.map(str::to_owned),
            message: Some(rpc::ChatMessage {
                id: id.into(),
                role: role.into(),
                parts: vec![rpc::ChatMessagePart::Text { text: id.into() }],
                model: None,
                finished: true,
                end_turn: true,
                attachments: Vec::new(),
                citations: Vec::new(),
            }),
        });
    }
    assert!(session.apply_read(read));
    assert_eq!(
        session
            .messages
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec!["first-user", "first-reply", "second-user", "second-reply"]
    );
    assert_eq!(
        session.content.expected_parent_id.as_deref(),
        Some("second-reply")
    );
}

#[test]
fn recovered_clipboard_requires_reattachment() {
    let mut session = Session::new(rpc::ChatPersistence::Saved);
    session.content.selections.push(rpc::ChatFileSelection {
        id: "lost".into(),
        source: None,
        name: "clipboard.png".into(),
        snapshot: None,
    });
    assert!(!session.submission());
    assert!(session.notice.contains("reattach") || session.notice.contains("attach it again"));
}

#[test]
fn narrow_render_and_metadata_do_not_emit_terminal_escape_sequences() {
    let mut chat = NativeChat::default();
    chat.new_session(rpc::ChatPersistence::Temporary);
    chat.sessions[0].title = "title\u{1b}[2J".into();
    chat.sessions[0]
        .composer
        .set_text_clearing_elements("literal ! @ $");
    for (width, height) in [(1, 1), (12, 4), (60, 14)] {
        let area = ratatui::layout::Rect::new(0, 0, width, height);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        chat.render(area, &mut buffer);
        assert!(
            buffer
                .content
                .iter()
                .all(|cell| !cell.symbol().contains('\u{1b}'))
        );
    }
}

#[test]
fn long_transcript_can_render_its_tail_beyond_u16_scroll() {
    let mut chat = NativeChat::default();
    chat.new_session(rpc::ChatPersistence::Temporary);
    chat.sessions[0].messages.push(rpc::ChatMessage {
        id: "fixture".into(),
        role: "assistant".into(),
        parts: vec![rpc::ChatMessagePart::Text {
            text: format!("{}TAIL-FIXTURE", "line\n".repeat(66_000)),
        }],
        model: None,
        finished: true,
        end_turn: true,
        attachments: Vec::new(),
        citations: Vec::new(),
    });
    let area = ratatui::layout::Rect::new(0, 0, 60, 14);
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    chat.render(area, &mut buffer);
    let rendered = buffer
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(rendered.contains("TAIL-FIXTURE"));
}

#[test]
fn native_shortcut_context_excludes_codex_tools_voice_and_history() {
    use crate::keymap::KeymapActionId;
    use crate::keymap::KeymapContext;
    use crate::keymap::KeymapContextSet;
    let context = KeymapContextSet::native_chat(false);
    for (context_id, action) in [
        (KeymapContext::Chat, "toggle_voice"),
        (KeymapContext::Composer, "history_search_previous"),
        (KeymapContext::Global, "open_external_editor"),
    ] {
        assert!(!context.contains_action(KeymapActionId {
            context: context_id,
            action
        }));
    }
    assert!(context.contains_action(KeymapActionId {
        context: KeymapContext::Global,
        action: "switch_surface"
    }));
}

#[test]
fn temporary_surface_with_codex_attention_snapshot() {
    use ratatui::widgets::Widget;
    let mut chat = NativeChat::default();
    chat.active = true;
    chat.new_session(rpc::ChatPersistence::Temporary);
    chat.sessions[0]
        .composer
        .set_text_clearing_elements("! @ $ stay literal");
    chat.sessions[0].notice = "Clipboard image selected; upload starts on Submit.".into();
    chat.sessions[0]
        .content
        .selections
        .push(rpc::ChatFileSelection {
            id: "fixture".into(),
            source: None,
            name: "clipboard.png".into(),
            snapshot: None,
        });
    let area = ratatui::layout::Rect::new(0, 0, 72, 12);
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    chat.header(true, true, "f6")
        .render(ratatui::layout::Rect::new(0, 0, 72, 1), &mut buffer);
    chat.render(ratatui::layout::Rect::new(0, 1, 72, 11), &mut buffer);
    let rendered = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("chat_temporary_surface", rendered);
}
