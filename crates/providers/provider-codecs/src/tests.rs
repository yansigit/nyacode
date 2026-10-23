//! Synthetic regression fixtures reflecting source-observed wire shapes. These are
//! deterministic interoperability tests, not a record of authenticated live traffic.
use crate::*;
use codex_provider_api::*;
use codex_provider_runtime::drive_session;
use serde_json::json;
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

fn request(provider: &str) -> Request {
    Request {
        namespace: Namespace {
            provider: provider.into(),
            account: "test-account".into(),
            model: "test-model".into(),
            thread: "test-thread".into(),
        },
        system: "test system".into(),
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "test prompt".into(),
            }],
        }],
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
        }],
        parallel_tool_calls: true,
        max_output_tokens: Some(8192),
        reasoning_effort: None,
        output_schema: None,
        checkpoint: None,
        metadata: json!({}),
    }
}
fn context() -> Context {
    Context {
        credentials: Credentials {
            access_token: "fixture-secret".into(),
            account_id: "test-account".into(),
            metadata: json!({}),
        },
        cancellation: CancellationToken::new(),
    }
}
fn feed(codec: &mut dyn WireCodec, value: serde_json::Value) -> Vec<Event> {
    codec.frame("", &value.to_string()).unwrap()
}

#[test]
fn responses_preserves_optional_tool_arguments() {
    let mut req = request("opencode-go");
    let parameters = json!({
        "type": "object",
        "properties": {
            "message": {"type": "string"},
            "model_provider": {"type": "string"},
            "reasoning_effort": {"type": "string"}
        },
        "required": ["message"],
        "additionalProperties": false
    });
    req.tools[0].name = "spawn_agent".into();
    req.tools[0].parameters = parameters.clone();
    let codec = ResponsesCodec::new(
        "https://example.invalid".into(),
        BTreeMap::new(),
        &req.tools,
    );
    let wire = codec.request(&req, &context()).unwrap();
    assert_eq!(wire.body["tools"][0]["strict"], false);
    assert_eq!(wire.body["tools"][0]["parameters"], parameters);
}

#[test]
fn responses_refuses_unexpected_encrypted_tool_arguments() {
    let req = request("opencode-go");
    let mut codec = ResponsesCodec::new("x".into(), BTreeMap::new(), &req.tools);
    feed(
        &mut codec,
        json!({"type":"response.completed","response":{"id":"fixture","output":[{"type":"function_call","call_id":"call-1","name":"read","arguments":"{}","encrypted_function_args":["message"]}]}}),
    );
    assert!(matches!(
        codec.assistant(&req.namespace),
        Err(ProviderError::Unsupported(_))
    ));
}

#[test]
fn google_expands_local_schema_references_without_losing_constraints_or_literal_data() {
    let mut req = request("antigravity");
    req.tools[0].parameters = json!({"type":"object","properties":{
        "first":{"$ref":"#/$defs/Label"}, "second":{"$ref":"#/definitions/Alias"},
        "$ref":{"type":"string","enum":["literal"]}
    },"required":["first"],"additionalProperties":false,
    "$defs":{"Label":{"type":"string","enum":["a","b"]}},
    "definitions":{"Alias":{"$ref":"#/$defs/Label","description":"alias"}}});
    let codec = GoogleCodec::new("x".into(), BTreeMap::new(), "project".into(), &req.tools);
    let body = codec.request(&req, &context()).unwrap().body;
    let parameters = &body["request"]["tools"][0]["functionDeclarations"][0]["parameters"];
    assert_eq!(
        parameters["properties"]["first"],
        json!({"type":"string","enum":["a","b"]})
    );
    assert_eq!(
        parameters["properties"]["second"],
        json!({"type":"string","enum":["a","b"],"description":"alias"})
    );
    assert_eq!(parameters["properties"]["$ref"]["enum"], json!(["literal"]));
    assert_eq!(parameters["required"], json!(["first"]));
    assert!(parameters.get("$defs").is_none());
    assert!(parameters.get("definitions").is_none());
}

#[test]
fn google_rejects_unresolved_remote_recursive_conflicting_and_excessive_references() {
    for schema in [
        json!({"$ref":"https://example.invalid/schema"}),
        json!({"$ref":"#/$defs/Missing"}),
        json!({"$id":"https://example.invalid/schema","$ref":"#/$defs/Label","$defs":{"Label":{"type":"string"}}}),
        json!({"$ref":"#/$defs/Node","$defs":{"Node":{"type":"object","properties":{"next":{"$ref":"#/$defs/Node"}}}}}),
        json!({"$ref":"#/$defs/Label","type":"integer","$defs":{"Label":{"type":"string"}}}),
        json!({"type":"object","properties":{"a":{"$ref":"#/$defs/Large"},"b":{"$ref":"#/$defs/Large"}},"$defs":{"Large":{"type":"string","description":"x".repeat(300_000)}}}),
    ] {
        let mut req = request("antigravity");
        req.tools[0].parameters = schema;
        let codec = GoogleCodec::new("x".into(), BTreeMap::new(), "project".into(), &req.tools);
        assert!(matches!(
            codec.request(&req, &context()),
            Err(ProviderError::Unsupported(_))
        ));
    }
}

#[test]
fn messages_preserves_thinking_signature_and_restores_tool_name() {
    let req = request("claude");
    let ctx = context();
    let mut codec = MessagesCodec::new(
        "https://example.invalid/v1/messages".into(),
        BTreeMap::new(),
        true,
        &req.tools,
    )
    .unwrap();
    let wire = codec.request(&req, &ctx).unwrap();
    assert_eq!(wire.body["tools"][0]["name"], "Read");
    assert_eq!(wire.headers["authorization"], "Bearer fixture-secret");
    feed(
        &mut codec,
        json!({"type":"message_start","message":{"id":"msg-test","usage":{"input_tokens":10,"cache_read_input_tokens":3}}}),
    );
    feed(
        &mut codec,
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
    );
    feed(
        &mut codec,
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}),
    );
    feed(
        &mut codec,
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed-"}}),
    );
    feed(
        &mut codec,
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"thought"}}),
    );
    feed(&mut codec, json!({"type":"content_block_stop","index":0}));
    feed(
        &mut codec,
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tool-1","name":"Read","input":{}}}),
    );
    feed(
        &mut codec,
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
    );
    feed(
        &mut codec,
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}),
    );
    feed(&mut codec, json!({"type":"content_block_stop","index":1}));
    let events = feed(
        &mut codec,
        json!({"type":"message_delta","usage":{"output_tokens":12}}),
    );
    assert!(matches!(
        &events[0],
        Event::Usage(Usage {
            input_tokens: 10,
            output_tokens: 12,
            cached_input_tokens: 3,
            ..
        })
    ));
    feed(&mut codec, json!({"type":"message_stop"}));
    let assistant = codec.assistant(&req.namespace).unwrap();
    assert!(
        assistant
            .content
            .iter()
            .any(|block| matches!(block,Content::ToolCall(ToolCall {name,..}) if name=="read"))
    );
    let mut continued = req;
    continued.messages.push(assistant);
    let continued_wire = codec.request(&continued, &ctx).unwrap();
    assert_eq!(
        continued_wire.body["messages"][1]["content"][0]["signature"],
        "signed-thought"
    );
    assert_eq!(
        continued_wire.body["messages"][1]["content"][1]["input"],
        json!({"path":"a"})
    );
}
#[test]
fn messages_rejects_unsigned_incomplete_tool_json() {
    let req = request("claude");
    let mut codec = MessagesCodec::new("x".into(), BTreeMap::new(), true, &req.tools).unwrap();
    feed(
        &mut codec,
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"1","name":"Read","input":{}}}),
    );
    feed(
        &mut codec,
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{"}}),
    );
    assert!(
        codec
            .frame(
                "",
                &json!({"type":"content_block_stop","index":0}).to_string()
            )
            .is_err()
    );
}
#[test]
fn chat_accumulates_fragmented_tool_arguments_and_reasoning_usage() {
    let req = request("opencode-go");
    let mut codec = ChatCodec::new("x".into(), BTreeMap::new(), true, &req.tools);
    feed(
        &mut codec,
        json!({"id":"chat-1","choices":[{"index":0,"delta":{"reasoning_content":"think","tool_calls":[{"index":0,"id":"call-1","function":{"name":"read","arguments":"{\"path\":"}}]}}]}),
    );
    feed(
        &mut codec,
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]},"finish_reason":"tool_calls"}]}),
    );
    let events = feed(
        &mut codec,
        json!({"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":8,"completion_tokens_details":{"reasoning_tokens":7}}}),
    );
    assert!(matches!(
        events[0],
        Event::Usage(Usage {
            reasoning_tokens: 7,
            ..
        })
    ));
    codec.frame("", "[DONE]").unwrap();
    let assistant = codec.assistant(&req.namespace).unwrap();
    assert!(assistant.content.iter().any(|block|matches!(block,Content::ToolCall(ToolCall {arguments,..}) if arguments==&json!({"path":"a"}))));
    let mut continued = req;
    continued.messages.push(assistant);
    assert_eq!(
        codec.request(&continued, &context()).unwrap().body["messages"][2]["reasoning_content"],
        "think"
    );
}
#[test]
fn google_replays_exact_signed_function_part() {
    let req = request("antigravity");
    let mut codec = GoogleCodec::new("x".into(), BTreeMap::new(), "project".into(), &req.tools);
    let signed = json!({"thoughtSignature":"actual-upstream-signature","functionCall":{"name":"read","args":{"path":"a"}}});
    feed(
        &mut codec,
        json!({"response":{"candidates":[{"content":{"role":"model","parts":[signed]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":6}}}),
    );
    let assistant = codec.assistant(&req.namespace).unwrap();
    let call = assistant
        .content
        .iter()
        .find_map(|block| match block {
            Content::ToolCall(call) => Some(call),
            _ => None,
        })
        .unwrap()
        .clone();
    let mut continued = req;
    continued.messages.push(assistant);
    continued.messages.push(Message {
        role: Role::Tool,
        content: vec![Content::ToolResult(ToolResult {
            call_id: call.id,
            output: json!("file contents"),
            is_error: false,
        })],
    });
    let wire = codec.request(&continued, &context()).unwrap();
    assert_eq!(wire.body["request"]["contents"][1]["parts"][0], signed);
    assert_eq!(wire.body["request"]["sessionId"], "test-thread");
    assert_eq!(
        wire.body["request"]["contents"][2]["parts"][0]["functionResponse"]["name"],
        "read"
    );
    assert!(wire.body.get("sessionId").is_none());
}
#[test]
fn responses_replays_encrypted_reasoning_and_tool_call_ids() {
    let req = request("opencode-go");
    let mut codec = ResponsesCodec::new("x".into(), BTreeMap::new(), &req.tools);
    let output = json!([{"type":"reasoning","id":"reason-1","summary":[],"encrypted_content":"opaque-signed-data"},{"type":"function_call","id":"fc-1","call_id":"call-1","name":"read","arguments":"{\"path\":\"a\"}"}]);
    feed(
        &mut codec,
        json!({"type":"response.completed","response":{"id":"response-1","output":output}}),
    );
    let mut continued = req.clone();
    continued
        .messages
        .push(codec.assistant(&req.namespace).unwrap());
    let wire = codec.request(&continued, &context()).unwrap();
    assert_eq!(
        wire.body["input"][1]["encrypted_content"],
        "opaque-signed-data"
    );
    assert_eq!(wire.body["input"][2]["call_id"], "call-1");
}
#[test]
fn checkpoint_cumulative_replay_rejects_namespace_and_history_changes() {
    let mut req = request("claude");
    let first = Message {
        role: Role::Assistant,
        content: vec![
            Content::Text {
                text: "first".into(),
            },
            Content::Opaque {
                namespace: req.namespace.clone(),
                value: json!({"anthropic_blocks":[{"type":"text","text":"first"}]}),
            },
        ],
    };
    req.messages.push(first.clone());
    let second = Message {
        role: Role::Assistant,
        content: vec![
            Content::Text {
                text: "second".into(),
            },
            Content::Opaque {
                namespace: req.namespace.clone(),
                value: json!({"anthropic_blocks":[{"type":"text","text":"second"}]}),
            },
        ],
    };
    let checkpoint = crate::replay::checkpoint(&req, &second).unwrap();
    let mut resumed = req.clone();
    resumed.messages[1].content.pop();
    resumed.messages.push(Message {
        role: Role::Assistant,
        content: vec![Content::Text {
            text: "second".into(),
        }],
    });
    resumed.checkpoint = Some(checkpoint.clone());
    restore_checkpoint(&mut resumed).unwrap();
    assert_eq!(resumed.messages[1], first);
    assert_eq!(resumed.messages[2], second);
    let mut changed = req.clone();
    changed.checkpoint = Some(checkpoint.clone());
    changed.namespace.account = "other".into();
    assert!(restore_checkpoint(&mut changed).is_err());
    let mut changed = req;
    changed.checkpoint = Some(checkpoint);
    changed.system = "edited".into();
    assert!(restore_checkpoint(&mut changed).is_err());
}

#[tokio::test]
async fn native_http_session_continues_tool_round_and_checkpoints_before_execution() {
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::body_partial_json;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    let server = MockServer::start().await;
    let first = concat!(
        "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"id\":\"terminal-fixture\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
    );
    Mock::given(method("POST")).and(path("/chat")).and(body_partial_json(json!({"messages":[{"role":"system","content":"test system"},{"role":"user","content":[{"type":"text","text":"test prompt"}]}]}))).respond_with(ResponseTemplate::new(200).set_body_string(first).insert_header("content-type","text/event-stream")).expect(1).mount(&server).await;
    let second = "data: {\"id\":\"r2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat"))
        .and(wiremock::matchers::body_string_contains("tool_call_id"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(second)
                .insert_header("content-type", "text/event-stream"),
        )
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    let req = request("opencode-go");
    let codec = ChatCodec::new(
        format!("{}/chat", server.uri()),
        BTreeMap::new(),
        false,
        &req.tools,
    );
    let mut session = HttpSession::start(reqwest::Client::new(), Box::new(codec), req, context())
        .await
        .unwrap();
    assert!(matches!(
        session.next_event().await.unwrap(),
        Some(Event::Checkpoint(_))
    ));
    let Some(Event::ToolCall(call)) = session.next_event().await.unwrap() else {
        panic!("expected tool call");
    };
    session
        .submit_tool_result(ToolResult {
            call_id: call.id,
            output: json!("contents"),
            is_error: false,
        })
        .await
        .unwrap();
    assert_eq!(
        session.next_event().await.unwrap(),
        Some(Event::ContinuationReady)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    session.continue_round().await.unwrap();
    assert_eq!(
        session.next_event().await.unwrap(),
        Some(Event::TextDelta("done".into()))
    );
    assert!(matches!(
        session.next_event().await.unwrap(),
        Some(Event::Checkpoint(_))
    ));
    assert_eq!(
        session.next_event().await.unwrap(),
        Some(Event::Completed { id: "r2".into() })
    );
    assert_eq!(session.next_event().await.unwrap(), None);
    server.verify().await;
}
#[tokio::test]
async fn cancelled_request_never_contacts_server() {
    let server = wiremock::MockServer::start().await;
    let req = request("opencode-go");
    let ctx = context();
    ctx.cancellation.cancel();
    let codec = ChatCodec::new(
        format!("{}/chat", server.uri()),
        BTreeMap::new(),
        false,
        &req.tools,
    );
    assert!(matches!(
        HttpSession::start(reqwest::Client::new(), Box::new(codec), req, ctx).await,
        Err(ProviderError::Cancelled)
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn clean_http_eof_without_terminal_is_protocol_failure() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(
            "data: {\"id\":\"r\",\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
        ))
        .mount(&server)
        .await;
    let req = request("opencode-go");
    let codec = ChatCodec::new(server.uri(), BTreeMap::new(), false, &req.tools);
    let mut session = HttpSession::start(reqwest::Client::new(), Box::new(codec), req, context())
        .await
        .unwrap();
    assert_eq!(
        session.next_event().await.unwrap(),
        Some(Event::TextDelta("partial".into()))
    );
    assert!(matches!(
        session.next_event().await,
        Err(ProviderError::Protocol(_))
    ));
}

#[test]
fn checkpoint_normalizes_text_fragments_and_allows_tool_catalog_growth() {
    let mut req = request("antigravity");
    let assistant = Message {
        role: Role::Assistant,
        content: vec![
            Content::Text {
                text: "part ".into(),
            },
            Content::Text { text: "two".into() },
            Content::Opaque {
                namespace: req.namespace.clone(),
                value: json!({"google_parts":[{"text":"part "},{"text":"two"}]}),
            },
        ],
    };
    let checkpoint = crate::replay::checkpoint(&req, &assistant).unwrap();
    req.messages.push(Message {
        role: Role::Assistant,
        content: vec![Content::Text {
            text: "part two".into(),
        }],
    });
    req.tools.push(ToolDefinition {
        name: "other".into(),
        description: "added tool".into(),
        parameters: json!({"type":"object"}),
    });
    req.checkpoint = Some(checkpoint);
    restore_checkpoint(&mut req).unwrap();
    assert_eq!(req.messages[1], assistant);
}
#[test]
fn checkpoint_restores_signed_empty_blocks_when_host_omits_public_empty_fragments() {
    let mut req = request("claude");
    let call = ToolCall {
        id: "c".into(),
        name: "read".into(),
        arguments: json!({"path":"a"}),
    };
    let assistant = Message {
        role: Role::Assistant,
        content: vec![
            Content::Thinking {
                text: String::new(),
                signature: Some("exact-signature".into()),
            },
            Content::Text {
                text: String::new(),
            },
            Content::ToolCall(call.clone()),
            Content::Opaque {
                namespace: req.namespace.clone(),
                value: json!({"anthropic_blocks":[{"type":"thinking","thinking":"","signature":"exact-signature"},{"type":"tool_use","id":"c","name":"Read","input":{"path":"a"}}]}),
            },
        ],
    };
    req.checkpoint = Some(crate::replay::checkpoint(&req, &assistant).unwrap());
    req.messages.push(Message {
        role: Role::Assistant,
        content: vec![Content::ToolCall(call)],
    });
    restore_checkpoint(&mut req).unwrap();
    assert_eq!(req.messages[1], assistant);
}
#[test]
fn checkpoint_accepts_parallel_tool_completion_order_but_rejects_changed_results() {
    let mut req = request("claude");
    let results = vec![
        Content::ToolResult(ToolResult {
            call_id: "z".into(),
            output: json!("first"),
            is_error: false,
        }),
        Content::ToolResult(ToolResult {
            call_id: "a".into(),
            output: json!("second"),
            is_error: false,
        }),
    ];
    req.messages.push(Message {
        role: Role::Tool,
        content: results,
    });
    let assistant = Message {
        role: Role::Assistant,
        content: vec![Content::Text {
            text: "done".into(),
        }],
    };
    req.checkpoint = Some(crate::replay::checkpoint(&req, &assistant).unwrap());
    req.messages[1].content.reverse();
    req.messages.push(assistant.clone());
    let mut changed = req.clone();
    restore_checkpoint(&mut req).unwrap();
    assert_eq!(req.messages[2], assistant);
    if let Content::ToolResult(result) = &mut changed.messages[1].content[0] {
        result.output = json!("changed");
    }
    assert!(restore_checkpoint(&mut changed).is_err());
}
#[test]
fn host_verified_prefix_restores_native_history_after_interleaved_results() {
    let mut native = request("claude");
    let first = ToolCall {
        id: "first".into(),
        name: "read".into(),
        arguments: json!({"path":"a"}),
    };
    let second = ToolCall {
        id: "second".into(),
        name: "read".into(),
        arguments: json!({"path":"b"}),
    };
    let first_result = ToolResult {
        call_id: "first".into(),
        output: json!("a"),
        is_error: false,
    };
    let second_result = ToolResult {
        call_id: "second".into(),
        output: json!("b"),
        is_error: false,
    };
    native.messages.push(Message { role: Role::Assistant, content: vec![Content::ToolCall(first.clone()), Content::ToolCall(second.clone()), Content::Opaque { namespace: native.namespace.clone(), value: json!({"anthropic_blocks":[{"type":"tool_use","id":"first","name":"Read","input":{"path":"a"}},{"type":"tool_use","id":"second","name":"Read","input":{"path":"b"}}]}) }] });
    native.messages.push(Message {
        role: Role::Tool,
        content: vec![
            Content::ToolResult(first_result.clone()),
            Content::ToolResult(second_result.clone()),
        ],
    });
    let final_message = Message {
        role: Role::Assistant,
        content: vec![Content::Text {
            text: "done".into(),
        }],
    };
    let checkpoint = crate::replay::checkpoint(&native, &final_message).unwrap();
    let mut host = request("claude");
    host.messages.extend([
        Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall(first)],
        },
        Message {
            role: Role::Tool,
            content: vec![Content::ToolResult(first_result)],
        },
        Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall(second)],
        },
        Message {
            role: Role::Tool,
            content: vec![Content::ToolResult(second_result)],
        },
        final_message.clone(),
    ]);
    host.checkpoint = Some(checkpoint);
    let host_prefix = host.messages.len();
    let next_user = Message {
        role: Role::User,
        content: vec![Content::Text {
            text: "next turn".into(),
        }],
    };
    host.messages.push(next_user.clone());
    assert!(restore_checkpoint(&mut host.clone()).is_err());
    host.metadata["host_checkpoint_prefix"] = json!(host_prefix);
    let mut corrupt = host.clone();
    corrupt.checkpoint.as_mut().unwrap().state["canonical_history"][0]["content"][0]["text"] =
        json!("changed");
    assert!(restore_checkpoint(&mut corrupt).is_err());
    restore_checkpoint(&mut host).unwrap();
    native.messages.extend([final_message, next_user]);
    assert_eq!(host.messages, native.messages);
}
#[test]
fn chat_assistant_history_uses_plain_text_and_deepseek_thinking_enablement() {
    let mut req = request("opencode-go");
    req.reasoning_effort = Some("high".into());
    req.messages.push(Message {
        role: Role::Assistant,
        content: vec![
            Content::Text { text: "a".into() },
            Content::Text { text: "b".into() },
        ],
    });
    let codec = ChatCodec::new("x".into(), BTreeMap::new(), true, &req.tools);
    let wire = codec.request(&req, &context()).unwrap();
    assert_eq!(wire.body["messages"][2]["content"], "ab");
    assert_eq!(wire.body["thinking"]["type"], "enabled");
}
#[tokio::test]
async fn parallel_tool_results_preserve_original_call_order_and_wait_for_full_batch() {
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::body_string_contains;
    use wiremock::matchers::method;
    let server = MockServer::start().await;
    let first = "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"z-call\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a\\\"}\"}},{\"index\":1,\"id\":\"a-call\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"b\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(first))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(body_string_contains("tool_call_id")).respond_with(ResponseTemplate::new(200).set_body_string("data: {\"id\":\"r2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")).with_priority(1).expect(1).mount(&server).await;
    let req = request("opencode-go");
    let codec = ChatCodec::new(server.uri(), BTreeMap::new(), false, &req.tools);
    let mut session = HttpSession::start(reqwest::Client::new(), Box::new(codec), req, context())
        .await
        .unwrap();
    assert!(matches!(
        session.next_event().await.unwrap(),
        Some(Event::Checkpoint(_))
    ));
    assert!(
        matches!(session.next_event().await.unwrap(),Some(Event::ToolCall(ToolCall {id,..})) if id=="z-call")
    );
    assert!(
        matches!(session.next_event().await.unwrap(),Some(Event::ToolCall(ToolCall {id,..})) if id=="a-call")
    );
    session
        .submit_tool_result(ToolResult {
            call_id: "a-call".into(),
            output: json!("second"),
            is_error: false,
        })
        .await
        .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), session.next_event())
            .await
            .is_err()
    );
    session
        .submit_tool_result(ToolResult {
            call_id: "z-call".into(),
            output: json!("first"),
            is_error: false,
        })
        .await
        .unwrap();
    assert_eq!(
        session.next_event().await.unwrap(),
        Some(Event::ContinuationReady)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    session.continue_round().await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(second["messages"][3]["tool_call_id"], "z-call");
    assert_eq!(second["messages"][4]["tool_call_id"], "a-call");
    assert_eq!(
        session.next_event().await.unwrap(),
        Some(Event::TextDelta("done".into()))
    );
    server.verify().await;
}

#[tokio::test]
async fn driver_waits_for_staggered_async_host_results_before_http_continuation() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::body_string_contains;
    use wiremock::matchers::method;
    struct DelayedHost(AtomicUsize);
    impl Host for DelayedHost {
        fn execute(
            &self,
            call: ToolCall,
            token: tokio_util::sync::CancellationToken,
        ) -> ProviderFuture<'_, ToolResult> {
            Box::pin(async move {
                let delay = if call.id == "first" { 50 } else { 100 };
                tokio::select! {
                    _ = token.cancelled() => return Err(ProviderError::Cancelled),
                    _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                }
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(ToolResult {
                    call_id: call.id,
                    output: json!("contents"),
                    is_error: false,
                })
            })
        }
        fn checkpoint(&self, _: Checkpoint) -> ProviderFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }
    let server = MockServer::start().await;
    let first = "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"first\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}},{\"index\":1,\"id\":\"second\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(first))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(body_string_contains("tool_call_id")).respond_with(ResponseTemplate::new(200).set_body_string("data: {\"id\":\"r2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")).with_priority(1).expect(1).mount(&server).await;
    let req = request("opencode-go");
    let codec = ChatCodec::new(server.uri(), BTreeMap::new(), false, &req.tools);
    let session = HttpSession::start(
        reqwest::Client::new(),
        Box::new(codec),
        req.clone(),
        context(),
    )
    .await
    .unwrap();
    let host = Arc::new(DelayedHost(AtomicUsize::new(0)));
    let (events, mut output) = tokio::sync::mpsc::channel(32);
    tokio::time::timeout(
        Duration::from_secs(3),
        drive_session(
            Box::new(session),
            host.clone(),
            &req,
            Default::default(),
            events,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(host.0.load(Ordering::SeqCst), 2);
    let mut completed = false;
    while let Some(event) = output.recv().await {
        if matches!(event.unwrap(), Event::Completed { .. }) {
            completed = true;
        }
    }
    assert!(completed);
    server.verify().await;
}

#[tokio::test]
async fn antigravity_drains_usage_after_finish_in_a_separate_network_chunk() {
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut bytes = [0; 1024];
            let size = socket.read(&mut bytes).await.unwrap();
            assert_ne!(size, 0);
            request.extend_from_slice(&bytes[..size]);
            if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                let length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        let content = "data: {\"response\":{\"responseId\":\"r\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"done\"}]},\"finishReason\":\"STOP\"}]}}\n\n";
        socket
            .write_all(format!("{:x}\r\n{content}\r\n", content.len()).as_bytes())
            .await
            .unwrap();
        socket.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let usage = "data: {\"response\":{\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":7,\"thoughtsTokenCount\":3}}}\n\n";
        socket
            .write_all(format!("{:x}\r\n{usage}\r\n0\r\n\r\n", usage.len()).as_bytes())
            .await
            .unwrap();
    });
    let req = request("antigravity");
    let codec = GoogleCodec::new(
        format!("http://{address}"),
        BTreeMap::new(),
        "project".into(),
        &req.tools,
    );
    let mut session = HttpSession::start(reqwest::Client::new(), Box::new(codec), req, context())
        .await
        .unwrap();
    assert_eq!(
        session.next_event().await.unwrap(),
        Some(Event::TextDelta("done".into()))
    );
    assert_eq!(
        session.next_event().await.unwrap(),
        Some(Event::Usage(Usage {
            input_tokens: 12,
            output_tokens: 7,
            cached_input_tokens: 0,
            reasoning_tokens: 3
        }))
    );
    assert!(matches!(
        session.next_event().await.unwrap(),
        Some(Event::Checkpoint(_))
    ));
    assert!(matches!(
        session.next_event().await.unwrap(),
        Some(Event::Completed { .. })
    ));
    upstream.await.unwrap();
}

#[test]
fn antigravity_claude_ignores_empty_stream_delimiters_and_preserves_tool_ids() {
    let mut req = request("antigravity");
    req.namespace.model = "claude-test".into();
    let mut codec = GoogleCodec::new("x".into(), BTreeMap::new(), "project".into(), &req.tools);
    feed(
        &mut codec,
        json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":""},{"text":"call tool"},{"functionCall":{"id":"toolu_1","name":"read","args":{"path":"a"}}},{"text":""}]},"finishReason":"STOP"}]}}),
    );
    req.messages.push(codec.assistant(&req.namespace).unwrap());
    req.messages.push(Message {
        role: Role::Tool,
        content: vec![Content::ToolResult(ToolResult {
            call_id: "toolu_1".into(),
            output: json!("contents"),
            is_error: false,
        })],
    });
    let wire = codec.request(&req, &context()).unwrap();
    assert_eq!(
        wire.body["request"]["contents"][1]["parts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        wire.body["request"]["contents"][1]["parts"][1]["functionCall"]["id"],
        "toolu_1"
    );
    assert_eq!(
        wire.body["request"]["contents"][2]["parts"][0]["functionResponse"]["id"],
        "toolu_1"
    );
    assert_eq!(
        wire.body["request"]["contents"][2]["parts"][0]["functionResponse"]["response"]["output"],
        "contents"
    );
}

fn multimodal_tool_request(provider: &str) -> Request {
    let mut req = request(provider);
    req.messages.push(Message {
        role: Role::Assistant,
        content: vec![Content::ToolCall(ToolCall {
            id: "image-call".into(),
            name: "read".into(),
            arguments: json!({}),
        })],
    });
    req.messages.push(Message {
        role: Role::Tool,
        content: vec![Content::ToolResult(ToolResult {
            call_id: "image-call".into(),
            output: json!([
                {"type":"input_text","text":"image caption"},
                {"type":"input_image","image_url":"data:image/png;base64,aW1hZ2U=","detail":"high"}
            ]),
            is_error: false,
        })],
    });
    req
}

#[test]
fn messages_tool_results_keep_text_and_image_blocks() {
    let req = multimodal_tool_request("claude");
    let codec = MessagesCodec::new("x".into(), BTreeMap::new(), true, &req.tools).unwrap();
    let body = codec.request(&req, &context()).unwrap().body;
    let result = &body["messages"][2]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "image-call");
    assert_eq!(
        result["content"],
        json!([
            {"type":"text","text":"image caption"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="}}
        ])
    );
}

#[test]
fn messages_image_only_tool_results_include_reference_text_placeholder() {
    let mut req = multimodal_tool_request("opencode-go");
    let Content::ToolResult(result) = &mut req.messages.last_mut().unwrap().content[0] else {
        panic!("expected tool result");
    };
    result.output = json!([{"type":"input_image","image_url":"data:image/png;base64,aW1hZ2U="}]);
    let codec = MessagesCodec::new("x".into(), BTreeMap::new(), false, &req.tools).unwrap();
    let body = codec.request(&req, &context()).unwrap().body;
    assert_eq!(
        body["messages"][2]["content"][0]["content"],
        json!([
            {"type":"text","text":"(see attached image)"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="}}
        ])
    );
}

#[test]
fn responses_tool_results_keep_content_items_and_image_detail() {
    let req = multimodal_tool_request("opencode-go");
    let codec = ResponsesCodec::new("x".into(), BTreeMap::new(), &req.tools);
    let body = codec.request(&req, &context()).unwrap().body;
    assert_eq!(
        body["input"][2]["output"],
        json!([
            {"type":"input_text","text":"image caption"},
            {"type":"input_image","image_url":"data:image/png;base64,aW1hZ2U=","detail":"high"}
        ])
    );
}

#[test]
fn chat_tool_images_follow_all_parallel_tool_responses() {
    let mut req = multimodal_tool_request("opencode-go");
    req.messages[1].content.push(Content::ToolCall(ToolCall {
        id: "second-call".into(),
        name: "read".into(),
        arguments: json!({}),
    }));
    req.messages.push(Message {
        role: Role::Tool,
        content: vec![Content::ToolResult(ToolResult {
            call_id: "second-call".into(),
            output: json!("second result"),
            is_error: false,
        })],
    });
    let codec = ChatCodec::new("x".into(), BTreeMap::new(), false, &req.tools);
    let body = codec.request(&req, &context()).unwrap().body;
    assert_eq!(body["messages"][3]["role"], "tool");
    assert_eq!(body["messages"][3]["content"], "image caption");
    assert_eq!(body["messages"][4]["role"], "tool");
    assert_eq!(body["messages"][5]["role"], "user");
    assert_eq!(
        body["messages"][5]["content"][0]["image_url"]["url"],
        "data:image/png;base64,aW1hZ2U="
    );
}

#[test]
fn google_gemini_three_tool_results_nest_inline_image_parts() {
    let mut req = multimodal_tool_request("antigravity");
    req.namespace.model = "gemini-3.5-flash-lite".into();
    let codec = GoogleCodec::new("x".into(), BTreeMap::new(), "project".into(), &req.tools);
    let body = codec.request(&req, &context()).unwrap().body;
    let response = &body["request"]["contents"][2]["parts"][0]["functionResponse"];
    assert_eq!(response["name"], "read");
    assert_eq!(response["response"]["output"], "image caption");
    assert_eq!(
        response["parts"],
        json!([{"inlineData":{"mimeType":"image/png","data":"aW1hZ2U="}}])
    );
    req.namespace.model = "claude-sonnet-4-6".into();
    let claude = codec.request(&req, &context()).unwrap().body;
    let parts = &claude["request"]["contents"][2]["parts"];
    assert_eq!(parts[0]["functionResponse"]["id"], "image-call");
    assert!(parts[0]["functionResponse"].get("parts").is_none());
    assert_eq!(
        parts[1],
        json!({"inlineData":{"mimeType":"image/png","data":"aW1hZ2U="}})
    );
    req.namespace.model = "gemini-2.5-pro".into();
    let older_gemini = codec.request(&req, &context()).unwrap().body;
    assert_eq!(older_gemini["request"]["contents"][2]["parts"][1], parts[1]);
}

#[test]
fn unsupported_tool_content_is_not_silently_stringified() {
    assert!(matches!(
        tool_output(&json!([{"type":"input_audio","audio_url":"data:audio/wav;base64,eA=="}])),
        Err(ProviderError::Unsupported(_))
    ));
    let mut req = multimodal_tool_request("claude");
    if let Content::ToolResult(result) = &mut req.messages[2].content[0] {
        result.output = json!([{"type":"input_image","file_id":"fixture-file"}]);
    }
    let messages = MessagesCodec::new("x".into(), BTreeMap::new(), true, &req.tools).unwrap();
    assert!(matches!(
        messages.request(&req, &context()),
        Err(ProviderError::Unsupported(_))
    ));
    let responses = ResponsesCodec::new("x".into(), BTreeMap::new(), &req.tools);
    assert_eq!(
        responses.request(&req, &context()).unwrap().body["input"][2]["output"],
        json!([{"type":"input_image","file_id":"fixture-file"}])
    );
}

#[tokio::test]
async fn terminal_events_with_partial_trailing_sse_never_checkpoint() {
    for family in ["messages", "responses", "chat"] {
        let server = wiremock::MockServer::start().await;
        let req = request("opencode-go");
        let (codec, terminal): (Box<dyn WireCodec>, &str) = match family {
            "messages" => (
                Box::new(
                    MessagesCodec::new(server.uri(), BTreeMap::new(), false, &req.tools).unwrap(),
                ),
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"terminal-fixture\",\"usage\":{}}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
            ),
            "responses" => (
                Box::new(ResponsesCodec::new(
                    server.uri(),
                    BTreeMap::new(),
                    &req.tools,
                )),
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"terminal-fixture\",\"output\":[],\"usage\":{}}}\n\n",
            ),
            _ => (
                Box::new(ChatCodec::new(
                    server.uri(),
                    BTreeMap::new(),
                    false,
                    &req.tools,
                )),
                "data: {\"id\":\"terminal-fixture\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            ),
        };
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string(format!("{terminal}data: {{\"unfinished\":")),
            )
            .mount(&server)
            .await;
        let mut session = HttpSession::start(reqwest::Client::new(), codec, req, context())
            .await
            .unwrap();
        assert!(
            matches!(session.next_event().await, Err(ProviderError::Protocol(message)) if message == "truncated SSE event"),
            "family {family}"
        );
    }
}

#[test]
fn google_empty_tool_arguments_are_explicit_on_replay_without_mutating_checkpoint_parts() {
    let mut req = request("antigravity");
    req.namespace.model = "claude-sonnet-4-6".into();
    let mut codec = GoogleCodec::new("x".into(), BTreeMap::new(), "project".into(), &req.tools);
    feed(
        &mut codec,
        json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"read","id":"empty-call"},"thoughtSignature":"fixture-signature"}]},"finishReason":"STOP"}]}}),
    );
    let assistant = codec.assistant(&req.namespace).unwrap();
    assert!(
        assistant
            .content
            .iter()
            .any(|part| matches!(part, Content::ToolCall(call) if call.arguments == json!({})))
    );
    let raw = assistant
        .content
        .iter()
        .find_map(|part| match part {
            Content::Opaque { value, .. } => Some(value),
            _ => None,
        })
        .unwrap();
    assert!(raw["google_parts"][0]["functionCall"].get("args").is_none());
    req.messages.push(assistant);
    let wire = codec.request(&req, &context()).unwrap();
    let replay = &wire.body["request"]["contents"][1]["parts"][0];
    assert_eq!(replay["functionCall"]["args"], json!({}));
    assert_eq!(replay["thoughtSignature"], "fixture-signature");
}

#[test]
fn antigravity_claude_fresh_branch_omits_empty_canonical_text_before_interrupted_tool() {
    let mut req = request("antigravity");
    req.namespace.model = "claude-sonnet-4-6".into();
    req.messages.extend([
        Message {
            role: Role::Assistant,
            content: vec![
                Content::Text {
                    text: String::new(),
                },
                Content::Thinking {
                    text: String::new(),
                    signature: None,
                },
                Content::ToolCall(ToolCall {
                    id: "interrupted-call".into(),
                    name: "read".into(),
                    arguments: json!({"path":"fixture"}),
                }),
            ],
        },
        Message {
            role: Role::Tool,
            content: vec![Content::ToolResult(ToolResult {
                call_id: "interrupted-call".into(),
                output: json!("aborted by user"),
                is_error: true,
            })],
        },
        Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "Continue this explicit branch without retrying tools.".into(),
            }],
        },
    ]);
    let codec = GoogleCodec::new("x".into(), BTreeMap::new(), "project".into(), &req.tools);
    let body = codec.request(&req, &context()).unwrap().body;
    let contents = body["request"]["contents"].as_array().unwrap();
    let model_parts = contents[1]["parts"].as_array().unwrap();
    assert_eq!(model_parts.len(), 1);
    assert_eq!(model_parts[0]["functionCall"]["id"], "interrupted-call");
    assert_eq!(
        model_parts[0]["functionCall"]["args"],
        json!({"path":"fixture"})
    );
    assert_eq!(
        contents[2]["parts"][0]["functionResponse"]["response"],
        json!({"error":"aborted by user"})
    );
    assert!(
        contents
            .iter()
            .flat_map(|content| content["parts"].as_array().unwrap())
            .all(|part| part.get("text").is_none_or(|text| text != ""))
    );
}

#[test]
fn account_handoff_refuses_thinking_and_unknown_replay_without_mutating_history() {
    let mut original = request("claude");
    original.messages.push(Message {
        role: Role::Assistant,
        content: vec![Content::Thinking {
            text: "thinking".into(),
            signature: Some("signed-original".into()),
        }],
    });
    assert!(crate::portable_history(&original).is_err());
    assert!(
        matches!(&original.messages.last().unwrap().content[0], Content::Thinking { signature: Some(value), .. } if value == "signed-original")
    );
    original.messages.last_mut().unwrap().content = vec![Content::Opaque {
        namespace: original.namespace.clone(),
        value: json!({"anthropic_blocks":[{"type":"redacted_thinking","data":"opaque"}]}),
    }];
    assert!(crate::portable_history(&original).is_err());
}

#[test]
fn account_handoff_exports_only_verified_ordinary_history() {
    let mut original = request("claude");
    original.messages.push(Message {
        role: Role::Assistant,
        content: vec![
            Content::Text {
                text: "recorded".into(),
            },
            Content::Opaque {
                namespace: original.namespace.clone(),
                value: json!({"anthropic_blocks":[{"type":"text","text":"recorded"}]}),
            },
        ],
    });
    let exported = crate::portable_history(&original).unwrap();
    assert!(exported.checkpoint.is_none());
    assert_eq!(
        exported.messages.last().unwrap().content,
        vec![Content::Text {
            text: "recorded".into()
        }]
    );
    assert_eq!(original.messages.last().unwrap().content.len(), 2);
}
