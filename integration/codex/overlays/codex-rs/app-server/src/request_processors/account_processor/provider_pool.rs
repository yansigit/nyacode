use super::*;
use codex_app_server_protocol::ProviderAccountAvailability;
use codex_app_server_protocol::ProviderAccountLogoutParams;
use codex_app_server_protocol::ProviderAccountStatus;
use codex_app_server_protocol::ProviderAccountsParams;
use codex_app_server_protocol::ProviderAccountsResponse;
use codex_app_server_protocol::ProviderPoolChangedNotification;
use codex_app_server_protocol::ProviderPoolUpdate;
use codex_app_server_protocol::ProviderPoolUpdateParams;
use codex_provider_adapter::backend::NativeBackend;

impl AccountRequestProcessor {
    pub(super) async fn provider_backend(
        &self,
        provider: &str,
        thread_id: Option<&str>,
    ) -> Result<NativeBackend, JSONRPCErrorError> {
        let config = match thread_id {
            Some(thread) => {
                let thread = codex_protocol::ThreadId::from_string(thread)
                    .map_err(|_| invalid_request("invalid thread id"))?;
                self.thread_manager
                    .get_thread(thread)
                    .await
                    .map_err(|_| invalid_request("thread not found"))?
                    .config()
                    .await
                    .as_ref()
                    .clone()
            }
            None => self.load_latest_config().await,
        };
        let info = if config.model_provider.native_provider_id() == Some(provider) {
            config.model_provider.clone()
        } else {
            config
                .model_providers
                .get(provider)
                .filter(|info| info.native_provider_id() == Some(provider))
                .cloned()
                .ok_or_else(|| invalid_request("unknown native provider"))?
        };
        Ok(NativeBackend::new(
            info,
            Some(config.codex_home.to_path_buf()),
        ))
    }
    pub(crate) async fn provider_accounts(
        &self,
        params: ProviderAccountsParams,
    ) -> Result<ProviderAccountsResponse, JSONRPCErrorError> {
        let backend = self
            .provider_backend(&params.provider, params.thread_id.as_deref())
            .await?;
        backend
            .accounts(params.thread_id.as_deref(), &CancellationToken::new())
            .await
            .map(response)
            .map_err(|error| internal_error(error.to_string()))
    }
    pub(crate) async fn provider_pool_update(
        &self,
        params: ProviderPoolUpdateParams,
    ) -> Result<ProviderAccountsResponse, JSONRPCErrorError> {
        use codex_provider_adapter::PoolUpdate as Update;
        let backend = self
            .provider_backend(&params.provider, params.thread_id.as_deref())
            .await?;
        let update = match params.update {
            ProviderPoolUpdate::Enable => Update::Enable,
            ProviderPoolUpdate::Disable => Update::Disable,
            ProviderPoolUpdate::Enroll { label } => Update::Enroll { label },
            ProviderPoolUpdate::Remove { label } => Update::Remove { label },
            ProviderPoolUpdate::SetAccountEnabled { label, enabled } => {
                Update::SetAccountEnabled { label, enabled }
            }
            ProviderPoolUpdate::Order { labels } => Update::Order { labels },
            ProviderPoolUpdate::Prefer { label } => Update::Prefer { label },
        };
        let status = backend
            .update_pool(params.revision, update, &CancellationToken::new())
            .await
            .map_err(|error| invalid_request(error.to_string()))?;
        self.provider_pool_changed(&status).await;
        let status = if params.thread_id.is_some() {
            backend
                .accounts(params.thread_id.as_deref(), &CancellationToken::new())
                .await
                .map_err(|error| internal_error(error.to_string()))?
        } else {
            status
        };
        Ok(response(status))
    }
    pub(crate) async fn provider_account_logout(
        &self,
        params: ProviderAccountLogoutParams,
    ) -> Result<ProviderAccountsResponse, JSONRPCErrorError> {
        let backend = self
            .provider_backend(&params.provider, params.thread_id.as_deref())
            .await?;
        let cancellation = CancellationToken::new();
        let status = backend
            .accounts(params.thread_id.as_deref(), &cancellation)
            .await
            .map_err(|error| internal_error(error.to_string()))?;
        if status.settings.revision != params.revision {
            return Err(invalid_request(
                "provider pool changed; reload before logging out",
            ));
        }
        backend
            .logout_account_at_revision(&params.account, params.revision, &cancellation)
            .await
            .map_err(|error| internal_error(error.to_string()))?;
        let status = backend
            .accounts(params.thread_id.as_deref(), &cancellation)
            .await
            .map_err(|error| internal_error(error.to_string()))?;
        self.provider_pool_changed(&status).await;
        Ok(response(status))
    }
    pub(super) async fn provider_pool_changed(&self, status: &codex_provider_adapter::PoolStatus) {
        self.outgoing
            .send_server_notification(ServerNotification::ProviderPoolChanged(
                ProviderPoolChangedNotification {
                    provider: status.provider.clone(),
                    endpoint: status.endpoint.clone(),
                    revision: status.settings.revision,
                },
            ))
            .await;
    }
}
fn response(status: codex_provider_adapter::PoolStatus) -> ProviderAccountsResponse {
    ProviderAccountsResponse {
        provider: status.provider,
        endpoint: status.endpoint,
        revision: status.settings.revision,
        enabled: status.enabled,
        explicitly_enabled: status.settings.preference,
        configured_token: status.configured_token,
        preferred_account: status.selected,
        effective_account: status.effective_account.map(|account| account.label),
        accounts: status
            .accounts
            .into_iter()
            .map(|account| ProviderAccountStatus {
                label: account.account.reference.label,
                enrolled: account.member,
                enabled: account.enabled,
                priority: account
                    .priority
                    .and_then(|priority| priority.try_into().ok()),
                credential_condition: format!("{:?}", account.account.condition),
                availability: account
                    .restrictions
                    .into_iter()
                    .map(|restriction| ProviderAccountAvailability {
                        reason: format!("{:?}", restriction.failure.code),
                        model: match restriction.failure.scope {
                            codex_provider_adapter::api::FailureScope::Model(model) => Some(model),
                            _ => None,
                        },
                        known_reset_at: restriction.failure.reset_at,
                        next_attempt_at: restriction.next_attempt_at,
                    })
                    .collect(),
            })
            .collect(),
    }
}
