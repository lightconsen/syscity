//! Core tool types: execution context, results, and the [`Tool`] trait.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AskQueue, ModelCapabilities, SandboxPolicy, ToolPolicy, UserContext};
use crate::providers::{FunctionDefinition, ToolResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillTrust {
    /// Community / untrusted skill — read-only (non-privileged) tools only.
    Community = 0,
    /// Installed / trusted skill — full tool access.
    #[default]
    Trusted = 1,
}

/// A unique identifier for a tool
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolId(pub String);

impl ToolId {
    /// Create a new tool ID
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for ToolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identity fields: who is calling the tool.
#[derive(Debug, Clone, Default)]
pub struct ToolIdentity {
    /// The user ID executing the tool
    pub user_id: String,
    /// The conversation ID
    pub conversation_id: String,
    /// Identifier of the sender/user that triggered the invocation.
    pub sender_id: Option<String>,
    /// Whether the sender is the system owner.
    pub sender_is_owner: bool,
    /// Optional per-user RBAC context.
    pub user_context: Option<UserContext>,
}

/// Sandbox / execution environment: where and how the tool runs.
#[derive(Debug, Clone)]
pub struct ToolSandbox {
    /// The working directory for file operations
    pub working_directory: std::path::PathBuf,
    /// Environment variables
    pub environment: HashMap<String, String>,
    /// Timeout for tool execution
    pub timeout: Duration,
    /// Allowed paths for file operations (if empty, no restrictions)
    pub allowed_paths: Vec<std::path::PathBuf>,
    /// Allowed commands for shell execution (if empty, no restrictions)
    pub allowed_commands: Vec<String>,
    /// Whether the tool is being executed in a sandbox
    pub sandboxed: bool,
    /// Maximum memory allowed for child processes in bytes (if sandboxed)
    pub memory_limit: Option<usize>,
    /// Maximum CPU time in seconds (if sandboxed)
    pub cpu_limit: Option<u64>,
    /// Maximum number of open file descriptors
    pub fd_limit: Option<u64>,
    /// Maximum process count (for preventing fork bombs)
    pub process_limit: Option<u64>,
    /// Root directory for file operations (workspace boundary). `None` means
    /// the process-default workspace dir, resolved on first read — building a
    /// sandbox must not require an installed path root.
    pub(crate) workspace_root: Option<std::path::PathBuf>,
    /// The owning agent's own workspace, when known.
    ///
    /// Differs from `workspace_root` for delegated children, whose
    /// `workspace_root` is the per-task scratch dir inside the delegation
    /// tree while their reports still belong in the owning agent's
    /// workspace. Falls back to `workspace_root` when unset.
    pub agent_workspace: Option<std::path::PathBuf>,
    /// When true, file operations are restricted to `workspace_root`.
    pub workspace_only: bool,
    /// Optional sandbox policy applied to tool execution.
    pub sandbox_policy: Option<SandboxPolicy>,
    /// Optional allowlist of plugin tool prefixes/names.
    pub plugin_allowlist: Option<Vec<String>>,
}

impl Default for ToolSandbox {
    fn default() -> Self {
        Self {
            working_directory: std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from(".")),
            environment: default_tool_environment(),
            timeout: Duration::from_secs(30),
            allowed_paths: Vec::new(),
            allowed_commands: Vec::new(),
            sandboxed: false,
            memory_limit: None,
            cpu_limit: None,
            fd_limit: None,
            process_limit: None,
            workspace_root: None,
            agent_workspace: None,
            workspace_only: true,
            sandbox_policy: None,
            plugin_allowlist: None,
        }
    }
}

/// Model / policy metadata: capabilities and access control.
#[derive(Debug, Clone)]
pub struct ToolModel {
    /// Name of the LLM model driving the current invocation.
    pub model_name: Option<String>,
    /// Name of the LLM provider driving the current invocation.
    pub provider_name: Option<String>,
    /// Capabilities of the current model (vision, tool use, etc.).
    pub model_capabilities: ModelCapabilities,
    /// Minimum trust level from active skills.
    pub skill_trust: SkillTrust,
    /// Optional per-context RBAC policy.
    pub tool_policy: Option<ToolPolicy>,
}

impl Default for ToolModel {
    fn default() -> Self {
        Self {
            model_name: None,
            provider_name: None,
            model_capabilities: ModelCapabilities::default(),
            skill_trust: SkillTrust::Trusted,
            tool_policy: None,
        }
    }
}

/// The execution context for a tool
///
/// Groups fields into three sub-structs for construction ergonomics:
/// - `identity`: who is making the call (`user_id`, `conversation_id`, etc.)
/// - `sandbox`: execution environment (working directory, timeouts, limits)
/// - `model`: LLM / policy metadata (model name, capabilities, trust)
///
/// Common identity fields are accessible via `Deref` — `ctx.user_id` still
/// works. Sandbox fields have convenience accessors: `ctx.working_directory()`.
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    /// Identity fields (who is calling the tool)
    pub identity: ToolIdentity,
    /// Sandbox / execution environment
    pub sandbox: ToolSandbox,
    /// Model / policy metadata
    pub model: ToolModel,
    /// Active delegation scope, when this tool call runs inside a delegated
    /// child agent.  `None` for ordinary top-level conversations.
    pub delegation: Option<crate::delegation::DelegationScope>,
    /// Ask queue for the `ask_user` clarification tool. `None` in contexts
    /// with no interactive human (goals build their own context and skip it).
    pub ask_queue: Option<Arc<AskQueue>>,
}

/// Allowed environment variables that are safe to forward to child processes.
///
/// This whitelist avoids leaking secrets such as API keys into shell and other
/// subprocess executions while preserving common system variables required by
/// most programs.
const ALLOWED_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "USER", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "TERM", "TZ",
];

/// Build a whitelisted environment map from the current process environment.
fn default_tool_environment() -> HashMap<String, String> {
    std::env::vars()
        .filter(|(k, _)| ALLOWED_ENV_VARS.contains(&k.as_str()))
        .collect()
}

impl std::ops::Deref for ToolContext {
    type Target = ToolIdentity;
    fn deref(&self) -> &Self::Target {
        &self.identity
    }
}

impl ToolContext {
    /// Convenience accessors for commonly-used sandbox fields.
    pub fn working_directory(&self) -> &std::path::PathBuf {
        &self.sandbox.working_directory
    }
    pub fn environment(&self) -> &HashMap<String, String> {
        &self.sandbox.environment
    }
    pub fn timeout(&self) -> Duration {
        self.sandbox.timeout
    }
    pub fn sandboxed(&self) -> bool {
        self.sandbox.sandboxed
    }
    pub fn allowed_paths(&self) -> &[std::path::PathBuf] {
        &self.sandbox.allowed_paths
    }
    pub fn allowed_commands(&self) -> &[String] {
        &self.sandbox.allowed_commands
    }
    /// The workspace boundary, resolving the process default on first read.
    pub fn workspace_root(&self) -> std::path::PathBuf {
        self.sandbox
            .workspace_root
            .clone()
            .unwrap_or_else(crate::dirs::workspace_data_dir)
    }
    pub fn workspace_only(&self) -> bool {
        self.sandbox.workspace_only
    }
    pub fn memory_limit(&self) -> Option<usize> {
        self.sandbox.memory_limit
    }
    pub fn cpu_limit(&self) -> Option<u64> {
        self.sandbox.cpu_limit
    }
    pub fn fd_limit(&self) -> Option<u64> {
        self.sandbox.fd_limit
    }
    pub fn process_limit(&self) -> Option<u64> {
        self.sandbox.process_limit
    }
    pub fn sandbox_policy(&self) -> Option<&SandboxPolicy> {
        self.sandbox.sandbox_policy.as_ref()
    }
    pub fn plugin_allowlist(&self) -> Option<&[String]> {
        self.sandbox.plugin_allowlist.as_deref()
    }

    /// Create a new tool context
    pub fn new(user_id: impl Into<String>, conversation_id: impl Into<String>) -> Self {
        Self {
            identity: ToolIdentity {
                user_id: user_id.into(),
                conversation_id: conversation_id.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Set the workspace root directory
    /// Set workspace root
    pub fn with_workspace_root(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.sandbox.workspace_root = Some(path.into());
        self
    }

    /// Set the owning agent's own workspace (see [`ToolSandbox::agent_workspace`]).
    pub fn with_agent_workspace(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.sandbox.agent_workspace = Some(path.into());
        self
    }

    /// Set workspace-only mode (restrict file ops to workspace_root)
    pub fn with_workspace_only(mut self, enabled: bool) -> Self {
        self.sandbox.workspace_only = enabled;
        self
    }

    /// Set the minimum skill trust level (controls which tools are exposed).
    pub fn with_skill_trust(mut self, trust: SkillTrust) -> Self {
        self.model.skill_trust = trust;
        self
    }

    /// Set the RBAC user context for policy evaluation.
    pub fn with_user_context(mut self, ctx: UserContext) -> Self {
        self.identity.user_context = Some(ctx);
        self
    }

    /// Set the RBAC policy applied to this context.
    pub fn with_tool_policy(mut self, policy: ToolPolicy) -> Self {
        self.model.tool_policy = Some(policy);
        self
    }

    /// Set the model name for model-based tool gating.
    pub fn with_model_name(mut self, model: impl Into<String>) -> Self {
        self.model.model_name = Some(model.into());
        self
    }

    /// Set the provider name for provider-based tool gating.
    pub fn with_provider_name(mut self, provider: impl Into<String>) -> Self {
        self.model.provider_name = Some(provider.into());
        self
    }

    /// Set the sender identifier for sender-based tool gating.
    pub fn with_sender_id(mut self, sender: impl Into<String>) -> Self {
        self.identity.sender_id = Some(sender.into());
        self
    }

    /// Mark the sender as the system owner.
    pub fn with_sender_is_owner(mut self, is_owner: bool) -> Self {
        self.identity.sender_is_owner = is_owner;
        self
    }

    /// Set an allowlist of plugin tool prefixes/names.
    pub fn with_plugin_allowlist(mut self, allowlist: Vec<String>) -> Self {
        self.sandbox.plugin_allowlist = Some(allowlist);
        self
    }

    /// Set the model capabilities for model-based tool gating.
    pub fn with_model_capabilities(mut self, capabilities: ModelCapabilities) -> Self {
        self.model.model_capabilities = capabilities;
        self
    }

    /// Attach the active delegation scope for this tool call.
    pub fn with_delegation(mut self, scope: Option<crate::delegation::DelegationScope>) -> Self {
        self.delegation = scope;
        self
    }

    /// Attach the ask queue so `ask_user` can suspend for a human answer.
    pub fn with_ask_queue(mut self, queue: Arc<AskQueue>) -> Self {
        self.ask_queue = Some(queue);
        self
    }

    /// Set the sandbox policy applied to tool execution.
    pub fn with_sandbox_policy(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox.sandbox_policy = Some(policy);
        self
    }

    /// Set the working directory
    pub fn with_working_dir(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.sandbox.working_directory = path.into();
        self
    }

    /// Set the timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.sandbox.timeout = timeout;
        self
    }

    /// Add an allowed path
    pub fn allow_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.sandbox.allowed_paths.push(path.into());
        self
    }

    /// Add an allowed command
    pub fn allow_command(mut self, command: impl Into<String>) -> Self {
        self.sandbox.allowed_commands.push(command.into());
        self
    }

    /// Set sandboxed mode
    pub fn with_sandboxed(mut self, sandboxed: bool) -> Self {
        self.sandbox.sandboxed = sandboxed;
        self
    }

    /// Set memory limit in bytes (only effective when sandboxed)
    pub fn with_memory_limit(mut self, bytes: usize) -> Self {
        self.sandbox.memory_limit = Some(bytes);
        self
    }

    /// Set CPU time limit in seconds (only effective when sandboxed)
    pub fn with_cpu_limit(mut self, seconds: u64) -> Self {
        self.sandbox.cpu_limit = Some(seconds);
        self
    }

    /// Set file descriptor limit (only effective when sandboxed)
    pub fn with_fd_limit(mut self, count: u64) -> Self {
        self.sandbox.fd_limit = Some(count);
        self
    }

    /// Set process limit for preventing fork bombs (only effective when
    /// sandboxed)
    pub fn with_process_limit(mut self, count: u64) -> Self {
        self.sandbox.process_limit = Some(count);
        self
    }

    /// Apply resource limits to the current process (Unix only)
    /// This should be called in a pre_exec hook before spawning the child
    /// process
    #[cfg(unix)]
    pub fn apply_resource_limits(&self) -> std::io::Result<()> {
        use std::io;

        // Only apply limits if sandboxed
        if !self.sandbox.sandboxed {
            return Ok(());
        }

        // Apply memory limit
        if let Some(memory_limit) = self.sandbox.memory_limit {
            // SAFETY: setrlimit is a standard POSIX syscall that modifies resource
            // limits for the current process. It is async-signal-safe and does not
            // access invalid memory.
            #[allow(unsafe_code)]
            unsafe {
                let limit = libc::rlimit {
                    rlim_cur: memory_limit as libc::rlim_t,
                    rlim_max: memory_limit as libc::rlim_t,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }

        // Apply CPU limit
        if let Some(cpu_limit) = self.sandbox.cpu_limit {
            // SAFETY: setrlimit is a standard POSIX syscall that modifies resource
            // limits for the current process. It is async-signal-safe and does not
            // access invalid memory.
            #[allow(unsafe_code)]
            unsafe {
                let limit = libc::rlimit {
                    rlim_cur: cpu_limit as libc::rlim_t,
                    rlim_max: cpu_limit as libc::rlim_t,
                };
                if libc::setrlimit(libc::RLIMIT_CPU, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }

        // Apply file descriptor limit
        if let Some(fd_limit) = self.sandbox.fd_limit {
            // SAFETY: setrlimit is a standard POSIX syscall that modifies resource
            // limits for the current process. It is async-signal-safe and does not
            // access invalid memory.
            #[allow(unsafe_code)]
            unsafe {
                let limit = libc::rlimit {
                    rlim_cur: fd_limit as libc::rlim_t,
                    rlim_max: fd_limit as libc::rlim_t,
                };
                if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }

        // Apply process limit (NPROC)
        if let Some(process_limit) = self.sandbox.process_limit {
            // SAFETY: setrlimit is a standard POSIX syscall that modifies resource
            // limits for the current process. It is async-signal-safe and does not
            // access invalid memory.
            #[allow(unsafe_code)]
            unsafe {
                let limit = libc::rlimit {
                    rlim_cur: process_limit as libc::rlim_t,
                    rlim_max: process_limit as libc::rlim_t,
                };
                if libc::setrlimit(libc::RLIMIT_NPROC, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }

        Ok(())
    }

    /// Apply resource limits is a no-op on non-Unix platforms
    #[cfg(not(unix))]
    pub fn apply_resource_limits(&self) -> std::io::Result<()> {
        // Resource limits are not implemented for non-Unix platforms
        Ok(())
    }

    /// Get a human-readable summary of resource limits
    pub fn resource_limits_summary(&self) -> String {
        if !self.sandbox.sandboxed {
            return "No sandbox (no resource limits)".to_string();
        }

        let mut parts = vec!["Sandbox active".to_string()];

        if let Some(mem) = self.sandbox.memory_limit {
            parts.push(format!("Memory: {} MB", mem / 1024 / 1024));
        }
        if let Some(cpu) = self.sandbox.cpu_limit {
            parts.push(format!("CPU: {}s", cpu));
        }
        if let Some(fd) = self.sandbox.fd_limit {
            parts.push(format!("FDs: {}", fd));
        }
        if let Some(proc) = self.sandbox.process_limit {
            parts.push(format!("Processes: {}", proc));
        }

        if parts.len() == 1 {
            parts.push("No specific limits set".to_string());
        }

        parts.join(" | ")
    }

    /// Check if a path is allowed
    pub fn is_path_allowed(&self, path: &std::path::Path) -> bool {
        // ── allowlist check ────────────────────────────────────────────────
        if !self.sandbox.allowed_paths.is_empty() {
            let path_canon = path.canonicalize().ok();
            let path_raw = path.to_path_buf();
            let in_allowlist = self.sandbox.allowed_paths.iter().any(|allowed| {
                // Try canonical comparison first (handles symlinks)
                if let Ok(ref ac) = allowed.canonicalize() {
                    if let Some(ref pc) = path_canon {
                        if pc.starts_with(ac) {
                            return true;
                        }
                    }
                }
                // Fallback to raw path comparison for non-existent paths
                path_raw.starts_with(allowed)
            });
            // When an allowlist is present it acts as a whitelist:
            // paths inside the allowlist are permitted, everything else is denied.
            return in_allowlist;
        }

        // ── workspace boundary check ──────────────────────
        if self.sandbox.workspace_only {
            let resolved = self.resolve_path(path);
            let resolved_canon = resolved.canonicalize().ok();
            let root_canon = self.workspace_root().canonicalize().ok();

            let within = if let (Some(ref rc), Some(ref wc)) = (resolved_canon, root_canon) {
                rc.starts_with(wc)
            } else {
                resolved.starts_with(self.workspace_root())
            };

            if !within {
                return false;
            }
        }

        true
    }

    /// Resolve a path relative to the workspace root.
    ///
    /// * Absolute paths are returned as-is (but still subject to
    ///   `is_path_allowed`).
    /// * Relative paths are joined with `workspace_root`.
    /// * `~` is expanded to the user's home directory.
    pub fn resolve_path(&self, path: &std::path::Path) -> std::path::PathBuf {
        // Expand tilde
        let expanded = if let Some(path_str) = path.to_str() {
            if path_str.starts_with("~/") || path_str == "~" {
                if let Some(home) = dirs::home_dir() {
                    let rest = &path_str[1..];
                    home.join(rest.trim_start_matches('/'))
                } else {
                    path.to_path_buf()
                }
            } else {
                path.to_path_buf()
            }
        } else {
            path.to_path_buf()
        };

        if expanded.is_absolute() {
            expanded
        } else {
            self.workspace_root().join(expanded)
        }
    }

    /// Check if a command is allowed
    pub fn is_command_allowed(&self, command: &str) -> bool {
        if self.sandbox.allowed_commands.is_empty() {
            return true;
        }
        let cmd = command.split_whitespace().next().unwrap_or(command);
        self.sandbox
            .allowed_commands
            .iter()
            .any(|allowed| allowed == cmd)
    }
}

/// A chunk emitted by a streaming tool execution.
///
/// Streaming tools can emit output, errors, and structured data incrementally
/// instead of buffering everything into a single [`ToolExecutionResult`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "payload")]
pub enum ToolExecutionChunk {
    /// Standard output / progress text.
    Output(String),
    /// Standard error / error text.
    Error(String),
    /// Structured data chunk.
    Data(Value),
    /// Stream completed successfully.
    Done,
}

/// The result of a tool execution
#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    /// Whether the execution was successful
    pub success: bool,
    /// The output data
    pub output: String,
    /// Error message if failed
    pub error: Option<String>,
    /// Additional structured data
    pub data: Option<Value>,
    /// Execution time
    pub execution_time: Duration,
}

impl Serialize for ToolExecutionResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ToolExecutionResult", 5)?;
        state.serialize_field("success", &self.success)?;
        state.serialize_field("output", &self.output)?;
        state.serialize_field("error", &self.error)?;
        state.serialize_field("data", &self.data)?;
        state.serialize_field("execution_time", &self.execution_time.as_millis())?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ToolExecutionResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Helper {
            success: bool,
            output: String,
            error: Option<String>,
            data: Option<Value>,
            execution_time: u64,
        }

        let helper = Helper::deserialize(deserializer)?;
        Ok(ToolExecutionResult {
            success: helper.success,
            output: helper.output,
            error: helper.error,
            data: helper.data,
            execution_time: Duration::from_millis(helper.execution_time),
        })
    }
}

impl std::fmt::Display for ToolExecutionResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.output)
    }
}

impl ToolExecutionResult {
    /// Create a successful result
    pub fn success(output: impl Into<String>) -> Self {
        Self {
            success: true,
            output: output.into(),
            error: None,
            data: None,
            execution_time: Duration::default(),
        }
    }

    /// Create an error result
    pub fn error(error: impl Into<String>) -> Self {
        Self {
            success: false,
            output: String::new(),
            error: Some(error.into()),
            data: None,
            execution_time: Duration::default(),
        }
    }

    /// Add structured data
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// Set execution time
    pub fn with_execution_time(mut self, duration: Duration) -> Self {
        self.execution_time = duration;
        self
    }

    /// Returns `true` if this is a successful result with no output content.
    pub fn is_empty(&self) -> bool {
        self.success && self.output.is_empty() && self.error.is_none()
    }

    /// Returns `true` if this result represents a failure.
    pub fn is_error(&self) -> bool {
        !self.success
    }

    /// Convert to a ToolResult for LLM response
    pub fn to_tool_result(self, tool_call_id: impl Into<String>) -> ToolResult {
        let content = if self.success {
            self.output
        } else {
            format!("Error: {}", self.error.unwrap_or_else(|| "Unknown error".to_string()))
        };

        ToolResult {
            tool_call_id: tool_call_id.into(),
            role: crate::providers::Role::Tool,
            content,
            is_error: Some(!self.success),
        }
    }
}

/// Trait for tools that can be executed by the agent
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    /// Get the unique name of this tool
    fn name(&self) -> &str;

    /// Get a description of what this tool does
    fn description(&self) -> &str;

    /// Get the JSON schema for this tool's parameters
    fn parameters_schema(&self) -> Value;

    /// Execute the tool with the given arguments
    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult>;

    /// Execute the tool as a stream of incremental chunks.
    ///
    /// The default implementation calls [`execute`](Tool::execute) once and
    /// yields the resulting output/error/data as a single chunk sequence.
    /// Tools that produce incremental output (long-running shells, process
    /// runners, etc.) should override this to emit [`ToolExecutionChunk`]s
    /// as data becomes available.
    fn execute_stream<'a>(
        &'a self,
        args: Value,
        context: &'a ToolContext,
    ) -> Pin<Box<dyn tokio_stream::Stream<Item = ToolExecutionChunk> + Send + 'a>> {
        Box::pin(async_stream::stream! {
            match self.execute(args, context).await {
                Ok(result) => {
                    if !result.output.is_empty() {
                        yield ToolExecutionChunk::Output(result.output);
                    }
                    if let Some(error) = result.error {
                        yield ToolExecutionChunk::Error(error);
                    }
                    if let Some(data) = result.data {
                        yield ToolExecutionChunk::Data(data);
                    }
                }
                Err(e) => yield ToolExecutionChunk::Error(e.to_string()),
            }
        })
    }

    /// Check if this tool is available in the given context
    fn is_available(&self, _context: &ToolContext) -> bool {
        true
    }

    /// Whether the content filter should skip this tool's output.
    ///
    /// Override for tools whose output is not human-readable text
    /// (e.g. screenshot tools that return binary image data).
    fn skip_content_filter(&self) -> bool {
        false
    }

    /// Advertised capabilities for RBAC and SDK discovery.
    ///
    /// Defaults to a low-risk, uncategorized tool. Individual tools can
    /// override this to expose their real risk level and categories.
    fn capabilities(&self) -> crate::tools::sdk::ToolCapabilities {
        crate::tools::sdk::ToolCapabilities::default()
    }

    /// Get the timeout for this tool (defaults to context timeout)
    fn timeout(&self, context: &ToolContext) -> Duration {
        context.timeout()
    }

    /// Convert to a function definition for LLM providers
    fn to_function_definition(&self) -> FunctionDefinition {
        FunctionDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }
}

/// A boxed tool for storage
pub type BoxedTool = Box<dyn Tool>;
/// An atomically-reference-counted tool for shared storage
pub type SharedTool = Arc<dyn Tool>;
