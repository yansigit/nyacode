//! Native Cloud Code Assist transport for Antigravity. Project onboarding is an RPC,
//! not a delegated `agy` process. All signed parts stay with their exact assistant turn.
mod failure;
use codex_provider_api::*;
use codex_provider_auth::Store;
use codex_provider_auth::StoreLock;
use codex_provider_codecs::*;
use reqwest::Client;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::Mutex;

pub const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const CLIENT_ID_ENV: &str = "ANTIGRAVITY_OAUTH_CLIENT_ID";
// Public desktop OAuth registration, observed in the pinned Antigravity clients.
pub const CLIENT_SECRET_ENV: &str = "ANTIGRAVITY_OAUTH_CLIENT_SECRET";
pub const REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";
pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

#[derive(Clone, Default, Serialize, Deserialize)]
struct ProjectState {
    project: Option<String>,
    pending: bool,
    operation: Option<String>,
}
fn persist_project(
    state: &ProjectState,
    account: &str,
    projects: &mut BTreeMap<String, ProjectState>,
    lock: Option<&StoreLock>,
) -> Result<()> {
    if let Some(lock) = lock {
        lock.write(state)?;
    }
    projects.insert(account.to_owned(), state.clone());
    Ok(())
}

pub struct AntigravityProvider {
    client: Client,
    base_url: String,
    projects: Mutex<BTreeMap<String, ProjectState>>,
    state_store: Option<Store>,
}
impl Default for AntigravityProvider {
    fn default() -> Self {
        Self::new(Client::new())
    }
}
impl AntigravityProvider {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            base_url: "https://daily-cloudcode-pa.googleapis.com".into(),
            projects: Mutex::new(BTreeMap::new()),
            state_store: None,
        }
    }
    pub fn with_base_url(client: Client, base_url: String) -> Result<Self> {
        let url = url::Url::parse(&base_url)
            .map_err(|_| ProviderError::Protocol("invalid Antigravity endpoint".into()))?;
        if url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1")))
        {
            return Err(ProviderError::Protocol(
                "Antigravity endpoint must use HTTPS".into(),
            ));
        }
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').into(),
            projects: Mutex::new(BTreeMap::new()),
            state_store: None,
        })
    }
    /// Persist provisioning fences and operation names across process restarts.
    /// Without a state home, onboarding recovery is limited to this instance.
    pub fn with_state_home(mut self, home: PathBuf) -> Self {
        self.state_store = Some(Store::new(&home));
        self
    }
    async fn rpc(&self, name: &str, body: Value, context: &Context) -> Result<Value> {
        let request = self
            .client
            .post(format!("{}/v1internal:{name}", self.base_url))
            .bearer_auth(&context.credentials.access_token)
            .header("user-agent", "antigravity-hub/2.2.1")
            .json(&body);
        self.read_request(request, context).await
    }
    async fn read_request(
        &self,
        request: reqwest::RequestBuilder,
        context: &Context,
    ) -> Result<Value> {
        let response = tokio::select! {biased;_=context.cancellation.cancelled()=>return Err(ProviderError::Cancelled),response=tokio::time::timeout(Duration::from_secs(30),request.send())=>response.map_err(|_|ProviderError::Transport("Antigravity RPC timed out".into()))?.map_err(|_|ProviderError::Transport("Antigravity RPC failed".into()))?};
        bounded_json(response, context).await
    }
    /// Resolve an account project and expose it for shared credential metadata persistence.
    // The async mutex serializes project discovery and the side-effectful onboarding
    // POST. Releasing it during network waits could onboard the same account twice.
    // All waits are bounded and cancellation-aware; no blocking mutex is involved.
    #[allow(clippy::await_holding_invalid_type)]
    pub async fn discover_project(&self, context: &Context) -> Result<String> {
        if let Some(project) = context.credentials.metadata["project_id"]
            .as_str()
            .filter(|project| !project.is_empty())
        {
            return Ok(project.to_owned());
        }
        let mut projects = tokio::select! {biased;_=context.cancellation.cancelled()=>return Err(ProviderError::Cancelled),projects=self.projects.lock()=>projects};
        let account = &context.credentials.account_id;
        let key = json!(("antigravity-project", &self.base_url, account)).to_string();
        let lock = match &self.state_store {
            Some(store) => Some(store.lock(&key, &context.cancellation).await?),
            None => None,
        };
        let mut state = match &lock {
            Some(lock) => lock.read::<ProjectState>()?.unwrap_or_default(),
            None => projects.get(account).cloned().unwrap_or_default(),
        };
        if let Some(project) = &state.project {
            return Ok(project.clone());
        }
        // Recheck provisioning before recovering an operation, including after
        // an ambiguous POST whose response could not be durably recorded.
        let load = self
            .rpc(
                "loadCodeAssist",
                json!({"metadata":{"ideType":"ANTIGRAVITY"}}),
                context,
            )
            .await?;
        let project = if let Some(project) = extract_project(&load) {
            project
        } else {
            let tier = load["allowedTiers"]
                .as_array()
                .and_then(|tiers| tiers.iter().find(|tier| tier["isDefault"] == true))
                .and_then(|tier| tier["id"].as_str())
                .or_else(|| load["currentTier"]["id"].as_str())
                .unwrap_or("free-tier");
            let mut operation = if let Some(name) = &state.operation {
                json!({"name":name,"done":false})
            } else {
                if state.pending {
                    return Err(ProviderError::Checkpoint("Antigravity onboarding has an unknown outcome; wait for provisioning and retry discovery; refusing another onboarding request".into()));
                }
                state.pending = true;
                persist_project(&state, account, &mut projects, lock.as_ref())?;
                let request=self.client.post(format!("{}/v1internal:onboardUser",self.base_url)).bearer_auth(&context.credentials.access_token).header("user-agent","google-api-nodejs-client/10.3.0").header("x-goog-api-client","gl-node/22.21.1").json(&json!({"tier_id":tier,"metadata":{"ide_type":"ANTIGRAVITY","ide_name":"antigravity","ide_version":"2.2.1"}}));
                let operation = self.read_request(request, context).await?;
                if operation["done"] != true {
                    let name = operation["name"].as_str().ok_or_else(|| {
                        ProviderError::Protocol("onboarding operation name missing".into())
                    })?;
                    validate_operation_name(name)?;
                    state.operation = Some(name.to_owned());
                    persist_project(&state, account, &mut projects, lock.as_ref())?;
                }
                operation
            };
            let end = tokio::time::Instant::now() + Duration::from_secs(300);
            loop {
                if operation.get("error").is_some() {
                    return Err(ProviderError::Authentication(
                        "Antigravity onboarding rejected".into(),
                    ));
                }
                if operation["done"] == true {
                    break extract_project(&operation["response"])
                        .or_else(|| extract_project(&operation))
                        .ok_or_else(|| {
                            ProviderError::Protocol("onboarding completed without a project".into())
                        })?;
                }
                let name = operation["name"]
                    .as_str()
                    .ok_or_else(|| {
                        ProviderError::Protocol("onboarding operation name missing".into())
                    })?
                    .to_owned();
                validate_operation_name(&name)?;
                if tokio::time::Instant::now() >= end {
                    return Err(ProviderError::Transport(
                        "Antigravity onboarding timeout".into(),
                    ));
                }
                tokio::select! {biased;_=context.cancellation.cancelled()=>return Err(ProviderError::Cancelled),_=tokio::time::sleep(Duration::from_secs(5))=>{}};
                // Poll the single operation. Never repeat the onboarding POST.
                operation = self
                    .read_request(
                        self.client
                            .get(format!("{}/v1internal/{name}", self.base_url))
                            .bearer_auth(&context.credentials.access_token)
                            .header("user-agent", "google-api-nodejs-client/10.3.0")
                            .header("x-goog-api-client", "gl-node/22.21.1"),
                        context,
                    )
                    .await?;
            }
        };
        state.project = Some(project.clone());
        state.pending = false;
        state.operation = None;
        persist_project(&state, account, &mut projects, lock.as_ref())?;
        Ok(project)
    }
}
fn validate_operation_name(name: &str) -> Result<()> {
    if !name.starts_with("operations/")
        || name.contains("..")
        || name.contains('?')
        || name.contains('#')
    {
        return Err(ProviderError::Protocol(
            "invalid onboarding operation name".into(),
        ));
    }
    Ok(())
}
fn extract_project(value: &Value) -> Option<String> {
    [
        "cloudaicompanionProject",
        "companionProject",
        "projectId",
        "project",
    ]
    .iter()
    .find_map(|key| {
        value[*key]
            .as_str()
            .or_else(|| value[*key]["id"].as_str())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    })
}
impl Provider for AntigravityProvider {
    fn id(&self) -> &'static str {
        "antigravity"
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
            self.discover_project(&context).await?;
            let body = self
                .rpc("fetchAvailableModels", json!({}), &context)
                .await?;
            let catalog = body["models"].as_object().ok_or_else(|| {
                ProviderError::Protocol("Antigravity catalog models missing".into())
            })?;
            if catalog.len() > 500 {
                return Err(ProviderError::Protocol(
                    "Antigravity catalog exceeds 500 models".into(),
                ));
            }
            Ok(catalog
                .iter()
                .map(|(id, model)| Model {
                    id: id.clone(),
                    name: model["displayName"]
                        .as_str()
                        .or_else(|| model["name"].as_str())
                        .unwrap_or(id)
                        .into(),
                    context_window: model["maxInputTokens"].as_u64(),
                    supports_images: None,
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
                    "Antigravity provider namespace mismatch".into(),
                ));
            }
            let project = self.discover_project(&context).await?;
            let headers = BTreeMap::from([(
                "user-agent".into(),
                "antigravity/ide/2.5.5 (os_type=windows; arch=amd64; aidev_client; auth_method=oauth)"
                    .into(),
            )]);
            let codec = GoogleCodec::new(
                format!("{}/v1internal:streamGenerateContent?alt=sse", self.base_url),
                headers,
                project,
                &request.tools,
            );
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
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::body_json;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    fn context() -> Context {
        Context {
            credentials: Credentials {
                access_token: "test-access".into(),
                account_id: "account-a".into(),
                metadata: json!({}),
            },
            cancellation: Default::default(),
        }
    }
    #[tokio::test]
    async fn project_discovery_onboards_once_and_discovers_real_catalog() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .and(header("authorization", "Bearer test-access"))
            .and(body_json(json!({"metadata":{"ideType":"ANTIGRAVITY"}})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"allowedTiers":[{"id":"source-default","isDefault":true}]}),
                ),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1internal:onboardUser"))
            .and(wiremock::matchers::body_partial_json(
                json!({"tier_id":"source-default"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"done":true,"response":{"cloudaicompanionProject":{"id":"project-a"}}}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/v1internal:fetchAvailableModels")).and(body_json(json!({}))).respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":{"gemini-test":{"displayName":"Gemini Test","maxInputTokens":12345}}}))).expect(1).mount(&server).await;
        let provider = AntigravityProvider::with_base_url(Client::new(), server.uri()).unwrap();
        let context = context();
        let (first, concurrent) = tokio::join!(
            provider.discover_project(&context),
            provider.discover_project(&context)
        );
        assert_eq!(first.unwrap(), "project-a");
        assert_eq!(concurrent.unwrap(), "project-a");
        let models = provider.models(context).await.unwrap();
        assert_eq!(
            models,
            vec![Model {
                id: "gemini-test".into(),
                name: "Gemini Test".into(),
                context_window: Some(12345),
                supports_images: None,
            }]
        );
        server.verify().await;
    }
    #[tokio::test]
    async fn supplied_project_avoids_control_plane_requests() {
        let server = MockServer::start().await;
        let provider = AntigravityProvider::with_base_url(Client::new(), server.uri()).unwrap();
        let mut ctx = context();
        ctx.credentials.metadata = json!({"project_id":"supplied-project"});
        assert_eq!(
            provider.discover_project(&ctx).await.unwrap(),
            "supplied-project"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }
    #[test]
    fn project_extraction_accepts_observed_string_and_object_shapes() {
        assert_eq!(
            extract_project(&json!({"cloudaicompanionProject":"p"})),
            Some("p".into())
        );
        assert_eq!(
            extract_project(&json!({"project":{"id":"p"}})),
            Some("p".into())
        );
        assert_eq!(extract_project(&json!({"project":" "})), None);
    }
    async fn wait_for_pending_state(home: &std::path::Path, base_url: &str, operation: bool) {
        let key = json!(("antigravity-project", base_url, "account-a")).to_string();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if Store::new(home)
                    .read::<ProjectState>(&key)
                    .unwrap()
                    .is_some_and(|state| state.pending && state.operation.is_some() == operation)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cancelled_onboarding_operation_resumes_after_provider_restart_without_another_post() {
        let server = MockServer::start().await;
        let home = tempfile::tempdir().unwrap();
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"allowedTiers":[{"id":"free-tier","isDefault":true}]})),
            )
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1internal:onboardUser"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"name":"operations/provision-test","done":false})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1internal/operations/provision-test"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"done":true,"response":{"project":"project-after-restart"}}),
                ),
            )
            .expect(1)
            .mount(&server)
            .await;
        let provider = std::sync::Arc::new(
            AntigravityProvider::with_base_url(Client::new(), server.uri())
                .unwrap()
                .with_state_home(home.path().to_owned()),
        );
        let ctx = context();
        let task = {
            let provider = provider.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move { provider.discover_project(&ctx).await })
        };
        wait_for_pending_state(home.path(), &server.uri(), true).await;
        ctx.cancellation.cancel();
        assert!(matches!(task.await.unwrap(), Err(ProviderError::Cancelled)));
        drop(provider);
        let restarted = AntigravityProvider::with_base_url(Client::new(), server.uri())
            .unwrap()
            .with_state_home(home.path().to_owned());
        assert_eq!(
            restarted.discover_project(&context()).await.unwrap(),
            "project-after-restart"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn ambiguous_onboarding_post_is_fenced_across_restart_until_project_exists() {
        let server = MockServer::start().await;
        let home = tempfile::tempdir().unwrap();
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1internal:onboardUser"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(10))
                    .set_body_json(json!({"name":"operations/unobserved","done":false})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let provider = std::sync::Arc::new(
            AntigravityProvider::with_base_url(Client::new(), server.uri())
                .unwrap()
                .with_state_home(home.path().to_owned()),
        );
        let ctx = context();
        let task = {
            let provider = provider.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move { provider.discover_project(&ctx).await })
        };
        wait_for_pending_state(home.path(), &server.uri(), false).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.url.path() == "/v1internal:onboardUser")
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        ctx.cancellation.cancel();
        assert!(matches!(task.await.unwrap(), Err(ProviderError::Cancelled)));
        drop(provider);
        let restarted = AntigravityProvider::with_base_url(Client::new(), server.uri())
            .unwrap()
            .with_state_home(home.path().to_owned());
        assert!(
            matches!(restarted.discover_project(&context()).await, Err(ProviderError::Checkpoint(message)) if message.contains("unknown outcome"))
        );
        server.verify().await;
        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"project":"provisioning-completed"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            restarted.discover_project(&context()).await.unwrap(),
            "provisioning-completed"
        );
        let key = json!(("antigravity-project", server.uri(), "account-a")).to_string();
        let state = Store::new(home.path())
            .read::<ProjectState>(&key)
            .unwrap()
            .unwrap();
        assert!(!state.pending);
        assert!(state.operation.is_none());
        server.verify().await;
    }
}
