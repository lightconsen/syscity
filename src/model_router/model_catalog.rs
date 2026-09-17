//! Dynamic Model Catalog with discovery and suppression
//!
//! Provides a unified registry of available LLM models across all
//! configured providers. Models can be discovered from static config,
//! provider APIs, or plugin extensions. Suppressed models are excluded
//! from routing and API responses.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Pricing information for a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Input cost per 1K tokens (USD)
    pub input_per_1k: f64,
    /// Output cost per 1K tokens (USD)
    pub output_per_1k: f64,
    /// Cached input cost per 1K tokens (if applicable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_per_1k: Option<f64>,
}

/// A single model entry in the catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    /// Provider-specific model ID (e.g. "gpt-4o", "claude-3-5-sonnet-20241022")
    pub id: String,
    /// Human-readable display name
    pub name: String,
    /// Provider name (e.g. "openai", "anthropic")
    pub provider: String,
    /// Context window size in tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    /// Whether the model supports vision / image input
    pub supports_vision: bool,
    /// Whether the model supports tool calling
    pub supports_tools: bool,
    /// Whether the model supports reasoning / thinking
    pub supports_reasoning: bool,
    /// Supported input modalities (e.g. ["text", "image", "audio"])
    pub input_modalities: Vec<String>,
    /// Pricing information
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ModelPricing>,
    /// Additional capability tags
    pub capabilities: Vec<String>,
}

impl ModelCatalogEntry {
    /// Create a minimal entry with just id, name, and provider.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            provider: provider.into(),
            context_window: None,
            supports_vision: false,
            supports_tools: false,
            supports_reasoning: false,
            input_modalities: vec!["text".to_string()],
            pricing: None,
            capabilities: Vec::new(),
        }
    }

    /// Set the context window.
    pub fn with_context_window(mut self, tokens: usize) -> Self {
        self.context_window = Some(tokens);
        self
    }

    /// Enable vision support.
    pub fn with_vision(mut self) -> Self {
        self.supports_vision = true;
        self
    }

    /// Enable tool support.
    pub fn with_tools(mut self) -> Self {
        self.supports_tools = true;
        self
    }

    /// Enable reasoning support.
    pub fn with_reasoning(mut self) -> Self {
        self.supports_reasoning = true;
        self
    }

    /// Set input modalities.
    pub fn with_modalities(mut self, modalities: Vec<String>) -> Self {
        self.input_modalities = modalities;
        self
    }

    /// Set pricing.
    pub fn with_pricing(mut self, pricing: ModelPricing) -> Self {
        self.pricing = Some(pricing);
        self
    }

    /// Add a capability tag.
    pub fn with_capability(mut self, cap: impl Into<String>) -> Self {
        self.capabilities.push(cap.into());
        self
    }
}

/// Trait for model discovery sources.
#[async_trait]
pub trait ModelDiscoverySource: Send + Sync {
    /// Discover available models from this source.
    async fn discover(&self) -> crate::Result<Vec<ModelCatalogEntry>>;

    /// Whether this source is the static source for the given provider.
    /// Defaults to `false`; [`StaticModelSource`] matches its owning provider
    /// so [`ModelCatalog::replace_static_source`] can drop stale per-provider
    /// sources on update.
    fn matches_provider(&self, _provider: &str) -> bool {
        false
    }
}

/// Static discovery source that returns models owned by a configured provider.
pub struct StaticModelSource {
    /// Owning provider key, used for source replacement on update.
    provider: String,
    models: Vec<(String, String)>, // (provider, model_id)
}

impl StaticModelSource {
    /// Create a source owned by `provider` from `(provider, model_id)` pairs.
    pub fn new(provider: impl Into<String>, models: Vec<(String, String)>) -> Self {
        Self {
            provider: provider.into(),
            models,
        }
    }
}

#[async_trait]
impl ModelDiscoverySource for StaticModelSource {
    async fn discover(&self) -> crate::Result<Vec<ModelCatalogEntry>> {
        let mut entries = Vec::new();
        for (provider, model_id) in &self.models {
            let mut entry =
                ModelCatalogEntry::new(model_id.clone(), model_id.clone(), provider.clone());

            // Heuristic: set known context windows and capabilities
            entry = apply_known_model_metadata(entry, model_id);
            entries.push(entry);
        }
        Ok(entries)
    }

    fn matches_provider(&self, provider: &str) -> bool {
        self.provider == provider
    }
}

/// Apply known metadata heuristics for common models.
fn apply_known_model_metadata(mut entry: ModelCatalogEntry, model_id: &str) -> ModelCatalogEntry {
    let id_lower = model_id.to_lowercase();

    // Family match, not version match. The list this replaced enumerated
    // shapes like `claude-3-5-sonnet` and `claude-4-sonnet`, so it had already
    // stopped recognising the model the shipped Anthropic preset ships
    // (`claude-sonnet-4-6` — the version sits in the middle) and would have
    // missed every later id too. What is true of a family stays true across its
    // generations: Claude models take images, call tools, and read 200k tokens.
    let is_claude = id_lower.starts_with("claude")
        && ["opus", "sonnet", "haiku"]
            .iter()
            .any(|family| id_lower.contains(family));
    if is_claude {
        entry.context_window = Some(200_000);
        entry.supports_vision = true;
        entry.supports_tools = true;
        entry.supports_reasoning = true;
        entry.input_modalities = vec!["text".to_string(), "image".to_string()];
        entry.capabilities = vec!["long_context".to_string(), "vision".to_string()];
    // (The `claude-3-haiku` branch that used to sit here is subsumed by the
    // family match above: any Claude model with `haiku` in the id takes it.)
    } else if id_lower.contains("gpt-4o") || id_lower.contains("gpt-4-turbo") {
        entry.context_window = Some(128_000);
        entry.supports_vision = true;
        entry.supports_tools = true;
        entry.input_modalities = vec!["text".to_string(), "image".to_string()];
    } else if id_lower.contains("gpt-4") {
        entry.context_window = Some(8192);
        entry.supports_tools = true;
    } else if id_lower.contains("gpt-3.5") || id_lower.contains("gpt-35") {
        entry.context_window = Some(16_385);
        entry.supports_tools = true;
    } else if id_lower.contains("o1") || id_lower.contains("o3") {
        entry.context_window = Some(200_000);
        entry.supports_reasoning = true;
        entry.supports_tools = true;
        entry.input_modalities = vec!["text".to_string(), "image".to_string()];
    }

    entry
}

/// Plugin-driven model discovery source.
///
/// Scans loaded plugin manifests for `Models` capabilities and
/// converts them into `ModelCatalogEntry` instances.
pub struct PluginModelSource {
    plugins: Vec<crate::plugins::manifest::PluginManifest>,
}

impl PluginModelSource {
    /// Create a source from a list of plugin manifests.
    pub fn new(plugins: Vec<crate::plugins::manifest::PluginManifest>) -> Self {
        Self { plugins }
    }
}

#[async_trait]
impl ModelDiscoverySource for PluginModelSource {
    async fn discover(&self) -> crate::Result<Vec<ModelCatalogEntry>> {
        let mut entries = Vec::new();
        for plugin in &self.plugins {
            for model in plugin.get_models() {
                let mut entry = ModelCatalogEntry::new(
                    model.id.clone(),
                    model.name.clone(),
                    model.provider.clone(),
                );
                if let Some(ctx) = model.context_window {
                    entry.context_window = Some(ctx);
                }
                entry.supports_vision = model.supports_vision;
                entry.supports_tools = model.supports_tools;
                entry.supports_reasoning = model.supports_reasoning;
                entry.input_modalities = model.input_modalities.clone();
                if let (Some(input), Some(output)) =
                    (model.input_cost_per_1k, model.output_cost_per_1k)
                {
                    entry.pricing = Some(ModelPricing {
                        input_per_1k: input,
                        output_per_1k: output,
                        cached_input_per_1k: None,
                    });
                }
                entries.push(entry);
            }
        }
        Ok(entries)
    }
}

/// Dynamic model catalog with discovery and suppression.
pub struct ModelCatalog {
    /// All discovered entries keyed by "provider:model_id"
    entries: RwLock<HashMap<String, ModelCatalogEntry>>,
    /// Suppressed model keys (excluded from routing and API responses)
    suppressed: RwLock<HashSet<String>>,
    /// Discovery sources
    sources: RwLock<Vec<Box<dyn ModelDiscoverySource>>>,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelCatalog {
    /// Create an empty catalog.
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            suppressed: RwLock::new(HashSet::new()),
            sources: RwLock::new(Vec::new()),
        }
    }

    /// Register a discovery source.
    pub async fn add_source(&self, source: Box<dyn ModelDiscoverySource>) {
        let mut sources = self.sources.write().await;
        sources.push(source);
    }

    /// Replace the static discovery source for a provider, dropping any
    /// entries it previously contributed. Used when a provider is added,
    /// updated, or re-added so stale model IDs do not linger in the catalog.
    pub async fn replace_static_source(&self, provider: &str, models: Vec<String>) {
        {
            let mut sources = self.sources.write().await;
            sources.retain(|s| !s.matches_provider(provider));
            sources.push(Box::new(StaticModelSource::new(
                provider.to_string(),
                models
                    .into_iter()
                    .map(|m| (provider.to_string(), m))
                    .collect(),
            )));
        }
        {
            let mut entries = self.entries.write().await;
            entries.retain(|key, _| key.rsplit_once(':').map(|(p, _)| p) != Some(provider));
        }
    }

    /// Run discovery from all registered sources and merge results.
    pub async fn discover(&self) -> crate::Result<usize> {
        let sources = self.sources.read().await;
        let mut new_entries = Vec::new();
        for source in sources.iter() {
            match source.discover().await {
                Ok(entries) => new_entries.extend(entries),
                Err(e) => {
                    tracing::warn!("Model discovery source failed: {}", e);
                }
            }
        }

        let mut entries = self.entries.write().await;
        for entry in new_entries {
            let key = format!("{}:{}", entry.provider, entry.id);
            entries.insert(key, entry);
        }
        Ok(entries.len())
    }

    /// Manually register an entry.
    pub async fn register(&self, entry: ModelCatalogEntry) {
        let key = format!("{}:{}", entry.provider, entry.id);
        let mut entries = self.entries.write().await;
        entries.insert(key, entry);
    }

    /// Get an entry by provider and model id.
    pub async fn get(&self, provider: &str, model_id: &str) -> Option<ModelCatalogEntry> {
        let key = format!("{}:{}", provider, model_id);
        let entries = self.entries.read().await;
        entries.get(&key).cloned()
    }

    /// List all non-suppressed entries.
    pub async fn list(&self) -> Vec<ModelCatalogEntry> {
        let entries = self.entries.read().await;
        let suppressed = self.suppressed.read().await;
        entries
            .values()
            .filter(|e| {
                let key = format!("{}:{}", e.provider, e.id);
                !suppressed.contains(&key)
            })
            .cloned()
            .collect()
    }

    /// List all entries including suppressed.
    pub async fn list_all(&self) -> Vec<ModelCatalogEntry> {
        let entries = self.entries.read().await;
        entries.values().cloned().collect()
    }

    /// Suppress a model by provider and id.
    pub async fn suppress(&self, provider: &str, model_id: &str) {
        let key = format!("{}:{}", provider, model_id);
        let mut suppressed = self.suppressed.write().await;
        suppressed.insert(key);
    }

    /// Unsuppress a model.
    pub async fn unsuppress(&self, provider: &str, model_id: &str) {
        let key = format!("{}:{}", provider, model_id);
        let mut suppressed = self.suppressed.write().await;
        suppressed.remove(&key);
    }

    /// Check if a model is suppressed.
    pub async fn is_suppressed(&self, provider: &str, model_id: &str) -> bool {
        let key = format!("{}:{}", provider, model_id);
        let suppressed = self.suppressed.read().await;
        suppressed.contains(&key)
    }

    /// Clear all entries and sources.
    pub async fn clear(&self) {
        let mut entries = self.entries.write().await;
        entries.clear();
        let mut sources = self.sources.write().await;
        sources.clear();
        let mut suppressed = self.suppressed.write().await;
        suppressed.clear();
    }

    /// Find models that support a given capability.
    pub async fn find_by_capability(&self, capability: &str) -> Vec<ModelCatalogEntry> {
        let entries = self.entries.read().await;
        let suppressed = self.suppressed.read().await;
        entries
            .values()
            .filter(|e| {
                let key = format!("{}:{}", e.provider, e.id);
                !suppressed.contains(&key) && e.capabilities.iter().any(|c| c == capability)
            })
            .cloned()
            .collect()
    }

    /// Find models by provider.
    pub async fn find_by_provider(&self, provider: &str) -> Vec<ModelCatalogEntry> {
        let entries = self.entries.read().await;
        let suppressed = self.suppressed.read().await;
        entries
            .values()
            .filter(|e| {
                let key = format!("{}:{}", e.provider, e.id);
                !suppressed.contains(&key) && e.provider == provider
            })
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Capability inference must not be keyed on a version.
    ///
    /// The list this replaced enumerated shapes like `claude-3-5-sonnet` and
    /// `claude-4-sonnet`, which meant it did not recognise the model the
    /// shipped Anthropic preset ships (`claude-sonnet-4-6` — version in the
    /// middle) and would not have recognised the default either.
    #[test]
    fn claude_families_are_recognised_whatever_the_version_shape() {
        for id in [
            crate::providers::DEFAULT_MODEL,
            "claude-sonnet-4-6",
            "claude-3-5-sonnet-20241022",
            "claude-opus-5",
            "claude-haiku-4-5",
        ] {
            let entry = apply_known_model_metadata(ModelCatalogEntry::new(id, id, "anthropic"), id);
            assert_eq!(
                entry.context_window,
                Some(200_000),
                "{id} should be known as a Claude-family model"
            );
            assert!(entry.supports_vision, "{id} should be known to take images");
            assert!(entry.supports_tools, "{id} should be known to call tools");
        }

        // A non-Claude id is still left alone.
        let other = apply_known_model_metadata(
            ModelCatalogEntry::new("gpt-4o", "GPT-4o", "openai"),
            "gpt-4o",
        );
        assert!(
            other.capabilities.is_empty() || !other.capabilities.contains(&"vision".to_string())
        );
    }

    #[test]
    fn test_entry_builder() {
        let entry = ModelCatalogEntry::new("gpt-4o", "GPT-4o", "openai")
            .with_context_window(128_000)
            .with_vision()
            .with_tools()
            .with_capability("json_mode");

        assert_eq!(entry.id, "gpt-4o");
        assert_eq!(entry.context_window, Some(128_000));
        assert!(entry.supports_vision);
        assert!(entry.supports_tools);
        assert_eq!(entry.capabilities, vec!["json_mode"]);
    }

    #[tokio::test]
    async fn test_catalog_register_and_list() {
        let catalog = ModelCatalog::new();
        catalog
            .register(ModelCatalogEntry::new("gpt-4", "GPT-4", "openai"))
            .await;
        catalog
            .register(ModelCatalogEntry::new("claude-3", "Claude 3", "anthropic"))
            .await;

        let list = catalog.list().await;
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn test_catalog_suppress() {
        let catalog = ModelCatalog::new();
        catalog
            .register(ModelCatalogEntry::new("gpt-4", "GPT-4", "openai"))
            .await;

        catalog.suppress("openai", "gpt-4").await;
        assert!(catalog.is_suppressed("openai", "gpt-4").await);

        let list = catalog.list().await;
        assert!(list.is_empty());

        catalog.unsuppress("openai", "gpt-4").await;
        assert!(!catalog.is_suppressed("openai", "gpt-4").await);

        let list = catalog.list().await;
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn test_catalog_find_by_capability() {
        let catalog = ModelCatalog::new();
        catalog
            .register(
                ModelCatalogEntry::new("gpt-4o", "GPT-4o", "openai").with_capability("vision"),
            )
            .await;
        catalog
            .register(ModelCatalogEntry::new("gpt-4", "GPT-4", "openai"))
            .await;

        let vision = catalog.find_by_capability("vision").await;
        assert_eq!(vision.len(), 1);
        assert_eq!(vision[0].id, "gpt-4o");
    }

    #[tokio::test]
    async fn test_static_source_discovery() {
        let source = StaticModelSource::new(
            "anthropic",
            vec![
                ("anthropic".to_string(), "claude-3-5-sonnet-20241022".to_string()),
                ("anthropic".to_string(), "claude-3-haiku-20240307".to_string()),
            ],
        );

        let entries = source.discover().await.unwrap();
        assert_eq!(entries.len(), 2);

        let sonnet = entries
            .iter()
            .find(|e| e.id == "claude-3-5-sonnet-20241022")
            .unwrap();
        assert_eq!(sonnet.provider, "anthropic");
        assert!(sonnet.supports_tools);
    }

    #[tokio::test]
    async fn test_catalog_discover() {
        let catalog = ModelCatalog::new();
        catalog
            .add_source(Box::new(StaticModelSource::new(
                "anthropic",
                vec![("anthropic".to_string(), "claude-3-5-sonnet-20241022".to_string())],
            )))
            .await;

        let count = catalog.discover().await.unwrap();
        assert_eq!(count, 1);

        let list = catalog.list().await;
        assert_eq!(list.len(), 1);
    }

    #[tokio::test]
    async fn test_replace_static_source_drops_stale_entries() {
        let catalog = ModelCatalog::new();
        catalog
            .replace_static_source("deepseek", vec!["deepseek-chat".to_string()])
            .await;
        catalog.discover().await.unwrap();
        assert_eq!(catalog.list().await.len(), 1);

        // Replacing the same provider must not leave the old model behind.
        catalog
            .replace_static_source("deepseek", vec!["deepseek-v4-flash".to_string()])
            .await;
        catalog.discover().await.unwrap();
        let list = catalog.list().await;
        let ids: Vec<String> = list.iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids, vec!["deepseek-v4-flash".to_string()]);

        // Another provider is untouched by the replacement.
        catalog
            .replace_static_source("openai", vec!["gpt-4o".to_string()])
            .await;
        catalog.discover().await.unwrap();
        let ids: Vec<String> = catalog.list().await.iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().any(|id| id == "deepseek-v4-flash"));
        assert!(ids.iter().any(|id| id == "gpt-4o"));
    }

    #[test]
    fn test_known_metadata_gpt4o() {
        let entry = ModelCatalogEntry::new("gpt-4o", "GPT-4o", "openai");
        let entry = apply_known_model_metadata(entry, "gpt-4o");
        assert_eq!(entry.context_window, Some(128_000));
        assert!(entry.supports_vision);
        assert!(entry.supports_tools);
    }

    #[test]
    fn test_known_metadata_claude_opus() {
        let entry = ModelCatalogEntry::new("claude-3-opus-20240229", "Claude 3 Opus", "anthropic");
        let entry = apply_known_model_metadata(entry, "claude-3-opus-20240229");
        assert_eq!(entry.context_window, Some(200_000));
        assert!(entry.supports_reasoning);
    }
}
