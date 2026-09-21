//! Human-in-the-loop approval system for high-risk tool calls
//!
//! Provides PendingApproval queue for tools that require explicit human
//! confirmation before execution. Integrates with ToolPolicyDecision to
//! suspend tool execution until approved or denied.
//!
//! ## Flow
//!
//! 1. Policy hook returns `ToolPolicyDecision::NeedsApproval`
//! 2. ToolRegistry creates PendingApproval with oneshot channel
//! 3. ApprovalQueue stores it and broadcasts ApprovalRequired event
//! 4. Human reviews via REST API and submits approve/deny
//! 5. ApprovalQueue resolves the oneshot, ToolRegistry resumes execution

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot, RwLock};
use tracing::{debug, info, warn};

/// Risk level for tool calls requiring approval
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Low risk - informational, read-only
    Low = 0,
    /// Medium risk - modifies state but recoverable
    Medium = 1,
    /// High risk - destructive or security-sensitive
    High = 2,
    /// Critical risk - irreversible or system-level
    Critical = 3,
}

impl RiskLevel {
    /// Human-readable description
    pub fn description(&self) -> &'static str {
        match self {
            RiskLevel::Low => "Low risk",
            RiskLevel::Medium => "Medium risk",
            RiskLevel::High => "High risk",
            RiskLevel::Critical => "Critical risk",
        }
    }
}

/// Decision from human reviewer
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Allow the tool call to proceed
    Approve,
    /// Deny the tool call
    Deny { reason: String },
}

/// A pending tool approval request
#[derive(Debug)]
pub struct PendingApproval {
    /// Unique approval ID
    pub id: String,
    /// Tool name being requested
    pub tool_name: String,
    /// Tool arguments
    pub args: serde_json::Value,
    /// When the approval was requested
    pub requested_at: Instant,
    /// User or agent that requested the tool
    pub requested_by: String,
    /// Risk level assessment
    pub risk_level: RiskLevel,
    /// Human-readable message explaining the request
    pub message: String,
    /// The conversation the tool call belongs to, when known — the gateway
    /// routes the announcement to that session's subscribers. `None` (or an
    /// empty conversation id, e.g. a context with no conversation) broadcasts.
    pub session_id: Option<String>,
    /// Channel to send resolution back to suspended execution
    pub(crate) response_tx: Option<oneshot::Sender<ApprovalDecision>>,
}

impl PendingApproval {
    /// Create a new pending approval with defaults; chain the `with_*`
    /// builders to set risk level, message, and the response channel.
    pub fn new(
        id: impl Into<String>,
        tool_name: impl Into<String>,
        args: serde_json::Value,
        requested_by: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            tool_name: tool_name.into(),
            args,
            requested_at: Instant::now(),
            requested_by: requested_by.into(),
            risk_level: RiskLevel::Medium,
            message: String::new(),
            session_id: None,
            response_tx: None,
        }
    }

    /// Set the risk level assessment.
    pub fn with_risk_level(mut self, risk_level: RiskLevel) -> Self {
        self.risk_level = risk_level;
        self
    }

    /// Set the human-readable message explaining the request.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    /// Set the channel the suspended execution waits on for the decision.
    pub fn with_response_tx(mut self, response_tx: oneshot::Sender<ApprovalDecision>) -> Self {
        self.response_tx = Some(response_tx);
        self
    }

    /// Set the owning conversation; an empty id means "no conversation" and
    /// is stored as `None`.
    pub fn with_session(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id.filter(|s| !s.is_empty());
        self
    }

    /// Age of this approval request
    pub fn age(&self) -> Duration {
        self.requested_at.elapsed()
    }
}

/// Summary of a pending approval (for API responses)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApprovalSummary {
    pub id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub requested_at: chrono::DateTime<chrono::Utc>,
    pub requested_by: String,
    pub risk_level: RiskLevel,
    pub message: String,
    /// The conversation that raised it, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub age_seconds: u64,
}

impl From<&PendingApproval> for PendingApprovalSummary {
    fn from(pa: &PendingApproval) -> Self {
        Self {
            id: pa.id.clone(),
            tool_name: pa.tool_name.clone(),
            args: pa.args.clone(),
            requested_at: chrono::DateTime::UNIX_EPOCH
                + chrono::Duration::from_std(pa.requested_at.elapsed()).unwrap_or_default(),
            requested_by: pa.requested_by.clone(),
            risk_level: pa.risk_level,
            message: pa.message.clone(),
            session_id: pa.session_id.clone(),
            age_seconds: pa.age().as_secs(),
        }
    }
}

/// Event broadcast when new approval is required
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequiredEvent {
    pub approval_id: String,
    pub tool_name: String,
    pub requested_by: String,
    pub risk_level: RiskLevel,
    pub message: String,
    /// The conversation that raised it; `None` reaches every client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Filter for listing pending approvals
#[derive(Debug, Clone, Default)]
pub struct ApprovalFilter {
    pub min_risk_level: Option<RiskLevel>,
    pub tool_name: Option<String>,
    pub requested_by: Option<String>,
    pub max_age: Option<Duration>,
}

/// Thread-safe approval queue with broadcast notifications
#[derive(Debug, Clone)]
pub struct ApprovalQueue {
    pending: Arc<RwLock<HashMap<String, PendingApproval>>>,
    /// Broadcast channel for new approval events
    pub event_tx: broadcast::Sender<ApprovalRequiredEvent>,
    /// Default timeout for approvals
    pub default_timeout: Duration,
}

impl ApprovalQueue {
    /// Create a new approval queue
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(256);
        Self {
            pending: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            default_timeout: Duration::from_secs(300), // 5 minutes
        }
    }

    /// Submit a new approval request
    ///
    /// Returns the approval ID. The caller should await the oneshot receiver
    /// to get the resolution.
    pub async fn submit(&self, approval: PendingApproval) -> String {
        let id = approval.id.clone();
        let event = ApprovalRequiredEvent {
            approval_id: id.clone(),
            tool_name: approval.tool_name.clone(),
            requested_by: approval.requested_by.clone(),
            risk_level: approval.risk_level,
            message: approval.message.clone(),
            session_id: approval.session_id.clone(),
        };

        {
            let mut pending = self.pending.write().await;
            pending.insert(id.clone(), approval);
        }

        info!(
            "Approval {} submitted for tool '{}' (risk: {:?})",
            id, event.tool_name, event.risk_level
        );

        // Broadcast event to subscribers (web UI, notifications, etc.)
        if self.event_tx.send(event).is_err() {
            warn!("No active subscribers for approval event");
        }

        id
    }

    /// Resolve an approval with a decision
    ///
    /// Returns true if the approval was found and resolved, false if not found
    /// or already resolved.
    pub async fn resolve(&self, approval_id: &str, decision: ApprovalDecision) -> bool {
        let approval = {
            let mut pending = self.pending.write().await;
            pending.remove(approval_id)
        };

        match approval {
            Some(mut pa) => {
                if let Some(tx) = pa.response_tx.take() {
                    let _ = tx.send(decision.clone());
                    match decision {
                        ApprovalDecision::Approve => {
                            info!("Approval {} approved", approval_id);
                        }
                        ApprovalDecision::Deny { reason } => {
                            info!("Approval {} denied: {}", approval_id, reason);
                        }
                    }
                    true
                } else {
                    warn!("Approval {} already resolved", approval_id);
                    false
                }
            }
            None => {
                warn!("Approval {} not found", approval_id);
                false
            }
        }
    }

    /// Get a specific pending approval
    pub async fn get(&self, id: &str) -> Option<PendingApprovalSummary> {
        let pending = self.pending.read().await;
        pending.get(id).map(|pa| pa.into())
    }

    /// List pending approvals with optional filter
    pub async fn list_pending(&self, filter: ApprovalFilter) -> Vec<PendingApprovalSummary> {
        let pending = self.pending.read().await;

        pending
            .values()
            .filter(|pa| {
                if let Some(min_risk) = filter.min_risk_level {
                    if pa.risk_level < min_risk {
                        return false;
                    }
                }
                if let Some(ref tool) = filter.tool_name {
                    if pa.tool_name != *tool {
                        return false;
                    }
                }
                if let Some(ref user) = filter.requested_by {
                    if pa.requested_by != *user {
                        return false;
                    }
                }
                if let Some(max_age) = filter.max_age {
                    if pa.age() > max_age {
                        return false;
                    }
                }
                true
            })
            .map(|pa| pa.into())
            .collect()
    }

    /// Get all pending approval IDs
    pub async fn list_ids(&self) -> Vec<String> {
        let pending = self.pending.read().await;
        pending.keys().cloned().collect()
    }

    /// Cancel (deny) all pending approvals for a given user/session
    pub async fn cancel_for(&self, requested_by: &str) -> usize {
        let ids: Vec<String> = {
            let pending = self.pending.read().await;
            pending
                .values()
                .filter(|pa| pa.requested_by == requested_by)
                .map(|pa| pa.id.clone())
                .collect()
        };

        let mut count = 0;
        for id in ids {
            if self
                .resolve(
                    &id,
                    ApprovalDecision::Deny {
                        reason: "Cancelled: session ended".into(),
                    },
                )
                .await
            {
                count += 1;
            }
        }
        count
    }

    /// Clean up stale approvals that have exceeded timeout
    pub async fn cleanup_stale(&self) -> usize {
        let timeout = self.default_timeout;
        let stale_ids: Vec<String> = {
            let pending = self.pending.read().await;
            pending
                .values()
                .filter(|pa| pa.age() > timeout)
                .map(|pa| pa.id.clone())
                .collect()
        };

        let mut count = 0;
        for id in stale_ids {
            if self
                .resolve(
                    &id,
                    ApprovalDecision::Deny {
                        reason: "Approval timed out".into(),
                    },
                )
                .await
            {
                count += 1;
                debug!("Cleaned up stale approval {}", id);
            }
        }
        count
    }

    /// Number of pending approvals
    pub async fn len(&self) -> usize {
        self.pending.read().await.len()
    }

    /// Check if queue is empty
    pub async fn is_empty(&self) -> bool {
        self.pending.read().await.is_empty()
    }
}

impl Default for ApprovalQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the gateway's approval sweeper relies on: an approval that has
    /// outlived the queue's own timeout leaves `pending`, even when the waiter
    /// it was raised for is gone. Nothing called this before, so such an entry
    /// stayed pending forever — visible to the UI and resolvable by nobody.
    #[tokio::test]
    async fn cleanup_stale_removes_expired_approvals() {
        let queue = ApprovalQueue::new();

        let (tx, rx) = oneshot::channel();
        let mut stale = PendingApproval::new("stale", "shell", serde_json::json!({}), "user");
        stale.requested_at = Instant::now() - Duration::from_secs(3600);
        queue.submit(stale.with_response_tx(tx)).await;
        drop(rx); // the waiter went away, which is the case that used to leak

        queue
            .submit(PendingApproval::new("fresh", "shell", serde_json::json!({}), "user"))
            .await;

        assert_eq!(queue.cleanup_stale().await, 1, "only the expired approval is denied");
        assert!(queue.get("stale").await.is_none(), "and it is gone");
        assert!(queue.get("fresh").await.is_some(), "a fresh one is untouched");
    }

    #[tokio::test]
    async fn test_approval_queue_submit_and_resolve() {
        let queue = ApprovalQueue::new();
        let (tx, rx) = oneshot::channel();

        let approval = PendingApproval::new(
            "test-1",
            "shell",
            serde_json::json!({"command": "ls"}),
            "user123",
        )
        .with_risk_level(RiskLevel::High)
        .with_message("Shell command requires approval")
        .with_response_tx(tx);

        let id = queue.submit(approval).await;
        assert_eq!(id, "test-1");
        assert_eq!(queue.len().await, 1);

        // Resolve the approval
        let resolved = queue.resolve(&id, ApprovalDecision::Approve).await;
        assert!(resolved);
        assert!(queue.is_empty().await);

        // Check the receiver got the decision
        let decision = rx.await.unwrap();
        assert_eq!(decision, ApprovalDecision::Approve);
    }

    #[tokio::test]
    async fn test_approval_queue_deny() {
        let queue = ApprovalQueue::new();
        let (tx, rx) = oneshot::channel();

        let approval = PendingApproval::new(
            "test-2",
            "file_delete",
            serde_json::json!({"path": "/tmp/test"}),
            "user456",
        )
        .with_risk_level(RiskLevel::Critical)
        .with_message("File deletion requires approval")
        .with_response_tx(tx);

        let id = queue.submit(approval).await;

        let resolved = queue
            .resolve(&id, ApprovalDecision::Deny { reason: "Not allowed".into() })
            .await;
        assert!(resolved);

        let decision = rx.await.unwrap();
        assert!(matches!(decision, ApprovalDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn test_approval_queue_not_found() {
        let queue = ApprovalQueue::new();

        let resolved = queue
            .resolve("nonexistent", ApprovalDecision::Approve)
            .await;
        assert!(!resolved);
    }

    #[tokio::test]
    async fn test_approval_queue_list_filter() {
        let queue = ApprovalQueue::new();

        // Add multiple approvals
        for i in 0..3 {
            let (tx, _rx) = oneshot::channel();
            let approval = PendingApproval::new(
                format!("test-{}", i),
                if i == 0 { "shell" } else { "memory_read" },
                serde_json::json!({}),
                if i == 0 { "user1" } else { "user2" },
            )
            .with_risk_level(if i == 2 {
                RiskLevel::Critical
            } else {
                RiskLevel::Medium
            })
            .with_message("Test")
            .with_response_tx(tx);
            queue.submit(approval).await;
        }

        assert_eq!(queue.len().await, 3);

        // Filter by tool name
        let filter = ApprovalFilter {
            tool_name: Some("shell".into()),
            ..Default::default()
        };
        let results = queue.list_pending(filter).await;
        assert_eq!(results.len(), 1);

        // Filter by risk level
        let filter = ApprovalFilter {
            min_risk_level: Some(RiskLevel::Critical),
            ..Default::default()
        };
        let results = queue.list_pending(filter).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].risk_level, RiskLevel::Critical);
    }

    #[tokio::test]
    async fn test_approval_queue_cancel_for() {
        let queue = ApprovalQueue::new();

        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();

        queue
            .submit(
                PendingApproval::new("a1", "tool", serde_json::json!({}), "user1")
                    .with_risk_level(RiskLevel::Low)
                    .with_message("Test")
                    .with_response_tx(tx1),
            )
            .await;

        queue
            .submit(
                PendingApproval::new("a2", "tool", serde_json::json!({}), "user2")
                    .with_risk_level(RiskLevel::Low)
                    .with_message("Test")
                    .with_response_tx(tx2),
            )
            .await;

        let cancelled = queue.cancel_for("user1").await;
        assert_eq!(cancelled, 1);
        assert_eq!(queue.len().await, 1);
    }

    #[test]
    fn test_risk_level_description() {
        assert_eq!(RiskLevel::Low.description(), "Low risk");
        assert_eq!(RiskLevel::Medium.description(), "Medium risk");
        assert_eq!(RiskLevel::High.description(), "High risk");
        assert_eq!(RiskLevel::Critical.description(), "Critical risk");
    }

    #[test]
    fn test_risk_level_ordering() {
        assert!(RiskLevel::Low < RiskLevel::Medium);
        assert!(RiskLevel::Medium < RiskLevel::High);
        assert!(RiskLevel::High < RiskLevel::Critical);
    }

    #[test]
    fn test_risk_level_serialization() {
        let json = serde_json::to_string(&RiskLevel::High).unwrap();
        assert!(json.contains("high"));
    }

    #[test]
    fn test_approval_decision_equality() {
        assert_eq!(ApprovalDecision::Approve, ApprovalDecision::Approve);
        assert_ne!(ApprovalDecision::Approve, ApprovalDecision::Deny { reason: "no".to_string() });
    }

    #[tokio::test]
    async fn test_approval_queue_get() {
        let queue = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();

        queue
            .submit(
                PendingApproval::new("g1", "tool", serde_json::json!({}), "user")
                    .with_risk_level(RiskLevel::Low)
                    .with_message("msg")
                    .with_response_tx(tx),
            )
            .await;

        let summary = queue.get("g1").await;
        assert!(summary.is_some());
        assert_eq!(summary.unwrap().tool_name, "tool");

        let missing = queue.get("missing").await;
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_approval_queue_list_ids() {
        let queue = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();

        queue
            .submit(
                PendingApproval::new("id1", "t1", serde_json::json!({}), "u")
                    .with_risk_level(RiskLevel::Low)
                    .with_message("m")
                    .with_response_tx(tx),
            )
            .await;

        let ids = queue.list_ids().await;
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], "id1");
    }

    #[tokio::test]
    async fn test_approval_queue_is_empty() {
        let queue = ApprovalQueue::new();
        assert!(queue.is_empty().await);

        let (tx, _rx) = oneshot::channel();
        queue
            .submit(
                PendingApproval::new("e1", "t", serde_json::json!({}), "u")
                    .with_risk_level(RiskLevel::Low)
                    .with_message("m")
                    .with_response_tx(tx),
            )
            .await;

        assert!(!queue.is_empty().await);
    }

    #[tokio::test]
    async fn test_approval_queue_default() {
        let queue: ApprovalQueue = Default::default();
        assert!(queue.is_empty().await);
        assert_eq!(queue.default_timeout, Duration::from_secs(300));
    }

    #[test]
    fn test_approval_filter_default() {
        let filter = ApprovalFilter::default();
        assert!(filter.min_risk_level.is_none());
        assert!(filter.tool_name.is_none());
        assert!(filter.requested_by.is_none());
        assert!(filter.max_age.is_none());
    }

    #[test]
    fn test_pending_approval_age() {
        let (tx, _rx) = oneshot::channel();
        let pa = PendingApproval::new("a1", "tool", serde_json::json!({}), "user")
            .with_risk_level(RiskLevel::Low)
            .with_message("test")
            .with_response_tx(tx);
        // Age should be very small since just created
        assert!(pa.age() < Duration::from_secs(1));
    }

    #[test]
    fn test_approval_required_event_serialization() {
        let event = ApprovalRequiredEvent {
            approval_id: "aid".to_string(),
            tool_name: "shell".to_string(),
            requested_by: "user".to_string(),
            risk_level: RiskLevel::High,
            message: "Approve?".to_string(),
            session_id: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("aid"));
        assert!(json.contains("shell"));
    }

    #[tokio::test]
    async fn test_approval_queue_double_resolve() {
        let queue = ApprovalQueue::new();
        let (tx, rx) = oneshot::channel();

        queue
            .submit(
                PendingApproval::new("d1", "tool", serde_json::json!({}), "user")
                    .with_risk_level(RiskLevel::Low)
                    .with_message("test")
                    .with_response_tx(tx),
            )
            .await;

        let first = queue.resolve("d1", ApprovalDecision::Approve).await;
        assert!(first);

        let second = queue.resolve("d1", ApprovalDecision::Approve).await;
        assert!(!second);

        let decision = rx.await.unwrap();
        assert_eq!(decision, ApprovalDecision::Approve);
    }

    /// The session an approval was raised in rides along on the announcement:
    /// it is what lets the gateway route the event to that conversation's
    /// subscribers instead of every connected client.
    #[tokio::test]
    async fn the_announcement_carries_the_session() {
        let queue = ApprovalQueue::new();
        let mut events = queue.event_tx.subscribe();

        queue
            .submit(
                PendingApproval::new("s1a", "tool", serde_json::json!({}), "user")
                    .with_session(Some("sess-9".to_string())),
            )
            .await;
        let event = events.recv().await.unwrap();
        assert_eq!(event.session_id.as_deref(), Some("sess-9"));

        // No conversation (or an empty id) means the announcement is global.
        queue
            .submit(
                PendingApproval::new("s1b", "tool", serde_json::json!({}), "user")
                    .with_session(Some(String::new())),
            )
            .await;
        let event = events.recv().await.unwrap();
        assert_eq!(event.session_id, None, "an empty conversation id is no conversation");

        // And it is visible on the summary `approvals.get` returns.
        let summary = queue.get("s1a").await.unwrap();
        assert_eq!(summary.session_id.as_deref(), Some("sess-9"));
    }
}
