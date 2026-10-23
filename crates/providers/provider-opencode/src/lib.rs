//! Native OpenCode Go HTTP provider. Route families come from the pinned provider catalog.
mod failure;
use codex_provider_api::*;
use codex_provider_codecs::*;
use reqwest::Client;
use serde_json::Value;
use std::collections::BTreeMap;

pub struct OpenCodeProvider {
    client: Client,
    base_url: String,
}
impl Default for OpenCodeProvider {
    fn default() -> Self {
        Self::new(Client::new())
    }
}
impl OpenCodeProvider {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            base_url: "https://opencode.ai/zen/go".into(),
        }
    }
    pub fn with_base_url(client: Client, base_url: String) -> Result<Self> {
        let url = url::Url::parse(&base_url)
            .map_err(|_| ProviderError::Protocol("invalid OpenCode endpoint".into()))?;
        if url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1")))
        {
            return Err(ProviderError::Protocol(
                "OpenCode endpoint must use HTTPS".into(),
            ));
        }
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }
    fn catalog() -> Result<Vec<Value>> {
        serde_json::from_str(include_str!("catalog.json"))
            .map_err(|_| ProviderError::Protocol("invalid OpenCode catalog".into()))
    }
}
impl Provider for OpenCodeProvider {
    fn id(&self) -> &'static str {
        "opencode-go"
    }
    fn handoff_request(
        &self,
        request: &Request,
        _boundary: ContinuationBoundary,
    ) -> Result<Request> {
        portable_history(request)
    }
    fn models(&self, context: Context) -> ProviderFuture<'_, Vec<Model>> {
        Box::pin(async move {
            if context.cancellation.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
            // This is a source-pinned routing catalog, not an entitlement check.
            Ok(Self::catalog()?
                .into_iter()
                .map(|model| Model {
                    id: model["id"].as_str().unwrap_or_default().into(),
                    name: model["name"].as_str().unwrap_or_default().into(),
                    context_window: model["context_window"].as_u64(),
                    supports_images: model["supports_images"].as_bool(),
                })
                .collect())
        })
    }
    fn start(
        &self,
        request: Request,
        context: Context,
    ) -> ProviderFuture<'_, Box<dyn ProviderSession>> {
        Box::pin(async move {
            if request.namespace.provider != self.id() {
                return Err(ProviderError::Protocol(
                    "OpenCode provider namespace mismatch".into(),
                ));
            }
            let model = Self::catalog()?
                .into_iter()
                .find(|model| model["id"] == request.namespace.model)
                .ok_or_else(|| {
                    ProviderError::Unsupported(format!(
                        "OpenCode Go model {} has no verified route",
                        request.namespace.model
                    ))
                })?;
            let mut headers = BTreeMap::new();
            headers.insert(
                "x-opencode-session".into(),
                request.namespace.thread.clone(),
            );
            let codec: Box<dyn WireCodec> = match model["api"].as_str() {
                Some("anthropic-messages") => Box::new(MessagesCodec::new(
                    format!("{}/v1/messages", self.base_url),
                    headers,
                    false,
                    &request.tools,
                )?),
                Some("openai-completions") => Box::new(ChatCodec::new(
                    format!("{}/v1/chat/completions", self.base_url),
                    headers,
                    model["reasoning_content"] == true,
                    &request.tools,
                )),
                Some("openai-responses") => Box::new(ResponsesCodec::new(
                    format!("{}/v1/responses", self.base_url),
                    headers,
                    &request.tools,
                )),
                _ => {
                    return Err(ProviderError::Unsupported(
                        "OpenCode model wire family".into(),
                    ));
                }
            };
            Ok(Box::new(
                HttpSession::start_with_policy(
                    self.client.clone(),
                    codec,
                    request,
                    context,
                    failure::classify,
                )
                .await?,
            ) as Box<dyn ProviderSession>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::body_partial_json;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    #[tokio::test]
    async fn catalog_preserves_model_specific_image_capabilities() {
        let models = OpenCodeProvider::default()
            .models(Context {
                credentials: Credentials {
                    access_token: "fixture-key".into(),
                    account_id: "account-a".into(),
                    metadata: json!({}),
                },
                cancellation: Default::default(),
            })
            .await
            .unwrap();
        assert!(models.iter().all(|model| model.supports_images.is_some()));
        for (id, supports_images) in [
            ("glm-5.3", false),
            ("deepseek-v4-flash", false),
            ("deepseek-v4-flash-vision-exp", true),
            ("minimax-m3", true),
            ("gpt-6-luna", true),
        ] {
            assert_eq!(
                models
                    .iter()
                    .find(|model| model.id == id)
                    .unwrap()
                    .supports_images,
                Some(supports_images),
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn verified_model_families_use_native_v1_paths_and_session_header() {
        for (model, endpoint, key_header, key_value) in [
            (
                "glm-5.3",
                "/v1/chat/completions",
                "authorization",
                "Bearer fixture-key",
            ),
            ("minimax-m3", "/v1/messages", "x-api-key", "fixture-key"),
            (
                "gpt-6-luna",
                "/v1/responses",
                "authorization",
                "Bearer fixture-key",
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(endpoint))
                .and(header(key_header, key_value))
                .and(header("x-opencode-session", "thread-a"))
                .and(body_partial_json(json!({"model":model,"stream":true})))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
            let provider = OpenCodeProvider::with_base_url(Client::new(), server.uri()).unwrap();
            let context = Context {
                credentials: Credentials {
                    access_token: "fixture-key".into(),
                    account_id: "account-a".into(),
                    metadata: json!({}),
                },
                cancellation: Default::default(),
            };
            let request = Request {
                namespace: Namespace {
                    provider: "opencode-go".into(),
                    account: "account-a".into(),
                    model: model.into(),
                    thread: "thread-a".into(),
                },
                system: String::new(),
                messages: vec![Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "hello".into(),
                    }],
                }],
                tools: vec![],
                parallel_tool_calls: true,
                max_output_tokens: Some(128),
                reasoning_effort: None,
                output_schema: None,
                checkpoint: None,
                metadata: json!({}),
            };
            drop(provider.start(request, context).await.unwrap());
            server.verify().await;
        }
    }
}
