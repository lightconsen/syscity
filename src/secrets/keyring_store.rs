//! Tier 1 backend — OS keyring.
//!
//! Each `(namespace, entity)` maps to a single keyring entry:
//!
//! - service: `syscity/{namespace}`
//! - account: `{entity}`
//! - value: JSON-serialized map of kind → value
//!
//! The keyring stores opaque byte strings, so the per-entity map is flattened
//! into a single JSON string. This mirrors the file fallback (`FileStore`),
//! where an entity is one TOML file whose `[secrets]` table is the same
//! kind → value map — both backends therefore expose identical semantics.
//!
//! The `keyring` crate is a synchronous, blocking API (Keychain Services /
//! D-Bus Secret Service), so every async `SecretStore` method runs the blocking
//! work on a blocking thread via `tokio::task::spawn_blocking` instead of
//! stalling the async runtime. Every call is additionally bounded by
//! `KEYRING_OP_TIMEOUT`: macOS dark wake can hang a SecurityServer RPC
//! indefinitely, so a hang must degrade to an error rather than block.

use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tracing::warn;

use crate::error::SyscityError;
use crate::secrets::store::{SecretId, SecretStore};

/// Max time a single keyring operation may take before it is treated as
/// failed. macOS dark wake can hang `SecKeychainFindGenericPassword` on a
/// securityd RPC that never returns, so every keyring call is bounded and a
/// hang degrades to an error instead of stalling the caller forever.
#[cfg(test)]
const KEYRING_OP_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(not(test))]
const KEYRING_OP_TIMEOUT: Duration = Duration::from_secs(5);

/// Minimum gap between keyring availability re-probes while the keyring is
/// down — avoids hammering securityd during a dark-wake window.
const PROBE_COOLDOWN_SECS: u64 = 10;

/// A single keyring credential operation (injectable for tests).
///
/// The production backend wraps the platform OS keyring; tests inject an
/// in-memory map so `KeyringStore` logic is exercised without touching the
/// user's keychain (keyring v3 ships no mock feature).
pub trait CredentialBackend: Debug + Send + Sync {
    /// Read a credential; `Ok(None)` when the entry does not exist.
    fn get(&self, service: &str, user: &str) -> crate::Result<Option<String>>;

    /// Write (create or overwrite) a credential.
    fn set(&self, service: &str, user: &str, value: &str) -> crate::Result<()>;

    /// Delete a credential; missing is not an error.
    fn delete(&self, service: &str, user: &str) -> crate::Result<()>;
}

/// Production backend backed by the platform OS keyring.
#[derive(Debug, Default, Clone)]
struct OsKeyringBackend;

impl CredentialBackend for OsKeyringBackend {
    fn get(&self, service: &str, user: &str) -> crate::Result<Option<String>> {
        let entry = keyring_entry(service, user)?;
        match entry.get_password() {
            Ok(raw) => Ok(Some(raw)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => {
                Err(SyscityError::Internal(format!("keyring read error ({service} / {user}): {e}")))
            }
        }
    }

    fn set(&self, service: &str, user: &str, value: &str) -> crate::Result<()> {
        let entry = keyring_entry(service, user)?;
        entry.set_password(value).map_err(|e| {
            SyscityError::Internal(format!("keyring write error ({service} / {user}): {e}"))
        })
    }

    fn delete(&self, service: &str, user: &str) -> crate::Result<()> {
        let entry = keyring_entry(service, user)?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SyscityError::Internal(format!(
                "keyring delete error ({service} / {user}): {e}"
            ))),
        }
    }
}

/// Tier 1 backend bound to a namespace (`syscity/{namespace}` service).
#[derive(Debug, Clone)]
pub struct KeyringStore {
    namespace: String,
    backend: Arc<dyn CredentialBackend>,
}

impl KeyringStore {
    /// Create a keyring backend bound to a namespace.
    pub fn new(namespace: &str) -> Self {
        Self {
            namespace: namespace.to_string(),
            backend: Arc::new(OsKeyringBackend),
        }
    }

    /// Test constructor: bind an injected credential backend.
    #[cfg(test)]
    pub(crate) fn with_backend(namespace: &str, backend: Arc<dyn CredentialBackend>) -> Self {
        Self {
            namespace: namespace.to_string(),
            backend,
        }
    }

    /// Keyring service name: `syscity/{namespace}`.
    pub fn service(&self) -> String {
        format!("syscity/{}", self.namespace)
    }

    /// Read an entity's whole map; a missing entry yields an empty map.
    fn get_entry(&self, entity: &str) -> crate::Result<HashMap<String, String>> {
        match self.backend.get(&self.service(), entity)? {
            Some(raw) => decode_value(&raw),
            None => Ok(HashMap::new()),
        }
    }

    /// Overwrite an entity's whole map (an empty map deletes the entry).
    fn set_entry(&self, entity: &str, map: &HashMap<String, String>) -> crate::Result<()> {
        if map.is_empty() {
            return self.delete_entry(entity);
        }
        let raw = encode_value(map)?;
        self.backend.set(&self.service(), entity, &raw)
    }

    /// Delete an entity's whole entry (missing is not an error).
    fn delete_entry(&self, entity: &str) -> crate::Result<()> {
        self.backend.delete(&self.service(), entity)
    }

    /// Whether an entity already has a keyring entry.
    fn has_entry(&self, entity: &str) -> bool {
        matches!(self.backend.get(&self.service(), entity), Ok(Some(_)))
    }
}

/// Run blocking keyring work off the async runtime, bounded by a timeout.
///
/// A hung SecurityServer RPC (macOS dark wake) must degrade to an error — and
/// mark the keyring down so routing falls back to the file store — instead of
/// stalling the async caller forever.
async fn blocking<T>(work: impl FnOnce() -> crate::Result<T> + Send + 'static) -> crate::Result<T>
where
    T: Send + 'static,
{
    match tokio::time::timeout(KEYRING_OP_TIMEOUT, tokio::task::spawn_blocking(work)).await {
        Ok(join) => {
            let result =
                join.map_err(|e| SyscityError::Internal(format!("keyring task failed: {e}")))?;
            if result.is_err() {
                mark_keyring_down();
            }
            result
        }
        Err(_) => {
            mark_keyring_down();
            Err(SyscityError::Internal(
                "keyring operation timed out (display asleep?)".to_string(),
            ))
        }
    }
}

#[async_trait::async_trait]
impl SecretStore for KeyringStore {
    async fn get(&self, id: &SecretId) -> crate::Result<Option<String>> {
        let store = self.clone();
        let entity = id.entity.clone();
        let kind = id.kind.clone();
        blocking(move || store.get_entry(&entity))
            .await
            .map(|map| map.get(&kind).cloned())
    }

    async fn set(&self, id: &SecretId, value: &str) -> crate::Result<()> {
        let store = self.clone();
        let entity = id.entity.clone();
        let kind = id.kind.clone();
        let value = value.to_string();
        blocking(move || {
            let mut map = store.get_entry(&entity)?;
            map.insert(kind, value);
            store.set_entry(&entity, &map)
        })
        .await
    }

    async fn delete(&self, id: &SecretId) -> crate::Result<()> {
        let store = self.clone();
        let entity = id.entity.clone();
        let kind = id.kind.clone();
        blocking(move || {
            let mut map = store.get_entry(&entity)?;
            if map.remove(&kind).is_some() {
                if map.is_empty() {
                    store.delete_entry(&entity)?;
                } else {
                    store.set_entry(&entity, &map)?;
                }
            }
            Ok(())
        })
        .await
    }

    async fn has(&self, id: &SecretId) -> bool {
        matches!(self.get(id).await, Ok(Some(_)))
    }

    async fn get_all(&self, entity: &str) -> crate::Result<HashMap<String, String>> {
        let store = self.clone();
        let entity = entity.to_string();
        blocking(move || store.get_entry(&entity)).await
    }

    async fn set_all(&self, entity: &str, map: &HashMap<String, String>) -> crate::Result<()> {
        let store = self.clone();
        let entity = entity.to_string();
        let map = map.clone();
        blocking(move || store.set_entry(&entity, &map)).await
    }

    async fn delete_entity(&self, entity: &str) -> crate::Result<()> {
        let store = self.clone();
        let entity = entity.to_string();
        blocking(move || store.delete_entry(&entity)).await
    }

    async fn has_entity(&self, entity: &str) -> bool {
        let store = self.clone();
        let entity = entity.to_string();
        match blocking(move || Ok(store.has_entry(&entity))).await {
            Ok(v) => v,
            Err(e) => {
                warn!("keyring task failed: {e}");
                false
            }
        }
    }
}

/// Build a keyring entry, mapping platform errors to a Syscity error.
fn keyring_entry(service: &str, user: &str) -> crate::Result<keyring::Entry> {
    keyring::Entry::new(service, user).map_err(|e| {
        SyscityError::Internal(format!("keyring entry unavailable ({service} / {user}): {e}"))
    })
}

/// Serialize a map into a single keyring value.
pub fn encode_value(map: &HashMap<String, String>) -> crate::Result<String> {
    serde_json::to_string(map)
        .map_err(|e| SyscityError::Internal(format!("keyring value serialization failed: {e}")))
}

/// Parse a keyring value back into a map.
pub fn decode_value(raw: &str) -> crate::Result<HashMap<String, String>> {
    serde_json::from_str(raw)
        .map_err(|e| SyscityError::Internal(format!("keyring value deserialization failed: {e}")))
}

/// Run a blocking closure on a detached thread with a timeout; `None` when it
/// does not finish in time.
///
/// Used by synchronous paths (availability probe, master key load) that run on
/// the caller's thread. A hung thread is leaked — bounded to one per call, so
/// callers must be throttled (the probe is cooldown-gated) or one-shot (the
/// master key is cached once per process).
pub(crate) fn with_timeout<T>(work: impl FnOnce() -> T + Send + 'static) -> Option<T>
where
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx.recv_timeout(KEYRING_OP_TIMEOUT).ok()
}

/// Throttled, recoverable keyring availability tracker.
///
/// Unlike a one-shot `OnceLock<bool>`, a failed probe is not cached for the
/// process lifetime: after `cooldown_secs` the next `available()` re-probes, so
/// a daemon started during macOS dark wake recovers automatically once the
/// display wakes — no restart required.
#[derive(Debug)]
struct KeyringHealth {
    up: AtomicBool,
    last_probe: AtomicU64,
    cooldown_secs: u64,
}

impl KeyringHealth {
    const fn new(cooldown_secs: u64) -> Self {
        Self {
            up: AtomicBool::new(false),
            last_probe: AtomicU64::new(0),
            cooldown_secs,
        }
    }

    /// Current availability, re-probing only when the cooldown has expired.
    ///
    /// `probe` runs under `with_timeout`, so a dark-wake hang degrades to
    /// `false` instead of blocking the caller. Returns `true` with no I/O while
    /// the keyring is confirmed up.
    fn available(&self, now_secs: u64, probe: impl Fn() -> bool + Send + 'static) -> bool {
        if self.up.load(Ordering::Relaxed) {
            return true;
        }
        if now_secs.saturating_sub(self.last_probe.load(Ordering::Relaxed)) < self.cooldown_secs {
            return false;
        }
        self.last_probe.store(now_secs, Ordering::Relaxed);
        let ok = with_timeout(probe).unwrap_or(false);
        self.up.store(ok, Ordering::Relaxed);
        ok
    }

    /// Record a failed/timed-out keyring operation: marks the keyring down and
    /// restarts the cooldown so the next `available()` re-probes after it
    /// expires, allowing automatic recovery.
    fn mark_down(&self, now_secs: u64) {
        self.up.store(false, Ordering::Relaxed);
        self.last_probe.store(now_secs, Ordering::Relaxed);
    }
}

/// Process-wide keyring availability.
static KEYRING_HEALTH: KeyringHealth = KeyringHealth::new(PROBE_COOLDOWN_SECS);

/// Current unix time in seconds.
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether the OS keyring is usable.
///
/// Recoverable: a failure is throttled by a cooldown and re-probed, so
/// availability is re-evaluated instead of being cached for the process
/// lifetime. Under `cfg(test)` the probe always returns `false` so tests never
/// touch a real keychain.
pub fn probe_keyring() -> bool {
    if keyring_disabled() {
        return false;
    }
    KEYRING_HEALTH.available(now_unix_secs(), probe_keyring_uncached)
}

/// Whether `SYSCITY_DISABLE_KEYRING` turns the OS keyring off for this process.
///
/// The `cfg(test)` guard only covers the library's own unit-test binary; an
/// integration test links the library built normally, so it would probe the
/// developer's real keychain — on macOS that means a password prompt per run.
/// This is the explicit opt-out for a host with no interactive keychain: a test
/// harness, a CI runner, or a headless server. The 0600 file store is used
/// instead, exactly as it is when the probe fails.
pub fn keyring_disabled() -> bool {
    match std::env::var("SYSCITY_DISABLE_KEYRING") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !(v.is_empty() || v == "0" || v == "false" || v == "no")
        }
        Err(_) => false,
    }
}

/// Mark the keyring as unavailable after a failed or timed-out operation so
/// subsequent routing cheaply uses the file fallback until a re-probe succeeds.
fn mark_keyring_down() {
    KEYRING_HEALTH.mark_down(now_unix_secs());
}

#[cfg(test)]
fn probe_keyring_uncached() -> bool {
    false
}

#[cfg(not(test))]
fn probe_keyring_uncached() -> bool {
    let entry = match keyring::Entry::new("syscity/probe", "probe") {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("Keyring probe failed to open entry: {e}");
            return false;
        }
    };
    // Clear any stale probe result, then verify a full write/read cycle.
    let _ = entry.delete_credential();
    if entry.set_password("probe-ok").is_err() {
        return false;
    }
    let ok = matches!(entry.get_password().as_deref(), Ok("probe-ok"));
    let _ = entry.delete_credential();
    ok
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory credential store used to test `KeyringStore` logic without a
    /// real keychain (keyring v3 provides no mock feature).
    #[derive(Debug, Default)]
    struct MemoryCredentialBackend {
        entries: std::sync::Mutex<HashMap<(String, String), String>>,
    }

    impl CredentialBackend for MemoryCredentialBackend {
        fn get(&self, service: &str, user: &str) -> crate::Result<Option<String>> {
            let entries = self.entries.lock().unwrap();
            Ok(entries
                .get(&(service.to_string(), user.to_string()))
                .cloned())
        }

        fn set(&self, service: &str, user: &str, value: &str) -> crate::Result<()> {
            let mut entries = self.entries.lock().unwrap();
            entries.insert((service.to_string(), user.to_string()), value.to_string());
            Ok(())
        }

        fn delete(&self, service: &str, user: &str) -> crate::Result<()> {
            let mut entries = self.entries.lock().unwrap();
            entries.remove(&(service.to_string(), user.to_string()));
            Ok(())
        }
    }

    fn mock_store(namespace: &str) -> KeyringStore {
        let backend = Arc::new(MemoryCredentialBackend::default());
        KeyringStore::with_backend(namespace, backend)
    }

    #[test]
    fn test_service_name() {
        let store = KeyringStore::new("mcp-env");
        assert_eq!(store.service(), "syscity/mcp-env");
    }

    #[test]
    fn test_value_roundtrip() {
        let mut map = HashMap::new();
        map.insert("refresh_token".to_string(), "rt_abc".to_string());
        map.insert("client_id".to_string(), "cid".to_string());
        let raw = encode_value(&map).unwrap();
        assert!(raw.contains("rt_abc"));
        let decoded = decode_value(&raw).unwrap();
        assert_eq!(decoded, map);
    }

    #[test]
    fn test_decode_rejects_garbage() {
        assert!(decode_value("not json").is_err());
        assert!(decode_value("").is_err());
    }

    #[test]
    fn test_probe_is_safe_under_test() {
        // Under cfg(test) the probe must never touch a real keychain.
        assert!(!probe_keyring());
    }

    /// A backend whose read blocks until the test sends on the channel —
    /// simulating a dark-wake SecurityServer RPC that never returns.
    #[derive(Debug)]
    struct HangingBackend {
        blocked: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl CredentialBackend for HangingBackend {
        fn get(&self, _service: &str, _user: &str) -> crate::Result<Option<String>> {
            // Runs on a blocking thread; the std lock is fine here.
            if let Ok(guard) = self.blocked.lock() {
                let _ = guard.recv();
            }
            Ok(None)
        }

        fn set(&self, _service: &str, _user: &str, _value: &str) -> crate::Result<()> {
            Ok(())
        }

        fn delete(&self, _service: &str, _user: &str) -> crate::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_blocking_times_out_on_hung_backend() {
        // A hung keychain read must degrade to an error instead of stalling.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let store = KeyringStore::with_backend(
            "llm",
            Arc::new(HangingBackend {
                blocked: std::sync::Mutex::new(rx),
            }),
        );
        let id = SecretId::new("llm", "deepseek", "api_key");

        let start = std::time::Instant::now();
        let result = store.get(&id).await;
        assert!(result.is_err(), "hung read should time out, got {result:?}");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "timeout should fire quickly"
        );

        // Release the blocked thread so the test process exits cleanly.
        let _ = tx.send(());
    }

    #[test]
    fn test_keyring_health_first_probe_and_recovery() {
        let health = KeyringHealth::new(10);
        // First call settles the state with a real probe.
        assert!(!health.available(1000, || false));
        // Within the cooldown the probe is not called again.
        assert!(!health.available(1005, || panic!("probe must be throttled")));
        // After the cooldown a re-probe succeeds → recovery.
        assert!(health.available(1011, || true));
        // Up is sticky — no further probing.
        assert!(health.available(2000, || panic!("must not re-probe while up")));
    }

    #[test]
    fn test_keyring_health_throttles_down_probes() {
        let health = KeyringHealth::new(10);
        assert!(!health.available(1000, || false)); // probe → down
        assert!(!health.available(1005, || panic!("throttled")));
        assert!(!health.available(1010, || false)); // cooldown over → re-probe → down
        assert!(!health.available(1015, || panic!("throttled")));
    }

    #[test]
    fn test_keyring_health_mark_down_restarts_cooldown() {
        let health = KeyringHealth::new(10);
        assert!(health.available(1000, || true)); // up
        health.mark_down(2000); // a keyring op failed at 2000
        assert!(!health.available(2005, || panic!("cooldown must restart on mark_down")));
        assert!(health.available(2011, || true)); // cooldown over → recovered
    }

    #[tokio::test]
    async fn test_get_set_delete_roundtrip() {
        let store = mock_store("channel");
        let id = SecretId::new("channel", "whatsapp", "access_token");

        assert!(!store.has(&id).await);
        assert_eq!(store.get(&id).await.unwrap(), None);

        store.set(&id, "at_secret").await.unwrap();
        assert!(store.has(&id).await);
        assert_eq!(store.get(&id).await.unwrap(), Some("at_secret".to_string()));

        // A second kind on the same entity shares the same entry.
        let id2 = SecretId::new("channel", "whatsapp", "app_secret");
        store.set(&id2, "app_sec").await.unwrap();
        assert_eq!(store.get(&id2).await.unwrap(), Some("app_sec".to_string()));
        assert_eq!(store.get(&id).await.unwrap(), Some("at_secret".to_string()));
        assert!(store.has_entity("whatsapp").await);

        // Deleting the last kind removes the whole entity entry.
        store.delete(&id).await.unwrap();
        assert!(!store.has(&id).await);
        assert!(store.has(&id2).await);
        store.delete(&id2).await.unwrap();
        assert!(!store.has(&id2).await);
        assert!(!store.has_entity("whatsapp").await);
    }

    #[tokio::test]
    async fn test_get_all_set_all_entity() {
        let store = mock_store("mcp-env");
        let mut env = HashMap::new();
        env.insert("GITHUB_TOKEN".to_string(), "ghp_x".to_string());
        store.set_all("github", &env).await.unwrap();

        assert!(store.has_entity("github").await);
        let loaded = store.get_all("github").await.unwrap();
        assert_eq!(loaded.get("GITHUB_TOKEN").map(String::as_str), Some("ghp_x"));

        // set_all with an empty map removes the entry.
        store.set_all("github", &HashMap::new()).await.unwrap();
        assert!(!store.has_entity("github").await);
        assert!(store.get_all("github").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_delete_missing_is_ok() {
        let store = mock_store("llm");
        let id = SecretId::new("llm", "anthropic", "api_key");
        store.delete(&id).await.unwrap();
        store.delete_entity("anthropic").await.unwrap();
        assert!(!store.has_entity("anthropic").await);
    }

    #[tokio::test]
    async fn test_namespaces_are_isolated() {
        let backend = Arc::new(MemoryCredentialBackend::default());
        let a = KeyringStore::with_backend("channel", backend.clone());
        let b = KeyringStore::with_backend("llm", backend);

        a.set(&SecretId::new("channel", "whatsapp", "token"), "v")
            .await
            .unwrap();
        // The same account name in a different namespace must not collide.
        assert!(!b.has_entity("whatsapp").await);
        assert!(a.has_entity("whatsapp").await);
    }
}
