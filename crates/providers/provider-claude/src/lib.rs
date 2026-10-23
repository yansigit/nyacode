//! Direct Claude Pro/Max OAuth Messages transport. OAuth lifecycle lives in shared auth.
mod failure;
use codex_provider_api::*;
use codex_provider_codecs::*;
use reqwest::Client;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Mutex;

pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const REDIRECT_URI: &str = "http://localhost:53692/callback";
pub const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

type CapabilityKey = (String, String, String);
static CAPABILITIES: OnceLock<Mutex<BTreeMap<CapabilityKey, (Instant, Value)>>> = OnceLock::new();

pub struct ClaudeProvider {
    client: Client,
    base_url: String,
}
impl Default for ClaudeProvider {
    fn default() -> Self {
        Self::new(Client::new())
    }
}
impl ClaudeProvider {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            base_url: "https://api.anthropic.com".into(),
        }
    }
    pub fn with_base_url(client: Client, base_url: String) -> Result<Self> {
        let url = url::Url::parse(&base_url)
            .map_err(|_| ProviderError::Protocol("invalid Claude endpoint".into()))?;
        if url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1")))
        {
            return Err(ProviderError::Protocol(
                "Claude endpoint must use HTTPS".into(),
            ));
        }
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').into(),
        })
    }
    async fn capabilities(&self, model: &str, context: &Context) -> Result<Value> {
        let key = (
            self.base_url.clone(),
            context.credentials.account_id.clone(),
            model.to_owned(),
        );
        let cache = CAPABILITIES.get_or_init(|| Mutex::new(BTreeMap::new()));
        {
            let cache = tokio::select! { biased; _ = context.cancellation.cancelled() => return Err(ProviderError::Cancelled), cache = cache.lock() => cache };
            if let Some((_, value)) = cache
                .get(&key)
                .filter(|(fetched, _)| fetched.elapsed() < Duration::from_secs(600))
            {
                return Ok(value.clone());
            }
        }
        let mut url = url::Url::parse(&format!("{}/v1/models/", self.base_url))
            .map_err(|_| ProviderError::Protocol("invalid Claude model endpoint".into()))?;
        url.path_segments_mut()
            .map_err(|_| ProviderError::Protocol("invalid Claude model endpoint".into()))?
            .pop_if_empty()
            .push(model);
        let mut builder = self
            .client
            .get(url)
            .bearer_auth(&context.credentials.access_token)
            .header("anthropic-version", "2023-06-01");
        for (key, value) in Self::headers() {
            builder = builder.header(key, value);
        }
        let response = tokio::select! { biased; _ = context.cancellation.cancelled() => return Err(ProviderError::Cancelled),
        response = tokio::time::timeout(Duration::from_secs(30), builder.send()) => response.map_err(|_| ProviderError::Transport("Claude capability discovery timeout".into()))?.map_err(|_| ProviderError::Transport("Claude capability discovery failed".into()))? };
        let model = bounded_json(response, context).await?;
        let capabilities = model["capabilities"].clone();
        let mut cache = tokio::select! { biased; _ = context.cancellation.cancelled() => return Err(ProviderError::Cancelled), cache = cache.lock() => cache };
        // Another request may have populated this entry while discovery was in flight.
        if let Some((_, value)) = cache
            .get(&key)
            .filter(|(fetched, _)| fetched.elapsed() < Duration::from_secs(600))
        {
            return Ok(value.clone());
        }
        if cache.len() >= 256
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (fetched, _))| *fetched)
                .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(key, (Instant::now(), capabilities.clone()));
        Ok(capabilities)
    }
    fn headers() -> BTreeMap<String, String> {
        BTreeMap::from([("anthropic-beta".into(),"claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14".into()),("user-agent".into(),"claude-cli/2.1.280".into()),("x-app".into(),"cli".into())])
    }
}
impl Provider for ClaudeProvider {
    fn id(&self) -> &'static str {
        "claude"
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
            let mut models = Vec::new();
            let mut after = None::<String>;
            let mut seen_cursors = BTreeSet::new();
            let mut pages = 0;
            loop {
                pages += 1;
                if pages > 32 {
                    return Err(ProviderError::Protocol(
                        "Claude model catalog exceeds pagination limit".into(),
                    ));
                }
                let mut builder = self
                    .client
                    .get(format!("{}/v1/models", self.base_url))
                    .bearer_auth(&context.credentials.access_token)
                    .header("anthropic-version", "2023-06-01")
                    .query(&[("limit", "100")]);
                for (key, value) in Self::headers() {
                    builder = builder.header(key, value);
                }
                if let Some(after) = &after {
                    builder = builder.query(&[("after_id", after)]);
                }
                let response = tokio::select! { biased; _=context.cancellation.cancelled()=>return Err(ProviderError::Cancelled),response=tokio::time::timeout(Duration::from_secs(30), builder.send())=>response.map_err(|_|ProviderError::Transport("Claude model discovery timeout".into()))?.map_err(|_|ProviderError::Transport("Claude model discovery failed".into()))?};
                let body = bounded_json(response, &context).await?;
                let data = body["data"]
                    .as_array()
                    .ok_or_else(|| ProviderError::Protocol("Claude models data missing".into()))?;
                for model in data {
                    let id = model["id"]
                        .as_str()
                        .ok_or_else(|| ProviderError::Protocol("Claude model id missing".into()))?;
                    models.push(Model {
                        id: id.into(),
                        name: model["display_name"].as_str().unwrap_or(id).into(),
                        context_window: None,
                        supports_images: None,
                    });
                    if models.len() > 500 {
                        return Err(ProviderError::Protocol(
                            "Claude model catalog exceeds 500 models".into(),
                        ));
                    }
                }
                if body["has_more"] != true {
                    break;
                }
                let cursor = body["last_id"].as_str().ok_or_else(|| {
                    ProviderError::Protocol("Claude pagination cursor missing".into())
                })?;
                if !seen_cursors.insert(cursor.to_owned()) {
                    return Err(ProviderError::Protocol(
                        "Claude repeated pagination cursor".into(),
                    ));
                }
                after = Some(cursor.into());
            }
            Ok(models)
        })
    }
    fn start(
        &self,
        mut request: Request,
        context: Context,
    ) -> ProviderFuture<'_, Box<dyn ProviderSession>> {
        Box::pin(async move {
            if request.namespace.provider != self.id() {
                return Err(ProviderError::Protocol(
                    "Claude provider namespace mismatch".into(),
                ));
            }
            if request
                .reasoning_effort
                .as_deref()
                .is_some_and(|effort| effort != "none" && effort != "off")
            {
                let capabilities = self
                    .capabilities(&request.namespace.model, &context)
                    .await?;
                request.metadata["anthropic_adaptive_thinking"] = serde_json::json!(
                    capabilities["thinking"]["types"]["adaptive"]["supported"] == true
                );
                if let Some(effort) = request.reasoning_effort.as_ref()
                    && request.metadata["anthropic_adaptive_thinking"] == true
                    && capabilities["effort"][effort]["supported"] != true
                {
                    return Err(ProviderError::Unsupported(format!(
                        "Claude model does not support reasoning effort {effort}"
                    )));
                }
            }
            let codec = MessagesCodec::new(
                format!("{}/v1/messages", self.base_url),
                Self::headers(),
                true,
                &request.tools,
            )?;
            Ok(Box::new(
                HttpSession::start_with_policy(
                    self.client.clone(),
                    Box::new(codec),
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
    use wiremock::matchers::query_param;
    #[tokio::test]
    async fn slow_capability_discovery_does_not_block_another_model() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models/slow"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(300))
                    .set_body_json(json!({"capabilities":{"slow":true}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models/fast"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"capabilities":{"fast":true}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let provider = ClaudeProvider::with_base_url(Client::new(), server.uri()).unwrap();
        let context = Context {
            credentials: Credentials {
                access_token: "fixture-token".into(),
                account_id: "fixture-account".into(),
                metadata: json!({}),
            },
            cancellation: Default::default(),
        };
        let (slow, fast) = tokio::join!(provider.capabilities("slow", &context), async {
            tokio::time::sleep(Duration::from_millis(25)).await;
            tokio::time::timeout(
                Duration::from_millis(150),
                provider.capabilities("fast", &context),
            )
            .await
            .unwrap()
        });
        assert_eq!(slow.unwrap(), json!({"slow":true}));
        assert_eq!(fast.unwrap(), json!({"fast":true}));
        server.verify().await;
    }
    #[tokio::test]
    async fn adaptive_thinking_uses_discovered_model_capability() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/v1/models/claude-test")).and(header("authorization","Bearer fixture-token")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"capabilities":{"thinking":{"types":{"adaptive":{"supported":true}}},"effort":{"high":{"supported":true}}}}))).expect(1).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_partial_json(
                json!({"thinking":{"type":"adaptive"},"output_config":{"effort":"high"}}),
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(2)
            .mount(&server)
            .await;
        let provider = ClaudeProvider::with_base_url(Client::new(), server.uri()).unwrap();
        let context = Context {
            credentials: Credentials {
                access_token: "fixture-token".into(),
                account_id: "account-a".into(),
                metadata: json!({}),
            },
            cancellation: Default::default(),
        };
        let request = Request {
            namespace: Namespace {
                provider: "claude".into(),
                account: "account-a".into(),
                model: "claude-test".into(),
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
            reasoning_effort: Some("high".into()),
            output_schema: None,
            checkpoint: None,
            metadata: json!({}),
        };
        drop(
            provider
                .start(request.clone(), context.clone())
                .await
                .unwrap(),
        );
        drop(provider.start(request, context).await.unwrap());
        server.verify().await;
    }
    #[tokio::test]
    async fn model_discovery_rejects_alternating_empty_pagination_cursors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":[],"has_more":true,"last_id":"a"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("after_id", "a"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":[],"has_more":true,"last_id":"b"})),
            )
            .with_priority(1)
            .mount(&server)
            .await;
        let context = Context {
            credentials: Credentials {
                access_token: "fixture-token".into(),
                account_id: "account-a".into(),
                metadata: json!({}),
            },
            cancellation: Default::default(),
        };
        let provider = ClaudeProvider::with_base_url(Client::new(), server.uri()).unwrap();
        assert!(
            matches!(provider.models(context).await, Err(ProviderError::Protocol(message)) if message.contains("repeated pagination cursor"))
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }
}
