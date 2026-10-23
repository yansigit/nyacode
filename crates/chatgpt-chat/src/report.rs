use crate::ChatError;
use serde::Serialize;
use std::str::FromStr;

/// Selects acceptance work explicitly. A failed prerequisite stops dependent
/// scenarios; selecting `All` never relaxes the native transport gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriteScenario {
    Preparation,
    SavedText,
    Continuation,
    RestartResume,
    Temporary,
    Attachments,
    All,
}

impl FromStr for WriteScenario {
    type Err = ChatError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "preparation" => Ok(Self::Preparation),
            "saved-text" => Ok(Self::SavedText),
            "continuation" => Ok(Self::Continuation),
            "restart-resume" => Ok(Self::RestartResume),
            "temporary" => Ok(Self::Temporary),
            "attachments" => Ok(Self::Attachments),
            "all" => Ok(Self::All),
            _ => Err(ChatError::InvalidInput),
        }
    }
}

impl WriteScenario {
    fn dependent_capabilities(self) -> &'static [Capability] {
        use Capability::*;
        match self {
            Self::Preparation => &[],
            Self::SavedText => &[SavedText],
            Self::Continuation => &[SavedText, FollowUp],
            Self::RestartResume => &[SavedText, FollowUp, RestartResume],
            Self::Temporary => &[TemporaryChat],
            Self::Attachments => &[PngUpload, JpegUpload, WebpUpload, PdfUpload, Utf8Upload],
            Self::All => &[
                SavedText,
                FollowUp,
                RestartResume,
                TemporaryChat,
                PngUpload,
                JpegUpload,
                WebpUpload,
                PdfUpload,
                Utf8Upload,
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Models,
    HistoryList,
    HistoryRead,
    ConversationPreparation,
    SavedText,
    FollowUp,
    RestartResume,
    TemporaryChat,
    PngUpload,
    JpegUpload,
    WebpUpload,
    PdfUpload,
    Utf8Upload,
    ServerCancellation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    NotRun,
    Observed,
    Blocked,
    Unsupported,
    Failed,
}

#[derive(Debug, Serialize)]
pub struct CapabilityResult {
    pub capability: Capability,
    pub status: CapabilityStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ChatError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<Capability>,
}

#[derive(Debug, Serialize)]
pub struct AttachmentObservation {
    pub kind: crate::attachment::AttachmentKind,
    pub upload: crate::upload::UploadObservation,
    pub turn: Option<crate::SavedSendObservation>,
}

/// App-server service evidence contains no account, draft, conversation, or
/// submission IDs. It does not establish the embedded/daemon RPC or TUI gate.
#[derive(Default, Debug, Serialize)]
pub struct ServiceObservation {
    pub saved_journal_accepted: bool,
    pub saved_journal_terminal: bool,
    pub authoritative_retrieval: bool,
    pub fixture_response_matched: bool,
    pub feature_disable_revoked: bool,
    pub remote_rejected: bool,
    pub requested_model: Option<String>,
    pub resolved_model: Option<String>,
    pub acceptance: crate::stream::Acceptance,
    pub generation: crate::stream::GenerationOutcome,
    pub error: Option<String>,
}

/// RPC evidence contains only bounded capability metadata, never IDs or content.
#[derive(Default, Debug, Serialize)]
pub struct RpcObservation {
    pub local_socket: bool,
    pub account_matched: bool,
    pub status_available: bool,
    pub model_count: usize,
    pub default_model: Option<String>,
    pub history_count: usize,
    pub accepted: bool,
    pub completed: bool,
    pub streamed_updates: usize,
    pub sequence_ordered: bool,
    pub authoritative_retrieval: bool,
    pub fixture_response_matched: bool,
    pub pending_cleared: bool,
    pub resumed_saved_draft: bool,
    pub parent_matched: bool,
    pub requested_model: Option<String>,
    pub resolved_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporary: Option<TemporaryRpcObservation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<RpcAttachmentObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<LifecycleRpcObservation>,
    pub error: Option<String>,
}

#[derive(Default, Debug, Serialize)]
pub struct LifecycleRpcObservation {
    pub runtime_enablement: bool,
    pub cancel_after_acceptance: bool,
    pub cancel_requested_locally: bool,
    pub cancel_finished_locally: bool,
    pub cancel_acceptance_retained: bool,
    pub server_cancellation_confirmed: bool,
    pub disable_after_acceptance: bool,
    pub disabled_has_no_capabilities: bool,
    pub revoked_scope_rejected: bool,
    pub fresh_scope: bool,
    pub temporary_draft_removed: bool,
    pub saved_messages_retrieved: usize,
}

/// Per-format RPC proof, with fixture contents and all identifiers omitted.
#[derive(Debug, Serialize)]
pub struct RpcAttachmentObservation {
    pub kind: crate::attachment::AttachmentKind,
    pub upload_updates: usize,
    pub sequence_ordered: bool,
    pub ready_metadata_matched: bool,
    pub accepted: bool,
    pub completed: bool,
    pub fixture_response_matched: bool,
    pub authoritative_retrieval: bool,
    pub parent_matched: bool,
    pub history_attachment_present: bool,
    pub pending_cleared: bool,
    pub error: Option<String>,
}

#[derive(Default, Debug, Serialize)]
pub struct TemporaryRpcObservation {
    pub completed_turns: usize,
    pub continuation_verified: bool,
    pub memory_snapshot_matched: bool,
    pub saved_read_rejected: bool,
    pub absent_from_recent_history: bool,
    pub saved_tables_unchanged: bool,
    pub recovery_unavailable: bool,
}

/// Safe to retain as a diagnostic. Successful reads never imply send entitlement.
#[derive(Debug, Serialize)]
pub struct ProbeReport {
    pub report_version: u32,
    pub stage: &'static str,
    pub live: bool,
    pub transport: crate::ProbeTransport,
    pub transport_explicit: bool,
    pub preparation_protocol: crate::PreparationProtocol,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport_version: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<&'static str>,
    /// Whether this individual probe established full release acceptance.
    /// Selected scenarios cannot replace the aggregate integrated assessment.
    pub release_gate_passed: bool,
    pub capabilities: Vec<CapabilityResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_scenario: Option<WriteScenario>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub preparation: Vec<crate::PreparationObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_run_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub saved_turns: Vec<crate::SavedSendObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporary: Option<crate::TemporaryObservation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpc: Option<RpcObservation>,
}

impl Default for ProbeReport {
    fn default() -> Self {
        use Capability::*;
        Self {
            report_version: 15,
            stage: "experimental",
            live: false,
            transport: crate::ProbeTransport::Standard,
            transport_explicit: false,
            preparation_protocol: crate::PreparationProtocol::Diagnostic,
            transport_version: None,
            profile: None,
            release_gate_passed: false,
            preparation: Vec::new(),
            saved_run_id: None,
            saved_turns: Vec::new(),
            temporary: None,
            attachments: Vec::new(),
            service: None,
            rpc: None,
            write_scenario: None,
            capabilities: [
                Models,
                HistoryList,
                HistoryRead,
                ConversationPreparation,
                SavedText,
                FollowUp,
                RestartResume,
                TemporaryChat,
                PngUpload,
                JpegUpload,
                WebpUpload,
                PdfUpload,
                Utf8Upload,
                ServerCancellation,
            ]
            .into_iter()
            .map(|capability| CapabilityResult {
                capability,
                status: CapabilityStatus::NotRun,
                error: None,
                item_count: None,
                latency_ms: None,
                blocked_by: None,
            })
            .collect(),
        }
    }
}

impl ProbeReport {
    pub fn select_transport(&mut self, transport: crate::ProbeTransport) {
        self.transport = transport;
        self.transport_explicit = true;
        let browser = transport == crate::ProbeTransport::BrowserEmulated;
        self.transport_version = browser.then_some(codex_http_client::CHAT_BROWSER_TRANSPORT);
        self.profile = browser.then_some(codex_http_client::CHAT_BROWSER_PROFILE);
    }

    /// Records a prerequisite failure without claiming dependent operations ran.
    /// Until a verified preparation-to-send implementation exists, even three
    /// successful endpoint probes leave preparation unsupported by this harness.
    pub fn record_preparation(
        &mut self,
        scenario: WriteScenario,
        observations: Vec<crate::PreparationObservation>,
        setup_error: Option<ChatError>,
    ) {
        self.write_scenario = Some(scenario);
        let failure = setup_error.or_else(|| observations.iter().find_map(|step| step.error));
        let error = failure.unwrap_or(ChatError::UnsupportedCapability);
        let latency_ms = observations.iter().map(|step| step.latency_ms).sum();
        self.record(Capability::ConversationPreparation, Err(error), latency_ms);
        if failure.is_none()
            && let Some(entry) = self
                .capabilities
                .iter_mut()
                .find(|entry| entry.capability == Capability::ConversationPreparation)
        {
            entry.status = CapabilityStatus::Unsupported;
        }
        for entry in &mut self.capabilities {
            if scenario
                .dependent_capabilities()
                .contains(&entry.capability)
            {
                entry.status = CapabilityStatus::Blocked;
                entry.error = Some(error);
                entry.item_count = None;
                entry.latency_ms = None;
                entry.blocked_by = Some(Capability::ConversationPreparation);
            }
        }
        self.preparation = observations;
    }

    pub fn record(
        &mut self,
        capability: Capability,
        result: Result<usize, ChatError>,
        latency_ms: u64,
    ) {
        if let Some(entry) = self
            .capabilities
            .iter_mut()
            .find(|entry| entry.capability == capability)
        {
            entry.latency_ms = Some(latency_ms);
            entry.blocked_by = None;
            match result {
                Ok(count) => {
                    entry.status = CapabilityStatus::Observed;
                    entry.item_count = Some(count);
                    entry.error = None;
                }
                Err(error) => {
                    entry.status = CapabilityStatus::Failed;
                    entry.item_count = None;
                    entry.error = Some(error);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn successful_reads_do_not_open_the_live_release_gate() {
        let mut report = ProbeReport::default();
        assert_eq!(report.stage, "experimental");
        for capability in [
            Capability::Models,
            Capability::HistoryList,
            Capability::HistoryRead,
        ] {
            report.record(capability, Ok(1), 0);
        }
        assert!(!report.release_gate_passed);
        assert_eq!(
            report
                .capabilities
                .iter()
                .filter(|entry| entry.status == CapabilityStatus::NotRun)
                .count(),
            11
        );
    }

    #[test]
    fn preparation_never_promotes_unattempted_scenarios() {
        let mut report = ProbeReport::default();
        report.record_preparation(
            WriteScenario::All,
            Vec::new(),
            Some(ChatError::CloudflareChallenge),
        );
        assert!(!report.release_gate_passed);
        for entry in &report.capabilities {
            if WriteScenario::All
                .dependent_capabilities()
                .contains(&entry.capability)
            {
                assert_eq!(entry.status, CapabilityStatus::Blocked);
                assert_eq!(entry.blocked_by, Some(Capability::ConversationPreparation));
                assert_eq!(entry.latency_ms, None);
                assert_eq!(entry.item_count, None);
            }
        }
        let cancellation = report
            .capabilities
            .iter()
            .find(|entry| entry.capability == Capability::ServerCancellation)
            .unwrap();
        assert_eq!(cancellation.status, CapabilityStatus::NotRun);
    }

    #[test]
    fn scenario_selection_does_not_claim_other_capabilities() {
        let mut report = ProbeReport::default();
        report.record_preparation(WriteScenario::Temporary, Vec::new(), None);
        for entry in &report.capabilities {
            match entry.capability {
                Capability::ConversationPreparation => {
                    assert_eq!(entry.status, CapabilityStatus::Unsupported)
                }
                Capability::TemporaryChat => assert_eq!(entry.status, CapabilityStatus::Blocked),
                _ => assert_eq!(entry.status, CapabilityStatus::NotRun),
            }
        }
        assert!(!report.release_gate_passed);
    }
}
