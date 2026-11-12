//! Vector storage trait and in-memory implementation.

use std::collections::{HashMap, VecDeque};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::chunk::{cosine_similarity, EmbeddedChunk};

/// Vector storage trait
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Store a chunk with its embedding
    async fn store_chunk(&self, chunk: EmbeddedChunk) -> crate::Result<()>;

    /// Store multiple chunks
    async fn store_chunks(&self, chunks: Vec<EmbeddedChunk>) -> crate::Result<()> {
        for chunk in chunks {
            self.store_chunk(chunk).await?;
        }
        Ok(())
    }

    /// Search for similar chunks.
    ///
    /// `collection` filters results to a single collection when provided. The
    /// backend should apply the filter as early as possible (ideally in the
    /// index/query) rather than fetching all results and filtering in memory.
    async fn search_similar(
        &self,
        query_embedding: &[f32],
        limit: usize,
        threshold: f32,
        collection: Option<&str>,
    ) -> crate::Result<Vec<(EmbeddedChunk, f32)>>;

    /// Delete chunks by source ID
    async fn delete_by_source(&self, source_id: &str) -> crate::Result<usize>;

    /// Delete chunks belonging to `source_id` **within a single collection**.
    ///
    /// Unlike [`VectorStore::delete_by_source`] (which is store-wide), this
    /// only removes vectors tagged with `collection`. The same document stem
    /// can be ingested into several per-agent collections (`kb-{agent_id}`);
    /// deleting it in one must not touch the others.
    async fn delete_by_source_in_collection(
        &self,
        collection: &str,
        source_id: &str,
    ) -> crate::Result<usize>;

    /// Delete all chunks in a collection.
    async fn delete_by_collection(&self, collection: &str) -> crate::Result<usize>;

    /// How many chunks the collection holds.
    ///
    /// Used to skip a search that cannot return anything: an empty knowledge
    /// base still cost a query expansion and an embedding on every turn.
    /// The default is "unknown, do not skip" — a backend that cannot answer
    /// cheaply must lose nothing but the optimisation.
    async fn collection_len(&self, _collection: &str) -> crate::Result<usize> {
        Ok(usize::MAX)
    }

    /// Get stats about the store
    async fn stats(&self) -> crate::Result<VectorStoreStats>;

    /// Clear all data
    async fn clear(&self) -> crate::Result<()>;
}

/// Statistics about the vector store
#[derive(Debug, Clone, Default)]
pub struct VectorStoreStats {
    pub total_vectors: usize,
    pub total_sources: usize,
    pub dimension: usize,
}

/// In-memory vector store (for testing/small datasets)
pub struct MemoryVectorStore {
    chunks: RwLock<HashMap<String, EmbeddedChunk>>,
    dimension: usize,
    /// Maximum number of chunks before oldest entries are evicted.
    max_chunks: usize,
    /// Insertion order for FIFO eviction.
    order: RwLock<VecDeque<String>>,
}

impl MemoryVectorStore {
    /// Create a new in-memory store with the default max of 100,000 chunks.
    pub fn new(dimension: usize) -> Self {
        Self::with_max(dimension, 100_000)
    }

    /// Create a new in-memory store with a specific cap.
    pub fn with_max(dimension: usize, max_chunks: usize) -> Self {
        Self {
            chunks: RwLock::new(HashMap::new()),
            dimension,
            max_chunks,
            order: RwLock::new(VecDeque::new()),
        }
    }
}

#[async_trait]
impl VectorStore for MemoryVectorStore {
    async fn store_chunk(&self, chunk: EmbeddedChunk) -> crate::Result<()> {
        if chunk.embedding.len() != self.dimension {
            return Err(crate::error::SyscityError::Validation(format!(
                "Embedding dimension mismatch: expected {}, got {}",
                self.dimension,
                chunk.embedding.len()
            )));
        }

        let mut chunks = self.chunks.write().await;
        let mut order = self.order.write().await;

        let chunk_id = chunk.id.clone();

        // If the chunk already exists, update in place and move to back.
        if chunks.contains_key(&chunk_id) {
            order.retain(|id| id != &chunk_id);
        }

        chunks.insert(chunk_id.clone(), chunk);
        order.push_back(chunk_id);

        // Evict oldest chunks when over capacity.
        while chunks.len() > self.max_chunks {
            if let Some(oldest_id) = order.pop_front() {
                chunks.remove(&oldest_id);
            } else {
                break;
            }
        }

        Ok(())
    }

    async fn search_similar(
        &self,
        query_embedding: &[f32],
        limit: usize,
        threshold: f32,
        collection: Option<&str>,
    ) -> crate::Result<Vec<(EmbeddedChunk, f32)>> {
        let chunks = self.chunks.read().await;

        let mut results: Vec<(EmbeddedChunk, f32)> = chunks
            .values()
            .filter_map(|chunk| {
                if let Some(coll) = collection {
                    if chunk.collection.as_deref() != Some(coll) {
                        return None;
                    }
                }
                let similarity = cosine_similarity(query_embedding, &chunk.embedding);
                if similarity >= threshold {
                    Some((chunk.clone(), similarity))
                } else {
                    None
                }
            })
            .collect();

        // Sort by similarity (descending)
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);

        Ok(results)
    }

    async fn delete_by_source(&self, source_id: &str) -> crate::Result<usize> {
        let mut chunks = self.chunks.write().await;
        let to_remove: Vec<String> = chunks
            .values()
            .filter(|c| c.source_id == source_id)
            .map(|c| c.id.clone())
            .collect();

        let count = to_remove.len();
        for id in &to_remove {
            chunks.remove(id);
        }
        // Clean up the FIFO order queue to prevent orphaned entries from accumulating.
        if !to_remove.is_empty() {
            let mut order = self.order.write().await;
            order.retain(|id| !to_remove.contains(id));
        }

        Ok(count)
    }

    async fn delete_by_source_in_collection(
        &self,
        collection: &str,
        source_id: &str,
    ) -> crate::Result<usize> {
        let mut chunks = self.chunks.write().await;
        let to_remove: Vec<String> = chunks
            .values()
            .filter(|c| c.source_id == source_id && c.collection.as_deref() == Some(collection))
            .map(|c| c.id.clone())
            .collect();

        let count = to_remove.len();
        for id in &to_remove {
            chunks.remove(id);
        }
        if !to_remove.is_empty() {
            let mut order = self.order.write().await;
            order.retain(|id| !to_remove.contains(id));
        }

        Ok(count)
    }

    async fn delete_by_collection(&self, collection: &str) -> crate::Result<usize> {
        let mut chunks = self.chunks.write().await;
        let to_remove: Vec<String> = chunks
            .values()
            .filter(|c| c.collection.as_deref() == Some(collection))
            .map(|c| c.id.clone())
            .collect();

        let count = to_remove.len();
        for id in &to_remove {
            chunks.remove(id);
        }
        if !to_remove.is_empty() {
            let mut order = self.order.write().await;
            order.retain(|id| !to_remove.contains(id));
        }

        Ok(count)
    }

    async fn stats(&self) -> crate::Result<VectorStoreStats> {
        let chunks = self.chunks.read().await;
        let sources: std::collections::HashSet<String> =
            chunks.values().map(|c| c.source_id.clone()).collect();

        Ok(VectorStoreStats {
            total_vectors: chunks.len(),
            total_sources: sources.len(),
            dimension: self.dimension,
        })
    }

    async fn collection_len(&self, collection: &str) -> crate::Result<usize> {
        let chunks = self.chunks.read().await;
        Ok(chunks
            .values()
            .filter(|c| c.collection.as_deref() == Some(collection))
            .count())
    }

    async fn clear(&self) -> crate::Result<()> {
        let mut chunks = self.chunks.write().await;
        chunks.clear();
        let mut order = self.order.write().await;
        order.clear();
        Ok(())
    }
}

/// Search result for API responses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: String,
    pub content: String,
    pub score: f32,
    pub metadata: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_vector_store_store_and_search() {
        let store = MemoryVectorStore::new(3);
        let chunk = EmbeddedChunk {
            id: "c1".to_string(),
            source_id: "doc1".to_string(),
            text: "hello world".to_string(),
            embedding: vec![1.0, 0.0, 0.0],
            position: 0,
            total_chunks: 1,
            collection: None,
            metadata: None,
        };
        store.store_chunk(chunk.clone()).await.unwrap();

        let results = store
            .search_similar(&[1.0, 0.0, 0.0], 5, 0.0, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.id, "c1");
        assert!((results[0].1 - 1.0).abs() < 0.001);

        // Orthogonal vector should not match above threshold
        let results = store
            .search_similar(&[0.0, 1.0, 0.0], 5, 0.5, None)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_memory_vector_store_delete_by_source() {
        let store = MemoryVectorStore::new(2);
        store
            .store_chunk(EmbeddedChunk {
                id: "c1".to_string(),
                source_id: "doc-a".to_string(),
                text: "a".to_string(),
                embedding: vec![1.0, 0.0],
                position: 0,
                total_chunks: 2,
                collection: None,
                metadata: None,
            })
            .await
            .unwrap();
        store
            .store_chunk(EmbeddedChunk {
                id: "c2".to_string(),
                source_id: "doc-a".to_string(),
                text: "b".to_string(),
                embedding: vec![0.0, 1.0],
                position: 1,
                total_chunks: 2,
                collection: None,
                metadata: None,
            })
            .await
            .unwrap();
        store
            .store_chunk(EmbeddedChunk {
                id: "c3".to_string(),
                source_id: "doc-b".to_string(),
                text: "c".to_string(),
                embedding: vec![1.0, 1.0],
                position: 0,
                total_chunks: 1,
                collection: None,
                metadata: None,
            })
            .await
            .unwrap();

        let deleted = store.delete_by_source("doc-a").await.unwrap();
        assert_eq!(deleted, 2);

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.total_vectors, 1);
    }

    #[tokio::test]
    async fn test_memory_vector_store_delete_by_source_in_collection() {
        let store = MemoryVectorStore::new(2);
        // Same source stem in two collections + one other doc.
        for (id, coll) in [("c1", Some("kb-a")), ("c2", Some("kb-b")), ("c3", None)] {
            store
                .store_chunk(EmbeddedChunk {
                    id: id.to_string(),
                    source_id: "readme".to_string(),
                    text: "x".to_string(),
                    embedding: vec![1.0, 0.0],
                    position: 0,
                    total_chunks: 1,
                    collection: coll.map(str::to_string),
                    metadata: None,
                })
                .await
                .unwrap();
        }

        // Deleting "readme" in kb-a must leave kb-b's and unscoped chunks.
        let deleted = store
            .delete_by_source_in_collection("kb-a", "readme")
            .await
            .unwrap();
        assert_eq!(deleted, 1);

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.total_vectors, 2);
    }

    #[tokio::test]
    async fn test_memory_vector_store_stats() {
        let store = MemoryVectorStore::new(4);
        store
            .store_chunk(EmbeddedChunk {
                id: "c1".to_string(),
                source_id: "s1".to_string(),
                text: "a".to_string(),
                embedding: vec![0.0; 4],
                position: 0,
                total_chunks: 1,
                collection: None,
                metadata: None,
            })
            .await
            .unwrap();
        store
            .store_chunk(EmbeddedChunk {
                id: "c2".to_string(),
                source_id: "s2".to_string(),
                text: "b".to_string(),
                embedding: vec![0.0; 4],
                position: 0,
                total_chunks: 1,
                collection: None,
                metadata: None,
            })
            .await
            .unwrap();

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.total_vectors, 2);
        assert_eq!(stats.total_sources, 2);
        assert_eq!(stats.dimension, 4);
    }

    #[tokio::test]
    async fn test_memory_vector_store_clear() {
        let store = MemoryVectorStore::new(2);
        store
            .store_chunk(EmbeddedChunk {
                id: "c1".to_string(),
                source_id: "s1".to_string(),
                text: "a".to_string(),
                embedding: vec![1.0, 0.0],
                position: 0,
                total_chunks: 1,
                collection: None,
                metadata: None,
            })
            .await
            .unwrap();

        store.clear().await.unwrap();
        let stats = store.stats().await.unwrap();
        assert_eq!(stats.total_vectors, 0);
    }

    #[test]
    fn test_vector_store_stats_default() {
        let stats = VectorStoreStats::default();
        assert_eq!(stats.total_vectors, 0);
        assert_eq!(stats.total_sources, 0);
        assert_eq!(stats.dimension, 0);
    }

    #[test]
    fn test_search_result_creation() {
        let result = SearchResult {
            id: "r1".to_string(),
            content: "content".to_string(),
            score: 0.95,
            metadata: None,
        };
        assert_eq!(result.id, "r1");
        assert!((result.score - 0.95).abs() < 0.001);
    }
}
