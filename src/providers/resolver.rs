//! Provider resolver — config-to-provider dispatch.
//!
//! Resolves user configuration (preset name + overrides) into a concrete
//! protocol-level provider by merging preset defaults with user overrides.

use std::sync::Arc;

use super::preset::{builtin_providers, canonical_provider_name};
use super::stream_wrappers::ProviderStreamFamily;
use super::{AnthropicProvider, GeminiProvider, OpenAiProvider, Provider};
use super::{AuthMethod, Protocol, ProviderInstanceConfig};

/// Resolve a provider configuration into a concrete provider instance.
///
/// # Arguments
/// * `provider_type` — Preset name (e.g. `"openai"`, `"kimi"`, `"anthropic"`)
///   or `"custom"`
/// * `api_key` — API key for the provider (if applicable)
/// * `base_url` — Override base URL
/// * `model` — Override model name
/// * `protocol` — Protocol override (required for `"custom"`, optional for
///   presets)
///
/// For full control, use `resolve_from_config()`.
pub fn resolve_provider(
    provider_type: &str,
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    protocol: Option<Protocol>,
) -> crate::Result<Arc<dyn Provider>> {
    let presets = builtin_providers();
    let provider_type = canonical_provider_name(provider_type);

    if provider_type == "custom" || !presets.contains_key(provider_type) {
        // Custom provider: protocol is required
        let proto = protocol.ok_or_else(|| crate::error::ConfigError::InvalidValue {
            key: "protocol".to_string(),
            message: "Custom providers require an explicit protocol".to_string(),
        })?;
        let instance = resolve_custom_instance(proto, api_key, base_url, model)?;
        return create_protocol_provider(&instance);
    }

    // Look up preset
    let preset = &presets[provider_type];

    // Select variant: by protocol override or default (first variant)
    let variant = match protocol {
        Some(p) => preset
            .variants
            .iter()
            .find(|v| v.protocol == p)
            .ok_or_else(|| {
                let available: Vec<String> = preset
                    .variants
                    .iter()
                    .map(|v| format!("{:?}", v.protocol))
                    .collect();
                crate::error::ConfigError::InvalidValue {
                    key: "protocol".to_string(),
                    message: format!(
                        "Provider '{}' does not support protocol '{:?}'. Available: {}",
                        provider_type,
                        p,
                        available.join(", "),
                    ),
                }
            })?,
        None => &preset.variants[0],
    };

    let instance = ProviderInstanceConfig {
        protocol: variant.protocol,
        auth_method: variant.auth_method.clone(),
        api_key,
        base_url: base_url.unwrap_or_else(|| variant.default_base_url.clone()),
        model: model.unwrap_or_else(|| variant.default_model.clone()),
        max_context: variant.default_max_context,
        supports_vision: variant.default_supports_vision,
        supports_tools: variant.default_supports_tools,
        stream_family: variant.default_stream_family,
    };

    create_protocol_provider(&instance)
}

/// Per-field overrides merged over a provider preset's defaults (used by
/// `ModelRouter`). Every field left as `None` falls back to the preset.
#[derive(Debug, Clone, Default)]
pub struct ProviderOverrides {
    /// Protocol override (required for `"custom"` providers).
    pub protocol: Option<Protocol>,
    /// Base URL override.
    pub base_url: Option<String>,
    /// Model override.
    pub model: Option<String>,
    /// Max context window override.
    pub max_context: Option<usize>,
    /// Vision support override.
    pub supports_vision: Option<bool>,
    /// Tool-calling support override.
    pub supports_tools: Option<bool>,
    /// Stream family override.
    pub stream_family: Option<ProviderStreamFamily>,
    /// Auth method override.
    pub auth_method: Option<AuthMethod>,
}

/// Resolve from a fully-specified set of parameters (used by `ModelRouter`).
///
/// Merges preset defaults with all overrides, then creates the provider.
///
/// # Arguments
/// * `provider_type` — Preset name or `"custom"`
/// * `api_key` — The effective API key
/// * `overrides` — Per-field overrides over the preset defaults
pub fn resolve_from_config(
    provider_type: &str,
    api_key: Option<String>,
    overrides: ProviderOverrides,
) -> crate::Result<Arc<dyn Provider>> {
    let ProviderOverrides {
        protocol,
        base_url,
        model,
        max_context,
        supports_vision,
        supports_tools,
        stream_family,
        auth_method,
    } = overrides;
    let presets = builtin_providers();
    let provider_type = canonical_provider_name(provider_type);

    let instance = if provider_type == "custom" || !presets.contains_key(provider_type) {
        let proto = protocol.ok_or_else(|| crate::error::ConfigError::InvalidValue {
            key: "protocol".to_string(),
            message: "Custom providers require an explicit protocol".to_string(),
        })?;
        ProviderInstanceConfig {
            protocol: proto,
            auth_method: auth_method.unwrap_or(AuthMethod::Bearer),
            api_key,
            base_url: base_url.clone().unwrap_or_default(),
            model: model.clone().unwrap_or_default(),
            max_context: max_context.unwrap_or(128_000),
            supports_vision: supports_vision.unwrap_or(true),
            supports_tools: supports_tools.unwrap_or(true),
            stream_family: stream_family.unwrap_or(ProviderStreamFamily::OpenAi),
        }
    } else {
        let preset = &presets[provider_type];
        let variant = match protocol {
            Some(p) => preset
                .variants
                .iter()
                .find(|v| v.protocol == p)
                .ok_or_else(|| {
                    let available: Vec<String> = preset
                        .variants
                        .iter()
                        .map(|v| format!("{:?}", v.protocol))
                        .collect();
                    crate::error::ConfigError::InvalidValue {
                        key: "protocol".to_string(),
                        message: format!(
                            "Provider '{}' does not support protocol '{:?}'. Available: {}",
                            provider_type,
                            p,
                            available.join(", "),
                        ),
                    }
                })?,
            None => &preset.variants[0],
        };

        ProviderInstanceConfig {
            protocol: variant.protocol,
            auth_method: auth_method.unwrap_or_else(|| variant.auth_method.clone()),
            api_key,
            base_url: base_url.unwrap_or_else(|| variant.default_base_url.clone()),
            model: model.unwrap_or_else(|| variant.default_model.clone()),
            max_context: max_context.unwrap_or(variant.default_max_context),
            supports_vision: supports_vision.unwrap_or(variant.default_supports_vision),
            supports_tools: supports_tools.unwrap_or(variant.default_supports_tools),
            stream_family: stream_family.unwrap_or(variant.default_stream_family),
        }
    };

    create_protocol_provider(&instance)
}

/// Resolve config for a custom (non-preset) provider.
fn resolve_custom_instance(
    protocol: Protocol,
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
) -> crate::Result<ProviderInstanceConfig> {
    let base_url = base_url.ok_or_else(|| crate::error::ConfigError::InvalidValue {
        key: "base_url".to_string(),
        message: "Custom providers require a base_url".to_string(),
    })?;

    Ok(ProviderInstanceConfig {
        protocol,
        auth_method: AuthMethod::Bearer,
        api_key,
        base_url,
        model: model.unwrap_or_else(|| "default".to_string()),
        max_context: 128_000,
        supports_vision: true,
        supports_tools: true,
        stream_family: ProviderStreamFamily::OpenAi,
    })
}

/// Create a protocol-level provider from a fully-resolved instance config.
fn create_protocol_provider(config: &ProviderInstanceConfig) -> crate::Result<Arc<dyn Provider>> {
    match config.protocol {
        Protocol::OpenAi => Ok(Arc::new(OpenAiProvider::from_config(config)?)),
        Protocol::Anthropic => Ok(Arc::new(AnthropicProvider::from_config(config)?)),
        Protocol::Gemini => Ok(Arc::new(GeminiProvider::from_config(config)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_openai_preset() {
        let provider = resolve_provider("openai", None, None, None, None).unwrap();
        assert!(provider.supports_tools());
        assert_eq!(provider.default_model(), "gpt-5.4-mini");
    }

    #[test]
    fn test_resolve_anthropic_preset() {
        let provider =
            resolve_provider("anthropic", Some("sk-test".into()), None, None, None).unwrap();
        // Asserted against the constant, not a literal: the Anthropic preset
        // ships the project's default model, and a literal here is one more
        // place that goes stale when it moves.
        assert_eq!(provider.default_model(), crate::providers::DEFAULT_MODEL);
    }

    #[test]
    fn test_resolve_gemini_preset() {
        let provider =
            resolve_provider("gemini", Some("test-key".into()), None, None, None).unwrap();
        assert_eq!(provider.default_model(), "gemini-2.5-flash");
    }

    #[test]
    fn test_resolve_kimi_default_variant() {
        // Default Kimi should use OpenAI protocol (first variant)
        let provider = resolve_provider("kimi", Some("sk-test".into()), None, None, None).unwrap();
        assert_eq!(provider.default_model(), "kimi-k2.6");
    }

    #[test]
    fn test_resolve_kimi_anthropic_variant() {
        // Explicitly choose Kimi's Anthropic variant
        let provider =
            resolve_provider("kimi", Some("sk-test".into()), None, None, Some(Protocol::Anthropic))
                .unwrap();
        assert_eq!(provider.default_model(), "kimi-k2.6");
    }

    #[test]
    fn test_resolve_with_model_override() {
        let provider = resolve_provider("openai", None, None, Some("gpt-4".into()), None).unwrap();
        assert_eq!(provider.default_model(), "gpt-4");
    }

    #[test]
    fn test_resolve_custom_provider() {
        let provider = resolve_provider(
            "custom",
            Some("sk-test".into()),
            Some("https://api.example.com/v1".into()),
            Some("my-model".into()),
            Some(Protocol::OpenAi),
        )
        .unwrap();
        assert_eq!(provider.default_model(), "my-model");
    }

    #[test]
    fn test_resolve_custom_without_protocol_fails() {
        let result = resolve_provider(
            "custom",
            Some("sk-test".into()),
            Some("https://api.example.com/v1".into()),
            None,
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_custom_without_base_url_fails() {
        let result = resolve_provider("custom", Some("sk-test".into()), None, None, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_ollama_preset() {
        let provider = resolve_provider("ollama", None, None, None, None).unwrap();
        assert_eq!(provider.default_model(), "qwen3");
    }

    #[test]
    fn test_resolve_minimax_preset() {
        let provider =
            resolve_provider("minimax", Some("sk-test".into()), None, None, None).unwrap();
        assert_eq!(provider.default_model(), "MiniMax-M3");
    }

    #[test]
    fn test_resolve_volcengine_preset() {
        let provider =
            resolve_provider("volcengine", Some("sk-test".into()), None, None, None).unwrap();
        assert_eq!(provider.default_model(), "doubao-seed-2-1-pro-260628");
    }

    #[test]
    fn test_resolve_doubao_alias() {
        // Legacy config key "doubao" must still resolve to the volcengine preset.
        let provider =
            resolve_provider("doubao", Some("sk-test".into()), None, None, None).unwrap();
        assert_eq!(provider.default_model(), "doubao-seed-2-1-pro-260628");
    }

    #[test]
    fn test_resolve_hunyuan_preset() {
        let provider =
            resolve_provider("hunyuan", Some("sk-test".into()), None, None, None).unwrap();
        assert_eq!(provider.default_model(), "hunyuan-turbos-latest");
    }

    #[test]
    fn test_resolve_grok_preset() {
        let provider = resolve_provider("grok", Some("sk-test".into()), None, None, None).unwrap();
        assert_eq!(provider.default_model(), "grok-4.3");
    }

    #[test]
    fn test_resolve_mistral_preset() {
        let provider =
            resolve_provider("mistral", Some("sk-test".into()), None, None, None).unwrap();
        assert_eq!(provider.default_model(), "mistral-large-latest");
    }

    #[test]
    fn test_resolve_cohere_preset() {
        let provider =
            resolve_provider("cohere", Some("sk-test".into()), None, None, None).unwrap();
        assert_eq!(provider.default_model(), "command-a-plus-05-2026");
    }

    #[test]
    fn test_resolve_invalid_protocol_for_preset_fails() {
        let result = resolve_provider("ollama", None, None, None, Some(Protocol::Anthropic));
        assert!(result.is_err());
    }
}
