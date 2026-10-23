use super::*;
use serde_json::json;

fn fixture() -> Value {
    json!({
        "conversation_id":"fixture-conversation", "title":"Fixture title", "update_time":1_700_000_000.25,
        "current_node":"assistant", "default_model_slug":"fixture-model", "is_read_only":false,
        "mapping": {
            "sibling":{"parent":"user","message":{"id":"sibling","author":{"role":"assistant"},"content":{"content_type":"text","parts":["inactive sibling"]},"status":"finished_successfully","end_turn":true}},
            "assistant":{"parent":"user","message":{"id":"assistant","author":{"role":"assistant"},"content":{"content_type":"text","parts":["active response"]},"status":"finished_successfully","end_turn":true,"metadata":{"model_slug":"resolved-model"}}},
            "root":{"parent":null,"message":null},
            "user":{"parent":"system","message":{"id":"user","author":{"role":"user"},"content":{"content_type":"text","parts":["literal ! @ $ text"]}}},
            "system":{"parent":"root","message":{"id":"system","author":{"role":"system"},"content":{"content_type":"text","parts":["hidden system fixture"]}}}
        }
    })
}

#[test]
fn authoritative_parent_chain_ignores_mapping_order_and_hides_system_content() {
    let snapshot = ConversationSnapshot::parse("fixture-conversation", &fixture()).unwrap();
    assert_eq!(
        snapshot
            .branch
            .nodes
            .iter()
            .map(|n| n.id.as_str())
            .collect::<Vec<_>>(),
        ["root", "system", "user", "assistant"]
    );
    assert!(snapshot.branch.nodes[1].message.is_none());
    assert_eq!(
        snapshot.branch.nodes[2].message.as_ref().unwrap().text(),
        "literal ! @ $ text"
    );
    assert_eq!(snapshot.other_branch_heads, ["sibling"]);
    assert_eq!(snapshot.default_model.as_deref(), Some("fixture-model"));
    assert_eq!(
        snapshot.branch.nodes[3]
            .message
            .as_ref()
            .unwrap()
            .model
            .as_deref(),
        Some("resolved-model")
    );
    assert_eq!(snapshot.updated_at_ms, Some(1_700_000_000_250));
    assert_eq!(snapshot.check_continuation("assistant"), Ok(()));
    assert_eq!(
        snapshot.check_continuation("sibling"),
        Err(ChatError::BranchConflict)
    );
    let serialized = serde_json::to_string(&snapshot).unwrap();
    assert!(!serialized.contains("inactive sibling"));
    assert!(!serialized.contains("hidden system fixture"));
}

#[test]
fn missing_nodes_cycles_duplicate_messages_and_identity_changes_fail_closed() {
    for variant in 0..8 {
        let mut value = fixture();
        match variant {
            0 => value["current_node"] = json!("absent"),
            1 => value["mapping"]["system"]["parent"] = json!("assistant"),
            2 => value["mapping"]["sibling"]["parent"] = json!("sibling"),
            3 => value["mapping"]["sibling"]["parent"] = json!("missing"),
            4 => value["mapping"]["sibling"]["message"]["id"] = json!("assistant"),
            5 => value["conversation_id"] = json!("different"),
            6 => value["mapping"]["assistant"]["id"] = json!("different"),
            _ => value["current_node"] = Value::Null,
        }
        assert!(matches!(
            ConversationSnapshot::parse("fixture-conversation", &value),
            Err(ChatError::ProtocolChanged)
        ));
    }
}

#[test]
fn custom_read_only_temporary_and_unknown_modes_explain_continuation_limits() {
    for (field, value, reason) in [
        (
            "gizmo_id",
            json!("g-fixture"),
            ContinuationBlocker::CustomGpt,
        ),
        ("is_read_only", json!(true), ContinuationBlocker::ReadOnly),
        (
            "is_temporary_chat",
            json!(true),
            ContinuationBlocker::Temporary,
        ),
        (
            "is_do_not_remember",
            json!(true),
            ContinuationBlocker::Temporary,
        ),
        (
            "conversation_mode",
            json!({"kind":"unknown_mode"}),
            ContinuationBlocker::UnsupportedMode,
        ),
        (
            "conversation_template_id",
            json!("template-fixture"),
            ContinuationBlocker::UnsupportedMode,
        ),
    ] {
        let mut input = fixture();
        input[field] = value;
        let snapshot = ConversationSnapshot::parse("fixture-conversation", &input).unwrap();
        assert_eq!(snapshot.continuation_blocker, Some(reason));
        assert_eq!(
            snapshot.check_continuation("assistant"),
            Err(ChatError::UnsupportedCapability)
        );
    }
}

#[test]
fn incomplete_or_unknown_leaf_does_not_claim_supported_continuation() {
    for variant in 0..4 {
        let mut value = fixture();
        let reason = match variant {
            0 => {
                value["current_node"] = json!("user");
                ContinuationBlocker::IncompleteGeneration
            }
            1 => {
                value["mapping"]["assistant"]["message"]["end_turn"] = json!(false);
                ContinuationBlocker::IncompleteGeneration
            }
            2 => {
                value["mapping"]["assistant"]["message"]["content"] =
                    json!({"content_type":"unknown","payload":{"private":"not retained"}});
                ContinuationBlocker::UnsupportedContent
            }
            _ => {
                value["mapping"]["assistant"]["message"]["metadata"]["is_visually_hidden_from_conversation"] =
                    json!(true);
                ContinuationBlocker::UnsupportedContent
            }
        };
        let snapshot = ConversationSnapshot::parse("fixture-conversation", &value).unwrap();
        assert_eq!(snapshot.continuation_blocker, Some(reason));
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("not retained")
        );
    }
}

#[test]
fn history_images_and_attachments_are_metadata_only() {
    let mut value = fixture();
    value["mapping"]["user"]["message"]["content"] = json!({"content_type":"multimodal_text","parts":["explain this image",{"content_type":"image_asset_pointer","asset_pointer":"file-service://file-fixture","width":10,"height":20,"download_url":"https://storage.invalid/?sig=secret"}]});
    value["mapping"]["user"]["message"]["metadata"] = json!({"attachments":[{"id":"file-fixture","name":"fixture.png","size":42,"mime_type":"image/png","download_url":"https://storage.invalid/?sig=secret"}],"secret":"discarded"});
    let snapshot = ConversationSnapshot::parse("fixture-conversation", &value).unwrap();
    let user = snapshot.branch.nodes[2].message.as_ref().unwrap();
    assert_eq!(user.text(), "explain this image");
    assert!(matches!(user.parts[1], MessagePart::Image(_)));
    assert_eq!(user.attachments[0].name, "fixture.png");
    let serialized = serde_json::to_string(&snapshot).unwrap();
    for unexpected in ["secret", "discarded", "https://", "download_url"] {
        assert!(!serialized.contains(unexpected));
    }
}

#[test]
fn oversized_graph_or_accumulated_message_content_is_rejected() {
    let mut value = fixture();
    value["mapping"]["assistant"]["message"]["content"]["parts"] = json!(["x".repeat(MAX_CONTENT)]);
    assert!(matches!(
        ConversationSnapshot::parse("fixture-conversation", &value),
        Err(ChatError::ResponseTooLarge)
    ));
    value = fixture();
    for i in 0..MAX_NODES {
        value["mapping"][format!("node-{i}")] = json!({"parent":null,"message":null});
    }
    assert!(matches!(
        ConversationSnapshot::parse("fixture-conversation", &value),
        Err(ChatError::ResponseTooLarge)
    ));
}

#[test]
fn empty_conversation_has_no_invented_parent_or_transcript() {
    let value = json!({"current_node":null,"mapping":{}});
    let snapshot = ConversationSnapshot::parse("fixture-conversation", &value).unwrap();
    assert!(snapshot.branch.current_node.is_none());
    assert!(snapshot.branch.nodes.is_empty());
    assert_eq!(
        snapshot.continuation_blocker,
        Some(ContinuationBlocker::EmptyConversation)
    );
}

#[test]
fn native_history_pagination_preserves_offset_and_total() {
    let value = json!({"offset":20,"total":24,"items":[
        {"id":"one","title":"First","update_time":"2023-11-14T22:13:20.250Z","gizmo_id":null},
        {"id":"two","title":"Second","is_archived":true,"gizmo_id":"g-fixture"}
    ]});
    let page = ConversationPage::parse(&value, 20, 2).unwrap();
    assert_eq!(page.next_offset, Some(22));
    assert_eq!(page.total, Some(24));
    assert_eq!(page.items[0].updated_at_ms, Some(1_700_000_000_250));
    assert!(page.items[1].archived);
    assert!(page.items[1].custom_gpt);
    assert!(ConversationPage::parse(&value, 0, 2).is_err());
    assert!(ConversationPage::parse(&value, 20, 1).is_err());
    let mut last = value.clone();
    last["total"] = json!(22);
    assert!(
        ConversationPage::parse(&last, 20, 2)
            .unwrap()
            .next_offset
            .is_none()
    );
    let mut duplicate = value;
    duplicate["items"][1]["id"] = json!("one");
    assert!(ConversationPage::parse(&duplicate, 20, 2).is_err());
}

#[test]
fn temporary_summaries_never_enter_saved_history_results() {
    for field in ["is_temporary_chat", "is_do_not_remember"] {
        let mut value = json!({"items":[{"id":"fixture","title":"private temporary title"}]});
        value["items"][0][field] = json!(true);
        assert!(matches!(
            ConversationPage::parse(&value, 0, 20),
            Err(ChatError::ProtocolChanged)
        ));
    }
}

#[test]
fn malformed_optional_metadata_is_not_coerced_into_safe_defaults() {
    for (field, value) in [
        ("is_read_only", json!("false")),
        ("title", json!({"unknown":"object"})),
        ("update_time", json!(-1)),
        ("update_time", json!("invalid")),
        ("gizmo_id", json!("../gizmo")),
    ] {
        let mut input = fixture();
        input[field] = value;
        assert!(matches!(
            ConversationSnapshot::parse("fixture-conversation", &input),
            Err(ChatError::ProtocolChanged)
        ));
    }
}

#[test]
fn internal_context_is_hidden_even_without_the_metadata_flag() {
    for kind in ["model_editable_context", "user_editable_context"] {
        let mut value = fixture();
        value["mapping"]["assistant"]["message"]["content"] =
            json!({"content_type":kind,"parts":["internal context fixture"]});
        let snapshot = ConversationSnapshot::parse("fixture-conversation", &value).unwrap();
        assert!(snapshot.branch.nodes.last().unwrap().message.is_none());
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("internal context fixture")
        );
    }
}
