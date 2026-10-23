use std::time::Duration;

use codex_provider_api::ProviderError;
use codex_provider_api::Result;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::LoginFlow;
use crate::OAuthConfig;
use crate::TokenRecord;

pub struct LoopbackLogin {
    pub auth_url: String,
    flow: LoginFlow,
    listener: TcpListener,
}
impl LoopbackLogin {
    pub async fn begin(config: OAuthConfig) -> Result<Self> {
        let redirect = url::Url::parse(&config.redirect_uri)
            .map_err(|_| ProviderError::Authentication("invalid login redirect".into()))?;
        if redirect.scheme() != "http"
            || !matches!(redirect.host_str(), Some("localhost" | "127.0.0.1"))
        {
            return Err(ProviderError::Authentication(
                "login requires an IPv4 loopback redirect".into(),
            ));
        }
        let port = redirect
            .port()
            .ok_or_else(|| ProviderError::Authentication("login redirect port missing".into()))?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|_| {
                ProviderError::Authentication(format!(
                    "unable to bind login callback on port {port}"
                ))
            })?;
        let (flow, auth_url) = LoginFlow::begin(config)?;
        Ok(Self {
            auth_url,
            flow,
            listener,
        })
    }
    pub async fn finish(self, cancellation: &CancellationToken) -> Result<TokenRecord> {
        tokio::select! {
            _ = cancellation.cancelled() => Err(ProviderError::Cancelled),
            result = tokio::time::timeout(Duration::from_secs(600), self.receive(cancellation)) => result.map_err(|_| ProviderError::Authentication("provider login timed out".into()))?,
        }
    }
    async fn receive(self, cancellation: &CancellationToken) -> Result<TokenRecord> {
        let (mut socket, _) = self
            .listener
            .accept()
            .await
            .map_err(|_| ProviderError::Authentication("login listener failed".into()))?;
        let mut buffer = Vec::new();
        loop {
            if buffer.len() >= 8192 {
                return Err(ProviderError::Authentication(
                    "login callback too large".into(),
                ));
            }
            let mut chunk = [0u8; 1024];
            let count = tokio::time::timeout(Duration::from_secs(10), socket.read(&mut chunk))
                .await
                .map_err(|_| ProviderError::Authentication("login callback timed out".into()))?
                .map_err(|_| ProviderError::Authentication("login callback failed".into()))?;
            if count == 0 {
                return Err(ProviderError::Authentication(
                    "incomplete login callback".into(),
                ));
            }
            buffer.extend_from_slice(&chunk[..count]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let request = std::str::from_utf8(&buffer)
            .map_err(|_| ProviderError::Authentication("invalid login callback".into()))?;
        let line = request.lines().next().unwrap_or_default();
        let mut fields = line.split_whitespace();
        if fields.next() != Some("GET") {
            return Err(ProviderError::Authentication(
                "login callback must use GET".into(),
            ));
        }
        let path = fields
            .next()
            .filter(|path| path.starts_with('/') && !path.starts_with("//"))
            .ok_or_else(|| ProviderError::Authentication("invalid login callback target".into()))?;
        let redirect = url::Url::parse(self.flow.redirect_uri())
            .map_err(|_| ProviderError::Authentication("invalid redirect".into()))?;
        let callback = format!("{}{path}", redirect.origin().ascii_serialization());
        let result = self.flow.finish(&callback, cancellation).await;
        let status = if result.is_ok() {
            "200 OK"
        } else {
            "400 Bad Request"
        };
        let body = if result.is_ok() {
            "Authorization received. Return to Codex."
        } else {
            "Authorization failed. Return to Codex and retry."
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        result
    }
}
