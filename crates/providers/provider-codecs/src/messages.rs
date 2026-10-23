use crate::ToolOutputPart;
use crate::WireCodec;
use crate::WireRequest;
use crate::image_source;
use crate::protocol;
use crate::string;
use crate::tool_image_url;
use crate::tool_output;
use codex_provider_api::*;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub struct MessagesCodec {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub subscription: bool,
    blocks: BTreeMap<u64, Value>,
    arguments: BTreeMap<u64, String>,
    tool_names: BTreeMap<String, String>,
    usage: Usage,
    id: String,
    done: bool,
    open_blocks: BTreeSet<u64>,
}
impl MessagesCodec {
    pub fn new(
        url: String,
        headers: BTreeMap<String, String>,
        subscription: bool,
        tools: &[ToolDefinition],
    ) -> Result<Self> {
        let mut tool_names = BTreeMap::new();
        for tool in tools {
            let wire = if subscription {
                canonical_name(&tool.name)
            } else {
                tool.name.clone()
            };
            if tool_names.insert(wire, tool.name.clone()).is_some() {
                return Err(protocol("tool names collide on the Messages wire"));
            }
        }
        Ok(Self {
            url,
            headers,
            subscription,
            blocks: BTreeMap::new(),
            arguments: BTreeMap::new(),
            tool_names,
            usage: Usage::default(),
            id: String::new(),
            done: false,
            open_blocks: BTreeSet::new(),
        })
    }
    fn wire_name(&self, name: &str) -> String {
        if self.subscription {
            canonical_name(name)
        } else {
            name.to_owned()
        }
    }
    fn encode_message(&self, message: &Message, namespace: &Namespace) -> Result<Vec<Value>> {
        for content in &message.content {
            if let Content::Opaque {
                namespace: binding,
                value,
            } = content
            {
                if binding != namespace {
                    return Err(ProviderError::Checkpoint(
                        "Messages replay namespace mismatch".into(),
                    ));
                }
                if let Some(blocks) = value["anthropic_blocks"].as_array() {
                    return Ok(blocks.clone());
                }
                return Err(ProviderError::Checkpoint(
                    "foreign Messages replay format".into(),
                ));
            }
        }
        let mut blocks = Vec::new();
        for content in &message.content {
            match content {
                Content::Text { text } => blocks.push(json!({"type":"text","text":text})),
                Content::Image { url } => {
                    let source = if let Some(data) = url.strip_prefix("data:") {
                        let (media, data) = data.split_once(";base64,").ok_or_else(|| protocol("Messages image must be a base64 data URL"))?;
                        json!({"type":"base64","media_type":media,"data":data})
                    } else { json!({"type":"url","url":url}) };
                    blocks.push(json!({"type":"image","source":source}));
                }
                Content::Thinking { text, signature: Some(signature) } => blocks.push(json!({"type":"thinking","thinking":text,"signature":signature})),
                Content::Thinking { .. } => {}, // An interrupted, unsigned thought is not valid signed replay.
                Content::ToolCall(call) => blocks.push(json!({"type":"tool_use","id":call.id,"name":self.wire_name(&call.name),"input":call.arguments})),
                Content::ToolResult(result) => {
                    let content = if result.output.is_array() {
                        let parts = tool_output(&result.output)?;
                        let mut content = parts.iter().map(|part| match part {
                            ToolOutputPart::Text(text) => Ok(json!({"type":"text","text":text})),
                            ToolOutputPart::Image(image) => Ok(json!({"type":"image","source":image_source(tool_image_url(image)?)?})),
                        }).collect::<Result<Vec<_>>>()?;
                        // The pinned Pi Messages adapter adds this text for image-only results.
                        if parts.iter().any(|part| matches!(part, ToolOutputPart::Image(_)))
                            && !parts.iter().any(|part| matches!(part, ToolOutputPart::Text(_)))
                        {
                            content.insert(0, json!({"type":"text","text":"(see attached image)"}));
                        }
                        json!(content)
                    } else { json!(crate::output_text(&result.output)) };
                    blocks.push(json!({"type":"tool_result","tool_use_id":result.call_id,"content":content,"is_error":result.is_error}));
                },
                Content::Opaque { .. } => {},
            }
        }
        Ok(blocks)
    }
}
fn canonical_name(name: &str) -> String {
    [
        "Read",
        "Write",
        "Edit",
        "Bash",
        "Grep",
        "Glob",
        "AskUserQuestion",
        "EnterPlanMode",
        "ExitPlanMode",
        "KillShell",
        "NotebookEdit",
        "Skill",
        "Task",
        "TaskOutput",
        "TodoWrite",
        "WebFetch",
        "WebSearch",
    ]
    .iter()
    .find(|value| value.eq_ignore_ascii_case(name))
    .map_or_else(|| name.to_owned(), |value| (*value).to_owned())
}
impl WireCodec for MessagesCodec {
    fn request(&self, request: &Request, context: &Context) -> Result<WireRequest> {
        let mut headers = self.headers.clone();
        if self.subscription {
            headers.insert(
                "authorization".into(),
                format!("Bearer {}", context.credentials.access_token),
            );
        } else {
            headers.insert("x-api-key".into(), context.credentials.access_token.clone());
        }
        headers.insert("anthropic-version".into(), "2023-06-01".into());
        let mut system = Vec::new();
        if self.subscription {
            system.push(json!({"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}));
        }
        if !request.system.is_empty() {
            system.push(json!({"type":"text","text":request.system}));
        }
        let mut messages: Vec<Value> = Vec::new();
        for message in &request.messages {
            let blocks = self.encode_message(message, &request.namespace)?;
            if blocks.is_empty() {
                continue;
            }
            if message.role == Role::System {
                system.extend(blocks);
                continue;
            }
            let role = if message.role == Role::Assistant {
                "assistant"
            } else {
                "user"
            };
            if let Some(previous) = messages
                .last_mut()
                .filter(|previous| previous["role"] == role)
            {
                previous["content"]
                    .as_array_mut()
                    .ok_or_else(|| protocol("invalid Messages history"))?
                    .extend(blocks);
            } else {
                messages.push(json!({"role":role,"content":blocks}));
            }
        }
        let mut body = json!({"model":request.namespace.model,"messages":messages,"max_tokens":request.max_output_tokens.unwrap_or(8192),"stream":true});
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(request.tools.iter().map(|tool| json!({"name":self.wire_name(&tool.name),"description":tool.description,"input_schema":tool.parameters})).collect::<Vec<_>>());
            body["tool_choice"] =
                json!({"type":"auto","disable_parallel_tool_use":!request.parallel_tool_calls});
        }
        if let Some(effort) = &request.reasoning_effort
            && effort != "none"
            && effort != "off"
        {
            if request.metadata["anthropic_adaptive_thinking"] == true {
                body["thinking"] = json!({"type":"adaptive"});
                body["output_config"] = json!({"effort":effort});
            } else {
                let budget = match effort.as_str() {
                    "low" => 1024,
                    "medium" => 4096,
                    "high" => 8192,
                    "xhigh" | "max" => 16384,
                    _ => {
                        return Err(ProviderError::Unsupported(
                            "Messages reasoning effort".into(),
                        ));
                    }
                };
                let max = request.max_output_tokens.unwrap_or(32768);
                if max <= budget {
                    return Err(protocol(
                        "thinking budget must be smaller than max output tokens",
                    ));
                }
                body["max_tokens"] = json!(max);
                body["thinking"] = json!({"type":"enabled","budget_tokens":budget});
            }
        }
        if let Some(schema) = &request.output_schema {
            body["output_config"]["format"] = json!({"type":"json_schema","schema":schema});
        }
        Ok(WireRequest {
            url: self.url.clone(),
            headers,
            body,
        })
    }
    fn reset(&mut self) {
        self.blocks.clear();
        self.open_blocks.clear();
        self.arguments.clear();
        self.usage = Usage::default();
        self.id.clear();
        self.done = false;
    }
    fn frame(&mut self, _event: &str, data: &str) -> Result<Vec<Event>> {
        let value: Value =
            serde_json::from_str(data).map_err(|_| protocol("invalid Messages SSE JSON"))?;
        let mut events = Vec::new();
        match value["type"].as_str() {
            Some("message_start") => {
                self.id = string(&value["message"], "id")?;
                self.usage.input_tokens = value["message"]["usage"]["input_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                self.usage.cached_input_tokens =
                    value["message"]["usage"]["cache_read_input_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                self.usage.output_tokens = value["message"]["usage"]["output_tokens"]
                    .as_u64()
                    .unwrap_or(0);
            }
            Some("content_block_start") => {
                let index = value["index"]
                    .as_u64()
                    .ok_or_else(|| protocol("missing Messages block index"))?;
                let block = value["content_block"].clone();
                self.open_blocks.insert(index);
                if let Some(text) = block["text"].as_str().filter(|text| !text.is_empty()) {
                    events.push(Event::TextDelta(text.to_owned()));
                }
                if let Some(text) = block["thinking"].as_str().filter(|text| !text.is_empty()) {
                    events.push(Event::ThinkingDelta(text.to_owned()));
                }
                if self.blocks.insert(index, block).is_some() {
                    return Err(protocol("duplicate Messages block index"));
                }
            }
            Some("content_block_delta") => {
                let index = value["index"]
                    .as_u64()
                    .ok_or_else(|| protocol("missing Messages block index"))?;
                let block = self
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(|| protocol("Messages delta before block start"))?;
                let delta = &value["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = string(delta, "text")?;
                        let full = block["text"].as_str().unwrap_or("").to_owned() + &text;
                        block["text"] = json!(full);
                        events.push(Event::TextDelta(text));
                    }
                    Some("thinking_delta") => {
                        let text = string(delta, "thinking")?;
                        let full = block["thinking"].as_str().unwrap_or("").to_owned() + &text;
                        block["thinking"] = json!(full);
                        events.push(Event::ThinkingDelta(text));
                    }
                    Some("signature_delta") => {
                        let signature = string(delta, "signature")?;
                        let full =
                            block["signature"].as_str().unwrap_or("").to_owned() + &signature;
                        block["signature"] = json!(full);
                    }
                    Some("input_json_delta") => self
                        .arguments
                        .entry(index)
                        .or_default()
                        .push_str(&string(delta, "partial_json")?),
                    _ => return Err(protocol("unsupported Messages content delta")),
                }
            }
            Some("content_block_stop") => {
                let index = value["index"]
                    .as_u64()
                    .ok_or_else(|| protocol("missing Messages block index"))?;
                if !self.open_blocks.remove(&index) {
                    return Err(protocol("Messages stop without open block"));
                }
                if let Some(arguments) = self.arguments.remove(&index) {
                    let block = self
                        .blocks
                        .get_mut(&index)
                        .ok_or_else(|| protocol("Messages stop before block start"))?;
                    block["input"] = serde_json::from_str(&arguments)
                        .map_err(|_| protocol("invalid tool arguments JSON"))?;
                }
            }
            Some("message_delta") => {
                if let Some(output) = value["usage"]["output_tokens"].as_u64() {
                    self.usage.output_tokens = output;
                }
                events.push(Event::Usage(self.usage.clone()));
            }
            Some("message_stop") => {
                if !self.arguments.is_empty() || !self.open_blocks.is_empty() {
                    return Err(protocol("unfinished Messages tool JSON"));
                }
                if self.id.is_empty() {
                    return Err(protocol("Messages terminal before message start"));
                }
                if self.subscription
                    && self.blocks.values().any(|block| {
                        block["type"] == "thinking"
                            && block["signature"].as_str().is_none_or(str::is_empty)
                    })
                {
                    return Err(protocol(
                        "Claude thinking block is missing its replay signature",
                    ));
                }
                self.done = true;
            }
            Some("ping") => {}
            Some("error") => {
                return Err(protocol(format!(
                    "Messages upstream error: {}",
                    value["error"]["type"].as_str().unwrap_or("unknown")
                )));
            }
            _ => return Err(protocol("unsupported Messages SSE event")),
        }
        Ok(events)
    }
    fn finished(&self) -> bool {
        self.done
    }
    fn response_id(&self) -> String {
        self.id.clone()
    }
    fn assistant(&self, namespace: &Namespace) -> Result<Message> {
        let mut content = Vec::new();
        for block in self.blocks.values() {
            match block["type"].as_str() {
                Some("text") => content.push(Content::Text {
                    text: string(block, "text")?,
                }),
                Some("thinking") => content.push(Content::Thinking {
                    text: string(block, "thinking")?,
                    signature: block["signature"].as_str().map(str::to_owned),
                }),
                Some("tool_use") => {
                    if !block["input"].is_object() {
                        return Err(protocol("Messages tool arguments must be an object"));
                    }
                    let name = string(block, "name")?;
                    let name = self
                        .tool_names
                        .get(&name)
                        .ok_or_else(|| protocol("Messages returned undeclared tool"))?
                        .clone();
                    content.push(Content::ToolCall(ToolCall {
                        id: string(block, "id")?,
                        name,
                        arguments: block["input"].clone(),
                    }));
                }
                Some("redacted_thinking") => {}
                _ => return Err(protocol("unsupported Messages output block")),
            }
        }
        content.push(Content::Opaque {
            namespace: namespace.clone(),
            value: json!({"anthropic_blocks":self.blocks.values().collect::<Vec<_>>()}),
        });
        Ok(Message {
            role: Role::Assistant,
            content,
        })
    }
}
