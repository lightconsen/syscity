//! Local GGUF Embedding Implementation using llama.cpp
//!
//! This module provides embedding generation using GGUF format models
//! with HuggingFace Hub auto-download support.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::token::LlamaToken;
use tracing::info;

/// HuggingFace model cache directory
const HF_CACHE_DIR: &str = ".syscity/models";

/// Global backend singleton (llama.cpp requires single backend instance)
static LLAMA_BACKEND: OnceLock<crate::Result<LlamaBackend>> = OnceLock::new();

/// Get or initialize the llama.cpp backend
fn get_backend() -> crate::Result<&'static LlamaBackend> {
    let result = LLAMA_BACKEND.get_or_init(|| {
        LlamaBackend::init().map_err(|e| {
            crate::error::SyscityError::Validation(format!(
                "Failed to init llama.cpp backend: {}",
                e
            ))
        })
    });
    match result {
        Ok(ref backend) => Ok(backend),
        Err(ref e) => Err(crate::error::SyscityError::Validation(format!(
            "Backend initialization failed: {}",
            e
        ))),
    }
}

/// Model source - either local path or HuggingFace Hub reference
#[derive(Debug, Clone)]
pub enum ModelSource {
    /// Local file path
    Local(PathBuf),
    /// HuggingFace Hub model (repo_id, filename)
    HuggingFace { repo_id: String, filename: String },
}

impl ModelSource {
    /// Parse a model source string
    ///
    /// Supports:
    /// - Local paths: `/path/to/model.gguf` or `./model.gguf`
    /// - HF Hub: `hf:repo_id/filename` or `repo_id/filename`
    pub fn parse(source: &str) -> Self {
        if source.starts_with("hf:") {
            let parts = source.strip_prefix("hf:").unwrap_or(source);
            Self::parse_hf_format(parts)
        } else if source.starts_with("https://huggingface.co/") {
            // Parse full HF URL
            let path = source
                .strip_prefix("https://huggingface.co/")
                .unwrap_or(source);
            Self::parse_hf_format(path)
        } else if source.contains('/') && !source.starts_with("./") && !source.starts_with("/") {
            // Ambiguous: could be HF (org/model) or relative path (dir/file.gguf).
            // If the last component has a file extension, treat as local path;
            // otherwise treat as HF reference.
            let parts: Vec<&str> = source.split('/').collect();
            let last = parts.last().copied().unwrap_or("");
            let has_extension =
                last.contains('.') && last.split('.').next_back().is_some_and(|e| !e.is_empty());
            if has_extension && parts.len() == 2 {
                // e.g. "models/file.gguf" → local
                ModelSource::Local(PathBuf::from(source))
            } else {
                // e.g. "org/model" or "org/model/file.gguf" → HF
                Self::parse_hf_format(source)
            }
        } else {
            // Local path
            ModelSource::Local(PathBuf::from(source))
        }
    }

    fn parse_hf_format(s: &str) -> Self {
        let parts: Vec<&str> = s.split('/').collect();
        if parts.len() >= 2 {
            let repo_id = format!("{}/{}", parts[0], parts[1]);
            let filename = if parts.len() > 2 {
                parts[2..].join("/")
            } else {
                "model.gguf".to_string()
            };
            ModelSource::HuggingFace { repo_id, filename }
        } else {
            ModelSource::Local(PathBuf::from(s))
        }
    }

    /// Resolve to a local path, downloading if necessary
    pub async fn resolve(&self) -> crate::Result<PathBuf> {
        match self {
            ModelSource::Local(path) => Ok(path.clone()),
            ModelSource::HuggingFace { repo_id, filename } => {
                download_from_hf(repo_id, filename).await
            }
        }
    }
}

/// Download model from HuggingFace Hub
async fn download_from_hf(repo_id: &str, filename: &str) -> crate::Result<PathBuf> {
    info!("Downloading model from HuggingFace Hub: {}/{}", repo_id, filename);

    let cache_dir = dirs::home_dir()
        .map(|h| h.join(HF_CACHE_DIR))
        .unwrap_or_else(|| PathBuf::from(HF_CACHE_DIR));

    tokio::fs::create_dir_all(&cache_dir)
        .await
        .map_err(crate::error::SyscityError::Io)?;

    // Use hf-hub for download
    let api = hf_hub::api::tokio::Api::new().map_err(|e| {
        crate::error::SyscityError::Validation(format!("Failed to create HF API: {}", e))
    })?;

    let repo = api.model(repo_id.to_string());

    let local_path = repo.get(filename).await.map_err(|e| {
        crate::error::SyscityError::Validation(format!("Failed to download model: {}", e))
    })?;

    info!("Model downloaded to: {:?}", local_path);
    Ok(local_path)
}

/// Lazy-initialized embedding model
pub struct LazyEmbeddingModel {
    source: ModelSource,
    model_name: String,
    dimension: usize,
    /// The actual model (initialized on first use)
    inner: tokio::sync::OnceCell<Arc<EmbeddingModelInner>>,
}

/// Inner model struct (initialized lazily)
struct EmbeddingModelInner {
    model: LlamaModel,
    context_params: LlamaContextParams,
    backend: &'static LlamaBackend,
}

impl LazyEmbeddingModel {
    /// Create a new lazy embedding model
    pub fn new(source: ModelSource, dimension: usize) -> Self {
        let model_name = match &source {
            ModelSource::Local(path) => path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string(),
            ModelSource::HuggingFace { repo_id, filename } => {
                format!("{}/{}", repo_id, filename)
            }
        };

        Self {
            source,
            model_name,
            dimension,
            inner: tokio::sync::OnceCell::new(),
        }
    }

    /// Get or initialize the model
    async fn get_model(&self) -> crate::Result<Arc<EmbeddingModelInner>> {
        self.inner
            .get_or_try_init(|| async {
                let path = self.source.resolve().await?;
                info!("Initializing GGUF model from: {:?}", path);

                let backend = get_backend()?;
                let path = path.clone();
                let model = tokio::task::spawn_blocking(move || {
                    let model_params = LlamaModelParams::default();
                    LlamaModel::load_from_file(backend, &path, &model_params).map_err(|e| {
                        crate::error::SyscityError::Validation(format!(
                            "Failed to load model: {}",
                            e
                        ))
                    })
                })
                .await
                .map_err(|e| {
                    crate::error::SyscityError::Validation(format!(
                        "Model loading task panicked: {}",
                        e
                    ))
                })??;

                let context_params = LlamaContextParams::default()
                    .with_n_batch(512)
                    .with_embeddings(true);

                info!("GGUF model loaded successfully");

                Ok(Arc::new(EmbeddingModelInner { model, context_params, backend }))
            })
            .await
            .cloned()
    }

    /// Generate embeddings for a batch of texts
    pub async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
        let inner = self.get_model().await?;

        let mut embeddings = Vec::with_capacity(texts.len());

        for text in texts {
            let inner = Arc::clone(&inner);
            let text = text.clone();
            let embedding =
                tokio::task::spawn_blocking(move || Self::embed_single_blocking(inner, text))
                    .await
                    .map_err(|e| {
                        crate::error::SyscityError::Validation(format!(
                            "Embedding task panicked: {}",
                            e
                        ))
                    })??;
            embeddings.push(embedding);
        }

        Ok(embeddings)
    }

    fn embed_single_blocking(
        inner: Arc<EmbeddingModelInner>,
        text: String,
    ) -> crate::Result<Vec<f32>> {
        // Tokenize the input
        let tokens = Self::tokenize(&inner, &text);

        // Create a context for inference
        let mut ctx = inner
            .model
            .new_context(inner.backend, inner.context_params.clone())
            .map_err(|e| {
                crate::error::SyscityError::Validation(format!("Failed to create context: {}", e))
            })?;

        // Create batch
        let mut batch = LlamaBatch::new(512, 1);

        // Add tokens to batch
        for (i, token) in tokens.iter().enumerate() {
            let is_last = i == tokens.len() - 1;
            batch.add(*token, i as i32, &[0], is_last).map_err(|e| {
                crate::error::SyscityError::Validation(format!("Failed to add token: {}", e))
            })?;
        }

        // Decode
        ctx.decode(&mut batch)
            .map_err(|e| crate::error::SyscityError::Validation(format!("Decode failed: {}", e)))?;

        // Extract embeddings from the last token
        let embedding = ctx.embeddings_seq_ith(0).map_err(|e| {
            crate::error::SyscityError::Validation(format!("Failed to get embeddings: {}", e))
        })?;

        // Convert to Vec<f32> and normalize
        let mut vec: Vec<f32> = embedding.to_vec();

        // Normalize to unit vector
        let magnitude: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if magnitude > 0.0 {
            vec.iter_mut().for_each(|x| *x /= magnitude);
        }

        Ok(vec)
    }

    /// Tokenize for embedding.
    ///
    /// `add_special` adds BOS when the model is configured for it — what
    /// `AddBos::Always` meant before the API moved onto the vocabulary — and
    /// `parse_special` stays **on**, because that is what the old call passed
    /// underneath; turning it off would silently change how text containing
    /// something like `<|endoftext|>` is split.
    ///
    /// Infallible now: the vocabulary's `tokenize` returns the tokens rather
    /// than a `Result`, so there is no failure left to report.
    fn tokenize(inner: &EmbeddingModelInner, text: &str) -> Vec<LlamaToken> {
        inner.model.vocab().tokenize(text.as_bytes(), true, true)
    }

    /// Get model name
    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    /// Get embedding dimension
    pub fn dimension(&self) -> usize {
        self.dimension
    }

    /// Check if the model is available (locally or cached from HuggingFace Hub)
    pub async fn is_available(&self) -> bool {
        match &self.source {
            ModelSource::Local(path) => path.exists(),
            ModelSource::HuggingFace { repo_id, filename } => {
                // Only check the local cache — do NOT trigger a download.
                let cache_dir = dirs::home_dir()
                    .map(|h| h.join(HF_CACHE_DIR))
                    .unwrap_or_else(|| PathBuf::from(HF_CACHE_DIR));
                // hf-hub stores models under: cache_dir/models--repo_id/filename
                let safe_id = repo_id.replace('/', "--");
                let cached_path = cache_dir
                    .join(format!("models--{}", safe_id))
                    .join(filename);
                cached_path.exists()
            }
        }
    }
}

/// Embedding provider that supports lazy initialization and fallback
#[allow(clippy::large_enum_variant)]
pub enum LocalEmbeddingProvider {
    /// Full GGUF embedding model
    Gguf(LazyEmbeddingModel),
    /// FTS-only mode (no embeddings)
    FtsOnly { reason: String },
}

impl LocalEmbeddingProvider {
    /// Create a new provider with auto-detection and fallback
    pub async fn create(source: ModelSource, dimension: usize) -> Self {
        let model = LazyEmbeddingModel::new(source, dimension);

        // Check if model is available
        if model.is_available().await {
            LocalEmbeddingProvider::Gguf(model)
        } else {
            LocalEmbeddingProvider::FtsOnly {
                reason: "Model not available".to_string(),
            }
        }
    }

    /// Create with explicit FTS-only mode
    pub fn fts_only(reason: impl Into<String>) -> Self {
        LocalEmbeddingProvider::FtsOnly { reason: reason.into() }
    }

    /// Check if this is FTS-only mode
    pub fn is_fts_only(&self) -> bool {
        matches!(self, LocalEmbeddingProvider::FtsOnly { .. })
    }

    /// Get the reason for FTS-only mode
    pub fn fts_reason(&self) -> Option<&str> {
        match self {
            LocalEmbeddingProvider::FtsOnly { reason } => Some(reason),
            _ => None,
        }
    }

    /// Generate embeddings (or return error if FTS-only)
    pub async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
        match self {
            LocalEmbeddingProvider::Gguf(model) => model.embed_batch(texts).await,
            LocalEmbeddingProvider::FtsOnly { reason } => {
                Err(crate::error::SyscityError::Validation(format!(
                    "Embeddings unavailable (FTS-only mode): {}. To enable embeddings, configure \
                     a valid embedding model.",
                    reason
                )))
            }
        }
    }

    /// Get model name (or "fts-only" for fallback mode)
    pub fn model_name(&self) -> &str {
        match self {
            LocalEmbeddingProvider::Gguf(model) => model.model_name(),
            LocalEmbeddingProvider::FtsOnly { .. } => "fts-only",
        }
    }

    /// Get dimension (or 0 for FTS-only)
    pub fn dimension(&self) -> usize {
        match self {
            LocalEmbeddingProvider::Gguf(model) => model.dimension(),
            LocalEmbeddingProvider::FtsOnly { .. } => 0,
        }
    }
}

// Import the EmbeddingProvider trait
use super::EmbeddingProvider;

#[async_trait::async_trait]
impl EmbeddingProvider for LocalEmbeddingProvider {
    fn model_name(&self) -> &str {
        self.model_name()
    }

    fn dimension(&self) -> usize {
        self.dimension()
    }

    async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
        self.embed_batch(texts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_source_parse_local() {
        let source = ModelSource::parse("/path/to/model.gguf");
        assert!(matches!(source, ModelSource::Local(_)));
    }

    #[test]
    fn test_model_source_parse_hf() {
        let source = ModelSource::parse("hf:org/model/file.gguf");
        assert!(matches!(source, ModelSource::HuggingFace { .. }));
    }

    #[test]
    fn test_model_source_parse_hf_shorthand() {
        let source = ModelSource::parse("org/model/file.gguf");
        assert!(matches!(source, ModelSource::HuggingFace { .. }));
    }

    #[test]
    fn test_model_source_parse_hf_url() {
        let source = ModelSource::parse("https://huggingface.co/org/model/file.gguf");
        assert!(
            matches!(source, ModelSource::HuggingFace { repo_id, filename } if repo_id == "org/model" && filename == "file.gguf")
        );
    }

    #[test]
    fn test_model_source_parse_local_relative() {
        let source = ModelSource::parse("./model.gguf");
        assert!(
            matches!(source, ModelSource::Local(path) if path.to_string_lossy() == "./model.gguf")
        );
    }

    #[test]
    fn test_model_source_parse_hf_default_filename() {
        let source = ModelSource::parse("org/model");
        assert!(
            matches!(source, ModelSource::HuggingFace { repo_id, filename } if repo_id == "org/model" && filename == "model.gguf")
        );
    }

    #[test]
    fn test_lazy_embedding_model_name_local() {
        let model =
            LazyEmbeddingModel::new(ModelSource::Local(PathBuf::from("/models/bge.gguf")), 384);
        assert_eq!(model.model_name(), "bge.gguf");
        assert_eq!(model.dimension(), 384);
    }

    #[test]
    fn test_lazy_embedding_model_name_hf() {
        let model = LazyEmbeddingModel::new(
            ModelSource::HuggingFace {
                repo_id: "org/model".to_string(),
                filename: "file.gguf".to_string(),
            },
            768,
        );
        assert_eq!(model.model_name(), "org/model/file.gguf");
        assert_eq!(model.dimension(), 768);
    }

    #[tokio::test]
    async fn test_model_source_resolve_local() {
        let source = ModelSource::Local(PathBuf::from("/path/to/model.gguf"));
        let resolved = source.resolve().await.unwrap();
        assert_eq!(resolved, PathBuf::from("/path/to/model.gguf"));
    }

    #[tokio::test]
    async fn test_local_embedding_provider_creates_fts_only_for_missing_model() {
        let source = ModelSource::Local(PathBuf::from("/definitely/not/a/real/model.gguf"));
        let provider = LocalEmbeddingProvider::create(source, 384).await;
        assert!(provider.is_fts_only());
        assert!(provider.fts_reason().is_some());
        assert_eq!(provider.dimension(), 0);
        assert_eq!(provider.model_name(), "fts-only");
    }

    #[test]
    fn test_local_embedding_provider_fts_only() {
        let provider = LocalEmbeddingProvider::fts_only("no model");
        assert!(provider.is_fts_only());
        assert_eq!(provider.fts_reason(), Some("no model"));
        assert_eq!(provider.model_name(), "fts-only");
        assert_eq!(provider.dimension(), 0);
    }

    #[test]
    fn test_local_embedding_provider_not_fts_only() {
        let model =
            LazyEmbeddingModel::new(ModelSource::Local(PathBuf::from("/tmp/model.gguf")), 128);
        let provider = LocalEmbeddingProvider::Gguf(model);
        assert!(!provider.is_fts_only());
        assert_eq!(provider.fts_reason(), None);
    }
}
