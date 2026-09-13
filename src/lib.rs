//! Syscity - Personal AI Assistant
//!
//! Syscity is a lightweight, fast, and secure Personal AI Assistant written in
//! Rust. It combines the simplicity philosophy of NanoClaw with the performance
//! characteristics of ZeroClaw.
//!
//! # Architecture
//!
//! - **Core** (`core`): Domain models and business logic
//! - **Providers** (`providers`): LLM provider abstractions (OpenAI, Anthropic,
//!   etc.)
//! - **Channels** (`channels`): Communication interfaces (CLI, Telegram,
//!   Discord, etc.)
//! - **Tools** (`tools`): Capabilities for the AI to interact with the world
//! - **Adapters** (`adapters`): External service integrations
//! - **Config** (`config`): Configuration management
//! - **CLI** (`cli`): Command-line interface
//! - **Utils** (`utils`): Shared utilities
//!
//! # Example Usage
//!
//! ```rust
//! use syscity::config::Config;
//! use syscity::providers::{CompletionRequest, Message, Role};
//!
//! # async fn example() -> syscity::error::Result<()> {
//! let config = Config::load()?;
//! // ... use providers, channels, tools
//! # Ok(())
//! # }
//! ```

// rust_2018_idioms disabled to avoid elided_lifetime_in_paths noise
// Documentation warnings allowed - public APIs are documented as needed
#![allow(missing_docs)]
#![deny(unsafe_code)]
#![recursion_limit = "256"]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod acp;
pub mod adapters;
pub mod agent;
pub mod attachments;
pub mod browser;
pub mod canvas;
pub mod channels;
pub mod cli;
pub mod client;
#[cfg(feature = "cloud")]
pub mod cloud;
pub mod computer;
pub mod config;
pub mod core;
pub mod cron;
pub mod daemon;
pub mod delegation;
pub mod device;
pub mod dirs;
pub mod embed;
pub mod error;
pub mod eval;
pub mod export;
pub mod gateway;
pub mod goal;
pub mod heartbeat;
pub mod hooks;
pub mod inbound;
pub mod logs;
pub mod mcp;
pub mod memory;
pub mod model_router;
pub mod observe;
pub mod office;
pub mod outbound;
pub mod planner;
pub mod plugins;
pub mod providers;
pub mod rag;
pub mod secrets;
pub mod security;
pub mod skills;
pub mod standing_orders;
pub mod test_helpers;
pub mod tools;
pub mod tui;
pub mod update;
pub mod utils;

// Re-export commonly used types
// Backward-compat: capabilities moved under computer/ (now platform/)
pub use computer::platform;
// Re-export hot reload types
pub use config::hot_reload::{
    ConfigChangeEvent, ConfigChangeType, ConfigFileType, HotReloadBuilder, HotReloadManager,
    WatchedConfig,
};
pub use config::{Config, ConfigWatcher, ReloadableConfig};
pub use error::{Result, SyscityError};

/// Application version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Git commit hash (embedded at build time via build.rs)
pub const GIT_HASH: &str = env!("GIT_HASH");

/// Application name
pub const NAME: &str = env!("CARGO_PKG_NAME");

/// Application description
pub const DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");

/// Application authors
pub const AUTHORS: &str = env!("CARGO_PKG_AUTHORS");

/// Check if the application is running in a production environment
pub fn is_production() -> bool {
    std::env::var("SYSCITY_ENV")
        .map(|v| v == "production")
        .unwrap_or(false)
}

/// Get the current environment name
pub fn environment() -> String {
    std::env::var("SYSCITY_ENV").unwrap_or_else(|_| "development".to_string())
}

/// Initialize the application
///
/// This function sets up logging, panic handlers, and other
/// global initialization.
pub fn init() -> Result<()> {
    utils::logging::setup_panic_handler();

    // Pin the process-wide filesystem root here, at the library's documented
    // entry point, so an embedder that calls `init()` cannot reach a `dirs::`
    // free function before a root exists. `SYSCITY_HOME` / `~` is read exactly
    // once — here — rather than lazily at whatever call site happens to run
    // first.
    //
    // `Err` means a root was already installed (an embedder or test pinned its
    // own first); that one wins, and the difference is harmless because both
    // resolve to the same layout.
    let root = std::sync::Arc::new(crate::dirs::SyscityPaths::from_env());
    if crate::dirs::set_default_paths(root).is_err() {
        tracing::debug!("path root already installed; keeping the existing one");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        assert!(!VERSION.is_empty());
        assert!(!NAME.is_empty());
    }

    #[test]
    fn test_environment() {
        // Should return development by default
        let env = environment();
        assert!(
            env == "development" || !std::env::var("SYSCITY_ENV").unwrap_or_default().is_empty()
        );
    }
}
