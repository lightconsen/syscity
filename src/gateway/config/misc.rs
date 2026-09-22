//! Update, observability, agent-override and channel configuration.

use super::*;
/// Online update (self-update) configuration.
///
/// Controls whether the daemon checks for new releases and applies them via
/// `syscity update` / the web update flow. Set `enabled = false` to disable
/// the update endpoints entirely; `auto_check = false` disables the
/// background check at daemon startup (manual checks still work).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    /// Master switch for online updates.
    pub enabled: bool,
    /// Check for new releases in the background at daemon startup.
    pub auto_check: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_check: true,
        }
    }
}

/// Observability (per-turn records) retention configuration.
///
/// Backs the daemon-startup sweep that prunes old turn JSON files and SQLite
/// metric rows (`llm_calls` / `tool_call_metrics` / `turn_outcomes`). Manual
/// `syscity observe prune --older-than` overrides this for a one-off run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ObserveConfig {
    /// Keep turn records for this many days. Records older than this are
    /// pruned at daemon startup. `0` disables auto-cleanup.
    pub retention_days: u32,
}

impl Default for ObserveConfig {
    fn default() -> Self {
        Self { retention_days: 30 }
    }
}

/// Per-agent parameter overrides layered on top of an agent's base config.
///
/// `None` fields mean "inherit the base value" (personality-derived for named
/// agents, `default_agent` for the default agent). All fields are optional so
/// an agent may override only the parameters it cares about.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentOverrides {
    /// Default temperature for completions.
    pub temperature: Option<f32>,
    /// Maximum tokens per completion.
    pub max_tokens: Option<u32>,
    /// Hard cap on conversation turns kept in context.
    pub max_turns: Option<usize>,
    /// Maximum number of concurrent tool calls.
    pub max_concurrent_tools: Option<usize>,
    /// Restrict file operations to the agent's workspace directory.
    pub workspace_only: Option<bool>,
    /// Overrides the personality-derived system prompt when set.
    pub system_prompt: Option<String>,
    /// Maximum context window size (in tokens).
    pub max_context_tokens: Option<usize>,
}

impl AgentOverrides {
    /// True when no field is overridden.
    pub fn is_empty(&self) -> bool {
        self.temperature.is_none()
            && self.max_tokens.is_none()
            && self.max_turns.is_none()
            && self.max_concurrent_tools.is_none()
            && self.workspace_only.is_none()
            && self.system_prompt.is_none()
            && self.max_context_tokens.is_none()
    }

    /// Overlay every non-`None` field onto a base `AgentConfig`.
    pub fn apply_to(&self, cfg: &mut AgentConfig) {
        if let Some(v) = self.temperature {
            cfg.temperature = v;
        }
        if let Some(v) = self.max_tokens {
            cfg.max_tokens = v;
        }
        if let Some(v) = self.max_turns {
            cfg.max_turns = Some(v);
        }
        if let Some(v) = self.max_concurrent_tools {
            cfg.max_concurrent_tools = v;
        }
        if let Some(v) = self.workspace_only {
            cfg.workspace_only = v;
        }
        if let Some(v) = self.system_prompt.clone() {
            cfg.system_prompt = v;
        }
        if let Some(v) = self.max_context_tokens {
            cfg.max_context_tokens = v;
        }
    }
}

/// Storage adapter configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Storage type: "memory", "file", "sqlite"
    pub storage_type: String,
    /// Base path for file/SQLite storage
    pub base_path: Option<String>,
    /// SQLite database URL (if using sqlite)
    pub database_url: Option<String>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            storage_type: "sqlite".to_string(),
            base_path: None,
            database_url: None,
        }
    }
}

/// Channel-specific configuration
///
/// `credentials` and `enabled` default so a channel table can be written
/// sparsely, matching how [`ChannelConfig::new`] fills them in. `channel_type`
/// stays required — a channel entry that does not name its type is a mistake
/// that should fail, not silently become something.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// Channel type
    pub channel_type: ChannelType,
    /// Whether channel is enabled
    #[serde(default = "default_channel_enabled")]
    pub enabled: bool,
    /// Channel-specific credentials/tokens
    #[serde(default)]
    pub credentials: HashMap<String, String>,
    /// DM policy: open, pairing, or allowlist
    #[serde(default)]
    pub dm_policy: DmPolicy,
    /// Require explicit mention in group chats (ignored for DMs)
    #[serde(default)]
    pub require_mention: bool,
    /// Allowlist of users/numbers (for allowlist policy)
    #[serde(default)]
    pub allow_from: Vec<String>,
    /// Blocklist of users/numbers
    #[serde(default)]
    pub block_from: Vec<String>,
    /// Agent ID to route to (None = default)
    pub agent_id: Option<String>,
}

/// Channels default to enabled, matching what `ChannelConfig::new` sets.
pub fn default_channel_enabled() -> bool {
    true
}

impl ChannelConfig {
    /// Create a new channel config with open policy (default).
    pub fn new(channel_type: ChannelType) -> Self {
        Self {
            channel_type,
            enabled: true,
            credentials: HashMap::new(),
            dm_policy: DmPolicy::Open,
            require_mention: false,
            allow_from: Vec::new(),
            block_from: Vec::new(),
            agent_id: None,
        }
    }

    /// Set the DM policy.
    pub fn with_dm_policy(mut self, policy: DmPolicy) -> Self {
        self.dm_policy = policy;
        self
    }

    /// Set the allowlist.
    pub fn with_allow_from(mut self, allow_from: Vec<String>) -> Self {
        self.allow_from = allow_from;
        self
    }

    /// Check if a user is in the allowlist.
    pub fn is_in_allowlist(&self, user_id: &str) -> bool {
        self.allow_from.iter().any(|a| a == user_id)
    }

    /// Check if a user is blocked.
    pub fn is_blocked(&self, user_id: &str) -> bool {
        self.block_from.iter().any(|b| b == user_id)
    }
}
