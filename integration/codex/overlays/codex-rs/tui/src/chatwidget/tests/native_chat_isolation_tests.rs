use super::*;

#[tokio::test]
async fn native_chat_messages_do_not_mutate_codex_widget_or_emit_events() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(None).await;
    while rx.try_recv().is_ok() {}
    while op_rx.try_recv().is_ok() {}
    chat.thread_usage.replaying_turn_completion = true;
    let before = render_bottom_popup(&chat, 80);
    let message = codex_app_server_protocol::ChatMessage {
        id: "fixture-message".into(),
        role: "assistant".into(),
        parts: vec![codex_app_server_protocol::ChatMessagePart::Text {
            text: "temporary Chat contents must stay isolated".into(),
        }],
        model: None,
        finished: true,
        end_turn: true,
        attachments: Vec::new(),
        citations: Vec::new(),
    };
    chat.handle_server_notification(
        ServerNotification::ChatMessage(codex_app_server_protocol::ChatMessageNotification {
            turn: codex_app_server_protocol::ChatTurnSnapshot {
                scope_id: "scope".into(),
                operation_id: "operation".into(),
                draft_id: "draft".into(),
                submission_id: "submission".into(),
                client_message_id: "client-message".into(),
                conversation_id: Some("conversation".into()),
                persistence: codex_app_server_protocol::ChatPersistence::Temporary,
                sequence: 1,
                phase: codex_app_server_protocol::ChatTurnPhase::Finished,
                acceptance: codex_app_server_protocol::ChatAcceptance::Accepted,
                outcome: codex_app_server_protocol::ChatGenerationOutcome::Completed,
                requested_model: None,
                resolved_model: None,
                messages: vec![message],
                error: None,
                recovery_pending: false,
            },
        }),
        None,
    );
    assert!(chat.thread_usage.replaying_turn_completion);
    assert_eq!(render_bottom_popup(&chat, 80), before);
    assert!(rx.try_recv().is_err());
    assert!(op_rx.try_recv().is_err());
}
