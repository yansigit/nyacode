//! Native ChatGPT file allocation, isolated signed transfer, and processing.
//! No upload is replayed automatically. Signed URLs and file bytes stay in memory.

mod processing;

use crate::ChatError;
use crate::ChatProbe;
use crate::attachment::AttachmentMetadata;
use crate::attachment::StagedAttachment;
use crate::journal::RunScope;
use crate::probe::bounded_body;
use crate::probe::classify_status;
use crate::probe::transport_error;
use crate::probe::valid_id;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::Method;
use processing::Processing;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadStage {
    Allocating,
    Uploading,
    Processing,
    Ready,
}

/// Sanitized diagnostics contain no file names, IDs, URLs, or response values.
#[derive(Default, Debug, Serialize)]
pub struct UploadObservation {
    pub allocation_status: Option<u16>,
    pub storage_status: Option<u16>,
    pub processing_status: Option<u16>,
    pub ready: bool,
    pub error: Option<ChatError>,
}

/// An opaque, expiring capability. It cannot be serialized or reconstructed from
/// a server file ID. The service retains it and exposes only its random handle.
pub struct ReadyUpload {
    handle: Uuid,
    scope: RunScope,
    generation: u64,
    session_id: Uuid,
    draft: Uuid,
    temporary: bool,
    created: Instant,
    file_id: String,
    metadata: AttachmentMetadata,
    total_tokens: Option<u64>,
}

impl ReadyUpload {
    pub fn handle(&self) -> Uuid {
        self.handle
    }
    pub fn is_expired(&self) -> bool {
        self.created.elapsed() >= Duration::from_secs(30 * 60)
    }

    pub fn metadata(&self) -> &AttachmentMetadata {
        &self.metadata
    }

    /// Typed metadata for a saved service journal. No signed URL or file bytes.
    pub fn recovery_metadata(&self) -> crate::journal::SavedAttachment {
        self.journal_metadata()
    }

    pub(crate) fn journal_metadata(&self) -> crate::journal::SavedAttachment {
        crate::journal::SavedAttachment {
            server_file_id: self.file_id.clone(),
            metadata: self.metadata.clone(),
        }
    }

    pub(crate) fn validate(
        &self,
        probe: &ChatProbe,
        draft: Uuid,
        temporary: bool,
    ) -> Result<(), ChatError> {
        if self.draft != draft
            || self.temporary != temporary
            || self.is_expired()
            || self.generation != probe.auth.auth_change_state_receiver().borrow().generation
            || self.scope != probe.run_scope()?
            || probe.transport_session_id(self.generation)? != self.session_id
        {
            return Err(ChatError::StaleUpload);
        }
        Ok(())
    }

    pub(crate) fn association(&self) -> Value {
        let mut value = json!({"id":self.file_id,"name":self.metadata.name,
            "size":self.metadata.size,"mime_type":self.metadata.kind.mime()});
        if let Some(tokens) = self.total_tokens {
            value["file_token_size"] = json!(tokens);
        }
        if let Some((width, height)) = self.metadata.dimensions {
            value["width"] = json!(width);
            value["height"] = json!(height);
        }
        value
    }

    pub(crate) fn image_part(&self) -> Option<Value> {
        self.metadata.dimensions.map(|(width, height)| json!({"content_type":"image_asset_pointer",
            "asset_pointer":format!("file-service://{}",self.file_id),"size_bytes":self.metadata.size,
            "width":width,"height":height}))
    }
}

impl ChatProbe {
    pub(crate) fn attachment_message(
        &self,
        draft: Uuid,
        temporary: bool,
        files: &[ReadyUpload],
        text: &str,
    ) -> Result<(Value, Value), ChatError> {
        self.attachment_message_refs(draft, temporary, files.iter(), text)
    }

    pub(crate) fn attachment_message_refs<'a>(
        &self,
        draft: Uuid,
        temporary: bool,
        files: impl Iterator<Item = &'a ReadyUpload>,
        text: &str,
    ) -> Result<(Value, Value), ChatError> {
        let files: Vec<_> = files.take(21).collect();
        if files.len() > 20
            || files
                .iter()
                .try_fold(0u64, |sum, f| sum.checked_add(f.metadata.size))
                .is_none_or(|sum| sum > crate::attachment::MAX_SUBMISSION_BYTES as u64)
        {
            return Err(ChatError::InvalidInput);
        }
        let mut handles = std::collections::HashSet::new();
        for file in &files {
            file.validate(self, draft, temporary)?;
            if !handles.insert(file.handle) {
                return Err(ChatError::StaleUpload);
            }
        }
        let mut parts: Vec<Value> = files.iter().filter_map(|file| file.image_part()).collect();
        let content = if parts.is_empty() {
            json!({"content_type":"text","parts":[text]})
        } else {
            parts.push(json!(text));
            json!({"content_type":"multimodal_text","parts":parts})
        };
        let metadata = if files.is_empty() {
            json!({})
        } else {
            json!({"attachments":files.iter().map(|file| file.association()).collect::<Vec<_>>()})
        };
        Ok((content, metadata))
    }

    /// The caller retains the immutable selection on failure. A successful
    /// result contains metadata only, so dropping the selection frees its bytes.
    pub async fn upload_attachment(
        &self,
        draft: Uuid,
        temporary: bool,
        file: &StagedAttachment,
        cancel: &CancellationToken,
        progress: impl FnMut(UploadStage, Option<u8>),
    ) -> (Option<ReadyUpload>, UploadObservation) {
        let mut observation = UploadObservation::default();
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ChatError::LocalCancellation),
            result = Box::pin(self.upload_inner(draft, temporary, file, progress, &mut observation)) => result,
        };
        match result {
            Ok(ready) => {
                observation.ready = true;
                (Some(ready), observation)
            }
            Err(error) => {
                observation.error = Some(error);
                (None, observation)
            }
        }
    }

    async fn upload_inner(
        &self,
        draft: Uuid,
        temporary: bool,
        file: &StagedAttachment,
        mut progress: impl FnMut(UploadStage, Option<u8>),
        observation: &mut UploadObservation,
    ) -> Result<ReadyUpload, ChatError> {
        let scope = self.run_scope()?;
        let generation = self.auth.auth_change_state_receiver().borrow().generation;
        let metadata = file.metadata();
        let use_case = if metadata.kind.is_image() {
            "multimodal"
        } else {
            "ace_upload"
        };
        progress(UploadStage::Allocating, None);
        let (allocation, session_id): (Value, Uuid) = self.guarded_generation(generation, async {
            let request = self.request(Method::POST, "files", "files").await?;
            let session_id = self.transport_session_id(generation)?;
            let response = request
                .json(&json!({"file_name":metadata.name,"file_size":metadata.size,"use_case":use_case,
                    "mime_type":metadata.kind.mime(),"timezone_offset_min":0,
                    "entry_surface":"chat_composer","store_in_library":false,
                    "supports_direct_azure_multipart":false}))
                .send().await.map_err(transport_error)?;
            self.observe_response(&response);
            observation.allocation_status = Some(response.status().as_u16());
            classify_status(response.status(), response.headers())?;
            let allocation = serde_json::from_slice(&bounded_body(response).await?).map_err(|_| ChatError::ProtocolChanged)?;
            if self.transport_session_id(generation)? != session_id { return Err(ChatError::SessionChanged); }
            Ok((allocation, session_id))
        }).await?;
        let allocation = allocation_request(&allocation, metadata.kind.mime())?;
        let file_id = allocation.file_id;
        progress(UploadStage::Uploading, Some(0));
        self.guarded_generation(generation, async {
            if self.transport_session_id(generation)? != session_id {
                return Err(ChatError::SessionChanged);
            }
            let response = self
                .storage_request(allocation.url, generation)?
                .headers(allocation.headers)
                .body(file.upload_bytes())
                .send()
                .await
                .map_err(transport_error)?;
            // A storage error is not a ChatGPT authentication failure. Never
            // follow redirects, refresh ChatGPT credentials, or retry this PUT.
            observation.storage_status = Some(response.status().as_u16());
            self.observe_response(&response);
            if !response.status().is_success() {
                return Err(ChatError::UploadFailed);
            }
            let _ = bounded_body(response).await?;
            Ok(())
        })
        .await?;
        progress(UploadStage::Processing, Some(0));
        let mut response = self.guarded_generation(generation, async {
            let request = self.request(Method::POST, "files/process_upload_stream", "files/process_upload_stream").await?;
            if self.transport_session_id(generation)? != session_id { return Err(ChatError::SessionChanged); }
            let response = request
                .json(&json!({"file_id":file_id,"use_case":use_case,"index_for_retrieval":false,
                    "file_name":metadata.name,"mime_type":metadata.kind.mime(),"entry_surface":"chat_composer",
                    "metadata":{"store_in_library":false,"is_temporary_chat":temporary,"is_project_thread":false},
                    "client_sha256_digest":metadata.sha256}))
                .send().await.map_err(transport_error)?;
            self.observe_response(&response);
            observation.processing_status = Some(response.status().as_u16());
            classify_status(response.status(), response.headers())?;
            Ok(response)
        }).await?;
        let mut decoder = Processing::new(file_id.clone());
        tokio::time::timeout(Duration::from_secs(180), async {
            loop {
                let chunk = self
                    .guarded_generation(generation, async {
                        if self.transport_session_id(generation)? != session_id {
                            return Err(ChatError::SessionChanged);
                        }
                        response.chunk().await.map_err(transport_error)
                    })
                    .await?;
                let Some(chunk) = chunk else {
                    break;
                };
                decoder.push(&chunk)?;
                progress(UploadStage::Processing, decoder.progress);
            }
            decoder.finish()
        })
        .await
        .map_err(|_| ChatError::Timeout)??;
        let ready = ReadyUpload {
            handle: Uuid::new_v4(),
            scope,
            generation,
            session_id,
            draft,
            temporary,
            created: Instant::now(),
            file_id,
            metadata: metadata.clone(),
            total_tokens: decoder.total_tokens,
        };
        ready.validate(self, draft, temporary)?;
        progress(UploadStage::Ready, Some(100));
        Ok(ready)
    }
}

struct Allocation {
    file_id: String,
    url: Url,
    headers: HeaderMap,
}

pub(crate) fn validate_storage_url(url: &Url) -> Result<(), ChatError> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.as_str().len() > 32768
    {
        return Err(ChatError::UnsupportedCapability);
    }
    Ok(())
}

fn allocation_request(value: &Value, mime: &str) -> Result<Allocation, ChatError> {
    if value.get("status").and_then(Value::as_str) != Some("success") {
        return Err(ChatError::UploadFailed);
    }
    if let Some(strategy) = value
        .get("direct_library_upload_strategy")
        .filter(|v| !v.is_null())
        && strategy.as_str() != Some("single_put")
    {
        return Err(ChatError::UnsupportedCapability);
    }
    let file_id = value
        .get("file_id")
        .and_then(Value::as_str)
        .filter(|s| valid_id(s))
        .ok_or(ChatError::ProtocolChanged)?
        .to_owned();
    let url = Url::parse(
        value
            .get("upload_url")
            .and_then(Value::as_str)
            .ok_or(ChatError::ProtocolChanged)?,
    )
    .map_err(|_| ChatError::ProtocolChanged)?;
    validate_storage_url(&url)?;
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_str(mime).map_err(|_| ChatError::InvalidInput)?,
    );
    if !url
        .query_pairs()
        .any(|(key, _)| key.eq_ignore_ascii_case("x-amz-algorithm"))
    {
        headers.insert("x-ms-blob-type", HeaderValue::from_static("BlockBlob"));
        headers.insert("x-ms-version", HeaderValue::from_static("2020-04-08"));
    }
    if let Some(extra) = value.get("upload_headers").filter(|v| !v.is_null()) {
        let extra = extra
            .as_object()
            .filter(|m| m.len() <= 16)
            .ok_or(ChatError::ProtocolChanged)?;
        for (name, value) in extra {
            let name = name.to_ascii_lowercase();
            if !matches!(
                name.as_str(),
                "content-type"
                    | "content-md5"
                    | "x-ms-blob-type"
                    | "x-ms-version"
                    | "x-ms-blob-content-type"
                    | "x-amz-checksum-sha256"
                    | "x-amz-content-sha256"
            ) {
                return Err(ChatError::UnsupportedCapability);
            }
            let value = value
                .as_str()
                .filter(|s| s.len() <= 1024)
                .ok_or(ChatError::ProtocolChanged)?;
            if (matches!(name.as_str(), "content-type" | "x-ms-blob-content-type") && value != mime)
                || (name == "x-ms-blob-type" && value != "BlockBlob")
            {
                return Err(ChatError::ProtocolChanged);
            }
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).map_err(|_| ChatError::ProtocolChanged)?,
                HeaderValue::from_str(value).map_err(|_| ChatError::ProtocolChanged)?,
            );
        }
    }
    Ok(Allocation {
        file_id,
        url,
        headers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn storage_is_https_only_and_never_accepts_chat_credentials_or_redirect_options() {
        let mut value = json!({"status":"success","file_id":"file-fixture",
            "upload_url":"https://storage.example/blob?sig=private"});
        let allocation = allocation_request(&value, "text/plain").unwrap();
        assert_eq!(allocation.headers.len(), 3);
        assert_eq!(allocation.headers["x-ms-blob-type"], "BlockBlob");
        for header in [
            "Authorization",
            "Cookie",
            "chatgpt-account-id",
            "OpenAI-Sentinel-Proof-Token",
            "Host",
            "Location",
            "Proxy-Authorization",
        ] {
            value["upload_headers"] = json!({header:"private"});
            assert!(matches!(
                allocation_request(&value, "text/plain"),
                Err(ChatError::UnsupportedCapability)
            ));
        }
        value.as_object_mut().unwrap().remove("upload_headers");
        for url in [
            "http://storage.example/blob",
            "https://user:pass@storage.example/blob",
            "/backend-api/estuary/upload_content_bytes",
            "https://storage.example/blob#fragment",
        ] {
            value["upload_url"] = json!(url);
            assert!(allocation_request(&value, "text/plain").is_err());
        }
        value["upload_url"] =
            json!("https://storage.example/blob?X-Amz-Algorithm=AWS4-HMAC-SHA256");
        let allocation = allocation_request(&value, "text/plain").unwrap();
        assert_eq!(allocation.headers.len(), 1);
    }
}
