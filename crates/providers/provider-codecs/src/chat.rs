use crate::ToolOutputPart;
use crate::WireCodec;
use crate::WireRequest;
use crate::protocol;
use crate::tool_image_url;
use crate::tool_output;
use codex_provider_api::*;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

pub struct ChatCodec {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub reasoning_content: bool,
    text: String,
    thinking: String,
    calls: BTreeMap<u64, (String, String, String)>,
    declared: Vec<String>,
    finish_reason: Option<String>,
    done: bool,
    id: String,
    content: Vec<Content>,
}
impl ChatCodec {
    pub fn new(
        url: String,
        headers: BTreeMap<String, String>,
        reasoning_content: bool,
        tools: &[ToolDefinition],
    ) -> Self {
        Self {
            url,
            headers,
            reasoning_content,
            text: String::new(),
            thinking: String::new(),
            calls: BTreeMap::new(),
            declared: tools.iter().map(|tool| tool.name.clone()).collect(),
            finish_reason: None,
            done: false,
            id: String::new(),
            content: Vec::new(),
        }
    }
}
impl WireCodec for ChatCodec {
    fn request(&self, request: &Request, context: &Context) -> Result<WireRequest> {
        let mut headers = self.headers.clone();
        headers.insert(
            "authorization".into(),
            format!("Bearer {}", context.credentials.access_token),
        );
        let mut messages = Vec::new();
        if !request.system.is_empty() {
            messages.push(json!({"role":"system","content":request.system}));
        }
        let mut tool_images = Vec::new();
        for message in &request.messages {
            if message.role != Role::Tool && !tool_images.is_empty() {
                messages.push(json!({"role":"user","content":std::mem::take(&mut tool_images)}));
            }
            let role = match message.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            let mut content = Vec::new();
            let mut calls = Vec::new();
            let mut thinking = String::new();
            for block in &message.content {
                match block {
                    Content::Text { text } => content.push(json!({"type":"text","text":text})),
                    Content::Image { url } => content.push(json!({"type":"image_url","image_url":{"url":url}})),
                    Content::Thinking { text, .. } => thinking.push_str(text),
                    Content::ToolCall(call) => calls.push(json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments.to_string()}})),
                    Content::ToolResult(result) => {
                        let mut text = Vec::new();
                        let mut images = Vec::new();
                        for part in tool_output(&result.output)? {
                            match part {
                                ToolOutputPart::Text(value) => text.push(value),
                                ToolOutputPart::Image(image) => images.push(json!({"type":"image_url","image_url":{"url":tool_image_url(&image)?}})),
                            }
                        }
                        let mut text = text.join("\n");
                        if text.is_empty() && !images.is_empty() { text = "(see attached image)".into(); }
                        messages.push(json!({"role":"tool","tool_call_id":result.call_id,"content":text}));
                        tool_images.extend(images);
                    },
                    Content::Opaque { namespace, .. } => { if namespace != &request.namespace { return Err(ProviderError::Checkpoint("Chat replay namespace mismatch".into())); } },
                }
            }
            if !content.is_empty() || !calls.is_empty() || !thinking.is_empty() {
                let wire_content = if role == "assistant" {
                    if content.iter().any(|block| block["type"] != "text") {
                        return Err(ProviderError::Unsupported(
                            "Chat assistant image replay".into(),
                        ));
                    }
                    let text = content
                        .iter()
                        .filter_map(|block| block["text"].as_str())
                        .collect::<String>();
                    if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text)
                    }
                } else {
                    json!(content)
                };
                let mut value = json!({"role":role,"content":wire_content});
                if !calls.is_empty() {
                    value["tool_calls"] = json!(calls);
                }
                if self.reasoning_content && role == "assistant" {
                    value["reasoning_content"] = json!(thinking);
                }
                messages.push(value);
            }
        }
        if !tool_images.is_empty() {
            messages.push(json!({"role":"user","content":tool_images}));
        }
        let mut body = json!({"model":request.namespace.model,"messages":messages,"stream":true,"stream_options":{"include_usage":true}});
        if let Some(max) = request.max_output_tokens {
            body["max_tokens"] = json!(max);
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(request.tools.iter().map(|tool| json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.parameters}})).collect::<Vec<_>>());
            body["parallel_tool_calls"] = json!(request.parallel_tool_calls);
        }
        if let Some(effort) = &request.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
            if self.reasoning_content {
                body["thinking"] = json!({"type":if effort == "none" || effort == "off" {"disabled"} else {"enabled"}});
            }
        }
        if let Some(schema) = &request.output_schema {
            body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"codex_output","strict":true,"schema":schema}});
        }
        Ok(WireRequest {
            url: self.url.clone(),
            headers,
            body,
        })
    }
    fn reset(&mut self) {
        self.text.clear();
        self.thinking.clear();
        self.calls.clear();
        self.finish_reason = None;
        self.done = false;
        self.id.clear();
        self.content.clear();
    }
    fn frame(&mut self, _event: &str, data: &str) -> Result<Vec<Event>> {
        if data == "[DONE]" {
            if self.finish_reason.is_none() {
                return Err(protocol("Chat completed without finish_reason"));
            }
            self.done = true;
            return Ok(Vec::new());
        }
        let value: Value =
            serde_json::from_str(data).map_err(|_| protocol("invalid Chat SSE JSON"))?;
        if value.get("error").is_some() {
            return Err(protocol("Chat upstream error"));
        }
        if let Some(id) = value["id"].as_str() {
            self.id = id.to_owned();
        }
        let mut events = Vec::new();
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            events.push(Event::Usage(Usage {
                input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
                output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
                cached_input_tokens: usage["prompt_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0),
                reasoning_tokens: usage["completion_tokens_details"]["reasoning_tokens"]
                    .as_u64()
                    .unwrap_or(0),
            }));
        }
        for choice in value["choices"]
            .as_array()
            .ok_or_else(|| protocol("missing Chat choices"))?
        {
            if choice["index"].as_u64().unwrap_or(0) != 0 {
                return Err(protocol("multiple Chat choices are unsupported"));
            }
            let delta = &choice["delta"];
            if let Some(text) = delta["content"].as_str() {
                self.text.push_str(text);
                self.content.push(Content::Text {
                    text: text.to_owned(),
                });
                events.push(Event::TextDelta(text.to_owned()));
            }
            if let Some(text) = delta["reasoning_content"]
                .as_str()
                .or_else(|| delta["reasoning"].as_str())
            {
                self.thinking.push_str(text);
                self.content.push(Content::Thinking {
                    text: text.to_owned(),
                    signature: None,
                });
                events.push(Event::ThinkingDelta(text.to_owned()));
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                for call in calls {
                    let index = call["index"]
                        .as_u64()
                        .ok_or_else(|| protocol("missing Chat tool index"))?;
                    let entry = self.calls.entry(index).or_default();
                    if let Some(id) = call["id"].as_str() {
                        entry.0.push_str(id);
                    }
                    if let Some(name) = call["function"]["name"].as_str() {
                        entry.1.push_str(name);
                    }
                    if let Some(arguments) = call["function"]["arguments"].as_str() {
                        entry.2.push_str(arguments);
                    }
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.finish_reason = Some(reason.to_owned());
            }
        }
        Ok(events)
    }
    fn finished(&self) -> bool {
        self.done
    }
    fn response_id(&self) -> String {
        self.id.clone()
    }
    fn assistant(&self, _namespace: &Namespace) -> Result<Message> {
        let mut content = self.content.clone();
        for (id, name, arguments) in self.calls.values() {
            if id.is_empty() || !self.declared.contains(name) {
                return Err(protocol("Chat returned missing id or undeclared tool"));
            }
            let arguments: Value = serde_json::from_str(arguments)
                .map_err(|_| protocol("invalid Chat tool arguments JSON"))?;
            if !arguments.is_object() {
                return Err(protocol("Chat tool arguments must be an object"));
            }
            content.push(Content::ToolCall(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments,
            }));
        }
        Ok(Message {
            role: Role::Assistant,
            content,
        })
    }
}
