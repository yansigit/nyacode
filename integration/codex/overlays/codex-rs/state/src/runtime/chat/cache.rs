//! Replace complete authoritative snapshots transactionally. These records are
//! for display only; they never prove acceptance or authorize continuation.
pub(super) mod types;

use super::SavedChatStore;
use super::types::ChatStoreError;
use super::types::MAX_RECORD_BYTES;
use super::types::decode;
use super::types::encode;
use super::types::validate_id;
use sqlx::Connection;
use sqlx::Row;
use std::collections::HashMap;
use types::*;

impl SavedChatStore {
    /// Cache only after the native reader has established saved persistence.
    /// Returns false when a later request already supplied a snapshot.
    pub async fn cache_conversation(
        &self,
        snapshot: &ChatCachedConversation,
    ) -> Result<bool, ChatStoreError> {
        let bytes = snapshot.validate()?;
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let previous: Option<i64> = sqlx::query_scalar("SELECT observed_at_ms FROM chat_conversations WHERE principal=? AND account=? AND workspace=? AND conversation_id=?")
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(&snapshot.id)
            .fetch_optional(&mut *tx).await?;
        if previous.is_some_and(|previous| previous >= snapshot.observed_at_ms) {
            return Ok(false);
        }
        sqlx::query("DELETE FROM chat_conversations WHERE principal=? AND account=? AND workspace=? AND conversation_id=?")
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(&snapshot.id)
            .execute(&mut *tx).await?;
        sqlx::query("INSERT INTO chat_conversations(principal,account,workspace,conversation_id,title,updated_at_ms,observed_at_ms,default_model,continuation_blocker,content_bytes) VALUES(?,?,?,?,?,?,?,?,?,?)")
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(&snapshot.id)
            .bind(&snapshot.title).bind(snapshot.updated_at_ms).bind(snapshot.observed_at_ms).bind(&snapshot.default_model)
            .bind(snapshot.continuation_blocker.map(|value| encode(&value)).transpose()?).bind(bytes as i64)
            .execute(&mut *tx).await?;
        for (position, node) in snapshot.nodes.iter().enumerate() {
            let message = node
                .message
                .clone()
                .map(|mut message| {
                    message.attachments.clear();
                    encode(&message)
                })
                .transpose()?;
            sqlx::query("INSERT INTO chat_messages(principal,account,workspace,conversation_id,node_id,parent_id,position,message) VALUES(?,?,?,?,?,?,?,?)")
                .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(&snapshot.id)
                .bind(&node.id).bind(&node.parent_id).bind(position as i64).bind(message)
                .execute(&mut *tx).await?;
            if let Some(message) = &node.message {
                for (position, attachment) in message.attachments.iter().enumerate() {
                    sqlx::query("INSERT INTO chat_attachments(principal,account,workspace,conversation_id,node_id,position,metadata) VALUES(?,?,?,?,?,?,?)")
                        .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(&snapshot.id)
                        .bind(&node.id).bind(position as i64).bind(encode(attachment)?)
                        .execute(&mut *tx).await?;
                }
            }
        }
        for (head, current) in snapshot
            .current_node
            .iter()
            .map(|head| (head, true))
            .chain(snapshot.other_branch_heads.iter().map(|head| (head, false)))
        {
            sqlx::query("INSERT INTO chat_branches(principal,account,workspace,conversation_id,node_id,current) VALUES(?,?,?,?,?,?)")
                .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(&snapshot.id)
                .bind(head).bind(current).execute(&mut *tx).await?;
        }
        // The bounded display cache can evict old snapshots. Durable drafts and
        // pending submissions have independent tables and are never evicted here.
        sqlx::query("DELETE FROM chat_conversations WHERE principal=? AND account=? AND workspace=? AND conversation_id IN (
            SELECT conversation_id FROM (
                SELECT conversation_id, SUM(content_bytes) OVER (ORDER BY observed_at_ms DESC, conversation_id) AS bytes,
                    ROW_NUMBER() OVER (ORDER BY observed_at_ms DESC, conversation_id) AS rank
                FROM chat_conversations WHERE principal=? AND account=? AND workspace=?
            ) WHERE bytes > 67108864 OR rank > 128)")
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace)
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn cached_conversation(
        &self,
        id: &str,
    ) -> Result<Option<ChatCachedConversation>, ChatStoreError> {
        validate_id(id)?;
        let mut connection = self.pool.acquire().await?;
        // One read transaction prevents mixed branches during another client's replacement.
        let mut tx = connection.begin().await?;
        // Check the actual stored lengths before allocating decoded messages.
        // Cached content_bytes alone cannot establish bounds after disk corruption.
        let bytes: i64 = sqlx::query_scalar("SELECT
            (SELECT COALESCE(SUM(COALESCE(length(CAST(message AS BLOB)),0) + length(CAST(node_id AS BLOB)) + COALESCE(length(CAST(parent_id AS BLOB)),0)),0) FROM chat_messages WHERE principal=? AND account=? AND workspace=? AND conversation_id=?) +
            (SELECT COALESCE(SUM(length(CAST(metadata AS BLOB)) + length(CAST(node_id AS BLOB))),0) FROM chat_attachments WHERE principal=? AND account=? AND workspace=? AND conversation_id=?) +
            (SELECT COALESCE(SUM(length(CAST(node_id AS BLOB))),0) FROM chat_branches WHERE principal=? AND account=? AND workspace=? AND conversation_id=?) +
            (SELECT COALESCE(SUM(length(CAST(title AS BLOB)) + COALESCE(length(CAST(default_model AS BLOB)),0) + COALESCE(length(CAST(continuation_blocker AS BLOB)),0)),0) FROM chat_conversations WHERE principal=? AND account=? AND workspace=? AND conversation_id=?)")
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(id)
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(id)
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(id)
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(id)
            .fetch_one(&mut *tx).await?;
        if bytes < 0 || bytes as usize > MAX_RECORD_BYTES {
            return Err(ChatStoreError::InvalidRecord);
        }
        let Some(row) = sqlx::query("SELECT title,updated_at_ms,observed_at_ms,default_model,continuation_blocker FROM chat_conversations WHERE principal=? AND account=? AND workspace=? AND conversation_id=?")
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(id)
            .fetch_optional(&mut *tx).await? else { return Ok(None); };
        let rows = sqlx::query("SELECT node_id,parent_id,position,message FROM chat_messages WHERE principal=? AND account=? AND workspace=? AND conversation_id=? ORDER BY position LIMIT 8193")
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(id)
            .fetch_all(&mut *tx).await?;
        if rows.len() > 8192 {
            return Err(ChatStoreError::InvalidRecord);
        }
        let mut nodes = Vec::with_capacity(rows.len());
        let mut indexes = HashMap::new();
        for (index, node) in rows.into_iter().enumerate() {
            if node.try_get::<i64, _>("position")? != index as i64 {
                return Err(ChatStoreError::InvalidRecord);
            }
            let message = node
                .try_get::<Option<String>, _>("message")?
                .map(|value| decode::<ChatCachedMessage>(&value))
                .transpose()?;
            if message
                .as_ref()
                .is_some_and(|message| !message.attachments.is_empty())
            {
                return Err(ChatStoreError::InvalidRecord);
            }
            let node = ChatCachedNode {
                id: node.try_get("node_id")?,
                parent_id: node.try_get("parent_id")?,
                message,
            };
            indexes.insert(node.id.clone(), index);
            nodes.push(node);
        }
        let attachments = sqlx::query("SELECT node_id,position,metadata FROM chat_attachments WHERE principal=? AND account=? AND workspace=? AND conversation_id=? ORDER BY node_id,position LIMIT 8193")
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(id)
            .fetch_all(&mut *tx).await?;
        if attachments.len() > 8192 {
            return Err(ChatStoreError::InvalidRecord);
        }
        for attachment in attachments {
            let node_id: String = attachment.try_get("node_id")?;
            let index = indexes.get(&node_id).ok_or(ChatStoreError::InvalidRecord)?;
            let message = nodes[*index]
                .message
                .as_mut()
                .ok_or(ChatStoreError::InvalidRecord)?;
            if attachment.try_get::<i64, _>("position")? != message.attachments.len() as i64 {
                return Err(ChatStoreError::InvalidRecord);
            }
            message
                .attachments
                .push(decode(&attachment.try_get::<String, _>("metadata")?)?);
        }
        let branches: Vec<(String, bool)> = sqlx::query_as("SELECT node_id,current FROM chat_branches WHERE principal=? AND account=? AND workspace=? AND conversation_id=? ORDER BY node_id LIMIT 8194")
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(id)
            .fetch_all(&mut *tx).await?;
        let mut current_node = None;
        let mut other_branch_heads = Vec::new();
        for (head, current) in branches {
            if current {
                if current_node.replace(head).is_some() {
                    return Err(ChatStoreError::InvalidRecord);
                }
            } else {
                other_branch_heads.push(head);
            }
        }
        let snapshot = ChatCachedConversation {
            id: id.into(),
            title: row.try_get("title")?,
            updated_at_ms: row.try_get("updated_at_ms")?,
            observed_at_ms: row.try_get("observed_at_ms")?,
            default_model: row.try_get("default_model")?,
            continuation_blocker: row
                .try_get::<Option<String>, _>("continuation_blocker")?
                .map(|value| decode(&value))
                .transpose()?,
            current_node,
            other_branch_heads,
            nodes,
        };
        snapshot
            .validate()
            .map_err(|_| ChatStoreError::InvalidRecord)?;
        tx.commit().await?;
        Ok(Some(snapshot))
    }
}
