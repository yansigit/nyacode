//! Provider-owned evidence and account identities. No host or transport types.
use serde::Deserialize;
use serde::Serialize;

/// Stable, non-secret identity used to join credentials drawing on shared quota.
pub fn quota_identity(kind: &str, id: &str) -> String {
    use sha2::Digest;
    format!(
        "{kind}:{}",
        hex::encode(sha2::Sha256::digest(id.as_bytes()))
    )
}

pub fn retry_after_seconds(value: &str, now: u64) -> Option<u64> {
    value.parse().ok().or_else(|| {
        httpdate::parse_http_date(value)
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|deadline| deadline.as_secs().saturating_sub(now))
    })
}

pub struct FailureResponse<'a> {
    pub status: u16,
    pub body: &'a [u8],
    pub retry_after_seconds: Option<u64>,
    pub model: &'a str,
    pub now: u64,
    pub outcome: RequestOutcome,
}
pub type FailureClassifier = for<'a> fn(FailureResponse<'a>) -> Failure;
impl FailureResponse<'_> {
    pub fn unknown(&self) -> Failure {
        Failure {
            category: FailureCategory::Unknown,
            code: FailureCode::Unknown,
            scope: FailureScope::Provider,
            reset_at: None,
            retry_at: None,
            outcome: self.outcome,
            another_account_may_help: false,
        }
    }
    pub fn failure(
        &self,
        category: FailureCategory,
        code: FailureCode,
        scope: FailureScope,
    ) -> Failure {
        Failure {
            category,
            code,
            scope,
            reset_at: None,
            retry_at: self
                .retry_after_seconds
                .map(|seconds| self.now.saturating_add(seconds)),
            outcome: self.outcome,
            another_account_may_help: matches!(
                category,
                FailureCategory::TemporaryLimit
                    | FailureCategory::QuotaExhausted
                    | FailureCategory::Authentication
                    | FailureCategory::LoginRequired
                    | FailureCategory::ModelEntitlement
            ),
        }
    }
    pub fn json(&self) -> Option<serde_json::Value> {
        if self.body.len() > 64 * 1024 {
            return None;
        }
        serde_json::from_slice(self.body).ok()
    }
}

/// A label may be reused, but a credential generation may never be reused.
/// Legacy credentials without a generation retain their published identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountRef {
    pub label: String,
    pub generation: Option<String>,
}
impl AccountRef {
    pub fn identity(&self) -> String {
        match &self.generation {
            Some(generation) => format!("{}:{generation}", self.label),
            None => self.label.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    TemporaryLimit,
    QuotaExhausted,
    Authentication,
    LoginRequired,
    ModelEntitlement,
    InvalidRequest,
    ContextOverflow,
    ProviderUnavailable,
    Unknown,
}

/// Closed vocabulary: never persist an upstream message, prompt, or error body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    RateLimit,
    QuotaExhausted,
    AuthenticationRejected,
    CredentialRevoked,
    ModelNotAllowed,
    InvalidRequest,
    ContextTooLarge,
    Overloaded,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum FailureScope {
    Credential,
    Account,
    Model(String),
    QuotaGroup(String),
    Provider,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOutcome {
    NotDispatched,
    Rejected,
    PotentiallyProcessed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub category: FailureCategory,
    pub code: FailureCode,
    pub scope: FailureScope,
    pub reset_at: Option<u64>,
    pub retry_at: Option<u64>,
    pub outcome: RequestOutcome,
    pub another_account_may_help: bool,
}
impl Failure {
    pub fn can_fail_over(&self) -> bool {
        self.another_account_may_help
            && self.outcome != RequestOutcome::PotentiallyProcessed
            && !matches!(self.scope, FailureScope::Provider)
            && matches!(
                self.category,
                FailureCategory::TemporaryLimit
                    | FailureCategory::QuotaExhausted
                    | FailureCategory::Authentication
                    | FailureCategory::LoginRequired
                    | FailureCategory::ModelEntitlement
            )
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "provider rejection: {:?}", self.code)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationBoundary {
    InitialRejection,
    CompletedTurn,
    RecordedTools,
}

/// Only the host can acknowledge its actual persisted conversation history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryAcknowledgement {
    pub hash: String,
    pub length: usize,
    pub checkpoint_scope: String,
}
