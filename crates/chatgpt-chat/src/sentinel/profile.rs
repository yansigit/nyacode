use super::Result;
use super::SentinelFailure;
use super::storage::MemoryStorage;
use super::storage::StorageKind;
use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use regex::Regex;
use serde_json::Value;
use serde_json::json;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Instant;
use url::Url;

// Must match wreq-util's Chrome136/Linux profile. A transport regression test
// pins the actual wire header independently of this fingerprint.
pub(crate) const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36";
// One declared virtual display. These are profile settings, not measurements
// taken from a browser, desktop session, or the terminal's physical monitor.
pub(super) const SCREEN_WIDTH: u32 = 1920;
pub(super) const SCREEN_HEIGHT: u32 = 1080;
pub(super) const SCREEN_COLOR_DEPTH: u8 = 24;
pub(super) const DEVICE_MEMORY_GIB: u8 = 8;
pub(super) const HARDWARE_CONCURRENCY: u8 = 8;
pub(super) const JS_HEAP_LIMIT: u64 = 4_294_967_296;

/// Only bounded, non-secret metadata is retained from the unauthenticated page.
pub(crate) struct Bootstrap {
    pub origin: String,
    pub build: String,
    pub scripts: Vec<String>,
    pub router: Result<serde_json::Map<String, Value>>,
}

impl Bootstrap {
    pub fn parse(html: &[u8], origin: &str) -> Result<Self> {
        static BUILD: LazyLock<std::result::Result<Regex, regex::Error>> =
            LazyLock::new(|| Regex::new(r#"\bdata-build=["']([^"'<>]{1,256})["']"#));
        static SCRIPT: LazyLock<std::result::Result<Regex, regex::Error>> = LazyLock::new(|| {
            Regex::new(r#"(?i)<script\b[^>]{0,4096}\bsrc=["']([^"'<>]{1,2048})["']"#)
        });
        let build_pattern = BUILD
            .as_ref()
            .map_err(|_| SentinelFailure::BootstrapMetadataMissing)?;
        let script_pattern = SCRIPT
            .as_ref()
            .map_err(|_| SentinelFailure::BootstrapMetadataMissing)?;
        if html.len() > 8 * 1024 * 1024 {
            return Err(SentinelFailure::InputLimit);
        }
        let html =
            std::str::from_utf8(html).map_err(|_| SentinelFailure::BootstrapMetadataMissing)?;
        let base = Url::parse(origin).map_err(|_| SentinelFailure::BootstrapMetadataMissing)?;
        let build = build_pattern
            .captures(html)
            .map(|c| c[1].to_owned())
            .ok_or(SentinelFailure::BootstrapMetadataMissing)?;
        let mut scripts = Vec::new();
        for capture in script_pattern.captures_iter(html).take(64) {
            let url = base
                .join(&capture[1])
                .map_err(|_| SentinelFailure::BootstrapMetadataMissing)?;
            if url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && (url.origin() == base.origin() || url.host_str() == Some("cdn.oaistatic.com"))
            {
                scripts.push(url.to_string());
            }
        }
        if scripts.is_empty() {
            return Err(SentinelFailure::BootstrapMetadataMissing);
        }
        Ok(Self {
            origin: base.origin().ascii_serialization(),
            build,
            scripts,
            router: router_metadata(html),
        })
    }
}

/// Read only React Router's literal server handoff metadata. This is not a
/// JavaScript evaluator: streamed loader data, application globals and later
/// assignments are unavailable. Neither session data nor source text survives.
fn router_metadata(html: &str) -> Result<serde_json::Map<String, Value>> {
    static ASSIGNMENT: LazyLock<std::result::Result<Regex, regex::Error>> = LazyLock::new(|| {
        Regex::new(r#"<(?i:script)\b([^>]{0,4096})>\s*window\.__reactRouterContext\s*=\s*"#)
    });
    static SOURCE: LazyLock<std::result::Result<Regex, regex::Error>> =
        LazyLock::new(|| Regex::new(r#"(?i)\bsrc\s*="#));
    let mut matches = ASSIGNMENT
        .as_ref()
        .map_err(|_| SentinelFailure::UnsupportedRouterAssignment)?
        .captures_iter(html);
    let assignment = matches.next().ok_or_else(|| {
        if html.contains("__reactRouterContext") {
            SentinelFailure::UnsupportedRouterAssignment
        } else {
            SentinelFailure::UnsupportedRouterContext
        }
    })?;
    if matches.next().is_some()
        || SOURCE
            .as_ref()
            .map_err(|_| SentinelFailure::UnsupportedRouterAssignment)?
            .is_match(&assignment[1])
    {
        return Err(SentinelFailure::UnsupportedRouterAssignment);
    }
    let remainder = &html[assignment
        .get(0)
        .ok_or(SentinelFailure::UnsupportedRouterAssignment)?
        .end()..];
    // A fixed byte window bounds parsing and scratch allocations. A truncated
    // UTF-8 value is rejected by the JSON decoder.
    let length = remainder.len().min(16 * 1024);
    let bytes = &remainder.as_bytes()[..length];
    let mut values = serde_json::Deserializer::from_slice(bytes).into_iter::<Value>();
    let value = values
        .next()
        .ok_or(SentinelFailure::InvalidRouterMetadata)?
        .map_err(|e| {
            if e.is_eof() && length == 16 * 1024 {
                SentinelFailure::RouterMetadataLimit
            } else {
                SentinelFailure::InvalidRouterMetadata
            }
        })?;
    if !bytes[values.byte_offset()..]
        .trim_ascii_start()
        .starts_with(b";")
    {
        return Err(SentinelFailure::UnsupportedRouterAssignment);
    }
    let fields = value
        .as_object()
        .ok_or(SentinelFailure::UnsupportedRouterMetadataShape)?;
    let mut retained = serde_json::Map::new();
    for (key, value) in fields {
        let valid = match key.as_str() {
            "ssr" | "isSpaMode" => value.is_boolean(),
            "basename" => value
                .as_str()
                .is_some_and(|v| v.starts_with('/') && v.len() <= 128 && !v.contains(['?', '#'])),
            "future" => value.as_object().is_some_and(|v| {
                v.len() <= 64
                    && v.iter().all(|(key, value)| {
                        key.len() <= 128
                            && !matches!(key.as_str(), "__proto__" | "constructor" | "prototype")
                            && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                            && value.is_boolean()
                    })
            }),
            "routeDiscovery" => value.as_object().is_some_and(|v| {
                v.iter().all(|(key, value)| match key.as_str() {
                    "mode" => matches!(value.as_str(), Some("initial" | "lazy")),
                    "manifestPath" => value.as_str().is_some_and(|v| {
                        v.starts_with('/') && v.len() <= 128 && !v.contains(['?', '#'])
                    }),
                    _ => false,
                })
            }),
            _ => continue,
        };
        if !valid {
            return Err(SentinelFailure::UnsupportedRouterMetadataShape);
        }
        retained.insert(key.clone(), value.clone());
    }
    Ok(retained)
}

/// A single declared emulation environment; never rerandomized per request.
/// DOM measurements, browser-imported storage, geolocation and arbitrary properties are
/// intentionally unavailable. The interpreter fails if they are required.
pub(crate) struct Profile {
    pub bootstrap: Bootstrap,
    pub device: String,
    pub started: Instant,
    pub epoch_ms: i64,
    local_storage: Mutex<MemoryStorage>,
    session_storage: Mutex<MemoryStorage>,
}

impl Profile {
    pub fn new(bootstrap: Bootstrap) -> Self {
        Self {
            bootstrap,
            device: uuid::Uuid::new_v4().to_string(),
            started: Instant::now(),
            epoch_ms: chrono::Utc::now().timestamp_millis(),
            local_storage: Mutex::default(),
            session_storage: Mutex::default(),
        }
    }

    pub(super) fn storage(&self, kind: StorageKind) -> &Mutex<MemoryStorage> {
        match kind {
            StorageKind::Local => &self.local_storage,
            StorageKind::Session => &self.session_storage,
        }
    }

    pub fn config(&self) -> Vec<Value> {
        // Values describe this explicit native emulation environment. They do
        // not claim to be measurements from a browser or imported localStorage.
        vec![
            json!(SCREEN_WIDTH + SCREEN_HEIGHT),
            json!(
                chrono::Utc::now()
                    .format("%a %b %d %Y %H:%M:%S GMT+0000 (Coordinated Universal Time)")
                    .to_string()
            ),
            json!(JS_HEAP_LIMIT),
            json!(1),
            json!(USER_AGENT),
            json!(self.bootstrap.scripts[0]),
            json!(self.bootstrap.build),
            json!("en-US"),
            json!("en-US,en"),
            json!(0),
            json!("vendor−Google Inc."),
            json!("location"),
            json!("navigator"),
            json!(self.started.elapsed().as_secs_f64() * 1000.0),
            json!(self.device),
            json!(""),
            json!(HARDWARE_CONCURRENCY),
            json!(self.epoch_ms),
            json!(0),
            json!(0),
            json!(0),
            json!(0),
            json!(0),
            json!(0),
            json!(0),
        ]
    }

    pub fn requirements(&self) -> Result<(Vec<Value>, String)> {
        let config = self.config();
        let bytes = serde_json::to_vec(&config).map_err(|_| SentinelFailure::MalformedProgram)?;
        // This explicit candidate revision follows Aurora's two-phase wire
        // contract. No legacy hash/token fallback is attempted.
        let p = format!("gAAAAAC{}~S", BASE64_STANDARD.encode(bytes));
        Ok((config, p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_handoff_retains_only_literal_non_secret_metadata() {
        let html = r#"<script nonce="fixture">window.__reactRouterContext = {"ssr":true,"isSpaMode":false,"basename":"/","future":{"v8_middleware":false},"routeDiscovery":{"mode":"lazy","manifestPath":"/__manifest"},"loaderData":{"private":"never retained"}};window.__reactRouterContext.stream = mustNotExecute();</script>"#;
        let metadata = router_metadata(html).unwrap();
        assert_eq!(metadata["ssr"], true);
        assert_eq!(metadata["isSpaMode"], false);
        assert_eq!(metadata["future"], json!({"v8_middleware":false}));
        assert!(!metadata.contains_key("loaderData"));
        assert!(
            !serde_json::to_string(&metadata)
                .unwrap()
                .contains("private")
        );
    }

    #[test]
    fn router_handoff_rejects_expressions_duplicates_and_oversized_values() {
        for html in [
            "<script>window.__reactRouterContext = makeContext();</script>",
            "<script>window.__reactRouterContext = {} || {};</script>",
            "<script src='/external.js'>window.__reactRouterContext = {};</script>",
            "<script>window.__reactRouterContext = {}; </script><script>window.__reactRouterContext = {};</script>",
            "<script>window.__reactRouterContext = {\"future\":{\"unexpected\":\"value\"}};</script>",
        ] {
            assert!(router_metadata(html).is_err());
        }
        let huge = format!(
            "<script>window.__reactRouterContext = {{\"future\":{{\"{}\":true}}}};</script>",
            "x".repeat(16 * 1024)
        );
        assert_eq!(
            router_metadata(&huge),
            Err(SentinelFailure::RouterMetadataLimit)
        );
    }

    #[test]
    fn bootstrap_keeps_only_required_bounded_metadata() {
        let html = br#"<html data-build="build-1"><script src="/assets/app.js"></script><script src="https://other.invalid/private.js?token=secret"></script><p>private</p>"#;
        let metadata = Bootstrap::parse(html, "https://chatgpt.com").unwrap();
        assert_eq!(metadata.build, "build-1");
        assert_eq!(metadata.scripts, ["https://chatgpt.com/assets/app.js"]);
        assert!(Bootstrap::parse(b"<html></html>", "https://chatgpt.com").is_err());
    }

    #[test]
    fn fingerprint_uses_one_profile_device_and_observed_build() {
        let profile = Profile::new(Bootstrap {
            origin: "https://chatgpt.com".into(),
            build: "fixture-build".into(),
            scripts: vec!["https://cdn.oaistatic.com/fixture.js".into()],
            router: Err(SentinelFailure::UnsupportedRouterContext),
        });
        let (config, p) = profile.requirements().unwrap();
        assert_eq!(config.len(), 25);
        assert_eq!(config[4], USER_AGENT);
        assert_eq!(config[6], "fixture-build");
        assert_eq!(config[14], profile.config()[14]);
        let encoded = p
            .strip_prefix("gAAAAAC")
            .unwrap()
            .strip_suffix("~S")
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&BASE64_STANDARD.decode(encoded).unwrap()).unwrap(),
            json!(config)
        );
    }
}
