use codex_provider_api::*;

pub fn classify(response: FailureResponse<'_>) -> Failure {
    let Some(body) = response.json() else {
        return response.unknown();
    };
    let error = body.get("error").unwrap_or(&body);
    match (response.status, error["code"].as_str().unwrap_or_default()) {
        (401, "unauthenticated") => response.failure(
            FailureCategory::Authentication,
            FailureCode::AuthenticationRejected,
            FailureScope::Credential,
        ),
        // Connect resource_exhausted alone does not prove an account quota limit.
        (503, "unavailable") => response.failure(
            FailureCategory::ProviderUnavailable,
            FailureCode::Overloaded,
            FailureScope::Provider,
        ),
        (400, "invalid_argument") => response.failure(
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
