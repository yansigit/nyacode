//! Typed display records intentionally omit Debug and any download capability.
use crate::chat::ChatStoreError;
use crate::runtime::chat::types::MAX_RECORD_BYTES;
use crate::runtime::chat::types::validate_id;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCachedConversation {
    pub id: String,
    pub title: String,
    pub updated_at_ms: Option<i64>,
    /// Request start time, not response completion time. A slow old read must
    /// not replace a cache written by a later request.
    pub observed_at_ms: i64,
    pub default_model: Option<String>,
    pub continuation_blocker: Option<ChatCacheBlocker>,
    pub current_node: Option<String>,
    pub other_branch_heads: Vec<String>,
    pub nodes: Vec<ChatCachedNode>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatCacheBlocker {
    ReadOnly,
    CustomGpt,
    UnsupportedMode,
    EmptyConversation,
    IncompleteGeneration,
    UnsupportedContent,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCachedNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub message: Option<ChatCachedMessage>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCachedMessage {
    pub id: String,
    pub role: ChatCacheRole,
    pub parts: Vec<ChatCachedPart>,
    pub model: Option<String>,
    pub finished: bool,
    pub end_turn: bool,
    pub citations: Vec<ChatCachedCitation>,
    /// Stored separately in chat_attachments, never as signed storage URLs.
    pub attachments: Vec<ChatCachedAttachment>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatCacheRole {
    User,
    Assistant,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ChatCachedPart {
    Text(String),
    Image {
        file_id: String,
        width: Option<u32>,
        height: Option<u32>,
        size_bytes: Option<u64>,
    },
    Unsupported {
        content_type: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCachedCitation {
    pub marker: String,
    pub urls: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCachedAttachment {
    pub file_id: String,
    pub name: String,
    pub mime_type: Option<String>,
    pub size_bytes: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl ChatCachedConversation {
    pub(super) fn validate(&self) -> Result<usize, ChatStoreError> {
        validate_id(&self.id)?;
        if self.title.len() > 4096
            || self.nodes.len() > 8192
            || self.other_branch_heads.len() > 8192
        {
            return Err(ChatStoreError::InvalidInput);
        }
        if let Some(model) = &self.default_model {
            validate_id(model)?;
        }
        let mut nodes = HashSet::new();
        let mut messages = HashSet::new();
        let mut attachments = 0usize;
        let mut prior = None;
        for node in &self.nodes {
            validate_id(&node.id)?;
            if !nodes.insert(&node.id) || node.parent_id.as_ref() != prior {
                return Err(ChatStoreError::InvalidInput);
            }
            prior = Some(&node.id);
            if let Some(message) = &node.message {
                validate_id(&message.id)?;
                if !messages.insert(&message.id)
                    || message.parts.len() > 256
                    || message.citations.len() > 256
                    || message.attachments.len() > 256
                {
                    return Err(ChatStoreError::InvalidInput);
                }
                if let Some(model) = &message.model {
                    validate_id(model)?;
                }
                attachments = attachments.saturating_add(message.attachments.len());
                if attachments > 8192 {
                    return Err(ChatStoreError::InvalidInput);
                }
                for part in &message.parts {
                    match part {
                        ChatCachedPart::Text(text) if text.len() <= MAX_RECORD_BYTES => {}
                        ChatCachedPart::Image {
                            file_id,
                            width,
                            height,
                            ..
                        } => {
                            validate_id(file_id)?;
                            dimensions(*width, *height)?;
                        }
                        ChatCachedPart::Unsupported { content_type }
                            if content_type.len() <= 128 => {}
                        _ => return Err(ChatStoreError::InvalidInput),
                    }
                }
                for attachment in &message.attachments {
                    validate_id(&attachment.file_id)?;
                    dimensions(attachment.width, attachment.height)?;
                    if attachment.name.is_empty()
                        || attachment.name.len() > 1024
                        || attachment.mime_type.as_ref().is_some_and(|mime| {
                            mime.len() > 256 || !mime.bytes().all(|b| b.is_ascii_graphic())
                        })
                    {
                        return Err(ChatStoreError::InvalidInput);
                    }
                }
                for citation in &message.citations {
                    citation.validate()?;
                }
            }
        }
        if self.current_node.as_ref() != prior {
            return Err(ChatStoreError::InvalidInput);
        }
        let mut heads = HashSet::new();
        for head in &self.other_branch_heads {
            validate_id(head)?;
            if nodes.contains(head) || !heads.insert(head) {
                return Err(ChatStoreError::InvalidInput);
            }
        }
        let mut counter = EncodedSize(0);
        serde_json::to_writer(&mut counter, self).map_err(|_| ChatStoreError::InvalidInput)?;
        Ok(counter.0)
    }
}

// Count escaped JSON bytes without allocating a second copy of the complete
// transcript. A large/malformed input cannot force an oversized serialization.
struct EncodedSize(usize);
impl std::io::Write for EncodedSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self.0.saturating_add(bytes.len());
        if next > MAX_RECORD_BYTES {
            return Err(std::io::Error::other("cache size limit"));
        }
        self.0 = next;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn dimensions(width: Option<u32>, height: Option<u32>) -> Result<(), ChatStoreError> {
    if width == Some(0) || height == Some(0) {
        return Err(ChatStoreError::InvalidInput);
    }
    Ok(())
}

impl ChatCachedCitation {
    fn validate(&self) -> Result<(), ChatStoreError> {
        if self.marker.len() > 1024
            || !self.marker.starts_with("\u{e200}cite\u{e202}")
            || !self.marker.ends_with('\u{e201}')
            || self.urls.len() > 16
        {
            return Err(ChatStoreError::InvalidInput);
        }
        for value in &self.urls {
            if value.len() > 4096 || value.chars().any(char::is_control) {
                return Err(ChatStoreError::InvalidInput);
            }
            let url = url::Url::parse(value).map_err(|_| ChatStoreError::InvalidInput)?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query_pairs().any(|(key, _)| {
                    matches!(
                        key.to_ascii_lowercase().as_str(),
                        "sig" | "signature" | "x-amz-signature" | "x-goog-signature"
                    )
                })
            {
                return Err(ChatStoreError::InvalidInput);
            }
        }
        Ok(())
    }
}
