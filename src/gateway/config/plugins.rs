//! Plugin system, hot reload, ACP, cron and cost-guard configuration.

use super::*;
/// Plugin system configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginConfig {
    /// Enable plugin system
    pub enabled: bool,
    /// Auto-load plugins on startup
    pub auto_load: bool,
    /// Plugin directory path (None = default)
    pub plugin_dir: Option<String>,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_load: true,
            plugin_dir: None,
        }
    }
}

/// Hot reload configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotReloadConfig {
    /// Enable hot reload for configuration
    pub enabled: bool,
    /// Watch config files for changes
    pub watch_config: bool,
    /// Watch agent files for changes
    pub watch_agents: bool,
    /// Watch plugin files for changes
    pub watch_plugins: bool,
    /// Debounce duration in seconds
    pub debounce_seconds: u64,
}

impl Default for HotReloadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            watch_config: true,
            watch_agents: true,
            watch_plugins: true,
            debounce_seconds: 2,
        }
    }
}

/// ACP (Agent Control Plane) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpConfig {
    /// Enable subagent spawning
    pub enabled: bool,
    /// Maximum concurrent subagents
    pub max_subagents: usize,
    /// Default subagent timeout in seconds
    pub default_timeout_seconds: u64,
    /// Maximum iterations per ACP session execution
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
}

fn default_max_iterations() -> usize {
    100
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_subagents: 10,
            default_timeout_seconds: 300,
            max_iterations: default_max_iterations(),
        }
    }
}

/// Cron scheduler configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronConfig {
    /// Enable cron scheduler
    pub enabled: bool,
    /// Check interval in seconds
    pub check_interval_seconds: u64,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_seconds: 60,
        }
    }
}

/// Cost guard configuration — live spend and action-rate tracking.
///
/// Set `daily_limit_cents` and/or `hourly_action_limit` to non-zero values to
/// enable limits. Zero means unlimited (default).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CostGuardConfig {
    /// Maximum daily LLM spend in cents (0 = unlimited).
    /// Example: 500 = $5.00/day cap.
    #[serde(default)]
    pub daily_limit_cents: u64,
    /// Maximum provider calls per hour across all agents (0 = unlimited).
    #[serde(default)]
    pub hourly_action_limit: u64,
}
