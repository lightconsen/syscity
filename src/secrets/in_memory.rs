//! Tier 3 backend — memory only (zeroize).
//!
//! For secrets with very short lifetimes that must never be written to disk
//! (e.g. OAuth access tokens). Values are zeroized automatically on drop;
//! `Debug` always shows `[REDACTED]`.

use std::collections::HashMap;
use std::sync::Mutex;

use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::secrets::store::{SecretId, SecretStore};

/// A single in-memory secret (value zeroized on drop).
#[derive(Zeroize, ZeroizeOnDrop)]
struct MemoryEntry {
    #[zeroize]
    value: String,
}

impl std::fmt::Debug for MemoryEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryEntry")
            .field("value", &"[REDACTED]")
            .finish()
    }
}

/// Memory backend: a map keyed by the stringified `SecretId`.
#[derive(Debug)]
pub struct MemoryStore {
    inner: Mutex<HashMap<String, MemoryEntry>>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    /// Create an empty memory backend.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl SecretStore for MemoryStore {
    async fn get(&self, id: &SecretId) -> crate::Result<Option<String>> {
        let inner = self.inner.lock().map_err(|_| {
            crate::error::SyscityError::Internal("memory store poisoned".to_string())
        })?;
        Ok(inner.get(&id.to_string()).map(|e| e.value.clone()))
    }

    async fn set(&self, id: &SecretId, value: &str) -> crate::Result<()> {
        let mut inner = self.inner.lock().map_err(|_| {
            crate::error::SyscityError::Internal("memory store poisoned".to_string())
        })?;
        inner.insert(id.to_string(), MemoryEntry { value: value.to_string() });
        Ok(())
    }

    async fn delete(&self, id: &SecretId) -> crate::Result<()> {
        let mut inner = self.inner.lock().map_err(|_| {
            crate::error::SyscityError::Internal("memory store poisoned".to_string())
        })?;
        inner.remove(&id.to_string());
        Ok(())
    }

    async fn has(&self, id: &SecretId) -> bool {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        inner.contains_key(&id.to_string())
    }
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::store::SecretId;

    #[tokio::test]
    async fn test_memory_store_roundtrip() {
        let store = MemoryStore::new();
        let id = SecretId::new("mcp", "server-a", "access_token");

        assert!(!store.has(&id).await);
        store.set(&id, "at_secret").await.unwrap();
        assert!(store.has(&id).await);
        assert_eq!(store.get(&id).await.unwrap(), Some("at_secret".to_string()));

        store.delete(&id).await.unwrap();
        assert!(!store.has(&id).await);
        assert_eq!(store.get(&id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_memory_store_namespace_isolation() {
        let store = MemoryStore::new();
        let a = SecretId::new("mcp", "a", "access_token");
        let b = SecretId::new("mcp", "b", "access_token");
        store.set(&a, "x").await.unwrap();
        assert!(!store.has(&b).await);
    }

    #[test]
    fn test_memory_entry_debug_redacts() {
        let e = MemoryEntry {
            value: "super-secret".to_string(),
        };
        let debug = format!("{e:?}");
        assert!(!debug.contains("super-secret"));
        assert!(debug.contains("REDACTED"));
    }
}
