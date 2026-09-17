//! What the agent engine still owes the database.
//!
//! Turn, thread, sample and session-row persistence run as fire-and-forget
//! tasks: the turn is marked complete in memory first and the write follows, so
//! a shutdown that does not wait for them loses whatever was in flight — and
//! nothing notices, because the in-memory state already says "done".
//!
//! These tasks are not in the gateway's `TaskRegistry` either, and putting them
//! there would be the wrong shape: that registry exists to *stop* work (cancel,
//! then abort), while a write that has been started should be allowed to finish.
//! So the count is its own, process-wide, and shutdown waits on it before
//! closing storage.
//!
//! Registration is explicit at the spawn site — a task holds a
//! [`guard`](PendingWrites::guard) for as long as it is writing — because the
//! engine has no gateway handle and should not grow one.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

/// The process-wide count, created on first use.
static PENDING: LazyLock<Arc<PendingWrites>> = LazyLock::new(|| Arc::new(PendingWrites::new()));

/// The process-wide count of writes still in flight.
pub fn pending() -> &'static Arc<PendingWrites> {
    &PENDING
}

/// Counts durable writes that have started but not finished.
#[derive(Debug, Default)]
pub struct PendingWrites {
    in_flight: AtomicUsize,
    idle: Notify,
}

impl PendingWrites {
    /// A fresh counter. Callers normally use [`pending`].
    pub fn new() -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
            idle: Notify::new(),
        }
    }

    /// Count a write for as long as the returned guard lives.
    ///
    /// Hold it *inside* the task that does the writing: it is then released
    /// when the write finishes, and also when the task is dropped without
    /// running, so a cancelled write cannot leave the count stuck above zero.
    pub fn guard(self: &Arc<Self>) -> PendingWriteGuard {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        PendingWriteGuard(Arc::clone(self))
    }

    /// How many writes are in flight.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Wait for every in-flight write to finish, up to `timeout`.
    ///
    /// Returns `false` if the deadline passed with writes still outstanding, so
    /// the caller can report that instead of assuming it drained.
    pub async fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.in_flight() == 0 {
                return true;
            }
            // Register the notification before re-checking the count: a write
            // that finishes in between would otherwise notify nobody and leave
            // this waiting for the full timeout.
            let notified = self.idle.notified();
            if self.in_flight() == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.in_flight() == 0;
            }
        }
    }
}

/// Keeps one write counted until it is dropped.
#[derive(Debug)]
pub struct PendingWriteGuard(Arc<PendingWrites>);

impl Drop for PendingWriteGuard {
    fn drop(&mut self) {
        if self.0.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            // The last writer out wakes whoever is waiting to close storage.
            self.0.idle.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_guard_counts_until_it_is_dropped() {
        let writes = Arc::new(PendingWrites::new());
        assert_eq!(writes.in_flight(), 0);

        let guard = writes.guard();
        assert_eq!(writes.in_flight(), 1);
        drop(guard);
        assert_eq!(writes.in_flight(), 0, "dropping the write releases its count");
    }

    #[tokio::test]
    async fn wait_idle_returns_when_the_last_write_finishes() {
        let writes = Arc::new(PendingWrites::new());
        let guard = writes.guard();

        let released = writes.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(guard);
            let _ = released;
        });

        assert!(
            writes.wait_idle(Duration::from_secs(5)).await,
            "waiting should end when the write does"
        );
        assert_eq!(writes.in_flight(), 0);
    }

    /// A write that never finishes must not hold shutdown open forever.
    #[tokio::test]
    async fn wait_idle_gives_up_on_a_stuck_write() {
        let writes = Arc::new(PendingWrites::new());
        let _guard = writes.guard();

        assert!(
            !writes.wait_idle(Duration::from_millis(50)).await,
            "a write that never finishes is reported, not waited out"
        );
    }

    /// The count is process-wide, which is what lets a spawn site deep in the
    /// engine and a shutdown in the gateway agree without a handle between them.
    #[test]
    fn pending_is_shared() {
        let guard = pending().guard();
        assert!(pending().in_flight() >= 1);
        drop(guard);
    }
}
