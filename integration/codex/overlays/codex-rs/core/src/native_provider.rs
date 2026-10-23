//! Bridge from private core prompt state to the compiled backend adapter.
use std::sync::Arc;

use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::openai_models::ModelInfo;
use codex_provider_adapter::backend::NativeBackend;

use crate::client_common::Prompt;
use crate::client_common::ResponseStream;

pub(crate) use codex_provider_adapter::session::tool_result;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream(
    backend: Arc<NativeBackend>,
    prompt: &Prompt,
    model: &ModelInfo,
    thread: String,
    reasoning_effort: Option<String>,
    checkpoint_scope: String,
    execution_purpose: &str,
    persistence_acknowledgements: bool,
) -> Result<ResponseStream> {
    let capabilities = backend.session_capabilities();
    let prompt = codex_provider_adapter::session::Prompt {
        input: prompt.input.clone(),
        tools: prompt
            .tools
            .iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<_, _>>()
            .map_err(|error| CodexErr::Stream(error.to_string()))?,
        system: prompt.base_instructions.text.clone(),
        parallel_tool_calls: prompt.parallel_tool_calls,
        output_schema: prompt.output_schema.clone(),
        persistence_acknowledgements,
    };
    let stream = codex_provider_adapter::session::stream(
        backend,
        &prompt,
        &model.slug,
        thread,
        reasoning_effort,
        checkpoint_scope,
        execution_purpose,
    )
    .await?;
    let (results, rx_event, consumer_dropped) = stream.into_parts();
    Ok(ResponseStream {
        session_control: Some(codex_provider_adapter::SessionControl {
            results,
            capabilities,
        }),
        rx_event,
        interrupt: None,
        consumer_dropped,
    })
}
