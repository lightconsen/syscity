//! Tool sandboxing
//!
//! [`SandboxedTool`] wraps any [`Tool`] with path-access restrictions, network
//! access control, and a hard execution timeout.  Violations return
//! [`SyscityError::SandboxViolation`] rather than panicking.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::error::SyscityError;
use crate::tools::sdk::ToolCapabilities;

/// Configuration for the sandbox around a tool.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Whether file-path arguments are evaluated at all.
    ///
    /// When `false`, any tool argument that looks like a file path is rejected.
    pub allow_file_access: bool,

    /// Whether tools may make outbound network calls.
    ///
    /// This is *advisory*: the registry cannot actually block syscalls, but it
    /// prevents the LLM from invoking network-capable tools through this
    /// wrapper without triggering an error.
    pub allow_network_access: bool,

    /// Paths that are explicitly permitted (empty = no allowlist restriction).
    ///
    /// When non-empty the requested path must be a prefix match of at least
    /// one entry here, **after** the blocklist has been checked.
    pub allowed_paths: Vec<PathBuf>,

    /// Paths that are always denied.
    pub blocked_paths: Vec<PathBuf>,

    /// Maximum wall-clock time the wrapped tool may run.
    pub timeout: Duration,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            allow_file_access: true,
            allow_network_access: false,
            allowed_paths: vec![],
            blocked_paths: vec![],
            timeout: Duration::from_secs(60),
        }
    }
}

impl SandboxConfig {
    /// Check whether `path` is permitted under this sandbox configuration.
    pub fn check_path(&self, path: &Path) -> crate::Result<()> {
        if !self.allow_file_access {
            return Err(SyscityError::SandboxViolation(
                "file access is disabled in this sandbox".to_string(),
            ));
        }

        // Blocklist takes priority.
        for blocked in &self.blocked_paths {
            if path.starts_with(blocked) {
                return Err(SyscityError::SandboxViolation(format!(
                    "access to '{}' is blocked",
                    path.display()
                )));
            }
        }

        // If an allowlist is configured, the path must match at least one entry.
        if !self.allowed_paths.is_empty() {
            let permitted = self.allowed_paths.iter().any(|a| path.starts_with(a));
            if !permitted {
                return Err(SyscityError::SandboxViolation(format!(
                    "'{}' is not in the sandbox allowlist",
                    path.display()
                )));
            }
        }

        Ok(())
    }
}

/// A [`Tool`] wrapper that enforces path restrictions and a hard timeout.
///
/// # Example
///
/// ```rust,no_run
/// # use std::path::PathBuf;
/// # use std::time::Duration;
/// # use syscity::tools::sandbox::{SandboxConfig, SandboxedTool};
/// # use syscity::tools::shell::ShellTool;
/// # fn make_shell_tool() -> ShellTool { ShellTool::new() }
/// let config = SandboxConfig {
///     allowed_paths: vec![PathBuf::from("/home/user/projects"), PathBuf::from("/tmp")],
///     blocked_paths: vec![PathBuf::from("/etc")],
///     timeout: Duration::from_secs(30),
///     ..Default::default()
/// };
/// let sandboxed = SandboxedTool::new(make_shell_tool(), config);
/// ```
pub struct SandboxedTool {
    inner: Arc<dyn Tool>,
    config: SandboxConfig,
}

impl std::fmt::Debug for SandboxedTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxedTool")
            .field("tool", &self.inner.name())
            .field("config", &self.config)
            .finish()
    }
}

impl SandboxedTool {
    /// Wrap `tool` with the given `config`.
    pub fn new(tool: impl Tool + 'static, config: SandboxConfig) -> Self {
        Self { inner: Arc::new(tool), config }
    }

    /// Inspect `args` for any path-like field and validate it.
    ///
    /// Checks the following common field names: `"path"`, `"file"`,
    /// `"directory"`, `"dir"`, `"source"`, `"destination"`, `"dst"`.
    fn check_path_args(&self, args: &Value) -> crate::Result<()> {
        const PATH_FIELDS: &[&str] = &[
            "path",
            "file",
            "directory",
            "dir",
            "source",
            "destination",
            "dst",
        ];

        for field in PATH_FIELDS {
            if let Some(raw) = args.get(field).and_then(|v| v.as_str()) {
                let path = Path::new(raw);
                self.config.check_path(path)?;
            }
        }

        Ok(())
    }
}

#[async_trait]
impl Tool for SandboxedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }

    fn capabilities(&self) -> ToolCapabilities {
        // The wrapper's own restriction is *added* to the wrapped tool's
        // declaration, never substituted for it: permission matching and the
        // requires_approval fallback must see the tool's real risk/approval
        // posture. (This used to replace the inner caps entirely, so `shell`
        // advertised no-approval/Medium here while shell.rs declared
        // approval/High.)
        let mut caps = self.inner.capabilities();
        if !caps.categories.iter().any(|c| c == "sandbox") {
            caps.categories.push("sandbox".to_string());
        }
        caps
    }

    fn is_available(&self, context: &ToolContext) -> bool {
        self.inner.is_available(context)
    }

    fn timeout(&self, _context: &ToolContext) -> Duration {
        // Override: our sandbox timeout is the binding constraint.
        self.config.timeout
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // ── path checks ──────────────────────────────────────────────────
        self.check_path_args(&args)?;

        // ── network access guard ─────────────────────────────────────────
        // Check the tool's advertised capabilities: if it has the "network"
        // category, it requires network access. This is more robust than
        // heuristic name matching.
        if !self.config.allow_network_access {
            let caps = self.inner.capabilities();
            let needs_network = caps.categories.iter().any(|c| c == "network");
            if needs_network {
                return Err(SyscityError::SandboxViolation(format!(
                    "network access is disabled; tool '{}' requires network",
                    self.inner.name()
                )));
            }
        }

        // ── timeout ──────────────────────────────────────────────────────
        let inner = Arc::clone(&self.inner);
        let exec_future = async move { inner.execute(args, context).await };

        tokio::time::timeout(self.config.timeout, exec_future)
            .await
            .map_err(|_| {
                SyscityError::SandboxViolation(format!(
                    "tool '{}' timed out after {:?}",
                    self.inner.name(),
                    self.config.timeout
                ))
            })?
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::tools::ToolExecutionResult;

    // ── minimal stub tool ────────────────────────────────────────────────
    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes args"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(
            &self,
            args: Value,
            _context: &ToolContext,
        ) -> crate::Result<ToolExecutionResult> {
            Ok(ToolExecutionResult::success(args.to_string()))
        }
    }

    struct SlowTool;

    #[async_trait]
    impl Tool for SlowTool {
        fn name(&self) -> &str {
            "slow"
        }
        fn description(&self) -> &str {
            "Sleeps"
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(
            &self,
            _args: Value,
            _context: &ToolContext,
        ) -> crate::Result<ToolExecutionResult> {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(ToolExecutionResult::success("done".to_string()))
        }
    }

    fn dummy_context() -> ToolContext {
        ToolContext {
            identity: crate::tools::ToolIdentity {
                user_id: "u1".to_string(),
                conversation_id: "c1".to_string(),
                sender_id: None,
            },
            sandbox: crate::tools::ToolSandbox {
                working_directory: PathBuf::from("/tmp"),
                environment: Default::default(),
                timeout: Duration::from_secs(10),
                allowed_paths: vec![],
                allowed_commands: vec![],
                sandboxed: false,
                memory_limit: None,
                cpu_limit: None,
                fd_limit: None,
                process_limit: None,
                workspace_root: Some(PathBuf::from("/tmp")),
                agent_workspace: None,
                workspace_only: false,
                fence_network: false,
                fence_namespaces: crate::tools::process_runner::NamespacePosture::Auto,
                plugin_allowlist: None,
            },
            model: crate::tools::ToolModel {
                model_name: None,
                provider_name: None,
                skill_trust: crate::tools::SkillTrust::Trusted,
            },
            delegation: None,
            ask_queue: None,
            approval_queue: None,
        }
    }

    #[tokio::test]
    async fn test_allowed_path_passes() {
        let config = SandboxConfig {
            allowed_paths: vec![PathBuf::from("/tmp")],
            ..Default::default()
        };
        let tool = SandboxedTool::new(EchoTool, config);
        let args = json!({"path": "/tmp/file.txt"});
        let result = tool.execute(args, &dummy_context()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_blocked_path_rejected() {
        let config = SandboxConfig {
            blocked_paths: vec![PathBuf::from("/etc")],
            ..Default::default()
        };
        let tool = SandboxedTool::new(EchoTool, config);
        let args = json!({"path": "/etc/passwd"});
        let result = tool.execute(args, &dummy_context()).await;
        assert!(matches!(result, Err(SyscityError::SandboxViolation(_))));
    }

    #[tokio::test]
    async fn test_path_not_in_allowlist_rejected() {
        let config = SandboxConfig {
            allowed_paths: vec![PathBuf::from("/tmp")],
            ..Default::default()
        };
        let tool = SandboxedTool::new(EchoTool, config);
        let args = json!({"path": "/home/user/secret"});
        let result = tool.execute(args, &dummy_context()).await;
        assert!(matches!(result, Err(SyscityError::SandboxViolation(_))));
    }

    #[tokio::test]
    async fn test_file_access_disabled() {
        let config = SandboxConfig {
            allow_file_access: false,
            ..Default::default()
        };
        let tool = SandboxedTool::new(EchoTool, config);
        let args = json!({"path": "/tmp/anything"});
        let result = tool.execute(args, &dummy_context()).await;
        assert!(matches!(result, Err(SyscityError::SandboxViolation(_))));
    }

    #[tokio::test]
    async fn test_timeout_enforced() {
        let config = SandboxConfig {
            timeout: Duration::from_millis(50),
            ..Default::default()
        };
        let tool = SandboxedTool::new(SlowTool, config);
        let result = tool.execute(json!({}), &dummy_context()).await;
        assert!(matches!(result, Err(SyscityError::SandboxViolation(_))));
    }

    #[tokio::test]
    async fn test_network_tool_blocked() {
        let config = SandboxConfig {
            allow_network_access: false,
            ..Default::default()
        };

        struct WebTool;
        #[async_trait]
        impl Tool for WebTool {
            fn name(&self) -> &str {
                "web_fetch"
            }
            fn description(&self) -> &str {
                ""
            }
            fn parameters_schema(&self) -> Value {
                json!({})
            }
            fn capabilities(&self) -> ToolCapabilities {
                ToolCapabilities {
                    categories: vec!["network".to_string()],
                    ..Default::default()
                }
            }
            async fn execute(
                &self,
                _: Value,
                _: &ToolContext,
            ) -> crate::Result<ToolExecutionResult> {
                Ok(ToolExecutionResult::success("ok".to_string()))
            }
        }

        let tool = SandboxedTool::new(WebTool, config);
        let result = tool.execute(json!({}), &dummy_context()).await;
        assert!(matches!(result, Err(SyscityError::SandboxViolation(_))));
    }
    #[test]
    fn sandboxed_tool_preserves_inner_capabilities() {
        // The wrapper adds its own category; it must not substitute its own
        // risk/approval posture for the wrapped tool's — permission matching
        // sees shell's real "requires approval / High" through the wrapper.
        struct ShellLikeTool;

        #[async_trait]
        impl Tool for ShellLikeTool {
            fn name(&self) -> &str {
                "shell"
            }
            fn description(&self) -> &str {
                ""
            }
            fn parameters_schema(&self) -> Value {
                json!({})
            }
            fn capabilities(&self) -> ToolCapabilities {
                ToolCapabilities {
                    requires_approval: true,
                    risk_level: crate::tools::approval::RiskLevel::High,
                    categories: vec!["system".to_string(), "exec".to_string()],
                    ..Default::default()
                }
            }
            async fn execute(
                &self,
                _: Value,
                _: &ToolContext,
            ) -> crate::Result<ToolExecutionResult> {
                Ok(ToolExecutionResult::success("ok".to_string()))
            }
        }

        let config = SandboxConfig::default();
        let wrapped = SandboxedTool::new(ShellLikeTool, config);
        let caps = wrapped.capabilities();
        assert!(caps.requires_approval, "inner approval requirement survives");
        assert_eq!(caps.risk_level, crate::tools::approval::RiskLevel::High);
        assert!(caps.categories.contains(&"exec".to_string()));
        assert!(caps.categories.contains(&"sandbox".to_string()));
    }
}
