//! Skill System for Syscity
//!
//! A comprehensive skill system supporting:
//! - Hot reloading with file watcher
//! - Installation specifications (brew, npm, go, uv, download)
//! - Runtime gating (binaries, env vars, config, OS)
//! - Multi-level skill storage (workspace, project, user, bundled)
//! - Token optimization (path compaction, size limits)
//! - Slash command integration
//! - YAML frontmatter with SKILL.md format
//!
//! The module is split into focused submodules:
//! - `types`: trigger and metadata types (`TriggerType`, `SkillTrigger`,
//!   `SkillRequires`, `SkillMetadata`)
//! - `skill`: the [`Skill`] struct and its behavior
//! - `manager`: [`SkillManager`] loading / hot reload / dependency logic
//! - `chain`: dependency-check results and execution chains
//! - `guard`: security scanning for skills and user input
// INVARIANTS-NONE: the catalog is loaded read-only from disk with digest-based whole-file replacement.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};

mod builtin;
mod builtin_macros;
mod chain;
mod config;
pub mod dependencies;
mod frontmatter;
pub mod guard;
mod install;
mod manager;
pub mod registry;
pub mod semver;
mod skill;
mod storage;
mod types;
mod watcher;

pub use chain::{DependencyCheckResult, SkillChain, VersionMismatch};
pub use config::{SkillConfig, SkillEntryConfig};
pub use dependencies::{resolve_skill_chain, DependencyGraph, DependencySpec};
pub use frontmatter::{
    parse_skill_md, InstallSpec as SkillInstallSpec, SkillFile, SkillFrontmatter, SkillTriggerItem,
};
pub use install::{install_all, install_binary, InstallResult};
pub use registry::{SkillListing, SkillRegistry, SkillUpdate};
pub use semver::{Version, VersionReq};
pub use skill::Skill;
pub use storage::SkillStorage;
pub use storage::StorageLevel;
pub use types::{SkillMetadata, SkillRequires, SkillTrigger, TriggerType};
pub use watcher::SkillWatcher;

/// Skill manager with hot reloading
pub struct SkillManager {
    /// Storage manager for multi-level skill lookup
    storage: SkillStorage,
    /// Loaded skills
    skills: Arc<RwLock<HashMap<String, Skill>>>,
    /// Configuration
    config: SkillConfig,
    /// File watcher
    watcher: Option<SkillWatcher>,
    /// Reload channel
    reload_tx: mpsc::Sender<String>,
    reload_rx: Arc<RwLock<mpsc::Receiver<String>>>,
    /// Layout root the user skills directory resolves against.
    paths: Arc<crate::dirs::SyscityPaths>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frontmatter_with_depends_on() {
        let content = r#"---
name: weather
version: "1.2.0"
depends_on:
  base-utils: ">=1.0.0"
  http-client: "^2.0.0"
provides:
  - forecast
  - alerts
chain:
  - summarize
---
Weather skill content.
"#;
        let file = SkillFile::parse(content, std::path::PathBuf::from("weather/SKILL.md")).unwrap();
        assert_eq!(file.frontmatter.depends_on.len(), 2);
        assert_eq!(
            file.frontmatter.depends_on.get("base-utils"),
            Some(">=1.0.0".to_string()).as_ref()
        );
        assert_eq!(file.frontmatter.provides, vec!["forecast", "alerts"]);
        assert_eq!(file.frontmatter.chain, vec!["summarize"]);
    }
}
