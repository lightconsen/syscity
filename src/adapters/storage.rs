//! Storage adapter for Syscity
//!
//! This module provides storage abstractions and implementations.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use sqlx::Row;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::warn;

use crate::core::models::{Entity, Id};
use crate::error::SyscityError;
// Re-export ChatMessage for users of the adapters crate
pub use crate::memory::ChatMessage;

/// Errors that can occur during storage operations
#[derive(Error, Debug)]
pub enum StorageError {
    #[error("Entity not found: {0}")]
    NotFound(Id),

    #[error("Storage is full")]
    Full,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Storage backend error: {0}")]
    Backend(String),
}

impl From<StorageError> for SyscityError {
    fn from(err: StorageError) -> Self {
        match err {
            StorageError::NotFound(id) => SyscityError::NotFound {
                resource: format!("Entity {} not found", id),
            },
            StorageError::Full => SyscityError::Validation("Storage is full".to_string()),
            StorageError::Io(e) => SyscityError::Io(e),
            StorageError::Serialization(msg) => {
                SyscityError::Internal(format!("Serialization error: {}", msg))
            }
            StorageError::Backend(msg) => {
                SyscityError::Internal(format!("Storage backend: {}", msg))
            }
        }
    }
}

/// Storage trait for entity persistence
#[async_trait]
pub trait Storage: Send + Sync {
    /// Get an entity by ID
    async fn get(&self, id: Id) -> Result<Entity, StorageError>;

    /// List all entities, optionally filtered by status
    async fn list(&self) -> Result<Vec<Entity>, StorageError>;

    /// Create a new entity
    async fn create(&self, entity: &Entity) -> Result<(), StorageError>;

    /// Update an existing entity
    async fn update(&self, entity: &Entity) -> Result<(), StorageError>;

    /// Delete an entity
    async fn delete(&self, id: Id) -> Result<(), StorageError>;

    /// Count total entities
    async fn count(&self) -> Result<usize, StorageError>;

    /// Check if storage is healthy
    async fn health_check(&self) -> Result<(), StorageError>;

    /// Get conversation history for a session
    /// Default implementation returns empty (for stores that don't support chat
    /// history)
    async fn get_conversation_history(
        &self,
        _conversation_id: &str,
        _limit: usize,
    ) -> Result<Vec<ChatMessage>, StorageError> {
        Ok(Vec::new())
    }

    /// Get last conversation ID for a user
    /// Default implementation returns None (for stores that don't support chat
    /// history)
    async fn get_last_conversation(&self, _user_id: &str) -> Result<Option<String>, StorageError> {
        Ok(None)
    }

    /// Get list of conversations for a user
    /// Default implementation returns empty (for stores that don't support chat
    /// history)
    async fn get_user_conversations(
        &self,
        _user_id: &str,
        _limit: usize,
    ) -> Result<Vec<String>, StorageError> {
        Ok(Vec::new())
    }

    /// Release backend resources at shutdown.
    ///
    /// Default implementation does nothing: there is nothing to release for a
    /// store that owns no connection. [`SqliteStorage`] overrides it to close
    /// the connection pool, which is what checkpoints the WAL — every
    /// sqlite-backed store (sessions, memory, auth, audit) holds a clone of
    /// that same pool, so closing it through any one of them closes them all.
    ///
    /// Callers must have stopped writing first: [`sqlx::Pool::close`] waits for
    /// every checked-out connection to come back, so calling it while a task is
    /// mid-query blocks instead of flushing.
    async fn close(&self) -> Result<(), StorageError> {
        Ok(())
    }
}

/// In-memory storage implementation
#[derive(Debug, Clone)]
pub struct InMemoryStorage {
    data: Arc<RwLock<HashMap<Id, Entity>>>,
    max_size: usize,
}

impl InMemoryStorage {
    /// Create a new in-memory storage with default capacity
    pub fn new() -> Self {
        Self::with_capacity(10_000)
    }

    /// Create a new in-memory storage with specified max size
    pub fn with_capacity(max_size: usize) -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::with_capacity(max_size))),
            max_size,
        }
    }
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Storage for InMemoryStorage {
    async fn get(&self, id: Id) -> Result<Entity, StorageError> {
        let data = self.data.read().await;
        data.get(&id).cloned().ok_or(StorageError::NotFound(id))
    }

    async fn list(&self) -> Result<Vec<Entity>, StorageError> {
        let data = self.data.read().await;
        Ok(data.values().cloned().collect())
    }

    async fn create(&self, entity: &Entity) -> Result<(), StorageError> {
        let mut data = self.data.write().await;

        if data.len() >= self.max_size {
            return Err(StorageError::Full);
        }

        data.insert(entity.id, entity.clone());
        Ok(())
    }

    async fn update(&self, entity: &Entity) -> Result<(), StorageError> {
        let mut data = self.data.write().await;

        if !data.contains_key(&entity.id) {
            return Err(StorageError::NotFound(entity.id));
        }

        data.insert(entity.id, entity.clone());
        Ok(())
    }

    async fn delete(&self, id: Id) -> Result<(), StorageError> {
        let mut data = self.data.write().await;

        data.remove(&id)
            .ok_or(StorageError::NotFound(id))
            .map(|_| ())
    }

    async fn count(&self) -> Result<usize, StorageError> {
        let data = self.data.read().await;
        Ok(data.len())
    }

    async fn health_check(&self) -> Result<(), StorageError> {
        let _guard = self.data.read().await;
        Ok(())
    }
}

/// File-based storage implementation
#[derive(Debug, Clone)]
pub struct FileStorage {
    base_path: PathBuf,
}

impl FileStorage {
    /// Create a new file storage at the given path
    pub fn new(base_path: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let base_path = base_path.into();
        std::fs::create_dir_all(&base_path)?;

        Ok(Self { base_path })
    }

    /// Get the path for a specific entity
    fn entity_path(&self, id: Id) -> PathBuf {
        self.base_path.join(format!("{}.json", id))
    }
}

#[async_trait]
impl Storage for FileStorage {
    async fn get(&self, id: Id) -> Result<Entity, StorageError> {
        let path = self.entity_path(id);

        if !path.exists() {
            return Err(StorageError::NotFound(id));
        }

        let content = tokio::fs::read_to_string(&path).await?;
        let entity: Entity = serde_json::from_str(&content)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        Ok(entity)
    }

    async fn list(&self) -> Result<Vec<Entity>, StorageError> {
        let mut entities = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.base_path).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                let content = tokio::fs::read_to_string(&path).await?;
                match serde_json::from_str::<Entity>(&content) {
                    Ok(entity) => entities.push(entity),
                    Err(e) => {
                        warn!(path = %path.display(), error = %e, "Skipping corrupted entity file");
                    }
                }
            }
        }

        Ok(entities)
    }

    async fn create(&self, entity: &Entity) -> Result<(), StorageError> {
        let path = self.entity_path(entity.id);

        if path.exists() {
            return Err(StorageError::Backend(format!("Entity {} already exists", entity.id)));
        }

        let content = serde_json::to_string_pretty(entity)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        // Atomic write: write to temp file, then rename
        let tmp_path = path.with_extension("tmp");
        tokio::fs::write(&tmp_path, &content).await?;
        tokio::fs::rename(&tmp_path, &path).await?;
        Ok(())
    }

    async fn update(&self, entity: &Entity) -> Result<(), StorageError> {
        let path = self.entity_path(entity.id);

        if !path.exists() {
            return Err(StorageError::NotFound(entity.id));
        }

        let content = serde_json::to_string_pretty(entity)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        // Atomic write: write to temp file, then rename
        let tmp_path = path.with_extension("tmp");
        tokio::fs::write(&tmp_path, &content).await?;
        tokio::fs::rename(&tmp_path, &path).await?;
        Ok(())
    }

    async fn delete(&self, id: Id) -> Result<(), StorageError> {
        let path = self.entity_path(id);

        if !path.exists() {
            return Err(StorageError::NotFound(id));
        }

        tokio::fs::remove_file(&path).await?;
        Ok(())
    }

    async fn count(&self) -> Result<usize, StorageError> {
        let mut count = 0;
        let mut entries = tokio::fs::read_dir(&self.base_path).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                count += 1;
            }
        }

        Ok(count)
    }

    async fn health_check(&self) -> Result<(), StorageError> {
        // Check if we can read the directory
        let _ = tokio::fs::read_dir(&self.base_path).await?;
        Ok(())
    }
}

/// SQLite-backed storage implementation
#[derive(Debug, Clone)]
pub struct SqliteStorage {
    pool: sqlx::SqlitePool,
}

impl SqliteStorage {
    /// Create a new SQLite storage with an existing pool
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }

    /// Create a new SQLite storage from a database URL
    pub async fn connect(database_url: &str) -> Result<Self, StorageError> {
        let pool = sqlx::SqlitePool::connect(database_url)
            .await
            .map_err(|e| StorageError::Backend(format!("Failed to connect: {}", e)))?;

        let storage = Self { pool };
        storage.init().await?;
        Ok(storage)
    }

    /// Initialize the database schema
    async fn init(&self) -> Result<(), StorageError> {
        // Core entities table
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS entities (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT,
                tags TEXT,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                version INTEGER NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| StorageError::Backend(format!("Failed to create entities table: {}", e)))?;

        // Create index on status for faster filtering
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_entities_status ON entities(status)")
            .execute(&self.pool)
            .await
            .map_err(|e| StorageError::Backend(format!("Failed to create index: {}", e)))?;

        // Chat messages table for conversation history
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS chat_messages (
                id TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                metadata TEXT  -- JSON
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| {
            StorageError::Backend(format!("Failed to create chat_messages table: {}", e))
        })?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_chat_messages_conversation ON \
             chat_messages(conversation_id, created_at DESC)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| StorageError::Backend(format!("Failed to create chat index: {}", e)))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_chat_messages_user ON chat_messages(user_id, \
             created_at DESC)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| StorageError::Backend(format!("Failed to create user chat index: {}", e)))?;

        // Memories table for agent memory
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                conversation_id TEXT,
                content TEXT NOT NULL,
                memory_type TEXT NOT NULL,
                embedding BLOB,  -- Serialized f32 array, optional
                created_at INTEGER NOT NULL,
                expires_at INTEGER,  -- NULL = never
                metadata TEXT  -- JSON
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| StorageError::Backend(format!("Failed to create memories table: {}", e)))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memories_user ON memories(user_id, memory_type)",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| StorageError::Backend(format!("Failed to create memory index: {}", e)))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memories_expires ON memories(expires_at) WHERE \
             expires_at IS NOT NULL",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| {
            StorageError::Backend(format!("Failed to create memory expires index: {}", e))
        })?;

        // Migration: add columns that may not exist in older schemas
        for migration in &[
            "ALTER TABLE memories ADD COLUMN importance_score REAL NOT NULL DEFAULT 0.5",
            "ALTER TABLE memories ADD COLUMN source TEXT NOT NULL DEFAULT 'agent'",
        ] {
            if let Err(e) = sqlx::query(migration).execute(&self.pool).await {
                // SQLite returns error code 1 for "duplicate column" — this is
                // expected during startup if the column already exists.
                // The error message contains "duplicate column name" on SQLite >= 3.36
                // or a generic "duplicate column" on older versions.
                // We only surface non-duplicate-column errors.
                let msg = e.to_string();
                if !msg.contains("duplicate column") {
                    return Err(StorageError::Backend(format!(
                        "Migration failed: {}: {}",
                        migration, msg
                    )));
                }
            }
        }

        Ok(())
    }

    /// Convert a database row to an Entity
    fn row_to_entity(row: &sqlx::sqlite::SqliteRow) -> Result<Entity, StorageError> {
        use chrono::DateTime;

        use crate::core::models::{Metadata, Status};

        let id_str: String = row.get("id");
        let id = Id::parse(&id_str)
            .map_err(|_| StorageError::Serialization(format!("Invalid ID: {}", id_str)))?;

        let name: String = row.get("name");
        let description: Option<String> = row.get("description");

        let tags_str: String = row.get("tags");
        let tags: Vec<String> = serde_json::from_str(&tags_str)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        let status_str: String = row.get("status");
        let status = status_str.parse::<Status>().map_err(|e| {
            StorageError::Serialization(format!("Invalid status: {} - {}", status_str, e))
        })?;

        let created_at_str: String = row.get("created_at");
        let created_at = DateTime::parse_from_rfc3339(&created_at_str)
            .map_err(|e| StorageError::Serialization(e.to_string()))?
            .with_timezone(&chrono::Utc);

        let updated_at_str: String = row.get("updated_at");
        let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
            .map_err(|e| StorageError::Serialization(e.to_string()))?
            .with_timezone(&chrono::Utc);

        let version: i64 = row.get("version");

        Ok(Entity {
            id,
            name,
            description,
            tags: Some(tags),
            status,
            metadata: Metadata {
                created_at,
                updated_at,
                version: version as u64,
                tags: None,
            },
        })
    }
}

#[async_trait]
impl Storage for SqliteStorage {
    /// Close the shared pool: checkpoints the WAL and releases the database
    /// file, so shutdown does not have to rely on process exit for it.
    async fn close(&self) -> Result<(), StorageError> {
        self.pool.close().await;
        Ok(())
    }

    async fn get(&self, id: Id) -> Result<Entity, StorageError> {
        let row = sqlx::query("SELECT * FROM entities WHERE id = ?1")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StorageError::Backend(e.to_string()))?;

        match row {
            Some(row) => Self::row_to_entity(&row),
            None => Err(StorageError::NotFound(id)),
        }
    }

    async fn list(&self) -> Result<Vec<Entity>, StorageError> {
        let rows = sqlx::query("SELECT * FROM entities ORDER BY created_at DESC")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| StorageError::Backend(e.to_string()))?;

        rows.iter().map(Self::row_to_entity).collect()
    }

    async fn create(&self, entity: &Entity) -> Result<(), StorageError> {
        let tags_json = serde_json::to_string(&entity.tags)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        sqlx::query(
            r#"
            INSERT INTO entities (id, name, description, tags, status, created_at, updated_at, version)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#
        )
        .bind(entity.id.to_string())
        .bind(&entity.name)
        .bind(&entity.description)
        .bind(tags_json)
        .bind(entity.status.to_string())
        .bind(entity.metadata.created_at.to_rfc3339())
        .bind(entity.metadata.updated_at.to_rfc3339())
        .bind(entity.metadata.version as i64)
        .execute(&self.pool)
        .await
        .map_err(|e| StorageError::Backend(e.to_string()))?;

        Ok(())
    }

    async fn update(&self, entity: &Entity) -> Result<(), StorageError> {
        let tags_json = serde_json::to_string(&entity.tags)
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        let result = sqlx::query(
            r#"
            UPDATE entities
            SET name = ?1, description = ?2, tags = ?3, status = ?4,
                updated_at = ?5, version = ?6
            WHERE id = ?7
            "#,
        )
        .bind(&entity.name)
        .bind(&entity.description)
        .bind(tags_json)
        .bind(entity.status.to_string())
        .bind(entity.metadata.updated_at.to_rfc3339())
        .bind(entity.metadata.version as i64)
        .bind(entity.id.to_string())
        .execute(&self.pool)
        .await
        .map_err(|e| StorageError::Backend(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(StorageError::NotFound(entity.id));
        }

        Ok(())
    }

    async fn delete(&self, id: Id) -> Result<(), StorageError> {
        let result = sqlx::query("DELETE FROM entities WHERE id = ?1")
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| StorageError::Backend(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(StorageError::NotFound(id));
        }

        Ok(())
    }

    async fn count(&self) -> Result<usize, StorageError> {
        let row = sqlx::query("SELECT COUNT(*) as count FROM entities")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| StorageError::Backend(e.to_string()))?;

        let count: i64 = row.get("count");
        Ok(count as usize)
    }

    async fn health_check(&self) -> Result<(), StorageError> {
        // Try to execute a simple query
        let _: (i64,) = sqlx::query_as("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| StorageError::Backend(e.to_string()))?;

        Ok(())
    }

    /// Override to provide actual chat history from SQLite
    async fn get_conversation_history(
        &self,
        conversation_id: &str,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, StorageError> {
        let rows = sqlx::query(
            r#"
            SELECT id, conversation_id, user_id, role, content, created_at, metadata
            FROM chat_messages
            WHERE conversation_id = ?1
            ORDER BY created_at DESC
            LIMIT ?2
            "#,
        )
        .bind(conversation_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StorageError::Backend(format!("Failed to get conversation history: {}", e)))?;

        let messages: Vec<ChatMessage> = rows
            .into_iter()
            .filter_map(|row| {
                let created_at_secs: i64 = row.get("created_at");
                let created_at = secs_to_system_time(created_at_secs)?;
                let metadata: Option<String> = row.get("metadata");
                let metadata = metadata.and_then(|m| serde_json::from_str(&m).ok());
                Some(ChatMessage {
                    id: row.get("id"),
                    conversation_id: row.get("conversation_id"),
                    user_id: row.get("user_id"),
                    role: row.get("role"),
                    content: row.get("content"),
                    created_at,
                    metadata,
                })
            })
            .rev()
            .collect();

        Ok(messages)
    }

    /// Override to provide actual last conversation lookup from SQLite
    async fn get_last_conversation(&self, user_id: &str) -> Result<Option<String>, StorageError> {
        let row: Option<(String,)> = sqlx::query_as(
            r#"
            SELECT conversation_id FROM chat_messages
            WHERE user_id = ?1
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| StorageError::Backend(e.to_string()))?;

        Ok(row.map(|r| r.0))
    }

    /// Override to provide actual conversation list from SQLite
    async fn get_user_conversations(
        &self,
        user_id: &str,
        limit: usize,
    ) -> Result<Vec<String>, StorageError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"
            SELECT conversation_id
            FROM chat_messages
            WHERE user_id = ?1
            GROUP BY conversation_id
            ORDER BY MAX(created_at) DESC
            LIMIT ?2
            "#,
        )
        .bind(user_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StorageError::Backend(e.to_string()))?;

        Ok(rows.into_iter().map(|r| r.0).collect())
    }
}

// =============================================================================
// Helper functions
// =============================================================================

fn secs_to_system_time(secs: i64) -> Option<std::time::SystemTime> {
    if secs <= 0 {
        None
    } else {
        Some(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::Entity;

    #[tokio::test]
    async fn test_in_memory_storage() {
        let storage = InMemoryStorage::new();

        // Test empty storage
        assert_eq!(storage.count().await.unwrap(), 0);

        // Create entity
        let entity = Entity::new("Test Entity");
        storage.create(&entity).await.unwrap();
        assert_eq!(storage.count().await.unwrap(), 1);

        // Get entity
        let retrieved = storage.get(entity.id).await.unwrap();
        assert_eq!(retrieved.id, entity.id);
        assert_eq!(retrieved.name, entity.name);

        // Update entity
        let mut updated = entity.clone();
        updated.set_name("Updated Name");
        storage.update(&updated).await.unwrap();

        let retrieved = storage.get(entity.id).await.unwrap();
        assert_eq!(retrieved.name, "Updated Name");

        // Delete entity
        storage.delete(entity.id).await.unwrap();
        assert_eq!(storage.count().await.unwrap(), 0);
        assert!(storage.get(entity.id).await.is_err());
    }

    #[tokio::test]
    async fn test_storage_capacity() {
        let storage = InMemoryStorage::with_capacity(2);

        storage.create(&Entity::new("Entity 1")).await.unwrap();
        storage.create(&Entity::new("Entity 2")).await.unwrap();

        // Third entity should fail
        assert!(storage.create(&Entity::new("Entity 3")).await.is_err());
    }

    #[tokio::test]
    async fn test_in_memory_storage_list() {
        let storage = InMemoryStorage::new();
        let e1 = Entity::new("Entity 1");
        let e2 = Entity::new("Entity 2");
        storage.create(&e1).await.unwrap();
        storage.create(&e2).await.unwrap();

        let list = storage.list().await.unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn test_in_memory_storage_health_check() {
        let storage = InMemoryStorage::new();
        assert!(storage.health_check().await.is_ok());
    }

    #[tokio::test]
    async fn test_in_memory_storage_get_not_found() {
        let storage = InMemoryStorage::new();
        let id = Id::new();
        let err = storage.get(id).await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_in_memory_storage_update_not_found() {
        let storage = InMemoryStorage::new();
        let entity = Entity::new("Test");
        let err = storage.update(&entity).await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_in_memory_storage_delete_not_found() {
        let storage = InMemoryStorage::new();
        let id = Id::new();
        let err = storage.delete(id).await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[test]
    fn test_in_memory_storage_default() {
        let storage: InMemoryStorage = Default::default();
        assert_eq!(storage.max_size, 10_000);
    }

    /// `close()` is what flushes the shared pool at shutdown, so it has to
    /// really close it — a no-op would leave the checkpoint to process exit.
    #[tokio::test]
    async fn test_sqlite_storage_close_releases_the_pool() {
        let tmp = tempfile::tempdir().unwrap();
        // sqlx will not create the file itself (the gateway creates it before
        // connecting, see `init_storage`).
        let db_path = tmp.path().join("close.db");
        std::fs::File::create(&db_path).unwrap();
        let url = format!("sqlite:///{}", db_path.display());
        let storage = SqliteStorage::connect(&url).await.unwrap();

        assert!(storage.count().await.is_ok(), "usable before close");
        storage.close().await.unwrap();
        assert!(
            storage.count().await.is_err(),
            "close() must release the pool, not just empty it"
        );
    }

    #[tokio::test]
    async fn test_file_storage_crud() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(tmp.path()).unwrap();

        let entity = Entity::new("File Entity");
        storage.create(&entity).await.unwrap();

        let retrieved = storage.get(entity.id).await.unwrap();
        assert_eq!(retrieved.id, entity.id);
        assert_eq!(retrieved.name, "File Entity");

        let mut updated = entity.clone();
        updated.set_name("Updated");
        storage.update(&updated).await.unwrap();

        let retrieved = storage.get(entity.id).await.unwrap();
        assert_eq!(retrieved.name, "Updated");

        let list = storage.list().await.unwrap();
        assert_eq!(list.len(), 1);

        storage.delete(entity.id).await.unwrap();
        assert!(storage.get(entity.id).await.is_err());
    }

    #[tokio::test]
    async fn test_file_storage_count() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(tmp.path()).unwrap();

        assert_eq!(storage.count().await.unwrap(), 0);
        storage.create(&Entity::new("E1")).await.unwrap();
        assert_eq!(storage.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_file_storage_health_check() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(tmp.path()).unwrap();
        assert!(storage.health_check().await.is_ok());
    }

    #[tokio::test]
    async fn test_file_storage_create_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(tmp.path()).unwrap();

        let entity = Entity::new("Test");
        storage.create(&entity).await.unwrap();
        let err = storage.create(&entity).await.unwrap_err();
        assert!(matches!(err, StorageError::Backend(_)));
    }

    #[test]
    fn test_storage_error_display() {
        let id = Id::new();
        assert_eq!(StorageError::NotFound(id).to_string(), format!("Entity not found: {}", id));
        assert_eq!(StorageError::Full.to_string(), "Storage is full");
        assert_eq!(
            StorageError::Serialization("bad json".to_string()).to_string(),
            "Serialization error: bad json"
        );
        assert_eq!(
            StorageError::Backend("db down".to_string()).to_string(),
            "Storage backend error: db down"
        );
    }

    #[test]
    fn test_storage_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let storage_err: StorageError = io_err.into();
        assert!(storage_err.to_string().contains("file missing"));
    }

    #[test]
    fn test_secs_to_system_time_zero_returns_none() {
        assert!(secs_to_system_time(0).is_none());
        assert!(secs_to_system_time(-1).is_none());
    }

    #[test]
    fn test_secs_to_system_time_positive() {
        let st = secs_to_system_time(1_000_000);
        assert!(st.is_some());
    }
}
