//! Configuration management for Syscity
//!
//! This module handles loading and validating configuration from
//! multiple sources: defaults, config files, and environment variables.
// INVARIANTS-NONE: revision-CAS correctness is enforced atomically at config.set time; no separately checkable persisted artifact yet.

pub mod hot_reload;
mod loader;
mod types;
mod watch;

#[cfg(feature = "browser")]
pub use types::BrowserConfig;
pub use types::{
    AppConfig, CapabilitiesConfig, ComputerConfig, Config, HeadlessConfig, LogFormat,
    LogRotationConfig, LoggingConfig, MemoryConfig, MemoryDreamingConfig,
    MemoryEffectivenessConfig, MemoryMultimodalConfig, MemoryTierConfig, MobileControlConfig,
    RemoteControlConfig, RetryConfig, ServerConfig, ServiceConfig, StorageConfig, StorageType,
    CURRENT_SCHEMA_VERSION, DEFAULT_CONFIG_FILE, ENV_PREFIX,
};
pub use watch::{ConfigChangeCallback, ConfigWatcher, ReloadableConfig};
