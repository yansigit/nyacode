//! Account-scoped storage for saved Chat only. Callers must establish trusted
//! local origin and current authentication before obtaining this store. A
//! temporary session must never obtain it, even for generic composer history.
//!
//! Content-bearing types deliberately omit Debug. Errors never include SQL,
//! account identifiers, paths, or payloads. Only typed schemas reach SQLite.

mod cache;
mod drafts;
mod submissions;
mod types;

pub use cache::types::*;
pub use types::*;

use super::StateRuntime;
use sqlx::Connection;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::pool::PoolConnection;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
pub struct SavedChatStore {
    pool: Arc<SqlitePool>,
    scope: ChatScope,
}

impl StateRuntime {
    pub fn saved_chat(&self, scope: ChatScope) -> SavedChatStore {
        SavedChatStore {
            pool: Arc::clone(&self.pool),
            scope,
        }
    }
}

impl SavedChatStore {
    /// Persist only the non-secret device/profile identity. Parallel openers
    /// receive the same identity; a profile upgrade requires an explicit adapter.
    pub async fn profile(&self, profile_version: &str) -> Result<ChatProfile, ChatStoreError> {
        validate_id(profile_version)?;
        let mut connection = self.write_connection().await?;
        let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO chat_profiles(principal,account,workspace,device_id,profile_version)
             VALUES(?,?,?,?,?) ON CONFLICT(principal,account,workspace) DO NOTHING",
        )
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .bind(Uuid::new_v4().to_string())
        .bind(profile_version)
        .execute(&mut *tx)
        .await?;
        let (device, version): (String, String) = sqlx::query_as(
            "SELECT device_id,profile_version FROM chat_profiles
             WHERE principal=? AND account=? AND workspace=?",
        )
        .bind(&self.scope.principal)
        .bind(&self.scope.account)
        .bind(&self.scope.workspace)
        .fetch_one(&mut *tx)
        .await?;
        if version != profile_version {
            return Err(ChatStoreError::ProfileMismatch);
        }
        let device_id = parse_uuid(&device)?;
        tx.commit().await?;
        Ok(ChatProfile { device_id, version })
    }

    async fn write_connection(&self) -> Result<PoolConnection<Sqlite>, ChatStoreError> {
        let mut connection = self.pool.acquire().await?;
        // The general state pool uses NORMAL. A committed saved-send journal
        // must survive a power failure before any HTTP transmission is permitted.
        // FULL adds WAL synchronization at each commit; fullfsync also requests
        // the stronger macOS flush. These settings are per connection and are
        // deliberately never weakened when returning it to the shared pool.
        // https://www.sqlite.org/pragma.html#pragma_synchronous
        sqlx::query("PRAGMA synchronous = FULL")
            .execute(&mut *connection)
            .await?;
        sqlx::query("PRAGMA fullfsync = ON")
            .execute(&mut *connection)
            .await?;
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&mut *connection)
            .await?;
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut *connection)
            .await?;
        if synchronous < 2 || !mode.eq_ignore_ascii_case("wal") {
            return Err(ChatStoreError::Unavailable);
        }
        Ok(connection)
    }
}

fn parse_uuid(value: &str) -> Result<Uuid, ChatStoreError> {
    Uuid::parse_str(value).map_err(|_| ChatStoreError::InvalidRecord)
}

#[cfg(test)]
mod tests;
