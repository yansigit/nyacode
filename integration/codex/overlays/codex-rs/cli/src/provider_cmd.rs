use std::io::IsTerminal;
use std::io::Read;

use clap::Args;
use clap::Subcommand;
use codex_provider_adapter::backend::NativeBackend;
use tokio_util::sync::CancellationToken;

fn provider_choices() -> Vec<&'static str> {
    codex_provider_adapter::registry::descriptors()
        .map(|provider| provider.id)
        .collect()
}

#[derive(Debug, Args)]
pub(crate) struct ProviderCommand {
    #[command(subcommand)]
    action: ProviderAction,
}

#[derive(Debug, Subcommand)]
enum ProviderAction {
    /// Log in to one native provider. Tokens are read from stdin, never arguments.
    Login {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(provider_choices()))]
        provider: String,
        #[arg(long, default_value = "default")]
        account: String,
        #[arg(long)]
        with_token: bool,
        /// Enroll without changing the manually selected account.
        #[arg(long)]
        add: bool,
    },
    /// Log out a named account, or the selected account when omitted.
    Logout {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(provider_choices()))]
        provider: String,
        #[arg(long)]
        account: Option<String>,
    },
    /// List saved accounts, pool membership, and availability.
    Accounts {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(provider_choices()))]
        provider: String,
        #[arg(long)]
        json: bool,
    },
    /// Manage endpoint-scoped native account pools.
    Pool {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(provider_choices()))]
        provider: String,
        #[command(subcommand)]
        action: PoolAction,
    },
    /// Show selected native accounts without exposing credentials.
    Status {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(provider_choices()))]
        provider: Option<String>,
    },
    /// Discover models using the selected provider account.
    Models {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(provider_choices()))]
        provider: String,
    },
}

#[derive(Debug, Subcommand)]
enum PoolAction {
    Status {
        #[arg(long)]
        json: bool,
    },
    Enable,
    Disable,
    Enroll {
        account: String,
    },
    Remove {
        account: String,
    },
    EnableAccount {
        account: String,
    },
    DisableAccount {
        account: String,
    },
    Order {
        #[arg(required = true)]
        accounts: Vec<String>,
    },
    Prefer {
        account: String,
    },
}
fn deadline(value: Option<u64>) -> String {
    value
        .and_then(|value| i64::try_from(value).ok())
        .and_then(|value| chrono::DateTime::from_timestamp(value, 0))
        .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "unknown".into())
}
fn print_accounts(status: &codex_provider_adapter::PoolStatus, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(status)?);
        return Ok(());
    }
    println!(
        "{} pool: {} (revision {}){}",
        status.provider,
        if status.enabled {
            "enabled"
        } else {
            "disabled"
        },
        status.settings.revision,
        if status.configured_token {
            "; bypassed by configured token"
        } else {
            ""
        }
    );
    let mut accounts: Vec<_> = status.accounts.iter().collect();
    accounts.sort_by_key(|entry| entry.priority.unwrap_or(usize::MAX));
    for entry in accounts {
        println!(
            "{}  {}  {:?}{}",
            entry.account.reference.label,
            if entry.enabled {
                "enabled"
            } else if entry.member {
                "disabled"
            } else {
                "not enrolled"
            },
            entry.account.condition,
            if status.selected.as_deref() == Some(&entry.account.reference.label) {
                " (preferred)"
            } else {
                ""
            }
        );
        for restriction in &entry.restrictions {
            println!(
                "  {:?}; reset: {}; next eligible attempt: {}",
                restriction.failure.code,
                deadline(restriction.failure.reset_at),
                deadline(restriction.next_attempt_at)
            );
        }
    }
    Ok(())
}

impl ProviderCommand {
    pub(crate) async fn run(
        self,
        overrides: codex_utils_cli::CliConfigOverrides,
    ) -> anyhow::Result<()> {
        let config = codex_core::config::Config::load_with_cli_overrides(
            overrides.parse_overrides().map_err(anyhow::Error::msg)?,
        )
        .await?;
        let home = config.codex_home;
        let provider_for = |id: &str| -> anyhow::Result<NativeBackend> {
            let info = if config.model_provider.native_provider_id() == Some(id) {
                config.model_provider.clone()
            } else {
                config
                    .model_providers
                    .get(id)
                    .filter(|info| info.native_provider_id() == Some(id))
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("unknown native provider"))?
            };
            Ok(NativeBackend::new(info, Some(home.to_path_buf())))
        };
        let cancellation = CancellationToken::new();
        let signal = cancellation.clone();
        let listener = tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            signal.cancel();
        });
        let result: anyhow::Result<()> = async {
            match self.action {
                ProviderAction::Status { provider } => {
                    let providers = match provider.as_deref() {
                        Some(id) => vec![id],
                        None => provider_choices(),
                    };
                    for id in providers {
                        if provider_for(id)?.has_configured_token() {
                            println!("{id}: configured token (environment or configuration)");
                            continue;
                        }
                        match provider_for(id)?.status()? {
                            Some(status) => println!(
                                "{id}: {}{}",
                                status.account,
                                if status.revoked {
                                    " (login required)"
                                } else {
                                    ""
                                }
                            ),
                            None => println!("{id}: not logged in"),
                        }
                    }
                }
                ProviderAction::Logout { provider, account } => {
                    anyhow::ensure!(!provider_for(&provider)?.has_configured_token(), "remove the configured environment or bearer token override before logging out");
                    let backend = provider_for(&provider)?;
                    if let Some(account) = account { backend.logout_account(&account, &cancellation).await?; } else { backend.logout(&cancellation).await?; }
                    println!("Logged out of {provider}");
                }
                ProviderAction::Login {
                    provider,
                    account,
                    with_token,
                    add,
                } => {
                    anyhow::ensure!(!account.trim().is_empty(), "account name must not be empty");
                    let backend = provider_for(&provider)?;
                    anyhow::ensure!(!backend.has_configured_token(), "remove the configured environment or bearer token override before logging in");
                    if with_token {
                        anyhow::ensure!(
                            !std::io::stdin().is_terminal(),
                            "pipe the token into stdin when using --with-token"
                        );
                        let mut token = String::new();
                        std::io::stdin()
                            .take(1024 * 1024 + 1)
                            .read_to_string(&mut token)?;
                        anyhow::ensure!(token.len() <= 1024 * 1024, "token exceeds 1 MiB");
                        let token = token.trim().to_owned();
                        anyhow::ensure!(!token.is_empty(), "empty token");
                        backend.login_token_with_selection(&account, token, !add, &cancellation).await?;
                    } else {
                        let operation = backend.begin_account_login(&account, &cancellation).await?;
                        let login = backend.begin_login().await?;
                        println!("Open this URL to log in to {provider}:\n{}", login.url());
                        let record = login.finish(&cancellation).await?;
                        backend.finish_account_login(&operation, record, !add, &cancellation).await?;
                    }
                    println!("Logged in to {provider} as {account}");
                }
                ProviderAction::Accounts { provider, json } => {
                    print_accounts(&provider_for(&provider)?.accounts(None, &cancellation).await?, json)?;
                }
                ProviderAction::Pool { provider, action } => {
                    use codex_provider_adapter::PoolUpdate;
                    let backend = provider_for(&provider)?;
                    let status = backend.accounts(None, &cancellation).await?;
                    if let PoolAction::Status { json } = action { print_accounts(&status, json)?; }
                    else {
                        let update = match action {
                            PoolAction::Enable => PoolUpdate::Enable,
                            PoolAction::Disable => PoolUpdate::Disable,
                            PoolAction::Enroll { account } => PoolUpdate::Enroll { label: account },
                            PoolAction::Remove { account } => PoolUpdate::Remove { label: account },
                            PoolAction::EnableAccount { account } => PoolUpdate::SetAccountEnabled { label: account, enabled: true },
                            PoolAction::DisableAccount { account } => PoolUpdate::SetAccountEnabled { label: account, enabled: false },
                            PoolAction::Order { accounts } => PoolUpdate::Order { labels: accounts },
                            PoolAction::Prefer { account } => PoolUpdate::Prefer { label: account },
                            PoolAction::Status { .. } => unreachable!(),
                        };
                        print_accounts(&backend.update_pool(status.settings.revision, update, &cancellation).await?, false)?;
                    }
                }
                ProviderAction::Models { provider } => {
                    let provider = provider_for(&provider)?;
                    let credentials = provider.credentials(cancellation.clone()).await?;
                    let models = provider
                        .provider()?
                        .models(codex_provider_adapter::api::Context {
                            credentials,
                            cancellation,
                        })
                        .await?;
                    for model in models {
                        println!("{}\t{}", model.id, model.name);
                    }
                }
            }
            Ok(())
        }
        .await;
        listener.abort();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderCommand;
    use clap::Args;

    #[test]
    fn provider_pool_cli_preserves_named_account_and_management_actions() {
        let command = ProviderCommand::augment_args(clap::Command::new("provider"));
        for args in [
            vec!["provider", "login", "cursor", "--account", "work", "--add"],
            vec!["provider", "logout", "cursor", "--account", "work"],
            vec!["provider", "accounts", "cursor", "--json"],
            vec!["provider", "pool", "cursor", "status", "--json"],
            vec!["provider", "pool", "cursor", "enable"],
            vec!["provider", "pool", "cursor", "disable"],
            vec!["provider", "pool", "cursor", "enroll", "work"],
            vec!["provider", "pool", "cursor", "remove", "work"],
            vec!["provider", "pool", "cursor", "enable-account", "work"],
            vec!["provider", "pool", "cursor", "disable-account", "work"],
            vec!["provider", "pool", "cursor", "order", "work", "personal"],
            vec!["provider", "pool", "cursor", "prefer", "work"],
        ] {
            assert!(
                command.clone().try_get_matches_from(args.clone()).is_ok(),
                "{args:?}"
            );
        }
        assert!(
            command
                .try_get_matches_from(["provider", "pool", "cursor", "order"])
                .is_err()
        );
    }

    #[test]
    fn provider_cli_parses_each_supported_provider_and_rejects_unknown_values() {
        let command = ProviderCommand::augment_args(clap::Command::new("provider"));
        for provider in ["cursor", "opencode-go", "claude", "antigravity"] {
            assert!(
                command
                    .clone()
                    .try_get_matches_from(["provider", "login", provider])
                    .is_ok(),
                "provider CLI should accept {provider}"
            );
        }
        assert!(
            command
                .try_get_matches_from(["provider", "login", "unsupported-provider"])
                .is_err()
        );
    }
}
