use codex_provider_api::*;

/// Gateway errors are shared by all three Go routes. Generic upstream 429s do
/// not establish whether another Go account could help.
pub fn classify(response: FailureResponse<'_>) -> Failure {
    let Some(body) = response.json() else {
        return response.unknown();
    };
    let kind = body["error"]["type"].as_str().unwrap_or_default();
    match (response.status, kind) {
        (401, "AuthError") => response.failure(
            FailureCategory::LoginRequired,
            FailureCode::CredentialRevoked,
            FailureScope::Credential,
        ),
        (429, "GoUsageLimitError")
        | (401, "MonthlyLimitError" | "UserLimitError" | "CreditsError") => response.failure(
            FailureCategory::QuotaExhausted,
            FailureCode::QuotaExhausted,
            body["metadata"]["workspace"]
                .as_str()
                .filter(|id| {
                    id.starts_with("wrk_")
                        && id.len() <= 128
                        && id
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                })
                .map(|id| FailureScope::QuotaGroup(quota_identity("workspace_id", id)))
                .unwrap_or(FailureScope::Account),
        ),
        // Gateway RateLimitError may be IP/model capacity; do not burn accounts.
        (429, "RateLimitError") => response.failure(
            FailureCategory::ProviderUnavailable,
            FailureCode::RateLimit,
            FailureScope::Provider,
        ),
        (401, "ModelError") | (403, "DataPolicyError" | "RegionError") => response.failure(
            FailureCategory::InvalidRequest,
            FailureCode::InvalidRequest,
            FailureScope::Provider,
        ),
        _ => response.unknown(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sanitized_failure_fixtures_require_service_specific_evidence() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/account-failures.json")).unwrap();
        for case in cases {
            let bytes = serde_json::to_vec(&case["body"]).unwrap();
            let failure = classify(FailureResponse {
                status: case["status"].as_u64().unwrap() as u16,
                body: &bytes,
                retry_after_seconds: None,
                model: "fixture-model",
                now: 100,
                outcome: RequestOutcome::Rejected,
            });
            assert_eq!(
                serde_json::to_value(failure.category).unwrap(),
                case["category"]
            );
            assert_eq!(
                failure.can_fail_over(),
                case["can_fail_over"].as_bool().unwrap()
            );
            let persisted = serde_json::to_string(&failure).unwrap();
            assert!(!persisted.contains("fixture secret"));
            let streaming = classify(FailureResponse {
                status: case["status"].as_u64().unwrap() as u16,
                body: &bytes,
                retry_after_seconds: None,
                model: "fixture-model",
                now: 100,
                outcome: RequestOutcome::PotentiallyProcessed,
            });
            assert!(!streaming.can_fail_over());
        }
        for body in [b"not JSON".to_vec(), vec![b' '; 65537], b"{}".to_vec()] {
            assert!(
                !classify(FailureResponse {
                    status: 429,
                    body: &body,
                    retry_after_seconds: Some(60),
                    model: "m",
                    now: 0,
                    outcome: RequestOutcome::Rejected
                })
                .can_fail_over()
            );
        }
    }
}
