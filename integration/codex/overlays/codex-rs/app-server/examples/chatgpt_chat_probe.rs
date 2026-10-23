//! Explicitly invoked native ChatGPT compatibility research; never starts the agent loop.
use clap::ArgGroup;
use clap::Parser;
use codex_app_server::in_process::EmbeddedNetworkPolicy;
use codex_chatgpt_chat::Capability;
use codex_chatgpt_chat::ChatError;
use codex_chatgpt_chat::ChatProbe;
use codex_chatgpt_chat::PreparationProtocol;
use codex_chatgpt_chat::ProbeReport;
use codex_chatgpt_chat::ProbeTransport;
use codex_chatgpt_chat::WriteScenario;
use codex_chatgpt_chat::journal::RunJournal;
use codex_chatgpt_chat::stream::Acceptance;
use codex_chatgpt_chat::stream::GenerationOutcome;
use codex_core::config::ConfigBuilder;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_utils_cli::CliConfigOverrides;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

#[path = "chatgpt_chat_probe/fixtures.rs"]
mod fixtures;
#[path = "chatgpt_chat_probe/rpc.rs"]
mod rpc_probe;
#[path = "chatgpt_chat_probe/service.rs"]
mod service;

#[derive(Parser)]
#[command(
    about = "Report native ChatGPT compatibility. Exit 2 means this probe does not establish full release acceptance."
)]
#[command(group(ArgGroup::new("account").args(["current_account", "account_id"]).multiple(false)))]
struct Args {
    /// Make read-only requests (unless --write-scenario is explicitly selected).
    #[arg(long, requires = "account")]
    live: bool,
    /// Designate the currently selected managed account for this run.
    #[arg(long, requires = "live")]
    current_account: bool,
    /// Require this selected account/workspace ID; does not switch accounts.
    #[arg(long, requires = "live")]
    account_id: Option<String>,
    /// Optional saved conversation to validate. No message contents are printed.
    #[arg(long, requires = "live")]
    conversation_id: Option<String>,
    /// Select native write acceptance; stops at any unverified prerequisite.
    #[arg(
        long,
        requires = "live",
        conflicts_with = "conversation_id",
        value_name = "SCENARIO",
        long_help = "Select native write acceptance: preparation, saved-text, continuation, restart-resume, temporary, attachments, all. The harness stops at any unverified prerequisite and reports dependent scenarios as blocked. No existing personal conversation is selected for writes."
    )]
    write_scenario: Option<WriteScenario>,
    /// Explicit transport selection for a comparison run; never a fallback.
    #[arg(long, requires = "live", value_name = "standard|browser-emulated")]
    transport: Option<ProbeTransport>,
    /// Explicit preparation revision; two-phase-v1 requires browser-emulated transport.
    #[arg(
        long,
        requires = "write_scenario",
        value_name = "diagnostic|two-phase-v1"
    )]
    preparation_protocol: Option<PreparationProtocol>,
    /// Resume only this private saved-run journal; never replay its pending POST.
    #[arg(long, requires = "write_scenario", conflicts_with = "conversation_id")]
    resume_run: Option<String>,
    /// Explicit temporary branch refresh candidate; never saves or resends.
    #[arg(
        long,
        requires = "write_scenario",
        value_name = "history-read|conduit-refresh-v1"
    )]
    temporary_protocol: Option<codex_chatgpt_chat::TemporaryProtocol>,
    /// Directory containing the five generated, digest-verified upload fixtures.
    #[arg(long, requires = "write_scenario")]
    attachment_fixtures: Option<PathBuf>,
    /// Exercise the app-server saved service and SQLite journal with a new fixture.
    #[arg(long, requires = "write_scenario", conflicts_with_all = ["resume_run", "temporary_protocol", "attachment_fixtures"])]
    service_saved: bool,
    /// Exercise native catalog/history through embedded app-server; never sends.
    #[arg(long, group = "rpc_mode", requires = "live", conflicts_with_all = ["service_saved", "write_scenario", "conversation_id", "rpc_saved", "rpc_temporary"])]
    rpc_read: bool,
    /// Exercise one saved fixture through embedded RPCs, streaming, and retrieval.
    #[arg(long, group = "rpc_mode", requires = "write_scenario", conflicts_with_all = ["service_saved", "resume_run", "temporary_protocol", "attachment_fixtures", "rpc_read", "rpc_temporary"])]
    rpc_saved: bool,
    /// Continue only an empty saved draft from this harness's completed fixture.
    /// The identifier stays in the private runner journal, never the report.
    #[arg(long, requires = "rpc_saved")]
    rpc_resume_draft: Option<uuid::Uuid>,
    /// Exercise two temporary turns through embedded RPCs without a saved journal.
    #[arg(long, group = "rpc_mode", requires = "write_scenario", conflicts_with_all = ["service_saved", "resume_run", "attachment_fixtures", "rpc_read", "rpc_saved"])]
    rpc_temporary: bool,
    /// Exercise all five upload formats and saved follow-ups through embedded RPCs.
    #[arg(long, group = "rpc_mode", requires_all = ["write_scenario", "attachment_fixtures"], conflicts_with_all = ["service_saved", "resume_run", "temporary_protocol", "rpc_read", "rpc_saved", "rpc_temporary"])]
    rpc_attachments: bool,
    /// Cancel and revoke new saved fixtures; toggles this test server's runtime Chat feature.
    #[arg(long, group = "rpc_mode", requires = "write_scenario", conflicts_with_all = ["service_saved", "resume_run", "temporary_protocol", "attachment_fixtures"])]
    rpc_lifecycle: bool,
    /// Use the ordinary local daemon's absolute Unix socket instead of embedding.
    #[arg(long, requires = "rpc_mode")]
    rpc_socket: Option<PathBuf>,
    #[arg(long)]
    codex_home: Option<PathBuf>,
    #[command(flatten)]
    config: CliConfigOverrides,
}

fn main() -> ExitCode {
    // Debug builds of the complete embedded server have large configuration
    // futures. Keep this explicit diagnostic runtime independent of the
    // caller's RUST_MIN_STACK setting; production Chat still uses its host.
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()
    else {
        eprintln!("Unable to start the compatibility probe runtime");
        return ExitCode::FAILURE;
    };
    runtime.block_on(main_async())
}

async fn main_async() -> ExitCode {
    match run(Args::parse()).await {
        Ok(report) => match serde_json::to_string_pretty(&report) {
            Ok(json) => {
                println!("{json}");
                ExitCode::from(2)
            }
            Err(_) => ExitCode::FAILURE,
        },
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<ProbeReport, &'static str> {
    let mut report = ProbeReport {
        preparation_protocol: args.preparation_protocol.unwrap_or_default(),
        ..ProbeReport::default()
    };
    if let Some(transport) = args.transport {
        report.select_transport(transport);
    }
    if !args.live {
        return Ok(report);
    }
    let overrides = args
        .config
        .parse_overrides()
        .map_err(|_| "Invalid configuration overrides")?;
    let mut builder = ConfigBuilder::default()
        .without_project_context()
        .cli_overrides(overrides.clone());
    if let Some(home) = args.codex_home {
        builder = builder.codex_home(home);
    }
    let mut config = builder
        .build()
        .await
        .map_err(|_| "Unable to load managed configuration")?;
    if !config.features.enabled(Feature::ChatgptChat) {
        return Err("Enable features.chatgpt_chat explicitly to run the live probe");
    }
    if config.chatgpt_base_url.trim_end_matches('/') != "https://chatgpt.com/backend-api" {
        return Err("This research harness does not yet support a custom ChatGPT backend");
    }
    let network_policy = EmbeddedNetworkPolicy::default();
    network_policy.activate(&mut config);
    codex_login::default_client::set_default_client_residency_requirement(
        config.enforce_residency.value(),
    );
    let auth = AuthManager::shared_from_config(&config, false)
        .await
        .map_err(|_| "Unable to initialize managed authentication")?;
    if args.rpc_read
        || args.rpc_saved
        || args.rpc_temporary
        || args.rpc_attachments
        || args.rpc_lifecycle
    {
        if args.transport != Some(ProbeTransport::BrowserEmulated)
            || ((args.rpc_saved || args.rpc_lifecycle)
                && (args.write_scenario != Some(WriteScenario::SavedText)
                    || args.preparation_protocol != Some(PreparationProtocol::TwoPhaseV1)))
            || (args.rpc_attachments
                && (args.write_scenario != Some(WriteScenario::Attachments)
                    || args.preparation_protocol != Some(PreparationProtocol::TwoPhaseV1)))
            || (args.rpc_temporary
                && (args.write_scenario != Some(WriteScenario::Temporary)
                    || args.preparation_protocol != Some(PreparationProtocol::TwoPhaseV1)
                    || args.temporary_protocol
                        != Some(codex_chatgpt_chat::TemporaryProtocol::ConduitRefreshV1)))
        {
            return Err(
                "RPC probes require browser-emulated; writes require two-phase-v1 and the matching saved-text, temporary, or attachments scenario; temporary also requires conduit-refresh-v1",
            );
        }
        if args.account_id.as_deref().is_some_and(|expected| {
            auth.auth_cached()
                .and_then(|auth| auth.get_account_id())
                .as_deref()
                != Some(expected)
        }) {
            return Err("The managed account does not match the designated account");
        }
        report.live = true;
        report.write_scenario = args.write_scenario;
        let scenario = if args.rpc_lifecycle {
            rpc_probe::Scenario::Lifecycle
        } else if args.rpc_attachments {
            rpc_probe::Scenario::Attachments(
                args.attachment_fixtures
                    .ok_or("Attachment fixtures are required")?,
            )
        } else if args.rpc_saved {
            if let Some(draft) = args.rpc_resume_draft {
                rpc_probe::Scenario::ResumeSaved(draft.to_string())
            } else {
                rpc_probe::Scenario::Saved
            }
        } else if args.rpc_temporary {
            rpc_probe::Scenario::Temporary
        } else {
            rpc_probe::Scenario::Read
        };
        let target = rpc_probe::Target {
            socket: args.rpc_socket,
            expected_account: auth
                .auth_cached()
                .and_then(|auth| auth.get_account_id())
                .ok_or("The managed ChatGPT account is unavailable")?,
        };
        report.rpc =
            Some(rpc_probe::run(config, overrides, network_policy, scenario, target).await);
        return Ok(report);
    }
    if args.service_saved {
        if args.write_scenario != Some(WriteScenario::SavedText)
            || args.transport != Some(ProbeTransport::BrowserEmulated)
            || args.preparation_protocol != Some(PreparationProtocol::TwoPhaseV1)
        {
            return Err(
                "--service-saved requires saved-text, browser-emulated, and two-phase-v1 explicitly",
            );
        }
        if args.account_id.as_deref().is_some_and(|expected| {
            auth.auth_cached()
                .and_then(|auth| auth.get_account_id())
                .as_deref()
                != Some(expected)
        }) {
            return Err("The managed account does not match the designated account");
        }
        report.live = true;
        report.write_scenario = Some(WriteScenario::SavedText);
        report.service = Some(service::run(auth, config.sqlite_config().clone()).await);
        return Ok(report);
    }
    let probe = ChatProbe::with_transport(
        auth,
        true,
        args.account_id.as_deref(),
        args.transport.unwrap_or_default(),
    );
    report.live = true;
    if args.temporary_protocol.is_some() && args.write_scenario != Some(WriteScenario::Temporary) {
        return Err("--temporary-protocol requires --write-scenario temporary");
    }
    if args.attachment_fixtures.is_some() && args.write_scenario != Some(WriteScenario::Attachments)
    {
        return Err("--attachment-fixtures requires --write-scenario attachments");
    }
    if args.write_scenario == Some(WriteScenario::Attachments)
        && report.preparation_protocol == PreparationProtocol::TwoPhaseV1
        && args.transport == Some(ProbeTransport::BrowserEmulated)
    {
        if args.resume_run.is_some() {
            return Err("Attachment fixture runs do not replay saved submissions");
        }
        let probe = probe.map_err(|_| "Unable to bind the designated managed account")?;
        let fixtures = args
            .attachment_fixtures
            .ok_or("Select the generated fixture directory with --attachment-fixtures")?;
        return run_attachments(probe, &config.codex_home, &fixtures, report).await;
    }
    if let Some(
        scenario @ (WriteScenario::SavedText
        | WriteScenario::Continuation
        | WriteScenario::RestartResume
        | WriteScenario::All),
    ) = args.write_scenario
        && report.preparation_protocol == PreparationProtocol::TwoPhaseV1
        && args.transport == Some(ProbeTransport::BrowserEmulated)
    {
        let probe = probe.map_err(|_| "Unable to bind the designated managed account")?;
        return run_saved(
            probe,
            &config.codex_home,
            scenario,
            args.resume_run.as_deref(),
            report,
        )
        .await;
    }
    if args.resume_run.is_some() {
        return Err(
            "--resume-run requires native browser-emulated saved acceptance with two-phase-v1",
        );
    }
    if args.write_scenario == Some(WriteScenario::Temporary)
        && report.preparation_protocol == PreparationProtocol::TwoPhaseV1
        && args.transport == Some(ProbeTransport::BrowserEmulated)
    {
        report.write_scenario = Some(WriteScenario::Temporary);
        let started = Instant::now();
        let result = match &probe {
            Ok(probe) => {
                probe
                    .inspect_temporary(args.temporary_protocol.unwrap_or_default())
                    .await
            }
            Err(error) => codex_chatgpt_chat::TemporaryObservation {
                error: Some(*error),
                ..Default::default()
            },
        };
        report.record(
            Capability::TemporaryChat,
            result.error.map_or(Ok(result.turns.len()), Err),
            elapsed_ms(started),
        );
        report.temporary = Some(result);
        return Ok(report);
    }
    if let Some(scenario) = args.write_scenario {
        let (observations, error) = match &probe {
            Ok(probe) => (
                match report.preparation_protocol {
                    PreparationProtocol::Diagnostic => {
                        probe
                            .inspect_preparation(scenario == WriteScenario::Temporary)
                            .await
                    }
                    PreparationProtocol::TwoPhaseV1 => {
                        probe
                            .inspect_native_preparation(scenario == WriteScenario::Temporary)
                            .await
                    }
                },
                None,
            ),
            Err(error) => (Vec::new(), Some(*error)),
        };
        let failed = error.is_some() || observations.iter().any(|step| step.error.is_some());
        report.record_preparation(scenario, observations, error);
        if failed {
            return Ok(report);
        }
    }
    let started = Instant::now();
    let result = match &probe {
        Ok(probe) => probe.model_count().await,
        Err(error) => Err(*error),
    };
    report.record(Capability::Models, result, elapsed_ms(started));
    if stops_run(result) {
        return Ok(report);
    }
    let started = Instant::now();
    let result = match &probe {
        Ok(probe) => probe.history_count(0, 20).await,
        Err(error) => Err(*error),
    };
    report.record(Capability::HistoryList, result, elapsed_ms(started));
    if stops_run(result) {
        return Ok(report);
    }
    if let Some(id) = args.conversation_id {
        let started = Instant::now();
        let result = match &probe {
            Ok(probe) => probe.active_message_count(&id).await,
            Err(error) => Err(*error),
        };
        report.record(Capability::HistoryRead, result, elapsed_ms(started));
    }
    Ok(report)
}

async fn run_saved(
    probe: ChatProbe,
    home: &std::path::Path,
    scenario: WriteScenario,
    resume: Option<&str>,
    mut report: ProbeReport,
) -> Result<ProbeReport, &'static str> {
    report.write_scenario = Some(scenario);
    let scope = probe
        .run_scope()
        .map_err(|_| "Managed login lacks a stable principal/workspace recovery scope")?;
    let mut journal = match resume {
        Some(id) => RunJournal::resume(home, id, &scope),
        None => RunJournal::create(home, scope),
    }
    .map_err(|_| "Unable to open private recovery journal; no message was transmitted")?;
    report.saved_run_id = Some(journal.run_id().to_owned());
    let expected = format!("NYACODE_NATIVE_SAVED_{}", journal.run_id());
    if resume.is_none() {
        journal
            .begin(
                None,
                uuid::Uuid::new_v4().to_string(),
                format!("Reply with exactly this marker and no other text: {expected}"),
            )
            .map_err(|_| "Unable to journal the saved fixture; no message was transmitted")?;
    }
    let started = Instant::now();
    let mut observation = if resume.is_some() {
        probe.inspect_saved_recovery(&mut journal).await
    } else {
        probe.send_saved(&mut journal).await
    };
    let matched = journal.current().is_some_and(|attempt| {
        attempt
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "assistant" && message.end_turn)
            .is_some_and(|message| message.text().trim() == expected)
    });
    observation.fixture_response_matched = Some(matched);
    let success = observation.error.is_none()
        && observation.authoritative_retrieval
        && observation.acceptance == Acceptance::Accepted
        && observation.generation == GenerationOutcome::Completed
        && matched;
    let result = if success {
        Ok(observation.assistant_characters)
    } else {
        Err(observation.error.unwrap_or(ChatError::IncompleteStream))
    };
    if resume.is_none() {
        let preparation_error = observation.preparation.iter().find_map(|step| step.error);
        report.record(
            Capability::ConversationPreparation,
            preparation_error.map_or(Ok(observation.preparation.len()), Err),
            observation
                .preparation
                .iter()
                .map(|step| step.latency_ms)
                .sum(),
        );
    }
    report.record(
        if resume.is_some() {
            Capability::HistoryRead
        } else {
            Capability::SavedText
        },
        result,
        elapsed_ms(started),
    );
    report.saved_turns.push(observation);
    if success
        && matches!(
            scenario,
            WriteScenario::Continuation | WriteScenario::RestartResume | WriteScenario::All
        )
    {
        let previous = journal
            .current()
            .ok_or("Saved run has no current submission")?;
        let conversation = previous
            .conversation_id
            .clone()
            .ok_or("Saved run lacks a verified conversation ID")?;
        let parent = previous
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "assistant" && message.end_turn)
            .map(|message| message.id.clone())
            .ok_or("Saved run lacks a completed parent")?;
        journal.begin(Some(conversation), parent, "Repeat the exact marker from my first message in this conversation, and nothing else.".into()).map_err(|_| "Unable to journal follow-up; no follow-up was transmitted")?;
        let started = Instant::now();
        let mut observation = probe.send_saved(&mut journal).await;
        let matched = journal.current().is_some_and(|attempt| {
            attempt
                .messages
                .iter()
                .rev()
                .find(|message| message.role == "assistant" && message.end_turn)
                .is_some_and(|message| message.text().trim() == expected)
        });
        observation.fixture_response_matched = Some(matched);
        let success = observation.error.is_none()
            && observation.authoritative_retrieval
            && observation.acceptance == Acceptance::Accepted
            && observation.generation == GenerationOutcome::Completed
            && matched;
        let result = if success {
            Ok(observation.assistant_characters)
        } else {
            Err(observation.error.unwrap_or(ChatError::IncompleteStream))
        };
        report.record(Capability::FollowUp, result, elapsed_ms(started));
        if resume.is_some() && scenario == WriteScenario::RestartResume {
            report.record(Capability::RestartResume, result, elapsed_ms(started));
        }
        report.saved_turns.push(observation);
    }
    Ok(report)
}

async fn run_attachments(
    probe: ChatProbe,
    home: &std::path::Path,
    fixtures: &std::path::Path,
    mut report: ProbeReport,
) -> Result<ProbeReport, &'static str> {
    use codex_chatgpt_chat::attachment::AttachmentStager;
    use tokio_util::sync::CancellationToken;
    let specifications = fixtures::SPECIFICATIONS;
    report.write_scenario = Some(WriteScenario::Attachments);
    let files = AttachmentStager::new()
        .snapshot(
            specifications
                .iter()
                .map(|(name, ..)| fixtures.join(name))
                .collect(),
            CancellationToken::new(),
        )
        .await
        .map_err(|_| "Unable to stage all five harmless attachment fixtures")?;
    for (file, (_, _, digest, kind, _)) in files.iter().zip(&specifications) {
        if file.metadata().sha256 != *digest || file.metadata().kind != *kind {
            return Err(
                "Fixture bytes differ from the generated acceptance corpus; no upload was attempted",
            );
        }
    }
    let scope = probe
        .run_scope()
        .map_err(|_| "Managed login lacks a stable recovery scope")?;
    let mut journal = RunJournal::create(home, scope)
        .map_err(|_| "Unable to create private attachment recovery journal")?;
    report.saved_run_id = Some(journal.run_id().to_owned());
    let mut conversation = None;
    let mut parent = uuid::Uuid::new_v4().to_string();
    for (file, (_, expected, _, kind, capability)) in files.into_iter().zip(specifications) {
        let draft = uuid::Uuid::new_v4();
        journal.begin(conversation.clone(), parent.clone(), "Read only the file attached to this message. Reply with exactly its verification code and no other text.".into())
            .map_err(|_| "Unable to journal attachment submission; no message was transmitted")?;
        let started = Instant::now();
        let (ready, upload) = probe
            .upload_attachment(draft, false, &file, &CancellationToken::new(), |_, _| {})
            .await;
        let mut observation = codex_chatgpt_chat::AttachmentObservation {
            kind,
            upload,
            turn: None,
        };
        let result = if let Some(ready) = ready {
            drop(file);
            let mut turn = probe
                .send_saved_with_attachments(&mut journal, draft, &[ready])
                .await;
            let matched = journal.current().is_some_and(|attempt| {
                attempt
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == "assistant" && m.end_turn)
                    .is_some_and(|m| m.text().trim() == expected)
            });
            turn.fixture_response_matched = Some(matched);
            let success = turn.error.is_none()
                && turn.authoritative_retrieval
                && turn.acceptance == Acceptance::Accepted
                && turn.generation == GenerationOutcome::Completed
                && matched;
            let result = if success {
                Ok(turn.assistant_characters)
            } else {
                Err(turn.error.unwrap_or(ChatError::IncompleteStream))
            };
            observation.turn = Some(turn);
            result
        } else {
            Err(observation.upload.error.unwrap_or(ChatError::UploadFailed))
        };
        report.record(capability, result, elapsed_ms(started));
        report.attachments.push(observation);
        if result.is_err() {
            break;
        }
        let attempt = journal
            .current()
            .ok_or("Attachment run has no current submission")?;
        conversation.clone_from(&attempt.conversation_id);
        parent = attempt
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant" && m.end_turn)
            .ok_or("Attachment run has no completed parent")?
            .id
            .clone();
    }
    Ok(report)
}

fn stops_run(result: Result<usize, ChatError>) -> bool {
    matches!(
        result,
        Err(ChatError::CloudflareChallenge
            | ChatError::UnsupportedChallenge
            | ChatError::RateLimited
            | ChatError::IdentityChanged
            | ChatError::SessionChanged
            | ChatError::LocalCancellation)
    )
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_require_an_explicit_live_account() {
        for args in [
            vec!["probe", "--write-scenario", "all"],
            vec!["probe", "--live", "--write-scenario", "saved-text"],
            vec!["probe", "--current-account", "--write-scenario", "all"],
            vec![
                "probe",
                "--live",
                "--current-account",
                "--write-scenario",
                "unknown",
            ],
            vec![
                "probe",
                "--live",
                "--current-account",
                "--conversation-id",
                "personal",
                "--write-scenario",
                "continuation",
            ],
        ] {
            assert!(Args::try_parse_from(args).is_err());
        }
        for scenario in [
            "preparation",
            "saved-text",
            "continuation",
            "restart-resume",
            "temporary",
            "attachments",
            "all",
        ] {
            assert!(
                Args::try_parse_from([
                    "probe",
                    "--live",
                    "--current-account",
                    "--write-scenario",
                    scenario
                ])
                .is_ok()
            );
        }
    }

    #[test]
    fn socket_requires_an_explicit_rpc_mode() {
        assert!(Args::try_parse_from(["probe", "--rpc-socket", "/tmp/fixture.sock"]).is_err());
        let read = Args::try_parse_from([
            "probe",
            "--live",
            "--current-account",
            "--rpc-read",
            "--rpc-socket",
            "/tmp/fixture.sock",
        ])
        .unwrap();
        assert!(read.write_scenario.is_none());
        assert_eq!(read.rpc_socket, Some(PathBuf::from("/tmp/fixture.sock")));
        assert!(
            Args::try_parse_from([
                "probe",
                "--live",
                "--current-account",
                "--rpc-read",
                "--rpc-saved",
                "--write-scenario",
                "saved-text"
            ])
            .is_err()
        );
    }

    #[test]
    fn rpc_resume_requires_an_explicit_saved_write_and_uuid() {
        let id = uuid::Uuid::new_v4().to_string();
        assert!(Args::try_parse_from(["probe", "--rpc-resume-draft", &id]).is_err());
        let args = [
            "probe",
            "--live",
            "--current-account",
            "--rpc-saved",
            "--write-scenario",
            "saved-text",
            "--rpc-resume-draft",
        ];
        assert!(Args::try_parse_from(args.into_iter().chain([id.as_str()])).is_ok());
        assert!(Args::try_parse_from(args.into_iter().chain(["invalid"])).is_err());
    }

    #[test]
    fn lifecycle_requires_explicit_writes_and_excludes_other_rpc_scenarios() {
        assert!(Args::try_parse_from(["probe", "--rpc-lifecycle"]).is_err());
        let args = [
            "probe",
            "--live",
            "--current-account",
            "--rpc-lifecycle",
            "--write-scenario",
            "saved-text",
        ];
        assert!(Args::try_parse_from(args).unwrap().rpc_lifecycle);
        for incompatible in ["--rpc-saved", "--rpc-temporary", "--rpc-read"] {
            assert!(Args::try_parse_from(args.into_iter().chain([incompatible])).is_err());
        }
    }

    #[test]
    fn original_invocations_remain_read_only() {
        let offline = Args::try_parse_from(["probe"]).unwrap();
        assert!(!offline.live);
        assert!(offline.write_scenario.is_none());
        let live = Args::try_parse_from(["probe", "--live", "--current-account"]).unwrap();
        assert!(live.live);
        assert!(live.write_scenario.is_none());
        assert!(live.transport.is_none());
        assert!(live.preparation_protocol.is_none());
        assert!(live.resume_run.is_none());
        assert!(!live.service_saved);
        assert!(!live.rpc_read);
        assert!(!live.rpc_saved);
        assert!(!live.rpc_temporary);
        assert!(!live.rpc_attachments);
        assert!(!live.rpc_lifecycle);
        assert!(
            Args::try_parse_from(["probe", "--live", "--current-account", "--rpc-attachments"])
                .is_err()
        );
        assert!(
            Args::try_parse_from([
                "probe",
                "--live",
                "--current-account",
                "--write-scenario",
                "attachments",
                "--rpc-attachments"
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from(["probe", "--live", "--current-account", "--rpc-temporary"])
                .is_err()
        );
        assert!(
            Args::try_parse_from(["probe", "--live", "--current-account", "--rpc-saved"]).is_err()
        );
        assert!(
            Args::try_parse_from(["probe", "--live", "--current-account", "--service-saved"])
                .is_err()
        );
        assert!(
            Args::try_parse_from([
                "probe",
                "--live",
                "--current-account",
                "--resume-run",
                "fixture"
            ])
            .is_err()
        );
    }

    #[test]
    fn protocol_selection_requires_explicit_write_scenario() {
        assert!(
            Args::try_parse_from([
                "probe",
                "--live",
                "--current-account",
                "--preparation-protocol",
                "two-phase-v1"
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "probe",
                "--live",
                "--current-account",
                "--write-scenario",
                "preparation",
                "--preparation-protocol",
                "two-phase-v1"
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "probe",
                "--live",
                "--current-account",
                "--write-scenario",
                "preparation",
                "--preparation-protocol",
                "auto"
            ])
            .is_err()
        );
    }

    #[test]
    fn comparison_transports_require_explicit_valid_selection() {
        for transport in ["standard", "browser-emulated"] {
            let args = Args::try_parse_from([
                "probe",
                "--live",
                "--current-account",
                "--transport",
                transport,
            ])
            .unwrap();
            assert!(args.transport.is_some());
            assert!(args.write_scenario.is_none());
        }
        assert!(Args::try_parse_from(["probe", "--transport", "browser-emulated"]).is_err());
        assert!(
            Args::try_parse_from([
                "probe",
                "--live",
                "--current-account",
                "--transport",
                "auto"
            ])
            .is_err()
        );
    }

    #[tokio::test]
    async fn offline_report_does_not_load_configuration_or_authentication() {
        let args = Args::try_parse_from([
            "probe",
            "--codex-home",
            "/missing/nyacode-home",
            "-c",
            "invalid",
        ])
        .unwrap();
        let report = run(args).await.unwrap();
        assert!(!report.live);
        assert!(!report.release_gate_passed);
        assert!(report.preparation.is_empty());
    }
}
