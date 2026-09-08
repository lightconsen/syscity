//! Cloud model provider registration (P2-5): an OpenAI-compatible provider
//! pointing at the cloud API. The credential is a SecretStore ref, so the
//! session token resolves dynamically per call — the provider never needs to
//! be rebuilt on login/logout.

use std::time::Duration;

use crate::cloud::config::CloudConfig;
use crate::cloud::session::{CLOUD_NS, ENTITY_SESSION};
use crate::model_router::config::{ProviderConfig, ProviderKey, ProviderType};
use crate::secrets::StoreRef;

/// Default cloud model set (mirrors the cloud `/v1/models` provider list —
/// keep in sync with the cloud proxy's supported models).
pub const CLOUD_MODELS: &[&str] = &[
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "deepseek-v4-flash-vision-exp",
];

/// Build the cloud provider config. `api_key` is a store ref to the cloud
/// session token, so the current token is used on every call.
pub fn provider_config(cfg: &CloudConfig) -> ProviderConfig {
    ProviderConfig {
        provider_type: ProviderType::OpenAi,
        models: CLOUD_MODELS.iter().map(|s| s.to_string()).collect(),
        default_model: CLOUD_MODELS[0].to_string(),
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
        let p = provider_config(&cfg());
        match &p.provider_type {
            ProviderType::OpenAi => {}
            _ => panic!("cloud provider must be OpenAI-compatible"),
        }
        assert_eq!(p.base_url.as_deref(), Some("https://api.example.com/v1"));
        assert!(p.models.contains(&"deepseek-v4-flash".to_string()));
        assert!(p.models.contains(&"deepseek-v4-pro".to_string()));
        match &p.api_key {
            ProviderKey::Ref(r) => {
                assert_eq!(r.namespace, CLOUD_NS);
                assert_eq!(r.entity, ENTITY_SESSION);
            }
            _ => panic!("cloud provider key must be a store ref (dynamic token)"),
        }
    }
}
