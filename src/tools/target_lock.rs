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
//!
//! Android is the exception, because there the coarse key became visible: a
//! desktop with two phones attached serialised them for no reason. Its lock is
//! still one per *kind* of target in the case that matters — [the device
//! key](ANDROID) — and [`android_device`] narrows it to a serial when the
//! caller knows which device it is about to address. A call that cannot name a
//! serial takes the kind lock, which is what a call with no `-s` actually is:
//! adb resolves it to whatever is attached.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// Input to a desktop: one pointer, one focus, wherever it is driven from.
pub const DESKTOP: &str = "desktop-input";
/// Input to an Android device, over adb.
pub const ANDROID: &str = "android-input";

/// The device key for one serial: a phone, not a kind of phone.
///
/// Two phones attached to one host are separate pointers and separate focuses.
/// Keying them together made every call on one wait for a call on the other,
/// which is pure cost — and it is the kind of cost a caller fixes by not
/// taking the lock at all, which is what the lock exists to prevent.
pub fn android_device(serial: &str) -> String {
    format!("android-device:{serial}")
}

/// Process-wide locks, one per target.
#[derive(Default)]
pub struct TargetLocks {
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
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

    /// Wait for exclusive use of a named target.
    ///
    /// The key is a `String` because one of the targets names a device: two
    /// phones on one host are two places to type, not one.
    pub async fn lock_named(&self, key: &str) -> OwnedMutexGuard<()> {
        self.handle(key).lock_owned().await
    }

    /// The lock object for `key`, created on first use.
    ///
    /// Public so a caller that has to take two locks can take them in one
    /// place and in one order ([`lock_named`](Self::lock_named) on each).
    pub fn handle(&self, key: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(locks.entry(key.to_string()).or_default())
    }

    /// Wait for exclusive use of a target, holding it until the guard drops.
    pub async fn lock(&self, key: &'static str) -> OwnedMutexGuard<()> {
        // The map's guard is gone by here: only the target's own lock is held
        // across the await, and only by the caller that asked for it.
        self.lock_named(key).await
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

    /// Two phones are two targets.
    #[tokio::test]
    async fn different_serials_do_not_block_each_other() {
        let locks = TargetLocks::default();
        let _one = locks.lock_named(&android_device("emulator-5554")).await;
        let other = tokio::time::timeout(
            Duration::from_millis(200),
            locks.lock_named(&android_device("emulator-5556")),
        )
        .await;
        assert!(other.is_ok(), "a second phone waited on the first");
    }

    /// The same serial, addressed twice, is the same phone.
    #[tokio::test]
    async fn one_serial_is_one_target() {
        let locks = Arc::new(TargetLocks::default());
        let _held = locks.lock_named(&android_device("emulator-5554")).await;
        let second = tokio::time::timeout(
            Duration::from_millis(200),
            locks.lock_named(&android_device("emulator-5554")),
        )
        .await;
        assert!(second.is_err(), "the same phone was locked twice at once");
    }

    /// A call with no serial keeps the old, kind-wide behaviour: it cannot name
    /// the device adb will pick, so it must not pretend to.
    #[tokio::test]
    async fn an_unnamed_device_takes_the_kind_lock() {
        use crate::computer::platform::mobile::android::device_lock_key;
        assert_eq!(device_lock_key(&None), ANDROID);
        assert_eq!(
            device_lock_key(&Some("emulator-5554".to_string())),
            android_device("emulator-5554")
        );
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
