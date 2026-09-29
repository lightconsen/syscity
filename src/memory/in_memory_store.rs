//! In-Memory Store — ephemeral working-memory backend
//!
//! Pure HashMap storage with no persistence.  Designed for the
//! **Working** memory tier: hot, volatile, process-local context
//! that is lost on restart.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::{debug, info};

use super::{Memory, MemoryId, MemoryQuery, MemoryStats, MemoryStore};

/// Default maximum number of entries in the in-memory store.
const DEFAULT_MAX_CAPACITY: usize = 10_000;

/// Ephemeral in-memory memory store.
#[derive(Debug, Clone)]
pub struct InMemoryStore {
    entries: Arc<RwLock<HashMap<String, Memory>>>,
    max_capacity: usize,
}

impl InMemoryStore {
    /// Create a new empty in-memory store with default capacity.
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            max_capacity: DEFAULT_MAX_CAPACITY,
        }
    }

    /// Create a new empty in-memory store with a specific max capacity.
    pub fn with_capacity(max_capacity: usize) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            max_capacity,
        }
    }

    /// Total number of stored memories.
    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    /// Check if the store is empty.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MemoryStore for InMemoryStore {
    async fn store(&self, mut memory: Memory) -> crate::Result<MemoryId> {
        let id = memory.id.clone();
        // Record initial access on store
        memory.record_access();
        let mut guard = self.entries.write().await;
        // Evict lowest-importance entry when at capacity to bound memory growth.
        if guard.len() >= self.max_capacity && !guard.contains_key(&id.to_string()) {
            if let Some(min_key) = guard
                .iter()
                .min_by(|a, b| {
                    a.1.importance_score
                        .partial_cmp(&b.1.importance_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(k, _)| k.clone())
            {
                guard.remove(&min_key);
                debug!("Evicted memory {} from working tier (capacity reached)", min_key);
            }
        }
        guard.insert(id.to_string(), memory);
        debug!("Stored memory in working tier: {}", id);
        Ok(id)
    }

    async fn get(&self, id: &MemoryId) -> crate::Result<Option<Memory>> {
        let mut guard = self.entries.write().await;
        if let Some(mut memory) = guard.get_mut(&id.to_string()).cloned() {
            // Record access and update in place
            memory.record_access();
            guard.insert(id.to_string(), memory.clone());
            Ok(Some(memory))
        } else {
            Ok(None)
        }
    }

    async fn update(&self, memory: Memory) -> crate::Result<()> {
        let mut guard = self.entries.write().await;
        guard.insert(memory.id.to_string(), memory);
        Ok(())
    }

    async fn update_importance_score(
        &self,
        id: &MemoryId,
        new_score: f32,
    ) -> crate::Result<Option<Memory>> {
        let mut guard = self.entries.write().await;
        let Some(memory) = guard.get_mut(&id.to_string()) else {
            return Ok(None);
        };
        if (memory.importance_score - new_score).abs() >= 0.001 {
            memory.importance_score = new_score;
        }
        Ok(Some(memory.clone()))
    }

    async fn delete(&self, id: &MemoryId) -> crate::Result<bool> {
        let mut guard = self.entries.write().await;
        Ok(guard.remove(&id.to_string()).is_some())
    }

    async fn search(&self, query: MemoryQuery) -> crate::Result<Vec<Memory>> {
        let guard = self.entries.read().await;
        let mut results: Vec<Memory> = guard
            .values()
            .filter(|m| {
                // user_id filter
                if let Some(ref user_id) = query.user_id {
                    if m.user_id != *user_id {
                        return false;
                    }
                }
                // conversation_id filter — an unbound memory is user-level
                // and matches any conversation (mirrors `DatabaseStore`).
                if let Some(ref conv_id) = query.conversation_id {
                    if m.conversation_id
                        .as_ref()
                        .is_some_and(|bound| bound != conv_id)
                    {
                        return false;
                    }
                }
                // memory_type filter
                if let Some(ref mem_type) = query.memory_type {
                    if m.memory_type != *mem_type {
                        return false;
                    }
                }
                // content query (simple substring)
                if let Some(ref content) = query.content_query {
                    if !m.content.to_lowercase().contains(&content.to_lowercase()) {
                        return false;
                    }
                }
                // expired filter
                if !query.include_expired && m.is_expired() {
                    return false;
                }
                true
            })
            .cloned()
            .collect();

        // Sort by importance descending
        results.sort_by(|a, b| {
            b.importance_score
                .partial_cmp(&a.importance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Apply limit and offset
        let offset = query.offset.min(results.len());
        let limit = query.limit.min(results.len() - offset);
        let sliced: Vec<Memory> = results.into_iter().skip(offset).take(limit).collect();

        Ok(sliced)
    }

    async fn cleanup_expired(&self) -> crate::Result<usize> {
        let mut guard = self.entries.write().await;
        let before = guard.len();
        guard.retain(|_, m| !m.is_expired());
        let removed = before - guard.len();
        debug!("Cleaned up {} expired working memories", removed);
        Ok(removed)
    }

    async fn stats(&self) -> crate::Result<MemoryStats> {
        let guard = self.entries.read().await;
        let total = guard.len();
        let mut count_by_type = HashMap::new();
        let mut expired = 0;
        for m in guard.values() {
            *count_by_type.entry(m.memory_type.clone()).or_insert(0) += 1;
            if m.is_expired() {
                expired += 1;
            }
        }
        Ok(MemoryStats {
            total_count: total,
            count_by_type,
            expired_count: expired,
        })
    }

    async fn close(&self) -> crate::Result<()> {
        let mut guard = self.entries.write().await;
        guard.clear();
        info!("InMemoryStore closed and cleared");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_in_memory_store_roundtrip() {
        let store = InMemoryStore::new();
        let mem = Memory::new("u1", "Hello world", "fact").with_importance_score(0.8);
        let id = store.store(mem.clone()).await.unwrap();

        let fetched = store.get(&id).await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().content, "Hello world");
    }

    #[tokio::test]
    async fn test_in_memory_search() {
        let store = InMemoryStore::new();
        store
            .store(Memory::new("u1", "I love sushi", "preference"))
            .await
            .unwrap();
        store
            .store(Memory::new("u1", "I work at Google", "fact"))
            .await
            .unwrap();
        store
            .store(Memory::new("u2", "I hate sushi", "preference"))
            .await
            .unwrap();

        let results = store
            .search(MemoryQuery::new().for_user("u1").with_content("sushi"))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("love"));
    }

    #[tokio::test]
    async fn test_in_memory_store_respects_capacity() {
        let store = InMemoryStore::with_capacity(3);

        // Insert memories with strictly increasing importance scores.
        for (idx, score) in [0.1_f32, 0.3, 0.5, 0.7, 0.9].iter().enumerate() {
            let mem =
                Memory::new("u1", format!("content-{}", idx), "fact").with_importance_score(*score);
            store.store(mem).await.unwrap();
        }

        assert_eq!(store.len().await, 3);

        let remaining: Vec<f32> = store
            .search(MemoryQuery::new().for_user("u1").limit(10))
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.importance_score)
            .collect();
        assert_eq!(remaining, vec![0.9_f32, 0.7, 0.5]);
    }

    #[tokio::test]
    async fn test_in_memory_store_capacity_boundary() {
        let store = InMemoryStore::with_capacity(2);

        for (idx, score) in [0.4_f32, 0.6].iter().enumerate() {
            let mem =
                Memory::new("u1", format!("content-{}", idx), "fact").with_importance_score(*score);
            store.store(mem).await.unwrap();
        }
        assert_eq!(store.len().await, 2);

        // Adding a third memory should evict the lowest-importance entry.
        store
            .store(Memory::new("u1", "content-2", "fact").with_importance_score(0.8))
            .await
            .unwrap();
        assert_eq!(store.len().await, 2);

        let remaining: Vec<f32> = store
            .search(MemoryQuery::new().for_user("u1").limit(10))
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.importance_score)
            .collect();
        assert_eq!(remaining, vec![0.8_f32, 0.6]);
    }
}

/// A memory stored without a conversation is user-level: it must match a
/// `for_conversation` query for *any* conversation, while a memory bound
/// to a different conversation stays out. (Mirrors `DatabaseStore`'s
/// `conversation_id IS NULL OR conversation_id = ?`.)
#[tokio::test]
async fn unbound_memories_match_any_conversation() {
    let store = InMemoryStore::new();
    store
        .store(Memory::new("u1", "user-level fact", "fact"))
        .await
        .unwrap();
    store
        .store(Memory::new("u1", "bound elsewhere", "compaction").with_conversation("conv-other"))
        .await
        .unwrap();

    let hits = store
        .search(MemoryQuery::new().for_user("u1").for_conversation("conv-1"))
        .await
        .unwrap();
    let contents: Vec<&str> = hits.iter().map(|m| m.content.as_str()).collect();
    assert!(
        contents.contains(&"user-level fact"),
        "an unbound memory must match any conversation, got {contents:?}"
    );
    assert!(
        !contents.contains(&"bound elsewhere"),
        "a memory bound elsewhere must stay scoped, got {contents:?}"
    );
}
