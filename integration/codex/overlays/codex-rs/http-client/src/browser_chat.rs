//! Isolated browser-profile transport for native Chat research.
//!
//! Ordinary Codex clients never select this backend. Redirects, transport
//! retries, proxy fallback and request diagnostics are deliberately unavailable.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use http::HeaderMap;
use http::header::COOKIE;
use http::header::USER_AGENT;
use reqwest::ResponseBuilderExt;

use crate::HttpClient;
use crate::HttpClientFactory;
use crate::HttpError;
use crate::HttpResponse;
use crate::OutboundProxyRoute;
use crate::client::HttpClientBackend;
use crate::request_draft::HeaderUpdate;
use crate::request_draft::RequestDraft;

pub const CHAT_BROWSER_PROFILE: &str = "chrome_136_linux";
pub const CHAT_BROWSER_TRANSPORT: &str = "wreq=6.0.0-rc.29;wreq-util=3.0.0-rc.14";
const MAX_ROUTES: usize = 16;

/// A single origin/session's connection pool. The owner must drop it on identity
/// change. No Chat bearer credentials, account cookies or preparation tokens are
/// held here; proxy credentials remain part of the resolved proxy route.
pub(crate) struct BrowserChatClient {
    factory: HttpClientFactory,
    origin: String,
    headers: HeaderMap,
    clients: tokio::sync::Mutex<HashMap<OutboundProxyRoute, wreq::Client>>,
}

impl std::fmt::Debug for BrowserChatClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserChatClient")
            .field("profile", &CHAT_BROWSER_PROFILE)
            .finish_non_exhaustive()
    }
}

impl HttpClientFactory {
    /// Constructs the Chat-only Chrome 136 transport. `origin` must be an HTTPS
    /// origin with no credentials, path, query or fragment. Certificate/proxy
    /// construction failures are surfaced on send, without backend fallback.
    pub fn browser_chat_client(
        &self,
        origin: &str,
        mut headers: HeaderMap,
    ) -> Result<HttpClient, HttpError> {
        let url = reqwest::Url::parse(origin).map_err(|_| build_error("invalid Chat origin"))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(build_error("Chat transport requires an HTTPS origin"));
        }
        // The profile owns all UA/client-hint values. Preserve residency and
        // workspace routing headers supplied by the application.
        headers.remove(USER_AGENT);
        let hint_names: Vec<_> = headers
            .keys()
            .filter(|name| name.as_str().starts_with("sec-ch-ua"))
            .cloned()
            .collect();
        for name in hint_names {
            headers.remove(name);
        }
        for name in [
            "authorization",
            "cookie",
            "chatgpt-account-id",
            "proxy-authorization",
            "x-conduit-token",
        ] {
            if headers.contains_key(name) {
                return Err(build_error("Chat credentials cannot be pool defaults"));
            }
        }
        if headers
            .keys()
            .any(|name| name.as_str().starts_with("openai-sentinel-"))
        {
            return Err(build_error(
                "Chat preparation tokens cannot be pool defaults",
            ));
        }
        if self.has_chatgpt_cookies() {
            // Never import externally supplied session/challenge cookies. The
            // infrastructure allowlist remains available through the factory.
            return Err(build_error(
                "Chat transport does not accept configured cookies",
            ));
        }
        Ok(HttpClient {
            backend: HttpClientBackend::BrowserChat(Arc::new(BrowserChatClient {
                factory: self
                    .clone()
                    .with_network_policy(self.network_policy().clone().for_current_account()),
                origin: url.origin().ascii_serialization(),
                headers,
                clients: tokio::sync::Mutex::new(HashMap::new()),
            })),
        })
    }
}

impl BrowserChatClient {
    pub(crate) async fn send(&self, draft: RequestDraft) -> Result<HttpResponse, HttpError> {
        let url = draft.request.url();
        if url.origin().ascii_serialization() != self.origin
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(build_error(
                "Chat request does not match its session origin",
            ));
        }
        let permit = self.factory.network_policy().acquire(url)?;
        let timeout = draft
            .request
            .timeout()
            .copied()
            .unwrap_or(Duration::from_secs(30));
        let response = permit
            .run(async {
                tokio::time::timeout(timeout, self.send_authorized(draft))
                    .await
                    .map_err(|_| HttpError::Timeout)?
            })
            .await??;
        // The adapter retains the existing HttpResponse body/stream APIs. The
        // outer permit also revokes reads after response headers have arrived.
        Ok(HttpResponse::new(response, permit).without_error_urls())
    }

    async fn send_authorized(&self, draft: RequestDraft) -> Result<reqwest::Response, HttpError> {
        let RequestDraft { request, headers } = draft;
        let url = request.url().clone();
        let route = self
            .factory
            .resolve_browser_chat_proxy_route(url.as_str())
            .await
            .map_err(|_| build_error("Chat proxy resolution failed"))?;
        let cached = self.clients.lock().await.get(&route).cloned();
        let client = match cached {
            Some(client) => client,
            None => {
                let build_route = route.clone();
                let client = tokio::task::spawn_blocking(move || build_client(&build_route))
                    .await
                    .map_err(|_| build_error("Chat transport construction task failed"))??;
                let mut clients = self.clients.lock().await;
                // Concurrent first requests may build in parallel, but must
                // select the same retained pool once construction finishes.
                if !clients.contains_key(&route) && clients.len() >= MAX_ROUTES {
                    clients.clear();
                }
                clients.entry(route).or_insert(client).clone()
            }
        };
        let mut merged = HeaderMap::new();
        for update in headers {
            match update {
                HeaderUpdate::Replace(headers) => merged.extend(headers),
                HeaderUpdate::Append(name, value) => {
                    merged.append(name, value);
                }
            }
        }
        if merged.contains_key(USER_AGENT)
            || merged
                .keys()
                .any(|name| name.as_str().starts_with("sec-ch-ua"))
        {
            return Err(build_error(
                "Chat requests cannot override the browser profile",
            ));
        }
        crate::client::apply_default_headers(&mut merged, &self.headers);
        let uri = url
            .as_str()
            .parse()
            .map_err(|_| build_error("invalid Chat request URL"))?;
        if !merged.contains_key(COOKIE)
            && let Some(cookie) = self.factory.chatgpt_cookie_header(&uri)
        {
            merged.insert(COOKIE, cookie);
        }
        let mut outgoing = client
            .request(request.method().clone(), url.as_str())
            .headers(merged)
            .timeout(
                request
                    .timeout()
                    .copied()
                    .unwrap_or(Duration::from_secs(30)),
            );
        if let Some(body) = request.body() {
            // Chat uploads use bounded immutable snapshots. Do not silently
            // buffer an arbitrary/unbounded producer supplied by another caller.
            let bytes = body
                .as_bytes()
                .ok_or_else(|| build_error("Chat requires a bounded request body"))?;
            outgoing = outgoing.body(bytes.to_vec());
        }
        let response = outgoing.send().await.map_err(map_error)?;
        self.factory
            .store_chatgpt_response_cookies(&uri, response.headers());
        let status = response.status();
        let headers = response.headers().clone();
        let version = response.version();
        let body = response.bytes_stream().map(|chunk| {
            chunk.map_err(|error| {
                // reqwest preserves io::ErrorKind::TimedOut through wrapped
                // bodies. Keep the category without retaining secret-bearing
                // native URLs, headers, or error sources.
                let kind = if error.is_timeout() {
                    std::io::ErrorKind::TimedOut
                } else {
                    std::io::ErrorKind::Other
                };
                std::io::Error::new(kind, "Chat response stream failed")
            })
        });
        let mut adapted = http::Response::builder()
            .url(url)
            .body(reqwest::Body::wrap_stream(body))
            .map_err(|_| build_error("failed to adapt Chat response"))?;
        *adapted.status_mut() = status;
        *adapted.headers_mut() = headers;
        *adapted.version_mut() = version;
        Ok(reqwest::Response::from(adapted))
    }
}

fn build_client(route: &OutboundProxyRoute) -> Result<wreq::Client, HttpError> {
    build_client_with_roots(route, crate::custom_ca::browser_chat_cert_store()?)
}

fn build_client_with_roots(
    route: &OutboundProxyRoute,
    roots: wreq::tls::trust::CertStore,
) -> Result<wreq::Client, HttpError> {
    let emulation = wreq_util::Emulation::builder()
        .profile(wreq_util::Profile::Chrome136)
        .platform(wreq_util::Platform::Linux)
        .build();
    let mut builder = wreq::Client::builder()
        .emulation(emulation)
        .redirect(wreq::redirect::Policy::none())
        .retry(wreq::retry::Policy::never())
        .no_proxy()
        .connect_timeout(Duration::from_secs(30))
        .tls_cert_store(roots);
    match route {
        OutboundProxyRoute::Direct => {}
        OutboundProxyRoute::Proxy { url, no_proxy } => {
            let parsed = reqwest::Url::parse(url)
                .map_err(|_| build_error("unsupported Chat proxy configuration"))?;
            if !matches!(
                parsed.scheme(),
                "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
            ) || parsed.host_str().is_none()
            {
                return Err(build_error("unsupported Chat proxy configuration"));
            }
            let proxy = wreq::Proxy::all(url)
                .map_err(|_| build_error("unsupported Chat proxy configuration"))?
                .no_proxy(no_proxy.as_deref().and_then(wreq::NoProxy::from_string));
            builder = builder.proxy(proxy);
        }
        OutboundProxyRoute::TransportDefault => {
            return Err(build_error("unresolved Chat proxy configuration"));
        }
    }
    builder
        .build()
        .map_err(|_| build_error("Chat TLS transport construction failed"))
}

fn map_error(error: wreq::Error) -> HttpError {
    // Never expose native errors: they may contain URLs, proxy credentials or
    // signed storage query parameters. No retry is implied by any category.
    if error.is_timeout() {
        HttpError::Timeout
    } else if error.is_connect() {
        build_error("Chat TLS or proxy connection failed")
    } else {
        build_error("Chat HTTP transport failed")
    }
}

fn build_error(message: &'static str) -> HttpError {
    HttpError::Build(message.to_owned())
}

#[cfg(test)]
#[path = "browser_chat_tests.rs"]
mod tests;
