//! Immutable attachment snapshots. File contents never implement Debug or
//! Serialize and never enter the process-global prompt-image cache.

use bytes::Bytes;
use image::ImageFormat;
use image::ImageReader;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::fs::OpenOptions;
use std::io::Cursor;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

pub const MAX_SUBMISSION_BYTES: usize = 32 * 1024 * 1024;
const MAX_STAGED_BYTES: usize = 128 * 1024 * 1024;
const MAX_FILES: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Png,
    Jpeg,
    Webp,
    Pdf,
    Utf8,
}

impl AttachmentKind {
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Pdf => "application/pdf",
            Self::Utf8 => "text/plain",
        }
    }
    pub fn is_image(self) -> bool {
        matches!(self, Self::Png | Self::Jpeg | Self::Webp)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentError {
    #[error("the selected file is missing or unreadable")]
    Unreadable,
    #[error("the selected file changed while being read")]
    Changed,
    #[error("the attachment format is unsupported or invalid")]
    InvalidFormat,
    #[error("the attachment exceeds a size or decoding limit")]
    Limit,
    #[error("attachment staging is at capacity; keep the selected files and try later")]
    Capacity,
    #[error("attachment staging was cancelled")]
    Cancelled,
}

/// Saved drafts may retain this metadata, never the file contents. Clipboard
/// images have no source path and require reattachment after a restart.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentMetadata {
    pub source: Option<PathBuf>,
    pub name: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub sha256: String,
    pub kind: AttachmentKind,
    pub dimensions: Option<(u32, u32)>,
}

pub struct StagedAttachment {
    metadata: AttachmentMetadata,
    bytes: Bytes,
}

impl StagedAttachment {
    pub fn metadata(&self) -> &AttachmentMetadata {
        &self.metadata
    }
    /// Clones share the same allocation and retain its staging permit. Upload
    /// cancellation cannot release capacity while a body still owns its bytes.
    pub(crate) fn upload_bytes(&self) -> Bytes {
        self.bytes.clone()
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

enum ClipboardInput {
    Bytes(Vec<u8>),
    Base64(String),
}

impl ClipboardInput {
    fn decoded_size(&self) -> Result<usize, AttachmentError> {
        match self {
            Self::Bytes(bytes) => Ok(bytes.len()),
            Self::Base64(encoded) => {
                const MAX_ENCODED: usize = (20usize * 1024 * 1024).div_ceil(3) * 4;
                if encoded.len() > MAX_ENCODED {
                    return Err(AttachmentError::Limit);
                }
                if encoded.is_empty() || encoded.len() % 4 != 0 {
                    return Err(AttachmentError::InvalidFormat);
                }
                let padding = encoded.bytes().rev().take_while(|b| *b == b'=').count();
                if padding > 2 {
                    return Err(AttachmentError::InvalidFormat);
                }
                Ok(encoded.len() / 4 * 3 - padding)
            }
        }
    }
}

struct LeasedBytes {
    bytes: Vec<u8>,
    _lease: Arc<OwnedSemaphorePermit>,
}

impl AsRef<[u8]> for LeasedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// Share one instance per Chat service. Capacity failure does not consume or
/// mutate the caller's selections. Decoder scratch work is serialized.
pub struct AttachmentStager {
    memory: Arc<Semaphore>,
    decoder: Arc<Semaphore>,
}

impl Default for AttachmentStager {
    fn default() -> Self {
        Self::new()
    }
}

impl AttachmentStager {
    pub fn new() -> Self {
        Self {
            memory: Arc::new(Semaphore::new(MAX_STAGED_BYTES)),
            decoder: Arc::new(Semaphore::new(1)),
        }
    }

    /// Clipboard callers must pass their in-memory encoded image directly.
    /// No temporary file, generic paste history, or path fallback is involved.
    pub async fn snapshot_clipboard(
        &self,
        name: String,
        bytes: Vec<u8>,
        cancel: CancellationToken,
    ) -> Result<StagedAttachment, AttachmentError> {
        self.snapshot_clipboard_input(name, ClipboardInput::Bytes(bytes), cancel)
            .await
    }

    /// Trusted local RPCs reserve decoded staging capacity before base64 decoding.
    /// Encoded input is bounded as well; decoding never uses a temporary file.
    pub async fn snapshot_clipboard_base64(
        &self,
        name: String,
        encoded: String,
        cancel: CancellationToken,
    ) -> Result<StagedAttachment, AttachmentError> {
        self.snapshot_clipboard_input(name, ClipboardInput::Base64(encoded), cancel)
            .await
    }

    async fn snapshot_clipboard_input(
        &self,
        name: String,
        input: ClipboardInput,
        cancel: CancellationToken,
    ) -> Result<StagedAttachment, AttachmentError> {
        let size = input.decoded_size()?;
        if size > 20 * 1024 * 1024 || file_name(Path::new(&name))? != name {
            return Err(AttachmentError::Limit);
        }
        let lease = Arc::new(
            Arc::clone(&self.memory)
                .try_acquire_many_owned(size as u32)
                .map_err(|_| AttachmentError::Capacity)?,
        );
        let decoder = Arc::clone(&self.decoder)
            .try_acquire_owned()
            .map_err(|_| AttachmentError::Capacity)?;
        let worker_cancel = cancel.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let _decoder = decoder;
            check_cancel(&worker_cancel)?;
            let bytes = match input {
                ClipboardInput::Bytes(bytes) => bytes,
                ClipboardInput::Base64(encoded) => {
                    use base64::Engine;
                    let mut bytes = vec![0; size];
                    let written = base64::engine::general_purpose::STANDARD
                        .decode_slice(encoded, &mut bytes)
                        .map_err(|_| AttachmentError::InvalidFormat)?;
                    bytes.truncate(written);
                    bytes
                }
            };
            let (kind, dimensions) = validate(&bytes, &name, &worker_cancel)?;
            if !kind.is_image() {
                return Err(AttachmentError::InvalidFormat);
            }
            let metadata = AttachmentMetadata {
                source: None,
                name,
                size: bytes.len() as u64,
                modified: None,
                sha256: hex::encode(Sha256::digest(&bytes)),
                kind,
                dimensions,
            };
            Ok(StagedAttachment {
                metadata,
                bytes: Bytes::from_owner(LeasedBytes {
                    bytes,
                    _lease: lease,
                }),
            })
        });
        tokio::select! { biased; _ = cancel.cancelled() => Err(AttachmentError::Cancelled), result = worker => result.map_err(|_| AttachmentError::InvalidFormat)? }
    }

    /// Recovery never trusts unchanged size/mtime alone. Snapshot again and
    /// compare the actual content digest before making the selection reusable.
    pub async fn recover(
        &self,
        metadata: &AttachmentMetadata,
        cancel: CancellationToken,
    ) -> Result<StagedAttachment, AttachmentError> {
        let path = metadata.source.clone().ok_or(AttachmentError::Unreadable)?;
        let mut files = self.snapshot(vec![path], cancel).await?;
        let file = files.pop().ok_or(AttachmentError::Unreadable)?;
        if file.metadata.sha256 != metadata.sha256
            || file.metadata.size != metadata.size
            || file.metadata.kind != metadata.kind
            || file.metadata.dimensions != metadata.dimensions
        {
            return Err(AttachmentError::Changed);
        }
        Ok(file)
    }

    pub async fn snapshot(
        &self,
        paths: Vec<PathBuf>,
        cancel: CancellationToken,
    ) -> Result<Vec<StagedAttachment>, AttachmentError> {
        if paths.is_empty() || paths.len() > MAX_FILES {
            return Err(AttachmentError::Limit);
        }
        let memory = Arc::clone(&self.memory);
        let decoder = Arc::clone(&self.decoder)
            .try_acquire_owned()
            .map_err(|_| AttachmentError::Capacity)?;
        let worker_cancel = cancel.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let _decoder = decoder;
            check_cancel(&worker_cancel)?;
            let mut sources = Vec::new();
            let mut total = 0usize;
            for path in paths {
                // Reject special files before opening. On Unix a replacement
                // FIFO must not trap the worker between this check and open.
                if !std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                    return Err(AttachmentError::Unreadable);
                }
                let mut options = OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NONBLOCK);
                }
                let file = options
                    .open(&path)
                    .map_err(|_| AttachmentError::Unreadable)?;
                let metadata = file.metadata().map_err(|_| AttachmentError::Unreadable)?;
                if !metadata.is_file() {
                    return Err(AttachmentError::Unreadable);
                }
                let size = usize::try_from(metadata.len()).map_err(|_| AttachmentError::Limit)?;
                total = total.checked_add(size).ok_or(AttachmentError::Limit)?;
                if total > MAX_SUBMISSION_BYTES {
                    return Err(AttachmentError::Limit);
                }
                let name = file_name(&path)?;
                sources.push((file, path, name, metadata));
            }
            let lease = Arc::new(
                memory
                    .try_acquire_many_owned(total as u32)
                    .map_err(|_| AttachmentError::Capacity)?,
            );
            let mut output = Vec::new();
            for (mut file, path, name, before) in sources {
                check_cancel(&worker_cancel)?;
                let mut bytes = Vec::with_capacity(before.len() as usize);
                let mut chunk = [0u8; 65536];
                loop {
                    check_cancel(&worker_cancel)?;
                    let n = file
                        .read(&mut chunk)
                        .map_err(|_| AttachmentError::Unreadable)?;
                    if n == 0 {
                        break;
                    }
                    if bytes.len().saturating_add(n) > before.len() as usize {
                        return Err(AttachmentError::Changed);
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let after = file.metadata().map_err(|_| AttachmentError::Unreadable)?;
                if bytes.len() as u64 != before.len()
                    || after.len() != before.len()
                    || before.modified().ok() != after.modified().ok()
                {
                    return Err(AttachmentError::Changed);
                }
                let (kind, dimensions) = validate(&bytes, &name, &worker_cancel)?;
                let metadata = AttachmentMetadata {
                    source: Some(path),
                    name,
                    size: bytes.len() as u64,
                    modified: before.modified().ok(),
                    sha256: hex::encode(Sha256::digest(&bytes)),
                    kind,
                    dimensions,
                };
                output.push(StagedAttachment {
                    metadata,
                    bytes: Bytes::from_owner(LeasedBytes {
                        bytes,
                        _lease: Arc::clone(&lease),
                    }),
                });
            }
            check_cancel(&worker_cancel)?;
            Ok(output)
        });
        tokio::select! { biased; _ = cancel.cancelled() => Err(AttachmentError::Cancelled), result = worker => result.map_err(|_| AttachmentError::InvalidFormat)? }
    }
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), AttachmentError> {
    if cancel.is_cancelled() {
        Err(AttachmentError::Cancelled)
    } else {
        Ok(())
    }
}

fn file_name(path: &Path) -> Result<String, AttachmentError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && name.len() <= 255 && !name.chars().any(char::is_control))
        .map(str::to_owned)
        .ok_or(AttachmentError::InvalidFormat)
}

fn validate(
    bytes: &[u8],
    name: &str,
    cancel: &CancellationToken,
) -> Result<(AttachmentKind, Option<(u32, u32)>), AttachmentError> {
    check_cancel(cancel)?;
    if bytes.len() > MAX_SUBMISSION_BYTES {
        return Err(AttachmentError::Limit);
    }
    let extension = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if bytes.starts_with(b"%PDF-") || extension == "pdf" {
        validate_pdf(bytes, cancel)?;
        return Ok((AttachmentKind::Pdf, None));
    }
    if let Ok(format) = image::guess_format(bytes) {
        // ChatGPT's documented per-image ceiling is lower than the aggregate
        // submission budget. Enforce it before allocating decoder buffers.
        if bytes.len() > 20 * 1024 * 1024 {
            return Err(AttachmentError::Limit);
        }
        let kind = match format {
            ImageFormat::Png => AttachmentKind::Png,
            ImageFormat::Jpeg => AttachmentKind::Jpeg,
            ImageFormat::WebP => AttachmentKind::Webp,
            _ => return Err(AttachmentError::InvalidFormat),
        };
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(16384);
        limits.max_image_height = Some(16384);
        limits.max_alloc = Some(128 * 1024 * 1024);
        reader.limits(limits);
        let decoded = reader
            .decode()
            .map_err(|_| AttachmentError::InvalidFormat)?;
        check_cancel(cancel)?;
        return Ok((kind, Some((decoded.width(), decoded.height()))));
    }
    if matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif") {
        return Err(AttachmentError::InvalidFormat);
    }
    std::str::from_utf8(bytes).map_err(|_| AttachmentError::InvalidFormat)?;
    // Binary formats with incidental UTF-8 validity are not text attachments.
    if bytes.contains(&0) {
        return Err(AttachmentError::InvalidFormat);
    }
    Ok((AttachmentKind::Utf8, None))
}

fn validate_pdf(bytes: &[u8], cancel: &CancellationToken) -> Result<(), AttachmentError> {
    crate::pdf::validate(bytes, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::fs::File;

    #[cfg(unix)]
    #[tokio::test]
    async fn named_pipes_are_rejected_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        let path = std::env::temp_dir().join(format!("chat-pipe-{}", uuid::Uuid::new_v4()));
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a live, NUL-terminated path; mkfifo retains no pointer.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            AttachmentStager::new().snapshot(vec![path.clone()], CancellationToken::new()),
        )
        .await;
        std::fs::remove_file(path).unwrap();
        assert!(matches!(result, Ok(Err(AttachmentError::Unreadable))));
    }

    #[test]
    fn pdf_requires_a_valid_catalog_page_tree_and_cross_reference() {
        let bytes = include_bytes!("../fixtures/attachments/fixture.pdf");
        assert_eq!(validate_pdf(bytes, &CancellationToken::new()), Ok(()));
        let malformed = String::from_utf8(bytes.to_vec())
            .unwrap()
            .replace("/Count 1", "/Count 2");
        assert!(validate_pdf(malformed.as_bytes(), &CancellationToken::new()).is_err());
        let mut bytes = bytes.to_vec();
        bytes.truncate(bytes.len() / 2);
        assert!(validate_pdf(&bytes, &CancellationToken::new()).is_err());
    }

    #[tokio::test]
    async fn recovery_checks_actual_bytes_and_clipboard_snapshots_never_get_a_path() {
        let path = std::env::temp_dir().join(format!("chat-recovery-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"original").unwrap();
        let stager = AttachmentStager::new();
        let files = stager
            .snapshot(vec![path.clone()], CancellationToken::new())
            .await
            .unwrap();
        assert!(
            stager
                .recover(files[0].metadata(), CancellationToken::new())
                .await
                .is_ok()
        );
        std::fs::write(&path, b"modified").unwrap();
        assert!(matches!(
            stager
                .recover(files[0].metadata(), CancellationToken::new())
                .await,
            Err(AttachmentError::Changed)
        ));
        std::fs::remove_file(path).unwrap();
        assert!(matches!(
            stager
                .recover(files[0].metadata(), CancellationToken::new())
                .await,
            Err(AttachmentError::Unreadable)
        ));
        let image = stager
            .snapshot_clipboard(
                "clipboard.png".into(),
                include_bytes!("../fixtures/attachments/fixture.png").to_vec(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(image.metadata.source.is_none());
        assert!(matches!(
            stager
                .recover(image.metadata(), CancellationToken::new())
                .await,
            Err(AttachmentError::Unreadable)
        ));
    }

    #[tokio::test]
    async fn aggregate_limit_is_checked_before_allocating_or_decoding_bytes() {
        let path = std::env::temp_dir().join(format!("chat-limit-{}.txt", uuid::Uuid::new_v4()));
        File::create(&path)
            .unwrap()
            .set_len(MAX_SUBMISSION_BYTES as u64 / 2 + 1)
            .unwrap();
        let stager = AttachmentStager::new();
        assert!(matches!(
            stager
                .snapshot(vec![path.clone(), path.clone()], CancellationToken::new())
                .await,
            Err(AttachmentError::Limit)
        ));
        assert_eq!(stager.memory.available_permits(), MAX_STAGED_BYTES);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn formats_are_decoded_and_corruption_never_falls_back_to_text() {
        let cancel = CancellationToken::new();
        for (format, kind, name) in [
            (ImageFormat::Png, AttachmentKind::Png, "fixture.png"),
            (ImageFormat::Jpeg, AttachmentKind::Jpeg, "fixture.jpg"),
            (ImageFormat::WebP, AttachmentKind::Webp, "fixture.webp"),
        ] {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(12, 8)
                .write_to(&mut bytes, format)
                .unwrap();
            assert_eq!(
                validate(bytes.get_ref(), name, &cancel),
                Ok((kind, Some((12, 8))))
            );
            assert!(validate(&bytes.get_ref()[..12], name, &cancel).is_err());
        }
        assert_eq!(
            validate("valid UTF-8 é😀".as_bytes(), "fixture.rs", &cancel),
            Ok((AttachmentKind::Utf8, None))
        );
        for (bytes, name) in [
            (b"text".as_slice(), "bad.png"),
            (b"%PDF-1.7\nmalformed", "bad.pdf"),
            (b"\xff", "bad.txt"),
            (b"text\0binary", "bad.txt"),
        ] {
            assert_eq!(
                validate(bytes, name, &cancel),
                Err(AttachmentError::InvalidFormat)
            );
        }
        cancel.cancel();
        assert_eq!(
            validate(b"text", "fixture.txt", &cancel),
            Err(AttachmentError::Cancelled)
        );
    }

    #[tokio::test]
    async fn immutable_snapshots_keep_capacity_until_all_upload_bodies_drop() {
        let path =
            std::env::temp_dir().join(format!("chat-attachment-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"fixture content").unwrap();
        let stager = AttachmentStager::new();
        let files = stager
            .snapshot(vec![path.clone()], CancellationToken::new())
            .await
            .unwrap();
        std::fs::write(&path, b"changed").unwrap();
        assert_eq!(files[0].bytes(), b"fixture content");
        assert_eq!(files[0].metadata().size, 15);
        let body = files[0].upload_bytes();
        drop(files);
        assert_eq!(stager.memory.available_permits(), MAX_STAGED_BYTES - 15);
        drop(body);
        assert_eq!(stager.memory.available_permits(), MAX_STAGED_BYTES);
        let reservation = Arc::clone(&stager.memory)
            .acquire_many_owned(MAX_STAGED_BYTES as u32)
            .await
            .unwrap();
        assert!(matches!(
            stager
                .snapshot(vec![path.clone()], CancellationToken::new())
                .await,
            Err(AttachmentError::Capacity)
        ));
        drop(reservation);
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(test)]
mod clipboard_rpc_tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn encoded_clipboard_reserves_shared_capacity_and_releases_with_last_owner() {
        let image = include_bytes!("../fixtures/attachments/fixture.png");
        let stager = AttachmentStager::new();
        let file = stager
            .snapshot_clipboard_base64(
                "clipboard.png".into(),
                STANDARD.encode(image),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(file.bytes(), image);
        assert!(file.metadata().source.is_none());
        assert_eq!(
            stager.memory.available_permits(),
            MAX_STAGED_BYTES - image.len()
        );
        let body = file.upload_bytes();
        drop(file);
        assert_eq!(
            stager.memory.available_permits(),
            MAX_STAGED_BYTES - image.len()
        );
        drop(body);
        assert_eq!(stager.memory.available_permits(), MAX_STAGED_BYTES);
        let _occupied = Arc::clone(&stager.memory)
            .acquire_many_owned(MAX_STAGED_BYTES as u32)
            .await
            .unwrap();
        assert!(matches!(
            stager
                .snapshot_clipboard_base64(
                    "clipboard.png".into(),
                    STANDARD.encode(image),
                    CancellationToken::new()
                )
                .await,
            Err(AttachmentError::Capacity)
        ));
    }

    #[tokio::test]
    async fn malformed_nonimage_and_oversized_clipboard_inputs_are_rejected() {
        let stager = AttachmentStager::new();
        for encoded in [
            "".into(),
            "====".into(),
            "bad".into(),
            "AA=A".into(),
            "@@==".into(),
            STANDARD.encode(b"literal text"),
        ] {
            assert!(matches!(
                stager
                    .snapshot_clipboard_base64(
                        "clipboard.png".into(),
                        encoded,
                        CancellationToken::new()
                    )
                    .await,
                Err(AttachmentError::InvalidFormat)
            ));
            assert_eq!(stager.memory.available_permits(), MAX_STAGED_BYTES);
        }
        let oversized = "A".repeat((20usize * 1024 * 1024).div_ceil(3) * 4 + 4);
        assert!(matches!(
            stager
                .snapshot_clipboard_base64(
                    "clipboard.png".into(),
                    oversized,
                    CancellationToken::new()
                )
                .await,
            Err(AttachmentError::Limit)
        ));
        assert_eq!(stager.memory.available_permits(), MAX_STAGED_BYTES);
    }

    #[tokio::test]
    async fn cancelled_clipboard_does_not_decode_or_retain_capacity() {
        let stager = AttachmentStager::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            stager
                .snapshot_clipboard_base64("clipboard.png".into(), "@@==".into(), cancel)
                .await,
            Err(AttachmentError::Cancelled)
        ));
        // The blocking worker owns its reservation until it observes cancellation.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while stager.memory.available_permits() != MAX_STAGED_BYTES {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
