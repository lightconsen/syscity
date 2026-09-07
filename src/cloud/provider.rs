//! Cloud model provider registration (P2-5): an OpenAI-compatible provider
//! pointing at the cloud API. The credential is a SecretStore ref, so the
//! session token resolves dynamically per call — the provider never needs to
//! be rebuilt on login/logout.

use std::time::Duration;

use crate::cloud::config::CloudConfig;
use crate::cloud::session::{CLOUD_NS, ENTITY_SESSION};
use crate::model_router::config::{ProviderConfig, ProviderKey, ProviderType};
use crate::secrets::StoreRef;

/// Default cloud model set (mirrors the cloud `/v1/models` provider list).
/// The cloud proxy routes by model prefix; these are the common ones.
pub const CLOUD_MODELS: &[&str] = &["qwen-flash", "qwen-max", "deepseek", "kimi"];

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
        base_url: Some(cfg.api_base.clone()),
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
        assert_eq!(p.base_url.as_deref(), Some("https://api.example.com"));
        assert!(p.models.contains(&"qwen-flash".to_string()));
        assert!(p.models.contains(&"deepseek".to_string()));
        match &p.api_key {
            ProviderKey::Ref(r) => {
                assert_eq!(r.namespace, CLOUD_NS);
                assert_eq!(r.entity, ENTITY_SESSION);
            }
            _ => panic!("cloud provider key must be a store ref (dynamic token)"),
        }
    }
}
