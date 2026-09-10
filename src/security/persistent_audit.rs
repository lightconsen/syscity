//! Persistent Audit Log for Syscity
//!
//! Provides SQLite-backed persistent storage of audit entries,
//! complementing the in-memory RuntimeAuditLog with durability.
//!
//! All security-relevant events are written to SQLite for:
//! - Forensic analysis
//! - Compliance reporting
//! - Long-term retention

use std::sync::Arc;

use sqlx::Row;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::security::request_context::RequestContext;
use crate::security::runtime_audit::{AuditEntry, AuditEventType, AuditLogger};

/// Persistent audit log backed by SQLite
#[derive(Debug, Clone)]
pub struct PersistentAuditLog {
    /// In-memory ring buffer for fast recent queries
    memory: Arc<RwLock<Vec<AuditEntry>>>,
    /// SQLite pool for persistent storage
    pool: Option<sqlx::SqlitePool>,
    /// Max in-memory entries
    memory_capacity: usize,
}

impl PersistentAuditLog {
    /// Create a new persistent audit log (in-memory only, no persistence)
    pub fn new() -> Self {
        Self {
            memory: Arc::new(RwLock::new(Vec::new())),
            pool: None,
            memory_capacity: 1000,
        }
    }

    /// Create with SQLite persistence
    pub fn with_pool(pool: sqlx::SqlitePool) -> Self {
        Self {
            memory: Arc::new(RwLock::new(Vec::new())),
            pool: Some(pool),
            memory_capacity: 1000,
        }
    }

    /// Initialize the audit table (call once at startup)
    pub async fn init(&self) -> crate::Result<()> {
        if let Some(ref pool) = self.pool {
            sqlx::query(
                r#"
                CREATE TABLE IF NOT EXISTS audit_log (
                    id TEXT PRIMARY KEY,
                    timestamp REAL NOT NULL,
                    event_type TEXT NOT NULL,
                    actor TEXT NOT NULL,
                    target TEXT NOT NULL,
                    allowed INTEGER NOT NULL,
                    description TEXT NOT NULL,
                    details TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_audit_timestamp ON audit_log(timestamp);
                CREATE INDEX IF NOT EXISTS idx_audit_event_type ON audit_log(event_type);
                CREATE INDEX IF NOT EXISTS idx_audit_actor ON audit_log(actor);
                "#,
            )
            .execute(pool)
            .await
            .map_err(|e| crate::error::SyscityError::Storage {
                context: "Failed to create audit table".into(),
                details: e.to_string(),
            })?;
            info!("Persistent audit log table initialized");
        }
        Ok(())
    }

    /// Log a new audit entry (memory + persistent storage)
    pub async fn log(
        &self,
        event_type: AuditEventType,
        actor: impl Into<String>,
        target: impl Into<String>,
        allowed: bool,
        description: impl Into<String>,
        details: Option<serde_json::Value>,
    ) {
        let entry = AuditEntry {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: std::time::SystemTime::now(),
            event_type,
            actor: actor.into(),
            target: target.into(),
            allowed,
            description: description.into(),
            details: details.clone(),
        };

        // Store in memory
        {
            let mut mem = self.memory.write().await;
            if mem.len() >= self.memory_capacity {
                mem.remove(0);
            }
            mem.push(entry.clone());
        }

        // Persist to SQLite
        if let Some(ref pool) = self.pool {
            let details_str = details.map(|d| d.to_string());
            let timestamp_secs = entry
                .timestamp
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();

            if let Err(e) = sqlx::query(
                r#"
                INSERT INTO audit_log (id, timestamp, event_type, actor, target, allowed, description, details)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
            )
            .bind(&entry.id)
            .bind(timestamp_secs)
            .bind(format!("{:?}", entry.event_type))
            .bind(&entry.actor)
            .bind(&entry.target)
            .bind(if entry.allowed { 1 } else { 0 })
            .bind(&entry.description)
            .bind(details_str)
            .execute(pool)
            .await
            {
                warn!("Failed to persist audit entry: {}", e);
            }
        }
    }

    /// Log a new audit entry using the actor carried by `ctx`.
    ///
    /// Convenience wrapper over [`PersistentAuditLog::log`] so per-request
    /// call sites thread one [`RequestContext`] instead of re-deriving the
    /// actor string (and so the actor can no longer silently fall back to a
    /// hardcoded literal).
    pub async fn log_with_context(
        &self,
        event_type: AuditEventType,
        ctx: &RequestContext,
        target: impl Into<String>,
        allowed: bool,
        description: impl Into<String>,
        details: Option<serde_json::Value>,
    ) {
        self.log(event_type, ctx.actor(), target, allowed, description, details)
            .await;
    }

    /// Retrieve recent entries from memory (fast)
    pub async fn recent(&self, n: usize) -> Vec<AuditEntry> {
        let mem = self.memory.read().await;
        mem.iter().rev().take(n).cloned().collect()
    }

    /// Query persistent store for entries in a time range
    pub async fn query_range(
        &self,
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
        limit: i64,
    ) -> Vec<AuditEntry> {
        let mut entries = Vec::new();
        if let Some(ref pool) = self.pool {
            let start_secs = start.timestamp() as f64;
            let end_secs = end.timestamp() as f64;

            match sqlx::query(
                r#"
                SELECT id, timestamp, event_type, actor, target, allowed, description, details
                FROM audit_log
                WHERE timestamp >= ?1 AND timestamp <= ?2
                ORDER BY timestamp DESC
                LIMIT ?3
                "#,
            )
            .bind(start_secs)
            .bind(end_secs)
            .bind(limit)
            .fetch_all(pool)
            .await
            {
                Ok(rows) => {
                    for row in rows {
                        let event_type_str: String = row.get("event_type");
                        let event_type = match event_type_str.as_str() {
                            "AccessCheck" => AuditEventType::AccessCheck,
                            "PairingRequest" => AuditEventType::PairingRequest,
                            "PairingApprove" => AuditEventType::PairingApprove,
                            "PairingReject" => AuditEventType::PairingReject,
                            "PairingRevoke" => AuditEventType::PairingRevoke,
                            "CommandGate" => AuditEventType::CommandGate,
                            "ConfigChange" => AuditEventType::ConfigChange,
                            "ToolInvocation" => AuditEventType::ToolInvocation,
                            "ToolDeny" => AuditEventType::ToolDeny,
                            _ => AuditEventType::Security,
                        };

                        let details: Option<String> = row.get("details");
                        let details_json = details.and_then(|d| serde_json::from_str(&d).ok());

                        entries.push(AuditEntry {
                            id: row.get("id"),
                            timestamp: std::time::UNIX_EPOCH
                                + std::time::Duration::from_secs_f64(row.get("timestamp")),
                            event_type,
                            actor: row.get("actor"),
                            target: row.get("target"),
                            allowed: row.get::<i32, _>("allowed") != 0,
                            description: row.get("description"),
                            details: details_json,
                        });
                    }
                }
                Err(e) => {
                    warn!("Failed to query audit log: {}", e);
                }
            }
        }
        entries
    }

    /// Get total count of persisted entries
    pub async fn persisted_count(&self) -> i64 {
        if let Some(ref pool) = self.pool {
            match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM audit_log")
                .fetch_one(pool)
                .await
            {
                Ok(count) => return count,
                Err(e) => {
                    warn!("Failed to count audit entries: {}", e);
                }
            }
        }
        0
    }

    /// Get all entries from memory (oldest first)
    pub async fn all(&self) -> Vec<AuditEntry> {
        let mem = self.memory.read().await;
        mem.iter().cloned().collect()
    }

    /// Filter entries by event type
    pub async fn filter(&self, event_type: AuditEventType) -> Vec<AuditEntry> {
        let mem = self.memory.read().await;
        mem.iter()
            .filter(|e| e.event_type == event_type)
            .cloned()
            .collect()
    }

    /// Current entry count in memory
    pub async fn len(&self) -> usize {
        self.memory.read().await.len()
    }

    /// Returns true if no entries are in memory.
    pub async fn is_empty(&self) -> bool {
        self.memory.read().await.is_empty()
    }

    /// Clear memory buffer
    pub async fn clear(&self) {
        self.memory.write().await.clear();
    }

    /// Export all entries as JSON
    pub async fn export_json(&self) -> Result<String, serde_json::Error> {
        let mem = self.memory.read().await;
        serde_json::to_string_pretty(&*mem)
    }
}

#[async_trait::async_trait]
impl AuditLogger for PersistentAuditLog {
    async fn log_entry(
        &self,
        event_type: AuditEventType,
        actor: String,
        target: String,
        allowed: bool,
        description: String,
        details: Option<serde_json::Value>,
    ) {
        self.log(event_type, actor, target, allowed, description, details)
            .await;
    }
}

impl Default for PersistentAuditLog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_audit_log() {
        let log = PersistentAuditLog::new();
        log.log(AuditEventType::AccessCheck, "user1", "telegram", true, "Access allowed", None)
            .await;

        let entries = log.recent(10).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].actor, "user1");
    }

    #[test]
    fn test_default_creates_in_memory_log() {
        let log: PersistentAuditLog = Default::default();
        assert!(log.pool.is_none());
    }

    #[tokio::test]
    async fn test_new_is_empty() {
        let log = PersistentAuditLog::new();
        assert_eq!(log.len().await, 0);
        assert!(log.recent(10).await.is_empty());
        assert!(log.all().await.is_empty());
    }

    #[tokio::test]
    async fn test_log_and_recent_ordering() {
        let log = PersistentAuditLog::new();
        log.log(AuditEventType::AccessCheck, "a", "t1", true, "first", None)
            .await;
        log.log(AuditEventType::ToolInvocation, "b", "t2", true, "second", None)
            .await;
        log.log(AuditEventType::Security, "c", "t3", false, "third", None)
            .await;

        let entries = log.recent(10).await;
        assert_eq!(entries.len(), 3);
        // recent() returns newest first
        assert_eq!(entries[0].actor, "c");
        assert_eq!(entries[1].actor, "b");
        assert_eq!(entries[2].actor, "a");
    }

    #[tokio::test]
    async fn test_recent_limit() {
        let log = PersistentAuditLog::new();
        for i in 0..5 {
            log.log(AuditEventType::AccessCheck, format!("u{}", i), "t", true, "msg", None)
                .await;
        }
        let entries = log.recent(2).await;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].actor, "u4");
        assert_eq!(entries[1].actor, "u3");
    }

    #[tokio::test]
    async fn test_all_returns_oldest_first() {
        let log = PersistentAuditLog::new();
        log.log(AuditEventType::AccessCheck, "a", "t", true, "first", None)
            .await;
        log.log(AuditEventType::AccessCheck, "b", "t", true, "second", None)
            .await;

        let entries = log.all().await;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].actor, "a");
        assert_eq!(entries[1].actor, "b");
    }

    #[tokio::test]
    async fn test_filter_by_event_type() {
        let log = PersistentAuditLog::new();
        log.log(AuditEventType::AccessCheck, "a", "t", true, "ac", None)
            .await;
        log.log(AuditEventType::ToolInvocation, "b", "t", true, "ti", None)
            .await;
        log.log(AuditEventType::AccessCheck, "c", "t", false, "ac2", None)
            .await;

        let filtered = log.filter(AuditEventType::AccessCheck).await;
        assert_eq!(filtered.len(), 2);
        assert!(filtered
            .iter()
            .all(|e| e.event_type == AuditEventType::AccessCheck));

        let filtered = log.filter(AuditEventType::ToolInvocation).await;
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].actor, "b");
    }

    #[tokio::test]
    async fn test_len_and_clear() {
        let log = PersistentAuditLog::new();
        assert_eq!(log.len().await, 0);

        log.log(AuditEventType::Security, "x", "t", true, "msg", None)
            .await;
        assert_eq!(log.len().await, 1);

        log.clear().await;
        assert_eq!(log.len().await, 0);
        assert!(log.recent(10).await.is_empty());
    }

    #[tokio::test]
    async fn test_export_json() {
        let log = PersistentAuditLog::new();
        log.log(
            AuditEventType::AccessCheck,
            "a",
            "t",
            true,
            "ok",
            Some(serde_json::json!({"key": "val"})),
        )
        .await;

        let json_str = log.export_json().await.unwrap();
        assert!(json_str.contains("a"));
        assert!(json_str.contains("ok"));
        // Should be valid JSON
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed.len(), 1);
    }

    #[tokio::test]
    async fn test_log_with_details() {
        let log = PersistentAuditLog::new();
        let details = Some(serde_json::json!({"ip": "1.2.3.4", "reason": "test"}));
        log.log(
            AuditEventType::ConfigChange,
            "admin",
            "system",
            true,
            "updated",
            details.clone(),
        )
        .await;

        let entries = log.recent(1).await;
        assert_eq!(entries[0].details, details);
        assert_eq!(entries[0].description, "updated");
        assert!(entries[0].allowed);
    }

    #[tokio::test]
    async fn test_query_range_no_pool_returns_empty() {
        let log = PersistentAuditLog::new();
        let start = chrono::Utc::now() - chrono::Duration::hours(1);
        let end = chrono::Utc::now();
        let results = log.query_range(start, end, 100).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_memory_capacity_eviction() {
        let mut log = PersistentAuditLog::new();
        log.memory_capacity = 3; // Small capacity for testing

        for i in 0..5 {
            log.log(AuditEventType::AccessCheck, format!("u{}", i), "t", true, "msg", None)
                .await;
        }

        assert_eq!(log.len().await, 3);
        let all = log.all().await;
        assert_eq!(all[0].actor, "u2"); // oldest remaining
        assert_eq!(all[2].actor, "u4"); // newest
    }

    #[tokio::test]
    async fn test_multiple_event_type_variants() {
        let log = PersistentAuditLog::new();
        let types = vec![
            AuditEventType::AccessCheck,
            AuditEventType::PairingRequest,
            AuditEventType::PairingApprove,
            AuditEventType::PairingReject,
            AuditEventType::PairingRevoke,
            AuditEventType::CommandGate,
            AuditEventType::ConfigChange,
            AuditEventType::ToolInvocation,
            AuditEventType::ToolDeny,
            AuditEventType::Security,
        ];
        for (i, t) in types.iter().enumerate() {
            log.log(t.clone(), format!("user{}", i), "target", i % 2 == 0, "desc", None)
                .await;
        }
        assert_eq!(log.len().await, 10);
    }
}
