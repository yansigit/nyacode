//! Metadata-only references. These types never contain download URLs or file
//! bytes and do not implement Debug, so generic diagnostics cannot print them.

use crate::stream::StreamError;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use url::Url;

/// An exact backend citation marker and its metadata-only links. Rendering
/// replaces only this marker; it must not treat reference text as instructions.
#[derive(Clone, Serialize, Deserialize)]
pub struct Citation {
    pub marker: String,
    pub urls: Vec<String>,
}

impl Citation {
    fn parse(value: &Value) -> Option<Self> {
        let marker = value.get("matched_text")?.as_str()?;
        if marker.len() > 1024
            || !marker.starts_with("\u{e200}cite\u{e202}")
            || !marker.ends_with('\u{e201}')
        {
            return None;
        }
        let links = value.get("safe_urls")?.as_array()?;
        if links.len() > 16 {
            return None;
        }
        let mut urls = Vec::new();
        for link in links {
            let text = link.as_str()?;
            if text.len() > 4096 || text.chars().any(char::is_control) {
                return None;
            }
            let parsed = Url::parse(text).ok()?;
            if !matches!(parsed.scheme(), "http" | "https")
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.query_pairs().any(|(key, _)| {
                    matches!(
                        key.to_ascii_lowercase().as_str(),
                        "sig" | "signature" | "x-amz-signature" | "x-goog-signature"
                    )
                })
            {
                return None;
            }
            if !urls.iter().any(|url| url == text) {
                urls.push(text.to_owned());
            }
        }
        Some(Self {
            marker: marker.into(),
            urls,
        })
    }

    pub(crate) fn weight(&self) -> usize {
        self.marker.len() + self.urls.iter().map(String::len).sum::<usize>() + 64
    }
}

pub(crate) fn citations(value: &Value) -> Result<(Vec<Citation>, bool), StreamError> {
    let Some(value) = value
        .pointer("/metadata/content_references")
        .filter(|v| !v.is_null())
    else {
        return Ok((Vec::new(), false));
    };
    let Some(entries) = value.as_array() else {
        return Ok((Vec::new(), true));
    };
    if entries.len() > 256 {
        return Err(StreamError::ContentLimit);
    }
    let mut citations = Vec::new();
    let mut unsupported = false;
    for entry in entries {
        if let Some(citation) = Citation::parse(entry) {
            citations.push(citation);
        } else {
            unsupported = true;
        }
    }
    Ok((citations, unsupported))
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ImageReference {
    pub file_id: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub size_bytes: Option<u64>,
}

impl ImageReference {
    pub(crate) fn parse(value: &Value) -> Option<Self> {
        if value.get("content_type")?.as_str()? != "image_asset_pointer" {
            return None;
        }
        // Saved history rewrites uploaded images to sediment references. Both
        // observed namespaces contain an opaque ID, never a downloadable URL.
        // Retain metadata only; this ID is not an upload handle or permission
        // to fetch an asset.
        let pointer = value.get("asset_pointer")?.as_str()?;
        let file_id = pointer
            .strip_prefix("file-service://")
            .or_else(|| pointer.strip_prefix("sediment://"))?;
        if !crate::probe::valid_id(file_id) {
            return None;
        }
        Some(Self {
            file_id: file_id.into(),
            width: dimension(value.get("width")),
            height: dimension(value.get("height")),
            size_bytes: value.get("size_bytes").and_then(Value::as_u64),
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AttachmentReference {
    pub file_id: String,
    pub name: String,
    pub mime_type: Option<String>,
    pub size_bytes: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl AttachmentReference {
    pub(crate) fn parse(value: &Value) -> Option<Self> {
        let file_id = value.get("id")?.as_str()?;
        if !crate::probe::valid_id(file_id) {
            return None;
        }
        let name = value.get("name")?.as_str()?;
        if name.is_empty() || name.len() > 1024 {
            return None;
        }
        let mime_type = value
            .get("mime_type")
            .and_then(Value::as_str)
            .filter(|mime| mime.len() <= 256 && mime.bytes().all(|b| b.is_ascii_graphic()))
            .map(str::to_owned);
        Some(Self {
            file_id: file_id.into(),
            name: name.into(),
            mime_type,
            size_bytes: value.get("size").and_then(Value::as_u64),
            width: dimension(value.get("width")),
            height: dimension(value.get("height")),
        })
    }

    pub(crate) fn weight(&self) -> usize {
        128 + self.file_id.len() + self.name.len() + self.mime_type.as_ref().map_or(0, String::len)
    }
}

fn dimension(value: Option<&Value>) -> Option<u32> {
    value
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v > 0)
}

pub(crate) fn attachments(value: &Value) -> Result<(Vec<AttachmentReference>, bool), StreamError> {
    let Some(value) = value
        .pointer("/metadata/attachments")
        .filter(|v| !v.is_null())
    else {
        return Ok((Vec::new(), false));
    };
    let Some(entries) = value.as_array() else {
        return Ok((Vec::new(), true));
    };
    if entries.len() > 256 {
        return Err(StreamError::ContentLimit);
    }
    let mut references = Vec::new();
    let mut unsupported = false;
    for entry in entries {
        if let Some(reference) = AttachmentReference::parse(entry) {
            references.push(reference);
        } else {
            unsupported = true;
        }
    }
    Ok((references, unsupported))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn citations_retain_only_bounded_markers_and_public_http_links() {
        let marker = "\u{e200}cite\u{e202}turn0search0\u{e201}";
        let (references, unsupported) = citations(&json!({"metadata":{"content_references":[{"matched_text":marker,"safe_urls":["https://example.org/page","https://example.org/page"],"title":"not retained","download_url":"https://storage.invalid/?sig=secret"}]}})).unwrap();
        assert!(!unsupported);
        assert_eq!(references[0].marker, marker);
        assert_eq!(references[0].urls, ["https://example.org/page"]);
        let encoded = serde_json::to_string(&references).unwrap();
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("not retained"));
        for url in [
            "javascript:alert(1)",
            "file:///tmp/private",
            "https://user:secret@example.org/",
            "https://storage.invalid/?X-Amz-Signature=secret",
            "https://storage.invalid/?sig=secret",
        ] {
            let (references, unsupported) = citations(&json!({"metadata":{"content_references":[{"matched_text":marker,"safe_urls":[url]}]}})).unwrap();
            assert!(references.is_empty());
            assert!(unsupported);
        }
        let (references, unsupported) = citations(&json!({"metadata":{"content_references":[{"matched_text":"arbitrary prose","safe_urls":["https://example.org"]}]}})).unwrap();
        assert!(references.is_empty());
        assert!(unsupported);
    }

    #[test]
    fn citation_count_and_link_count_are_bounded() {
        assert!(matches!(
            citations(&json!({"metadata":{"content_references":vec![json!({});257]}})),
            Err(StreamError::ContentLimit)
        ));
        assert!(Citation::parse(&json!({"matched_text":"\u{e200}cite\u{e202}turn0search0\u{e201}","safe_urls":vec!["https://example.org";17]})).is_none());
    }

    #[test]
    fn metadata_never_retains_storage_urls_or_unknown_fields() {
        let reference = AttachmentReference::parse(&json!({"id":"file-fixture","name":"fixture.pdf","mime_type":"application/pdf","size":42,"download_url":"https://storage.invalid/?sig=secret","unknown":{"text":"private"}})).unwrap();
        let normalized = serde_json::to_string(&reference).unwrap();
        assert!(!normalized.contains("secret"));
        assert!(!normalized.contains("private"));
        assert!(!normalized.contains("https"));
        for pointer in [
            "https://storage.invalid/?sig=secret",
            "file-service://file-fixture/path",
            "sediment://unknown/path",
            "sediment://unknown?sig=secret",
            "sediment://unknown#fragment",
            "sediment://",
            "unobserved://unknown",
        ] {
            assert!(
                ImageReference::parse(
                    &json!({"content_type":"image_asset_pointer","asset_pointer":pointer})
                )
                .is_none()
            );
        }
        let (references, unsupported) =
            attachments(&json!({"metadata":{"attachments":[{"unknown":"private"}]}})).unwrap();
        assert!(references.is_empty());
        assert!(unsupported);
    }

    #[test]
    fn saved_image_references_keep_only_bounded_observed_metadata() {
        for scheme in ["file-service", "sediment"] {
            let image = ImageReference::parse(&json!({
                "content_type":"image_asset_pointer",
                "asset_pointer":format!("{scheme}://file-fixture"),
                "size_bytes":42,"width":10,"height":20,
                "fovea":null,"metadata":{"secret":"discarded"}
            }))
            .unwrap();
            assert_eq!(image.file_id, "file-fixture");
            assert_eq!(image.width, Some(10));
            assert_eq!(image.height, Some(20));
            assert_eq!(image.size_bytes, Some(42));
            let encoded = serde_json::to_string(&image).unwrap();
            assert!(!encoded.contains("secret"));
            assert!(!encoded.contains("discarded"));
            assert!(!encoded.contains("://"));
        }
    }
}
