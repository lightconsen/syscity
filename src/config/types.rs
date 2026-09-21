//! Configuration types and defaults.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::secrets::SecretRef;

#[allow(clippy::unwrap_used)]
pub(super) static RE_ENV_VAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?P<full>\$\$(?P<escaped>[\w_]+)|\$\{(?P<braced>\w+)\}|\$(?P<plain>\w+))").unwrap()
});

#[allow(clippy::unwrap_used)]
pub(super) static RE_HHMM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{2}:\d{2}$").unwrap());

/// Default configuration file name
pub const DEFAULT_CONFIG_FILE: &str = "config.toml";

/// Environment variable prefix
pub const ENV_PREFIX: &str = "SYSCITY";

/// Current configuration schema version.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

fn current_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

/// Application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Configuration schema version for migration support
    #[serde(default = "current_schema_version")]
    pub schema_version: u32,

    /// Application metadata
    #[serde(skip)]
    pub app: AppConfig,

    /// Server configuration
    #[serde(default)]
    pub server: ServerConfig,

    /// Logging configuration
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Storage configuration
    #[serde(default)]
    pub storage: StorageConfig,

    /// External service configurations
    #[serde(default)]
    pub services: HashMap<String, ServiceConfig>,

    /// Browser automation configuration
    #[cfg(feature = "browser")]
    #[serde(default)]
    pub browser: BrowserConfig,

    /// Memory subsystem configuration
    #[serde(default)]
    pub memory: MemoryConfig,

    /// Heartbeat scheduler configuration
    #[serde(default)]
    pub heartbeat: crate::heartbeat::HeartbeatConfig,

    /// Computer / desktop automation configuration
    #[serde(default)]
    pub computer: ComputerConfig,

    /// Standing orders configuration (persistent background agent programs)
    #[serde(default)]
    pub standing_orders: crate::standing_orders::config::StandingOrderConfig,

    /// Capability set configuration (profile, scope, enabled sets)
    #[serde(default)]
    pub capabilities: CapabilitiesConfig,

    /// Syscity Cloud integration (§2.7). Compiled only with the `cloud` feature.
    #[cfg(feature = "cloud")]
    #[serde(default)]
    pub cloud: crate::cloud::config::CloudConfig,

    /// Custom key-value pairs
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
}

/// Application metadata
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Application name
    pub name: String,
    /// Application version
    pub version: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            name: env!("CARGO_PKG_NAME").to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Host to bind to
    #[serde(default = "default_host")]
    pub host: String,
    /// Port to listen on
    #[serde(default = "default_port")]
    pub port: u16,
    /// Request timeout in seconds
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    /// Maximum request body size in bytes
    #[serde(default = "default_max_body_size")]
    pub max_body_size: usize,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_timeout() -> u64 {
    30
}

fn default_max_body_size() -> usize {
    10 * 1024 * 1024 // 10 MB
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            timeout_seconds: default_timeout(),
            max_body_size: default_max_body_size(),
        }
    }
}

/// Logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level (trace, debug, info, warn, error)
    #[serde(default = "default_log_level")]
    pub level: String,
    /// Log format (json, pretty, compact)
    #[serde(default = "default_log_format")]
    pub format: LogFormat,
    /// Optional log file path
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    /// Whether to log to stdout
    #[serde(default = "default_true")]
    pub stdout: bool,
    /// Log rotation configuration
    #[serde(default)]
    pub rotation: LogRotationConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Pretty,
    Compact,
}

/// Log rotation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRotationConfig {
    /// Enable log rotation
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Maximum file size before rotation (bytes)
    #[serde(default = "default_max_size")]
    pub max_size: u64,
    /// Maximum number of archived files to keep
    #[serde(default = "default_max_files")]
    pub max_files: usize,
}

impl Default for LogRotationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_size: 10 * 1024 * 1024, // 10 MB
            max_files: 5,
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_format() -> LogFormat {
    LogFormat::Compact
}

fn default_true() -> bool {
    true
}

fn default_max_size() -> u64 {
    10 * 1024 * 1024 // 10 MB
}

fn default_max_files() -> usize {
    5
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: default_log_format(),
            file: None,
            stdout: true,
            rotation: LogRotationConfig::default(),
        }
    }
}

/// Storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Storage type (memory, file, database)
    #[serde(default = "default_storage_type")]
    pub storage_type: StorageType,
    /// Connection string or path
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    /// Database name (for database storage)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

/// Storage type
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StorageType {
    Memory,
    File,
    Sqlite,
    #[serde(alias = "db")]
    Database,
}

fn default_storage_type() -> StorageType {
    StorageType::Memory
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            storage_type: default_storage_type(),
            connection: None,
            database: None,
        }
    }
}

/// Memory subsystem configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryConfig {
    /// Multimodal file storage settings
    #[serde(default)]
    pub multimodal: MemoryMultimodalConfig,
    /// Dreaming engine settings
    #[serde(default)]
    pub dreaming: MemoryDreamingConfig,
    /// Tier system settings
    #[serde(default)]
    pub tier: MemoryTierConfig,
    /// Effectiveness tracking settings
    #[serde(default)]
    pub effectiveness: MemoryEffectivenessConfig,
}

/// Multimodal storage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMultimodalConfig {
    /// Enable multimodal storage
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Enabled modalities (image, audio)
    #[serde(default = "default_multimodal_modalities")]
    pub modalities: Vec<String>,
    /// Maximum file size in bytes
    #[serde(default = "default_multimodal_max_bytes")]
    pub max_file_bytes: u64,
}

impl Default for MemoryMultimodalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            modalities: default_multimodal_modalities(),
            max_file_bytes: default_multimodal_max_bytes(),
        }
    }
}

fn default_multimodal_modalities() -> Vec<String> {
    vec!["image".to_string(), "audio".to_string()]
}

fn default_multimodal_max_bytes() -> u64 {
    10 * 1024 * 1024 // 10 MB
}

/// Dreaming engine configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDreamingConfig {
    /// Enable dreaming
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Cron expression for scheduling (default: daily at 3 AM)
    #[serde(default = "default_dreaming_frequency")]
    pub frequency: String,
    /// Speed: fast, balanced, slow
    #[serde(default = "default_dreaming_speed")]
    pub speed: String,
    /// Thinking depth: low, medium, high
    #[serde(default = "default_dreaming_thinking")]
    pub thinking: String,
    /// Budget: cheap, medium, expensive
    #[serde(default = "default_dreaming_budget")]
    pub budget: String,
    /// Similarity threshold for deduplication
    #[serde(default = "default_dreaming_dedup_threshold")]
    pub dedup_similarity_threshold: f32,
}

impl Default for MemoryDreamingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            frequency: default_dreaming_frequency(),
            speed: default_dreaming_speed(),
            thinking: default_dreaming_thinking(),
            budget: default_dreaming_budget(),
            dedup_similarity_threshold: default_dreaming_dedup_threshold(),
        }
    }
}

fn default_dreaming_frequency() -> String {
    "0 0 3 * * *".to_string()
}

fn default_dreaming_speed() -> String {
    "balanced".to_string()
}

fn default_dreaming_thinking() -> String {
    "medium".to_string()
}

fn default_dreaming_budget() -> String {
    "medium".to_string()
}

fn default_dreaming_dedup_threshold() -> f32 {
    0.95
}

/// Memory tier configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryTierConfig {
    /// Enable tier management
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Auto-promote/demote memories
    #[serde(default = "default_true")]
    pub auto_promote: bool,
    /// Maintenance interval in seconds
    #[serde(default = "default_tier_maintenance_interval")]
    pub maintenance_interval_secs: u64,
}

impl Default for MemoryTierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_promote: true,
            maintenance_interval_secs: default_tier_maintenance_interval(),
        }
    }
}

fn default_tier_maintenance_interval() -> u64 {
    24 * 60 * 60 // Daily
}

/// Memory effectiveness tracking configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEffectivenessConfig {
    /// Enable effectiveness tracking
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Auto-adjust memory importance based on hit rate
    #[serde(default = "default_true")]
    pub auto_adjust: bool,
    /// Hit rate threshold for promotion (0.0-1.0)
    #[serde(default = "default_effectiveness_promotion_threshold")]
    pub promotion_threshold: f32,
    /// Hit rate threshold for demotion (0.0-1.0)
    #[serde(default = "default_effectiveness_demotion_threshold")]
    pub demotion_threshold: f32,
}

impl Default for MemoryEffectivenessConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_adjust: true,
            promotion_threshold: default_effectiveness_promotion_threshold(),
            demotion_threshold: default_effectiveness_demotion_threshold(),
        }
    }
}

fn default_effectiveness_promotion_threshold() -> f32 {
    0.7
}

fn default_effectiveness_demotion_threshold() -> f32 {
    0.2
}

/// External service configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    /// Service endpoint URL
    pub endpoint: String,
    /// API key (can be raw string, env var reference like "$ENV_VAR", or
    /// SecretRef object)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SecretRef>,
    /// Request timeout in seconds
    #[serde(default = "default_service_timeout")]
    pub timeout_seconds: u64,
    /// Retry configuration
    #[serde(default)]
    pub retry: RetryConfig,
}

fn default_service_timeout() -> u64 {
    30
}

/// Retry configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Maximum number of retries
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Base delay between retries in milliseconds
    #[serde(default = "default_retry_delay_ms")]
    pub base_delay_ms: u64,
    /// Maximum delay between retries in milliseconds
    #[serde(default = "default_max_retry_delay_ms")]
    pub max_delay_ms: u64,
}

fn default_max_retries() -> u32 {
    3
}

fn default_retry_delay_ms() -> u64 {
    1000
}

fn default_max_retry_delay_ms() -> u64 {
    30000
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            base_delay_ms: default_retry_delay_ms(),
            max_delay_ms: default_max_retry_delay_ms(),
        }
    }
}

/// Computer / desktop automation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputerConfig {
    /// Remote control configuration for external machines
    #[serde(default)]
    pub remote_control: RemoteControlConfig,
    /// Headless display configuration for CI/CD environments
    #[serde(default)]
    pub headless: HeadlessConfig,
    /// Enable computer use loop in agent responses
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Maximum steps per computer use session
    #[serde(default = "default_max_computer_steps")]
    pub max_steps: usize,
    /// Settle delay after actions (milliseconds)
    #[serde(default = "default_settle_delay_ms")]
    pub settle_delay_ms: u64,
}

/// Remote control configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteControlConfig {
    /// Target host (IP or hostname)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// SSH username
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Port (22 for SSH, 5900 for VNC, 3389 for RDP)
    #[serde(default = "default_remote_port")]
    pub port: u16,
    /// Protocol: ssh, vnc, rdp
    #[serde(default = "default_remote_protocol")]
    pub protocol: String,
    /// Path to SSH private key
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_path: Option<String>,
    /// Remote display for Linux X11 apps (e.g. ":0")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// Extra SSH arguments
    #[serde(default)]
    pub ssh_extra_args: Vec<String>,
    /// Connection timeout in seconds
    #[serde(default = "default_remote_timeout")]
    pub timeout_secs: u64,
}

/// Headless display configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeadlessConfig {
    /// Enable headless mode (Xvfb/virtual display)
    #[serde(default = "default_false")]
    pub enabled: bool,
    /// Display identifier (e.g. ":99")
    #[serde(default = "default_headless_display")]
    pub display: String,
    /// Screen resolution width
    #[serde(default = "default_headless_width")]
    pub width: u32,
    /// Screen resolution height
    #[serde(default = "default_headless_height")]
    pub height: u32,
    /// Color depth
    #[serde(default = "default_headless_depth")]
    pub depth: u8,
}

fn default_max_computer_steps() -> usize {
    30
}

fn default_settle_delay_ms() -> u64 {
    500
}

fn default_remote_port() -> u16 {
    22
}

fn default_remote_protocol() -> String {
    "ssh".to_string()
}

fn default_remote_timeout() -> u64 {
    10
}

fn default_headless_display() -> String {
    ":99".to_string()
}

fn default_headless_width() -> u32 {
    1920
}

fn default_headless_height() -> u32 {
    1080
}

fn default_headless_depth() -> u8 {
    24
}

impl Default for ComputerConfig {
    fn default() -> Self {
        Self {
            remote_control: RemoteControlConfig::default(),
            headless: HeadlessConfig::default(),
            enabled: true,
            max_steps: default_max_computer_steps(),
            settle_delay_ms: default_settle_delay_ms(),
        }
    }
}

impl Default for RemoteControlConfig {
    fn default() -> Self {
        Self {
            host: None,
            user: None,
            port: default_remote_port(),
            protocol: default_remote_protocol(),
            key_path: None,
            display: Some(":0".to_string()),
            ssh_extra_args: Vec::new(),
            timeout_secs: default_remote_timeout(),
        }
    }
}

impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            display: default_headless_display(),
            width: default_headless_width(),
            height: default_headless_height(),
            depth: default_headless_depth(),
        }
    }
}

/// Capability set configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitiesConfig {
    /// Capability profile: minimal, observer, server, desktop, full, custom
    #[serde(default = "default_capability_profile")]
    pub profile: String,
    /// Custom set IDs when profile is "custom"
    #[serde(default)]
    pub custom_sets: Vec<String>,
    /// Maximum OsControlScope to allow (read_only, user_space, system, root)
    #[serde(default = "default_capability_max_scope")]
    pub max_scope: String,
    /// Explicitly disable specific set IDs regardless of profile
    #[serde(default)]
    pub disabled_sets: Vec<String>,
}

fn default_capability_profile() -> String {
    "full".to_string()
}

fn default_capability_max_scope() -> String {
    "root".to_string()
}

impl Default for CapabilitiesConfig {
    fn default() -> Self {
        Self {
            profile: default_capability_profile(),
            custom_sets: Vec::new(),
            max_scope: default_capability_max_scope(),
            disabled_sets: Vec::new(),
        }
    }
}

/// Browser automation configuration
#[cfg(feature = "browser")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserConfig {
    /// Enable the browser bridge server
    #[serde(default = "default_false")]
    pub bridge_enabled: bool,
    /// Port for the browser bridge server
    #[serde(default = "default_bridge_port")]
    pub bridge_port: u16,
    /// Pool configuration
    #[serde(default)]
    pub pool: crate::browser::BrowserPoolConfig,
    /// Browser profiles
    #[serde(default)]
    pub profiles: Vec<crate::browser::BrowserProfile>,
}

#[cfg(feature = "browser")]
impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            bridge_enabled: false,
            bridge_port: default_bridge_port(),
            pool: crate::browser::BrowserPoolConfig::default(),
            profiles: vec![crate::browser::BrowserProfile::default()],
        }
    }
}

#[cfg(feature = "browser")]
fn default_bridge_port() -> u16 {
    18800
}

// Used by always-compiled configs (e.g. HeadlessConfig), so it must not be
// feature-gated even though the browser config also uses it.
fn default_false() -> bool {
    false
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            app: AppConfig::default(),
            server: ServerConfig::default(),
            logging: LoggingConfig::default(),
            storage: StorageConfig::default(),
            #[cfg(feature = "browser")]
            browser: BrowserConfig::default(),
            memory: MemoryConfig::default(),
            heartbeat: crate::heartbeat::HeartbeatConfig::default(),
            computer: ComputerConfig::default(),
            standing_orders: crate::standing_orders::config::StandingOrderConfig::default(),
            capabilities: CapabilitiesConfig::default(),
            #[cfg(feature = "cloud")]
            cloud: crate::cloud::config::CloudConfig::default(),
            services: HashMap::new(),
            extra: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.logging.level, "info");
        assert_eq!(config.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn test_schema_version_from_toml() {
        let toml_str = r#"
schema_version = 0

[server]
host = "0.0.0.0"
port = 3000
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.schema_version, 0);
    }

    #[test]
    fn test_load_from_toml() {
        let toml_str = r#"
[server]
host = "0.0.0.0"
port = 3000

[logging]
level = "debug"
format = "json"
"#;

        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.port, 3000);
        assert_eq!(config.logging.level, "debug");
        match config.logging.format {
            LogFormat::Json => {}
            _ => panic!("Expected JSON format"),
        }
    }

    #[test]
    fn test_service_config() {
        let toml_str = r#"
[services.api]
endpoint = "https://api.example.com"
api_key = "secret123"
timeout_seconds = 60

[services.api.retry]
max_retries = 5
base_delay_ms = 500
"#;

        let config: Config = toml::from_str(toml_str).unwrap();
        let api_service = config.get_service("api").unwrap();
        assert_eq!(api_service.endpoint, "https://api.example.com");
        assert_eq!(api_service.api_key, Some(SecretRef::String("secret123".to_string())));
        assert_eq!(api_service.timeout_seconds, 60);
        assert_eq!(api_service.retry.max_retries, 5);
        assert_eq!(api_service.retry.base_delay_ms, 500);
    }

    #[test]
    fn test_service_config_with_env_ref() {
        let toml_str = r#"
[services.api]
endpoint = "https://api.example.com"
api_key = "$API_KEY"
timeout_seconds = 60
"#;

        let config: Config = toml::from_str(toml_str).unwrap();
        let api_service = config.get_service("api").unwrap();
        assert_eq!(api_service.api_key, Some(SecretRef::String("$API_KEY".to_string())));
    }

    #[test]
    fn test_service_config_with_explicit_env() {
        let toml_str = r#"
[services.api]
endpoint = "https://api.example.com"
api_key = { env = "API_KEY" }
timeout_seconds = 60
"#;

        let config: Config = toml::from_str(toml_str).unwrap();
        let api_service = config.get_service("api").unwrap();
        assert_eq!(
            api_service.api_key,
            Some(SecretRef::Explicit {
                env: Some("API_KEY".to_string()),
                file: None,
                exec: None,
            })
        );
    }

    #[test]
    fn test_service_config_with_file_ref() {
        let toml_str = r#"
[services.api]
endpoint = "https://api.example.com"
api_key = { file = "/run/secrets/api_key" }
timeout_seconds = 60
"#;

        let config: Config = toml::from_str(toml_str).unwrap();
        let api_service = config.get_service("api").unwrap();
        assert_eq!(
            api_service.api_key,
            Some(SecretRef::Explicit {
                env: None,
                file: Some(std::path::PathBuf::from("/run/secrets/api_key")),
                exec: None,
            })
        );
    }

    #[test]
    #[cfg(feature = "browser")]
    fn test_browser_config_default() {
        let config = Config::default();
        assert!(!config.browser.bridge_enabled);
        assert_eq!(config.browser.bridge_port, 18800);
        assert_eq!(config.browser.pool.default_profile, "default");
        assert_eq!(config.browser.profiles.len(), 1);
    }

    #[test]
    #[cfg(feature = "browser")]
    fn test_browser_config_from_toml() {
        let toml_str = r#"
[browser]
bridge_enabled = true
bridge_port = 18801

[browser.pool]
idle_timeout_secs = 600
cleanup_interval_secs = 120

[[browser.profiles]]
name = "default"
headless = true
viewport_width = 1280
viewport_height = 720

[[browser.profiles]]
name = "headed"
headless = false
viewport_width = 1920
viewport_height = 1080
"#;

        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.browser.bridge_enabled);
        assert_eq!(config.browser.bridge_port, 18801);
        assert_eq!(config.browser.pool.idle_timeout_secs, 600);
        assert_eq!(config.browser.pool.cleanup_interval_secs, 120);
        assert_eq!(config.browser.profiles.len(), 2);
        assert_eq!(config.browser.profiles[0].name, "default");
        assert!(config.browser.profiles[0].headless);
        assert_eq!(config.browser.profiles[1].name, "headed");
        assert!(!config.browser.profiles[1].headless);
    }

    #[test]
    fn test_computer_config_default() {
        let config = Config::default();
        assert!(config.computer.enabled);
        assert_eq!(config.computer.max_steps, 30);
        assert_eq!(config.computer.settle_delay_ms, 500);
        assert!(!config.computer.headless.enabled);
        assert_eq!(config.computer.headless.display, ":99");
        assert_eq!(config.computer.remote_control.port, 22);
        assert_eq!(config.computer.remote_control.protocol, "ssh");
    }

    #[test]
    fn test_computer_config_from_toml() {
        let toml_str = r#"
[computer]
enabled = true
max_steps = 50

[computer.remote_control]
host = "192.168.1.100"
user = "admin"
port = 2222
protocol = "ssh"
key_path = "~/.ssh/id_rsa"
display = ":1"

[computer.headless]
enabled = true
display = ":99"
width = 1280
height = 720
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.computer.enabled);
        assert_eq!(config.computer.max_steps, 50);
        assert_eq!(config.computer.remote_control.host, Some("192.168.1.100".to_string()));
        assert_eq!(config.computer.remote_control.user, Some("admin".to_string()));
        assert_eq!(config.computer.remote_control.port, 2222);
        assert_eq!(config.computer.remote_control.protocol, "ssh");
        assert_eq!(config.computer.remote_control.key_path, Some("~/.ssh/id_rsa".to_string()));
        assert_eq!(config.computer.remote_control.display, Some(":1".to_string()));
        assert!(config.computer.headless.enabled);
        assert_eq!(config.computer.headless.display, ":99");
        assert_eq!(config.computer.headless.width, 1280);
        assert_eq!(config.computer.headless.height, 720);
    }

    #[test]
    fn test_standing_orders_config_default() {
        let config = Config::default();
        assert!(config.standing_orders.enabled);
        assert!(config.standing_orders.orders.is_empty());
    }

    #[test]
    fn test_standing_orders_config_from_toml() {
        let toml_str = r#"
[standing_orders]
enabled = true

[[standing_orders.orders]]
name = "daily_summary"
description = "Send daily summary to team channel"
agent_id = "assistant"
schedule = "0 0 9 * * *"
prompt = "Generate a summary of today's key events and priorities."
output_channel = "slack_general"
enabled = true

[[standing_orders.orders]]
name = "hourly_check"
agent_id = "monitor"
schedule = "0 * * * * *"
prompt = "Check system health and report anomalies."
enabled = false
timeout_secs = 30
"#;

        let config: Config = toml::from_str(toml_str).unwrap();
        let so = &config.standing_orders;
        assert!(so.enabled);
        assert_eq!(so.orders.len(), 2);

        let first = &so.orders[0];
        assert_eq!(first.name, "daily_summary");
        assert_eq!(first.description.as_deref(), Some("Send daily summary to team channel"));
        assert_eq!(first.agent_id, "assistant");
        assert_eq!(first.schedule, "0 0 9 * * *");
        assert_eq!(first.prompt, "Generate a summary of today's key events and priorities.");
        assert_eq!(first.output_channel.as_deref(), Some("slack_general"));
        assert!(first.enabled);
        assert!(first.timeout_secs.is_none());

        let second = &so.orders[1];
        assert_eq!(second.name, "hourly_check");
        assert!(!second.enabled);
        assert_eq!(second.timeout_secs, Some(30));
    }
}
