//! One acceptance contract for embedded and trusted local socket connections.
//! The socket path is never interpreted as a URL or used for a TCP connection.

use codex_app_server::in_process::InProcessClientHandle;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_protocol as rpc;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::Path;

pub(super) enum Client {
    Embedded(InProcessClientHandle),
    Socket(RemoteAppServerClient),
}

impl Client {
    pub(super) async fn socket(path: &Path, expected_home: &Path) -> Result<Self, String> {
        let socket_path = AbsolutePathBuf::from_absolute_path_checked(path)
            .map_err(|_| "rpc_socket_must_be_absolute")?;
        let args = RemoteAppServerConnectArgs {
            endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
            client_name: "nyacode-chat-rpc-probe".into(),
            client_version: "1".into(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: Vec::new(),
            channel_capacity: 128,
        };
        #[cfg(windows)]
        let client = RemoteAppServerClient::connect_local_daemon(args).await;
        #[cfg(not(windows))]
        let client = RemoteAppServerClient::connect(args).await;
        let client = client.map_err(|_| "rpc_socket_connect_failed")?;
        // The database audit must inspect the same application's home. Never
        // issue Chat reads or writes to an unrelated local daemon.
        let expected = expected_home
            .canonicalize()
            .map_err(|_| "rpc_home_unavailable")?;
        let actual = client
            .codex_home()
            .and_then(|home| Path::new(home).canonicalize().ok());
        if actual.as_deref() != Some(expected.as_path()) {
            let _ = client.shutdown().await;
            return Err("rpc_daemon_home_mismatch".into());
        }
        Ok(Self::Socket(client))
    }

    pub(super) async fn request(
        &self,
        request: rpc::ClientRequest,
    ) -> std::io::Result<Result<serde_json::Value, rpc::JSONRPCErrorError>> {
        match self {
            Self::Embedded(client) => client.request(request).await,
            Self::Socket(client) => client.request(request).await,
        }
    }

    pub(super) async fn next_event(&mut self) -> Option<AppServerEvent> {
        match self {
            Self::Embedded(client) => client.next_event().await.map(Into::into),
            Self::Socket(client) => client.next_event().await,
        }
    }

    pub(super) async fn shutdown(self) -> std::io::Result<()> {
        match self {
            Self::Embedded(client) => client.shutdown().await,
            Self::Socket(client) => client.shutdown().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relative_paths_and_websocket_urls_never_connect() {
        for value in [
            "relative.sock",
            "ws://localhost/rpc",
            "wss://example.org/rpc",
        ] {
            let result = Client::socket(Path::new(value), Path::new("/")).await;
            assert!(matches!(result, Err(ref error) if error == "rpc_socket_must_be_absolute"));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn another_daemon_home_is_rejected_before_any_chat_request() {
        use futures::SinkExt;
        use futures::StreamExt;
        use tokio_tungstenite::tungstenite::Message;
        let directory = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let other_path = other.path().to_path_buf();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = tokio_tungstenite::accept_async(stream).await.unwrap();
            let message = stream.next().await.unwrap().unwrap().into_text().unwrap();
            let initialize: rpc::JSONRPCRequest = serde_json::from_str(&message).unwrap();
            assert_eq!(initialize.method, "initialize");
            let response = serde_json::json!({"jsonrpc":"2.0","id":initialize.id,"result":{"userAgent":"fixture/1","codexHome":other_path}});
            stream
                .send(Message::Text(response.to_string().into()))
                .await
                .unwrap();
            while let Some(Ok(message)) = stream.next().await {
                match message {
                    Message::Text(text) => {
                        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                        assert_eq!(value["method"], "initialized");
                        assert!(value.get("id").is_none());
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });
        let result = Client::socket(&path, directory.path()).await;
        assert!(matches!(result, Err(ref error) if error == "rpc_daemon_home_mismatch"));
        server.await.unwrap();
    }
}
