use super::SavedChatStore;
use super::drafts::read_draft;
use super::parse_uuid;
use super::types::*;
use chrono::Utc;
use sqlx::Connection;
use sqlx::Row;
use sqlx::SqliteConnection;
use sqlx::sqlite::SqliteRow;
use uuid::Uuid;

impl SavedChatStore {
    /// Journal the immutable saved submission before any user-message POST.
    /// `owner` is fresh for each service authentication/ownership generation.
    /// Uploads and preparation may precede this operation; transmitting may not.
    pub async fn begin_submission(
        &self,
        request: ChatSubmissionRequest,
        owner: Uuid,
    ) -> Result<(ChatSubmission, ChatSubmissionLease), ChatStoreError> {
        validate_id(&request.expected_parent_id)?;
        validate_id(&request.requested_model)?;
        let mut connection = self.write_connection().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let duplicate: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM chat_submissions WHERE principal=? AND account=? AND workspace=?
             AND (submission_id=? OR client_message_id=?))",
        )
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .bind(request.submission_id.to_string())
        .bind(request.client_message_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        if duplicate {
            return Err(ChatStoreError::DuplicateSubmission);
        }
        let draft = read_draft(&mut tx, &self.scope, request.draft_id)
            .await?
            .ok_or(ChatStoreError::NotFound)?;
        if draft.revision != request.expected_revision {
            return Err(ChatStoreError::RevisionConflict);
        }
        if draft
            .content
            .expected_parent_id
            .as_ref()
            .is_some_and(|parent| parent != &request.expected_parent_id)
        {
            return Err(ChatStoreError::RevisionConflict);
        }
        let target_key = target_key(draft.id, draft.content.conversation_id.as_deref());
        let pending: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM chat_submissions WHERE principal=? AND account=? AND workspace=?
             AND active=1 AND (draft_id=? OR target_key=?))",
        )
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .bind(draft.id.to_string())
        .bind(&target_key)
        .fetch_one(&mut *tx)
        .await?;
        if pending {
            return Err(ChatStoreError::PendingSubmission);
        }
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM chat_submissions WHERE principal=? AND account=? AND workspace=? AND active=1",
        )
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .fetch_one(&mut *tx)
        .await?;
        if count >= 1000 {
            return Err(ChatStoreError::Capacity);
        }
        let snapshot = ChatSubmissionSnapshot {
            version: 1,
            draft: draft.content,
            requested_model: request.requested_model,
            files: request.files,
        };
        snapshot.validate()?;
        let encoded = encode(&snapshot)?;
        let now = Utc::now().timestamp_millis();
        let lease_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO chat_submissions(principal,account,workspace,submission_id,client_message_id,
              draft_id,draft_revision,conversation_id,expected_parent_id,target_key,snapshot,
              owner_id,lease_id,lease_until_ms,created_at_ms,updated_at_ms)
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .bind(request.submission_id.to_string())
        .bind(request.client_message_id.to_string())
        .bind(draft.id.to_string())
        .bind(draft.revision.to_string())
        .bind(&snapshot.draft.conversation_id)
        .bind(&request.expected_parent_id)
        .bind(target_key)
        .bind(encoded)
        .bind(owner.to_string())
        .bind(lease_id.to_string())
        .bind(now + LEASE_MS)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((
            ChatSubmission {
                submission_id: request.submission_id,
                client_message_id: request.client_message_id,
                draft_id: draft.id,
                draft_revision: draft.revision,
                conversation_id: snapshot.draft.conversation_id.clone(),
                expected_parent_id: request.expected_parent_id,
                snapshot,
                transmission_started: false,
                accepted: false,
                resolved_model: None,
                outcome: ChatSubmissionOutcome::InProgress,
                active: true,
                created_at_ms: now,
                updated_at_ms: now,
            },
            ChatSubmissionLease {
                submission_id: request.submission_id,
                lease_id,
            },
        ))
    }

    /// Durably consume the initial owner's single transmission permission.
    /// Any commit error is ambiguous and must stop the send. No caller may
    /// turn an error here into a new submission automatically.
    pub async fn mark_transmitting(
        &self,
        lease: &ChatSubmissionLease,
    ) -> Result<ChatTransmissionPermit, ChatStoreError> {
        let mut connection = self.write_connection().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let current = owned_submission(&mut tx, &self.scope, lease).await?;
        if !current.may_transmit || current.submission.transmission_started {
            return Err(ChatStoreError::ReconcileOnly);
        }
        sqlx::query(
            "UPDATE chat_submissions SET transmission_started=1,may_transmit=0,updated_at_ms=?
             WHERE principal=? AND account=? AND workspace=? AND submission_id=?",
        )
        .bind(Utc::now().timestamp_millis())
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .bind(lease.submission_id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ChatTransmissionPermit {
            submission_id: lease.submission_id,
        })
    }

    pub async fn renew_lease(&self, lease: &ChatSubmissionLease) -> Result<(), ChatStoreError> {
        let mut connection = self.write_connection().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        owned_submission(&mut tx, &self.scope, lease).await?;
        sqlx::query(
            "UPDATE chat_submissions SET lease_until_ms=?
             WHERE principal=? AND account=? AND workspace=? AND submission_id=?",
        )
        .bind(Utc::now().timestamp_millis() + LEASE_MS)
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .bind(lease.submission_id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Lease expiry grants access to reconcile exact IDs, never permission to
    /// send. This remains true for crashes before mark_transmitting committed.
    pub async fn claim_reconciliation(
        &self,
        id: Uuid,
        owner: Uuid,
    ) -> Result<(ChatSubmission, ChatSubmissionLease), ChatStoreError> {
        let mut connection = self.write_connection().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let row = read_row(&mut tx, &self.scope, id)
            .await?
            .ok_or(ChatStoreError::NotFound)?;
        let submission = submission_from_row(&row)?;
        let now = Utc::now().timestamp_millis();
        if !submission.active || row.try_get::<i64, _>("lease_until_ms")? > now {
            return Err(ChatStoreError::LeaseLost);
        }
        let lease_id = Uuid::new_v4();
        sqlx::query(
            "UPDATE chat_submissions SET owner_id=?,lease_id=?,lease_until_ms=?,may_transmit=0
             WHERE principal=? AND account=? AND workspace=? AND submission_id=?",
        )
        .bind(owner.to_string())
        .bind(lease_id.to_string())
        .bind(now + LEASE_MS)
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((
            submission,
            ChatSubmissionLease {
                submission_id: id,
                lease_id,
            },
        ))
    }

    /// Invalidate a disconnected or changed authentication generation. All
    /// outstanding references to its leases become unusable immediately.
    pub async fn release_owner(&self, owner: Uuid) -> Result<(), ChatStoreError> {
        let mut connection = self.write_connection().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "UPDATE chat_submissions SET lease_id=?,lease_until_ms=0,may_transmit=0
             WHERE principal=? AND account=? AND workspace=? AND owner_id=? AND active=1",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .bind(owner.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Keep a newly assigned conversation ID even before backend acceptance is
    /// established. A crash after a creation event can then reconcile by exact
    /// message ID. Assignment alone does not establish message acceptance.
    pub async fn record_conversation(
        &self,
        lease: &ChatSubmissionLease,
        conversation_id: &str,
    ) -> Result<(), ChatStoreError> {
        validate_id(conversation_id)?;
        let mut connection = self.write_connection().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let current = owned_submission(&mut tx, &self.scope, lease)
            .await?
            .submission;
        bind_conversation(&mut tx, &self.scope, &current, conversation_id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Preserve a positively identified accepted user message independently of
    /// generation outcome. The lease stays active until finish_submission so
    /// reconciliation can also record a recovered generation outcome.
    pub async fn record_acceptance(
        &self,
        lease: &ChatSubmissionLease,
        proof: ChatAcceptanceProof,
    ) -> Result<(), ChatStoreError> {
        validate_id(&proof.conversation_id)?;
        validate_id(&proof.parent_message_id)?;
        let mut connection = self.write_connection().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let current = owned_submission(&mut tx, &self.scope, lease)
            .await?
            .submission;
        if !current.transmission_started
            || proof.client_message_id != current.client_message_id
            || proof.parent_message_id != current.expected_parent_id
            || current
                .conversation_id
                .as_ref()
                .is_some_and(|id| id != &proof.conversation_id)
        {
            return Err(ChatStoreError::InvalidTransition);
        }
        bind_conversation(&mut tx, &self.scope, &current, &proof.conversation_id).await?;
        sqlx::query(
            "UPDATE chat_submissions SET accepted=1,updated_at_ms=?
             WHERE principal=? AND account=? AND workspace=? AND submission_id=?",
        )
        .bind(Utc::now().timestamp_millis())
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .bind(lease.submission_id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Unknown acceptance after transmission remains pending regardless of a
    /// stream failure, local stop, or absent history entry. Accepted user text
    /// remains in the snapshot even when generation subsequently fails.
    pub async fn finish_submission(
        &self,
        lease: &ChatSubmissionLease,
        outcome: ChatSubmissionOutcome,
        resolved_model: Option<&str>,
    ) -> Result<(), ChatStoreError> {
        self.finish_submission_and_advance(lease, outcome, resolved_model, None)
            .await
    }

    /// Commit the terminal journal checkpoint and advance its unchanged draft
    /// together. A client disconnect cannot leave accepted input sendable, and
    /// a newer draft revision is never cleared or moved to another parent.
    pub async fn finish_submission_and_advance(
        &self,
        lease: &ChatSubmissionLease,
        outcome: ChatSubmissionOutcome,
        resolved_model: Option<&str>,
        next_parent_id: Option<&str>,
    ) -> Result<(), ChatStoreError> {
        if let Some(parent) = next_parent_id {
            validate_id(parent)?;
        }
        if outcome == ChatSubmissionOutcome::InProgress {
            return Err(ChatStoreError::InvalidTransition);
        }
        if let Some(model) = resolved_model {
            validate_id(model)?;
        }
        let mut connection = self.write_connection().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        let current = owned_submission(&mut tx, &self.scope, lease)
            .await?
            .submission;
        if outcome == ChatSubmissionOutcome::Completed && !current.accepted {
            return Err(ChatStoreError::InvalidTransition);
        }
        let active = current.transmission_started && !current.accepted;
        sqlx::query(
            "UPDATE chat_submissions SET outcome=?,resolved_model=COALESCE(?,resolved_model),
              active=?,may_transmit=0,updated_at_ms=?
             WHERE principal=? AND account=? AND workspace=? AND submission_id=?",
        )
        .bind(outcome.as_str())
        .bind(resolved_model)
        .bind(active)
        .bind(Utc::now().timestamp_millis())
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .bind(lease.submission_id.to_string())
        .execute(&mut *tx)
        .await?;
        if current.accepted
            && let Some(parent) = next_parent_id
            && let Some(conversation) = current.conversation_id.as_deref()
        {
            sqlx::query(
                "UPDATE chat_drafts SET revision=?,conversation_id=?,expected_parent_id=?,
                  text='',selections='[]',updated_at_ms=MAX(updated_at_ms+1,?)
                 WHERE principal=? AND account=? AND workspace=? AND draft_id=? AND revision=?",
            )
            .bind(Uuid::new_v4().to_string())
            .bind(conversation)
            .bind(parent)
            .bind(Utc::now().timestamp_millis())
            .bind(&self.scope.principal)
            .bind(&self.scope.account)
            .bind(&self.scope.workspace)
            .bind(current.draft_id.to_string())
            .bind(current.draft_revision.to_string())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn read_submission(
        &self,
        id: Uuid,
    ) -> Result<Option<ChatSubmission>, ChatStoreError> {
        let mut connection = self.pool.acquire().await?;
        read_row(&mut connection, &self.scope, id)
            .await?
            .as_ref()
            .map(submission_from_row)
            .transpose()
    }

    /// Reconnect snapshots list only pending IDs, not every private message
    /// body. The client reads a selected submission through its conversation.
    pub async fn pending_submission_ids(&self) -> Result<Vec<Uuid>, ChatStoreError> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT submission_id FROM chat_submissions
             WHERE principal=? AND account=? AND workspace=? AND active=1
             ORDER BY created_at_ms,submission_id LIMIT 1001",
        )
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .fetch_all(self.pool.as_ref())
        .await?;
        if ids.len() > 1000 {
            return Err(ChatStoreError::InvalidRecord);
        }
        ids.iter().map(|id| parse_uuid(id)).collect()
    }

    pub async fn pending_submission_for_draft(
        &self,
        draft_id: Uuid,
    ) -> Result<Option<ChatSubmission>, ChatStoreError> {
        let id: Option<String> = sqlx::query_scalar("SELECT submission_id FROM chat_submissions WHERE principal=? AND account=? AND workspace=? AND draft_id=? AND active=1")
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(draft_id.to_string())
            .fetch_optional(self.pool.as_ref()).await?;
        match id {
            Some(id) => self.read_submission(parse_uuid(&id)?).await,
            None => Ok(None),
        }
    }

    pub async fn pending_submission_for_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ChatSubmission>, ChatStoreError> {
        validate_id(conversation_id)?;
        let id: Option<String> = sqlx::query_scalar("SELECT submission_id FROM chat_submissions WHERE principal=? AND account=? AND workspace=? AND conversation_id=? AND active=1")
            .bind(&self.scope.principal).bind(&self.scope.account).bind(&self.scope.workspace).bind(conversation_id)
            .fetch_optional(self.pool.as_ref()).await?;
        match id {
            Some(id) => self.read_submission(parse_uuid(&id)?).await,
            None => Ok(None),
        }
    }
}

fn target_key(draft: Uuid, conversation: Option<&str>) -> String {
    conversation.map_or_else(
        || format!("draft:{draft}"),
        |id| format!("conversation:{id}"),
    )
}

async fn bind_conversation(
    connection: &mut SqliteConnection,
    scope: &ChatScope,
    current: &ChatSubmission,
    conversation_id: &str,
) -> Result<(), ChatStoreError> {
    if !current.transmission_started
        || current
            .conversation_id
            .as_ref()
            .is_some_and(|id| id != conversation_id)
    {
        return Err(ChatStoreError::InvalidTransition);
    }
    let target = target_key(current.draft_id, Some(conversation_id));
    let conflict: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM chat_submissions WHERE principal=? AND account=? AND workspace=?
         AND target_key=? AND active=1 AND submission_id!=?)",
    )
    .bind(&scope.principal)
    .bind(&scope.account)
    .bind(&scope.workspace)
    .bind(&target)
    .bind(current.submission_id.to_string())
    .fetch_one(&mut *connection)
    .await?;
    if conflict {
        return Err(ChatStoreError::PendingSubmission);
    }
    sqlx::query(
        "UPDATE chat_submissions SET conversation_id=?,target_key=?,updated_at_ms=?
         WHERE principal=? AND account=? AND workspace=? AND submission_id=?",
    )
    .bind(conversation_id)
    .bind(target)
    .bind(Utc::now().timestamp_millis())
    .bind(&scope.principal)
    .bind(&scope.account)
    .bind(&scope.workspace)
    .bind(current.submission_id.to_string())
    .execute(connection)
    .await?;
    Ok(())
}

struct OwnedSubmission {
    submission: ChatSubmission,
    may_transmit: bool,
}

async fn owned_submission(
    connection: &mut SqliteConnection,
    scope: &ChatScope,
    lease: &ChatSubmissionLease,
) -> Result<OwnedSubmission, ChatStoreError> {
    let row = read_row(connection, scope, lease.submission_id)
        .await?
        .ok_or(ChatStoreError::NotFound)?;
    let submission = submission_from_row(&row)?;
    if !submission.active
        || parse_uuid(row.try_get("lease_id")?)? != lease.lease_id
        || row.try_get::<i64, _>("lease_until_ms")? <= Utc::now().timestamp_millis()
    {
        return Err(ChatStoreError::LeaseLost);
    }
    Ok(OwnedSubmission {
        submission,
        may_transmit: row.try_get("may_transmit")?,
    })
}

async fn read_row(
    connection: &mut SqliteConnection,
    scope: &ChatScope,
    id: Uuid,
) -> Result<Option<SqliteRow>, ChatStoreError> {
    Ok(sqlx::query(
        "SELECT * FROM chat_submissions WHERE principal=? AND account=? AND workspace=? AND submission_id=?",
    )
    .bind(&scope.principal)
    .bind(&scope.account)
    .bind(&scope.workspace)
    .bind(id.to_string())
    .fetch_optional(connection)
    .await?)
}

fn submission_from_row(row: &SqliteRow) -> Result<ChatSubmission, ChatStoreError> {
    let snapshot: ChatSubmissionSnapshot = decode(row.try_get("snapshot")?)?;
    snapshot
        .validate()
        .map_err(|_| ChatStoreError::InvalidRecord)?;
    let submission = ChatSubmission {
        submission_id: parse_uuid(row.try_get("submission_id")?)?,
        client_message_id: parse_uuid(row.try_get("client_message_id")?)?,
        draft_id: parse_uuid(row.try_get("draft_id")?)?,
        draft_revision: parse_uuid(row.try_get("draft_revision")?)?,
        conversation_id: row.try_get("conversation_id")?,
        expected_parent_id: row.try_get("expected_parent_id")?,
        snapshot,
        transmission_started: row.try_get("transmission_started")?,
        accepted: row.try_get("accepted")?,
        resolved_model: row.try_get("resolved_model")?,
        outcome: ChatSubmissionOutcome::parse(row.try_get("outcome")?)?,
        active: row.try_get("active")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
    };
    for id in [
        Some(&submission.expected_parent_id),
        submission.conversation_id.as_ref(),
        submission.resolved_model.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_id(id).map_err(|_| ChatStoreError::InvalidRecord)?;
    }
    if submission
        .snapshot
        .draft
        .conversation_id
        .as_ref()
        .is_some_and(|id| Some(id) != submission.conversation_id.as_ref())
        || submission
            .snapshot
            .draft
            .expected_parent_id
            .as_ref()
            .is_some_and(|id| id != &submission.expected_parent_id)
        || row.try_get::<&str, _>("target_key")?
            != target_key(submission.draft_id, submission.conversation_id.as_deref())
    {
        return Err(ChatStoreError::InvalidRecord);
    }
    Ok(submission)
}
