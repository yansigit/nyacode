//! Saved display caches are filled only after authoritative native reads.
//! Cache failures never change transport results or authorize a send.
use super::*;
use codex_chatgpt_chat::content::AttachmentReference;
use codex_chatgpt_chat::content::Citation;
use codex_chatgpt_chat::content::ImageReference;
use codex_chatgpt_chat::conversation::ContinuationBlocker;
use codex_chatgpt_chat::conversation::ConversationBranch;
use codex_chatgpt_chat::conversation::ConversationNode;
use codex_chatgpt_chat::conversation::ConversationSnapshot;
use codex_chatgpt_chat::stream::Message;
use codex_chatgpt_chat::stream::MessagePart;
use codex_state::chat::ChatCacheBlocker;
use codex_state::chat::ChatCacheRole;
use codex_state::chat::ChatCachedAttachment;
use codex_state::chat::ChatCachedCitation;
use codex_state::chat::ChatCachedConversation;
use codex_state::chat::ChatCachedMessage;
use codex_state::chat::ChatCachedNode;
use codex_state::chat::ChatCachedPart;
use std::time::Duration;

impl SavedSession {
    pub(super) async fn cache_authoritative_conversation(
        &self,
        snapshot: &ConversationSnapshot,
        observed_at_ms: i64,
    ) {
        if self.check().is_err()
            || snapshot.continuation_blocker == Some(ContinuationBlocker::Temporary)
        {
            return;
        }
        let Some(store) = &self.store else {
            return;
        };
        let cached = to_cache(snapshot, observed_at_ms);
        // Display caching cannot make an otherwise successful read wait for an
        // unavailable database. Dropping this future rolls back partial writes.
        let _ = tokio::time::timeout(
            Duration::from_millis(250),
            store.cache_conversation(&cached),
        )
        .await;
    }

    /// Explicit cache access, never an automatic transport fallback. The caller
    /// must label the returned snapshot local and may not use it for continuation.
    pub async fn cached_conversation_read(
        &self,
        id: &str,
    ) -> Result<(ConversationSnapshot, i64), ServiceError> {
        self.guarded(async {
            let cached = self
                .store()?
                .cached_conversation(id)
                .await?
                .ok_or(ChatStoreError::NotFound)?;
            let observed_at_ms = cached.observed_at_ms;
            Ok((from_cache(cached), observed_at_ms))
        })
        .await
    }
}

fn to_cache(snapshot: &ConversationSnapshot, observed_at_ms: i64) -> ChatCachedConversation {
    ChatCachedConversation {
        id: snapshot.id.clone(),
        title: snapshot.title.clone(),
        updated_at_ms: snapshot.updated_at_ms,
        observed_at_ms,
        default_model: snapshot.default_model.clone(),
        continuation_blocker: snapshot.continuation_blocker.map(|blocker| match blocker {
            ContinuationBlocker::ReadOnly | ContinuationBlocker::Temporary => {
                ChatCacheBlocker::ReadOnly
            }
            ContinuationBlocker::CustomGpt => ChatCacheBlocker::CustomGpt,
            ContinuationBlocker::UnsupportedMode => ChatCacheBlocker::UnsupportedMode,
            ContinuationBlocker::EmptyConversation => ChatCacheBlocker::EmptyConversation,
            ContinuationBlocker::IncompleteGeneration => ChatCacheBlocker::IncompleteGeneration,
            ContinuationBlocker::UnsupportedContent => ChatCacheBlocker::UnsupportedContent,
        }),
        current_node: snapshot.branch.current_node.clone(),
        other_branch_heads: snapshot.other_branch_heads.clone(),
        nodes: snapshot
            .branch
            .nodes
            .iter()
            .map(|node| ChatCachedNode {
                id: node.id.clone(),
                parent_id: node.parent_id.clone(),
                message: node.message.as_ref().and_then(|message| {
                    let role = match message.role.as_str() {
                        "user" => ChatCacheRole::User,
                        "assistant" => ChatCacheRole::Assistant,
                        _ => return None,
                    };
                    Some(ChatCachedMessage {
                        id: message.id.clone(),
                        role,
                        model: message.model.clone(),
                        finished: message.finished,
                        end_turn: message.end_turn,
                        parts: message
                            .parts
                            .iter()
                            .map(|part| match part {
                                MessagePart::Text(text) => ChatCachedPart::Text(text.clone()),
                                MessagePart::Image(image) => ChatCachedPart::Image {
                                    file_id: image.file_id.clone(),
                                    width: image.width,
                                    height: image.height,
                                    size_bytes: image.size_bytes,
                                },
                                MessagePart::Unsupported { content_type } => {
                                    ChatCachedPart::Unsupported {
                                        content_type: content_type.clone(),
                                    }
                                }
                            })
                            .collect(),
                        citations: message
                            .citations
                            .iter()
                            .map(|citation| ChatCachedCitation {
                                marker: citation.marker.clone(),
                                urls: citation.urls.clone(),
                            })
                            .collect(),
                        attachments: message
                            .attachments
                            .iter()
                            .map(|file| ChatCachedAttachment {
                                file_id: file.file_id.clone(),
                                name: file.name.clone(),
                                mime_type: file.mime_type.clone(),
                                size_bytes: file.size_bytes,
                                width: file.width,
                                height: file.height,
                            })
                            .collect(),
                    })
                }),
            })
            .collect(),
    }
}

fn from_cache(cached: ChatCachedConversation) -> ConversationSnapshot {
    ConversationSnapshot {
        id: cached.id,
        title: cached.title,
        updated_at_ms: cached.updated_at_ms,
        default_model: cached.default_model,
        // Even a once-completed cached branch may have changed on another
        // client. Only a new authoritative read can re-enable continuation.
        continuation_blocker: Some(ContinuationBlocker::ReadOnly),
        other_branch_heads: cached.other_branch_heads,
        branch: ConversationBranch {
            current_node: cached.current_node,
            nodes: cached
                .nodes
                .into_iter()
                .map(|node| ConversationNode {
                    id: node.id,
                    parent_id: node.parent_id,
                    message: node.message.map(|message| Message {
                        id: message.id,
                        role: match message.role {
                            ChatCacheRole::User => "user",
                            ChatCacheRole::Assistant => "assistant",
                        }
                        .into(),
                        model: message.model,
                        finished: message.finished,
                        end_turn: message.end_turn,
                        parts: message
                            .parts
                            .into_iter()
                            .map(|part| match part {
                                ChatCachedPart::Text(text) => MessagePart::Text(text),
                                ChatCachedPart::Image {
                                    file_id,
                                    width,
                                    height,
                                    size_bytes,
                                } => MessagePart::Image(ImageReference {
                                    file_id,
                                    width,
                                    height,
                                    size_bytes,
                                }),
                                ChatCachedPart::Unsupported { content_type } => {
                                    MessagePart::Unsupported { content_type }
                                }
                            })
                            .collect(),
                        citations: message
                            .citations
                            .into_iter()
                            .map(|citation| Citation {
                                marker: citation.marker,
                                urls: citation.urls,
                            })
                            .collect(),
                        attachments: message
                            .attachments
                            .into_iter()
                            .map(|file| AttachmentReference {
                                file_id: file.file_id,
                                name: file.name,
                                mime_type: file.mime_type,
                                size_bytes: file.size_bytes,
                                width: file.width,
                                height: file.height,
                            })
                            .collect(),
                    }),
                })
                .collect(),
        },
    }
}

#[cfg(test)]
mod tests;
