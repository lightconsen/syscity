//! ModelRouter — multi-provider LLM routing with fallback chains
//!
//! Provides the primary [`ModelRouter`] struct with support for:
//! - Providers that own one or more concrete model IDs
//! - Multi-provider routing with automatic fallback
//! - Health checking and circuit breaker
//! - Auth profile / API key rotation
//! - Cost-aware routing with task classification
//! - Pluggable task classifier
//!
//! The `impl ModelRouter` blocks are split across focused submodules:
//! - `init`: construction, builder wiring, initialization
//! - `health`: health checks, circuit breaker, capability routing
//! - `failure`: provider creation, failure recording, key rotation
//! - `routing`: completion/streaming request flow with fallback
//! - `cost_aware`: cost-aware automatic model selection
//! - `quota`: usage snapshots with remote/local quota
//! - `admin`: provider/model/fallback-chain management

mod admin;
mod cost_aware;
mod failure;
mod health;
mod init;
mod quota;
mod routing;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::future::join_all;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::gateway::task_registry::TaskRegistry;
use crate::model_router::auth_profile::{AuthProfileManager, ProfileStatus};
use crate::model_router::classifier::{KeywordTaskClassifier, TaskClassifierImpl};
use crate::model_router::config::{
    CircuitState, CostAwareConfig, FallbackEntry, ModelRouterConfig, ProviderConfig,
    ProviderHealth, ProviderHealthInfo, ProviderInfo, ProviderType, TaskType,
};
use crate::model_router::failure_class::FailureClass;
use crate::model_router::model_catalog::{ModelCatalog, ModelCatalogEntry};
use crate::model_router::oauth_credential::Credential;
use crate::model_router::usage_fetcher::{
    LocalBudgetFetcher, OpenAiUsageFetcher, UsageFetcher, UsageFetcherRegistry,
};
use crate::model_router::usage_tracker::{
    ProviderUsageSnapshot, ProviderUsageTracker, UsageQuota, UsageTrackerConfig,
};
use crate::observe::record::RouteRecord;
use crate::providers::{
    CompletionRequest, CompletionResponse, CompletionStream, Message, Provider, ToolDefinition,
    Usage,
};

// ------------------------------------------------------------------
// ModelRouter
// ------------------------------------------------------------------

/// Model router for multi-provider LLM routing.
///
/// # Lock ordering
///
/// To avoid deadlocks, acquire locks in this order and release them before
/// any `.await` that does not strictly need the lock:
///
/// 1. `config`
/// 2. `providers`
/// 3. `health`
/// 4. `usage_fetchers`
///
/// Write locks must never be held across an `.await`.
pub struct ModelRouter {
    /// Configuration
    config: RwLock<ModelRouterConfig>,
    /// Provider instances
    providers: RwLock<HashMap<String, Arc<dyn Provider + Send + Sync>>>,
    /// Health tracking per provider
    health: RwLock<HashMap<String, ProviderHealth>>,
    /// Active fallback chains
    fallback_chains: RwLock<HashMap<String, Vec<FallbackEntry>>>,
    /// Auth profile manager for API key rotation
    pub auth_profiles: AuthProfileManager,
    /// Per-provider usage tracker
    pub usage_tracker: ProviderUsageTracker,
    /// Dynamic model catalog with discovery and suppression
    pub model_catalog: ModelCatalog,
    /// Remote usage quota fetchers keyed by provider name.
    pub usage_fetchers: RwLock<UsageFetcherRegistry>,
    /// Optional SQLite pool for persisting auth profile state across restarts.
    db_pool: Option<sqlx::Pool<sqlx::Sqlite>>,
    /// Optional task registry for tracking the health-check background task.
    task_registry: Option<Arc<TaskRegistry>>,
    /// Shutdown token for cancelling the health-check background task.
    shutdown_token: CancellationToken,
    /// Pluggable task classifier for cost-aware routing.
    classifier: Box<dyn TaskClassifierImpl>,
}

impl Default for ModelRouter {
    fn default() -> Self {
        Self::new(ModelRouterConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::gateway::task_registry::TaskRegistry;
    use crate::model_router::auth_profile::{AuthKeyConfig, AuthProfileConfig};
    use crate::model_router::config::{
        CircuitState, CostAwareConfig, ModelRouterConfig, ProviderConfig, ProviderType,
    };
    use crate::model_router::model_catalog::ModelCatalogEntry;
    use crate::model_router::usage_fetcher::UsageFetcher;
    use crate::model_router::usage_tracker::{QuotaSource, UsageQuota};
    use crate::providers::{
        CompletionRequest, CompletionResponse, CompletionStream, Message, Provider, Usage,
    };

    /// Build a test provider config that owns the given model IDs.
    fn test_provider_config(models: &[&str]) -> ProviderConfig {
        ProviderConfig {
            provider_type: ProviderType::Anthropic,
            models: models.iter().map(|s| s.to_string()).collect(),
            default_model: models.first().map(|s| s.to_string()).unwrap_or_default(),
            api_key: "test-key".to_string().into(),
            api_keys: vec![],
            auth_profile: None,
            oauth: None,
            base_url: None,
            timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay_ms: 1000,
        }
    }

    #[tokio::test]
    async fn default_config_has_no_providers() {
        let config = ModelRouterConfig::default();
        assert!(config.providers.is_empty());
        assert_eq!(config.default_model, "");
    }

    #[tokio::test]
    async fn models_with_providers_returns_provider_model_pairs() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        router
            .add_provider("openai", test_provider_config(&["gpt-4o", "gpt-4-turbo"]))
            .await
            .unwrap();
        router
            .add_provider("anthropic", test_provider_config(&["claude-3-5-sonnet"]))
            .await
            .unwrap();

        let models = router.models_with_providers().await;
        assert_eq!(models.len(), 3);
        assert!(models.contains(&("openai".to_string(), "gpt-4o".to_string())));
        assert!(models.contains(&("openai".to_string(), "gpt-4-turbo".to_string())));
        assert!(models.contains(&("anthropic".to_string(), "claude-3-5-sonnet".to_string())));
    }

    #[tokio::test]
    async fn models_with_providers_attributes_duplicated_ids_deterministically() {
        // A direct vendor config and the cloud proxy can serve the same
        // model ids. Attribution must not depend on HashMap iteration
        // order: the direct provider claims duplicated ids and cloud only
        // contributes models unique to it.
        let router = ModelRouter::new(ModelRouterConfig::default());
        router
            .add_provider(
                "cloud",
                test_provider_config(&["deepseek-v4-flash", "deepseek-v4-flash-vision-exp"]),
            )
            .await
            .unwrap();
        router
            .add_provider(
                "deepseek",
                test_provider_config(&["deepseek-v4-flash", "deepseek-v4-pro"]),
            )
            .await
            .unwrap();

        let models = router.models_with_providers().await;
        assert!(models.contains(&("deepseek".to_string(), "deepseek-v4-flash".to_string())));
        assert!(models.contains(&("deepseek".to_string(), "deepseek-v4-pro".to_string())));
        assert!(models.contains(&("cloud".to_string(), "deepseek-v4-flash-vision-exp".to_string())));
        assert!(!models.contains(&("cloud".to_string(), "deepseek-v4-flash".to_string())));

        // Routing resolution follows the same convention.
        assert_eq!(
            router
                .provider_for_model("deepseek-v4-flash")
                .await
                .as_deref(),
            Some("deepseek")
        );
        assert_eq!(
            router
                .provider_for_model("deepseek-v4-flash-vision-exp")
                .await
                .as_deref(),
            Some("cloud")
        );
    }

    #[tokio::test]
    async fn create_default_provider_prefers_non_cloud() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        router
            .add_provider("cloud", test_provider_config(&["cloud-only"]))
            .await
            .unwrap();
        router
            .add_provider("deepseek", test_provider_config(&["deepseek-only"]))
            .await
            .unwrap();

        let default_provider = router.create_default_provider().await.unwrap();
        let deepseek_provider = router.get_provider("deepseek").await.unwrap();
        assert!(
            std::sync::Arc::ptr_eq(&default_provider, &deepseek_provider),
            "default provider must deterministically be the non-cloud one"
        );
    }

    #[tokio::test]
    async fn add_provider_rejects_duplicate_name() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        router
            .add_provider("openai", test_provider_config(&["gpt-4o"]))
            .await
            .unwrap();
        let result = router
            .add_provider("openai", test_provider_config(&["gpt-4"]))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn update_provider_replaces_models_and_keeps_provider() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        router
            .add_provider("openai", test_provider_config(&["gpt-4o"]))
            .await
            .unwrap();

        // Disable the provider, then update it: health state must survive.
        router.disable_provider("openai").await.unwrap();
        let updated = test_provider_config(&["gpt-4o", "gpt-4-turbo"]);
        router.update_provider("openai", updated).await.unwrap();

        let pairs = router.models_with_providers().await;
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&("openai".to_string(), "gpt-4o".to_string())));
        assert!(pairs.contains(&("openai".to_string(), "gpt-4-turbo".to_string())));

        let health = router.get_provider_health("openai").await.unwrap();
        assert_eq!(health.state, "Open");
    }

    #[tokio::test]
    async fn update_provider_drops_removed_models_from_catalog() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        router
            .add_provider("openai", test_provider_config(&["gpt-4o", "gpt-4-turbo"]))
            .await
            .unwrap();
        // Discover so catalog entries exist for both models.
        router.model_catalog.discover().await.unwrap();
        assert!(router.provider_for_model("gpt-4-turbo").await.is_some());

        // Shrink the provider's model list. The dropped model must not linger
        // in the catalog (the update replaces the static source) or resolve.
        let updated = test_provider_config(&["gpt-4o"]);
        router.update_provider("openai", updated).await.unwrap();

        let pairs = router.models_with_providers().await;
        assert_eq!(pairs.len(), 1);
        assert!(!pairs.contains(&("openai".to_string(), "gpt-4-turbo".to_string())));
        assert!(router.provider_for_model("gpt-4-turbo").await.is_none());
        assert!(router.provider_for_model("gpt-4o").await.is_some());
    }

    #[tokio::test]
    async fn update_provider_rejects_unknown_provider() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        let result = router
            .update_provider("openai", test_provider_config(&["gpt-4o"]))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn provider_exists_reports_registration() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        assert!(!router.provider_exists("openai").await);
        router
            .add_provider("openai", test_provider_config(&["gpt-4o"]))
            .await
            .unwrap();
        assert!(router.provider_exists("openai").await);
        assert!(!router.provider_exists("anthropic").await);
    }

    #[tokio::test]
    async fn router_config_returns_clone() {
        let mut config = ModelRouterConfig::default();
        config.default_model = "claude-3".to_string();
        let router = ModelRouter::new(config);

        let snapshot = router.router_config().await;
        assert_eq!(snapshot.default_model, "claude-3");
    }

    #[tokio::test]
    async fn switch_default_model_changes_default() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        router
            .add_provider("openai", test_provider_config(&["gpt-3.5", "gpt-4"]))
            .await
            .unwrap();
        router.switch_default_model("gpt-3.5").await.unwrap();

        let default = router.get_default_model().await;
        assert_eq!(default, "gpt-3.5");
    }

    #[tokio::test]
    async fn switch_default_model_rejects_unknown_model_id() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        let result = router.switch_default_model("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn list_providers_returns_empty_when_none() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        let providers = router.list_providers().await;
        assert_eq!(providers.len(), 0);
    }

    #[tokio::test]
    async fn add_and_remove_provider() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        let config = test_provider_config(&["test-model"]);

        router.add_provider("test-provider", config).await.unwrap();
        let providers = router.list_providers().await;
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name, "test-provider");

        router.remove_provider("test-provider").await.unwrap();
        let providers = router.list_providers().await;
        assert_eq!(providers.len(), 0);
    }

    #[tokio::test]
    async fn enable_disable_provider() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        let config = test_provider_config(&["test-model"]);

        router.add_provider("p1", config).await.unwrap();

        let providers = router.list_providers().await;
        assert!(providers[0].enabled);
        assert_eq!(providers[0].circuit_state, CircuitState::Closed);

        router.disable_provider("p1").await.unwrap();
        let providers = router.list_providers().await;
        assert!(!providers[0].enabled);
        assert_eq!(providers[0].circuit_state, CircuitState::Open);

        router.enable_provider("p1").await.unwrap();
        let providers = router.list_providers().await;
        assert!(providers[0].enabled);
        assert_eq!(providers[0].circuit_state, CircuitState::Closed);
    }

    #[tokio::test]
    async fn enable_unknown_provider_fails() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        let result = router.enable_provider("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fallback_chain_roundtrip_by_model_id() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        router
            .add_provider("anthropic", test_provider_config(&["claude-3"]))
            .await
            .unwrap();
        router
            .set_fallback_chain("claude-3", vec!["p1".to_string(), "p2".to_string()])
            .await
            .unwrap();

        let chain = router.get_fallback_chain("claude-3").await;
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0], "p1");
        assert_eq!(chain[1], "p2");
    }

    #[tokio::test]
    async fn fallback_chain_for_unknown_model_returns_empty() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        let chain = router.get_fallback_chain("nonexistent").await;
        assert!(chain.is_empty());
    }

    #[tokio::test]
    async fn get_provider_health_returns_info() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        let config = test_provider_config(&["test-model"]);

        router.add_provider("p1", config).await.unwrap();

        let health = router.get_provider_health("p1").await;
        assert!(health.is_some());
        let info = health.unwrap();
        assert_eq!(info.state, "Closed");
        assert_eq!(info.failures, 0);
        assert_eq!(info.successes, 0);
        assert_eq!(info.avg_latency_ms, 0);
    }

    #[tokio::test]
    async fn get_provider_health_unknown_returns_none() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        let health = router.get_provider_health("nonexistent").await;
        assert!(health.is_none());
    }

    #[tokio::test]
    async fn check_provider_health_unknown_fails() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        let result = router.check_provider_health("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn provider_list_includes_circuit_state() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        let config = test_provider_config(&["test-model"]);

        router.add_provider("p1", config).await.unwrap();
        let providers = router.list_providers().await;
        assert_eq!(providers[0].circuit_state, CircuitState::Closed);

        router.disable_provider("p1").await.unwrap();
        let providers = router.list_providers().await;
        assert_eq!(providers[0].circuit_state, CircuitState::Open);
    }

    #[tokio::test]
    async fn fallback_chain_set_and_clear() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        router
            .add_provider("anthropic", test_provider_config(&["claude-3"]))
            .await
            .unwrap();

        router
            .set_fallback_chain("claude-3", vec!["a".to_string(), "b".to_string()])
            .await
            .unwrap();
        let chain = router.get_fallback_chain("claude-3").await;
        assert_eq!(chain, vec!["a", "b"]);

        router.set_fallback_chain("claude-3", vec![]).await.unwrap();
        let chain = router.get_fallback_chain("claude-3").await;
        assert!(chain.is_empty());
    }

    #[tokio::test]
    async fn set_fallback_chain_unknown_model_fails() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        let result = router
            .set_fallback_chain("nonexistent", vec!["a".to_string()])
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn switch_default_model_persists() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        router
            .add_provider("openai", test_provider_config(&["gpt-3.5", "gpt-4"]))
            .await
            .unwrap();
        router
            .add_provider("anthropic", test_provider_config(&["claude-3", "claude-3-opus"]))
            .await
            .unwrap();

        router.switch_default_model("gpt-3.5").await.unwrap();
        assert_eq!(router.get_default_model().await, "gpt-3.5");

        router.switch_default_model("claude-3-opus").await.unwrap();
        assert_eq!(router.get_default_model().await, "claude-3-opus");
    }

    #[tokio::test]
    async fn default_model_is_empty_by_default() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        let default = router.get_default_model().await;
        assert_eq!(default, "");
    }

    #[tokio::test]
    async fn cost_aware_disabled_by_default() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        let spend = router.get_daily_spend().await;
        assert_eq!(spend, 0.0);
    }

    #[tokio::test]
    async fn cost_aware_config_default_routing_rules() {
        let config = CostAwareConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.model_costs.len(), 3);
        assert_eq!(config.routing_rules.len(), 9);
        assert_eq!(config.default_model, "default");
    }

    #[tokio::test]
    async fn reset_daily_spend_no_op_when_disabled() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        router.reset_daily_spend().await;
        assert_eq!(router.get_daily_spend().await, 0.0);
    }

    #[tokio::test]
    async fn cost_aware_config_with_enabled_spend_tracking() {
        let mut config = ModelRouterConfig::default();
        let mut cost_aware = CostAwareConfig::default();
        cost_aware.enabled = true;
        cost_aware.daily_spend_usd = 5.0;
        config.cost_aware = Some(cost_aware);

        let router = ModelRouter::new(config);
        assert_eq!(router.get_daily_spend().await, 5.0);

        router.reset_daily_spend().await;
        assert_eq!(router.get_daily_spend().await, 0.0);
    }

    #[tokio::test]
    async fn health_check_uses_health_check_not_complete() {
        #[derive(Clone)]
        struct TrackingProvider {
            complete_count: Arc<Mutex<usize>>,
            health_check_count: Arc<Mutex<usize>>,
        }

        impl TrackingProvider {
            fn new() -> Self {
                Self {
                    complete_count: Arc::new(Mutex::new(0)),
                    health_check_count: Arc::new(Mutex::new(0)),
                }
            }
        }

        #[async_trait]
        impl Provider for TrackingProvider {
            fn name(&self) -> &str {
                "tracking"
            }
            fn default_model(&self) -> &str {
                "tracking-model"
            }
            fn supports_tools(&self) -> bool {
                false
            }
            fn max_context(&self) -> usize {
                4096
            }
            async fn complete(
                &self,
                _request: CompletionRequest,
            ) -> crate::Result<CompletionResponse> {
                *self.complete_count.lock().unwrap() += 1;
                Ok(CompletionResponse {
                    message: Message::assistant("ok"),
                    model: "tracking-model".to_string(),
                    usage: Some(Usage::default()),
                    finish_reason: Some("stop".to_string()),
                })
            }
            async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
                unimplemented!()
            }
            async fn health_check(&self) -> crate::Result<bool> {
                *self.health_check_count.lock().unwrap() += 1;
                Ok(true)
            }
            async fn set_credential(
                &self,
                _credential: crate::model_router::Credential,
            ) -> crate::Result<()> {
                Ok(())
            }
        }

        let mut config = ModelRouterConfig::default();
        config.health_check_interval_secs = 1;
        let registry = Arc::new(TaskRegistry::new());
        let token = CancellationToken::new();
        let router = Arc::new(
            ModelRouter::new(config)
                .with_task_registry(registry.clone())
                .with_shutdown_token(token.clone()),
        );

        let provider = TrackingProvider::new();
        router
            .add_provider_instance("tracking", Arc::new(provider.clone()))
            .await
            .unwrap();

        router.clone().start_health_checks();
        tokio::time::sleep(Duration::from_millis(2500)).await;
        token.cancel();

        assert!(*provider.health_check_count.lock().unwrap() > 0);
        assert_eq!(*provider.complete_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn health_check_task_respects_shutdown() {
        #[derive(Clone)]
        struct HealthyProvider;

        #[async_trait]
        impl Provider for HealthyProvider {
            fn name(&self) -> &str {
                "healthy"
            }
            fn default_model(&self) -> &str {
                "healthy-model"
            }
            fn supports_tools(&self) -> bool {
                false
            }
            fn max_context(&self) -> usize {
                4096
            }
            async fn complete(
                &self,
                _request: CompletionRequest,
            ) -> crate::Result<CompletionResponse> {
                Ok(CompletionResponse {
                    message: Message::assistant("ok"),
                    model: "healthy-model".to_string(),
                    usage: Some(Usage::default()),
                    finish_reason: Some("stop".to_string()),
                })
            }
            async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
                unimplemented!()
            }
            async fn health_check(&self) -> crate::Result<bool> {
                Ok(true)
            }
            async fn set_credential(
                &self,
                _credential: crate::model_router::Credential,
            ) -> crate::Result<()> {
                Ok(())
            }
        }

        let mut config = ModelRouterConfig::default();
        config.health_check_interval_secs = 60;
        let registry = Arc::new(TaskRegistry::new());
        let token = CancellationToken::new();
        let router = Arc::new(
            ModelRouter::new(config)
                .with_task_registry(registry.clone())
                .with_shutdown_token(token.clone()),
        );

        router
            .add_provider_instance("healthy", Arc::new(HealthyProvider))
            .await
            .unwrap();

        router.clone().start_health_checks();

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(registry.contains("model_router:health_check").await);

        token.cancel();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let handle = registry
            .remove_join_or_abort("model_router:health_check")
            .await;
        assert!(handle.is_some());
        let result = tokio::time::timeout(Duration::from_secs(2), handle.unwrap()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn auth_failure_rotates_key_and_retries_successfully() {
        #[derive(Clone)]
        struct RotatingProvider {
            calls: Arc<Mutex<usize>>,
            rotated_key: Arc<Mutex<Option<String>>>,
        }

        impl RotatingProvider {
            fn new() -> Self {
                Self {
                    calls: Arc::new(Mutex::new(0)),
                    rotated_key: Arc::new(Mutex::new(None)),
                }
            }
        }

        #[async_trait]
        impl Provider for RotatingProvider {
            fn name(&self) -> &str {
                "rotator"
            }
            fn default_model(&self) -> &str {
                "rotator-model"
            }
            fn supports_tools(&self) -> bool {
                false
            }
            fn max_context(&self) -> usize {
                4096
            }
            async fn complete(
                &self,
                _request: CompletionRequest,
            ) -> crate::Result<CompletionResponse> {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                if *calls == 1 {
                    Err(crate::error::SyscityError::ExternalService {
                        source: "OpenAI API error 401: Unauthorized".into(),
                        cause: None,
                    })
                } else {
                    Ok(CompletionResponse {
                        message: Message::assistant("ok"),
                        model: "rotator-model".to_string(),
                        usage: Some(Usage::default()),
                        finish_reason: Some("stop".to_string()),
                    })
                }
            }
            async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
                unimplemented!()
            }
            async fn health_check(&self) -> crate::Result<bool> {
                Ok(true)
            }
            async fn set_credential(
                &self,
                credential: crate::model_router::Credential,
            ) -> crate::Result<()> {
                if let crate::model_router::Credential::ApiKey { key } = credential {
                    *self.rotated_key.lock().unwrap() = Some(key);
                }
                Ok(())
            }
        }

        let router = ModelRouter::new(ModelRouterConfig::default());
        let provider = Arc::new(RotatingProvider::new());
        router
            .add_provider_instance("rotator", provider.clone())
            .await
            .unwrap();

        let auth_config = AuthProfileConfig {
            keys: vec![
                AuthKeyConfig {
                    key: "first-key".to_string(),
                    label: "first".to_string(),
                },
                AuthKeyConfig {
                    key: "second-key".to_string(),
                    label: "second".to_string(),
                },
            ],
            cooldown_secs: 60,
            max_failures: 3,
        };
        router
            .auth_profiles
            .register_from_config("rotator", &auth_config)
            .await;

        router
            .model_catalog
            .register(ModelCatalogEntry::new("rotator-model", "rotator-model", "rotator"))
            .await;

        let response = router
            .complete("rotator-model", vec![Message::user("hi")], None)
            .await
            .expect("complete should succeed after key rotation");
        assert_eq!(response.message.content, "ok");
        assert_eq!(*provider.calls.lock().unwrap(), 2);
        assert_eq!(provider.rotated_key.lock().unwrap().as_deref(), Some("second-key"));
    }

    #[tokio::test]
    async fn content_policy_failure_is_not_retried() {
        #[derive(Clone)]
        struct ContentPolicyProvider {
            calls: Arc<Mutex<usize>>,
        }

        #[async_trait]
        impl Provider for ContentPolicyProvider {
            fn name(&self) -> &str {
                "content-policy"
            }
            fn default_model(&self) -> &str {
                "model"
            }
            fn supports_tools(&self) -> bool {
                false
            }
            fn max_context(&self) -> usize {
                4096
            }
            async fn complete(
                &self,
                _request: CompletionRequest,
            ) -> crate::Result<CompletionResponse> {
                *self.calls.lock().unwrap() += 1;
                Err(crate::error::SyscityError::ExternalService {
                    source: "OpenAI API error 400: Content policy violation".into(),
                    cause: None,
                })
            }
            async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
                unimplemented!()
            }
            async fn health_check(&self) -> crate::Result<bool> {
                Ok(true)
            }
            async fn set_credential(
                &self,
                _credential: crate::model_router::Credential,
            ) -> crate::Result<()> {
                Ok(())
            }
        }

        let router = ModelRouter::new(ModelRouterConfig::default());
        let provider = Arc::new(ContentPolicyProvider { calls: Arc::new(Mutex::new(0)) });
        router
            .add_provider_instance("content-policy", provider.clone())
            .await
            .unwrap();

        router
            .model_catalog
            .register(ModelCatalogEntry::new("model", "model", "content-policy"))
            .await;

        let result = router
            .complete("model", vec![Message::user("hi")], None)
            .await;
        assert!(result.is_err());
        assert_eq!(*provider.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn concurrent_complete_calls_do_not_deadlock() {
        #[derive(Clone)]
        struct SlowProvider {
            calls: Arc<Mutex<usize>>,
        }

        #[async_trait]
        impl Provider for SlowProvider {
            fn name(&self) -> &str {
                "slow"
            }
            fn default_model(&self) -> &str {
                "slow-model"
            }
            fn supports_tools(&self) -> bool {
                false
            }
            fn max_context(&self) -> usize {
                4096
            }
            async fn complete(
                &self,
                _request: CompletionRequest,
            ) -> crate::Result<CompletionResponse> {
                tokio::time::sleep(Duration::from_millis(50)).await;
                *self.calls.lock().unwrap() += 1;
                Ok(CompletionResponse {
                    message: Message::assistant("ok"),
                    model: "slow-model".to_string(),
                    usage: Some(Usage::default()),
                    finish_reason: Some("stop".to_string()),
                })
            }
            async fn stream(&self, _request: CompletionRequest) -> crate::Result<CompletionStream> {
                unimplemented!()
            }
            async fn health_check(&self) -> crate::Result<bool> {
                Ok(true)
            }
            async fn set_credential(
                &self,
                _credential: crate::model_router::Credential,
            ) -> crate::Result<()> {
                Ok(())
            }
        }

        let router = Arc::new(ModelRouter::new(ModelRouterConfig::default()));
        let provider = Arc::new(SlowProvider { calls: Arc::new(Mutex::new(0)) });
        router
            .add_provider_instance("slow", provider.clone())
            .await
            .unwrap();
        router
            .model_catalog
            .register(ModelCatalogEntry::new("slow-model", "slow-model", "slow"))
            .await;

        let r1 = router.clone();
        let r2 = router.clone();
        let (first, second) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(2), async move {
                r1.complete("slow-model", vec![Message::user("a")], None)
                    .await
            }),
            tokio::time::timeout(Duration::from_secs(2), async move {
                r2.complete("slow-model", vec![Message::user("b")], None)
                    .await
            }),
        );

        assert!(first.is_ok(), "first complete timed out (possible deadlock)");
        assert!(second.is_ok(), "second complete timed out (possible deadlock)");
        assert!(first.unwrap().is_ok());
        assert!(second.unwrap().is_ok());
        assert_eq!(*provider.calls.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn all_snapshots_with_quota_fetches_for_all_providers() {
        #[derive(Clone)]
        struct MockUsageFetcher {
            remaining: f64,
        }

        #[async_trait]
        impl UsageFetcher for MockUsageFetcher {
            fn provider(&self) -> &str {
                "mock"
            }

            async fn fetch(&self) -> crate::Result<Option<UsageQuota>> {
                Ok(Some(UsageQuota {
                    remaining: self.remaining,
                    limit: 100.0,
                    reset_at: None,
                    unit: "usd".to_string(),
                    source: QuotaSource::Remote,
                }))
            }
        }

        let router = ModelRouter::new(ModelRouterConfig::default());
        let usage = Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            ..Default::default()
        };
        router.usage_tracker.record("openai", usage, "gpt-4o").await;
        router
            .usage_tracker
            .record("anthropic", usage, "claude-3-opus")
            .await;

        {
            let mut fetchers = router.usage_fetchers.write().await;
            fetchers.register("openai", Arc::new(MockUsageFetcher { remaining: 42.0 }));
            fetchers.register("anthropic", Arc::new(MockUsageFetcher { remaining: 7.0 }));
        }

        let snapshots = router.all_snapshots_with_quota().await;
        assert_eq!(snapshots.len(), 2);

        let openai = snapshots.iter().find(|s| s.provider == "openai").unwrap();
        let anthropic = snapshots
            .iter()
            .find(|s| s.provider == "anthropic")
            .unwrap();

        assert_eq!(openai.quota.as_ref().unwrap().remaining, 42.0);
        assert_eq!(anthropic.quota.as_ref().unwrap().remaining, 7.0);
    }

    #[tokio::test]
    async fn record_probe_failure_does_not_reset_half_open_cooldown_while_open() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        router
            .add_provider("p", test_provider_config(&["m"]))
            .await
            .unwrap();

        let stale = chrono::Utc::now() - chrono::Duration::seconds(290);
        {
            let mut health = router.health.write().await;
            let h = health.get_mut("p").unwrap();
            h.state = CircuitState::Open;
            h.failures = 5;
            h.last_failure = Some(stale);
        }

        // While Open, a failed probe must NOT refresh last_failure — the
        // Open→HalfOpen cooldown keeps running so real traffic can trial.
        router.record_probe_failure("p").await;

        let health = router.health.read().await;
        let h = health.get("p").unwrap();
        assert_eq!(h.state, CircuitState::Open);
        assert_eq!(h.last_failure, Some(stale));
    }

    #[tokio::test]
    async fn record_probe_failure_opens_breaker_from_closed() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        router
            .add_provider("p", test_provider_config(&["m"]))
            .await
            .unwrap();

        // Below the default threshold (5): counts but stays Closed.
        router.record_probe_failure("p").await;
        {
            let health = router.health.read().await;
            let h = health.get("p").unwrap();
            assert_eq!(h.state, CircuitState::Closed);
            assert!(h.last_failure.is_some());
        }

        for _ in 0..4 {
            router.record_probe_failure("p").await;
        }
        let health = router.health.read().await;
        assert_eq!(health.get("p").unwrap().state, CircuitState::Open);
    }

    #[tokio::test]
    async fn record_probe_failure_reopens_breaker_from_half_open() {
        let router = ModelRouter::new(ModelRouterConfig::default());
        router
            .add_provider("p", test_provider_config(&["m"]))
            .await
            .unwrap();

        {
            let mut health = router.health.write().await;
            let h = health.get_mut("p").unwrap();
            h.state = CircuitState::HalfOpen;
            h.failures = 5;
            h.last_failure = Some(chrono::Utc::now() - chrono::Duration::seconds(290));
        }

        // HalfOpen + failed probe → re-open and restart the cooldown.
        router.record_probe_failure("p").await;

        let health = router.health.read().await;
        let h = health.get("p").unwrap();
        assert_eq!(h.state, CircuitState::Open);
        assert!(h.last_failure.unwrap() > chrono::Utc::now() - chrono::Duration::seconds(10));
    }
}
