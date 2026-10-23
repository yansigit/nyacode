use codex_provider_api::*;

/// Only documented error types combined with their HTTP context establish rejection.
pub fn classify(response: FailureResponse<'_>) -> Failure {
    let Some(body) = response.json() else {
        return response.unknown();
    };
    let kind = body["error"]["type"].as_str().unwrap_or_default();
    match (response.status, kind) {
        (401, "authentication_error") => response.failure(
            FailureCategory::Authentication,
            FailureCode::AuthenticationRejected,
            FailureScope::Credential,
        ),
        (429, "rate_limit_error") => {
            // A spend-cap rejection can omit Retry-After. Do not promise a reset.
            let message = body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let spent = message.contains("spend limit") || message.contains("spend cap");
            response.failure(
                if spent {
                    FailureCategory::QuotaExhausted
                } else {
                    FailureCategory::TemporaryLimit
                },
                if spent {
                    FailureCode::QuotaExhausted
                } else {
                    FailureCode::RateLimit
                },
                FailureScope::Account,
            )
        }
        (529 | 200, "overloaded_error") => response.failure(
            FailureCategory::ProviderUnavailable,
            FailureCode::Overloaded,
            FailureScope::Provider,
        ),
        (400 | 413, "invalid_request_error" | "request_too_large") => response.failure(
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
