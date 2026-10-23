//! Private provider account management through the same revision-checked RPCs as
//! other clients. Token input is masked and is never added to chat or history.
use super::CancellationEvent;
use super::bottom_pane_view::BottomPaneView;
use crate::render::renderable::Renderable;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::CancelLoginAccountParams;
use codex_app_server_protocol::CancelLoginAccountResponse;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::LoginAccountParams;
use codex_app_server_protocol::LoginAccountResponse;
use codex_app_server_protocol::ProviderAccountLogoutParams;
use codex_app_server_protocol::ProviderAccountsParams;
use codex_app_server_protocol::ProviderAccountsResponse;
use codex_app_server_protocol::ProviderPoolUpdate;
use codex_app_server_protocol::ProviderPoolUpdateParams;
use codex_app_server_protocol::RequestId;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::oneshot;

enum Reply {
    Accounts(ProviderAccountsResponse),
    Login(LoginAccountResponse),
}
enum Input {
    Label { enroll: bool },
    Token { label: String },
}
pub(crate) struct ProviderManager {
    handle: AppServerRequestHandle,
    thread: Option<String>,
    providers: Vec<&'static codex_provider_registry::Descriptor>,
    provider: Option<usize>,
    selected: usize,
    status: Option<ProviderAccountsResponse>,
    pending: Option<oneshot::Receiver<Result<Reply, String>>>,
    input: Option<Input>,
    text: String,
    message: String,
    login_id: Option<String>,
    login_label: Option<String>,
    login_revision: Option<u64>,
    next_refresh: Instant,
    complete: bool,
}
fn deadline(value: Option<u64>) -> String {
    value
        .and_then(|value| i64::try_from(value).ok())
        .and_then(|value| chrono::DateTime::from_timestamp(value, 0))
        .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "unknown".into())
}
fn request_id() -> RequestId {
    RequestId::String(format!("provider-manager-{}", uuid::Uuid::new_v4()))
}
impl ProviderManager {
    pub(crate) fn new(handle: AppServerRequestHandle, thread: Option<String>) -> Self {
        Self {
            handle,
            thread,
            providers: codex_provider_registry::descriptors().collect(),
            provider: None,
            selected: 0,
            status: None,
            pending: None,
            input: None,
            text: String::new(),
            message: String::new(),
            login_id: None,
            login_label: None,
            login_revision: None,
            next_refresh: Instant::now(),
            complete: false,
        }
    }
    fn provider_id(&self) -> String {
        self.provider
            .map(|index| self.providers[index].id.to_string())
            .unwrap_or_default()
    }
    fn request(&mut self, request: ClientRequest, login: bool) {
        let handle = self.handle.clone();
        let (send, receive) = oneshot::channel();
        self.pending = Some(receive);
        tokio::spawn(async move {
            let result = if login {
                handle
                    .request_typed::<LoginAccountResponse>(request)
                    .await
                    .map(Reply::Login)
            } else {
                handle
                    .request_typed::<ProviderAccountsResponse>(request)
                    .await
                    .map(Reply::Accounts)
            }
            .map_err(|error| error.to_string());
            if let Err(Ok(Reply::Login(LoginAccountResponse::Native {
                login_id: Some(login_id),
                ..
            }))) = send.send(result)
            {
                let _ = handle
                    .request_typed::<CancelLoginAccountResponse>(
                        ClientRequest::CancelLoginAccount {
                            request_id: request_id(),
                            params: CancelLoginAccountParams { login_id },
                        },
                    )
                    .await;
            }
        });
    }
    fn refresh(&mut self) {
        self.request(
            ClientRequest::ProviderAccounts {
                request_id: request_id(),
                params: ProviderAccountsParams {
                    provider: self.provider_id(),
                    thread_id: self.thread.clone(),
                },
            },
            false,
        );
    }
    fn update(&mut self, update: ProviderPoolUpdate) {
        let Some(status) = &self.status else {
            return;
        };
        self.request(
            ClientRequest::ProviderPoolUpdate {
                request_id: request_id(),
                params: ProviderPoolUpdateParams {
                    provider: status.provider.clone(),
                    thread_id: self.thread.clone(),
                    revision: status.revision,
                    update,
                },
            },
            false,
        );
    }
    fn start_login(&mut self, label: String, api_key: Option<String>) {
        self.message = "Starting login… Esc cancels.".into();
        self.login_label = Some(label.clone());
        self.login_revision = self.status.as_ref().map(|status| status.revision);
        self.request(
            ClientRequest::LoginAccount {
                request_id: request_id(),
                params: LoginAccountParams::Native {
                    provider: self.provider_id(),
                    account_id: label,
                    api_key,
                    select: Some(false),
                    thread_id: self.thread.clone(),
                },
            },
            true,
        );
    }
    fn cancel_login(&mut self) {
        self.login_label = None;
        self.login_revision = None;
        if let Some(login_id) = self.login_id.take() {
            let handle = self.handle.clone();
            tokio::spawn(async move {
                let _ = handle
                    .request_typed::<CancelLoginAccountResponse>(
                        ClientRequest::CancelLoginAccount {
                            request_id: request_id(),
                            params: CancelLoginAccountParams { login_id },
                        },
                    )
                    .await;
            });
        }
    }
    fn submit_input(&mut self) {
        if self.text.trim().is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text).trim().to_string();
        match self.input.take() {
            Some(Input::Label { enroll: true }) => {
                self.update(ProviderPoolUpdate::Enroll { label: text })
            }
            Some(Input::Label { enroll: false }) => {
                if self.provider.is_some_and(|index| {
                    self.providers[index].authentication
                        == codex_provider_registry::Authentication::ApiKey
                }) {
                    self.input = Some(Input::Token { label: text });
                } else {
                    self.start_login(text, None);
                }
            }
            Some(Input::Token { label }) => self.start_login(label, Some(text)),
            None => {}
        }
    }
    fn lines(&self) -> Vec<String> {
        let mut lines = vec!["Provider accounts".into()];
        if let Some(index) = self.provider {
            lines.push(self.providers[index].display_name.into());
            if let Some(status) = &self.status {
                lines.push(format!(
                    "Pool {}{} · effective: {} · preferred: {}",
                    if status.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    if status.configured_token {
                        " (configured token bypass)"
                    } else {
                        ""
                    },
                    status.effective_account.as_deref().unwrap_or("none"),
                    status.preferred_account.as_deref().unwrap_or("none")
                ));
                for (index, account) in status
                    .accounts
                    .iter()
                    .enumerate()
                    .skip(self.selected.saturating_sub(3))
                    .take(7)
                {
                    lines.push(format!(
                        "{} {} · {} · {}",
                        if index == self.selected { "›" } else { " " },
                        account.label,
                        if account.enabled {
                            "enabled"
                        } else if account.enrolled {
                            "disabled"
                        } else {
                            "not enrolled"
                        },
                        account.credential_condition
                    ));
                    if index == self.selected {
                        for reason in &account.availability {
                            lines.push(format!(
                                "  {} · reset {} · next attempt {}",
                                reason.reason,
                                deadline(reason.known_reset_at),
                                deadline(reason.next_attempt_at)
                            ));
                        }
                    }
                }
            }
            lines.push(
                "↑↓ account · Space pool on/off · e enable/disable · p prefer · [ ] priority"
                    .into(),
            );
            lines.push("a add login · n enroll saved label · x remove from pool · l log out · r refresh · Esc back".into());
        } else {
            for (index, provider) in self.providers.iter().enumerate() {
                lines.push(format!(
                    "{} {}",
                    if index == self.selected { "›" } else { " " },
                    provider.display_name
                ));
            }
            lines.push("↑↓ choose provider · Enter manage · Esc close".into());
        }
        if let Some(input) = &self.input {
            lines.push(match input {
                Input::Label { .. } => format!("Account label: {}", self.text),
                Input::Token { .. } => {
                    format!("Token: {}", "•".repeat(self.text.chars().count().min(64)))
                }
            });
        }
        if self.pending.is_some() {
            lines.push("Working…".into());
        }
        if !self.message.is_empty() {
            lines.push(self.message.clone());
        }
        lines
    }
}
impl Drop for ProviderManager {
    fn drop(&mut self) {
        self.cancel_login();
        self.text.clear();
    }
}
impl BottomPaneView for ProviderManager {
    fn is_complete(&self) -> bool {
        self.complete
    }
    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.cancel_login();
        self.complete = true;
        CancellationEvent::Handled
    }
    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }
    fn next_frame_delay(&self) -> Option<Duration> {
        self.provider
            .map(|_| Duration::from_millis(if self.pending.is_some() { 100 } else { 1000 }))
    }
    fn pre_draw_tick(&mut self, now: Instant) -> bool {
        if let Some(receiver) = &mut self.pending {
            let result = match receiver.try_recv() {
                Ok(value) => value,
                Err(oneshot::error::TryRecvError::Empty) => return false,
                Err(_) => Err("Provider request interrupted; refresh to check its outcome".into()),
            };
            self.pending = None;
            match result {
                Ok(Reply::Accounts(mut status)) => {
                    status
                        .accounts
                        .sort_by_key(|account| account.priority.unwrap_or(u32::MAX));
                    if self
                        .status
                        .as_ref()
                        .is_some_and(|previous| !previous.enabled)
                        && status.enabled
                    {
                        self.message = "Account pooling activated.".into();
                    }
                    if self.login_id.is_some()
                        && self
                            .login_revision
                            .is_some_and(|revision| revision != status.revision)
                        && status.accounts.iter().any(|account| {
                            Some(&account.label) == self.login_label.as_ref()
                                && account.credential_condition == "Ready"
                        })
                    {
                        self.login_id = None;
                        self.login_label = None;
                        if self.message != "Account pooling activated." {
                            self.message = "Account added.".into();
                        }
                    }
                    self.selected = self.selected.min(status.accounts.len().saturating_sub(1));
                    self.status = Some(status);
                }
                Ok(Reply::Login(LoginAccountResponse::Native {
                    login_id, auth_url, ..
                })) => {
                    self.login_id = login_id;
                    self.message = auth_url.map(|url| format!("Open to finish login: {url}\nEsc cancels. Press r to refresh accounts.")).unwrap_or_else(|| "Account added.".into());
                    if self.login_id.is_none() {
                        self.refresh();
                    }
                }
                Ok(_) => self.message = "Unexpected login response".into(),
                Err(error) => self.message = error,
            }
            self.next_refresh = now + Duration::from_secs(2);
            return true;
        }
        if self.provider.is_some() && self.input.is_none() && now >= self.next_refresh {
            self.refresh();
            self.next_refresh = now + Duration::from_secs(2);
        }
        false
    }
    fn handle_key_event(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.cancel_login();
            self.text.clear();
            if self.input.take().is_some() {
                return;
            }
            if self.provider.take().is_some() {
                self.status = None;
                self.pending = None;
                self.selected = 0;
                self.message.clear();
            } else {
                self.complete = true;
            }
            return;
        }
        if self.input.is_some() {
            match key.code {
                KeyCode::Enter => self.submit_input(),
                KeyCode::Backspace => {
                    self.text.pop();
                }
                KeyCode::Char(c) if !c.is_control() && self.text.len() < 65536 => self.text.push(c),
                _ => {}
            }
            return;
        }
        if self.pending.is_some() {
            return;
        }
        let count = if self.provider.is_some() {
            self.status
                .as_ref()
                .map_or(0, |status| status.accounts.len())
        } else {
            self.providers.len()
        };
        match key.code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                return;
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1));
                return;
            }
            KeyCode::Enter if self.provider.is_none() && count > 0 => {
                self.provider = Some(self.selected);
                self.selected = 0;
                self.refresh();
                return;
            }
            _ => {}
        }
        if self.provider.is_none() {
            return;
        }
        match key.code {
            KeyCode::Char('a') => self.input = Some(Input::Label { enroll: false }),
            KeyCode::Char('n') => self.input = Some(Input::Label { enroll: true }),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char(' ') => {
                if let Some(status) = &self.status {
                    self.update(if status.enabled {
                        ProviderPoolUpdate::Disable
                    } else {
                        ProviderPoolUpdate::Enable
                    });
                }
            }
            _ => {
                let Some(status) = &self.status else {
                    return;
                };
                let Some(account) = status.accounts.get(self.selected) else {
                    return;
                };
                let label = account.label.clone();
                match key.code {
                    KeyCode::Char('e') if !account.enrolled => {
                        self.update(ProviderPoolUpdate::Enroll { label })
                    }
                    KeyCode::Char('e') => self.update(ProviderPoolUpdate::SetAccountEnabled {
                        label,
                        enabled: !account.enabled,
                    }),
                    KeyCode::Char('p') => self.update(ProviderPoolUpdate::Prefer { label }),
                    KeyCode::Char('x') => self.update(ProviderPoolUpdate::Remove { label }),
                    KeyCode::Char('l') => self.request(
                        ClientRequest::ProviderAccountLogout {
                            request_id: request_id(),
                            params: ProviderAccountLogoutParams {
                                provider: status.provider.clone(),
                                account: label,
                                thread_id: self.thread.clone(),
                                revision: status.revision,
                            },
                        },
                        false,
                    ),
                    KeyCode::Char('[' | ']') => {
                        let mut labels: Vec<_> = status
                            .accounts
                            .iter()
                            .filter(|account| account.enrolled)
                            .map(|account| account.label.clone())
                            .collect();
                        if let Some(index) = labels.iter().position(|entry| entry == &label) {
                            let next = if key.code == KeyCode::Char('[') {
                                index.saturating_sub(1)
                            } else {
                                (index + 1).min(labels.len() - 1)
                            };
                            labels.swap(index, next);
                            self.update(ProviderPoolUpdate::Order { labels });
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    fn handle_paste(&mut self, text: String) -> bool {
        if self.input.is_none() {
            return false;
        }
        self.text.extend(
            text.chars()
                .filter(|c| !c.is_control())
                .take(65536usize.saturating_sub(self.text.len())),
        );
        true
    }
}
impl Renderable for ProviderManager {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(self.lines().join("\n"))
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }
    fn desired_height(&self, _width: u16) -> u16 {
        self.lines().len().saturating_add(2).min(24) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_client::RemoteAppServerClient;
    use codex_app_server_client::RemoteAppServerConnectArgs;
    use codex_app_server_client::RemoteAppServerEndpoint;
    use codex_app_server_protocol::JSONRPCMessage;
    use futures::SinkExt;
    use futures::StreamExt;
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message;

    #[tokio::test]
    async fn provider_manager_masks_tokens_and_sends_revision_checked_thread_context() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (requests, mut received) = tokio::sync::mpsc::channel(8);
        let (release_login, login_ready) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut login_ready = Some(login_ready);
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let JSONRPCMessage::Request(request) = serde_json::from_str(&text).unwrap() else {
                    continue;
                };
                let result = if request.method == "initialize" {
                    json!({"userAgent":"pool-test"})
                } else {
                    requests.send(request.clone()).await.unwrap();
                    if request.method == "account/login/start" {
                        login_ready.take().unwrap().await.unwrap();
                        json!({"type":"native", "provider":"fifth", "accountId":"work", "loginId":"owned-login", "authUrl":"https://example.test/login"})
                    } else if request.method == "account/login/cancel" {
                        json!({"status":"canceled"})
                    } else {
                        json!({"provider":"fifth", "endpoint":"test", "revision":8, "enabled":true, "explicitlyEnabled":null, "configuredToken":false, "preferredAccount":"personal", "effectiveAccount":"work", "accounts":[]})
                    }
                };
                socket
                    .send(Message::Text(
                        json!({"id":request.id, "result":result}).to_string().into(),
                    ))
                    .await
                    .unwrap();
            }
        });
        let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
            endpoint: RemoteAppServerEndpoint::WebSocket {
                websocket_url: url,
                auth_token: None,
            },
            client_name: "pool-test".into(),
            client_version: "0.0.0".into(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: vec![],
            channel_capacity: 8,
        })
        .await
        .unwrap();
        let mut manager = ProviderManager::new(
            AppServerRequestHandle::Remote(client.request_handle()),
            Some("thread".into()),
        );
        assert!(!manager.providers.is_empty());
        manager.provider = Some(0);
        manager.status = Some(serde_json::from_value(json!({"provider":"fifth", "endpoint":"test", "revision":7, "enabled":false, "explicitlyEnabled":null, "configuredToken":false, "preferredAccount":"personal", "effectiveAccount":"work", "accounts":[]})).unwrap());
        manager.input = Some(Input::Token {
            label: "private".into(),
        });
        assert!(manager.handle_paste("SECRET_TOKEN\n".into()));
        assert_eq!(manager.text, "SECRET_TOKEN");
        let rendered = manager.lines().join("\n");
        assert!(!rendered.contains("SECRET_TOKEN"));
        assert!(rendered.contains("Token: •"));
        manager.handle_key_event(KeyEvent::from(KeyCode::Esc));
        assert!(manager.text.is_empty() && manager.input.is_none());
        assert!(manager.provider.is_some());
        manager.update(ProviderPoolUpdate::Enable);
        let request = tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request.method, "provider/pool/update");
        assert_eq!(
            request.params.unwrap(),
            json!({"provider":"fifth", "threadId":"thread", "revision":7, "update":{"action":"enable"}})
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while manager.pending.is_some() {
                manager.pre_draw_tick(Instant::now());
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(manager.message, "Account pooling activated.");
        assert!(manager.lines().join("\n").contains("effective: work"));
        manager.start_login("work".into(), None);
        let request = tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request.method, "account/login/start");
        let params = request.params.unwrap();
        assert_eq!(params["select"], false);
        assert_eq!(params["threadId"], "thread");
        // Dismiss while the server is still starting OAuth. The eventual login
        // identifier must be cancelled even though the view never received it.
        drop(manager);
        release_login.send(()).unwrap();
        let request = tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request.method, "account/login/cancel");
        assert_eq!(request.params.unwrap(), json!({"loginId":"owned-login"}));
        server.abort();
    }
}
