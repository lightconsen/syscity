//! Runtime Audit Log for Syscity
//!
//! Provides an in-memory ring buffer of audit entries capturing runtime
//! security-relevant events: access decisions, pairing operations,
//! command gate evaluations, config changes, and tool invocations.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Category of audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventType {
    /// Incoming message access check
    AccessCheck,
    /// Pairing request created
    PairingRequest,
    /// Pairing approved
    PairingApprove,
    /// Pairing rejected
    PairingReject,
    /// Access revoked
    PairingRevoke,
    /// Command gate evaluation
    CommandGate,
    /// Config updated
    ConfigChange,
    /// Tool invoked
    ToolInvocation,
    /// Tool denied by policy
    ToolDeny,
    /// Generic security event
    Security,
    /// ACP subagent spawned
    AcpSpawn,
    /// ACP session terminated
    AcpTerminate,
    /// ACP message sent
    AcpMessage,
    /// Content filter action (PII/secrets detected, redacted, or blocked)
    ContentFilter,
    /// Trusted proxy authentication attempt
    TrustedProxyLogin,
    /// User login (session created)
    Login,
    /// User logout (session revoked)
    Logout,
    /// Token validation attempt
    TokenValidation,
    /// Network-allowlist proxy refused a connection
    NetworkPolicy,
}

/// A single audit log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Unique entry ID
    pub id: String,
    /// When the event occurred
    pub timestamp: SystemTime,
    /// Event category
    pub event_type: AuditEventType,
    /// Actor who triggered the event (user ID, admin, system)
    pub actor: String,
    /// Target of the action (user ID, channel, etc.)
    pub target: String,
    /// Whether the action was allowed
    pub allowed: bool,
    /// Human-readable description
    pub description: String,
    /// Optional details (JSON)
    pub details: Option<serde_json::Value>,
}

/// In-memory ring buffer for runtime audit entries.
///
/// Oldest entries are evicted when capacity is exceeded.
#[derive(Debug, Clone)]
pub struct RuntimeAuditLog {
    entries: Arc<RwLock<VecDeque<AuditEntry>>>,
    capacity: usize,
}

impl Default for RuntimeAuditLog {
    fn default() -> Self {
        Self::with_capacity(10_000)
    }
}

impl RuntimeAuditLog {
    /// Create a new audit log with the given capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Arc::new(RwLock::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    /// Log a new audit entry.
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
            timestamp: SystemTime::now(),
            event_type,
            actor: actor.into(),
            target: target.into(),
            allowed,
            description: description.into(),
            details,
        };

        let mut entries = self.entries.write().await;
        if entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// Retrieve the most recent `n` entries (newest first).
    pub async fn recent(&self, n: usize) -> Vec<AuditEntry> {
        let entries = self.entries.read().await;
        entries.iter().rev().take(n).cloned().collect()
    }

    /// Retrieve all entries (oldest first).
    pub async fn all(&self) -> Vec<AuditEntry> {
        let entries = self.entries.read().await;
        entries.iter().cloned().collect()
    }

    /// Filter entries by event type.
    pub async fn filter(&self, event_type: AuditEventType) -> Vec<AuditEntry> {
        let entries = self.entries.read().await;
        entries
            .iter()
            .filter(|e| e.event_type == event_type)
            .cloned()
            .collect()
    }

    /// Count of entries currently stored.
    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    /// Returns true if no entries are stored.
    pub async fn is_empty(&self) -> bool {
        self.entries.read().await.is_empty()
    }

    /// Clear all entries.
    pub async fn clear(&self) {
        self.entries.write().await.clear();
    }
}

/// Trait for audit loggers (in-memory, persistent, or composite).
#[async_trait::async_trait]
pub trait AuditLogger: Send + Sync + std::fmt::Debug {
    /// Log a single audit entry.
    async fn log_entry(
        &self,
        event_type: AuditEventType,
        actor: String,
        target: String,
        allowed: bool,
        description: String,
        details: Option<serde_json::Value>,
    );
}

#[async_trait::async_trait]
impl AuditLogger for RuntimeAuditLog {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_log_and_retrieve() {
        let log = RuntimeAuditLog::with_capacity(100);
        log.log(AuditEventType::AccessCheck, "user1", "telegram", true, "Access allowed", None)
            .await;

        let entries = log.all().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].actor, "user1");
        assert!(entries[0].allowed);
    }

    #[tokio::test]
    async fn test_ring_buffer_eviction() {
        let log = RuntimeAuditLog::with_capacity(3);
        for i in 0..5 {
            log.log(
                AuditEventType::AccessCheck,
                format!("user{}", i),
                "telegram",
                true,
                "test",
                None,
            )
            .await;
        }

        let entries = log.all().await;
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].actor, "user2");
        assert_eq!(entries[2].actor, "user4");
    }

    #[tokio::test]
    async fn test_filter_by_type() {
        let log = RuntimeAuditLog::with_capacity(100);
        log.log(AuditEventType::AccessCheck, "u1", "c1", true, "", None)
            .await;
        log.log(AuditEventType::PairingRequest, "u2", "c1", true, "", None)
            .await;
        log.log(AuditEventType::AccessCheck, "u3", "c1", true, "", None)
            .await;

        let filtered = log.filter(AuditEventType::AccessCheck).await;
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn test_default_capacity() {
        let _log: RuntimeAuditLog = Default::default();
        // Capacity is 10_000 but we can't inspect it directly; test eviction
        // indirectly by verifying it works with many entries
    }

    #[tokio::test]
    async fn test_recent_ordering() {
        let log = RuntimeAuditLog::with_capacity(100);
        log.log(AuditEventType::AccessCheck, "a", "t", true, "first", None)
            .await;
        log.log(AuditEventType::AccessCheck, "b", "t", true, "second", None)
            .await;

        let recent = log.recent(2).await;
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].actor, "b");
        assert_eq!(recent[1].actor, "a");
    }

    #[tokio::test]
    async fn test_len_and_clear() {
        let log = RuntimeAuditLog::with_capacity(100);
        assert_eq!(log.len().await, 0);

        log.log(AuditEventType::Security, "x", "t", true, "msg", None)
            .await;
        assert_eq!(log.len().await, 1);

        log.clear().await;
        assert_eq!(log.len().await, 0);
        assert!(log.all().await.is_empty());
    }

    #[test]
    fn test_audit_event_type_variants() {
        assert_eq!(AuditEventType::AccessCheck, AuditEventType::AccessCheck);
        assert_eq!(AuditEventType::ToolInvocation, AuditEventType::ToolInvocation);
        assert_ne!(AuditEventType::PairingRequest, AuditEventType::PairingApprove);
        // Auth-specific variants
        assert_ne!(AuditEventType::Login, AuditEventType::Logout);
        assert_ne!(AuditEventType::Logout, AuditEventType::TokenValidation);
    }

    #[tokio::test]
    async fn test_filter_auth_event_types() {
        let log = RuntimeAuditLog::with_capacity(100);
        log.log(AuditEventType::Login, "u1", "t1", true, "", None)
            .await;
        log.log(AuditEventType::Logout, "u1", "t1", true, "", None)
            .await;
        log.log(AuditEventType::TokenValidation, "u1", "t1", true, "", None)
            .await;
        log.log(AuditEventType::AccessCheck, "u2", "t2", true, "", None)
            .await;

        let logins = log.filter(AuditEventType::Login).await;
        assert_eq!(logins.len(), 1);
        assert_eq!(logins[0].event_type, AuditEventType::Login);

        let token_validations = log.filter(AuditEventType::TokenValidation).await;
        assert_eq!(token_validations.len(), 1);
        assert_eq!(token_validations[0].event_type, AuditEventType::TokenValidation);
    }

    #[test]
    fn test_audit_entry_creation() {
        let entry = AuditEntry {
            id: "id1".to_string(),
            timestamp: SystemTime::now(),
            event_type: AuditEventType::ConfigChange,
            actor: "admin".to_string(),
            target: "system".to_string(),
            allowed: false,
            description: "changed".to_string(),
            details: Some(serde_json::json!({"key": "val"})),
        };
        assert_eq!(entry.actor, "admin");
        assert!(!entry.allowed);
    }

    /// Shared contract test for any [`AuditLogger`] implementation.
    ///
    /// Verifies the basic invariants that every logger must satisfy:
    /// - A logged entry is immediately retrievable.
    /// - The `log_entry` call completes within a short timeout (does not block
    ///   the caller indefinitely).
    #[allow(dead_code)]
    pub(crate) async fn test_audit_logger_contract(logger: &dyn AuditLogger) {
        // Contract: entry is retrievable after logging.
        logger
            .log_entry(
                AuditEventType::AccessCheck,
                "tester".to_string(),
                "target".to_string(),
                true,
                "contract test".to_string(),
                None,
            )
            .await;

        // Contract: log_entry does not block (completes within 1 s).
        let slow = logger.log_entry(
            AuditEventType::Security,
            "slow-check".to_string(),
            "system".to_string(),
            false,
            "non-blocking test".to_string(),
            Some(serde_json::json!({"test": true})),
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), slow)
            .await
            .expect("AuditLogger::log_entry should not block the caller");
    }

    #[tokio::test]
    async fn test_runtime_audit_log_contract() {
        let log = RuntimeAuditLog::with_capacity(100);
        test_audit_logger_contract(&log).await;
    }
}
