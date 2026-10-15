//! Configuration types for the Model Router
//!
//! Contains all configuration-related types extracted from the original
//! monolithic `mod.rs`: provider configs, routing rules, cost-aware routing,
//! circuit breaker state, health tracking, and built-in provider presets.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::model_router::auth_profile::{AuthKeyConfig, AuthProfileConfig};
use crate::secrets::{SecretStoreHandle, StoreRef};

// ------------------------------------------------------------------
// OAuthConfig
// ------------------------------------------------------------------

/// OAuth 2.0 configuration for a provider
///
/// Flow parameters only — plus a *reference* to the refresh token, never its
/// value. `docs/secret-storage.md` principle 1 is explicit: "config never
/// stores values, only references". The refresh token lives in the encrypted
/// secret store under `llm-oauth/{provider}/refresh_token`; the access token
/// and its expiry are memory-only, since a restart re-acquires them from the
/// refresh token.
///
/// Unknown keys are rejected rather than ignored. Every other config struct in
/// the crate silently drops what it does not recognise, which is precisely how
/// a `provider auth` block that could never be read back went unnoticed — the
/// fields deserialized "successfully" and vanished. A hard error here names the
/// offending key instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthConfig {
    /// OAuth2 client ID
    pub client_id: String,
    /// Authorization endpoint URL (the browser flow's `/authorize`).
    ///
    /// Unset means this provider offers no browser flow: a device-only provider
    /// names [`Self::device_auth_url`] instead, and asking for a browser start
    /// is refused rather than turned into a URL that cannot work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_url: Option<String>,
    /// Device authorization endpoint URL (RFC 8628), e.g.
    /// `https://provider.example/oauth/device/code`.
    ///
    /// Declaration is what makes the device flow available for a provider: it is
    /// the one login that works with no browser on the gateway's host — over
    /// SSH, in a container, on a headless box — where the browser flow needs a
    /// redirect back to this gateway. A provider whose config names this and no
    /// `auth_url` is device-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_auth_url: Option<String>,
    /// Token endpoint URL
    pub token_url: String,
    /// Optional scope string
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// OAuth2 client secret (required by some providers for token exchange)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Public base the provider redirects the browser back to, e.g.
    /// `https://gw.example.com`. The gateway appends its own callback path
    /// ([`crate::model_router::provider_oauth::CALLBACK_PATH`]) to it.
    ///
    /// Unset means "derive from the address this gateway is bound to", which is
    /// right for a desktop install talking to a provider that accepts
    /// `http://127.0.0.1:…` redirect URIs. A gateway behind a proxy, or a
    /// provider with a strict registered-URI list, has to say so explicitly —
    /// the value must match what was registered with the provider, and only the
    /// operator knows that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_base: Option<String>,
    /// Reference to the stored refresh token, e.g.
    /// `{ namespace = "llm-oauth", entity = "grok", kind = "refresh_token" }`.
    ///
    /// `StoreRef` only: there is deliberately no inline-string variant, because
    /// accepting one would mean config could hold a live credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<StoreRef>,
}

impl OAuthConfig {
    /// The redirect URI for the gateway-hosted callback: `redirect_base` when
    /// configured, otherwise the gateway's own address.
    pub fn redirect_uri(&self, gateway_base: &str) -> String {
        let base = self.redirect_base.as_deref().unwrap_or(gateway_base);
        format!(
            "{}{}",
            base.trim_end_matches('/'),
            crate::model_router::provider_oauth::CALLBACK_PATH
        )
    }
}

// ------------------------------------------------------------------
// ProviderType
// ------------------------------------------------------------------

/// Supported provider types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderType {
    Anthropic,
    OpenAi,
    Azure,
    Ollama,
    Gemini,
    Moonshot,
    Minimax,
    Custom { name: String },
}

impl std::fmt::Display for ProviderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderType::Anthropic => write!(f, "anthropic"),
            ProviderType::OpenAi => write!(f, "openai"),
            ProviderType::Azure => write!(f, "azure"),
            ProviderType::Ollama => write!(f, "ollama"),
            ProviderType::Gemini => write!(f, "gemini"),
            ProviderType::Moonshot => write!(f, "moonshot"),
            ProviderType::Minimax => write!(f, "minimax"),
            ProviderType::Custom { name } => write!(f, "{name}"),
        }
    }
}

// ------------------------------------------------------------------
// ProviderKey
// ------------------------------------------------------------------

/// A provider API key: either an inline value or a reference into the secret
/// store (resolved at provider-creation time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProviderKey {
    /// Inline (plaintext) API key, or an env/file shorthand.
    Inline(String),
    /// Reference into the secret store (`{ namespace, entity, kind }`).
    Ref(StoreRef),
}

impl ProviderKey {
    /// The inline value, if this key is inline (not a store reference).
    pub fn inline_value(&self) -> Option<&str> {
        match self {
            ProviderKey::Inline(s) => Some(s),
            ProviderKey::Ref(_) => None,
        }
    }
}

impl Default for ProviderKey {
    fn default() -> Self {
        ProviderKey::Inline(String::new())
    }
}

impl From<String> for ProviderKey {
    fn from(s: String) -> Self {
        ProviderKey::Inline(s)
    }
}

impl From<&str> for ProviderKey {
    fn from(s: &str) -> Self {
        ProviderKey::Inline(s.to_string())
    }
}

// ------------------------------------------------------------------
// ProviderConfig
// ------------------------------------------------------------------

/// Provider configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Provider type
    pub provider_type: ProviderType,
    /// Concrete model IDs supported by this provider.
    #[serde(default)]
    pub models: Vec<String>,
    /// Default model ID for this provider (must be in `models`).
    #[serde(default)]
    pub default_model: String,
    /// API key (single key, backward compatible). Inline value or store ref.
    #[serde(default)]
    pub api_key: ProviderKey,
    /// Multiple API keys for rotation (optional, takes precedence over api_key)
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Auth profile configuration (optional, most flexible)
    #[serde(default)]
    pub auth_profile: Option<AuthProfileConfig>,
    /// OAuth 2.0 configuration for initial authorization flow
    #[serde(default)]
    pub oauth: Option<OAuthConfig>,
    /// Base URL (for custom deployments)
    pub base_url: Option<String>,
    /// Request timeout
    #[serde(default = "default_timeout")]
    pub timeout: Duration,
    /// Max retries
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Retry delay base
    #[serde(default = "default_retry_delay_ms")]
    pub retry_delay_ms: u64,
}

fn default_timeout() -> Duration {
    Duration::from_secs(30)
}

fn default_max_retries() -> u32 {
    3
}

fn default_retry_delay_ms() -> u64 {
    1000
}

impl ProviderConfig {
    /// The provider's default model ID, falling back to the first listed
    /// model if `default_model` is empty or not listed.
    pub fn default_model(&self) -> &str {
        if self.models.contains(&self.default_model) {
            return &self.default_model;
        }
        self.models.first().map(String::as_str).unwrap_or("")
    }

    /// Whether this provider supports the given concrete model ID.
    pub fn supports_model(&self, model_id: &str) -> bool {
        self.models.iter().any(|m| m == model_id)
    }

    /// Get the effective API key to use for provider creation.
    /// Prefers auth_profile keys, then api_keys, then single api_key (inline
    /// value, or a store ref resolved via the secret-store handle).
    ///
    /// A `StoreRef` needs a [`SecretStoreHandle`] to resolve; callers without
    /// one (e.g. a bare config inspection that knows the key is inline) may
    /// pass `None`, in which case a ref resolves to an empty key.
    pub async fn effective_key(&self, secrets: Option<&SecretStoreHandle>) -> String {
        if let Some(ref profile) = self.auth_profile {
            if let Some(first) = profile.keys.first() {
                return first.key.clone();
            }
        }
        if let Some(first) = self.api_keys.first() {
            return first.clone();
        }
        match &self.api_key {
            ProviderKey::Inline(s) => s.clone(),
            ProviderKey::Ref(r) => match secrets {
                Some(secrets) => match secrets.resolve_store_ref(r).await {
                    Ok(Some(v)) if !v.is_empty() => v,
                    _ => String::new(),
                },
                None => String::new(),
            },
        }
    }

    /// Build an AuthProfileConfig from this config if one is not explicitly
    /// set. Only the inline key participates (a store ref is resolved by
    /// `effective_key` at provider-creation time).
    pub fn derived_auth_profile_config(&self) -> AuthProfileConfig {
        if let Some(ref profile) = self.auth_profile {
            return profile.clone();
        }
        let mut keys = Vec::new();
        if let Some(key) = self.api_key.inline_value() {
            if !key.is_empty() {
                keys.push(AuthKeyConfig {
                    key: key.to_string(),
                    label: "primary".to_string(),
                });
            }
        }
        for (i, key) in self.api_keys.iter().enumerate() {
            if i == 0 && self.api_key.inline_value() == Some(key.as_str()) {
                continue; // avoid duplicate
            }
            keys.push(AuthKeyConfig {
                key: key.clone(),
                label: format!("key-{i}"),
            });
        }
        AuthProfileConfig {
            keys,
            cooldown_secs: 60,
            max_failures: 3,
        }
    }
}

// ------------------------------------------------------------------
// ModelCost
// ------------------------------------------------------------------

/// Cost information for a model (per 1K tokens in USD)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCost {
    /// Input cost per 1K tokens
    pub input_cost_per_1k: f64,
    /// Output cost per 1K tokens
    pub output_cost_per_1k: f64,
}

impl ModelCost {
    /// Estimate cost for a given usage
    pub fn estimate(&self, usage: &crate::providers::Usage) -> f64 {
        let input = usage.prompt_tokens as f64 * self.input_cost_per_1k / 1000.0;
        let output = usage.completion_tokens as f64 * self.output_cost_per_1k / 1000.0;
        input + output
    }
}

// ------------------------------------------------------------------
// TaskType
// ------------------------------------------------------------------

/// Task type classification for cost-aware routing
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    /// Complex code generation or refactoring
    Coding,
    /// Multi-step logical reasoning
    Reasoning,
    /// Creative writing, storytelling
    Creative,
    /// Summarizing long content
    Summarization,
    /// Simple categorization or labeling
    Classification,
    /// Structured data extraction
    Extraction,
    /// Language translation
    Translation,
    /// General conversation
    Chat,
    /// Default / fallback
    Unknown,
}

impl std::fmt::Display for TaskType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            serde_json::to_string(self)
                .unwrap_or_default()
                .trim_matches('"')
        )
    }
}

// ------------------------------------------------------------------
// TaskRoutingRule
// ------------------------------------------------------------------

/// Task-to-model routing rule
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRoutingRule {
    /// Task type this rule applies to
    pub task_type: TaskType,
    /// Preferred model ID
    pub preferred_model: String,
    /// Fallback model ID if preferred is unavailable
    pub fallback_model: Option<String>,
    /// Maximum input tokens for this rule (route to larger model if exceeded)
    pub max_input_tokens: Option<u32>,
}

// ------------------------------------------------------------------
// CostAwareConfig
// ------------------------------------------------------------------

/// Cost-aware routing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostAwareConfig {
    /// Whether cost-aware routing is enabled
    pub enabled: bool,
    /// Cost per 1K tokens for each model ID
    pub model_costs: HashMap<String, ModelCost>,
    /// Routing rules by task type
    pub routing_rules: Vec<TaskRoutingRule>,
    /// Default model ID when no rule matches
    pub default_model: String,
    /// Optional daily budget limit in USD
    pub budget_limit_usd: Option<f64>,
    /// Current daily spend (reset at midnight UTC)
    pub daily_spend_usd: f64,
}

impl Default for CostAwareConfig {
    fn default() -> Self {
        let mut model_costs = HashMap::new();
        // Default costs (approximate, should be updated with actual pricing)
        model_costs.insert(
            "fast".to_string(),
            ModelCost {
                input_cost_per_1k: 0.25,
                output_cost_per_1k: 1.25,
            },
        );
        model_costs.insert(
            "default".to_string(),
            ModelCost {
                input_cost_per_1k: 3.0,
                output_cost_per_1k: 15.0,
            },
        );
        model_costs.insert(
            "smart".to_string(),
            ModelCost {
                input_cost_per_1k: 15.0,
                output_cost_per_1k: 75.0,
            },
        );

        let routing_rules = vec![
            TaskRoutingRule {
                task_type: TaskType::Coding,
                preferred_model: "smart".to_string(),
                fallback_model: Some("default".to_string()),
                max_input_tokens: Some(8000),
            },
            TaskRoutingRule {
                task_type: TaskType::Reasoning,
                preferred_model: "smart".to_string(),
                fallback_model: Some("default".to_string()),
                max_input_tokens: None,
            },
            TaskRoutingRule {
                task_type: TaskType::Classification,
                preferred_model: "fast".to_string(),
                fallback_model: Some("default".to_string()),
                max_input_tokens: Some(4000),
            },
            TaskRoutingRule {
                task_type: TaskType::Summarization,
                preferred_model: "default".to_string(),
                fallback_model: Some("fast".to_string()),
                max_input_tokens: Some(16000),
            },
            TaskRoutingRule {
                task_type: TaskType::Extraction,
                preferred_model: "fast".to_string(),
                fallback_model: Some("default".to_string()),
                max_input_tokens: Some(8000),
            },
            TaskRoutingRule {
                task_type: TaskType::Translation,
                preferred_model: "fast".to_string(),
                fallback_model: Some("default".to_string()),
                max_input_tokens: None,
            },
            TaskRoutingRule {
                task_type: TaskType::Creative,
                preferred_model: "default".to_string(),
                fallback_model: Some("smart".to_string()),
                max_input_tokens: None,
            },
            TaskRoutingRule {
                task_type: TaskType::Chat,
                preferred_model: "default".to_string(),
                fallback_model: Some("fast".to_string()),
                max_input_tokens: Some(4000),
            },
            TaskRoutingRule {
                task_type: TaskType::Unknown,
                preferred_model: "default".to_string(),
                fallback_model: Some("fast".to_string()),
                max_input_tokens: None,
            },
        ];

        Self {
            enabled: false,
            model_costs,
            routing_rules,
            default_model: "default".to_string(),
            budget_limit_usd: None,
            daily_spend_usd: 0.0,
        }
    }
}

// ------------------------------------------------------------------
// ProviderPreset / provider_presets()
// ------------------------------------------------------------------

/// Preset for a known LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderPreset {
    /// Display name (e.g. "DeepSeek")
    pub display_name: String,
    /// Underlying protocol
    pub protocol: ProviderType,
    /// Default base URL (optional for native providers)
    pub default_base_url: Option<String>,
    /// Suggested model IDs
    pub models: Vec<String>,
}

/// Built-in provider presets keyed by provider name.
///
/// A *view* of `providers/presets.toml`, not a second registry: the vendor
/// list, the display names, the base URLs and the model lists all come from
/// that one table, so adding a vendor is one entry there. What this function
/// adds is the shape the config-facing callers want — a flat map keyed by name,
/// carrying the legacy [`ProviderType`] — plus the synthetic `custom` entry.
pub fn provider_presets() -> HashMap<String, ProviderPreset> {
    let mut presets: HashMap<String, ProviderPreset> =
        crate::providers::preset::builtin_providers()
            .values()
            .map(|vendor| {
                let first = vendor.variants.first();
                let protocol = match first {
                    Some(variant) => variant.protocol,
                    None => crate::providers::Protocol::OpenAi,
                };
                let preset = ProviderPreset {
                    display_name: vendor.display_name.clone(),
                    protocol: provider_type_for_key(&vendor.name, protocol),
                    default_base_url: first.map(|v| v.default_base_url.clone()),
                    models: vendor.models.clone(),
                };
                (vendor.name.clone(), preset)
            })
            .collect();
    // "custom" is a shape rather than a vendor: no table describes it, because
    // its defaults are the absence of defaults.
    presets.insert(
        "custom".to_string(),
        ProviderPreset {
            display_name: "Custom".to_string(),
            protocol: ProviderType::Custom { name: "custom".to_string() },
            default_base_url: None,
            models: Vec::new(),
        },
    );
    presets
}

/// The legacy [`ProviderType`] for a preset key.
///
/// `ProviderConfig.provider_type` predates the preset registry, and what is
/// read from it today is a *name*: the router stringifies it and looks the
/// preset up by that string. Four keys keep a dedicated variant because the
/// configs that already exist carry it (`moonshot` is still mapped back to
/// `kimi` on the way in); every other vendor speaks the OpenAI protocol, so the
/// legacy answer for it is `OpenAi`, with the vendor's own endpoint travelling
/// separately in `base_url` and the preset itself resolved from the provider's
/// key.
fn provider_type_for_key(key: &str, protocol: crate::providers::Protocol) -> ProviderType {
    use crate::providers::Protocol;
    match key {
        "azure" => ProviderType::Azure,
        "ollama" => ProviderType::Ollama,
        "kimi" => ProviderType::Moonshot,
        "minimax" => ProviderType::Minimax,
        _ => match protocol {
            Protocol::OpenAi => ProviderType::OpenAi,
            Protocol::Anthropic => ProviderType::Anthropic,
            Protocol::Gemini => ProviderType::Gemini,
        },
    }
}

/// Resolve a provider preset by its config key or by its display name
/// (case-insensitive). Lets a user-entered name like "DeepSeek" resolve to the
/// `deepseek` preset's protocol/base URL even when the config key differs.
pub fn provider_preset_for_name(name: &str) -> Option<ProviderPreset> {
    let name = crate::providers::preset::canonical_provider_name(name);
    let presets = provider_presets();
    if let Some(p) = presets.get(name) {
        return Some(p.clone());
    }
    presets
        .values()
        .find(|p| p.display_name.eq_ignore_ascii_case(name))
        .cloned()
}

/// Human-readable display name for a provider config key (e.g. "deepseek" →
/// "DeepSeek"). Falls back to the key itself for custom providers.
pub fn provider_display_name(key: &str) -> String {
    let key = crate::providers::preset::canonical_provider_name(key);
    let presets = provider_presets();
    if let Some(p) = presets.get(key) {
        return p.display_name.clone();
    }
    presets
        .values()
        .find(|p| p.display_name.eq_ignore_ascii_case(key))
        .map(|p| p.display_name.clone())
        .unwrap_or_else(|| key.to_string())
}

// ------------------------------------------------------------------
// FallbackEntry
// ------------------------------------------------------------------

/// Fallback chain entry
#[derive(Debug, Clone)]
pub struct FallbackEntry {
    /// Provider name
    pub provider: String,
    /// Model ID
    pub model: String,
    /// Whether to use if primary fails
    pub enabled: bool,
    /// Health score (0-100)
    pub health_score: u8,
}

// ------------------------------------------------------------------
// ModelRouterConfig
// ------------------------------------------------------------------

/// Model router configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRouterConfig {
    /// Default model ID
    pub default_model: String,
    /// Provider configurations (unique by provider name)
    pub providers: HashMap<String, ProviderConfig>,
    /// Fallback chain: model ID -> ordered list of providers
    pub fallback_chains: HashMap<String, Vec<String>>,
    /// Health check interval
    pub health_check_interval_secs: u64,
    /// Circuit breaker threshold (failures before opening)
    pub circuit_breaker_threshold: u32,
    /// Circuit breaker reset timeout
    pub circuit_breaker_reset_secs: u64,
    /// Cost-aware routing configuration
    #[serde(default)]
    pub cost_aware: Option<CostAwareConfig>,
}

impl Default for ModelRouterConfig {
    fn default() -> Self {
        Self {
            default_model: String::new(),
            providers: HashMap::new(),
            fallback_chains: HashMap::new(),
            health_check_interval_secs: 60,
            circuit_breaker_threshold: 5,
            circuit_breaker_reset_secs: 300,
            cost_aware: None,
        }
    }
}

/// Prefix that qualifies a model reference as belonging to the cloud proxy.
const CLOUD_REF_PREFIX: &str = "cloud/";

/// Split a provider-qualified model reference into `(provider_hint, bare_id)`.
///
/// Only the cloud proxy may be qualified (`cloud/<bare-id>`), because that is
/// the one provider whose model ids can collide with a directly-configured
/// provider. Bare ids return `(None, id)` unchanged. The split is purely
/// syntactic — whether the cloud provider actually serves the bare id is
/// decided by the caller via [`ModelRouterConfig::cloud_serves`].
///
/// An empty remainder (`"cloud/"`) is not a qualification and is returned
/// verbatim as a bare id.
pub fn parse_model_ref(model_ref: &str) -> (Option<&str>, &str) {
    match model_ref.strip_prefix(CLOUD_REF_PREFIX) {
        Some(rest) if !rest.is_empty() => (Some("cloud"), rest),
        _ => (None, model_ref),
    }
}

impl ModelRouterConfig {
    /// Whether the cloud proxy serves the given bare model id.
    pub fn cloud_serves(&self, bare: &str) -> bool {
        self.providers
            .get("cloud")
            .is_some_and(|p| p.supports_model(bare))
    }

    /// Find the provider name that owns the given concrete model ID.
    ///
    /// Deterministic when several providers serve the same model (a direct
    /// vendor config and the cloud proxy both carry e.g.
    /// "deepseek-v4-flash"): an explicit `cloud/<id>` reference selects the
    /// cloud proxy; otherwise the non-cloud provider wins so routing is
    /// stable across restarts and direct-capable calls are not metered
    /// through the proxy by accident.
    pub fn provider_for_model(&self, model_id: &str) -> Option<&str> {
        if let (Some(_), bare) = parse_model_ref(model_id) {
            if self.cloud_serves(bare) {
                return Some("cloud");
            }
        }
        let direct = self
            .providers
            .iter()
            .filter(|(name, _)| name.as_str() != "cloud")
            .find(|(_, cfg)| cfg.supports_model(model_id))
            .map(|(name, _)| name.as_str());
        direct.or_else(|| {
            self.providers
                .iter()
                .find(|(name, cfg)| name.as_str() == "cloud" && cfg.supports_model(model_id))
                .map(|(name, _)| name.as_str())
        })
    }
}

// ------------------------------------------------------------------
// CircuitState
// ------------------------------------------------------------------

/// Circuit breaker state
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum CircuitState {
    #[default]
    Closed, // Normal operation
    Open,     // Failing, reject requests
    HalfOpen, // Testing if recovered
}

// ------------------------------------------------------------------
// ProviderHealth
// ------------------------------------------------------------------

/// Provider health tracking
#[derive(Debug, Clone)]
pub struct ProviderHealth {
    /// Current circuit state
    pub state: CircuitState,
    /// Consecutive failures
    pub failures: u32,
    /// Successful requests
    pub successes: u64,
    /// Last failure time
    pub last_failure: Option<chrono::DateTime<chrono::Utc>>,
    /// Average latency (ms)
    pub avg_latency_ms: u64,
    /// Last health check
    pub last_health_check: Option<chrono::DateTime<chrono::Utc>>,
}

impl Default for ProviderHealth {
    fn default() -> Self {
        Self {
            state: CircuitState::Closed,
            failures: 0,
            successes: 0,
            last_failure: None,
            avg_latency_ms: 0,
            last_health_check: None,
        }
    }
}

// ------------------------------------------------------------------
// ProviderInfo / ProviderHealthInfo
// ------------------------------------------------------------------

/// Provider information for API responses
#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    /// Provider name
    pub name: String,
    /// Provider type (anthropic, openai, etc.)
    pub provider_type: String,
    /// Whether provider is enabled
    pub enabled: bool,
    /// Health information
    pub health: ProviderHealthInfo,
    /// Circuit breaker state (internal use)
    #[serde(skip)]
    pub circuit_state: CircuitState,
}

/// Provider health information for API responses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderHealthInfo {
    /// Circuit state (Closed, Open, HalfOpen)
    pub state: String,
    /// Consecutive failures
    pub failures: u32,
    /// Successful requests
    pub successes: u64,
    /// Average latency in ms
    pub avg_latency_ms: u64,
    /// Last failure timestamp
    pub last_failure: Option<chrono::DateTime<chrono::Utc>>,
    /// Last health check timestamp
    pub last_health_check: Option<chrono::DateTime<chrono::Utc>>,
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circuit_state_default_is_closed() {
        let state = CircuitState::default();
        assert_eq!(state, CircuitState::Closed);
    }

    #[tokio::test]
    async fn provider_config_effective_key_prefers_auth_profile() {
        let mut config = ProviderConfig {
            provider_type: ProviderType::OpenAi,
            models: vec!["gpt-4o".to_string()],
            default_model: "gpt-4o".to_string(),
            api_key: "single-key".to_string().into(),
            api_keys: vec!["multi-key".to_string()],
            auth_profile: None,
            oauth: None,
            base_url: None,
            timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay_ms: 1000,
        };

        // api_keys takes precedence over api_key
        assert_eq!(config.effective_key(None).await, "multi-key");

        // auth_profile takes precedence over both
        config.auth_profile = Some(AuthProfileConfig {
            keys: vec![AuthKeyConfig {
                key: "profile-key".to_string(),
                label: "primary".to_string(),
            }],
            cooldown_secs: 60,
            max_failures: 3,
        });
        assert_eq!(config.effective_key(None).await, "profile-key");
    }

    #[tokio::test]
    async fn provider_config_effective_key_resolves_store_ref() {
        // With no secret-store handle the ref cannot be resolved → empty key.
        // With a handle rooted at a temp dir, the ref still resolves to empty
        // because no entry exists for the synthetic id.
        let config = ProviderConfig {
            provider_type: ProviderType::OpenAi,
            models: vec!["gpt-4o".to_string()],
            default_model: "gpt-4o".to_string(),
            api_key: ProviderKey::Ref(crate::secrets::StoreRef::new("llm", "nope", "api_key")),
            api_keys: Vec::new(),
            auth_profile: None,
            oauth: None,
            base_url: None,
            timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay_ms: 1000,
        };
        assert_eq!(config.effective_key(None).await, "");

        let tmp = tempfile::tempdir().expect("tempdir");
        let handle =
            crate::secrets::SecretStoreHandle::with_root(tmp.path().to_path_buf()).expect("handle");
        assert_eq!(config.effective_key(Some(&handle)).await, "");
    }

    #[test]
    fn provider_key_untagged_serde() {
        // Plain string → Inline; a map → Ref(StoreRef).
        let inline: ProviderConfig =
            toml::from_str("provider_type = \"open_ai\"\napi_key = \"sk-abc\"\n").unwrap();
        match &inline.api_key {
            ProviderKey::Inline(s) => assert_eq!(s, "sk-abc"),
            ProviderKey::Ref(_) => panic!("expected inline"),
        }

        let refer: ProviderConfig = toml::from_str(
            "provider_type = \"open_ai\"\napi_key = { namespace = \"llm\", entity = \"openai\", kind = \"api_key\" }\n",
        )
        .unwrap();
        match &refer.api_key {
            ProviderKey::Ref(r) => {
                assert_eq!(r.namespace, "llm");
                assert_eq!(r.entity, "openai");
                assert_eq!(r.kind, "api_key");
            }
            ProviderKey::Inline(_) => panic!("expected store ref"),
        }
    }

    #[test]
    fn model_cost_estimate() {
        let cost = ModelCost {
            input_cost_per_1k: 3.0,
            output_cost_per_1k: 15.0,
        };
        let usage = crate::providers::Usage {
            prompt_tokens: 1000,
            completion_tokens: 500,
            total_tokens: 1500,
            ..Default::default()
        };
        let estimated = cost.estimate(&usage);
        assert!((estimated - 10.5).abs() < 0.001);
    }

    #[test]
    fn cost_aware_config_default_routing_rules() {
        let config = CostAwareConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.model_costs.len(), 3);
        assert_eq!(config.routing_rules.len(), 9);
        assert_eq!(config.default_model, "default");
    }

    #[test]
    fn provider_presets_agree_with_the_preset_table() {
        // The registry has one source. A vendor added to `presets.toml` and not
        // here — or one whose display name, base URL or model list drifted —
        // used to be possible: there were two hand-maintained tables, and they
        // disagreed (three vendors resolved but never appeared in the model
        // pickers).
        let presets = provider_presets();
        for vendor in crate::providers::preset::builtin_providers().values() {
            let preset = presets
                .get(&vendor.name)
                .unwrap_or_else(|| panic!("{} has no preset", vendor.name));
            assert_eq!(preset.display_name, vendor.display_name, "{}", vendor.name);
            assert_eq!(preset.models, vendor.models, "{}", vendor.name);
            assert_eq!(
                preset.default_base_url.as_deref(),
                vendor.variants.first().map(|v| v.default_base_url.as_str()),
                "{}",
                vendor.name
            );
            assert!(
                !preset.models.is_empty(),
                "{} ships no model list, so the pickers would show none",
                vendor.name
            );
        }
    }

    #[test]
    fn provider_presets_carry_the_legacy_provider_types() {
        // `provider_type` is a name, and these are the names existing configs
        // hold: the router resolves a preset by stringifying it. Anything else
        // would silently move a configured provider onto another vendor's
        // preset.
        let presets = provider_presets();
        let legacy = |key: &str| format!("{:?}", presets[key].protocol);
        assert_eq!(legacy("openai"), "OpenAi");
        assert_eq!(legacy("anthropic"), "Anthropic");
        assert_eq!(legacy("gemini"), "Gemini");
        assert_eq!(legacy("azure"), "Azure");
        assert_eq!(legacy("ollama"), "Ollama");
        assert_eq!(legacy("minimax"), "Minimax");
        // Kimi's configs say "moonshot"; the router maps it back to the preset.
        assert_eq!(legacy("kimi"), "Moonshot");
        // An OpenAI-compatible vendor with no legacy variant of its own.
        assert_eq!(legacy("deepseek"), "OpenAi");
        assert_eq!(legacy("custom"), "Custom { name: \"custom\" }");
    }

    #[test]
    fn provider_preset_for_name_matches_key_or_display_name() {
        // Exact config key.
        let by_key = provider_preset_for_name("deepseek").unwrap();
        assert_eq!(by_key.display_name, "DeepSeek");

        // Case-insensitive display-name match (e.g. user typed "DeepSeek").
        let by_display = provider_preset_for_name("DeepSeek").unwrap();
        assert_eq!(format!("{:?}", by_display.protocol), format!("{:?}", by_key.protocol));

        // Unknown provider.
        assert!(provider_preset_for_name("no-such-provider").is_none());
    }

    #[test]
    fn provider_display_name_resolves_preset_or_falls_back_to_key() {
        assert_eq!(provider_display_name("deepseek"), "DeepSeek");
        assert_eq!(provider_display_name("DeepSeek"), "DeepSeek");
        assert_eq!(provider_display_name("my-custom"), "my-custom");
    }

    #[test]
    fn provider_for_model_prefers_non_cloud_when_duplicated() {
        let direct = ProviderConfig {
            provider_type: ProviderType::OpenAi,
            models: vec!["deepseek-v4-flash".into(), "deepseek-v4-pro".into()],
            default_model: "deepseek-v4-flash".into(),
            api_key: "sk-test".to_string().into(),
            api_keys: vec![],
            auth_profile: None,
            oauth: None,
            base_url: None,
            timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay_ms: 1000,
        };
        let proxy = ProviderConfig {
            provider_type: ProviderType::OpenAi,
            models: vec![
                "deepseek-v4-flash".into(),
                "deepseek-v4-flash-vision-exp".into(),
            ],
            default_model: "deepseek-v4-flash".into(),
            api_key: "sk-test".to_string().into(),
            api_keys: vec![],
            auth_profile: None,
            oauth: None,
            base_url: None,
            timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay_ms: 1000,
        };
        let mut config = ModelRouterConfig::default();
        config.providers.insert("deepseek".to_string(), direct);
        config.providers.insert("cloud".to_string(), proxy);

        // Duplicated ids resolve to the direct provider; cloud-unique ids
        // still resolve to the proxy.
        assert_eq!(config.provider_for_model("deepseek-v4-flash"), Some("deepseek"));
        assert_eq!(config.provider_for_model("deepseek-v4-pro"), Some("deepseek"));
        assert_eq!(config.provider_for_model("deepseek-v4-flash-vision-exp"), Some("cloud"));
        assert_eq!(config.provider_for_model("gpt-4o"), None);

        // A `cloud/<id>` reference selects the proxy even when the bare id
        // also exists on a direct provider; unserved cloud ids resolve to None.
        assert_eq!(config.provider_for_model("cloud/deepseek-v4-flash"), Some("cloud"));
        assert_eq!(config.provider_for_model("cloud/deepseek-v4-flash-vision-exp"), Some("cloud"));
        // The proxy does not serve this id, so the qualification is not real.
        assert_eq!(config.provider_for_model("cloud/deepseek-v4-pro"), None);
        assert_eq!(config.provider_for_model("cloud/ghost"), None);
    }

    #[test]
    fn parse_model_ref_splits_only_cloud_prefix() {
        assert_eq!(parse_model_ref("deepseek-v4-pro"), (None, "deepseek-v4-pro"));
        assert_eq!(parse_model_ref("cloud/deepseek-v4-pro"), (Some("cloud"), "deepseek-v4-pro"));
        // Prefix is stripped exactly once, even when the id contains '/'.
        assert_eq!(parse_model_ref("cloud/a/b"), (Some("cloud"), "a/b"));
        // Empty remainder and non-lowercase prefixes are not qualifications.
        assert_eq!(parse_model_ref("cloud/"), (None, "cloud/"));
        assert_eq!(parse_model_ref("Cloud/x"), (None, "Cloud/x"));
    }

    #[test]
    fn literal_cloud_prefixed_id_falls_back_to_bare_scan() {
        // A direct provider may legitimately own an id that starts with
        // "cloud/"; it must still resolve when the proxy does not serve it.
        let config = ModelRouterConfig {
            providers: [
                ("local".to_string(), test_config(&["cloud/foo"])),
                ("cloud".to_string(), test_config(&["bar"])),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        assert_eq!(config.provider_for_model("cloud/foo"), Some("local"));
        // But when the proxy does serve the bare id, the qualification wins.
        let config = ModelRouterConfig {
            providers: [
                ("local".to_string(), test_config(&["cloud/foo"])),
                ("cloud".to_string(), test_config(&["foo"])),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        assert_eq!(config.provider_for_model("cloud/foo"), Some("cloud"));
    }

    fn test_config(models: &[&str]) -> ProviderConfig {
        ProviderConfig {
            provider_type: ProviderType::OpenAi,
            models: models.iter().map(|s| s.to_string()).collect(),
            default_model: models.first().map(|s| s.to_string()).unwrap_or_default(),
            api_key: "sk-test".to_string().into(),
            api_keys: vec![],
            auth_profile: None,
            oauth: None,
            base_url: None,
            timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay_ms: 1000,
        }
    }
}
