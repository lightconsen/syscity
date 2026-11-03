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
use std::sync::atomic::{AtomicU64, Ordering};
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

/// One virtual display of one phone.
///
/// A display has its own focus, so two displays on one phone are two places to
/// type — the per-device lock made them wait for each other for no reason. The
/// user's own screen (display 0) is *not* keyed this way: it has one focus and
/// one visible owner, and [`android_device`] is what protects it.
pub fn android_display(serial: &str, display: i32) -> String {
    format!("android-display:{serial}:{display}")
}

/// One app on one phone, whatever display it is on.
///
/// Taking this *as well as* a display lock is what stops two tasks driving the
/// same app on two different displays: the displays do not conflict, but the
/// app does — one process, one account, one set of unsaved edits. It is always
/// taken after the display lock and never on its own, so the two can never
/// deadlock.
pub fn android_app(serial: &str, package: &str) -> String {
    format!("android-app:{serial}:{package}")
}

/// One app's lock, and how many writes have finished on it.
#[derive(Default)]
struct AppGate {
    /// Held as an `Arc` so a caller's owned guard can be taken from a clone of
    /// it, without borrowing the gate the map owns.
    lock: Arc<AsyncMutex<()>>,
    completed: Arc<AtomicU64>,
}

/// Exclusive use of one app, plus whether it arrived too late to be useful.
///
/// A caller finds out which app it is about to write to by reading the screen,
/// and only then takes that app's lock — so the read is necessarily older than
/// the lock. When another write to the same app finished in between, possibly
/// on another display, [`stale`](Self::stale) says so and the caller re-reads
/// rather than acting on a screen that has moved.
pub struct AppLease {
    guard: Option<OwnedMutexGuard<()>>,
    completed: Option<Arc<AtomicU64>>,
    pub stale: bool,
}

impl AppLease {
    /// A lease nobody needed: no app was named.
    pub fn none() -> Self {
        Self {
            guard: None,
            completed: None,
            stale: false,
        }
    }

    pub fn is_held(&self) -> bool {
        self.guard.is_some()
    }
}

impl Drop for AppLease {
    fn drop(&mut self) {
        // Counted here, on release, not on acquisition: a caller that acquired,
        // wrote and released once must not read its own write as somebody
        // else's and consider its own read stale.
        if let Some(completed) = &self.completed {
            completed.fetch_add(1, Ordering::SeqCst);
        }
        // Then the guard falls, releasing the app for the next writer.
    }
}

/// The app leases and their completion counts.
#[derive(Default)]
pub struct AppGates {
    gates: Mutex<HashMap<String, Arc<AppGate>>>,
}

impl AppGates {
    pub fn global() -> &'static AppGates {
        static GATES: OnceLock<AppGates> = OnceLock::new();
        GATES.get_or_init(AppGates::default)
    }

    fn gate(&self, key: &str) -> Arc<AppGate> {
        let mut gates = self
            .gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(gates.entry(key.to_string()).or_default())
    }

    /// How many writes to this app have finished so far.
    ///
    /// Take this *before* reading the screen, and hand it to
    /// [`lease`](Self::lease) afterwards.
    pub fn completed(&self, key: &str) -> u64 {
        self.gate(key).completed.load(Ordering::SeqCst)
    }

    /// Wait for exclusive use of one app.
    ///
    /// `seen_completed` is what [`completed`](Self::completed) said before the
    /// screen was read; the lease reports whether that read is still current.
    pub async fn lease(&self, key: &str, seen_completed: u64) -> AppLease {
        let gate = self.gate(key);
        let guard = gate.lock.clone().lock_owned().await;
        AppLease {
            stale: gate.completed.load(Ordering::SeqCst) != seen_completed,
            completed: Some(Arc::clone(&gate.completed)),
            guard: Some(guard),
        }
    }
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

    /// Two displays of one phone are two focuses; the same app on both is still
    /// one app.
    #[tokio::test]
    async fn one_app_on_two_displays_is_one_app() {
        let gates = AppGates::default();
        let key = android_app("emulator-5554", "com.example.chat");
        let _held = gates.lease(&key, 0).await;
        let second = tokio::time::timeout(Duration::from_millis(200), gates.lease(&key, 0)).await;
        assert!(second.is_err(), "two writers got into the same app at once");
    }

    /// And two different apps on those displays do not wait for each other.
    #[tokio::test]
    async fn different_apps_on_one_phone_do_not_block_each_other() {
        let gates = AppGates::default();
        let _one = gates
            .lease(&android_app("emulator-5554", "com.example.chat"), 0)
            .await;
        let other = tokio::time::timeout(
            Duration::from_millis(200),
            gates.lease(&android_app("emulator-5554", "com.example.mail"), 0),
        )
        .await;
        assert!(other.is_ok(), "a second app waited on the first");
    }

    /// The same app on two phones is two apps.
    #[tokio::test]
    async fn the_same_app_on_two_phones_is_two_leases() {
        let gates = AppGates::default();
        let _one = gates
            .lease(&android_app("emulator-5554", "com.example.chat"), 0)
            .await;
        let other = tokio::time::timeout(
            Duration::from_millis(200),
            gates.lease(&android_app("emulator-5556", "com.example.chat"), 0),
        )
        .await;
        assert!(other.is_ok(), "a second phone waited on the first");
    }

    /// A lease taken after somebody else wrote to the app says the read that
    /// preceded it is out of date.
    ///
    /// This is the whole reason the completion count exists: the package name
    /// that keys the lease comes from a read taken *before* the lock, so the
    /// read can describe a screen somebody else has already changed.
    #[tokio::test]
    async fn a_lease_after_another_write_reports_a_stale_read() {
        let gates = Arc::new(AppGates::default());
        let key = android_app("emulator-5554", "com.example.chat");

        // Read the count, as a caller does before its screen read.
        let seen = gates.completed(&key);

        // Somebody else writes and finishes first.
        drop(gates.lease(&key, 0).await);

        let late = gates.lease(&key, seen).await;
        assert!(late.stale, "the read was older than a completed write but was not reported");
    }

    /// The writer that got in first has nothing to complain about.
    #[tokio::test]
    async fn an_uncontended_lease_is_not_stale() {
        let gates = AppGates::default();
        let key = android_app("emulator-5554", "com.example.chat");
        let seen = gates.completed(&key);
        assert!(!gates.lease(&key, seen).await.stale);
    }

    /// A lease counting its own release as a foreign write would mark every
    /// second call stale, and the caller would re-read after nothing happened.
    #[tokio::test]
    async fn a_caller_does_not_make_its_own_read_stale() {
        let gates = AppGates::default();
        let key = android_app("emulator-5554", "com.example.chat");
        let seen = gates.completed(&key);
        drop(gates.lease(&key, seen).await);
        let next_seen = gates.completed(&key);
        assert_eq!(next_seen, seen + 1, "the release was not counted");
        assert!(!gates.lease(&key, next_seen).await.stale);
    }
}
