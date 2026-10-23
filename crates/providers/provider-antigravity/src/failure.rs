use codex_provider_api::*;

pub fn classify(response: FailureResponse<'_>) -> Failure {
    let Some(body) = response.json() else {
        return response.unknown();
    };
    let status = body["error"]["status"].as_str().unwrap_or_default();
    if response.status == 401 && status == "UNAUTHENTICATED" {
        return response.failure(
            FailureCategory::Authentication,
            FailureCode::AuthenticationRejected,
            FailureScope::Credential,
        );
    }
    if response.status == 503 && status == "UNAVAILABLE" {
        return response.failure(
            FailureCategory::ProviderUnavailable,
            FailureCode::Overloaded,
            FailureScope::Provider,
        );
    }
    if response.status != 429 || status != "RESOURCE_EXHAUSTED" {
        return response.unknown();
    }
    let details = body["error"]["details"].as_array();
    let reason = details
        .into_iter()
        .flatten()
        .find(|detail| {
            detail["@type"] == "type.googleapis.com/google.rpc.ErrorInfo"
                && detail["domain"] == "cloudcode-pa.googleapis.com"
        })
        .and_then(|detail| detail["reason"].as_str());
    let (category, code) = match reason {
        Some("QUOTA_EXHAUSTED" | "QUOTA_EXCEEDED") => {
            (FailureCategory::QuotaExhausted, FailureCode::QuotaExhausted)
        }
        Some("RATE_LIMIT_EXCEEDED") => (FailureCategory::TemporaryLimit, FailureCode::RateLimit),
        _ => return response.unknown(),
    };
    let mut failure = response.failure(category, code, FailureScope::Model(response.model.into()));
    if let Some(seconds) = details
        .into_iter()
        .flatten()
        .find(|detail| detail["@type"] == "type.googleapis.com/google.rpc.RetryInfo")
        .and_then(|detail| detail["retryDelay"].as_str())
        .and_then(|delay| delay.strip_suffix('s'))
        .and_then(|seconds| seconds.parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0 && *seconds < 31536000.0)
    {
        failure.retry_at = Some(response.now.saturating_add(seconds.ceil() as u64));
    }
    failure
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
