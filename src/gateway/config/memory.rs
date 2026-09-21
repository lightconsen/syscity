//! Knowledge-base, search, embedding and vector-memory configuration.

use super::*;
/// Knowledge Base auto-ingest configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgeBaseConfig {
    /// Auto-ingest stale/new KB documents on daemon startup.
    pub auto_ingest_on_startup: bool,
    /// Max concurrency for ingestion (default: 2).
    pub max_concurrent_ingests: usize,
}

impl Default for KnowledgeBaseConfig {
    fn default() -> Self {
        Self {
            auto_ingest_on_startup: false,
            max_concurrent_ingests: 2,
        }
    }
}

/// Default search provider name
fn default_search_provider() -> String {
    "duckduckgo".to_string()
}

/// Default provider API keys map
fn default_search_keys() -> std::collections::HashMap<String, String> {
    std::collections::HashMap::new()
}

/// Default ordered list of search providers for fallback.
fn default_search_providers() -> Vec<String> {
    vec![default_search_provider()]
}

/// Web search provider configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchConfig {
    /// Legacy single search provider name.
    /// Use `providers` for fallback ordering.
    #[serde(default = "default_search_provider")]
    pub provider: String,
    /// Ordered list of search providers to try.
    /// When empty, falls back to `[provider]`.
    #[serde(default = "default_search_providers")]
    pub providers: Vec<String>,
    /// Legacy single API key field.
    #[serde(default)]
    pub api_key: String,
    /// Per-provider API keys. Allows configuring multiple providers at once.
    /// The active provider uses the key from `keys[provider]` or falls back to
    /// `api_key`.
    #[serde(default = "default_search_keys")]
    pub keys: std::collections::HashMap<String, String>,
}

impl SearchConfig {
    /// Return the ordered list of provider names to try.
    /// Prefers `providers`; when empty, uses `[provider]`.
    pub fn provider_list(&self) -> Vec<String> {
        if self.providers.is_empty() {
            vec![self.provider.clone()]
        } else {
            self.providers.clone()
        }
    }

    /// Get the API key for a given provider name.
    /// Prefers `keys[provider]`, then the legacy `api_key`.
    pub fn api_key_for(&self, provider: &str) -> Option<String> {
        self.keys
            .get(provider)
            .cloned()
            .filter(|k| !k.is_empty())
            .or_else(|| self.api_key.clone().into())
            .filter(|k| !k.is_empty())
    }

    /// Resolve the configured provider list into provider instances.
    ///
    /// Unknown names are skipped with a warning; an empty result falls back
    /// to DuckDuckGo so the search tool is never left provider-less.
    pub fn to_providers(&self) -> Vec<crate::tools::web::SearchProvider> {
        let mut providers: Vec<crate::tools::web::SearchProvider> = self
            .provider_list()
            .iter()
            .filter_map(|name| {
                match crate::tools::web::SearchProvider::from_config_name(
                    name,
                    self.api_key_for(name),
                ) {
                    Some(p) => Some(p),
                    None => {
                        tracing::warn!("Unknown search provider '{}', skipping", name);
                        None
                    }
                }
            })
            .collect();
        if providers.is_empty() {
            providers.push(crate::tools::web::SearchProvider::DuckDuckGo);
        }
        providers
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            provider: default_search_provider(),
            providers: default_search_providers(),
            api_key: String::new(),
            keys: default_search_keys(),
        }
    }
}

/// Embedding provider type
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum EmbeddingProviderType {
    /// OpenAI API (requires API key)
    #[default]
    OpenAi,
    /// Local GGUF model (direct loading, no external service)
    LocalGguf,
}

/// Vector memory configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorMemoryConfig {
    /// Enable vector memory / semantic search
    pub enabled: bool,
    /// Embedding provider type
    pub provider: EmbeddingProviderType,
    /// Embedding provider API key (e.g., OpenAI)
    pub embedding_api_key: Option<String>,
    /// Embedding model to use (for API providers)
    pub embedding_model: String,
    /// Embedding dimension
    pub embedding_dimension: usize,
    /// API base URL (for Azure, etc.)
    pub api_base_url: Option<String>,
    /// Local GGUF model path (for local-embeddings feature)
    pub local_model_path: Option<String>,
    /// Query transformer configuration (HyDE, etc.)
    #[serde(default)]
    pub query_transformer: QueryTransformerConfig,
    /// Cross-encoder reranker configuration
    #[serde(default)]
    pub reranker: RerankerConfig,
    /// Context-window-aware memory budgeting
    #[serde(default)]
    pub context_window: MemoryContextWindowConfig,
    /// Multi-Query expansion configuration
    #[serde(default)]
    pub multi_query: MultiQueryConfig,
    /// Embedding hyper-parameters (chunking / batching). Configurable so the
    /// harness can tune retrieval quality at runtime without code changes.
    #[serde(default)]
    pub embedding: EmbeddingParams,
}

impl Default for VectorMemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false, // Disabled by default to avoid blocking on model download
            provider: EmbeddingProviderType::LocalGguf,
            embedding_api_key: None,
            embedding_model: "text-embedding-3-small".to_string(),
            embedding_dimension: 1536,
            api_base_url: None,
            local_model_path: Some(
                "hf:unsloth/embedding-gemma-2b-GGUF/embedding-gemma-2b-Q4_K_M.gguf".to_string(),
            ),
            query_transformer: QueryTransformerConfig::default(),
            reranker: RerankerConfig::default(),
            context_window: MemoryContextWindowConfig::default(),
            multi_query: MultiQueryConfig::default(),
            embedding: EmbeddingParams::default(),
        }
    }
}

/// Embedding hyper-parameters used by the vector-memory / knowledge-base
/// chunking pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingParams {
    /// Maximum chunk size for text splitting.
    #[serde(default = "default_embedding_chunk_size")]
    pub chunk_size: usize,
    /// Chunk overlap for sliding window (only used when `chunk_strategy` is
    /// `Fixed`).
    #[serde(default = "default_embedding_chunk_overlap")]
    pub chunk_overlap: usize,
    /// Batch size for embedding generation.
    #[serde(default = "default_embedding_batch_size")]
    pub batch_size: usize,
    /// Chunking strategy (defaults to the rag default: recursive 512).
    #[serde(default)]
    pub chunk_strategy: crate::rag::ChunkStrategy,
}

fn default_embedding_chunk_size() -> usize {
    512
}
fn default_embedding_chunk_overlap() -> usize {
    50
}
fn default_embedding_batch_size() -> usize {
    32
}

impl Default for EmbeddingParams {
    fn default() -> Self {
        Self {
            chunk_size: default_embedding_chunk_size(),
            chunk_overlap: default_embedding_chunk_overlap(),
            batch_size: default_embedding_batch_size(),
            chunk_strategy: crate::rag::ChunkStrategy::default(),
        }
    }
}

/// Query transformer configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryTransformerConfig {
    /// Enable HyDE (Hypothetical Document Embeddings) using the default LLM.
    pub enable_hyde: bool,
    /// Optional model override for HyDE generation.
    pub hyde_model: Option<String>,
}

/// Multi-Query expansion configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiQueryConfig {
    /// Enable Multi-Query expansion.
    #[serde(default)]
    pub enabled: bool,
    /// Number of LLM-generated sub-queries (not counting the original query).
    #[serde(default = "default_multi_query_variations")]
    pub num_variations: usize,
    /// Provider name for the expansion LLM (a key under `[providers.*]`).
    /// Pin an explicit direct/cheap provider here: unset falls back to the
    /// router's *first registered* provider, which is nondeterministic and
    /// may be a metered one (e.g. cloud), silently billing credits per turn.
    #[serde(default)]
    pub provider: Option<String>,
    /// Model override for the expansion LLM. Unset → the provider's
    /// configured default model.
    #[serde(default)]
    pub model: Option<String>,
}

fn default_multi_query_variations() -> usize {
    3
}

impl Default for MultiQueryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            num_variations: 3,
            provider: None,
            model: None,
        }
    }
}

/// Cross-encoder reranker configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankerConfig {
    /// Enable cross-encoder reranking.
    pub enabled: bool,
    /// Cohere Rerank API key.
    pub api_key: Option<String>,
    /// Model name (e.g. "rerank-english-v3.0").
    pub model: String,
    /// Max results to return after reranking.
    pub top_k: usize,
}

impl Default for RerankerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: None,
            model: "rerank-english-v3.0".to_string(),
            top_k: 10,
        }
    }
}

/// Context-window-aware memory budgeting configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryContextWindowConfig {
    /// Enable token-budget-aware memory filtering.
    pub enabled: bool,
    /// Maximum total tokens the LLM context can hold.
    pub max_tokens: usize,
    /// Tokens reserved for the LLM's response generation.
    pub reserved_for_response: usize,
    /// Minimum number of memories to retain, even if over budget.
    pub min_chunks: usize,
}

impl Default for MemoryContextWindowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_tokens: 128_000,
            reserved_for_response: 4_096,
            min_chunks: 1,
        }
    }
}
