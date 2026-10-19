//! Embedding provider traits and implementations.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Trait for embedding providers
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Get the model name
    fn model_name(&self) -> &str;

    /// Get the embedding dimension
    fn dimension(&self) -> usize;

    /// Generate embeddings for texts (batch)
    async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>>;

    /// Generate embedding for single text
    async fn embed(&self, text: &str) -> crate::Result<Vec<f32>> {
        let mut results = self.embed_batch(&[text.to_string()]).await?;
        Ok(results.pop().unwrap_or_default())
    }
}

/// Blanket impl so `Arc<dyn EmbeddingProvider>` can be passed where a concrete
/// `EmbeddingProvider` is expected (e.g. as the inner of
/// `CachedEmbeddingProvider`).
#[async_trait]
impl EmbeddingProvider for Arc<dyn EmbeddingProvider> {
    fn model_name(&self) -> &str {
        (**self).model_name()
    }

    fn dimension(&self) -> usize {
        (**self).dimension()
    }

    async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
        (**self).embed_batch(texts).await
    }
}

/// API-based embedding provider (OpenAI, etc.)
pub struct ApiEmbeddingProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    dimension: usize,
}

/// Re-export the LocalEmbeddingProvider from local_embeddings module
#[cfg(feature = "local-embeddings")]
pub use super::local_embeddings::LocalEmbeddingProvider as LocalGgufEmbeddingProvider;

/// Stub when local-embeddings feature is disabled
#[cfg(not(feature = "local-embeddings"))]
pub struct LocalGgufEmbeddingProvider;

#[cfg(not(feature = "local-embeddings"))]
impl LocalGgufEmbeddingProvider {
    /// Create stub
    pub async fn create(_source: (), _dimension: usize) -> Self {
        Self
    }

    /// FTS-only stub
    pub fn fts_only(_reason: impl Into<String>) -> Self {
        Self
    }

    /// Always returns true for stub
    pub fn is_fts_only(&self) -> bool {
        true
    }

    /// Returns the reason for FTS-only mode
    pub fn fts_reason(&self) -> Option<&str> {
        Some("'local-embeddings' feature not enabled")
    }

    /// Always returns error
    pub async fn embed_batch(&self, _texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
        Err(crate::error::SyscityError::Validation(
            "Local GGUF embeddings require 'local-embeddings' feature. Install with: cargo build \
             --features local-embeddings"
                .to_string(),
        ))
    }

    /// Returns stub name
    pub fn model_name(&self) -> &str {
        "disabled"
    }

    /// Returns 0
    pub fn dimension(&self) -> usize {
        0
    }
}

#[cfg(not(feature = "local-embeddings"))]
#[async_trait]
impl EmbeddingProvider for LocalGgufEmbeddingProvider {
    fn model_name(&self) -> &str {
        self.model_name()
    }

    fn dimension(&self) -> usize {
        self.dimension()
    }

    async fn embed_batch(&self, _texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
        self.embed_batch(_texts).await
    }
}

impl ApiEmbeddingProvider {
    /// Create a new API embedding provider
    pub fn new(api_key: String, model: String, dimension: usize) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| {
                // A reqwest client only fails to build if the TLS backend or a
                // user-provided connector is invalid; fallback to the default
                // client so the provider is still usable.
                reqwest::Client::new()
            });
        Self {
            client,
            api_key,
            base_url: "https://api.openai.com/v1".to_string(),
            model,
            dimension,
        }
    }

    /// Set custom base URL (for Azure, etc.)
    pub fn with_base_url(mut self, url: String) -> Self {
        self.base_url = url;
        self
    }
}

#[async_trait]
impl EmbeddingProvider for ApiEmbeddingProvider {
    fn model_name(&self) -> &str {
        &self.model
    }

    fn dimension(&self) -> usize {
        self.dimension
    }

    async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
        #[derive(Debug, Serialize)]
        struct Request {
            model: String,
            input: Vec<String>,
        }

        #[derive(Debug, Deserialize)]
        struct EmbeddingResponse {
            data: Vec<EmbeddingData>,
        }

        #[derive(Debug, Deserialize)]
        struct EmbeddingData {
            embedding: Vec<f32>,
            index: usize,
        }

        let request = Request {
            model: self.model.clone(),
            input: texts.to_vec(),
        };

        let response: EmbeddingResponse = self
            .client
            .post(format!("{}/embeddings", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&request)
            .send()
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: "Embedding API request failed".to_string(),
                cause: Some(Box::new(e)),
            })?
            .json()
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: "Invalid embedding response".to_string(),
                cause: Some(Box::new(e)),
            })?;

        // Sort by index to maintain order
        let mut embeddings: Vec<(usize, Vec<f32>)> = response
            .data
            .into_iter()
            .map(|d| (d.index, d.embedding))
            .collect();
        embeddings.sort_by_key(|(idx, _)| *idx);

        Ok(embeddings.into_iter().map(|(_, emb)| emb).collect())
    }
}

// ── Embedding dedup cache
// ─────────────────────────────────────────────────────

/// In-memory SHA-256 content-dedup cache for embedding vectors.
///
/// Wraps any [`EmbeddingProvider`] and skips API calls for texts whose SHA-256
/// hash has already been cached. The cache is bounded to `max_entries`; when
/// full, the oldest inserted entry is evicted (simple FIFO).
pub struct CachedEmbeddingProvider<P: EmbeddingProvider> {
    inner: P,
    /// SHA-256 hex → embedding vector.
    cache: RwLock<HashMap<String, Vec<f32>>>,
    /// Insertion-order keys for FIFO eviction.
    order: RwLock<VecDeque<String>>,
    max_entries: usize,
}

impl<P: EmbeddingProvider> CachedEmbeddingProvider<P> {
    /// Wrap `provider` with a FIFO dedup cache capped at `max_entries`.
    pub fn new(provider: P, max_entries: usize) -> Self {
        Self {
            inner: provider,
            cache: RwLock::new(HashMap::new()),
            order: RwLock::new(VecDeque::new()),
            max_entries,
        }
    }

    /// SHA-256 hex digest of `text` used as the cache key.
    fn sha256_key(text: &str) -> String {
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(text.as_bytes());
        hex::encode(hash)
    }

    /// Current number of cached entries.
    pub async fn cache_size(&self) -> usize {
        self.cache.read().await.len()
    }

    /// Remove all cached entries.
    pub async fn clear_cache(&self) {
        self.cache.write().await.clear();
        self.order.write().await.clear();
    }
}

#[async_trait]
impl<P: EmbeddingProvider + Send + Sync> EmbeddingProvider for CachedEmbeddingProvider<P> {
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn dimension(&self) -> usize {
        self.inner.dimension()
    }

    async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
        let mut result: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        let mut miss_indices: Vec<usize> = Vec::new();
        let mut miss_texts: Vec<String> = Vec::new();

        // Cache-hit pass.
        {
            let cache = self.cache.read().await;
            for (i, text) in texts.iter().enumerate() {
                let key = Self::sha256_key(text);
                if let Some(emb) = cache.get(&key) {
                    result[i] = Some(emb.clone());
                } else {
                    miss_indices.push(i);
                    miss_texts.push(text.clone());
                }
            }
        }

        if miss_texts.is_empty() {
            return Ok(result.into_iter().flatten().collect());
        }

        // Fetch missing embeddings from the inner provider.
        let fetched = self.inner.embed_batch(&miss_texts).await?;

        // Store fetched embeddings in cache, evicting oldest if full.
        {
            let mut cache = self.cache.write().await;
            let mut order = self.order.write().await;

            for (text, embedding) in miss_texts.iter().zip(fetched.iter()) {
                let key = Self::sha256_key(text);
                if !cache.contains_key(&key) {
                    // Evict oldest if at capacity.
                    if cache.len() >= self.max_entries {
                        if let Some(oldest) = order.pop_front() {
                            cache.remove(&oldest);
                        }
                    }
                    cache.insert(key.clone(), embedding.clone());
                    order.push_back(key);
                }
            }
        }

        // Merge fetched embeddings back into result.
        if fetched.len() != miss_indices.len() {
            return Err(crate::error::SyscityError::Validation(format!(
                "Embedding provider returned {} embeddings for {} texts",
                fetched.len(),
                miss_indices.len()
            )));
        }
        for (local_idx, global_idx) in miss_indices.into_iter().enumerate() {
            result[global_idx] = Some(fetched[local_idx].clone());
        }

        Ok(result.into_iter().flatten().collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    /// Minimal stub that counts embed_batch calls.
    struct CountingProvider {
        calls: Arc<AtomicUsize>,
        dim: usize,
    }

    #[async_trait]
    impl EmbeddingProvider for CountingProvider {
        fn model_name(&self) -> &str {
            "stub"
        }
        fn dimension(&self) -> usize {
            self.dim
        }
        async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(texts.iter().map(|_| vec![0.0_f32; self.dim]).collect())
        }
    }

    #[tokio::test]
    async fn test_cached_embedding_hit() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider { calls: calls.clone(), dim: 4 };
        let cached = CachedEmbeddingProvider::new(provider, 100);

        let texts = vec!["hello world".to_string()];
        let _ = cached.embed_batch(&texts).await.unwrap();
        let _ = cached.embed_batch(&texts).await.unwrap();

        // Second call should be served from cache → only 1 actual call to inner.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cached.cache_size().await, 1);
    }

    #[tokio::test]
    async fn test_cached_embedding_miss_different_text() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider { calls: calls.clone(), dim: 4 };
        let cached = CachedEmbeddingProvider::new(provider, 100);

        let _ = cached.embed_batch(&["text_a".to_string()]).await.unwrap();
        let _ = cached.embed_batch(&["text_b".to_string()]).await.unwrap();

        // Each unique text is a cache miss.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(cached.cache_size().await, 2);
    }

    #[tokio::test]
    async fn test_cached_embedding_eviction() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider { calls: calls.clone(), dim: 2 };
        let cached = CachedEmbeddingProvider::new(provider, 2); // cap = 2

        let _ = cached.embed_batch(&["a".to_string()]).await.unwrap();
        let _ = cached.embed_batch(&["b".to_string()]).await.unwrap();
        // Full: inserting "c" should evict "a".
        let _ = cached.embed_batch(&["c".to_string()]).await.unwrap();

        assert_eq!(cached.cache_size().await, 2);
    }

    #[tokio::test]
    async fn test_cached_embedding_clear() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = CountingProvider { calls: calls.clone(), dim: 2 };
        let cached = CachedEmbeddingProvider::new(provider, 100);

        let _ = cached.embed_batch(&["hello".to_string()]).await.unwrap();
        assert_eq!(cached.cache_size().await, 1);

        cached.clear_cache().await;
        assert_eq!(cached.cache_size().await, 0);
    }

    #[test]
    fn test_api_embedding_provider_new() {
        let provider =
            ApiEmbeddingProvider::new("key123".into(), "text-embedding-3-small".into(), 1536);
        assert_eq!(provider.model_name(), "text-embedding-3-small");
        assert_eq!(provider.dimension(), 1536);
    }

    #[test]
    fn test_api_embedding_provider_with_base_url() {
        let provider = ApiEmbeddingProvider::new("k".into(), "m".into(), 128)
            .with_base_url("https://azure.example.com".to_string());
        // base_url is private, but we can verify the struct was created
        assert_eq!(provider.model_name(), "m");
    }
}
