//! Vector Memory Service — memory-domain bridge over the generic RAG
//! infrastructure.
//!
//! Wraps [`crate::rag::EmbeddingProvider`] and [`crate::rag::VectorStore`] to
//! provide a memory-domain API that operates on [`Memory`] and [`MemoryId`]
//! types.

use std::sync::Arc;

use super::{Memory, MemoryId};
use crate::providers::Provider;
use crate::rag::chunk::{BatchEmbeddingProcessor, EmbeddedChunk, TextChunker};
use crate::rag::config::EmbeddingConfig;
use crate::rag::embedding::EmbeddingProvider;
use crate::rag::multi_query::{expand_query_with_llm, MultiQueryConfig};
use crate::rag::query::{NoopTransformer, QueryTransformer};
use crate::rag::reranker::{NoopReranker, Reranker};
use crate::rag::vector_store::{VectorStore, VectorStoreStats};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// High-level vector memory service
pub struct VectorMemoryService {
    embedding_provider: Arc<dyn EmbeddingProvider>,
    vector_store: Arc<dyn VectorStore>,
    chunker: TextChunker,
    /// Reserved for future batched re-indexing. Currently unused.
    _batch_processor: BatchEmbeddingProcessor,
    /// Tracks the set of collections that have been written to
    collections: tokio::sync::RwLock<std::collections::HashSet<String>>,
    /// Optional query transformer for rewriting before embedding.
    query_transformer: Arc<dyn QueryTransformer>,
    /// Optional reranker for cross-encoder re-scoring.
    reranker: Arc<dyn Reranker>,
    /// Optional Multi-Query expansion provider, model override and config.
    multi_query_provider: Option<Arc<dyn Provider>>,
    multi_query_model: Option<String>,
    multi_query_config: Option<MultiQueryConfig>,
}

impl VectorMemoryService {
    /// Create a new vector memory service
    pub fn new(
        embedding_provider: Arc<dyn EmbeddingProvider>,
        vector_store: Arc<dyn VectorStore>,
        config: &EmbeddingConfig,
    ) -> Self {
        let chunker = TextChunker::new(config.chunk_size, config.chunk_overlap);
        let batch_processor = BatchEmbeddingProcessor::new(
            embedding_provider.clone(),
            chunker.clone(),
            config.batch_size,
        );

        let mut initial_collections = std::collections::HashSet::new();
        initial_collections.insert("default".to_string());

        Self {
            embedding_provider,
            vector_store,
            chunker,
            _batch_processor: batch_processor,
            collections: tokio::sync::RwLock::new(initial_collections),
            query_transformer: Arc::new(NoopTransformer),
            reranker: Arc::new(NoopReranker),
            multi_query_provider: None,
            multi_query_model: None,
            multi_query_config: None,
        }
    }

    /// Attach a query transformer for rewriting queries before embedding.
    pub fn with_query_transformer(mut self, transformer: Arc<dyn QueryTransformer>) -> Self {
        self.query_transformer = transformer;
        self
    }

    /// Attach a reranker for cross-encoder re-scoring after initial retrieval.
    pub fn with_reranker(mut self, reranker: Arc<dyn Reranker>) -> Self {
        self.reranker = reranker;
        self
    }

    /// Attach a Multi-Query expansion provider. `model` overrides the
    /// provider's default model for the expansion call (`None` = default).
    pub fn with_multi_query(
        mut self,
        provider: Arc<dyn Provider>,
        model: Option<String>,
        config: MultiQueryConfig,
    ) -> Self {
        self.multi_query_provider = Some(provider);
        self.multi_query_model = model;
        self.multi_query_config = Some(config);
        self
    }

    /// Get the current reranker.
    pub fn reranker(&self) -> &dyn Reranker {
        &*self.reranker
    }

    /// Store a memory with automatic chunking and embedding
    pub async fn store_memory(&self, memory: &Memory) -> crate::Result<Vec<EmbeddedChunk>> {
        let chunks = self.chunker.chunk_async(&memory.content).await?;
        let total = chunks.len();

        let mut embedded_chunks = Vec::new();
        let embeddings = self.embedding_provider.embed_batch(&chunks).await?;

        for (pos, (text, embedding)) in chunks.into_iter().zip(embeddings).enumerate() {
            embedded_chunks.push(EmbeddedChunk {
                id: format!("{}-{}", memory.id, pos),
                source_id: memory.id.to_string(),
                text,
                embedding,
                position: pos,
                total_chunks: total,
                collection: None,
                metadata: memory.metadata.clone(),
            });
        }

        self.vector_store
            .store_chunks(embedded_chunks.clone())
            .await?;

        Ok(embedded_chunks)
    }

    /// Search memories semantically
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        threshold: f32,
    ) -> crate::Result<Vec<(EmbeddedChunk, f32)>> {
        let sub_queries = self.expand(query).await;
        self.search_expanded(&sub_queries, limit, threshold, None)
            .await
    }

    /// Expand `query` into the sub-queries to search, original first.
    ///
    /// Split out of [`Self::search`] so a caller that queries several scopes
    /// in one turn (global memory *and* a knowledge-base collection) pays for
    /// the LLM expansion once instead of once per scope. Expansion is an
    /// enhancement: a failed or disabled call degrades to the original query
    /// rather than failing the search.
    pub async fn expand(&self, query: &str) -> Vec<String> {
        let Some(ref mq_config) = self.multi_query_config else {
            return vec![query.to_string()];
        };
        if !mq_config.enabled || mq_config.num_variations == 0 {
            return vec![query.to_string()];
        }
        let Some(ref provider) = self.multi_query_provider else {
            return vec![query.to_string()];
        };

        match expand_query_with_llm(
            query,
            mq_config.num_variations,
            provider.as_ref(),
            self.multi_query_model.as_deref(),
        )
        .await
        {
            Ok(q) => q,
            Err(e) => {
                warn!("Multi-Query expansion failed, falling back to the original query: {}", e);
                vec![query.to_string()]
            }
        }
    }

    /// Run the searches for already-expanded sub-queries and RRF-merge them.
    ///
    /// `collection` scopes every sub-query to one collection; `None` searches
    /// the global memory scope.
    pub async fn search_expanded(
        &self,
        sub_queries: &[String],
        limit: usize,
        threshold: f32,
        collection: Option<&str>,
    ) -> crate::Result<Vec<(EmbeddedChunk, f32)>> {
        // Each sub-query goes through QueryTransformer (HyDE) → Embed → Search
        let mut per_query = Vec::with_capacity(sub_queries.len());
        for sub_q in sub_queries {
            let rewritten = self.query_transformer.transform(sub_q).await?;
            let query_embedding = self.embedding_provider.embed(&rewritten).await?;
            let results = self
                .vector_store
                .search_similar(&query_embedding, limit, threshold, collection)
                .await?;
            per_query.push(results);
        }

        if per_query.len() == 1 {
            // Single-query path (multi-query disabled, or expansion degraded):
            // RRF over one list would only renumber it.
            return Ok(per_query.pop().unwrap_or_default());
        }

        let Some(ref mq_config) = self.multi_query_config else {
            return Ok(per_query.into_iter().next().unwrap_or_default());
        };
        let crate::rag::multi_query::MergeStrategy::Rrf(rrf_config) = &mq_config.merge_strategy;

        Ok(crate::rag::multi_query::merge_results(
            per_query,
            rrf_config,
            limit,
            |chunk: &EmbeddedChunk| chunk.id.clone(),
        ))
    }

    /// Delete memory embeddings
    pub async fn delete_memory(&self, memory_id: &MemoryId) -> crate::Result<usize> {
        self.vector_store
            .delete_by_source(&memory_id.to_string())
            .await
    }

    /// Get stats
    pub async fn stats(&self) -> crate::Result<VectorStoreStats> {
        self.vector_store.stats().await
    }

    /// How many chunks `collection` holds; see [`VectorStore::collection_len`].
    pub async fn collection_len(&self, collection: &str) -> crate::Result<usize> {
        self.vector_store.collection_len(collection).await
    }

    /// Search memories in a specific collection (simplified API for gateway)
    pub async fn search_collection(
        &self,
        query: &str,
        limit: usize,
        collection: &str,
        threshold: f32,
    ) -> crate::Result<Vec<SearchResult>> {
        let sub_queries = self.expand(query).await;
        self.search_collection_expanded(&sub_queries, limit, collection, threshold)
            .await
    }

    /// [`Self::search_collection`] with the expansion already done — see
    /// [`Self::search_expanded`].
    pub async fn search_collection_expanded(
        &self,
        sub_queries: &[String],
        limit: usize,
        collection: &str,
        threshold: f32,
    ) -> crate::Result<Vec<SearchResult>> {
        let results = self
            .search_expanded(sub_queries, limit, threshold, Some(collection))
            .await?;
        Ok(results
            .into_iter()
            .map(|(chunk, score)| SearchResult {
                id: chunk.id,
                content: chunk.text,
                score,
                metadata: chunk.metadata,
            })
            .collect())
    }

    /// Add content to a collection (simplified API for gateway)
    pub async fn add_to_collection(
        &self,
        content: &str,
        metadata: Option<serde_json::Value>,
        collection: &str,
    ) -> crate::Result<String> {
        let doc_id = uuid::Uuid::new_v4().to_string();
        let chunks = self.chunker.chunk_async(content).await?;
        let total = chunks.len();

        let embeddings = self.embedding_provider.embed_batch(&chunks).await?;

        let embedded_chunks: Vec<EmbeddedChunk> = chunks
            .into_iter()
            .zip(embeddings)
            .enumerate()
            .map(|(pos, (text, embedding))| EmbeddedChunk {
                id: format!("{}-{}-{}", doc_id, collection, pos),
                source_id: doc_id.clone(),
                text,
                embedding,
                position: pos,
                total_chunks: total,
                collection: Some(collection.to_string()),
                metadata: metadata.clone(),
            })
            .collect();

        self.vector_store.store_chunks(embedded_chunks).await?;

        // Record the collection name so list_collections() returns it
        self.collections
            .write()
            .await
            .insert(collection.to_string());

        Ok(doc_id)
    }

    /// List available collections
    pub async fn list_collections(&self) -> Vec<String> {
        let cols = self.collections.read().await;
        let mut v: Vec<String> = cols.iter().cloned().collect();
        v.sort();
        v
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
    use async_trait::async_trait;

    use super::*;
    use crate::rag::embedding::EmbeddingProvider;
    use crate::rag::vector_store::MemoryVectorStore;

    struct FixedEmbeddingProvider;
    #[async_trait]
    impl EmbeddingProvider for FixedEmbeddingProvider {
        fn model_name(&self) -> &str {
            "fixed"
        }
        fn dimension(&self) -> usize {
            2
        }
        async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| vec![t.len() as f32, 0.0]).collect())
        }
    }

    #[tokio::test]
    async fn test_vector_memory_service_search_collection() -> crate::Result<()> {
        let provider = Arc::new(FixedEmbeddingProvider) as Arc<dyn EmbeddingProvider>;
        let store = Arc::new(MemoryVectorStore::new(2)) as Arc<dyn VectorStore>;
        let config = EmbeddingConfig::default();
        let service = VectorMemoryService::new(provider, store, &config);

        let doc_id = service
            .add_to_collection("hello world", None, "test-col")
            .await?;
        assert!(!doc_id.is_empty());

        let collections = service.list_collections().await;
        assert!(collections.contains(&"test-col".to_string()));
        assert!(collections.contains(&"default".to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn test_vector_memory_service_search_collection_respects_collection() -> crate::Result<()>
    {
        let provider = Arc::new(FixedEmbeddingProvider) as Arc<dyn EmbeddingProvider>;
        let store = Arc::new(MemoryVectorStore::new(2)) as Arc<dyn VectorStore>;
        let config = EmbeddingConfig::default();
        let service = VectorMemoryService::new(provider, store, &config);

        service
            .add_to_collection("alpha content", None, "col-a")
            .await?;
        service
            .add_to_collection("beta content", None, "col-b")
            .await?;

        let results = service
            .search_collection("alpha content", 10, "col-a", 0.5)
            .await?;

        assert!(!results.is_empty(), "querying col-a should return results");
        assert!(
            results.iter().all(|r| r.id.contains("-col-a-")),
            "all results must belong to the requested collection"
        );
        assert!(
            results.iter().all(|r| !r.id.contains("-col-b-")),
            "results must not leak from another collection"
        );

        let results_b = service
            .search_collection("beta content", 10, "col-b", 0.5)
            .await?;
        assert!(!results_b.is_empty());
        assert!(results_b.iter().all(|r| r.id.contains("-col-b-")));
        Ok(())
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

    /// A service with no multi-query provider must pass the query through
    /// unchanged, and an empty collection must report itself as empty so a
    /// caller can skip the search.
    #[tokio::test]
    async fn test_expand_without_multi_query_is_identity_and_empty_collection_len(
    ) -> crate::Result<()> {
        let provider = Arc::new(FixedEmbeddingProvider) as Arc<dyn EmbeddingProvider>;
        let store = Arc::new(MemoryVectorStore::new(2)) as Arc<dyn VectorStore>;
        let config = EmbeddingConfig::default();
        let service = VectorMemoryService::new(provider, store, &config);

        assert_eq!(service.expand("hello").await, vec!["hello".to_string()]);

        assert_eq!(service.collection_len("never-written").await?, 0);
        service.add_to_collection("x", None, "col-a").await?;
        assert!(service.collection_len("col-a").await? > 0);
        Ok(())
    }
}
