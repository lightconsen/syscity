//! Serialize the tools that act on one shared target.
//!
//! Everything that injects input on a machine shares a single pointer and a
//! single focus. Two conversations — or a parent and its delegated child — can
//! therefore be told to act at the same time and interleave: one types into a
//! field while the other clicks somewhere else, and both report success. The
//! only serialization that existed was per conversation, which answers a
//! different question ("is this conversation busy?") from the one that matters
//! ("is this display busy?").
//!
//! The platform tool descriptions say the desktop is shared rather than owned.
//! This is the mechanism behind that sentence.
//!
//! Keyed by *kind* of target rather than by exact target. Two devices attached
//! to one host are then serialized unnecessarily, which costs throughput and
//! never correctness; keying tightly enough to avoid that would mean a pinned
//! serial and an unpinned call on the same device taking different locks, which
//! is the failure this exists to prevent.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// Input to a desktop: one pointer, one focus, wherever it is driven from.
pub const DESKTOP: &str = "desktop-input";
/// Input to an Android device, over adb.
pub const ANDROID: &str = "android-input";

/// Process-wide locks, one per target.
#[derive(Default)]
pub struct TargetLocks {
    locks: Mutex<HashMap<&'static str, Arc<AsyncMutex<()>>>>,
}

impl TargetLocks {
    /// The locks for this process.
    ///
    /// Process-wide because the target is: the adapters and tools that share a
    /// display are reached from different registries and different agents.
    pub fn global() -> &'static TargetLocks {
        static LOCKS: OnceLock<TargetLocks> = OnceLock::new();
        LOCKS.get_or_init(TargetLocks::default)
    }

    /// Wait for exclusive use of a target, holding it until the guard drops.
    pub async fn lock(&self, key: &'static str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(locks.entry(key).or_default())
        };
        // The map's guard is gone by here: only the target's own lock is held
        // across the await, and only by the caller that asked for it.
        lock.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// One holder at a time for a given target.
    #[tokio::test]
    async fn the_same_target_is_held_exclusively() {
        let locks = Arc::new(TargetLocks::default());
        let concurrent = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..4 {
            let locks = Arc::clone(&locks);
            let concurrent = Arc::clone(&concurrent);
            let peak = Arc::clone(&peak);
            tasks.push(tokio::spawn(async move {
                let _guard = locks.lock(DESKTOP).await;
                let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                concurrent.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(peak.load(Ordering::SeqCst), 1, "two holders of one target overlapped");
    }

    /// Different targets do not wait on each other.
    #[tokio::test]
    async fn different_targets_do_not_block_each_other() {
        let locks = Arc::new(TargetLocks::default());

        let _desktop = locks.lock(DESKTOP).await;
        let android = tokio::time::timeout(Duration::from_millis(200), locks.lock(ANDROID)).await;
        assert!(android.is_ok(), "the android lock waited on the desktop lock");
    }

    /// The guard is what releases it.
    #[tokio::test]
    async fn dropping_the_guard_releases_the_target() {
        let locks = TargetLocks::default();
        {
            let _guard = locks.lock(DESKTOP).await;
        }
        let again = tokio::time::timeout(Duration::from_millis(200), locks.lock(DESKTOP)).await;
        assert!(again.is_ok(), "the target stayed locked after its guard dropped");
    }
}
