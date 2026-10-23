use super::SavedChatStore;
use super::parse_uuid;
use super::types::*;
use chrono::Utc;
use sqlx::Connection;
use sqlx::Row;
use sqlx::SqliteConnection;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

impl SavedChatStore {
    pub async fn read_draft(&self, id: Uuid) -> Result<Option<ChatDraft>, ChatStoreError> {
        let mut connection = self.pool.acquire().await?;
        read_draft(&mut connection, &self.scope, id).await
    }

    /// None creates a draft; Some edits exactly that revision. A stale write
    /// never merges, retries, or overwrites another client's composer contents.
    pub async fn write_draft(
        &self,
        id: Uuid,
        expected_revision: Option<Uuid>,
        content: ChatDraftContent,
    ) -> Result<ChatDraft, ChatStoreError> {
        content.validate()?;
        let selections = encode(&content.selections)?;
        let mut connection = self.write_connection().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let previous = read_draft(&mut tx, &self.scope, id).await?;
        if previous.as_ref().map(|draft| draft.revision) != expected_revision {
            return Err(ChatStoreError::RevisionConflict);
        }
        let revision = Uuid::new_v4();
        let updated_at_ms = previous
            .as_ref()
            .map_or(0, |draft| draft.updated_at_ms.saturating_add(1))
            .max(Utc::now().timestamp_millis());
        sqlx::query(
            "INSERT INTO chat_drafts(principal,account,workspace,draft_id,revision,
              conversation_id,expected_parent_id,requested_model,text,selections,updated_at_ms)
             VALUES(?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(principal,account,workspace,draft_id) DO UPDATE SET
              revision=excluded.revision, conversation_id=excluded.conversation_id,
              expected_parent_id=excluded.expected_parent_id, requested_model=excluded.requested_model,
              text=excluded.text, selections=excluded.selections, updated_at_ms=excluded.updated_at_ms",
        )
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .bind(id.to_string())
        .bind(revision.to_string())
        .bind(&content.conversation_id)
        .bind(&content.expected_parent_id)
        .bind(&content.requested_model)
        .bind(&content.text)
        .bind(selections)
        .bind(updated_at_ms)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ChatDraft {
            id,
            revision,
            content,
            updated_at_ms,
        })
    }

    pub async fn delete_draft(
        &self,
        id: Uuid,
        expected_revision: Uuid,
    ) -> Result<(), ChatStoreError> {
        let mut connection = self.write_connection().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let previous = read_draft(&mut tx, &self.scope, id)
            .await?
            .ok_or(ChatStoreError::NotFound)?;
        if previous.revision != expected_revision {
            return Err(ChatStoreError::RevisionConflict);
        }
        let pending: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM chat_submissions
             WHERE principal=? AND account=? AND workspace=? AND draft_id=? AND active=1)",
        )
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .bind(id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        if pending {
            return Err(ChatStoreError::PendingSubmission);
        }
        sqlx::query(
            "DELETE FROM chat_drafts WHERE principal=? AND account=? AND workspace=? AND draft_id=?",
        )
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Only bounded previews and metadata are selected; listing does not load
    /// every draft body or attachment selection into memory.
    pub async fn list_drafts(
        &self,
        before: Option<ChatPageCursor>,
        limit: usize,
    ) -> Result<ChatDraftPage, ChatStoreError> {
        if !(1..=100).contains(&limit) {
            return Err(ChatStoreError::InvalidInput);
        }
        let rows = sqlx::query(
            "SELECT draft_id,revision,conversation_id,requested_model,substr(text,1,160) AS preview,
              json_array_length(selections) AS attachment_count,updated_at_ms
             FROM chat_drafts WHERE principal=? AND account=? AND workspace=?
              AND (? IS NULL OR updated_at_ms < ? OR (updated_at_ms = ? AND draft_id < ?))
             ORDER BY updated_at_ms DESC,draft_id DESC LIMIT ?",
        )
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .bind(before.map(|cursor| cursor.updated_at_ms))
        .bind(before.map(|cursor| cursor.updated_at_ms))
        .bind(before.map(|cursor| cursor.updated_at_ms))
        .bind(before.map(|cursor| cursor.id.to_string()))
        .bind((limit + 1) as i64)
        .fetch_all(self.pool.as_ref())
        .await?;
        let has_more = rows.len() > limit;
        let items = rows
            .into_iter()
            .take(limit)
            .map(|row| {
                Ok(ChatDraftSummary {
                    id: parse_uuid(row.try_get("draft_id")?)?,
                    revision: parse_uuid(row.try_get("revision")?)?,
                    conversation_id: row.try_get("conversation_id")?,
                    requested_model: row.try_get("requested_model")?,
                    preview: row.try_get("preview")?,
                    attachment_count: usize::try_from(row.try_get::<i64, _>("attachment_count")?)
                        .map_err(|_| ChatStoreError::InvalidRecord)?,
                    updated_at_ms: row.try_get("updated_at_ms")?,
                })
            })
            .collect::<Result<Vec<_>, ChatStoreError>>()?;
        let next = items
            .last()
            .filter(|_| has_more)
            .map(|draft| ChatPageCursor {
                updated_at_ms: draft.updated_at_ms,
                id: draft.id,
            });
        Ok(ChatDraftPage { items, next })
    }
}

pub(super) async fn read_draft(
    connection: &mut SqliteConnection,
    scope: &ChatScope,
    id: Uuid,
) -> Result<Option<ChatDraft>, ChatStoreError> {
    sqlx::query(
        "SELECT * FROM chat_drafts WHERE principal=? AND account=? AND workspace=? AND draft_id=?",
    )
    .bind(&scope.principal)
    .bind(&scope.account)
    .bind(&scope.workspace)
    .bind(id.to_string())
    .fetch_optional(connection)
    .await?
    .as_ref()
    .map(draft_from_row)
    .transpose()
}

fn draft_from_row(row: &SqliteRow) -> Result<ChatDraft, ChatStoreError> {
    let content = ChatDraftContent {
        conversation_id: row.try_get("conversation_id")?,
        expected_parent_id: row.try_get("expected_parent_id")?,
        requested_model: row.try_get("requested_model")?,
        text: row.try_get("text")?,
        selections: decode(row.try_get("selections")?)?,
    };
    content
        .validate()
        .map_err(|_| ChatStoreError::InvalidRecord)?;
    Ok(ChatDraft {
        id: parse_uuid(row.try_get("draft_id")?)?,
        revision: parse_uuid(row.try_get("revision")?)?,
        content,
        updated_at_ms: row.try_get("updated_at_ms")?,
    })
}
