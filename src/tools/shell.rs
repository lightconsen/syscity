//! Shell tool for executing commands
//!
//! This tool allows the AI to execute shell commands in a sandboxed
//! environment.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tracing::{debug, error, info, warn};

use super::{create_schema, head_tail_truncate, Tool, ToolContext, ToolExecutionResult};
use crate::tools::process_runner::{ProcessError, ProcessRequest, WriteFence};
use crate::tools::sdk::ToolCapabilities;

/// Shell tool for executing commands
#[derive(Debug)]
pub struct ShellTool {
    /// Default working directory
    default_cwd: Option<std::path::PathBuf>,
    /// Maximum output size in bytes
    max_output_size: usize,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self {
            default_cwd: None,
            // Hard memory-safety cap. Model-facing presentation of large
            // outputs is the registry's spill stage, so this can be generous.
            max_output_size: 256 * 1024, // 256 KB
        }
    }
}

impl ShellTool {
    /// Create a new shell tool
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the default working directory
    pub fn with_default_cwd(mut self, cwd: impl Into<std::path::PathBuf>) -> Self {
        self.default_cwd = Some(cwd.into());
        self
    }

    /// Set the maximum output size
    pub fn with_max_output_size(mut self, size: usize) -> Self {
        self.max_output_size = size;
        self
    }

    /// Truncate output if it exceeds the limit, keeping both the head and
    /// the tail — long-running command errors usually sit at the end.
    fn truncate_output(&self, output: String) -> String {
        head_tail_truncate(&output, self.max_output_size)
    }
}

/// Allowed shell interpreters. Any other value of `$SHELL` falls back to
/// `/bin/sh` to prevent a malicious or unexpected interpreter from being
/// invoked by the shell tool.
const ALLOWED_SHELLS: &[&str] = &[
    "/bin/sh",
    "/bin/bash",
    "/usr/bin/bash",
    "/bin/zsh",
    "/usr/bin/zsh",
];

/// Resolve the shell interpreter to use, validating against an allowlist.
pub(crate) fn resolve_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| ALLOWED_SHELLS.contains(&s.as_str()))
        .unwrap_or_else(|| "/bin/sh".to_string())
}

/// Returns `true` if `cmd` contains shell control operators that could be
/// used to chain additional commands beyond the first one.
///
/// Used to harden command-allowlist enforcement. Without this check a user
/// who allows `echo` could be hit by `echo hello && rm -rf /`.
fn contains_shell_control(cmd: &str) -> bool {
    // Strip the first "word" (the command name) — anything after is arguments
    // and shell operators.
    let rest = cmd
        .trim_start()
        .split_once(|c: char| c.is_whitespace())
        .map(|(_, rest)| rest)
        .unwrap_or("");
    if rest.is_empty() {
        return false;
    }

    // Check for shell control operators in the non-command portion.
    // We only check outside quotes to reduce false positives.
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = rest.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ if in_single || in_double => continue,
            _ => {}
        }
        if in_single || in_double {
            continue;
        }
        match ch {
            ';' | '|' | '&' | '`' => return true,
            '$' if chars.peek() == Some(&'(') => return true,
            _ => {}
        }
    }
    false
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command for file operations, running scripts, or system commands. \
         Commands are executed with safety restrictions. Note: For scheduling or recurring tasks, \
         use the 'cron' tool instead — do NOT use shell commands with 'at', 'cron', or 'schedule'. \
         A command that the workspace fence refuses comes back as a permission error; if it \
         genuinely needs to write outside the workspace (or otherwise leave the fence), say so in \
         the call with \"permissions\": {\"require_escalated\": true, \"justification\": \"why\"} \
         and a human is asked before it runs — do not try to work around the fence."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Execute a shell command",
            serde_json::json!({
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "working_dir": {
                    "type": "string",
                    "description": "Optional working directory for the command"
                }
            }),
            vec!["command"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let command_str = args["command"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("Missing 'command' argument".to_string())
        })?;

        // Check if command is allowed
        if !context.is_command_allowed(command_str) {
            return Ok(ToolExecutionResult::error(format!(
                "Command '{}' is not in the allowlist",
                command_str
            )));
        }

        // When an allowlist is active, reject shell control operators that
        // could chain additional commands beyond the allowed one.
        if !context.allowed_commands().is_empty() && contains_shell_control(command_str) {
            return Ok(ToolExecutionResult::error(format!(
                "Command '{}' contains shell control operators which are not permitted when \
                 command allowlisting is active",
                command_str
            )));
        }

        // Get working directory
        let working_dir = args["working_dir"]
            .as_str()
            .map(|p| context.resolve_path(std::path::Path::new(p)))
            .or_else(|| self.default_cwd.clone())
            .unwrap_or_else(|| context.workspace_root().clone());

        // Validate working directory
        if !context.is_path_allowed(&working_dir) {
            return Ok(ToolExecutionResult::error(format!(
                "Working directory '{}' is outside the workspace or not in the allowlist",
                working_dir.display()
            )));
        }

        info!("Executing shell command: {}", command_str);
        debug!("Working directory: {:?}", working_dir);

        // Parse command (handle shell operators like |, &&, etc.)
        let shell = resolve_shell();

        let start_time = std::time::Instant::now();

        // Build the request with resource limits if sandboxed
        let mut req = ProcessRequest {
            argv: vec![shell, "-c".to_string(), command_str.to_string()],
            cwd: Some(working_dir),
            env_clear: true,
            env: context.environment().clone(),
            timeout: Some(context.timeout()),
            fence: context.workspace_only().then(|| {
                WriteFence::new(
                    context.workspace_root().clone(),
                    context.allowed_paths().to_vec(),
                    context.fence_network(),
                    context.fence_namespaces(),
                )
            }),
            ..Default::default()
        };

        // Apply resource limits in sandboxed mode (Unix only)
        #[cfg(unix)]
        {
            if context.sandboxed() {
                let limits_summary = context.resource_limits_summary();
                debug!("Applying resource limits: {}", limits_summary);

                // Clone the limits to move into the closure
                let memory_limit = context.memory_limit();
                let cpu_limit = context.cpu_limit();
                let fd_limit = context.fd_limit();
                let process_limit = context.process_limit();

                // SAFETY: pre_exec runs in the child process after fork but before exec.
                // We only call async-signal-safe libc functions (setrlimit) here, which
                // is the documented safety requirement for pre_exec callbacks.
                let pre_exec: Arc<dyn Fn() -> std::io::Result<()> + Send + Sync> =
                    Arc::new(move || {
                        #[allow(unsafe_code)]
                        unsafe {
                            // Apply memory limit
                            if let Some(limit_bytes) = memory_limit {
                                let limit = libc::rlimit {
                                    rlim_cur: limit_bytes as libc::rlim_t,
                                    rlim_max: limit_bytes as libc::rlim_t,
                                };
                                if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                                    return Err(std::io::Error::last_os_error());
                                }
                            }

                            // Apply CPU limit
                            if let Some(limit_secs) = cpu_limit {
                                let limit = libc::rlimit {
                                    rlim_cur: limit_secs as libc::rlim_t,
                                    rlim_max: limit_secs as libc::rlim_t,
                                };
                                if libc::setrlimit(libc::RLIMIT_CPU, &limit) != 0 {
                                    return Err(std::io::Error::last_os_error());
                                }
                            }

                            // Apply FD limit
                            if let Some(limit_count) = fd_limit {
                                let limit = libc::rlimit {
                                    rlim_cur: limit_count as libc::rlim_t,
                                    rlim_max: limit_count as libc::rlim_t,
                                };
                                if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                                    return Err(std::io::Error::last_os_error());
                                }
                            }

                            // Apply process limit
                            if let Some(limit_count) = process_limit {
                                let limit = libc::rlimit {
                                    rlim_cur: limit_count as libc::rlim_t,
                                    rlim_max: limit_count as libc::rlim_t,
                                };
                                if libc::setrlimit(libc::RLIMIT_NPROC, &limit) != 0 {
                                    return Err(std::io::Error::last_os_error());
                                }
                            }

                            Ok(())
                        }
                    });
                req.pre_exec = Some(pre_exec);
            }
        }

        let mut escalated = false;
        let result = match crate::tools::process_runner::run_collect(&req).await {
            // A command the fence refused fails like any other command; ask a
            // human whether to run it again outside the fence. Declining (or
            // having nobody to ask) leaves the refusal exactly as it was.
            Ok(output) if !output.success() && !output.timed_out => {
                match crate::tools::escalation::fence_denial_reason(&output.stderr_string()) {
                    Some(reason) => {
                        match crate::tools::escalation::escalate_and_rerun(
                            context, "shell", &args, &req, reason,
                        )
                        .await
                        {
                            Some(retry) => {
                                escalated = true;
                                Ok(retry)
                            }
                            None => Ok(output),
                        }
                    }
                    None => Ok(output),
                }
            }
            other => other,
        };

        let duration = start_time.elapsed();

        match result {
            Ok(output) => {
                let stdout = output.stdout_string();
                let stderr = output.stderr_string();

                let mut combined_output = if stderr.is_empty() {
                    stdout
                } else {
                    format!("{}{}", stdout, stderr)
                };
                if escalated {
                    // Say so in the result the model reads: the command ran,
                    // but not under the rules the rest of the turn runs under.
                    combined_output = format!(
                        "(ran outside the workspace fence, approved by the operator)\n{combined_output}"
                    );
                }

                let total_bytes = combined_output.len();
                let truncated = self.truncate_output(combined_output);
                let data = serde_json::json!({
                    "exit_code": output.exit_code(),
                    "signal": output.signal,
                    "timed_out": output.timed_out,
                    "duration_ms": duration.as_millis() as u64,
                    "escalated": escalated,
                });

                if output.timed_out {
                    warn!("Command timed out after {:?}: {}", context.timeout(), command_str);
                    let message = if truncated.is_empty() {
                        format!("Command timed out after {:?}", context.timeout())
                    } else {
                        format!(
                            "Command timed out after {:?}. Partial output ({} bytes):\n{}",
                            context.timeout(),
                            total_bytes,
                            truncated
                        )
                    };
                    Ok(ToolExecutionResult::error(message)
                        .with_data(data)
                        .with_execution_time(duration))
                } else if output.success() {
                    info!("Command executed successfully in {:?}", duration);
                    Ok(ToolExecutionResult::success(truncated)
                        .with_data(data)
                        .with_execution_time(duration))
                } else if let Some(signal) = output.signal {
                    // Terminated by a signal (externally or by the command
                    // itself) — there is no exit code to report, so lead with
                    // the signal number instead.
                    warn!("Command terminated by signal {}: {}", signal, command_str);
                    Ok(ToolExecutionResult::error(format!(
                        "Terminated by signal {}: {}",
                        signal, truncated
                    ))
                    .with_data(data)
                    .with_execution_time(duration))
                } else {
                    let exit_code = output.exit_code().unwrap_or(-1);
                    warn!("Command failed with exit code {}: {}", exit_code, command_str);
                    Ok(ToolExecutionResult::error(format!(
                        "Exit code {}: {}",
                        exit_code, truncated
                    ))
                    .with_data(data)
                    .with_execution_time(duration))
                }
            }
            Err(e) => match e {
                ProcessError::Timeout { duration: timeout_dur } => {
                    error!("Command timed out after {:?}", timeout_dur);
                    Ok(ToolExecutionResult::error(format!(
                        "Command timed out after {:?}",
                        timeout_dur
                    )))
                }
                ProcessError::Spawn { source, .. } => {
                    error!("Failed to execute command: {}", source);
                    Ok(ToolExecutionResult::error(format!("Execution failed: {}", source)))
                }
                other => {
                    error!("Failed to execute command: {}", other);
                    Ok(ToolExecutionResult::error(format!("Execution failed: {}", other)))
                }
            },
        }
    }

    /// On iOS the sandbox forbids `fork`/`exec` entirely, so there is no
    /// shell (§3.2). Desktop and Android can run `/bin/sh` (toybox on
    /// Android), subject to the sandbox rules below.
    #[cfg(target_os = "ios")]
    fn is_available(&self, _context: &ToolContext) -> bool {
        false
    }

    #[cfg(not(target_os = "ios"))]
    fn is_available(&self, context: &ToolContext) -> bool {
        // Shell is available if we're not in strict sandbox mode
        // or if there are allowed commands specified
        !context.sandboxed() || !context.allowed_commands().is_empty()
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: crate::tools::approval::RiskLevel::High,
            categories: vec!["system".to_string(), "exec".to_string()],
            // A command run twice runs twice. Nothing to compensate with:
            // the tool cannot know what the command did.
            idempotent: false,
            compensation: None,
            ..ToolCapabilities::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn test_shell_tool_creation() {
        let tool = ShellTool::new();
        assert_eq!(tool.name(), "shell");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn test_truncate_output() {
        let tool = ShellTool::new().with_max_output_size(10);
        let output = "This is a very long string that definitely exceeds the limit".to_string();
        let truncated = tool.truncate_output(output.clone());
        // Degenerate budget: plain head truncation, prefix preserved.
        assert!(truncated.len() <= 10);
        assert!(truncated.starts_with("This is a "));
    }

    #[test]
    fn test_truncate_output_keeps_tail() {
        let tool = ShellTool::new().with_max_output_size(300);
        let output = "x".repeat(2000);
        let truncated = tool.truncate_output(output);
        assert!(truncated.contains("bytes omitted"));
        assert!(truncated.len() <= 320, "marker overhead bounded");
        assert!(truncated.starts_with('x') && truncated.ends_with('x'));
    }

    #[tokio::test]
    async fn test_shell_tool_execute() {
        let tool = ShellTool::new();
        let context = ToolContext::new("user", "conv1");

        let args = serde_json::json!({
            "command": "echo hello"
        });

        let result = tool.execute(args, &context).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("hello"));
    }

    #[tokio::test]
    async fn test_shell_tool_timeout() {
        let tool = ShellTool::new();
        // Use a 1s timeout to avoid flakiness on oversubscribed CI runners.
        let context = ToolContext::new("user", "conv1").with_timeout(Duration::from_secs(1));

        let args = serde_json::json!({
            "command": "sleep 100"
        });

        let start = Instant::now();
        let result = tool.execute(args, &context).await.unwrap();
        let elapsed = start.elapsed();

        assert!(!result.success, "timed-out shell command should not report success");
        assert!(
            result.error.as_ref().unwrap().contains("timed out"),
            "expected timeout error, got {:?}",
            result.error
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "shell command was not killed within timeout: {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_shell_tool_timeout_keeps_partial_output() {
        let tool = ShellTool::new();
        let context = ToolContext::new("user", "conv1").with_timeout(Duration::from_secs(1));

        let args = serde_json::json!({
            "command": "echo partial-out; sleep 100"
        });

        let start = Instant::now();
        let result = tool.execute(args, &context).await.unwrap();
        let elapsed = start.elapsed();

        assert!(!result.success);
        let error = result.error.as_ref().unwrap();
        assert!(error.contains("timed out"), "expected timeout, got {error:?}");
        assert!(
            error.contains("partial-out"),
            "partial output must survive the timeout, got {error:?}"
        );
        let data = result.data.expect("orthogonal fields present");
        assert_eq!(data["timed_out"], true);
        assert!(data["exit_code"].is_null());
        assert!(elapsed < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn test_shell_tool_result_data_fields() {
        let tool = ShellTool::new();
        let context = ToolContext::new("user", "conv1");

        let ok = tool
            .execute(serde_json::json!({"command": "echo hi"}), &context)
            .await
            .unwrap();
        let data = ok.data.expect("data present");
        assert_eq!(data["exit_code"], 0);
        assert_eq!(data["timed_out"], false);
        assert!(data["duration_ms"].is_number());

        let fail = tool
            .execute(serde_json::json!({"command": "exit 3"}), &context)
            .await
            .unwrap();
        assert!(!fail.success);
        assert_eq!(fail.data.as_ref().unwrap()["exit_code"], 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_shell_tool_reports_terminating_signal() {
        let tool = ShellTool::new();
        let context = ToolContext::new("user", "conv1");

        let result = tool
            .execute(serde_json::json!({"command": "kill -TERM $$"}), &context)
            .await
            .unwrap();

        assert!(!result.success, "signaled command must not report success");
        let error = result.error.as_ref().unwrap();
        assert!(
            error.contains(&format!("Terminated by signal {}", libc::SIGTERM)),
            "expected signal in error message, got {error:?}"
        );
        let data = result.data.expect("orthogonal fields present");
        assert_eq!(data["signal"], libc::SIGTERM);
        // A signaled process has no exit code and is not a timeout.
        assert!(data["exit_code"].is_null());
        assert_eq!(data["timed_out"], false);
    }

    #[test]
    fn test_truncate_output_utf8_safe() {
        // Test that truncation does not panic on multi-byte UTF-8 characters.
        // The smiley emoji is 4 bytes. If max_output_size = 10 and the string
        // contains multi-byte chars near the boundary, the old code would panic.
        let tool = ShellTool::new().with_max_output_size(10);
        let output = "Hello 😀 world".to_string(); // 😀 is 4 bytes
        let truncated = tool.truncate_output(output.clone());
        // The result must be valid UTF-8 (no split multi-byte char)
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn test_truncate_output_exact_boundary() {
        let tool = ShellTool::new().with_max_output_size(4);
        let output = "😀😀😀".to_string(); // 12 bytes, each 😀 is 4 bytes
        let truncated = tool.truncate_output(output.clone());
        // With max 4 bytes, should keep exactly one emoji
        assert_eq!(truncated, "😀");
    }

    #[test]
    fn test_truncate_output_no_truncation_needed() {
        let tool = ShellTool::new().with_max_output_size(1000);
        let output = "Short".to_string();
        let truncated = tool.truncate_output(output.clone());
        assert_eq!(truncated, output);
    }

    #[tokio::test]
    async fn test_shell_tool_command_not_allowed() {
        let tool = ShellTool::new();
        let context = ToolContext::new("user", "conv1").allow_command("ls");

        let args = serde_json::json!({
            "command": "rm -rf /"
        });

        let result = tool.execute(args, &context).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_ref()
            .unwrap()
            .contains("not in the allowlist"));
    }

    #[tokio::test]
    async fn test_shell_tool_command_injection_blocked() {
        let tool = ShellTool::new();
        let context = ToolContext::new("user", "conv1").allow_command("ls");

        // Command chaining should be blocked when only 'ls' is allowed
        let args = serde_json::json!({
            "command": "ls; rm -rf /"
        });

        let result = tool.execute(args, &context).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_ref()
            .unwrap()
            .contains("not in the allowlist"));
    }

    #[tokio::test]
    async fn test_shell_tool_allowed_command_executes() {
        let tool = ShellTool::new();
        let context = ToolContext::new("user", "conv1").allow_command("echo");

        let args = serde_json::json!({
            "command": "echo hello"
        });

        let result = tool.execute(args, &context).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("hello"));
    }

    #[tokio::test]
    async fn test_shell_tool_and_operator_blocked_with_allowlist() {
        let tool = ShellTool::new();
        let context = ToolContext::new("user", "conv1").allow_command("echo");

        // `&&` should be blocked when allowlist is active, even though the
        // first token ("echo") is allowed — the operator chains additional
        // commands beyond the allowed one.
        let args = serde_json::json!({
            "command": "echo hello && rm -rf /"
        });

        let result = tool.execute(args, &context).await.unwrap();
        assert!(!result.success, "chained && should be blocked");
        assert!(result
            .error
            .as_ref()
            .unwrap()
            .contains("shell control operators"));
    }

    #[tokio::test]
    async fn test_shell_tool_or_operator_blocked_with_allowlist() {
        let tool = ShellTool::new();
        let context = ToolContext::new("user", "conv1").allow_command("echo");

        let args = serde_json::json!({
            "command": "echo hello || rm -rf /"
        });

        let result = tool.execute(args, &context).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_ref()
            .unwrap()
            .contains("shell control operators"));
    }

    #[test]
    fn test_contains_shell_control_semicolon() {
        assert!(contains_shell_control("echo a; echo b"));
    }

    #[test]
    fn test_contains_shell_control_and_operator() {
        assert!(contains_shell_control("echo a && echo b"));
    }

    #[test]
    fn test_contains_shell_control_pipe() {
        assert!(contains_shell_control("cat foo | grep bar"));
    }

    #[test]
    fn test_contains_shell_control_subshell() {
        assert!(contains_shell_control("echo $(whoami)"));
    }

    #[test]
    fn test_contains_shell_control_backtick() {
        assert!(contains_shell_control("echo `whoami`"));
    }

    #[test]
    fn test_contains_shell_control_simple_command() {
        assert!(!contains_shell_control("echo hello world"));
    }

    #[test]
    fn test_contains_shell_control_semicolon_in_quotes() {
        // A semicolon inside a quoted string is not a control operator.
        assert!(!contains_shell_control("echo 'hello; world'"));
    }

    #[test]
    fn test_contains_shell_control_no_args() {
        assert!(!contains_shell_control("ls"));
    }

    #[test]
    fn test_contains_shell_control_ampersand_in_url() {
        // `&` inside double-quotes should not trigger.
        assert!(!contains_shell_control("echo \"foo & bar\""));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn test_shell_fence_blocks_outside_writes_when_workspace_only() {
        // Skip when the Seatbelt binary is absent (the runner then hard-fails
        // and the "fenced" half of this test would pass vacuously).
        if !std::path::Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        let tool = ShellTool::new();
        let target = format!("/tmp/syscity_shell_fence_{}", std::process::id());
        let _ = std::fs::remove_file(&target);

        // An explicit, existing workspace root: under `cfg(test)` the dirs
        // default is a temp root nothing creates, and the shell's cwd is the
        // workspace root — a missing cwd fails the spawn for reasons that have
        // nothing to do with the fence.
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().to_path_buf();

        // workspace_only defaults to true -> shell runs behind the kernel
        // write fence, so a /tmp write is denied even though the shell tool's
        // parent-process path check only validates the working directory.
        let fenced = ToolContext::new("user", "conv1").with_workspace_root(ws.clone());
        let result = tool
            .execute(serde_json::json!({ "command": format!("echo x > {target}") }), &fenced)
            .await
            .unwrap();
        assert!(!result.success, "fenced shell must not write outside workspace");
        assert!(!std::path::Path::new(&target).exists());

        // workspace_only=false -> no fence, the write goes through.
        let relaxed = ToolContext::new("user", "conv1")
            .with_workspace_root(ws)
            .with_workspace_only(false);
        let result = tool
            .execute(serde_json::json!({ "command": format!("echo x > {target}") }), &relaxed)
            .await
            .unwrap();
        assert!(result.success, "non-fenced shell should write freely");
        assert!(std::path::Path::new(&target).exists());
        let _ = std::fs::remove_file(&target);
    }

    /// A refusal by the fence can be escalated: the operator is asked, and on
    /// approval the command runs again without the fence.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_fence_refusal_can_be_escalated_to_an_unfenced_rerun() {
        if !std::path::Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let target = format!("/tmp/syscity_escalation_{}", std::process::id());
        let _ = std::fs::remove_file(&target);

        let queue = Arc::new(crate::tools::approval::ApprovalQueue::new());
        let mut events = queue.event_tx.subscribe();
        let approver_queue = Arc::clone(&queue);
        let approver = tokio::spawn(async move {
            let event = events.recv().await.expect("an escalation request");
            assert!(
                event.message.contains("outside the workspace fence"),
                "the prompt must name what is being asked: {}",
                event.message
            );
            approver_queue
                .resolve(&event.approval_id, crate::tools::approval::ApprovalDecision::Approve)
                .await;
        });

        let ctx = ToolContext::new("user", "conv1")
            .with_workspace_root(dir.path().to_path_buf())
            .with_ask_queue(Arc::new(crate::tools::ask_user::AskQueue::new()))
            .with_approval_queue(Arc::clone(&queue));

        let tool = ShellTool::new();
        let result = tool
            .execute(serde_json::json!({ "command": format!("echo x > {target}") }), &ctx)
            .await
            .unwrap();
        approver.await.expect("approver");

        assert!(result.success, "the approved re-run must succeed: {result:?}");
        assert!(
            std::path::Path::new(&target).exists(),
            "the unfenced re-run writes where the fence refused"
        );
        assert!(
            result.output.contains("outside the workspace fence"),
            "the result must say the run left the fence: {}",
            result.output
        );
        assert_eq!(result.data.as_ref().expect("data")["escalated"], true);
        let _ = std::fs::remove_file(&target);
    }

    /// Declining leaves the refusal exactly as it was.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_declined_escalation_keeps_the_refusal() {
        if !std::path::Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let target = format!("/tmp/syscity_escalation_denied_{}", std::process::id());
        let _ = std::fs::remove_file(&target);

        let queue = Arc::new(crate::tools::approval::ApprovalQueue::new());
        let mut events = queue.event_tx.subscribe();
        let denier_queue = Arc::clone(&queue);
        let denier = tokio::spawn(async move {
            let event = events.recv().await.expect("an escalation request");
            denier_queue
                .resolve(
                    &event.approval_id,
                    crate::tools::approval::ApprovalDecision::Deny {
                        reason: "not this time".into(),
                    },
                )
                .await;
        });

        let ctx = ToolContext::new("user", "conv1")
            .with_workspace_root(dir.path().to_path_buf())
            .with_ask_queue(Arc::new(crate::tools::ask_user::AskQueue::new()))
            .with_approval_queue(Arc::clone(&queue));

        let tool = ShellTool::new();
        let result = tool
            .execute(serde_json::json!({ "command": format!("echo x > {target}") }), &ctx)
            .await
            .unwrap();
        denier.await.expect("denier");

        assert!(!result.success, "the refusal stands: {result:?}");
        assert!(!std::path::Path::new(&target).exists());
        assert!(
            !result.output.contains("outside the workspace fence"),
            "a declined escalation must not claim to have run unfenced"
        );
    }

    /// A context with nobody to ask (no ask queue) is never prompted: cron,
    /// goals and delegated children keep the refusal and get no approval.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_context_with_no_human_keeps_the_refusal_silently() {
        if !std::path::Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let target = format!("/tmp/syscity_escalation_silent_{}", std::process::id());
        let _ = std::fs::remove_file(&target);

        let queue = Arc::new(crate::tools::approval::ApprovalQueue::new());
        let mut events = queue.event_tx.subscribe();

        // An approval queue but no ask queue: `can_ask_a_human` is false.
        let ctx = ToolContext::new("system", "cron:job")
            .with_workspace_root(dir.path().to_path_buf())
            .with_approval_queue(Arc::clone(&queue));

        let tool = ShellTool::new();
        let result = tool
            .execute(serde_json::json!({ "command": format!("echo x > {target}") }), &ctx)
            .await
            .unwrap();

        assert!(!result.success, "the refusal stands: {result:?}");
        assert!(!std::path::Path::new(&target).exists());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), events.recv())
                .await
                .is_err(),
            "no approval may be submitted when there is nobody to answer it"
        );
    }

    /// The network posture travels with the context into the fence: the same
    /// command reaches a loopback listener by default and is refused with
    /// `fence_network` on.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn test_shell_fence_denies_network_only_when_asked() {
        if !std::path::Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        // The listener is what makes the control half meaningful — without it
        // both runs fail to connect and the test would pass for the wrong
        // reason.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        // `nc` rather than bash's `/dev/tcp`: the shell tool runs `$SHELL`,
        // and zsh has no `/dev/tcp`. Under the network deny `nc` fails
        // silently (exit 1, no stderr), so the assertion is on success.
        let command = format!("nc -z 127.0.0.1 {port}");
        let tool = ShellTool::new();
        // Same explicit root as the write test above: the shell needs a cwd
        // that exists, and the fence needs a root to grant.
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().to_path_buf();

        let permissive = ToolContext::new("user", "conv1").with_workspace_root(ws.clone());
        let result = tool
            .execute(serde_json::json!({ "command": command }), &permissive)
            .await
            .unwrap();
        assert!(result.success, "the port is reachable by default: {result:?}");

        let denied = ToolContext::new("user", "conv1")
            .with_workspace_root(ws)
            .with_fence_network(true);
        let result = tool
            .execute(serde_json::json!({ "command": command }), &denied)
            .await
            .unwrap();
        assert!(!result.success, "fence_network must refuse the connect: {result:?}");
    }
}
