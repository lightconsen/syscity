//! Cloud model provider registration (P2-5): an OpenAI-compatible provider
//! pointing at the cloud API. The credential is a SecretStore ref, so the
//! session token resolves dynamically per call — the provider never needs to
//! be rebuilt on login/logout.

use std::time::Duration;

use crate::cloud::config::CloudConfig;
use crate::cloud::session::{CLOUD_NS, ENTITY_SESSION};
use crate::model_router::config::{ProviderConfig, ProviderKey, ProviderType};
use crate::model_router::ModelRouter;
use crate::secrets::StoreRef;

/// Seed model set used until the cloud `/v1/models` discovery succeeds (and
/// as the fallback when it is unreachable). The live list comes from
/// [`apply_discovered_models`] — do not treat this as authoritative.
pub const CLOUD_MODELS: &[&str] = &[
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "deepseek-v4-flash-vision-exp",
];

/// Build the cloud provider config. `models` is the discovered model list; an
/// empty slice falls back to [`CLOUD_MODELS`]. `api_key` is a store ref to the
/// cloud session token, so the current token is used on every call.
pub fn provider_config(cfg: &CloudConfig, models: &[String]) -> ProviderConfig {
    let models: Vec<String> = if models.is_empty() {
        CLOUD_MODELS.iter().map(|s| s.to_string()).collect()
    } else {
        models.to_vec()
    };
    ProviderConfig {
        provider_type: ProviderType::OpenAi,
        default_model: models.first().cloned().unwrap_or_default(),
        models,
        api_key: ProviderKey::Ref(StoreRef {
            namespace: CLOUD_NS.to_string(),
            entity: ENTITY_SESSION.to_string(),
            kind: "secret".to_string(),
        }),
        api_keys: Vec::new(),
        auth_profile: None,
        oauth: None,
        // `api_base` is the cloud ORIGIN (`{base}/api/v1/*` for REST); the
        // OpenAI wire lives under `{base}/v1/*`. OpenAiProvider joins paths
        // directly onto base_url ("/models", "/chat/completions"), so the
        // `/v1` suffix must be added here — a bare origin would make every
        // health probe and completion 404 and trip the circuit breaker.
        base_url: Some(format!("{}/v1", cfg.api_base.trim_end_matches('/'))),
        timeout: Duration::from_secs(60),
        max_retries: 2,
        retry_delay_ms: 200,
    }
}

/// Apply a freshly discovered cloud model list to the router's `cloud`
/// provider, registering it if absent. Returns `Ok(true)` when the provider
/// was (re)built.
///
/// An empty `ids` is ignored (never wipe a working list on a bad fetch), and
/// an unchanged list is a no-op so the provider instance — and its
/// circuit-breaker health — survives repeated refreshes.
pub async fn apply_discovered_models(
    router: &ModelRouter,
    cfg: &CloudConfig,
    ids: &[String],
) -> crate::Result<bool> {
    if ids.is_empty() {
        return Ok(false);
    }
    if let Some(existing) = router.router_config().await.providers.get("cloud") {
        let mut current = existing.models.clone();
        let mut wanted = ids.to_vec();
        current.sort();
        wanted.sort();
        if current == wanted {
            return Ok(false);
        }
    }
    let cfg = provider_config(cfg, ids);
    if router.provider_exists("cloud").await {
        router.update_provider("cloud", cfg).await?;
    } else {
        router.add_provider("cloud", cfg).await?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CloudConfig {
        CloudConfig {
            enabled: true,
            api_base: "https://api.example.com".to_string(),
            redirect_base: "http://localhost:18080/cloud/login/callback".to_string(),
            console_url: "https://api.example.com".to_string(),
        }
    }

    #[test]
    fn cloud_provider_config_is_openai_compatible() {
        let p = provider_config(&cfg(), &[]);
        match &p.provider_type {
            ProviderType::OpenAi => {}
            _ => panic!("cloud provider must be OpenAI-compatible"),
        }
        assert_eq!(p.base_url.as_deref(), Some("https://api.example.com/v1"));
        // Empty list falls back to the seed set.
        assert!(p.models.contains(&"deepseek-v4-flash".to_string()));
        assert!(p.models.contains(&"deepseek-v4-pro".to_string()));
        assert_eq!(p.default_model, CLOUD_MODELS[0]);
        match &p.api_key {
            ProviderKey::Ref(r) => {
                assert_eq!(r.namespace, CLOUD_NS);
                assert_eq!(r.entity, ENTITY_SESSION);
            }
            _ => panic!("cloud provider key must be a store ref (dynamic token)"),
        }
    }

    #[test]
    fn cloud_provider_config_uses_discovered_models() {
        let models = vec!["deepseek-flash".to_string(), "deepseek-v4-pro".to_string()];
        let p = provider_config(&cfg(), &models);
        assert_eq!(p.models, models);
        assert_eq!(p.default_model, "deepseek-flash");
    }

    #[tokio::test]
    async fn apply_discovered_models_registers_and_refreshes() {
        let router = ModelRouter::new(crate::model_router::config::ModelRouterConfig::default());
        let ids = vec!["deepseek-flash".to_string(), "deepseek-v4-pro".to_string()];

        // Absent provider: created.
        assert!(apply_discovered_models(&router, &cfg(), &ids)
            .await
            .unwrap());
        let registered = router.router_config().await;
        assert_eq!(registered.providers["cloud"].models, ids);
        drop(registered);

        // Identical list: no rebuild.
        assert!(!apply_discovered_models(&router, &cfg(), &ids)
            .await
            .unwrap());

        // Empty list: ignored, existing list preserved.
        assert!(!apply_discovered_models(&router, &cfg(), &[]).await.unwrap());
        assert_eq!(router.router_config().await.providers["cloud"].models, ids);

        // Different list: refreshed.
        let next = vec!["deepseek-flash".to_string()];
        assert!(apply_discovered_models(&router, &cfg(), &next)
            .await
            .unwrap());
        assert_eq!(router.router_config().await.providers["cloud"].models, next);
    }
}
