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
use std::sync::OnceLock;

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
    /// Suggested model IDs, the vendor's default model first. Defaulted so a
    /// table that omits the key still parses; `every_preset_lists_models`
    /// keeps that from being a way to ship an empty picker.
    #[serde(default)]
    models: Vec<String>,
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
///
/// Parsed once and held for the process, and the keys are handed out as
/// `&'static str`: the table is read on paths that run per request (a display
/// name per model row, a preset look-up per provider add), so parsing it — and
/// leaking a key string to satisfy that signature — on every call was a leak
/// that grew with traffic, not with the vendor list.
pub fn builtin_providers() -> &'static HashMap<&'static str, ProviderDefinition> {
    static PROVIDERS: OnceLock<HashMap<&'static str, ProviderDefinition>> = OnceLock::new();
    PROVIDERS.get_or_init(|| match toml::from_str::<HashMap<String, RawPreset>>(PRESETS_TOML) {
        Ok(raw) => raw
            .into_iter()
            .map(|(name, p)| {
                let def = ProviderDefinition {
                    name: name.clone(),
                    display_name: p.display_name,
                    models: p.models,
                    variants: p.variants,
                };
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
    })
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
            models: vec!["gpt-5.4-mini".into()],
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
            // The fallback set exists so the gateway still boots; it carries
            // the two vendors' defaults and nothing else.
            models: vec![crate::providers::DEFAULT_MODEL.into()],
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
        for key in [
            "openai",
            "anthropic",
            "gemini",
            "kimi",
            "deepseek",
            "ollama",
            "qwen",
            "minimax",
            "azure",
            "glm",
            "volcengine",
            "hunyuan",
            "grok",
            "mistral",
            "cohere",
            "openrouter",
            "ai-gateway",
            "huggingface",
            "opencode",
            "kilocode",
            "nvidia",
            "fireworks",
            "novita",
            "gmi",
            "nebius",
            "deepinfra",
            "arcee",
            "upstage",
            "groq",
            "zai",
            "stepfun",
            "xiaomi",
            "tencent-tokenhub",
            "ollama-cloud",
            "lmstudio",
        ] {
            assert!(providers.contains_key(key), "{key} is missing");
        }
    }

    #[test]
    fn test_embedded_toml_parses() {
        // If the embedded TOML is malformed we would fall back to 2 entries;
        // the full set proves the file parsed cleanly. The floor is below the
        // real count on purpose — this catches a fallback, not a missing row,
        // which is what the test above is for.
        let providers = builtin_providers();
        assert!(
            providers.len() >= 30,
            "expected full preset set from presets.toml, got {}",
            providers.len()
        );
    }

    #[test]
    fn test_names_match_keys() {
        let providers = builtin_providers();
        for (key, def) in providers {
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
        for (name, def) in providers {
            assert!(!def.variants.is_empty(), "{} has no variants", name);
        }
    }

    /// The model list is what the pickers show and what a provider with no
    /// model list of its own is backfilled from, so an empty one is a vendor
    /// that cannot be selected.
    #[test]
    fn every_preset_lists_models() {
        let providers = builtin_providers();
        for (name, def) in providers {
            assert!(!def.models.is_empty(), "{name} lists no models");
        }
    }

    /// Two vendors sharing an endpoint means one of them was copied and not
    /// finished — the failure mode a table this size invites.
    #[test]
    fn every_preset_has_its_own_base_url() {
        let providers = builtin_providers();
        let mut seen: HashMap<&str, &str> = HashMap::new();
        for (name, def) in providers {
            for variant in &def.variants {
                assert!(
                    variant.default_base_url.starts_with("http"),
                    "{name} has no usable base URL"
                );
                if let Some(other) = seen.insert(variant.default_base_url.as_str(), name) {
                    panic!("{name} and {other} share a base URL");
                }
            }
        }
    }

    /// The first listed model and the variant's `default_model` are read
    /// interchangeably — the backfill takes the list's head as the provider's
    /// default, while the provider instance defaults to `default_model` — so
    /// they have to be the same model. They were not: the OpenAI preset offered
    /// `gpt-5.5` first and defaulted to `gpt-5.4-mini`.
    #[test]
    fn every_preset_lists_its_default_model_first() {
        let providers = builtin_providers();
        for (name, def) in providers {
            assert_eq!(
                def.models.first().map(String::as_str),
                Some(def.variants[0].default_model.as_str()),
                "{name}: the model list must lead with the default model"
            );
        }
    }
}
