use super::Budget;
use super::Limits;
use super::Result;
use super::SentinelFailure;
use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use serde_json::Value;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// FNV-1a with avalanche, using JavaScript UTF-16 code units and wrapping imul.
/// Protocol revision-specific; never substituted for the older SHA3 contract.
fn hash(input: &str) -> u32 {
    let mut h = 2_166_136_261_u32;
    for unit in input.encode_utf16() {
        h = (h ^ u32::from(unit)).wrapping_mul(16_777_619);
    }
    h = (h ^ (h >> 16)).wrapping_mul(2_246_822_507);
    h = (h ^ (h >> 13)).wrapping_mul(3_266_489_909);
    h ^ (h >> 16)
}

pub(crate) fn solve_proof(
    seed: &str,
    difficulty: &str,
    mut config: Vec<Value>,
    cancellation: CancellationToken,
) -> Result<String> {
    if seed.is_empty()
        || seed.len() > 1024
        || difficulty.is_empty()
        || difficulty.len() > 8
        || !difficulty.bytes().all(|b| b.is_ascii_hexdigit())
        || config.len() != 25
    {
        return Err(SentinelFailure::InvalidProofChallenge);
    }
    let limit =
        u32::from_str_radix(difficulty, 16).map_err(|_| SentinelFailure::InvalidProofChallenge)?;
    let shift = 4 * (8 - difficulty.len());
    let budget = Budget::new(Limits::default(), cancellation);
    for nonce in 0..500_000_u32 {
        budget.check()?;
        config[3] = json!(nonce);
        config[9] = json!(u64::try_from(budget.started.elapsed().as_millis()).unwrap_or(u64::MAX));
        let encoded = BASE64_STANDARD
            .encode(serde_json::to_vec(&config).map_err(|_| SentinelFailure::MalformedProgram)?);
        if hash(&format!("{seed}{encoded}")) >> shift <= limit {
            return Ok(format!("gAAAAAB{encoded}~S"));
        }
    }
    Err(SentinelFailure::ProofExhausted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_fixed_vectors_including_utf16_surrogates() {
        for (input, expected) in [
            ("", 0xab3e_7c0b),
            ("hello", 0x888d_766e),
            ("seedAAAA", 0x9ad1_2ff7),
            ("😀", 0x6a85_a426),
        ] {
            assert_eq!(hash(input), expected);
        }
    }

    #[test]
    fn proof_binds_seed_and_preserves_fingerprint() {
        let config = vec![json!(0); 25];
        let result =
            solve_proof("fixture", "0f", config.clone(), CancellationToken::new()).unwrap();
        let encoded = result
            .strip_prefix("gAAAAAB")
            .unwrap()
            .strip_suffix("~S")
            .unwrap();
        let decoded: Vec<Value> =
            serde_json::from_slice(&BASE64_STANDARD.decode(encoded).unwrap()).unwrap();
        assert!(decoded[3].is_u64());
        assert_eq!(decoded[4..9], config[4..9]);
        assert!(hash(&format!("fixture{encoded}")) >> 24 <= 0x0f);
    }

    #[test]
    fn malformed_or_cancelled_proofs_never_return_fallback_tokens() {
        for difficulty in ["", "000000000", "no"] {
            assert_eq!(
                solve_proof(
                    "seed",
                    difficulty,
                    vec![json!(0); 25],
                    CancellationToken::new()
                ),
                Err(SentinelFailure::InvalidProofChallenge)
            );
        }
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            solve_proof("seed", "f", vec![json!(0); 25], cancel),
            Err(SentinelFailure::Cancelled)
        );
    }
}
