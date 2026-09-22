//! Gateway configuration types.
//!
//! All `*Config` structs that make up the top-level [`GatewayConfig`] tree
//! plus their `Default` impls and serde defaults. Extracted from
//! `gateway/mod.rs` so the control-plane file isn't dominated by data
//! definitions.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::agent::AgentConfig;
use crate::channels::ChannelType;
use crate::mcp::McpSettings;
use crate::security::pairing::DmPolicy;

mod memory;
mod misc;
mod optimizer;
mod plugins;
mod security;
#[cfg(test)]
mod tests;
mod tui;

pub use memory::{
    EmbeddingParams, EmbeddingProviderType, KnowledgeBaseConfig, MemoryContextWindowConfig,
    MultiQueryConfig, QueryTransformerConfig, RerankerConfig, SearchConfig, VectorMemoryConfig,
};
pub use misc::{
    default_channel_enabled, AgentOverrides, ChannelConfig, ObserveConfig, StorageConfig,
    UpdateConfig,
};
pub use optimizer::{
    BadcaseGovernanceConfig, CompressionQualityConfig, HumanReviewConfig, OnlineMonitoringConfig,
    OnlineSamplingConfig, OptimizerGuardrailConfig, OptimizerVerdictConfig, ScalarOptimizerConfig,
    StructuralProposerConfig,
};
pub use plugins::{AcpConfig, CostGuardConfig, CronConfig, HotReloadConfig, PluginConfig};
pub use security::{
    default_device_scopes, default_local_scopes, default_max_ws_connections,
    default_shared_token_scopes, CredentialPrecedence, RateLimitConfig, SecurityConfig, TierConfig,
};
pub use tui::{EvalConfig, ThemeSetting, TuiConfig};
/// Gateway configuration
///
/// `#[serde(default)]` is load-bearing, not cosmetic: several call sites parse
/// a `config.toml` written by hand or by an older build straight into this
/// type, and without it any file missing a single field fails to deserialize.
/// The failure mode was the worst kind — the daemon logged a warning and
/// carried on with `GatewayConfig::default()`, so *every* setting in the file
/// was silently discarded, not just the missing one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    /// Host to bind to
    pub host: String,
    /// Port for gateway control plane (serves API + WebSocket + SPA)
    pub port: u16,
    /// Default agent configuration
    pub default_agent: AgentConfig,
    /// Channel configurations
    pub channels: HashMap<String, ChannelConfig>,
    /// Vector memory configuration
    #[serde(default)]
    pub vector_memory: VectorMemoryConfig,
    /// Plugin system configuration
    #[serde(default)]
    pub plugins: PluginConfig,
    /// Hot reload configuration
    #[serde(default)]
    pub hot_reload: HotReloadConfig,
    /// ACP (Agent Control Plane) configuration
    #[serde(default)]
    pub acp: AcpConfig,
    /// Cron scheduler configuration
    #[serde(default)]
    pub cron: CronConfig,
    /// Heartbeat scheduler configuration
    #[serde(default)]
    pub heartbeat: crate::heartbeat::HeartbeatConfig,
    /// Security configuration
    #[serde(default)]
    pub security: SecurityConfig,
    /// Syscity Cloud integration (§2.7; feature `cloud`)
    #[cfg(feature = "cloud")]
    #[serde(default)]
    pub cloud: crate::cloud::config::CloudConfig,
    /// Storage adapter configuration
    #[serde(default)]
    pub storage: StorageConfig,
    /// LLM Provider configurations (provider name -> config)
    #[serde(default)]
    pub providers: HashMap<String, crate::model_router::ProviderConfig>,
    /// Default model name (e.g., "claude-3-sonnet-20240229", "qwen3.5-plus")
    #[serde(default = "default_model")]
    pub model: String,
    /// Model provider (e.g., "anthropic", "openai")
    #[serde(default = "default_model_provider")]
    pub model_provider: String,
    /// Per-agent model binding (agent_id -> concrete model ID). Sessions
    /// inherit this when they have no explicit model pin; empty map = global
    /// default.
    #[serde(default)]
    pub agent_models: HashMap<String, String>,
    /// Per-agent parameter overrides (agent_id -> overrides). A named agent's
    /// effective runtime config is its personality-derived base config with
    /// these fields layered on top. The default agent has no entry — it is
    /// configured directly through `default_agent`.
    #[serde(default)]
    pub agent_overrides: HashMap<String, AgentOverrides>,
    /// MCP server configurations (auto-connected on startup)
    #[serde(default)]
    pub mcp: McpSettings,
    /// Live spend and action-rate guard for LLM calls.
    #[serde(default)]
    pub cost_guard: CostGuardConfig,
    /// Workspace directory for file operations.
    /// All relative paths are resolved against this directory.
    /// When `workspace_only` is true, file operations are restricted to this
    /// directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_dir: Option<std::path::PathBuf>,
    /// When true, restrict file operations to `workspace_dir`.
    #[serde(default)]
    pub workspace_only: bool,
    /// Browser configuration (bridge, profiles, pool)
    #[cfg(feature = "browser")]
    #[serde(default)]
    pub browser: crate::config::BrowserConfig,
    /// Computer / desktop automation configuration
    #[serde(default)]
    pub computer: crate::config::ComputerConfig,
    /// Dream scheduler configuration for background memory consolidation
    #[serde(default)]
    pub dreaming: crate::config::MemoryDreamingConfig,
    /// Standing orders configuration (persistent background agent programs)
    #[serde(default)]
    pub standing_orders: crate::standing_orders::config::StandingOrderConfig,
    /// Capability set configuration (profile, scope, enabled sets)
    #[serde(default)]
    pub capabilities: crate::config::CapabilitiesConfig,
    /// Web search provider configuration.
    #[serde(default)]
    pub search: SearchConfig,
    /// Quality gate configuration for pre-release gating.
    #[serde(default)]
    pub quality_gate: crate::gateway::quality_gate::QualityGateConfig,
    /// Knowledge Base configuration for auto-ingest and watcher.
    #[serde(default)]
    pub knowledge_base: KnowledgeBaseConfig,
    /// Observability retention configuration for per-turn records.
    #[serde(default)]
    pub observe: ObserveConfig,
    /// Online update configuration (self-update via GitHub Releases).
    #[serde(default)]
    pub update: UpdateConfig,
    /// Harness self-tuning configuration (§十二: 可反馈/可复盘/可调参/护栏).
    #[serde(default)]
    pub eval: EvalConfig,
    /// TUI client preferences (appearance). Persisted here so the client can
    /// restore its theme on next launch and share it across machines.
    #[serde(default)]
    pub tui: TuiConfig,
    /// Claude-Code-style tool permissions: default mode, the bypass flag,
    /// and allow/deny/ask rules. The registry's gate consults this before
    /// every tool call; `permissions.*` config arms and the session mode
    /// override update the shared runtime live.
    #[serde(default)]
    pub permissions: crate::tools::PermissionsConfig,
}

/// The model a gateway uses when the configuration does not name one.
///
/// Delegates to [`crate::providers::DEFAULT_MODEL`] so there is one answer to
/// "what does a fresh install run" — see that constant.
fn default_model() -> String {
    crate::providers::DEFAULT_MODEL.to_string()
}

fn default_model_provider() -> String {
    crate::providers::DEFAULT_MODEL_PROVIDER.to_string()
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 18080,
            default_agent: AgentConfig::default(),
            channels: HashMap::new(),
            vector_memory: VectorMemoryConfig::default(),
            plugins: PluginConfig::default(),
            hot_reload: HotReloadConfig::default(),
            acp: AcpConfig::default(),
            cron: CronConfig::default(),
            heartbeat: crate::heartbeat::HeartbeatConfig::default(),
            security: SecurityConfig::default(),
            #[cfg(feature = "cloud")]
            cloud: crate::cloud::config::CloudConfig::default(),
            storage: StorageConfig::default(),
            providers: HashMap::new(),
            model: default_model(),
            model_provider: default_model_provider(),
            agent_models: HashMap::new(),
            agent_overrides: HashMap::new(),
            mcp: McpSettings::default(),
            cost_guard: CostGuardConfig::default(),
            workspace_dir: None,
            workspace_only: true,
            #[cfg(feature = "browser")]
            browser: crate::config::BrowserConfig::default(),
            computer: crate::config::ComputerConfig::default(),
            dreaming: crate::config::MemoryDreamingConfig::default(),
            standing_orders: crate::standing_orders::config::StandingOrderConfig::default(),
            capabilities: crate::config::CapabilitiesConfig::default(),
            search: SearchConfig::default(),
            quality_gate: crate::gateway::quality_gate::QualityGateConfig::default(),
            knowledge_base: KnowledgeBaseConfig::default(),
            observe: ObserveConfig::default(),
            update: UpdateConfig::default(),
            eval: EvalConfig::default(),
            tui: TuiConfig::default(),
            permissions: crate::tools::PermissionsConfig::default(),
        }
    }
}

// ── Hot-reload snapshot/diff ─────────────────────────────────────────────────

/// Snapshot of hot-reloadable configuration values used to compute diffs
/// across reloads.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigSnapshot {
    /// Timestamp when the snapshot was taken
    pub timestamp: String,
    /// The hot-reloadable field values keyed by dotted path (e.g.
    /// "providers.openai.base_url")
    pub fields: HashMap<String, serde_json::Value>,
}

/// A single configuration field change detected during hot reload.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigChange {
    /// Dotted path of the changed field (e.g. "model", "providers.openai")
    pub path: String,
    /// Previous value (absent for newly added fields)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_value: Option<serde_json::Value>,
    /// New value (absent for removed fields)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_value: Option<serde_json::Value>,
}

/// Recursively rebuild a JSON value with object keys sorted, so a hash over it
/// is independent of `HashMap` iteration order. Defensive: `serde_json`'s
/// default `Map` is already a `BTreeMap`, but this survives a future
/// `preserve_order` unification.
fn canonicalize_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut pairs: Vec<(String, serde_json::Value)> = map
                .iter()
                .map(|(k, v)| (k.clone(), canonicalize_json(v)))
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(pairs.into_iter().collect())
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(canonicalize_json).collect())
        }
        other => other.clone(),
    }
}

/// Canonical SHA-256 (hex) of a config JSON value. The canonical form makes
/// the hash stable across `HashMap` insertion orders, so two equal configs
/// always hash equal — the basis for optimistic-locking revisions.
pub(crate) fn config_json_hash(value: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};

    let canonical = canonicalize_json(value);
    let digest = Sha256::digest(canonical.to_string().as_bytes());
    format!("{digest:x}")
}

/// Current revision of the whole `GatewayConfig` — the same fingerprint the
/// gateway tool reports as `hash`, so both write surfaces agree on CAS.
///
/// Synchronous and lock-free: callers hold a read/write guard, and computing
/// the hash must not `.await` (respects `await_holding_lock` deny).
pub(crate) fn config_revision(config: &GatewayConfig) -> String {
    // All GatewayConfig fields implement Serialize; this cannot fail.
    #[allow(clippy::expect_used)]
    let value = serde_json::to_value(config).expect("GatewayConfig serialization cannot fail");
    config_json_hash(&value)
}

impl GatewayConfig {
    /// Find the provider name that owns a concrete model ID, if any.
    pub fn provider_for_model(&self, model_id: &str) -> Option<&str> {
        self.providers
            .iter()
            .find(|(_, cfg)| cfg.supports_model(model_id))
            .map(|(name, _)| name.as_str())
    }

    /// Merge the persisted per-agent overrides for `agent_id` into `base` in
    /// place. `base` is the agent's personality-derived config (named agents)
    /// or `default_agent` (default agent); agents with no override entry are
    /// left untouched.
    pub fn apply_agent_overrides(&self, agent_id: &str, base: &mut AgentConfig) {
        if let Some(overrides) = self.agent_overrides.get(agent_id) {
            overrides.apply_to(base);
        }
    }

    /// Apply a single per-agent override field from a `config.set` JSON value.
    ///
    /// `field` is the final path segment (e.g. `"temperature"`). A `Null`
    /// value (or an empty string for `system_prompt`) clears the override so
    /// the agent falls back to its base value. Returns `Ok(true)` when the
    /// stored override changed.
    pub fn apply_agent_override_field(
        &mut self,
        agent_id: &str,
        field: &str,
        value: &serde_json::Value,
    ) -> crate::Result<bool> {
        use crate::error::ConfigError;
        let invalid = |msg: String| ConfigError::InvalidValue {
            key: format!("agent_overrides.{}.{}", agent_id, field),
            message: msg,
        };

        let overrides = self
            .agent_overrides
            .entry(agent_id.to_string())
            .or_default();
        let changed = match field {
            "temperature" => {
                if value.is_null() {
                    let c = overrides.temperature.is_some();
                    overrides.temperature = None;
                    c
                } else {
                    let v = value
                        .as_f64()
                        .map(|f| f as f32)
                        .ok_or_else(|| invalid("temperature must be a number".into()))?;
                    let c = overrides.temperature != Some(v);
                    overrides.temperature = Some(v);
                    c
                }
            }
            "max_tokens" => {
                if value.is_null() {
                    let c = overrides.max_tokens.is_some();
                    overrides.max_tokens = None;
                    c
                } else {
                    let v = value
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| {
                            invalid("max_tokens must be a non-negative integer".into())
                        })?;
                    let c = overrides.max_tokens != Some(v);
                    overrides.max_tokens = Some(v);
                    c
                }
            }
            "max_turns" => {
                if value.is_null() {
                    let c = overrides.max_turns.is_some();
                    overrides.max_turns = None;
                    c
                } else {
                    let v = value
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| {
                            invalid("max_turns must be a non-negative integer".into())
                        })?;
                    let c = overrides.max_turns != Some(v);
                    overrides.max_turns = Some(v);
                    c
                }
            }
            "max_concurrent_tools" => {
                if value.is_null() {
                    let c = overrides.max_concurrent_tools.is_some();
                    overrides.max_concurrent_tools = None;
                    c
                } else {
                    let v = value
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| {
                            invalid("max_concurrent_tools must be a non-negative integer".into())
                        })?;
                    let c = overrides.max_concurrent_tools != Some(v);
                    overrides.max_concurrent_tools = Some(v);
                    c
                }
            }
            "workspace_only" => {
                if value.is_null() {
                    let c = overrides.workspace_only.is_some();
                    overrides.workspace_only = None;
                    c
                } else {
                    let v = value
                        .as_bool()
                        .ok_or_else(|| invalid("workspace_only must be a boolean".into()))?;
                    let c = overrides.workspace_only != Some(v);
                    overrides.workspace_only = Some(v);
                    c
                }
            }
            "system_prompt" => {
                // Empty string means "inherit the personality prompt".
                if value.is_null() || value.as_str().is_none_or(|s| s.is_empty()) {
                    let c = overrides.system_prompt.is_some();
                    overrides.system_prompt = None;
                    c
                } else {
                    let v = value
                        .as_str()
                        .ok_or_else(|| invalid("system_prompt must be a string".into()))?
                        .to_string();
                    let c = overrides.system_prompt != Some(v.clone());
                    overrides.system_prompt = Some(v);
                    c
                }
            }
            "max_context_tokens" => {
                if value.is_null() {
                    let c = overrides.max_context_tokens.is_some();
                    overrides.max_context_tokens = None;
                    c
                } else {
                    let v = value
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| {
                            invalid("max_context_tokens must be a non-negative integer".into())
                        })?;
                    let c = overrides.max_context_tokens != Some(v);
                    overrides.max_context_tokens = Some(v);
                    c
                }
            }
            other => {
                return Err(crate::SyscityError::Config(ConfigError::InvalidValue {
                    key: format!("agent_overrides.{}.{}", agent_id, other),
                    message: format!("Unknown agent parameter field: {}", other),
                }))
            }
        };

        // Drop empty override entries so they stop appearing in config.get and
        // don't linger as empty TOML tables. Applies even when this call made
        // no change — a clear on an already-empty agent would otherwise
        // re-create an empty entry via `entry().or_default()` above.
        if overrides.is_empty() {
            self.agent_overrides.remove(agent_id);
        }
        Ok(changed)
    }

    /// Remove all overrides for an agent (full reset to base config).
    pub fn clear_agent_overrides(&mut self, agent_id: &str) {
        self.agent_overrides.remove(agent_id);
    }

    /// Capture a snapshot of all hot-reloadable fields.
    pub fn snapshot(&self) -> ConfigSnapshot {
        let mut fields = HashMap::new();

        let json = serde_json::to_value(self).unwrap_or_default();
        let obj = json.as_object().cloned().unwrap_or_default();

        // Only capture fields that are actually hot-reloadable
        let reloadable_keys = [
            "security",
            "providers",
            "mcp",
            "hot_reload",
            "cost_guard",
            "capabilities",
            "computer",
            "workspace_dir",
            "workspace_only",
            "model",
            "model_provider",
            "dreaming",
            "standing_orders",
            "cron",
            "browser",
        ];

        for key in &reloadable_keys {
            if let Some(val) = obj.get(*key) {
                if !val.is_null() {
                    fields.insert(key.to_string(), val.clone());
                }
            }
        }

        ConfigSnapshot {
            timestamp: chrono::Utc::now().to_rfc3339(),
            fields,
        }
    }

    /// Compute the list of field-level changes between a previous snapshot
    /// and the current configuration.
    pub fn diff_since(&self, old: &ConfigSnapshot) -> Vec<ConfigChange> {
        let current = self.snapshot();
        let mut changes = Vec::new();

        let all_keys: std::collections::BTreeSet<&String> =
            old.fields.keys().chain(current.fields.keys()).collect();

        for key in all_keys {
            let old_val = old.fields.get(key);
            let new_val = current.fields.get(key);

            match (old_val, new_val) {
                (Some(a), Some(b)) if a == b => continue,
                (Some(_), Some(b)) => {
                    changes.push(ConfigChange {
                        path: key.clone(),
                        old_value: old_val.cloned(),
                        new_value: Some(b.clone()),
                    });
                }
                (Some(a), None) => {
                    changes.push(ConfigChange {
                        path: key.clone(),
                        old_value: Some(a.clone()),
                        new_value: None,
                    });
                }
                (None, Some(b)) => {
                    changes.push(ConfigChange {
                        path: key.clone(),
                        old_value: None,
                        new_value: Some(b.clone()),
                    });
                }
                (None, None) => {}
            }
        }

        changes
    }
}
