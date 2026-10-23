use crate::ToolOutputPart;
use crate::WireCodec;
use crate::WireRequest;
use crate::protocol;
use crate::string;
use crate::tool_output;
use codex_provider_api::*;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

pub struct ResponsesCodec {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    output: Vec<Value>,
    declared: Vec<String>,
    id: String,
    done: bool,
}
impl ResponsesCodec {
    pub fn new(url: String, headers: BTreeMap<String, String>, tools: &[ToolDefinition]) -> Self {
        Self {
            url,
            headers,
            output: Vec::new(),
            declared: tools.iter().map(|tool| tool.name.clone()).collect(),
            id: String::new(),
            done: false,
        }
    }
}
impl WireCodec for ResponsesCodec {
    fn request(&self, request: &Request, context: &Context) -> Result<WireRequest> {
        let mut headers = self.headers.clone();
        headers.insert(
            "authorization".into(),
            format!("Bearer {}", context.credentials.access_token),
        );
        let mut input = Vec::new();
        for message in &request.messages {
            let opaque = message.content.iter().find_map(|block| match block {
                Content::Opaque { namespace, value } => Some((namespace, value)),
                _ => None,
            });
            if let Some((namespace, value)) = opaque {
                if namespace != &request.namespace {
                    return Err(ProviderError::Checkpoint(
                        "Responses replay namespace mismatch".into(),
                    ));
                }
                input.extend(
                    value["responses_output"]
                        .as_array()
                        .ok_or_else(|| protocol("foreign Responses replay"))?
                        .clone(),
                );
                continue;
            }
            let mut content = Vec::new();
            for block in &message.content {
                match block {
                    Content::Text { text } => content.push(json!({"type":if message.role == Role::Assistant {"output_text"} else {"input_text"},"text":text})),
                    Content::Image { url } => content.push(json!({"type":"input_image","image_url":url})),
                    Content::ToolCall(call) => input.push(json!({"type":"function_call","call_id":call.id,"name":call.name,"arguments":call.arguments.to_string()})),
                    Content::ToolResult(result) => {
                        let output = if result.output.is_array() {
                            json!(tool_output(&result.output)?.into_iter().map(|part| match part {
                                ToolOutputPart::Text(text) => json!({"type":"input_text","text":text}),
                                ToolOutputPart::Image(image) => image,
                            }).collect::<Vec<_>>())
                        } else { json!(crate::output_text(&result.output)) };
                        input.push(json!({"type":"function_call_output","call_id":result.call_id,"output":output}));
                    },
                    Content::Thinking { .. } | Content::Opaque { .. } => {},
                }
            }
            if !content.is_empty() {
                input.push(json!({"type":"message","role":match message.role { Role::Assistant => "assistant", Role::System => "system", _ => "user" },"content":content}));
            }
        }
        let mut body = json!({"model":request.namespace.model,"input":input,"instructions":request.system,"stream":true,"store":false,"include":["reasoning.encrypted_content"],"parallel_tool_calls":request.parallel_tool_calls});
        if !request.tools.is_empty() {
            // Host tools contain optional arguments (including spawn settings).
            // Responses normalizes omitted strict settings differently from Chat;
            // preserve the host's permissive schema instead of requiring every field.
            body["tools"] = json!(request.tools.iter().map(|tool| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters,"strict":false})).collect::<Vec<_>>());
        }
        if let Some(max) = request.max_output_tokens {
            body["max_output_tokens"] = json!(max);
        }
        if let Some(effort) = &request.reasoning_effort {
            body["reasoning"] = json!({"effort":effort});
        }
        if let Some(schema) = &request.output_schema {
            body["text"] = json!({"format":{"type":"json_schema","name":"codex_output","schema":schema,"strict":true}});
        }
        Ok(WireRequest {
            url: self.url.clone(),
            headers,
            body,
        })
    }
    fn reset(&mut self) {
        self.output.clear();
        self.id.clear();
        self.done = false;
    }
    fn frame(&mut self, _event: &str, data: &str) -> Result<Vec<Event>> {
        let value: Value =
            serde_json::from_str(data).map_err(|_| protocol("invalid Responses SSE JSON"))?;
        match value["type"].as_str() {
            Some("response.output_text.delta") => {
                Ok(vec![Event::TextDelta(string(&value, "delta")?)])
            }
            Some("response.reasoning_summary_text.delta") => {
                Ok(vec![Event::ThinkingDelta(string(&value, "delta")?)])
            }
            Some("response.completed") => {
                let response = &value["response"];
                self.id = string(response, "id")?;
                self.output = response["output"]
                    .as_array()
                    .cloned()
                    .ok_or_else(|| protocol("Responses output missing"))?;
                self.done = true;
                let usage = &response["usage"];
                Ok(vec![Event::Usage(Usage {
                    input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
                    output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                    cached_input_tokens: usage["input_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .unwrap_or(0),
                    reasoning_tokens: usage["output_tokens_details"]["reasoning_tokens"]
                        .as_u64()
                        .unwrap_or(0),
                })])
            }
            Some("error" | "response.failed" | "response.incomplete") => {
                Err(protocol("Responses upstream failed or incomplete"))
            }
            Some(_) => Ok(Vec::new()),
            None => Err(protocol("Responses SSE event type missing")),
        }
    }
    fn finished(&self) -> bool {
        self.done
    }
    fn response_id(&self) -> String {
        self.id.clone()
    }
    fn assistant(&self, namespace: &Namespace) -> Result<Message> {
        let mut content = Vec::new();
        for item in &self.output {
            match item["type"].as_str() {
                Some("message") => {
                    for block in item["content"]
                        .as_array()
                        .ok_or_else(|| protocol("Responses message content missing"))?
                    {
                        if block["type"] == "output_text" {
                            content.push(Content::Text {
                                text: string(block, "text")?,
                            });
                        }
                    }
                }
                Some("reasoning") => {
                    if let Some(summary) = item["summary"].as_array() {
                        for block in summary {
                            if let Some(text) = block["text"].as_str() {
                                content.push(Content::Thinking {
                                    text: text.to_owned(),
                                    signature: None,
                                });
                            }
                        }
                    }
                }
                Some("function_call") => {
                    if item.get("encrypted_function_args").is_some_and(|args| {
                        !args.is_null() && args.as_array().is_none_or(|args| !args.is_empty())
                    }) {
                        return Err(ProviderError::Unsupported(
                            "native Responses received encrypted tool arguments".into(),
                        ));
                    }
                    let name = string(item, "name")?;
                    if !self.declared.contains(&name) {
                        return Err(protocol("Responses returned undeclared tool"));
                    }
                    content.push(Content::ToolCall(ToolCall {
                        id: string(item, "call_id")?,
                        name,
                        arguments: serde_json::from_str(&string(item, "arguments")?)
                            .map_err(|_| protocol("invalid Responses tool arguments"))?,
                    }));
                }
                _ => return Err(ProviderError::Unsupported("Responses output item".into())),
            }
        }
        content.push(Content::Opaque {
            namespace: namespace.clone(),
            value: json!({"responses_output":self.output}),
        });
        Ok(Message {
            role: Role::Assistant,
            content,
        })
    }
}
