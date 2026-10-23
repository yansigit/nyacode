//! Normalized native history. Raw backend metadata, storage URLs, and hidden
//! message contents never reach the display/cache types. ChatGPT's mapping and
//! current_node are authoritative; timestamps never select a branch.

use crate::ChatError;
use crate::ChatProbe;
use crate::probe::valid_id;
use crate::stream::Message;
use crate::stream::MessagePart;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::HashSet;

const MAX_NODES: usize = 8192;
const MAX_CONTENT: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationBlocker {
    ReadOnly,
    CustomGpt,
    Temporary,
    UnsupportedMode,
    EmptyConversation,
    IncompleteGeneration,
    UnsupportedContent,
}

// Content-bearing types deliberately do not implement Debug.
#[derive(Clone, Serialize, Deserialize)]
pub struct ConversationNode {
    pub id: String,
    pub parent_id: Option<String>,
    /// Hidden/system/developer messages retain their node IDs, not their text.
    pub message: Option<Message>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ConversationBranch {
    pub current_node: Option<String>,
    /// Chronological parent chain, including structural nodes with no message.
    pub nodes: Vec<ConversationNode>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ConversationSnapshot {
    pub id: String,
    pub title: String,
    pub updated_at_ms: Option<i64>,
    pub branch: ConversationBranch,
    /// Other leaves are metadata only; branch switching/editing is deferred.
    pub other_branch_heads: Vec<String>,
    pub default_model: Option<String>,
    pub continuation_blocker: Option<ContinuationBlocker>,
}

impl ConversationSnapshot {
    pub fn check_continuation(&self, expected_parent: &str) -> Result<(), ChatError> {
        if self.branch.current_node.as_deref() != Some(expected_parent) {
            return Err(ChatError::BranchConflict);
        }
        if self.continuation_blocker.is_some() {
            return Err(ChatError::UnsupportedCapability);
        }
        Ok(())
    }

    pub(crate) fn parse(id: &str, value: &Value) -> Result<Self, ChatError> {
        if !valid_id(id) {
            return Err(ChatError::InvalidInput);
        }
        let object = value.as_object().ok_or(ChatError::ProtocolChanged)?;
        for key in ["conversation_id", "id"] {
            if let Some(reported) = object.get(key).filter(|v| !v.is_null())
                && reported.as_str() != Some(id)
            {
                return Err(ChatError::ProtocolChanged);
            }
        }
        let title = optional_text(value.get("title"), 4096)?.unwrap_or_default();
        let default_model = optional_text(value.get("default_model_slug"), 256)?;
        let current_node = optional_id(Some(
            value
                .get("current_node")
                .ok_or(ChatError::ProtocolChanged)?,
        ))?;
        let mapping = value
            .get("mapping")
            .and_then(Value::as_object)
            .ok_or(ChatError::ProtocolChanged)?;
        if mapping.len() > MAX_NODES {
            return Err(ChatError::ResponseTooLarge);
        }
        if !mapping.is_empty() && current_node.is_none() {
            return Err(ChatError::ProtocolChanged);
        }
        let mut nodes = BTreeMap::new();
        let mut message_ids = HashSet::new();
        let mut content_bytes = 0usize;
        for (id, raw) in mapping {
            if !valid_id(id) || !raw.is_object() {
                return Err(ChatError::ProtocolChanged);
            }
            if raw
                .get("id")
                .filter(|v| !v.is_null())
                .is_some_and(|v| v.as_str() != Some(id))
            {
                return Err(ChatError::ProtocolChanged);
            }
            let parent_id = optional_id(raw.get("parent"))?;
            let message = if let Some(raw) = raw.get("message").filter(|v| !v.is_null()) {
                let message_id = optional_id(raw.get("id"))?.ok_or(ChatError::ProtocolChanged)?;
                if !message_ids.insert(message_id) {
                    return Err(ChatError::ProtocolChanged);
                }
                if !Message::is_visible(raw).map_err(|_| ChatError::ProtocolChanged)? {
                    None
                } else {
                    let message = Message::parse(raw).map_err(|_| ChatError::ProtocolChanged)?;
                    content_bytes = content_bytes.saturating_add(message.weight());
                    if content_bytes > MAX_CONTENT {
                        return Err(ChatError::ResponseTooLarge);
                    }
                    Some(message)
                }
            } else {
                None
            };
            nodes.insert(
                id.clone(),
                ConversationNode {
                    id: id.clone(),
                    parent_id,
                    message,
                },
            );
        }
        validate_graph(&nodes)?;
        let mut chain = Vec::new();
        let mut cursor = current_node.as_deref();
        while let Some(id) = cursor {
            let node = nodes.get(id).ok_or(ChatError::ProtocolChanged)?;
            chain.push(node.clone());
            cursor = node.parent_id.as_deref();
        }
        chain.reverse();
        let parents: HashSet<&str> = nodes
            .values()
            .filter_map(|n| n.parent_id.as_deref())
            .collect();
        let other_branch_heads = nodes
            .keys()
            .filter(|id| {
                Some(id.as_str()) != current_node.as_deref() && !parents.contains(id.as_str())
            })
            .cloned()
            .collect();
        let continuation_blocker = continuation_blocker(value, &chain)?;
        Ok(Self {
            id: id.into(),
            title,
            updated_at_ms: timestamp(value.get("update_time"))?,
            branch: ConversationBranch {
                current_node,
                nodes: chain,
            },
            other_branch_heads,
            default_model,
            continuation_blocker,
        })
    }
}

/// Validate every parent chain in linear work, including inactive branches.
fn validate_graph(nodes: &BTreeMap<String, ConversationNode>) -> Result<(), ChatError> {
    let mut resolved = HashSet::new();
    for id in nodes.keys() {
        let mut path = HashSet::new();
        let mut current = Some(id.as_str());
        while let Some(id) = current {
            if resolved.contains(id) {
                break;
            }
            if !path.insert(id) {
                return Err(ChatError::ProtocolChanged);
            }
            current = nodes
                .get(id)
                .ok_or(ChatError::ProtocolChanged)?
                .parent_id
                .as_deref();
        }
        resolved.extend(path);
    }
    Ok(())
}

fn continuation_blocker(
    value: &Value,
    nodes: &[ConversationNode],
) -> Result<Option<ContinuationBlocker>, ChatError> {
    let reason = if optional_bool(value.get("is_read_only"))? {
        Some(ContinuationBlocker::ReadOnly)
    } else if optional_id(value.get("gizmo_id"))?.is_some() {
        Some(ContinuationBlocker::CustomGpt)
    } else if optional_bool(value.get("is_temporary_chat"))?
        || optional_bool(value.get("is_do_not_remember"))?
    {
        // This is the saved-history adapter. Temporary continuation uses its
        // independently verified in-memory conduit, never saved history.
        Some(ContinuationBlocker::Temporary)
    } else if value
        .get("conversation_template_id")
        .is_some_and(|v| !v.is_null())
        || value
            .get("conversation_mode")
            .filter(|v| !v.is_null())
            .is_some_and(|mode| {
                mode.get("kind").and_then(Value::as_str) != Some("primary_assistant")
            })
    {
        Some(ContinuationBlocker::UnsupportedMode)
    } else if let Some(message) = nodes.last().and_then(|node| node.message.as_ref()) {
        if message.role != "assistant" || !message.finished || !message.end_turn {
            Some(ContinuationBlocker::IncompleteGeneration)
        } else if message
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::Unsupported { .. }))
        {
            Some(ContinuationBlocker::UnsupportedContent)
        } else {
            None
        }
    } else if nodes.iter().any(|node| node.message.is_some()) {
        Some(ContinuationBlocker::UnsupportedContent)
    } else {
        Some(ContinuationBlocker::EmptyConversation)
    };
    Ok(reason)
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub updated_at_ms: Option<i64>,
    pub archived: bool,
    pub custom_gpt: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ConversationPage {
    pub items: Vec<ConversationSummary>,
    pub next_offset: Option<u32>,
    pub total: Option<u64>,
}

impl ConversationPage {
    fn parse(value: &Value, offset: u32, limit: u32) -> Result<Self, ChatError> {
        let entries = value
            .get("items")
            .and_then(Value::as_array)
            .ok_or(ChatError::ProtocolChanged)?;
        if entries.len() > limit as usize {
            return Err(ChatError::ProtocolChanged);
        }
        if value
            .get("offset")
            .is_some_and(|v| v.as_u64() != Some(u64::from(offset)))
        {
            return Err(ChatError::ProtocolChanged);
        }
        let total = value
            .get("total")
            .filter(|v| !v.is_null())
            .map(|v| v.as_u64().ok_or(ChatError::ProtocolChanged))
            .transpose()?;
        let mut ids = HashSet::new();
        let mut items = Vec::new();
        for entry in entries {
            let id = optional_id(entry.get("id"))?.ok_or(ChatError::ProtocolChanged)?;
            if !ids.insert(id.clone()) {
                return Err(ChatError::ProtocolChanged);
            }
            // A temporary entry in saved history is a protocol error. Never
            // persist its title through the saved-history display/cache path.
            if optional_bool(entry.get("is_temporary_chat"))?
                || optional_bool(entry.get("is_do_not_remember"))?
            {
                return Err(ChatError::ProtocolChanged);
            }
            items.push(ConversationSummary {
                id,
                title: optional_text(entry.get("title"), 4096)?.unwrap_or_default(),
                updated_at_ms: timestamp(entry.get("update_time"))?,
                archived: optional_bool(entry.get("is_archived"))?,
                custom_gpt: optional_id(entry.get("gizmo_id"))?.is_some(),
            });
        }
        let next_offset = offset.checked_add(items.len() as u32).filter(|next| {
            !items.is_empty()
                && total.map_or(items.len() == limit as usize, |total| {
                    u64::from(*next) < total
                })
        });
        Ok(Self {
            items,
            next_offset,
            total,
        })
    }
}

impl ChatProbe {
    pub async fn conversation_list(
        &self,
        offset: u32,
        limit: u32,
    ) -> Result<ConversationPage, ChatError> {
        if !(1..=100).contains(&limit) {
            return Err(ChatError::InvalidInput);
        }
        let page: Value = self
            .read(
                &format!("conversations?offset={offset}&limit={limit}&order=updated"),
                "conversations",
            )
            .await?;
        let normalized = ConversationPage::parse(&page, offset, limit)?;
        self.check_owner()?;
        Ok(normalized)
    }

    pub async fn conversation_read(&self, id: &str) -> Result<ConversationSnapshot, ChatError> {
        if !valid_id(id) {
            return Err(ChatError::InvalidInput);
        }
        let path = format!("conversation/{id}");
        let value: Value = self.read(&path, &path).await?;
        if optional_bool(value.get("is_temporary_chat"))?
            || optional_bool(value.get("is_do_not_remember"))?
        {
            return Err(ChatError::UnsupportedCapability);
        }
        let normalized = ConversationSnapshot::parse(id, &value)?;
        self.check_owner()?;
        Ok(normalized)
    }
}

fn optional_id(value: Option<&Value>) -> Result<Option<String>, ChatError> {
    let id = optional_text(value, 128)?;
    if id.as_ref().is_some_and(|id| !valid_id(id)) {
        return Err(ChatError::ProtocolChanged);
    }
    Ok(id)
}

fn optional_text(value: Option<&Value>, limit: usize) -> Result<Option<String>, ChatError> {
    value
        .filter(|v| !v.is_null())
        .map(|v| {
            v.as_str()
                .filter(|s| s.len() <= limit)
                .map(str::to_owned)
                .ok_or(ChatError::ProtocolChanged)
        })
        .transpose()
}

fn optional_bool(value: Option<&Value>) -> Result<bool, ChatError> {
    value
        .filter(|v| !v.is_null())
        .map(|v| v.as_bool().ok_or(ChatError::ProtocolChanged))
        .transpose()
        .map(|v| v.unwrap_or(false))
}

fn timestamp(value: Option<&Value>) -> Result<Option<i64>, ChatError> {
    value
        .filter(|v| !v.is_null())
        .map(|value| {
            if let Some(seconds) = value.as_f64() {
                if seconds.is_finite() && (0.0..=253_402_300_799.0).contains(&seconds) {
                    return Ok((seconds * 1000.0).round() as i64);
                }
            } else if let Some(text) = value.as_str()
                && text.len() <= 64
                && let Ok(time) = chrono::DateTime::parse_from_rfc3339(text)
            {
                return Ok(time.timestamp_millis());
            }
            Err(ChatError::ProtocolChanged)
        })
        .transpose()
}

#[cfg(test)]
mod tests;
