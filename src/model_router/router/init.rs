//! `ModelRouter` construction, builder wiring, and initialization.

use super::*;

impl ModelRouter {
    /// Create a new model router
    pub fn new(config: ModelRouterConfig) -> Self {
        Self {
            config: RwLock::new(config),
            providers: RwLock::new(HashMap::new()),
            health: RwLock::new(HashMap::new()),
            fallback_chains: RwLock::new(HashMap::new()),
            auth_profiles: AuthProfileManager::new(),
            usage_tracker: ProviderUsageTracker::new(UsageTrackerConfig::default()),
            model_catalog: ModelCatalog::new(),
            usage_fetchers: RwLock::new(UsageFetcherRegistry::default()),
            db_pool: None,
            task_registry: None,
            shutdown_token: CancellationToken::new(),
            classifier: Box::new(KeywordTaskClassifier),
            secrets: None,
        }
    }

    /// Attach the secret-store handle used to resolve `StoreRef` provider keys.
    pub fn with_secrets(mut self, secrets: Arc<crate::secrets::SecretStoreHandle>) -> Self {
        self.secrets = Some(secrets);
        self
    }

    /// Attach a SQLite connection pool for persisting auth profile state.
    pub fn with_db_pool(mut self, pool: sqlx::Pool<sqlx::Sqlite>) -> Self {
        self.db_pool = Some(pool);
        self
    }

    /// Attach the task registry used to track background health checks.
    pub fn with_task_registry(mut self, registry: Arc<TaskRegistry>) -> Self {
        self.task_registry = Some(registry);
        self
    }

    /// Attach the shutdown token used to cancel background health checks.
    pub fn with_shutdown_token(mut self, token: CancellationToken) -> Self {
        self.shutdown_token = token;
        self
    }

    /// Attach a custom task classifier for cost-aware routing.
    pub fn with_classifier(mut self, classifier: Box<dyn TaskClassifierImpl>) -> Self {
        self.classifier = classifier;
        self
    }

    /// Return a clone of the current router configuration.
    pub async fn router_config(&self) -> ModelRouterConfig {
        self.config.read().await.clone()
    }

    // ==================== INITIALIZATION ====================

    /// Initialize providers, fallback chains, and model catalog from config.
    pub async fn initialize(&self) -> crate::Result<()> {
        // Wire up persistent store if a database pool is available
        if let Some(ref pool) = self.db_pool {
            let store = std::sync::Arc::new(
                crate::model_router::auth_profile_store::AuthProfileStore::new(pool.clone()),
            );
            self.auth_profiles.set_store(store).await;
        }

        let config = self.config.read().await;

        for (name, provider_config) in &config.providers {
            info!("Initializing provider: {}", name);

            let auth_config = provider_config.derived_auth_profile_config();
            self.auth_profiles
                .register_from_config(name, &auth_config)
                .await;
            info!("Registered auth profile for '{}' with {} key(s)", name, auth_config.keys.len());

            let provider = self.create_provider(provider_config).await?;

            let mut providers = self.providers.write().await;
            providers.insert(name.clone(), provider);

            let mut health = self.health.write().await;
            health.insert(name.clone(), ProviderHealth::default());

            if matches!(provider_config.provider_type, ProviderType::OpenAi) {
                let api_key = provider_config.effective_key(self.secrets.as_deref()).await;
                if !api_key.is_empty() {
                    let fetcher = OpenAiUsageFetcher::new(api_key);
                    let mut fetchers = self.usage_fetchers.write().await;
                    fetchers.register(name.clone(), Arc::new(fetcher));
                }
            }
        }

        // Initialize fallback chains
        self.init_fallback_chains_from_config(&config).await;

        // Initialize model catalog from the models owned by each provider.
        // Replace per provider so a combined source cannot resurrect models
        // dropped by a later runtime update.
        let provider_models: Vec<(String, Vec<String>)> = config
            .providers
            .iter()
            .map(|(provider, pcfg)| (provider.clone(), pcfg.models.clone()))
            .collect();
        drop(config);

        for (provider, models) in &provider_models {
            self.model_catalog
                .replace_static_source(provider, models.clone())
                .await;
        }
        if let Err(e) = self.model_catalog.discover().await {
            warn!("Model catalog discovery failed: {}", e);
        }

        Ok(())
    }

    /// Initialize fallback chains and model catalog without re-creating
    /// providers.  Safe to call after `add_provider()` for production init.
    pub async fn init_catalog_and_chains(&self) {
        let config = self.config.read().await;
        self.init_fallback_chains_from_config(&config).await;

        let provider_models: Vec<(String, Vec<String>)> = config
            .providers
            .iter()
            .map(|(provider, pcfg)| (provider.clone(), pcfg.models.clone()))
            .collect();
        drop(config);

        for (provider, models) in &provider_models {
            self.model_catalog
                .replace_static_source(provider, models.clone())
                .await;
        }
        if let Err(e) = self.model_catalog.discover().await {
            warn!("Model catalog discovery failed: {}", e);
        }
    }

    async fn init_fallback_chains_from_config(&self, config: &ModelRouterConfig) {
        let mut chains = self.fallback_chains.write().await;
        for (model_id, provider_list) in &config.fallback_chains {
            let entries: Vec<FallbackEntry> = provider_list
                .iter()
                .map(|p| FallbackEntry {
                    provider: p.clone(),
                    model: model_id.clone(),
                    enabled: true,
                    health_score: 100,
                })
                .collect();
            chains.insert(model_id.clone(), entries);
        }
    }
}
