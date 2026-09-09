//! `ModelRouter` provider/model/fallback-chain management.

use std::collections::HashSet;

use super::*;

impl ModelRouter {
    // ==================== DEFAULT PROVIDER ====================

    /// Create a default provider. Deterministic pick: the alphabetically
    /// first non-cloud registered provider. The registry is a HashMap, so
    /// `.iter().next()` flapped the default between restarts — potentially
    /// landing on the metered cloud proxy for unconfigured LLM calls.
    pub async fn create_default_provider(&self) -> crate::Result<Arc<dyn Provider + Send + Sync>> {
        let providers = self.providers.read().await;

        let picked: Option<(String, Arc<dyn Provider + Send + Sync>)> = {
            let mut names: Vec<&str> = providers.keys().map(String::as_str).collect();
            names.sort_by_key(|n| (*n == "cloud", *n));
            names
                .first()
                .and_then(|n| providers.get(*n).map(|p| ((*n).to_string(), p.clone())))
        };

        if let Some((name, provider)) = picked {
            info!("Using default provider: {name}");
            Ok(provider)
        } else {
            drop(providers);

            if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
                info!("Creating default Anthropic provider from environment");
                let provider =
                    crate::providers::anthropic::AnthropicProvider::new(api_key.clone())?;
                let provider_arc = Arc::new(provider);

                self.auth_profiles
                    .register_single_key("anthropic", api_key)
                    .await;

                let mut providers = self.providers.write().await;
                providers.insert("anthropic".to_string(), provider_arc.clone());

                Ok(provider_arc)
            } else {
                Err(crate::error::ConfigError::Missing(
                    "No providers configured and ANTHROPIC_API_KEY not set".to_string(),
                )
                .into())
            }
        }
    }

    // ==================== MODEL MANAGEMENT ====================

    /// Get a registered provider instance by config name (`None` when the
    /// name is not registered). Deterministic counterpart to
    /// [`create_default_provider`] for callers that must pin a provider.
    pub async fn get_provider(&self, name: &str) -> Option<Arc<dyn Provider + Send + Sync>> {
        self.providers.read().await.get(name).cloned()
    }

    /// List all `(provider, model_id)` pairs from provider configs and
    /// catalog-discovered models. Catalog entries are only included for
    /// providers that still exist in config, so removed providers do not
    /// resurrect their models.
    pub async fn models_with_providers(&self) -> Vec<(String, String)> {
        let mut seen = HashSet::new();
        let mut result = Vec::new();

        {
            let config = self.config.read().await;
            // HashMap iteration order is randomized per process, and two
            // providers can serve the same model id (a direct vendor config
            // and the cloud proxy both carry e.g. "deepseek-v4-flash").
            // Scan providers in a deterministic order — sorted, with the
            // cloud proxy last — so a duplicated id is attributed to its
            // direct provider and the cloud section only lists models
            // unique to it.
            let mut entries: Vec<_> = config.providers.iter().collect();
            entries.sort_by_key(|(name, _)| (name.as_str() == "cloud", name.as_str()));
            for (provider, pcfg) in entries {
                for model in &pcfg.models {
                    if seen.insert(model.clone()) {
                        result.push((provider.clone(), model.clone()));
                    }
                }
            }
        }

        let provider_names: Vec<String> = {
            let config = self.config.read().await;
            config.providers.keys().cloned().collect()
        };

        for entry in self.model_catalog.list().await {
            if provider_names.contains(&entry.provider) && seen.insert(entry.id.clone()) {
                result.push((entry.provider, entry.id));
            }
        }

        result
    }

    /// Resolve the provider that owns a concrete model ID, checking provider
    /// configs first and then the model catalog (for API-discovered models).
    pub async fn provider_for_model(&self, model_id: &str) -> Option<String> {
        {
            let config = self.config.read().await;
            if let Some(provider) = config.provider_for_model(model_id) {
                return Some(provider.to_string());
            }
        }
        self.model_catalog
            .list()
            .await
            .into_iter()
            .find(|e| e.id == model_id)
            .map(|e| e.provider)
    }

    /// Switch the default model to a concrete model ID.
    pub async fn switch_default_model(&self, model_id: &str) -> crate::Result<()> {
        if self.provider_for_model(model_id).await.is_none() {
            return Err(crate::error::ConfigError::InvalidValue {
                key: "default_model".to_string(),
                message: format!("Unknown model: {model_id}"),
            }
            .into());
        }

        let mut config = self.config.write().await;
        info!("Switching default model from '{}' to '{model_id}'", config.default_model);
        config.default_model = model_id.to_string();
        Ok(())
    }

    /// Get the current default concrete model ID.
    pub async fn get_default_model(&self) -> String {
        let config = self.config.read().await;
        config.default_model.clone()
    }

    // ==================== PROVIDER MANAGEMENT ====================

    /// List all available providers with their status
    pub async fn list_providers(&self) -> Vec<ProviderInfo> {
        // Narrow lock scopes (Issue 9): collect data while holding each lock,
        // then release before acquiring the next.
        let provider_names: Vec<String> = {
            let providers = self.providers.read().await;
            providers.keys().cloned().collect()
        };

        let health_snapshot: HashMap<String, ProviderHealth> = {
            let health = self.health.read().await;
            health.clone()
        };

        let provider_configs: HashMap<String, ProviderConfig> = {
            let config = self.config.read().await;
            config.providers.clone()
        };

        provider_names
            .into_iter()
            .map(|name| {
                let h = health_snapshot.get(&name).cloned().unwrap_or_default();
                let provider_config = provider_configs.get(&name).cloned();

                ProviderInfo {
                    name: name.clone(),
                    provider_type: provider_config
                        .as_ref()
                        .map(|c| format!("{:?}", c.provider_type))
                        .unwrap_or_default(),
                    enabled: h.state != CircuitState::Open,
                    health: crate::model_router::config::ProviderHealthInfo {
                        state: format!("{:?}", h.state),
                        failures: h.failures,
                        successes: h.successes,
                        avg_latency_ms: h.avg_latency_ms,
                        last_failure: h.last_failure,
                        last_health_check: h.last_health_check,
                    },
                    circuit_state: h.state,
                }
            })
            .collect()
    }

    /// Enable a provider (close circuit breaker)
    pub async fn enable_provider(&self, name: &str) -> crate::Result<()> {
        let mut health = self.health.write().await;
        if let Some(h) = health.get_mut(name) {
            h.state = CircuitState::Closed;
            h.failures = 0;
            info!("Provider {name} enabled (circuit closed)");
            Ok(())
        } else {
            Err(crate::error::ConfigError::InvalidValue {
                key: "provider".to_string(),
                message: format!("Unknown provider: {name}"),
            }
            .into())
        }
    }

    /// Disable a provider (open circuit breaker)
    pub async fn disable_provider(&self, name: &str) -> crate::Result<()> {
        let mut health = self.health.write().await;
        if let Some(h) = health.get_mut(name) {
            h.state = CircuitState::Open;
            info!("Provider {name} disabled (circuit opened)");
            Ok(())
        } else {
            Err(crate::error::ConfigError::InvalidValue {
                key: "provider".to_string(),
                message: format!("Unknown provider: {name}"),
            }
            .into())
        }
    }

    /// Check whether a provider name is already registered.
    pub async fn provider_exists(&self, name: &str) -> bool {
        let providers = self.providers.read().await;
        providers.contains_key(name)
    }

    /// Add a new provider at runtime. Rejects a provider name that already
    /// exists.
    pub async fn add_provider(&self, name: &str, config: ProviderConfig) -> crate::Result<()> {
        info!("Adding new provider at runtime: {name}");

        {
            let providers = self.providers.read().await;
            if providers.contains_key(name) {
                return Err(crate::error::ConfigError::InvalidValue {
                    key: "provider".to_string(),
                    message: format!("Provider already exists: {name}"),
                }
                .into());
            }
        }

        let auth_config = config.derived_auth_profile_config();
        self.auth_profiles
            .register_from_config(name, &auth_config)
            .await;

        let provider = self.create_provider(&config).await?;

        {
            let mut providers = self.providers.write().await;
            providers.insert(name.to_string(), provider);
        }

        {
            let mut health = self.health.write().await;
            health.insert(name.to_string(), ProviderHealth::default());
        }

        let model_ids = config.models.clone();
        {
            let mut router_config = self.config.write().await;
            router_config.providers.insert(name.to_string(), config);
        }

        // Register the provider's models in the model catalog so capability
        // routing and model listing can see them immediately. Replace (rather
        // than append) so entries from a prior add of the same name do not
        // linger after a remove + re-add.
        self.model_catalog
            .replace_static_source(name, model_ids)
            .await;
        if let Err(e) = self.model_catalog.discover().await {
            warn!("Model catalog discovery failed: {}", e);
        }

        Ok(())
    }

    /// Replace an existing provider's configuration at runtime (models,
    /// default model, credentials, base URL). The provider instance is rebuilt
    /// from the new config; circuit-breaker health state is preserved.
    pub async fn update_provider(&self, name: &str, config: ProviderConfig) -> crate::Result<()> {
        info!("Updating provider at runtime: {name}");

        {
            let providers = self.providers.read().await;
            if !providers.contains_key(name) {
                return Err(crate::error::ConfigError::InvalidValue {
                    key: "provider".to_string(),
                    message: format!("Unknown provider: {name}"),
                }
                .into());
            }
        }

        let auth_config = config.derived_auth_profile_config();
        self.auth_profiles
            .register_from_config(name, &auth_config)
            .await;

        let provider = self.create_provider(&config).await?;
        {
            let mut providers = self.providers.write().await;
            providers.insert(name.to_string(), provider);
        }

        // Swap the persisted provider entry. Remember the models dropped by
        // the update so stale fallback chains can be cleaned up below.
        let dropped_models: Vec<String> = {
            let mut router_config = self.config.write().await;
            let old_models = router_config
                .providers
                .get(name)
                .map(|p| p.models.clone())
                .unwrap_or_default();
            router_config
                .providers
                .insert(name.to_string(), config.clone());
            old_models
                .into_iter()
                .filter(|m| !config.models.contains(m))
                .collect()
        };

        // Refresh the provider's static catalog source so discovery sees the
        // new model list immediately. Replace (don't append) so models removed
        // by the update do not linger in the catalog.
        self.model_catalog
            .replace_static_source(name, config.models.clone())
            .await;
        if let Err(e) = self.model_catalog.discover().await {
            warn!("Model catalog discovery failed: {}", e);
        }

        // Drop fallback chains for models that no longer exist anywhere.
        let gone: Vec<String> = {
            let owned: HashSet<String> = {
                let router_config = self.config.read().await;
                router_config
                    .providers
                    .values()
                    .flat_map(|p| p.models.iter().cloned())
                    .collect()
            };
            dropped_models
                .into_iter()
                .filter(|m| !owned.contains(m))
                .collect()
        };
        if !gone.is_empty() {
            let mut chains = self.fallback_chains.write().await;
            let mut router_config = self.config.write().await;
            for m in &gone {
                chains.remove(m);
                router_config.fallback_chains.remove(m);
            }
        }

        Ok(())
    }

    /// Add a pre-built provider instance at runtime (e.g. from a plugin).
    pub async fn add_provider_instance(
        &self,
        name: &str,
        provider: Arc<dyn crate::providers::Provider + Send + Sync>,
    ) -> crate::Result<()> {
        info!("Adding provider instance at runtime: {name}");

        {
            let mut providers = self.providers.write().await;
            providers.insert(name.to_string(), provider);
        }

        {
            let mut health = self.health.write().await;
            health.insert(name.to_string(), ProviderHealth::default());
        }

        Ok(())
    }

    /// Remove a provider at runtime
    pub async fn remove_provider(&self, name: &str) -> crate::Result<()> {
        info!("Removing provider at runtime: {name}");

        {
            let mut providers = self.providers.write().await;
            if providers.remove(name).is_none() {
                return Err(crate::error::ConfigError::InvalidValue {
                    key: "provider".to_string(),
                    message: format!("Unknown provider: {name}"),
                }
                .into());
            }
        }

        {
            let mut health = self.health.write().await;
            health.remove(name);
        }

        {
            let mut config = self.config.write().await;
            config.providers.remove(name);
        }

        Ok(())
    }

    /// Remove a concrete model ID from its provider. If the provider ends up
    /// with no models it is removed entirely. If the removed model was the
    /// default, fall back to another provider's default model (deterministic:
    /// lowest provider name).
    pub async fn remove_model(&self, model_id: &str) -> crate::Result<()> {
        let provider_name = self.provider_for_model(model_id).await.ok_or_else(|| {
            crate::error::ConfigError::InvalidValue {
                key: "model".to_string(),
                message: format!("Unknown model: {model_id}"),
            }
        })?;

        let provider_emptied = {
            let mut config = self.config.write().await;
            let Some(pcfg) = config.providers.get_mut(&provider_name) else {
                return Err(crate::error::ConfigError::InvalidValue {
                    key: "provider".to_string(),
                    message: format!("Unknown provider: {provider_name}"),
                }
                .into());
            };
            pcfg.models.retain(|m| m != model_id);
            pcfg.models.is_empty()
        };

        if provider_emptied {
            info!("Provider '{provider_name}' has no remaining models; removing it");
            self.remove_provider(&provider_name).await?;
        }

        {
            let mut config = self.config.write().await;
            config.fallback_chains.remove(model_id);
        }

        {
            let mut config = self.config.write().await;
            if config.default_model == model_id {
                let mut candidates: Vec<(String, String)> = config
                    .providers
                    .iter()
                    .filter(|(_, c)| !c.models.is_empty())
                    .map(|(name, c)| (name.clone(), c.default_model().to_string()))
                    .collect();
                candidates.sort_by(|a, b| a.0.cmp(&b.0));
                let new_default = candidates
                    .first()
                    .map(|(_, m)| m.clone())
                    .unwrap_or_default();
                info!("Default model '{model_id}' removed; falling back to '{new_default}'");
                config.default_model = new_default;
            }
        }

        Ok(())
    }

    /// Get detailed health status for a specific provider
    pub async fn get_provider_health(&self, name: &str) -> Option<ProviderHealthInfo> {
        let health = self.health.read().await;
        health.get(name).map(|h| ProviderHealthInfo {
            state: format!("{:?}", h.state),
            failures: h.failures,
            successes: h.successes,
            avg_latency_ms: h.avg_latency_ms,
            last_failure: h.last_failure,
            last_health_check: h.last_health_check,
        })
    }

    /// Force a health check on a specific provider
    pub async fn check_provider_health(&self, name: &str) -> crate::Result<bool> {
        let provider = {
            let providers = self.providers.read().await;
            providers
                .get(name)
                .cloned()
                .ok_or_else(|| crate::error::ConfigError::InvalidValue {
                    key: "provider".to_string(),
                    message: format!("Unknown provider: {name}"),
                })?
        };

        let start = std::time::Instant::now();
        match provider.health_check().await {
            Ok(true) => {
                self.record_success(name, start.elapsed()).await;
                Ok(true)
            }
            Ok(false) => {
                self.record_failure(name, None).await;
                Ok(false)
            }
            Err(e) => {
                warn!("Health check failed for {}: {}", name, e);
                self.record_failure(name, None).await;
                Ok(false)
            }
        }
    }

    /// Complete a request with a specific provider override
    pub async fn complete_with_provider(
        &self,
        provider_name: &str,
        model: Option<String>,
        messages: Vec<Message>,
        tools: Option<Vec<ToolDefinition>>,
    ) -> crate::Result<CompletionResponse> {
        let provider = {
            let providers = self.providers.read().await;
            providers.get(provider_name).cloned().ok_or_else(|| {
                crate::error::ConfigError::InvalidValue {
                    key: "provider".to_string(),
                    message: format!("Unknown provider: {provider_name}"),
                }
            })?
        };

        if self.is_circuit_open(provider_name).await {
            return Err(crate::error::SyscityError::ExternalService {
                source: format!("Provider {provider_name} circuit is open"),
                cause: None,
            });
        }

        let model_id = model.clone();
        let request = CompletionRequest {
            model,
            messages,
            temperature: None,
            max_tokens: None,
            stream: false,
            tools,
            stop: None,
            extra: None,
            ..Default::default()
        };

        let usage_model = model_id.as_deref().unwrap_or("unknown").to_string();
        let start = std::time::Instant::now();
        match provider.complete(request.clone()).await {
            Ok(response) => {
                self.record_completion_success(
                    provider_name,
                    start.elapsed(),
                    response.usage,
                    &usage_model,
                )
                .await;
                Ok(response)
            }
            Err(ref e) => {
                let class = FailureClass::from_error(e, None);
                warn!("Provider {provider_name} failed with {}: {e}", class.description());

                match self
                    .handle_provider_failure(provider_name, &usage_model, class, e, || {
                        let p = provider.clone();
                        let r = request.clone();
                        async move { p.complete(r).await }
                    })
                    .await
                {
                    Ok(response) => {
                        self.record_completion_success(
                            provider_name,
                            start.elapsed(),
                            response.usage,
                            &usage_model,
                        )
                        .await;
                        Ok(response)
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }

    // ==================== FALLBACK CHAINS ====================

    /// Get fallback chain for a model ID
    pub async fn get_fallback_chain(&self, model_id: &str) -> Vec<String> {
        let chains = self.fallback_chains.read().await;
        chains
            .get(model_id)
            .map(|entries| entries.iter().map(|e| e.provider.clone()).collect())
            .unwrap_or_default()
    }

    /// Update fallback chain for a model ID at runtime
    pub async fn set_fallback_chain(
        &self,
        model_id: &str,
        provider_chain: Vec<String>,
    ) -> crate::Result<()> {
        if self.provider_for_model(model_id).await.is_none() {
            return Err(crate::error::ConfigError::InvalidValue {
                key: "model".to_string(),
                message: format!("Unknown model: {model_id}"),
            }
            .into());
        }

        let entries: Vec<FallbackEntry> = provider_chain
            .iter()
            .map(|p| FallbackEntry {
                provider: p.clone(),
                model: model_id.to_string(),
                enabled: true,
                health_score: 100,
            })
            .collect();

        {
            let mut chains = self.fallback_chains.write().await;
            chains.insert(model_id.to_string(), entries);
        }

        {
            let mut config = self.config.write().await;
            config
                .fallback_chains
                .insert(model_id.to_string(), provider_chain);
        }

        Ok(())
    }
}
