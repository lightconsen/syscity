//! Built-in provider presets.
//!
//! Each preset defines a known LLM vendor with its protocol variants
//! (endpoints). A single vendor may expose multiple protocols (e.g. Kimi
//! supports both OpenAI-compatible and Anthropic-compatible endpoints).
//!
//! The vendor list lives in `presets.toml` (embedded at compile time via
//! [`include_str!`]) so adding a new vendor is a data-only change. If the
//! embedded TOML ever fails to parse, [`builtin_providers`] logs the error
//! and falls back to a minimal hand-rolled set (OpenAI + Anthropic) so the
//! system still boots.

use std::collections::HashMap;

use serde::Deserialize;

use super::stream_wrappers::ProviderStreamFamily;
use super::{AuthMethod, Protocol, ProtocolVariant, ProviderDefinition};

/// Embedded preset table, parsed once per call.
const PRESETS_TOML: &str = include_str!("presets.toml");

/// A preset entry as stored in `presets.toml`. The provider `name` is
/// supplied by the table key, so it is not repeated in the file.
#[derive(Debug, Deserialize)]
struct RawPreset {
    display_name: String,
    variants: Vec<ProtocolVariant>,
}

/// Legacy provider-name aliases: old config key → current preset key. Kept so
/// configs written before a rename still resolve (e.g. `"doubao"` → `"volcengine"`).
pub const PROVIDER_ALIASES: &[(&str, &str)] = &[("doubao", "volcengine")];

/// Canonicalize a provider name through the alias table. Unknown names pass
/// through unchanged so callers can keep their existing error handling.
pub fn canonical_provider_name(name: &str) -> &str {
    PROVIDER_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map(|(_, canonical)| *canonical)
        .unwrap_or(name)
}

/// All built-in provider definitions.
///
/// The key is the preset name used in config (e.g. `"openai"`, `"kimi"`).
pub fn builtin_providers() -> HashMap<&'static str, ProviderDefinition> {
    match toml::from_str::<HashMap<String, RawPreset>>(PRESETS_TOML) {
        Ok(raw) => raw
            .into_iter()
            .map(|(name, p)| {
                let def = ProviderDefinition {
                    name: name.clone(),
                    display_name: p.display_name,
                    variants: p.variants,
                };
                // Leak the key to obtain a `&'static str`. The preset set is
                // small and built once; this keeps the long-standing
                // `&'static str` key contract without churn at call sites.
                (Box::leak(name.into_boxed_str()) as &'static str, def)
            })
            .collect(),
        Err(e) => {
            tracing::error!(
                "failed to parse embedded provider presets.toml ({e}); falling back to minimal \
                 hand-rolled set"
            );
            fallback_providers()
        }
    }
}

/// Minimal hand-rolled provider set used only if `presets.toml` fails to
/// parse. Keeps the gateway bootable with the two most common vendors.
fn fallback_providers() -> HashMap<&'static str, ProviderDefinition> {
    let mut m = HashMap::new();
    m.insert(
        "openai",
        ProviderDefinition {
            name: "openai".into(),
            display_name: "OpenAI".into(),
            variants: vec![ProtocolVariant {
                protocol: Protocol::OpenAi,
                default_base_url: "https://api.openai.com/v1".into(),
                default_model: "gpt-5.4-mini".into(),
                auth_method: AuthMethod::Bearer,
                models_endpoint: Some("/models".into()),
                default_max_context: 128_000,
                default_supports_vision: true,
                default_supports_tools: true,
                default_stream_family: ProviderStreamFamily::OpenAi,
            }],
        },
    );
    m.insert(
        "anthropic",
        ProviderDefinition {
            name: "anthropic".into(),
            display_name: "Anthropic".into(),
            variants: vec![ProtocolVariant {
                protocol: Protocol::Anthropic,
                default_base_url: "https://api.anthropic.com".into(),
                default_model: crate::providers::DEFAULT_MODEL.into(),
                auth_method: AuthMethod::ApiKeyHeader,
                models_endpoint: Some("/v1/models".into()),
                default_max_context: 200_000,
                default_supports_vision: true,
                default_supports_tools: true,
                default_stream_family: ProviderStreamFamily::Anthropic,
            }],
        },
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The provider list is embedded data, so it cannot reference
    /// [`DEFAULT_MODEL`] — this is what keeps it in step with the constant
    /// instead of drifting a generation behind, as it had.
    #[test]
    fn the_anthropic_preset_ships_the_default_model() {
        let providers = builtin_providers();
        let anthropic = providers.get("anthropic").expect("anthropic preset");
        let variant = anthropic
            .variants
            .iter()
            .find(|v| v.protocol == Protocol::Anthropic)
            .expect("anthropic variant");
        assert_eq!(variant.default_model, crate::providers::DEFAULT_MODEL);
    }

    /// One place answers "which model does a fresh install use": the config
    /// default, the provider's built-in default, and the preset all read the
    /// same constant. They disagreed before — three generations between them.
    #[test]
    fn the_default_model_has_one_source() {
        let config = crate::gateway::GatewayConfig::default();
        assert_eq!(config.model, crate::providers::DEFAULT_MODEL);
        assert_eq!(config.model_provider, crate::providers::DEFAULT_MODEL_PROVIDER);

        // The provider's own built-in default (`AnthropicProvider::new`, no
        // preset involved) is the same value.
        let provider = crate::providers::AnthropicProvider::new("sk-test").unwrap();
        assert_eq!(
            crate::providers::Provider::default_model(&provider),
            crate::providers::DEFAULT_MODEL
        );
    }

    #[test]
    fn test_builtin_providers_contains_expected() {
        let providers = builtin_providers();
        assert!(providers.contains_key("openai"));
        assert!(providers.contains_key("anthropic"));
        assert!(providers.contains_key("gemini"));
        assert!(providers.contains_key("kimi"));
        assert!(providers.contains_key("deepseek"));
        assert!(providers.contains_key("ollama"));
        assert!(providers.contains_key("qwen"));
        assert!(providers.contains_key("minimax"));
        assert!(providers.contains_key("azure"));
        assert!(providers.contains_key("glm"));
        assert!(providers.contains_key("volcengine"));
        assert!(providers.contains_key("hunyuan"));
        assert!(providers.contains_key("grok"));
        assert!(providers.contains_key("mistral"));
        assert!(providers.contains_key("cohere"));
    }

    #[test]
    fn test_embedded_toml_parses() {
        // If the embedded TOML is malformed we would fall back to 2 entries;
        // the full set proves the file parsed cleanly.
        let providers = builtin_providers();
        assert!(
            providers.len() >= 9,
            "expected full preset set from presets.toml, got {}",
            providers.len()
        );
    }

    #[test]
    fn test_names_match_keys() {
        let providers = builtin_providers();
        for (key, def) in &providers {
            assert_eq!(*key, def.name, "preset key must match definition name");
        }
    }

    #[test]
    fn test_kimi_has_two_variants() {
        let providers = builtin_providers();
        let kimi = providers.get("kimi").unwrap();
        assert_eq!(kimi.variants.len(), 2);
        assert_eq!(kimi.variants[0].protocol, Protocol::OpenAi);
        assert_eq!(kimi.variants[1].protocol, Protocol::Anthropic);
    }

    #[test]
    fn test_ollama_no_auth() {
        let providers = builtin_providers();
        let ollama = providers.get("ollama").unwrap();
        assert_eq!(ollama.variants[0].auth_method, AuthMethod::None);
    }

    #[test]
    fn test_models_endpoint_loaded() {
        let providers = builtin_providers();
        assert_eq!(
            providers.get("openai").unwrap().variants[0]
                .models_endpoint
                .as_deref(),
            Some("/models")
        );
        assert_eq!(
            providers.get("anthropic").unwrap().variants[0]
                .models_endpoint
                .as_deref(),
            Some("/v1/models")
        );
        assert_eq!(
            providers.get("glm").unwrap().variants[0]
                .models_endpoint
                .as_deref(),
            Some("/models")
        );
    }

    #[test]
    fn test_gemini_uses_google_auth() {
        let providers = builtin_providers();
        let gemini = providers.get("gemini").unwrap();
        assert_eq!(gemini.variants[0].auth_method, AuthMethod::GoogleApiKey);
    }

    #[test]
    fn test_provider_aliases() {
        assert_eq!(canonical_provider_name("doubao"), "volcengine");
        assert_eq!(canonical_provider_name("volcengine"), "volcengine");
        assert_eq!(canonical_provider_name("openai"), "openai");
    }

    #[test]
    fn test_each_provider_has_at_least_one_variant() {
        let providers = builtin_providers();
        for (name, def) in &providers {
            assert!(!def.variants.is_empty(), "{} has no variants", name);
        }
    }
}
