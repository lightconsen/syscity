//! Plugin-extensible Provider
//!
//! Allows WASM plugins to register custom LLM provider implementations.
//! The plugin declares its provider capability in the manifest, and the
//! PluginManager creates a `PluginProvider` that delegates complete/stream
//! calls to the plugin runtime.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::warn;

use crate::providers::stream_wrappers::ProviderStreamFamily;
use crate::providers::{
    CompletionChunk, CompletionRequest, CompletionResponse, CompletionStream, Provider,
};

/// A provider implementation backed by a plugin.
///
/// The plugin must implement the `provider_complete` and `provider_stream`
/// exports (or the runtime must handle the delegation).
pub struct PluginProvider {
    plugin_id: String,
    provider_name: String,
    default_model: String,
    supports_tools: bool,
    max_context: usize,
    stream_family: ProviderStreamFamily,
    runtime: Arc<crate::plugins::runtime::PluginRuntime>,
}

impl PluginProvider {
    /// Create a new plugin-backed provider.
    pub fn new(
        plugin_id: String,
        name: String,
        default_model: String,
        supports_tools: bool,
        max_context: usize,
        stream_family: ProviderStreamFamily,
        runtime: Arc<crate::plugins::runtime::PluginRuntime>,
    ) -> Self {
        Self {
            plugin_id,
            provider_name: name,
            default_model,
            supports_tools,
            max_context,
            stream_family,
            runtime,
        }
    }

    /// Parse a stream family string into the enum.
    pub fn parse_stream_family(family: &str) -> ProviderStreamFamily {
        match family.to_lowercase().as_str() {
            "openai" => ProviderStreamFamily::OpenAi,
            "anthropic" => ProviderStreamFamily::Anthropic,
            "openai_reasoning" | "openai-reasoning" => ProviderStreamFamily::OpenAiReasoning,
            "anthropic_thinking" | "anthropic-thinking" => ProviderStreamFamily::AnthropicThinking,
            "google_thinking" | "google-thinking" => ProviderStreamFamily::GoogleThinking,
            "openrouter" => ProviderStreamFamily::OpenRouter,
            _ => ProviderStreamFamily::Generic,
        }
    }
}

#[async_trait]
impl Provider for PluginProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn supports_tools(&self) -> bool {
        self.supports_tools
    }

    fn max_context(&self) -> usize {
        self.max_context
    }

    async fn complete(&self, request: CompletionRequest) -> crate::Result<CompletionResponse> {
        // Delegate to plugin runtime
        let request_val = serde_json::to_value(request).map_err(|e| {
            crate::error::SyscityError::Internal(format!(
                "Failed to serialize provider request: {}",
                e
            ))
        })?;
        let result = self
            .runtime
            .call_provider_complete(&self.plugin_id, &request_val)
            .await?;

        let response: CompletionResponse = serde_json::from_value(result).map_err(|e| {
            crate::error::SyscityError::Internal(format!(
                "Plugin provider response parse error: {}",
                e
            ))
        })?;
        Ok(response)
    }

    async fn stream(&self, request: CompletionRequest) -> crate::Result<CompletionStream> {
        // For plugin providers, we delegate to the runtime which returns
        // a stream of JSON chunks. We convert those into CompletionChunks.
        let request_val = serde_json::to_value(request).map_err(|e| {
            crate::error::SyscityError::Internal(format!(
                "Failed to serialize provider stream request: {}",
                e
            ))
        })?;
        let result = self
            .runtime
            .call_provider_stream(&self.plugin_id, &request_val)
            .await?;

        // The plugin runtime returns a receiver or a stream descriptor.
        // For now, we return an empty stream as a placeholder.
        // In a full implementation, the runtime would return a channel receiver
        // that we convert into a CompletionStream.
        let chunks: Vec<CompletionChunk> = match serde_json::from_value(result) {
            Ok(chunks) => chunks,
            Err(e) => {
                warn!(
                    "Plugin provider '{}' stream returned unparseable JSON: {}",
                    self.plugin_id, e
                );
                Vec::new()
            }
        };
        Ok(Box::pin(futures::stream::iter(chunks)))
    }

    fn stream_family(&self) -> ProviderStreamFamily {
        self.stream_family
    }

    async fn health_check(&self) -> crate::Result<bool> {
        match self
            .runtime
            .call_provider_health_check(&self.plugin_id)
            .await
        {
            Ok(val) => match val.as_bool() {
                Some(b) => Ok(b),
                None => {
                    warn!(
                        "Plugin provider '{}' health_check returned non-boolean: {}",
                        self.plugin_id, val
                    );
                    Ok(false)
                }
            },
            Err(_) => Ok(false),
        }
    }

    async fn set_credential(
        &self,
        _credential: crate::model_router::Credential,
    ) -> crate::Result<()> {
        // Plugin providers manage credentials inside the plugin runtime; this
        // provider adapter does not hold credentials directly.
        warn!(
            "Plugin provider '{}' does not support set_credential (credentials managed internally)",
            self.plugin_id
        );
        Ok(())
    }
}

/// Registry of plugin-backed providers.
#[derive(Default)]
pub struct PluginProviderRegistry {
    providers: std::collections::HashMap<String, Arc<PluginProvider>>,
}

impl PluginProviderRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            providers: std::collections::HashMap::new(),
        }
    }

    /// Register a plugin provider.
    pub fn register(&mut self, name: String, provider: Arc<PluginProvider>) {
        self.providers.insert(name, provider);
    }

    /// Get a provider by name.
    pub fn get(&self, name: &str) -> Option<Arc<PluginProvider>> {
        self.providers.get(name).cloned()
    }

    /// Remove a provider.
    pub fn remove(&mut self, name: &str) -> Option<Arc<PluginProvider>> {
        self.providers.remove(name)
    }

    /// List all registered plugin provider names.
    pub fn list(&self) -> Vec<&str> {
        self.providers.keys().map(|s| s.as_str()).collect()
    }

    pub fn has(&self, name: &str) -> bool {
        self.providers.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // parse_stream_family
    // ------------------------------------------------------------------

    #[test]
    fn test_parse_stream_family_openai() {
        assert_eq!(PluginProvider::parse_stream_family("openai"), ProviderStreamFamily::OpenAi);
    }

    #[test]
    fn test_parse_stream_family_anthropic() {
        assert_eq!(
            PluginProvider::parse_stream_family("anthropic"),
            ProviderStreamFamily::Anthropic
        );
    }

    #[test]
    fn test_parse_stream_family_openai_reasoning() {
        assert_eq!(
            PluginProvider::parse_stream_family("openai_reasoning"),
            ProviderStreamFamily::OpenAiReasoning
        );
        assert_eq!(
            PluginProvider::parse_stream_family("openai-reasoning"),
            ProviderStreamFamily::OpenAiReasoning
        );
    }

    #[test]
    fn test_parse_stream_family_anthropic_thinking() {
        assert_eq!(
            PluginProvider::parse_stream_family("anthropic_thinking"),
            ProviderStreamFamily::AnthropicThinking
        );
        assert_eq!(
            PluginProvider::parse_stream_family("anthropic-thinking"),
            ProviderStreamFamily::AnthropicThinking
        );
    }

    #[test]
    fn test_parse_stream_family_google_thinking() {
        assert_eq!(
            PluginProvider::parse_stream_family("google_thinking"),
            ProviderStreamFamily::GoogleThinking
        );
        assert_eq!(
            PluginProvider::parse_stream_family("google-thinking"),
            ProviderStreamFamily::GoogleThinking
        );
    }

    #[test]
    fn test_parse_stream_family_openrouter() {
        assert_eq!(
            PluginProvider::parse_stream_family("openrouter"),
            ProviderStreamFamily::OpenRouter
        );
    }

    #[test]
    fn test_parse_stream_family_unknown() {
        assert_eq!(PluginProvider::parse_stream_family("unknown"), ProviderStreamFamily::Generic);
        assert_eq!(PluginProvider::parse_stream_family(""), ProviderStreamFamily::Generic);
    }

    #[test]
    fn test_parse_stream_family_case_insensitive() {
        assert_eq!(PluginProvider::parse_stream_family("OpenAI"), ProviderStreamFamily::OpenAi);
        assert_eq!(
            PluginProvider::parse_stream_family("ANTHROPIC"),
            ProviderStreamFamily::Anthropic
        );
    }

    // ------------------------------------------------------------------
    // PluginProvider::new and getters
    // ------------------------------------------------------------------

    /// Stub runtime for tests that do not actually invoke WASM.
    fn stub_runtime() -> Arc<crate::plugins::runtime::PluginRuntime> {
        Arc::new(
            crate::plugins::runtime::PluginRuntime::new(
                std::env::temp_dir().join("syscity-plugin-tests"),
            )
            .unwrap(),
        )
    }

    #[test]
    fn test_plugin_provider_new() {
        let rt = stub_runtime();
        let provider = PluginProvider::new(
            "com.test.provider".to_string(),
            "test-provider".to_string(),
            "gpt-4".to_string(),
            true,
            8192,
            ProviderStreamFamily::OpenAi,
            rt.clone(),
        );
        assert_eq!(provider.name(), "test-provider");
        assert_eq!(provider.default_model(), "gpt-4");
        assert!(provider.supports_tools());
        assert_eq!(provider.max_context(), 8192);
        assert_eq!(provider.stream_family(), ProviderStreamFamily::OpenAi);
        assert_eq!(provider.plugin_id, "com.test.provider");
    }

    #[test]
    fn test_plugin_provider_defaults() {
        let rt = stub_runtime();
        let provider = PluginProvider::new(
            "com.test.defaults".to_string(),
            "default-provider".to_string(),
            "claude-3".to_string(),
            false,
            4096,
            ProviderStreamFamily::Anthropic,
            rt,
        );
        assert!(!provider.supports_tools());
        assert_eq!(provider.max_context(), 4096);
        assert_eq!(provider.default_model(), "claude-3");
    }

    // ------------------------------------------------------------------
    // PluginProviderRegistry
    // ------------------------------------------------------------------

    fn stub_provider() -> Arc<PluginProvider> {
        let rt = stub_runtime();
        Arc::new(PluginProvider::new(
            "com.reg.test".to_string(),
            "reg-test".to_string(),
            "model".to_string(),
            false,
            1024,
            ProviderStreamFamily::Generic,
            rt,
        ))
    }

    #[test]
    fn test_registry_new_empty() {
        let registry = PluginProviderRegistry::new();
        assert!(registry.list().is_empty());
        assert!(!registry.has("anything"));
        assert!(registry.get("anything").is_none());
    }

    #[test]
    fn test_registry_register_and_get() {
        let mut registry = PluginProviderRegistry::new();
        let provider = stub_provider();
        registry.register("test-provider".to_string(), provider.clone());
        assert!(registry.has("test-provider"));
        let retrieved = registry.get("test-provider");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name(), "reg-test");
    }

    #[test]
    fn test_registry_remove() {
        let mut registry = PluginProviderRegistry::new();
        let provider = stub_provider();
        registry.register("removable".to_string(), provider);
        assert!(registry.has("removable"));
        let removed = registry.remove("removable");
        assert!(removed.is_some());
        assert!(!registry.has("removable"));
    }

    #[test]
    fn test_registry_list() {
        let mut registry = PluginProviderRegistry::new();
        registry.register(
            "first".to_string(),
            Arc::new(PluginProvider::new(
                "com.first".to_string(),
                "first".to_string(),
                "m1".to_string(),
                false,
                1,
                ProviderStreamFamily::Generic,
                stub_runtime(),
            )),
        );
        registry.register(
            "second".to_string(),
            Arc::new(PluginProvider::new(
                "com.second".to_string(),
                "second".to_string(),
                "m2".to_string(),
                true,
                2,
                ProviderStreamFamily::Generic,
                stub_runtime(),
            )),
        );
        let mut names = registry.list();
        names.sort();
        assert_eq!(names, vec!["first", "second"]);
    }

    #[test]
    fn test_registry_remove_nonexistent() {
        let mut registry = PluginProviderRegistry::new();
        let removed = registry.remove("nonexistent");
        assert!(removed.is_none());
    }
}
