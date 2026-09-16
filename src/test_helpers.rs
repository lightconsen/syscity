//! Shared test helpers for negative testing.
//!
//! These utilities are intentionally **not** behind `#[cfg(test)]` so
//! integration tests (inside `tests/`) can import them. They should
//! only be referenced from test code.
//!
//! # Tools
//!
//! - `WatchableAuditLog`: wraps a `RuntimeAuditLog` and signals via
//!   `broadcast::Sender` on each `log_entry` call. Tests can `tokio::select!`
//!   on the receiver instead of polling.
//! - `CollectingAuditLog`: records every `log_entry` call into a
//!   `Vec<AuditEntry>` accessible via `Arc<Mutex<...>>`.
//! - `assert_eventually`: poll a predicate with backoff until it returns `true`
//!   or a timeout elapses.

// INVARIANTS-NONE: shipped test utilities; no runtime role
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::sync::Mutex;

use crate::security::runtime_audit::{AuditEntry, AuditEventType, AuditLogger, RuntimeAuditLog};

/// Poll `f()` with geometric backoff until it returns `true` or
/// `timeout` elapses.
///
/// This is useful for asserting on outcomes that happen asynchronously
/// (e.g. a broadcast event being received, a log line being written).
pub async fn assert_eventually<F, Fut>(f: F, timeout: Duration, msg: &str)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    let mut delay = Duration::from_millis(5);
    while start.elapsed() < timeout {
        if f().await {
            return;
        }
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, Duration::from_millis(100));
    }
    // One last try before failing.
    if !f().await {
        panic!("assert_eventually failed after {:.1}s: {}", start.elapsed().as_secs_f64(), msg);
    }
}

/// A wrapper around [`RuntimeAuditLog`] that notifies a
/// `broadcast::Sender` on every `log_entry` call, so tests can wait
/// for entries without polling.
#[derive(Debug, Clone)]
pub struct WatchableAuditLog {
    inner: RuntimeAuditLog,
    tx: broadcast::Sender<()>,
}

impl WatchableAuditLog {
    /// Create a new `WatchableAuditLog` with the given capacity.
    ///
    /// Returns the log and a receiver that gets a notification on every
    /// `log_entry` call.
    pub fn new(capacity: usize) -> (Self, broadcast::Receiver<()>) {
        let (tx, rx) = broadcast::channel(256);
        (
            Self {
                inner: RuntimeAuditLog::with_capacity(capacity),
                tx,
            },
            rx,
        )
    }

    /// Delegate to the inner [`RuntimeAuditLog::log`].
    pub async fn log(
        &self,
        event_type: AuditEventType,
        actor: impl Into<String>,
        target: impl Into<String>,
        allowed: bool,
        description: impl Into<String>,
        details: Option<serde_json::Value>,
    ) {
        self.inner
            .log(event_type, actor, target, allowed, description, details)
            .await;
        let _ = self.tx.send(());
    }

    /// Delegate to [`RuntimeAuditLog::filter`].
    pub async fn filter(&self, event_type: AuditEventType) -> Vec<AuditEntry> {
        self.inner.filter(event_type).await
    }

    /// Delegate to [`RuntimeAuditLog::recent`].
    pub async fn recent(&self, n: usize) -> Vec<AuditEntry> {
        self.inner.recent(n).await
    }

    /// Delegate to [`RuntimeAuditLog::all`].
    pub async fn all(&self) -> Vec<AuditEntry> {
        self.inner.all().await
    }

    /// Delegate to [`RuntimeAuditLog::len`].
    pub async fn len(&self) -> usize {
        self.inner.len().await
    }

    /// Delegate to [`RuntimeAuditLog::is_empty`].
    pub async fn is_empty(&self) -> bool {
        self.inner.is_empty().await
    }

    /// Delegate to [`RuntimeAuditLog::clear`].
    pub async fn clear(&self) {
        self.inner.clear().await;
    }
}

#[async_trait::async_trait]
impl AuditLogger for WatchableAuditLog {
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

/// A simple audit log that records every entry into an
/// `Arc<Mutex<Vec<AuditEntry>>>` so tests can inspect the exact call
/// sequence.
#[derive(Debug, Clone)]
pub struct CollectingAuditLog {
    entries: Arc<Mutex<Vec<AuditEntry>>>,
}

impl Default for CollectingAuditLog {
    fn default() -> Self {
        Self::new()
    }
}

impl CollectingAuditLog {
    /// Create a new, empty collecting audit log.
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Return all entries collected so far.
    pub async fn entries(&self) -> Vec<AuditEntry> {
        self.entries.lock().await.clone()
    }

    /// Return entries matching the given predicate.
    pub async fn filtered<F>(&self, predicate: F) -> Vec<AuditEntry>
    where
        F: Fn(&AuditEntry) -> bool,
    {
        self.entries
            .lock()
            .await
            .iter()
            .filter(|e| predicate(e))
            .cloned()
            .collect()
    }

    /// Number of entries collected.
    pub async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }

    /// Returns true if no entries have been collected.
    pub async fn is_empty(&self) -> bool {
        self.entries.lock().await.is_empty()
    }

    /// Clear all collected entries.
    pub async fn clear(&self) {
        self.entries.lock().await.clear();
    }
}

#[async_trait::async_trait]
impl AuditLogger for CollectingAuditLog {
    async fn log_entry(
        &self,
        event_type: AuditEventType,
        actor: String,
        target: String,
        allowed: bool,
        description: String,
        details: Option<serde_json::Value>,
    ) {
        let entry = AuditEntry {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: std::time::SystemTime::now(),
            event_type,
            actor,
            target,
            allowed,
            description,
            details,
        };
        self.entries.lock().await.push(entry);
    }
}
