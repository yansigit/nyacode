use codex_provider_api::*;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

fn hash(request: &Request, messages: &[Message]) -> Result<String> {
    // Match the original sorted JSON bytes even when another workspace target
    // enables serde_json's preserve_order feature. Wire requests stay unchanged.
    let mut history = json!({"system":request.system,"messages":messages.iter().map(public_message).collect::<Vec<_>>()});
    history.sort_all_objects();
    let bytes = serde_json::to_vec(&history)
        .map_err(|_| ProviderError::Checkpoint("invalid history".into()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}
pub(crate) fn checkpoint(request: &Request, assistant: &Message) -> Result<Checkpoint> {
    let replay = request
        .messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message
                .content
                .iter()
                .any(|content| matches!(content, Content::Opaque { .. }))
        })
        .map(|(index, message)| json!({"index":index,"message":message}))
        .collect::<Vec<_>>();
    let mut canonical_history = request.messages.clone();
    canonical_history.push(assistant.clone());
    let state = json!({"prefix_len":request.messages.len(),"assistant":assistant,"replay":replay,"canonical_history":canonical_history});
    if state.to_string().len() > 16 * 1024 * 1024 {
        return Err(ProviderError::Checkpoint(
            "signed replay exceeds 16 MiB".into(),
        ));
    }
    Ok(Checkpoint {
        version: 1,
        namespace: request.namespace.clone(),
        context_hash: hash(request, &request.messages)?,
        state,
    })
}
fn public_message(message: &Message) -> Message {
    let mut content = Vec::new();
    for block in &message.content {
        let block = match block {
            Content::Opaque { .. } => continue,
            Content::Text { text } | Content::Thinking { text, .. } if text.is_empty() => continue,
            Content::Thinking { text, .. } => Content::Thinking {
                text: text.clone(),
                signature: None,
            },
            block => block.clone(),
        };
        match (content.last_mut(), &block) {
            (Some(Content::Text { text: previous }), Content::Text { text }) => {
                previous.push_str(text)
            }
            (Some(Content::Thinking { text: previous, .. }), Content::Thinking { text, .. }) => {
                previous.push_str(text)
            }
            _ => content.push(block),
        }
    }
    // Parallel hosts persist results as they finish; wire requests retain call
    // order. The same identified result batch has identical replay semantics.
    if message.role == Role::Tool
        && content
            .iter()
            .all(|block| matches!(block, Content::ToolResult(_)))
    {
        content.sort_by(|left, right| match (left, right) {
            (Content::ToolResult(left), Content::ToolResult(right)) => {
                left.call_id.cmp(&right.call_id)
            }
            _ => std::cmp::Ordering::Equal,
        });
    }
    Message {
        role: message.role.clone(),
        content,
    }
}
/// Restore only the matching, generated assistant message. Signed state cannot migrate
/// across accounts, models, threads, or edited/compacted conversation prefixes.
pub fn restore_checkpoint(request: &mut Request) -> Result<()> {
    let Some(checkpoint) = request.checkpoint.as_ref() else {
        return Ok(());
    };
    if checkpoint.version != 1 || checkpoint.namespace != request.namespace {
        return Err(ProviderError::Checkpoint(
            "checkpoint namespace/version mismatch".into(),
        ));
    }
    let prefix_len = checkpoint.state["prefix_len"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| ProviderError::Checkpoint("invalid prefix length".into()))?;
    // The core host independently binds its exact persisted history, which can
    // differ from native wire history after parallel execution or tool adapters.
    // This marker is supplied only after that host-side lineage validation.
    if let Some(host_prefix) = request.metadata.get("host_checkpoint_prefix") {
        let host_prefix = host_prefix
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| ProviderError::Checkpoint("invalid verified host prefix".into()))?;
        let mut canonical: Vec<Message> =
            serde_json::from_value(checkpoint.state["canonical_history"].clone()).map_err(
                |_| ProviderError::Checkpoint("native canonical checkpoint history missing".into()),
            )?;
        if host_prefix > request.messages.len()
            || canonical.len() != prefix_len.saturating_add(1)
            || hash(request, &canonical[..prefix_len])? != checkpoint.context_hash
        {
            return Err(ProviderError::Checkpoint(
                "verified native checkpoint history mismatch".into(),
            ));
        }
        canonical.extend_from_slice(&request.messages[host_prefix..]);
        request.messages = canonical;
    }
    if prefix_len > request.messages.len()
        || hash(request, &request.messages[..prefix_len])? != checkpoint.context_hash
    {
        return Err(ProviderError::Checkpoint(
            "checkpoint conversation prefix mismatch".into(),
        ));
    }
    if let Some(replay) = checkpoint.state["replay"].as_array() {
        for entry in replay {
            let index = entry["index"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| ProviderError::Checkpoint("invalid replay index".into()))?;
            if index >= prefix_len {
                return Err(ProviderError::Checkpoint(
                    "replay index exceeds checkpoint prefix".into(),
                ));
            }
            let message: Message = serde_json::from_value(entry["message"].clone())
                .map_err(|_| ProviderError::Checkpoint("invalid replay message".into()))?;
            let existing = request
                .messages
                .get_mut(index)
                .ok_or_else(|| ProviderError::Checkpoint("missing replay message".into()))?;
            if public_message(existing) != public_message(&message) {
                return Err(ProviderError::Checkpoint(
                    "historical replay mismatch".into(),
                ));
            }
            *existing = message;
        }
    }
    let assistant: Message = serde_json::from_value(checkpoint.state["assistant"].clone())
        .map_err(|_| ProviderError::Checkpoint("invalid assistant replay".into()))?;
    if let Some(existing) = request.messages.get_mut(prefix_len) {
        if public_message(existing) != public_message(&assistant) {
            return Err(ProviderError::Checkpoint(
                "checkpoint assistant mismatch".into(),
            ));
        }
        *existing = assistant;
    } else {
        request.messages.push(assistant);
    }
    request.checkpoint = None;
    Ok(())
}

/// Export ordinary, fully understood history. Signed thinking and unfamiliar
/// replay blocks are refused intact; they are never rewritten to force a fit.
pub fn portable_history(request: &Request) -> Result<Request> {
    let mut exported = request.clone();
    restore_checkpoint(&mut exported)?;
    for message in &mut exported.messages {
        for block in &message.content {
            match block {
                Content::Thinking { .. } => {
                    return Err(ProviderError::Unsupported(
                        "account handoff with thinking requires verified signature portability"
                            .into(),
                    ));
                }
                Content::Opaque { namespace, value }
                    if namespace != &request.namespace || !ordinary_replay(value) =>
                {
                    return Err(ProviderError::Unsupported(
                        "account handoff with opaque or signed history is not verified".into(),
                    ));
                }
                _ => {}
            }
        }
        message
            .content
            .retain(|block| !matches!(block, Content::Opaque { .. }));
    }
    exported.checkpoint = None;
    if let Some(metadata) = exported.metadata.as_object_mut() {
        metadata.remove("host_checkpoint_prefix");
    }
    Ok(exported)
}
fn ordinary_replay(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object().filter(|object| object.len() == 1) else {
        return false;
    };
    if let Some(blocks) = object
        .get("anthropic_blocks")
        .and_then(serde_json::Value::as_array)
    {
        return blocks.iter().all(|block| match block["type"].as_str() {
            Some("text") => keys_only(block, &["type", "text"]),
            Some("tool_use") => keys_only(block, &["type", "id", "name", "input"]),
            _ => false,
        });
    }
    if let Some(items) = object
        .get("responses_output")
        .and_then(serde_json::Value::as_array)
    {
        return items.iter().all(|item| match item["type"].as_str() {
            Some("function_call") => keys_only(
                item,
                &["type", "id", "status", "call_id", "name", "arguments"],
            ),
            Some("message") => {
                keys_only(item, &["type", "id", "status", "role", "content"])
                    && item["content"].as_array().is_some_and(|parts| {
                        parts.iter().all(|part| {
                            part["type"] == "output_text"
                                && keys_only(part, &["type", "text", "annotations", "logprobs"])
                        })
                    })
            }
            _ => false,
        });
    }
    false
}
fn keys_only(value: &serde_json::Value, keys: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.keys().all(|key| keys.contains(&key.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_hashes_keep_baseline_bytes_across_json_map_features() {
        let messages = serde_json::from_str(r#"[{"role":"assistant","content":[{"type":"tool_call","id":"call","name":"lookup","arguments":{"z":{"b":2,"a":1},"a":"first"}}]},{"role":"tool","content":[{"type":"tool_result","call_id":"call","output":{"z":[{"b":2,"a":1}],"a":1},"is_error":true}]}]"#).unwrap();
        let mut request = Request {
            namespace: Namespace {
                provider: "claude".into(),
                account: "a".into(),
                model: "m".into(),
                thread: "t".into(),
            },
            system: "rules".into(),
            messages,
            tools: vec![],
            parallel_tool_calls: false,
            max_output_tokens: None,
            reasoning_effort: None,
            output_schema: None,
            checkpoint: None,
            metadata: json!({}),
        };
        assert_eq!(
            hash(&request, &request.messages).unwrap(),
            "8377e32212ac08e30e751146fc78f2475380643531d221efb88e4e63f69e21c8"
        );
        let assistant = Message {
            role: Role::Assistant,
            content: vec![Content::Text {
                text: "done".into(),
            }],
        };
        request.checkpoint = Some(checkpoint(&request, &assistant).unwrap());
        restore_checkpoint(&mut request).unwrap();
        assert_eq!(request.messages.last(), Some(&assistant));
    }
}
