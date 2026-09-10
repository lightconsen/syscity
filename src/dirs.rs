//! Centralized directory management for Syscity
//!
//! All Syscity data is stored in ~/.syscity/ with the following structure:
//! ~/.syscity/
//! ├── config.toml # Configuration file
//! ├── data/ # SQLite database (syscity.db) - unified storage
//! ├── logs/ # Log files (daemon.log)
//! ├── skills/ # User-installed skills
//! ├── secrets/ # Encrypted secret store (see docs/secret-storage.md)
//! ├── agents/ # Agent configurations
//! │ └── {agent-id}/
//! │ ├── personality.toml # Agent configuration
//! │ ├── workspace/ # Agent-specific workspace (AI file ops)
//! │ └── data/ # Agent runtime data (sessions, state)
//! ├── cron/ # Cron job data
//! ├── todos/ # Task persistence
//! ├── workspace/ # Default workspace for AI file operations
//! │ # (also holds SOUL.md, IDENTITY.md, BOOTSTRAP.md, USER.md)
//! └── memory/ # Legacy directory (deprecated, kept for backward compatibility)
//!
//! # Explicit roots
//!
//! The layout used to be derived implicitly from the process-global home
//! directory (or the `SYSCITY_HOME` override) at *every* call site. That made
//! it impossible to point a process at a different root at runtime and let
//! tests silently write into the real `~/.syscity`.
//!
//! [`SyscityPaths`] now owns that root explicitly. It is constructed **once**
//! at startup ([`SyscityPaths::from_env`] for the production default) and can
//! be built around any directory ([`SyscityPaths::from_root`]) for tests or for
//! embedding more than one instance in a process. The gateway threads its
//! instance through `GatewayState::paths`.
//!
//! The free functions in this module remain for backwards compatibility: they
//! delegate to the process-default [`paths`] handle, which is installed from
//! `SYSCITY_HOME` (or `<home>/.syscity`) on first use. Behaviour with no
//! `SYSCITY_HOME` set is byte-for-byte identical to the previous
//! implementation.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use tracing::{debug, info};

/// Base directory name
const SYSCITY_DIR: &str = ".syscity";

/// Get the home directory
fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// The Syscity on-disk layout rooted at an explicit base directory.
///
/// Every accessor is a cheap path join; the struct holds no open handles and
/// does not read the process environment after construction. Two handles built
/// from distinct roots therefore resolve to fully disjoint layouts, which is
/// what makes hermetic tests (and multi-instance embedding) possible.
///
/// Construct the production instance once via [`SyscityPaths::from_env`] and
/// thread it through `GatewayState::paths`; use [`SyscityPaths::from_root`] for
/// tests or per-instance roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyscityPaths {
    root: PathBuf,
}

impl SyscityPaths {
    /// Build a layout rooted at an explicit directory.
    ///
    /// No environment variable is consulted; the caller owns the root.
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Build the production layout: `SYSCITY_HOME` when set and non-empty,
    /// otherwise `<home>/.syscity`.
    pub fn from_env() -> Self {
        Self::from_root(resolve_base_dir(std::env::var_os("SYSCITY_HOME")))
    }

    /// The base directory every other path is derived from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Config directory (== the base root).
    pub fn config_dir(&self) -> PathBuf {
        self.root.clone()
    }

    /// Legacy memory directory (`<root>/memory`).
    pub fn memory_dir(&self) -> PathBuf {
        self.root.join("memory")
    }

    /// Logs directory (`<root>/logs`).
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// User-installed skills directory (`<root>/skills`).
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    /// Agents directory (`<root>/agents`).
    pub fn agents_dir(&self) -> PathBuf {
        self.root.join("agents")
    }

    /// A specific agent's base directory (`<root>/agents/{id}`).
    pub fn agent_dir(&self, agent_id: &str) -> PathBuf {
        self.agents_dir().join(agent_id)
    }

    /// A specific agent's workspace (`<root>/agents/{id}/workspace`).
    pub fn agent_workspace_dir(&self, agent_id: &str) -> PathBuf {
        self.agent_dir(agent_id).join("workspace")
    }

    /// A specific agent's runtime data (`<root>/agents/{id}/data`).
    pub fn agent_data_dir(&self, agent_id: &str) -> PathBuf {
        self.agent_dir(agent_id).join("data")
    }

    /// Cron job directory (`<root>/cron`).
    pub fn cron_dir(&self) -> PathBuf {
        self.root.join("cron")
    }

    /// Connectors state/cache directory (`<root>/connectors`).
    pub fn connectors_dir(&self) -> PathBuf {
        self.root.join("connectors")
    }

    /// Default workspace for AI file operations (`<root>/workspace`).
    pub fn workspace_data_dir(&self) -> PathBuf {
        self.root.join("workspace")
    }

    /// Unified data directory (`<root>/data`) holding `syscity.db`.
    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    /// Todos directory (`<root>/todos`).
    pub fn todos_dir(&self) -> PathBuf {
        self.root.join("todos")
    }

    /// Teams directory (`<root>/teams`).
    pub fn teams_dir(&self) -> PathBuf {
        self.root.join("teams")
    }

    /// Goals directory (`<root>/goals`).
    pub fn goals_dir(&self) -> PathBuf {
        self.root.join("goals")
    }

    /// Plugin data directory (`<root>/plugins/data`).
    pub fn plugins_data_dir(&self) -> PathBuf {
        self.root.join("plugins").join("data")
    }

    /// Extensions directory (`<root>/extensions`).
    pub fn extensions_dir(&self) -> PathBuf {
        self.root.join("extensions")
    }

    /// Transcripts directory (`<root>/transcripts`).
    pub fn transcripts_dir(&self) -> PathBuf {
        self.root.join("transcripts")
    }

    /// Turn observability records directory (`<root>/turns`).
    pub fn turns_dir(&self) -> PathBuf {
        self.root.join("turns")
    }

    /// Content-addressed artifacts directory (`<root>/artifacts`).
    pub fn artifacts_dir(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    /// Delegation tree workspace (`<root>/delegations/{root_id}`).
    pub fn delegation_workspace_dir(&self, root_id: &str) -> PathBuf {
        self.root.join("delegations").join(root_id)
    }

    /// Tree-wide shared files (`<root>/delegations/{root_id}/shared`).
    pub fn delegation_shared_dir(&self, root_id: &str) -> PathBuf {
        self.delegation_workspace_dir(root_id).join("shared")
    }

    /// A single task's scratch directory
    /// (`<root>/delegations/{root_id}/tasks/{task_id}`).
    pub fn delegation_task_dir(&self, root_id: &str, task_id: &str) -> PathBuf {
        self.delegation_workspace_dir(root_id)
            .join("tasks")
            .join(task_id)
    }

    /// Attachment store directory (`<root>/attachments`).
    pub fn attachments_dir(&self) -> PathBuf {
        self.root.join("attachments")
    }

    /// Disk budget tracking directory (`<root>/budget`).
    pub fn budget_dir(&self) -> PathBuf {
        self.root.join("budget")
    }

    /// Session files directory (`<root>/session_files`).
    pub fn session_files_dir(&self) -> PathBuf {
        self.root.join("session_files")
    }

    /// Group sessions directory (`<root>/groups`).
    pub fn groups_dir(&self) -> PathBuf {
        self.root.join("groups")
    }

    /// Cache directory (`<root>/cache`).
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// Downloaded model directory (`<root>/models`).
    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }

    /// Secret store directory (`<root>/secrets`).
    ///
    /// Mirrors `crate::secrets::secrets_root_dir` so the `SecretStoreHandle`
    /// and the rest of the layout always share one root.
    pub fn secrets_dir(&self) -> PathBuf {
        self.root.join("secrets")
    }

    /// PID file (`<root>/daemon.pid`).
    pub fn pid_file(&self) -> PathBuf {
        self.root.join("daemon.pid")
    }

    /// Default config file (`<root>/config.toml`).
    pub fn default_config_file(&self) -> PathBuf {
        self.config_dir().join("config.toml")
    }

    /// Default unified DB (`<root>/data/syscity.db`).
    pub fn default_memory_db(&self) -> PathBuf {
        self.data_dir().join("syscity.db")
    }

    /// Default daemon log (`<root>/logs/daemon.log`).
    pub fn default_log_file(&self) -> PathBuf {
        self.logs_dir().join("daemon.log")
    }

    /// Workspace state file
    /// (`<root>/workspace/.syscity/workspace-state.json`).
    pub fn workspace_state_file(&self) -> PathBuf {
        self.workspace_data_dir()
            .join(".syscity")
            .join("workspace-state.json")
    }

    /// The canonical path for a [`FileType`].
    pub fn path_for(&self, file_type: FileType) -> PathBuf {
        match file_type {
            FileType::Config => self.default_config_file(),
            FileType::MemoryDb => self.default_memory_db(),
            FileType::Log => self.default_log_file(),
            FileType::Pid => self.pid_file(),
            FileType::Soul => self.workspace_data_dir().join("SOUL.md"),
            FileType::Identity => self.workspace_data_dir().join("IDENTITY.md"),
            FileType::Bootstrap => self.workspace_data_dir().join("BOOTSTRAP.md"),
            FileType::User => self.workspace_data_dir().join("USER.md"),
            FileType::Agents => self.workspace_data_dir().join("AGENTS.md"),
            FileType::Tools => self.workspace_data_dir().join("TOOLS.md"),
            FileType::Heartbeat => self.workspace_data_dir().join("HEARTBEAT.md"),
            FileType::Memory => self.workspace_data_dir().join("MEMORY.md"),
        }
    }
}

/// Process-default path handle, installed on first use.
static DEFAULT_PATHS: OnceLock<Arc<SyscityPaths>> = OnceLock::new();

/// The process-default path handle (constructed from `SYSCITY_HOME` or
/// `<home>/.syscity` on first call).
///
/// Gateway startup installs its explicitly-built instance here via
/// [`set_default_paths`] so that legacy free-function call sites observe the
/// same root as the injected `GatewayState::paths`.
pub fn paths() -> Arc<SyscityPaths> {
    DEFAULT_PATHS
        .get_or_init(|| Arc::new(SyscityPaths::from_env()))
        .clone()
}

/// Install the process-default path handle.
///
/// Intended to be called exactly once at startup. Returns `Err(existing)` when
/// a handle has already been installed, leaving the existing one in place; the
/// caller decides whether the difference is fatal (it is harmless when both
/// resolve to the same root). This is deliberately *not* a mutable global:
/// the root is fixed for the life of the process.
pub fn set_default_paths(paths: Arc<SyscityPaths>) -> Result<(), Arc<SyscityPaths>> {
    DEFAULT_PATHS.set(paths)
}

/// Resolve the base directory from the `SYSCITY_HOME` override value,
/// falling back to `<home>/.syscity`. Separated from the process env read
/// so tests stay pure (no process-global env mutation).
fn resolve_base_dir(override_value: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(dir) = override_value {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    home_dir()
        .map(|h| h.join(SYSCITY_DIR))
        .unwrap_or_else(|| PathBuf::from(SYSCITY_DIR))
}

/// Get the base Syscity directory (~/.syscity)
///
/// Delegates to the process-default [`paths`] handle. The `SYSCITY_HOME`
/// environment variable overrides the base directory; desktop behaviour is
/// unchanged when it is unset.
pub fn syscity_dir() -> PathBuf {
    paths().root().to_path_buf()
}

/// Get the config directory (~/.syscity)
pub fn config_dir() -> PathBuf {
    paths().config_dir()
}

/// Get the memory/database directory (~/.syscity/memory)
pub fn memory_dir() -> PathBuf {
    paths().memory_dir()
}

/// Get the workspace data directory for files (~/.syscity/workspace)
///
/// This is where SOUL.md, IDENTITY.md, BOOTSTRAP.md, and USER.md are stored.
pub fn workspace_memory_dir() -> PathBuf {
    paths().workspace_data_dir()
}

/// Deprecated: Use workspace_memory_dir() instead
#[deprecated(since = "0.1.0", note = "Use workspace_memory_dir() instead")]
pub fn memory_files_dir() -> PathBuf {
    paths().workspace_data_dir()
}

/// Get the logs directory (~/.syscity/logs)
pub fn logs_dir() -> PathBuf {
    paths().logs_dir()
}

/// Get the skills directory (~/.syscity/skills)
pub fn skills_dir() -> PathBuf {
    paths().skills_dir()
}

/// Get the agents directory (~/.syscity/agents)
pub fn agents_dir() -> PathBuf {
    paths().agents_dir()
}

/// Get a specific agent's base directory (~/.syscity/agents/{id})
pub fn agent_dir(agent_id: &str) -> PathBuf {
    paths().agent_dir(agent_id)
}

/// Get a specific agent's workspace directory
/// (~/.syscity/agents/{id}/workspace)
pub fn agent_workspace_dir(agent_id: &str) -> PathBuf {
    paths().agent_workspace_dir(agent_id)
}

/// Get a specific agent's data directory (~/.syscity/agents/{id}/data)
pub fn agent_data_dir(agent_id: &str) -> PathBuf {
    paths().agent_data_dir(agent_id)
}

/// Resolve a path, expanding `~` to the user's home directory.
///
/// If the path starts with `~` or `~/`, it is expanded using the home
/// directory. Otherwise, the path is returned unchanged.
pub fn resolve_tilde(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if let Some(path_str) = path.to_str() {
        if let Some(rest) = path_str.strip_prefix("~/") {
            if let Some(home) = home_dir() {
                return home.join(rest);
            }
        } else if path_str == "~" {
            if let Some(home) = home_dir() {
                return home;
            }
        }
    }
    path.to_path_buf()
}

/// Get the cron directory (~/.syscity/cron)
pub fn cron_dir() -> PathBuf {
    paths().cron_dir()
}

/// Get the connectors directory (~/.syscity/connectors)
///
/// Holds the connector state machine, the synced remote catalog, and the
/// versioned package cache (see `crate::mcp::connectors`).
pub fn connectors_dir() -> PathBuf {
    paths().connectors_dir()
}

/// Get the workspace data directory (~/.syscity/workspace)
pub fn workspace_data_dir() -> PathBuf {
    paths().workspace_data_dir()
}

/// Get the data directory (~/.syscity/data)
pub fn data_dir() -> PathBuf {
    paths().data_dir()
}

/// Get the todos directory (~/.syscity/todos)
pub fn todos_dir() -> PathBuf {
    paths().todos_dir()
}

/// Get the teams directory (~/.syscity/teams)
pub fn teams_dir() -> PathBuf {
    paths().teams_dir()
}

/// Get the goals directory (~/.syscity/goals)
pub fn goals_dir() -> PathBuf {
    paths().goals_dir()
}

/// Get the plugins data directory (~/.syscity/plugins/data)
pub fn plugins_data_dir() -> PathBuf {
    paths().plugins_data_dir()
}

/// Get the extensions directory (~/.syscity/extensions)
pub fn extensions_dir() -> PathBuf {
    paths().extensions_dir()
}

/// Get the transcripts directory (~/.syscity/transcripts)
pub fn transcripts_dir() -> PathBuf {
    paths().transcripts_dir()
}

/// Get the turn observability records directory (~/.syscity/turns)
pub fn turns_dir() -> PathBuf {
    paths().turns_dir()
}

/// Get the artifacts directory (~/.syscity/artifacts)
pub fn artifacts_dir() -> PathBuf {
    paths().artifacts_dir()
}

/// Get a delegation tree's shared workspace directory
/// (~/.syscity/delegations/{root_id}).
///
/// Every task under the same tree root shares this directory; file tools of
/// delegated children are confined to it (plus their own agent workspace), so
/// cross-tree and cross-task isolation come from the root_id key.  `root_id`
/// is a uuid and thus safe to use as a path segment.
pub fn delegation_workspace_dir(root_id: &str) -> PathBuf {
    paths().delegation_workspace_dir(root_id)
}

/// Tree-wide shared files directory inside a delegation workspace.
///
/// Any member of the tree may read and write here; this is the explicit place
/// to hand a file to sibling or descendant agents.
pub fn delegation_shared_dir(root_id: &str) -> PathBuf {
    paths().delegation_shared_dir(root_id)
}

/// A single task's private scratch directory inside a delegation workspace.
///
/// This is the default `workspace_root` for a delegated child: relative file
/// paths resolve here, so parallel tasks never collide even when they run the
/// same agent.
pub fn delegation_task_dir(root_id: &str, task_id: &str) -> PathBuf {
    paths().delegation_task_dir(root_id, task_id)
}

/// Get the attachment store directory (~/.syscity/attachments)
///
/// Content-addressed payloads (screenshots and other large tool-produced
/// blobs) live under `attachments/sha256/<first2>/<rest>`; created lazily by
/// the store on first write.
pub fn attachments_dir() -> PathBuf {
    paths().attachments_dir()
}

/// Get the disk budget tracking directory (~/.syscity/budget)
pub fn budget_dir() -> PathBuf {
    paths().budget_dir()
}

/// Get the session files directory (~/.syscity/session_files)
pub fn session_files_dir() -> PathBuf {
    paths().session_files_dir()
}

/// Get the group sessions directory (~/.syscity/groups)
pub fn groups_dir() -> PathBuf {
    paths().groups_dir()
}

/// Get the PID file path (~/.syscity/daemon.pid)
pub fn pid_file() -> PathBuf {
    paths().pid_file()
}

/// Get the default config file path (~/.syscity/config.toml)
pub fn default_config_file() -> PathBuf {
    paths().default_config_file()
}

/// Get the default memory DB path (~/.syscity/data/syscity.db)
///
/// Note: Previously returned ~/.syscity/memory/memory.db, now consolidated
/// to use the main gateway database for unified storage.
pub fn default_memory_db() -> PathBuf {
    paths().default_memory_db()
}

/// Get the default log file path (~/.syscity/logs/daemon.log)
pub fn default_log_file() -> PathBuf {
    paths().default_log_file()
}

/// Get the workspace state file path
/// (~/.syscity/workspace/.syscity/workspace-state.json)
pub fn workspace_state_file() -> PathBuf {
    paths().workspace_state_file()
}

/// Initialize all Syscity directories
///
/// Creates the ~/.syscity directory structure if it doesn't exist.
/// Returns the base directory path.
pub async fn init() -> crate::Result<PathBuf> {
    let paths = paths();
    let base = paths.root().to_path_buf();

    // Create all subdirectories
    let dirs = [
        base.clone(),
        paths.memory_dir(),
        paths.data_dir(),
        paths.workspace_data_dir(),
        paths.logs_dir(),
        paths.skills_dir(),
        paths.agents_dir(),
        paths.cron_dir(),
        paths.goals_dir(),
        paths.todos_dir(),
        paths.transcripts_dir(),
        paths.artifacts_dir(),
        paths.budget_dir(),
        paths.groups_dir(),
        paths.plugins_data_dir(),
    ];

    for dir in &dirs {
        if !dir.exists() {
            debug!("Creating directory: {:?}", dir);
            tokio::fs::create_dir_all(dir).await.map_err(|e| {
                crate::error::SyscityError::Storage {
                    context: format!("Failed to create directory: {:?}", dir),
                    details: e.to_string(),
                }
            })?;
        }
    }

    // Seed default agent personality templates
    seed_default_agent_personality(&base).await?;

    info!("Syscity directories initialized at: {:?}", base);
    Ok(base)
}

/// Seed the default agent (`agents/default/`) with standard
/// personality files if they don't already exist.
async fn seed_default_agent_personality(base: &Path) -> crate::Result<()> {
    let default_agent_dir = base.join("agents").join("default");
    let params = crate::agent::AgentTemplateParams::default();
    crate::agent::seed_agent_personality(&default_agent_dir, &params).await
}

/// Initialize directories synchronously (for non-async contexts)
pub fn init_sync() -> crate::Result<PathBuf> {
    let paths = paths();
    let base = paths.root().to_path_buf();

    // Create all subdirectories
    let dirs = [
        base.clone(),
        paths.memory_dir(),
        paths.data_dir(),
        paths.workspace_data_dir(),
        paths.logs_dir(),
        paths.skills_dir(),
        paths.agents_dir(),
        paths.cron_dir(),
        paths.goals_dir(),
        paths.todos_dir(),
        paths.transcripts_dir(),
        paths.artifacts_dir(),
        paths.budget_dir(),
        paths.groups_dir(),
        paths.plugins_data_dir(),
    ];

    for dir in &dirs {
        if !dir.exists() {
            debug!("Creating directory: {:?}", dir);
            std::fs::create_dir_all(dir).map_err(|e| crate::error::SyscityError::Storage {
                context: format!("Failed to create directory: {:?}", dir),
                details: e.to_string(),
            })?;
        }
    }

    // Seed default agent personality templates (sync)
    seed_default_agent_personality_sync(&base)?;

    info!("Syscity directories initialized at: {:?}", base);
    Ok(base)
}

/// Synchronous version of `seed_default_agent_personality`.
fn seed_default_agent_personality_sync(base: &Path) -> crate::Result<()> {
    let default_agent_dir = base.join("agents").join("default");
    let params = crate::agent::AgentTemplateParams::default();
    crate::agent::seed_agent_personality_sync(&default_agent_dir, &params)
}

/// Check if Syscity directories are initialized
pub fn is_initialized() -> bool {
    paths().root().exists()
}

/// Get the path for a specific file type
pub fn path_for(file_type: FileType) -> PathBuf {
    paths().path_for(file_type)
}

/// Types of files that can be retrieved
#[derive(Debug, Clone, Copy)]
pub enum FileType {
    /// Main configuration file
    Config,
    /// Memory database
    MemoryDb,
    /// Log file
    Log,
    /// PID file
    Pid,
    /// SOUL.md personality file
    Soul,
    /// IDENTITY.md personality file
    Identity,
    /// BOOTSTRAP.md personality file
    Bootstrap,
    /// USER.md user-specific memory file
    User,
    /// AGENTS.md operating instructions file
    Agents,
    /// TOOLS.md tool notes and conventions file
    Tools,
    /// HEARTBEAT.md periodic task checklist file
    Heartbeat,
    /// MEMORY.md curated long-term memory file
    Memory,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Layout entries exercised by the isolation/hermetic tests. Every entry is
    /// produced by a `SyscityPaths` method so the tests cover the whole surface.
    fn layout_entries(p: &SyscityPaths) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("root", p.root().to_path_buf()),
            ("config_dir", p.config_dir()),
            ("memory_dir", p.memory_dir()),
            ("logs_dir", p.logs_dir()),
            ("skills_dir", p.skills_dir()),
            ("agents_dir", p.agents_dir()),
            ("agent_dir", p.agent_dir("a1")),
            ("agent_workspace_dir", p.agent_workspace_dir("a1")),
            ("agent_data_dir", p.agent_data_dir("a1")),
            ("cron_dir", p.cron_dir()),
            ("connectors_dir", p.connectors_dir()),
            ("workspace_data_dir", p.workspace_data_dir()),
            ("data_dir", p.data_dir()),
            ("todos_dir", p.todos_dir()),
            ("teams_dir", p.teams_dir()),
            ("goals_dir", p.goals_dir()),
            ("plugins_data_dir", p.plugins_data_dir()),
            ("extensions_dir", p.extensions_dir()),
            ("transcripts_dir", p.transcripts_dir()),
            ("turns_dir", p.turns_dir()),
            ("artifacts_dir", p.artifacts_dir()),
            ("delegation_workspace_dir", p.delegation_workspace_dir("root-1")),
            ("delegation_shared_dir", p.delegation_shared_dir("root-1")),
            ("delegation_task_dir", p.delegation_task_dir("root-1", "task-1")),
            ("attachments_dir", p.attachments_dir()),
            ("budget_dir", p.budget_dir()),
            ("session_files_dir", p.session_files_dir()),
            ("groups_dir", p.groups_dir()),
            ("cache_dir", p.cache_dir()),
            ("models_dir", p.models_dir()),
            ("secrets_dir", p.secrets_dir()),
            ("pid_file", p.pid_file()),
            ("default_config_file", p.default_config_file()),
            ("default_memory_db", p.default_memory_db()),
            ("default_log_file", p.default_log_file()),
            ("workspace_state_file", p.workspace_state_file()),
        ]
    }

    #[test]
    fn test_distinct_roots_produce_disjoint_paths() {
        let a = SyscityPaths::from_root("/tmp/syscity-a");
        let b = SyscityPaths::from_root("/tmp/syscity-b");

        for ((name_a, path_a), (name_b, path_b)) in
            layout_entries(&a).into_iter().zip(layout_entries(&b))
        {
            assert_eq!(name_a, name_b);
            assert!(path_a.starts_with("/tmp/syscity-a"), "{name_a}: {path_a:?}");
            assert!(path_b.starts_with("/tmp/syscity-b"), "{name_b}: {path_b:?}");
            assert_ne!(path_a, path_b, "{name_a} unexpectedly equal");
        }
    }

    #[test]
    fn test_temp_root_never_touches_real_syscity_home() {
        let temp = tempfile::tempdir().expect("tempdir");
        let p = SyscityPaths::from_root(temp.path());
        // The real root the process would use if nothing was injected.
        let real_root = resolve_base_dir(std::env::var_os("SYSCITY_HOME"));

        // Every resolved path must live under the temp root — never under the
        // real `~/.syscity` (or anywhere else).
        for (name, path) in layout_entries(&p) {
            assert!(path.starts_with(temp.path()), "{name} escaped temp root: {path:?}");
            assert!(!path.starts_with(&real_root), "{name} would touch the real root: {path:?}");
        }
    }

    #[test]
    fn test_default_handle_matches_previous_implementation() {
        // Regression guard: the default (env-derived) handle must resolve to
        // exactly the same base as the previous `resolve_base_dir` logic.
        let previous = resolve_base_dir(std::env::var_os("SYSCITY_HOME"));
        let handle = SyscityPaths::from_env();
        assert_eq!(handle.root(), previous.as_path());

        // And the derived entries must match the historical layout strings.
        let root = &previous;
        assert_eq!(handle.data_dir(), root.join("data"));
        assert_eq!(handle.agents_dir(), root.join("agents"));
        assert_eq!(handle.agent_dir("x"), root.join("agents").join("x"));
        assert_eq!(handle.default_memory_db(), root.join("data").join("syscity.db"));
        assert_eq!(handle.secrets_dir(), root.join("secrets"));
        assert_eq!(handle.logs_dir(), root.join("logs"));
        assert_eq!(handle.default_log_file(), root.join("logs").join("daemon.log"));
    }

    #[test]
    fn test_from_env_honours_syscity_home_shape() {
        // `from_env` must agree with the pure resolver for both branches.
        let empty = SyscityPaths::from_env();
        assert!(empty.root().to_string_lossy().contains(".syscity"));

        let explicit = SyscityPaths::from_root("/data/data/com.syscity/files/syscity");
        assert_eq!(
            explicit.default_memory_db(),
            PathBuf::from("/data/data/com.syscity/files/syscity/data/syscity.db")
        );
    }

    #[test]
    fn test_syscity_dir_structure() {
        // Just verify the paths are constructed correctly
        let base = syscity_dir();
        assert!(base.to_string_lossy().contains(".syscity"));

        assert!(config_dir().to_string_lossy().contains(".syscity"));
        assert!(memory_dir().to_string_lossy().contains("memory"));
        assert!(logs_dir().to_string_lossy().contains("logs"));
        assert!(skills_dir().to_string_lossy().contains("skills"));
    }

    #[test]
    fn test_path_for() {
        assert!(path_for(FileType::Config)
            .to_string_lossy()
            .contains("config.toml"));
        assert!(path_for(FileType::MemoryDb)
            .to_string_lossy()
            .contains("data/syscity.db"));
        assert!(path_for(FileType::Log)
            .to_string_lossy()
            .contains("daemon.log"));
        assert!(path_for(FileType::Pid)
            .to_string_lossy()
            .contains("daemon.pid"));
    }

    #[test]
    fn test_path_for_workspace_files() {
        assert!(path_for(FileType::Soul)
            .to_string_lossy()
            .contains("SOUL.md"));
        assert!(path_for(FileType::Identity)
            .to_string_lossy()
            .contains("IDENTITY.md"));
        assert!(path_for(FileType::Bootstrap)
            .to_string_lossy()
            .contains("BOOTSTRAP.md"));
        assert!(path_for(FileType::User)
            .to_string_lossy()
            .contains("USER.md"));
        assert!(path_for(FileType::Agents)
            .to_string_lossy()
            .contains("AGENTS.md"));
        assert!(path_for(FileType::Tools)
            .to_string_lossy()
            .contains("TOOLS.md"));
        assert!(path_for(FileType::Heartbeat)
            .to_string_lossy()
            .contains("HEARTBEAT.md"));
        assert!(path_for(FileType::Memory)
            .to_string_lossy()
            .contains("MEMORY.md"));
    }

    #[test]
    fn test_default_memory_db() {
        let db = default_memory_db();
        assert!(db.to_string_lossy().contains("data/syscity.db"));
    }

    #[test]
    fn test_pid_file() {
        let pid = pid_file();
        assert!(pid.to_string_lossy().contains("daemon.pid"));
    }

    #[test]
    fn test_default_log_file() {
        let log = default_log_file();
        assert!(log.to_string_lossy().contains("daemon.log"));
    }

    #[test]
    fn test_workspace_state_file() {
        let state = workspace_state_file();
        assert!(state.to_string_lossy().contains("workspace-state.json"));
    }

    #[test]
    fn test_transcripts_dir() {
        assert!(transcripts_dir().to_string_lossy().contains("transcripts"));
    }

    #[test]
    fn test_artifacts_dir() {
        assert!(artifacts_dir().to_string_lossy().contains("artifacts"));
    }

    #[test]
    fn test_delegation_workspace_dir() {
        let root = delegation_workspace_dir("root-1");
        assert!(root.to_string_lossy().contains("delegations"));
        assert!(root.ends_with("delegations/root-1"));
        assert_eq!(delegation_shared_dir("root-1"), root.join("shared"));
        assert_eq!(delegation_task_dir("root-1", "task-1"), root.join("tasks").join("task-1"));
    }

    #[test]
    fn test_attachments_dir() {
        assert!(attachments_dir().to_string_lossy().contains("attachments"));
    }

    #[test]
    fn test_budget_dir() {
        assert!(budget_dir().to_string_lossy().contains("budget"));
    }

    #[test]
    fn test_session_files_dir() {
        assert!(session_files_dir()
            .to_string_lossy()
            .contains("session_files"));
    }

    #[test]
    fn test_groups_dir() {
        assert!(groups_dir().to_string_lossy().contains("groups"));
    }

    #[test]
    fn test_teams_dir() {
        assert!(teams_dir().to_string_lossy().contains("teams"));
    }

    #[test]
    fn test_extensions_dir() {
        assert!(extensions_dir().to_string_lossy().contains("extensions"));
    }

    #[test]
    fn test_plugins_data_dir() {
        let dir = plugins_data_dir();
        assert!(dir.to_string_lossy().contains("plugins"));
        assert!(dir.to_string_lossy().contains("data"));
    }

    #[test]
    fn test_syscity_home_override_relocates_base() {
        let dir = resolve_base_dir(Some(std::ffi::OsString::from(
            "/data/data/com.syscity/files/syscity",
        )));
        assert_eq!(dir, PathBuf::from("/data/data/com.syscity/files/syscity"));
    }

    #[test]
    fn test_syscity_home_empty_override_falls_back() {
        let dir = resolve_base_dir(Some(std::ffi::OsString::from("")));
        assert!(dir.to_string_lossy().contains(".syscity"));
    }

    #[test]
    fn test_syscity_home_unset_falls_back() {
        let dir = resolve_base_dir(None);
        assert!(dir.to_string_lossy().contains(".syscity"));
    }

    #[test]
    fn test_is_initialized() {
        // Just verify it doesn't panic
        let _ = is_initialized();
    }

    #[test]
    fn test_resolve_tilde_home() {
        let home = home_dir().unwrap();
        assert_eq!(resolve_tilde("~"), home);
    }

    #[test]
    fn test_resolve_tilde_home_subdir() {
        let home = home_dir().unwrap();
        assert_eq!(resolve_tilde("~/projects"), home.join("projects"));
    }

    #[test]
    fn test_resolve_tilde_no_tilde() {
        let path = "/usr/local/bin";
        assert_eq!(resolve_tilde(path), PathBuf::from(path));
    }

    #[test]
    fn test_agent_dir() {
        let base = agents_dir();
        assert_eq!(agent_dir("my-agent"), base.join("my-agent"));
    }

    #[test]
    fn test_agent_workspace_dir() {
        assert_eq!(
            agent_workspace_dir("my-agent"),
            agents_dir().join("my-agent").join("workspace")
        );
    }

    #[test]
    fn test_agent_data_dir() {
        assert_eq!(agent_data_dir("my-agent"), agents_dir().join("my-agent").join("data"));
    }
}
