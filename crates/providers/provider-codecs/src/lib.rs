//! Direct HTTP codecs. No SDK, proxy, or tool executor is involved here.
mod chat;
mod google;
mod messages;
mod replay;
mod responses;
mod session;
mod sse;
pub use chat::ChatCodec;
use codex_provider_api::ProviderError;
use codex_provider_api::Result;
pub use google::GoogleCodec;
pub use messages::MessagesCodec;
pub use replay::portable_history;
pub use replay::restore_checkpoint;
pub use responses::ResponsesCodec;
use serde_json::Value;
pub use session::HttpSession;
pub use session::WireCodec;
pub use session::WireRequest;
pub use session::bounded_json;
pub use session::classify_response;
pub use sse::SseDecoder;

pub(crate) fn protocol(message: impl Into<String>) -> ProviderError {
    ProviderError::Protocol(message.into())
}
pub(crate) fn string(value: &Value, key: &str) -> Result<String> {
    value[key]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| protocol(format!("missing {key}")))
}
pub(crate) fn output_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

/// Core tool outputs use Responses-compatible content items for multimodal results.
/// Keep them structured; an unsupported item must not become JSON prose.
pub(crate) enum ToolOutputPart {
    Text(String),
    Image(Value),
}
pub(crate) fn tool_output(value: &Value) -> Result<Vec<ToolOutputPart>> {
    let Some(items) = value.as_array() else {
        return Ok(vec![ToolOutputPart::Text(output_text(value))]);
    };
    items
        .iter()
        .map(|item| match item["type"].as_str() {
            Some("input_text") => Ok(ToolOutputPart::Text(string(item, "text")?)),
            Some("input_image") => {
                let url = item["image_url"].as_str();
                let file = item["file_id"].as_str();
                if url.is_some() == file.is_some() {
                    return Err(protocol(
                        "tool image requires exactly one image_url or file_id",
                    ));
                }
                Ok(ToolOutputPart::Image(item.clone()))
            }
            _ => Err(ProviderError::Unsupported(
                "tool output content item type".into(),
            )),
        })
        .collect()
}
pub(crate) fn tool_image_url(image: &Value) -> Result<&str> {
    image["image_url"].as_str().ok_or_else(|| {
        ProviderError::Unsupported("this protocol cannot resolve a tool image file_id".into())
    })
}
pub(crate) fn image_source(url: &str) -> Result<Value> {
    if let Some(data) = url.strip_prefix("data:") {
        let (mime, data) = data
            .split_once(";base64,")
            .ok_or_else(|| protocol("image must be a base64 data URL"))?;
        Ok(serde_json::json!({"type":"base64","media_type":mime,"data":data}))
    } else {
        Ok(serde_json::json!({"type":"url","url":url}))
    }
}

#[cfg(test)]
mod tests;
