//! Who is watching which screen.
//!
//! The registry the watchers live in. It is deliberately small: a map of watch
//! id to a stop flag, and nothing else. The loop that captures lives in the
//! gateway (it needs an event bus and a task registry), and what it shares with
//! `computer.unwatch` is the single bit "are you still wanted".
//!
//! # Why a flag and not an abort
//!
//! A watcher between captures must stop *without* dropping the frame it is
//! writing. Aborting the task mid-broadcast would be fine too — the frame is
//! already on the bus at that point — but a flag lets the loop finish its
//! current interval and log why it ended, which is what makes "the watcher
//! stopped" and "the watcher died" different lines in a log.
//!
//! # Why anyone may stop any watch
//!
//! There is one screen. Two viewers watching it is two streams of the same
//! pixels, and the person who needs a watcher stopped is often not the one who
//! started it (a tab was left open, a viewer reconnected). The registry is
//! therefore keyed only by id, with no owner check — the same reasoning as
//! `computer_runs`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// One running watcher's stop flag.
#[derive(Debug)]
pub struct WatchHandle {
    id: String,
    stop: Arc<AtomicBool>,
    /// For the listing: what interval it was asked to run at.
    interval_ms: u64,
}

impl WatchHandle {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    /// Whether the watcher has been told to stop.
    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Tell the watcher to stop after the frame it is on.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The live watchers.
#[derive(Debug, Default)]
pub struct WatchRegistry {
    inner: Mutex<HashMap<String, Arc<WatchHandle>>>,
}

impl WatchRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start tracking a watcher and return its handle.
    ///
    /// The id is minted here rather than by the caller so that two viewers
    /// cannot collide on one, and so that a client has no say in what a frame
    /// is labelled.
    pub fn start(&self, interval_ms: u64) -> Arc<WatchHandle> {
        let id = uuid::Uuid::new_v4().to_string();
        let handle = Arc::new(WatchHandle {
            id: id.clone(),
            stop: Arc::new(AtomicBool::new(false)),
            interval_ms,
        });
        self.lock().insert(id, handle.clone());
        handle
    }

    /// Forget a watcher. Idempotent: the loop calls this as it exits, and
    /// `unwatch` may have called it already.
    pub fn finish(&self, id: &str) {
        self.lock().remove(id);
    }

    /// Ask one watcher to stop, and forget it. `None` when no watcher has that
    /// id.
    ///
    /// The entry is removed here rather than when the task notices the flag,
    /// because the registry has to answer "is something capturing my screen"
    /// the moment the caller is told it stopped. Leaving it until the loop
    /// wakes up makes `unwatch` followed by `computer.status` report a watcher
    /// that is already told to go — a lie in the direction that costs a person
    /// a second call they did not need to make. The loop's own `finish` is
    /// therefore a no-op after this, which is why it is idempotent.
    pub fn stop(&self, id: &str) -> Option<Arc<WatchHandle>> {
        let handle = self.lock().remove(id)?;
        handle.stop();
        Some(handle)
    }

    /// Every live watcher, for a status read.
    pub fn list(&self) -> Vec<Arc<WatchHandle>> {
        let mut out: Vec<_> = self.lock().values().cloned().collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A poisoned map is not a reason to lose the screen: the map holds
    /// handles, and a panic that poisoned it left them valid.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<WatchHandle>>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starting_a_watch_mints_a_distinct_id() {
        let registry = WatchRegistry::new();
        let a = registry.start(1000);
        let b = registry.start(1000);
        assert_ne!(a.id(), b.id());
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn stopping_one_watch_leaves_the_others_running() {
        let registry = WatchRegistry::new();
        let a = registry.start(500);
        let b = registry.start(500);
        assert!(registry.stop(a.id()).is_some());
        assert!(a.stopping(), "the one asked to stop is stopping");
        assert!(!b.stopping(), "the other one is not");
        assert_eq!(registry.len(), 1, "the stopped watcher is forgotten at once");
    }

    #[test]
    fn stopping_an_unknown_watch_is_not_an_error() {
        let registry = WatchRegistry::new();
        assert!(registry.stop("no-such-watch").is_none());
    }

    #[test]
    fn finishing_is_idempotent() {
        let registry = WatchRegistry::new();
        let handle = registry.start(1000);
        registry.finish(handle.id());
        registry.finish(handle.id());
        assert!(registry.is_empty());
    }

    #[test]
    fn the_listing_is_ordered_so_a_client_can_diff_it() {
        let registry = WatchRegistry::new();
        for _ in 0..5 {
            registry.start(1000);
        }
        let listed: Vec<String> = registry.list().iter().map(|h| h.id().to_string()).collect();
        let mut sorted = listed.clone();
        sorted.sort();
        assert_eq!(listed, sorted);
    }
}
