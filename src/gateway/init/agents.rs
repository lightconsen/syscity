//! Agent subsystem initialization.
//!
//! Builds the ACP control plane, model router, skill manager, agent registry,
//! and session manager that make up the agent runtime.

use std::sync::Arc;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::acp::AcpControlPlane;
use crate::agent::session_store::SessionStore;
use crate::agent::{AgentBuilder, AgentRegistry, SessionManager};
use crate::gateway::task_registry::TaskRegistry;
use crate::gateway::GatewayConfig;
use crate::model_router::{ModelRouter, ModelRouterConfig};
use crate::skills::SkillManager;
use crate::tools::ToolRegistry;

/// Agent subsystem initialization result.
pub struct AgentsInit {
    pub acp: Arc<AcpControlPlane>,
    pub model_router: Arc<ModelRouter>,
    pub skills_manager: Arc<RwLock<SkillManager>>,
    pub agent_registry: Arc<RwLock<AgentRegistry>>,
    pub session_manager: Arc<RwLock<SessionManager>>,
}

/// Initialize the ACP control plane.
pub async fn init_acp(
    config: &GatewayConfig,
    session_store: Option<Arc<SessionStore>>,
) -> Arc<AcpControlPlane> {
    let acp_max_iter = config.acp.max_iterations;
    let acp = if let Some(ref store) = session_store {
        info!("Wiring ACP control plane to SessionStore for persistent subagent sessions");
        Arc::new(AcpControlPlane::new(acp_max_iter).with_store(store.clone()))
    } else {
        if config.storage.storage_type == "sqlite" {
            warn!(
                "Storage type is 'database' but no SessionStore is available; ACP subagent \
                 sessions will not persist. Check the SQLite pool configuration."
            );
        } else {
            info!("ACP control plane running without SessionStore (ephemeral subagent sessions)");
        }
        Arc::new(AcpControlPlane::new(acp_max_iter))
    };
    acp.load_persisted_sessions().await;
    acp
}

/// Initialize the model router and configure providers from config.
pub async fn init_model_router(
    config: &GatewayConfig,
    task_registry: Arc<TaskRegistry>,
    shutdown_token: CancellationToken,
    secrets: Arc<crate::secrets::SecretStoreHandle>,
) -> Arc<ModelRouter> {
    // The default model is a concrete model ID owned by a provider.
    let model_router_config = ModelRouterConfig {
        default_model: config.model.clone(),
        ..Default::default()
    };

    let model_router = Arc::new(
        ModelRouter::new(model_router_config)
            .with_task_registry(task_registry)
            .with_shutdown_token(shutdown_token)
            .with_secrets(secrets),
    );
    for (name, provider_config) in &config.providers {
        info!("Configuring provider: {}", name);
        let mut provider_config = provider_config.clone();
        // Migration: providers that predate per-provider model lists get their
        // models and default model backfilled from the built-in preset.
        if provider_config.models.is_empty() {
            let key = crate::providers::preset::canonical_provider_name(name.as_str());
            if let Some(preset) = crate::model_router::provider_presets().get(key) {
                provider_config.models = preset.models.clone();
                provider_config.default_model = preset.models.first().cloned().unwrap_or_default();
                warn!(
                    "Provider '{}' had no models; backfilled {} from preset",
                    name,
                    provider_config.models.len()
                );
            }
        }
        if let Err(e) = model_router.add_provider(name, provider_config).await {
            warn!("Failed to add provider '{}': {}", name, e);
        }
    }

    // Migration: a default model that no provider owns (e.g. a legacy alias
    // name) falls back to the first provider's default model.
    if model_router
        .provider_for_model(&config.model)
        .await
        .is_none()
    {
        if let Some((_, first_model)) = model_router
            .models_with_providers()
            .await
            .into_iter()
            .next()
        {
            warn!(
                "Default model '{}' is not owned by any provider; falling back to '{}'",
                config.model, first_model
            );
            if let Err(e) = model_router.switch_default_model(&first_model).await {
                warn!("Failed to switch default model to '{}': {}", first_model, e);
            }
        }
    }

    // Initialize fallback chains and model catalog from config.
    model_router.init_catalog_and_chains().await;

    model_router
}

/// Migrate a `GatewayConfig` from the alias era to the provider-owned-model
/// schema, in place. Older configs did not store per-provider model lists
/// (the "default" alias was synthesized in memory at startup) and could
/// persist an alias name (e.g. "default") as the default model or as an agent
/// binding. This rewrites:
///   - providers with an empty model list -> backfilled from the built-in
///     preset (preserving the preset's default base URL / protocol)
///   - a provider `default_model` not listed in its own `models` -> first
///     listed model
///   - a `model` no provider owns -> the owning provider recorded in
///     `model_provider`, else the first provider's default model
///     (`model_provider` is re-derived from the final `model`)
///   - `agent_models` values no provider owns -> the (now valid) global
///     default model
///
/// Returns `true` if any field changed, so callers can persist once.
pub async fn migrate_model_router_config(config: &mut GatewayConfig) -> bool {
    let mut changed = false;

    // 1. Backfill per-provider model lists and fix provider default models.
    for (name, pcfg) in config.providers.iter_mut() {
        if pcfg.models.is_empty() {
            let key = crate::providers::preset::canonical_provider_name(name.as_str());
            if let Some(preset) = crate::model_router::provider_presets().get(key) {
                pcfg.models = preset.models.clone();
                pcfg.default_model = preset.models.first().cloned().unwrap_or_default();
                changed = true;
                warn!(
                    "Provider '{}' had no models; backfilled {} from preset",
                    name,
                    pcfg.models.len()
                );
            }
        }
        if pcfg.default_model.is_empty() || !pcfg.models.contains(&pcfg.default_model) {
            if let Some(first) = pcfg.models.first().cloned() {
                pcfg.default_model = first;
                changed = true;
            }
        }
    }

    // 2. The default model must be owned by a provider. Prefer the provider
    // recorded in `model_provider` when it is still valid, else the first
    // provider with models.
    if config.provider_for_model(&config.model).is_none() {
        let fallback = config
            .providers
            .get(&config.model_provider)
            .map(|p| p.default_model().to_string())
            .filter(|m| !m.is_empty())
            .or_else(|| {
                config
                    .providers
                    .iter()
                    .find(|(_, p)| !p.models.is_empty())
                    .map(|(_, p)| p.default_model().to_string())
            });
        if let Some(fallback) = fallback {
            warn!(
                "Default model '{}' is not owned by any provider; falling back to '{}'",
                config.model, fallback
            );
            config.model = fallback;
            changed = true;
        }
    }
    if let Some(provider) = config.provider_for_model(&config.model) {
        if config.model_provider != provider {
            config.model_provider = provider.to_string();
            changed = true;
        }
    }

    // 3. Agent bindings must reference a model some provider owns. A legacy
    // alias value (e.g. "default") is rewritten to the global default so the
    // binding never points at a model the router cannot resolve.
    let owned_models: std::collections::HashSet<String> = config
        .providers
        .values()
        .flat_map(|p| p.models.iter().cloned())
        .collect();
    for (agent_id, model) in config.agent_models.iter_mut() {
        if !owned_models.contains(model) {
            warn!(
                "Agent '{}' model '{}' is not owned by any provider; falling back to '{}'",
                agent_id, model, config.model
            );
            *model = config.model.clone();
            changed = true;
        }
    }

    changed
}

/// Configure the ACP default agent builder after provider and tools are ready.
pub async fn configure_acp_agent_builder(
    acp: &AcpControlPlane,
    config: &GatewayConfig,
    model_router: Arc<ModelRouter>,
    tool_registry: Arc<ToolRegistry>,
    skills_manager: Arc<RwLock<SkillManager>>,
) {
    if let Ok(default_provider) = model_router.create_default_provider().await {
        let mut default_agent_config = config.default_agent.clone();
        default_agent_config.workspace_dir = config
            .workspace_dir
            .as_ref()
            .map(crate::dirs::resolve_tilde);
        default_agent_config.workspace_only = config.workspace_only;
        let provider_clone = default_provider.clone();
        let model_router_clone = model_router.clone();
        let default_model = config.model.clone();
        let skills_manager_clone = Arc::clone(&skills_manager);
        acp.set_agent_builder(move |subagent_id| {
            let mut cfg = default_agent_config.clone();
            cfg.agent_id = Some(subagent_id.to_string());
            AgentBuilder::new()
                .config(cfg)
                .provider(provider_clone.clone())
                .tools(tool_registry.clone())
                .model_router(model_router_clone.clone())
                .model(default_model.clone())
                .planner_model(default_model.clone())
                .skill_manager(Arc::clone(&skills_manager_clone))
                .build()
        })
        .await;
    } else {
        warn!(
            "No default LLM provider available — ACP subagent spawning will fail until a provider \
             is configured"
        );
    }
}

/// Initialize skill manager, agent registry, and session manager.
pub async fn init_agent_state(
    paths: std::sync::Arc<crate::dirs::SyscityPaths>,
) -> crate::Result<(
    Arc<RwLock<SkillManager>>,
    Arc<RwLock<AgentRegistry>>,
    Arc<RwLock<SessionManager>>,
)> {
    let skills_manager = Arc::new(RwLock::new(SkillManager::new(paths).await?));
    let agent_registry = Arc::new(RwLock::new(AgentRegistry::new()));
    let session_manager = Arc::new(RwLock::new(SessionManager::new()));
    Ok((skills_manager, agent_registry, session_manager))
}

/// Convenience helper that wires the ACP builder and returns the agent
/// subsystem bundle.
pub async fn init_agents(
    config: &GatewayConfig,
    session_store: Option<Arc<SessionStore>>,
    tool_registry: Arc<ToolRegistry>,
    task_registry: Arc<crate::gateway::task_registry::TaskRegistry>,
    shutdown_token: CancellationToken,
    secrets: Arc<crate::secrets::SecretStoreHandle>,
    paths: std::sync::Arc<crate::dirs::SyscityPaths>,
) -> crate::Result<AgentsInit> {
    let acp = init_acp(config, session_store).await;
    let model_router = init_model_router(config, task_registry, shutdown_token, secrets).await;
    let (skills_manager, agent_registry, session_manager) = init_agent_state(paths).await?;

    configure_acp_agent_builder(
        &acp,
        config,
        model_router.clone(),
        tool_registry,
        skills_manager.clone(),
    )
    .await;

    Ok(AgentsInit {
        acp,
        model_router,
        skills_manager,
        agent_registry,
        session_manager,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::GatewayConfig;
    use crate::model_router::{ProviderConfig, ProviderType};

    /// Build a `(name, ProviderConfig)` pair for tests.
    fn provider(name: &str, models: &[&str]) -> (String, ProviderConfig) {
        let models: Vec<String> = models.iter().map(|s| s.to_string()).collect();
        let default_model = models.first().cloned().unwrap_or_default();
        (
            name.to_string(),
            ProviderConfig {
                provider_type: ProviderType::OpenAi,
                models,
                default_model,
                api_key: String::new().into(),
                api_keys: Vec::new(),
                auth_profile: None,
                oauth: None,
                base_url: None,
                timeout: std::time::Duration::from_secs(30),
                max_retries: 3,
                retry_delay_ms: 1000,
            },
        )
    }

    #[tokio::test]
    async fn migrate_backfills_empty_provider_models_from_preset() {
        let mut config = GatewayConfig::default();
        config
            .providers
            .insert("deepseek".to_string(), provider("deepseek", &[]).1);
        config.model = "deepseek-chat".to_string();
        config.model_provider = "deepseek".to_string();

        let changed = migrate_model_router_config(&mut config).await;

        assert!(changed);
        let p = config.providers.get("deepseek").unwrap();
        assert_eq!(p.models, vec!["deepseek-v4-flash", "deepseek-v4-pro"]);
        assert_eq!(p.default_model, "deepseek-v4-flash");
    }

    #[tokio::test]
    async fn migrate_rewrites_legacy_default_model_alias() {
        let mut config = GatewayConfig::default();
        config.providers.insert(
            "deepseek".to_string(),
            provider("deepseek", &["deepseek-chat", "deepseek-reasoner"]).1,
        );
        config.model = "default".to_string();
        config.model_provider = "deepseek".to_string();

        let changed = migrate_model_router_config(&mut config).await;

        assert!(changed);
        assert_eq!(config.model, "deepseek-chat");
        assert_eq!(config.model_provider, "deepseek");
    }

    #[tokio::test]
    async fn migrate_default_fallback_prefers_model_provider() {
        let mut config = GatewayConfig::default();
        config
            .providers
            .insert("deepseek".to_string(), provider("deepseek", &["deepseek-chat"]).1);
        config
            .providers
            .insert("openai".to_string(), provider("openai", &["gpt-4o"]).1);
        config.model = "bogus".to_string();
        config.model_provider = "openai".to_string();

        let changed = migrate_model_router_config(&mut config).await;

        assert!(changed);
        assert_eq!(config.model, "gpt-4o");
        assert_eq!(config.model_provider, "openai");
    }

    #[tokio::test]
    async fn migrate_rewrites_legacy_agent_model_alias() {
        let mut config = GatewayConfig::default();
        config
            .providers
            .insert("deepseek".to_string(), provider("deepseek", &["deepseek-chat"]).1);
        config.model = "deepseek-chat".to_string();
        config
            .agent_models
            .insert("main".to_string(), "default".to_string());

        let changed = migrate_model_router_config(&mut config).await;

        assert!(changed);
        assert_eq!(config.agent_models.get("main").cloned(), Some("deepseek-chat".to_string()));
    }

    #[tokio::test]
    async fn migrate_fixes_provider_default_model_not_listed() {
        let mut config = GatewayConfig::default();
        let mut p = provider("deepseek", &["deepseek-chat", "deepseek-reasoner"]).1;
        p.default_model = "bogus".to_string();
        config.providers.insert("deepseek".to_string(), p);
        config.model = "deepseek-chat".to_string();
        config.model_provider = "deepseek".to_string();

        let changed = migrate_model_router_config(&mut config).await;

        assert!(changed);
        assert_eq!(config.providers.get("deepseek").unwrap().default_model, "deepseek-chat");
    }

    #[tokio::test]
    async fn migrate_is_noop_when_config_already_valid() {
        let mut config = GatewayConfig::default();
        config
            .providers
            .insert("deepseek".to_string(), provider("deepseek", &["deepseek-chat"]).1);
        config.model = "deepseek-chat".to_string();
        config.model_provider = "deepseek".to_string();
        config
            .agent_models
            .insert("main".to_string(), "deepseek-chat".to_string());

        let changed = migrate_model_router_config(&mut config).await;

        assert!(!changed);
        assert_eq!(config.model, "deepseek-chat");
        assert_eq!(config.agent_models.get("main").cloned(), Some("deepseek-chat".to_string()));
    }
}
