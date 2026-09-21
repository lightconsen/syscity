//! Programmatic Tool Calling (PTC) - Code Execution Tool
//!
//! This tool allows the agent to write and execute Python scripts that can call
//! other tools programmatically via RPC. This enables self-orchestration and
//! collapses multi-step chains into single inference turns.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::tools::process_runner::{ProcessError, ProcessRequest, StdioMode, WriteFence};
use crate::tools::sdk::ToolCapabilities;

/// Code execution sandbox configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Maximum execution time in seconds
    pub timeout_secs: u64,
    /// Maximum stdout/stderr size in bytes
    pub max_output_size: usize,
    /// Allowed Python imports (empty = all allowed)
    pub allowed_imports: Vec<String>,
    /// Forbidden Python imports
    pub forbidden_imports: Vec<String>,
    /// Enable network access
    pub allow_network: bool,
    /// Maximum memory usage in MB
    pub max_memory_mb: usize,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 300,       // 5 minutes
            max_output_size: 50_000, // 50KB
            allowed_imports: vec![],
            forbidden_imports: vec![
                "os.system".to_string(),
                "subprocess".to_string(),
                "socket".to_string(),
                "ctypes".to_string(),
            ],
            allow_network: false,
            max_memory_mb: 256,
        }
    }
}

/// Code execution tool
#[derive(Debug)]
pub struct CodeExecutionTool {
    config: SandboxConfig,
}

impl CodeExecutionTool {
    /// Create a new code execution tool with default config
    pub fn new() -> Self {
        Self {
            config: SandboxConfig::default(),
        }
    }

    /// Create with custom sandbox config
    pub fn with_config(config: SandboxConfig) -> Self {
        Self { config }
    }

    /// Validate Python code for forbidden patterns
    fn validate_code(&self, code: &str) -> Result<(), Vec<String>> {
        let mut violations = Vec::new();

        // Check for forbidden imports
        for forbidden in &self.config.forbidden_imports {
            let pattern = format!(
                r"(?i)(import\s+{}|from\s+{}\s+import)",
                regex::escape(forbidden),
                regex::escape(forbidden)
            );
            if let Ok(re) = regex::Regex::new(&pattern) {
                if re.is_match(code) {
                    violations.push(format!("Forbidden import: {}", forbidden));
                }
            }
        }

        // Check for exec/eval with dangerous patterns
        let dangerous_patterns = [
            (r"(?i)exec\s*\(", "exec() is not allowed"),
            (r"(?i)eval\s*\(", "eval() is not allowed"),
            (r"(?i)__import__", "__import__ is not allowed"),
            (r"(?i)compile\s*\(", "compile() is not allowed"),
        ];

        for (pattern, message) in &dangerous_patterns {
            if let Ok(re) = regex::Regex::new(pattern) {
                if re.is_match(code) {
                    violations.push(message.to_string());
                }
            }
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }

    /// Execute Python code in sandbox
    async fn execute_python(
        &self,
        code: &str,
        context: &ToolContext,
        timeout_secs: u64,
    ) -> crate::Result<CodeResult> {
        // Create wrapped code with output capture
        let max_size = self.config.max_output_size;
        let header = format!(
            r#"# -*- coding: utf-8 -*-
import sys
import json
import traceback

# Limit output size
class LimitedOutput:
    def __init__(self, original, limit):
        self.original = original
        self.limit = limit
        self.written = 0

    def write(self, data):
        if self.written < self.limit:
            to_write = data[:self.limit - self.written]
            self.original.write(to_write)
            self.written += len(to_write)
        return len(data)

    def flush(self):
        self.original.flush()

sys.stdout = LimitedOutput(sys.stdout, {})
sys.stderr = LimitedOutput(sys.stderr, {})

# Execute user code
result = {{}}
try:
    exec_globals = {{}}
    exec_locals = {{}}
"#,
            max_size, max_size
        );

        let footer = r#"
    result['success'] = True
    result['globals'] = {k: str(v) for k, v in exec_locals.items() if not k.startswith('_')}
except Exception as e:
    result['success'] = False
    result['error'] = str(e)
    result['traceback'] = traceback.format_exc()

# Output result as JSON
print("\n__PTC_RESULT__")
print(json.dumps(result))
"#;

        let code_escaped =
            format!("    exec(compile({:?}, '<string>', 'exec'), exec_globals, exec_locals)", code);
        let wrapped_code = format!("{}{}{}", header, code_escaped, footer);

        // Spawn Python process via the platform process runner
        let mut req = ProcessRequest {
            argv: vec!["python3".to_string(), "-c".to_string(), wrapped_code],
            stdio: StdioMode::Piped,
            fence: context.workspace_only().then(|| {
                WriteFence::new(
                    context.workspace_root().clone(),
                    context.allowed_paths().to_vec(),
                    context.fence_network(),
                )
            }),
            ..Default::default()
        };

        // Apply resource limits via pre_exec (Unix only)
        #[cfg(unix)]
        {
            let max_memory_mb = self.config.max_memory_mb;

            // SAFETY: pre_exec runs in the child process after fork but before exec.
            // We only call async-signal-safe libc functions (setrlimit) here, which
            // is the documented safety requirement for pre_exec callbacks.
            let pre_exec: Arc<dyn Fn() -> std::io::Result<()> + Send + Sync> =
                Arc::new(move || {
                    #[allow(unsafe_code)]
                    unsafe {
                        // Memory limit (RLIMIT_AS)
                        let mem_bytes = max_memory_mb * 1024 * 1024;
                        let limit = libc::rlimit {
                            rlim_cur: mem_bytes as libc::rlim_t,
                            rlim_max: mem_bytes as libc::rlim_t,
                        };
                        let _ = libc::setrlimit(libc::RLIMIT_AS, &limit);

                        // CPU limit
                        let limit = libc::rlimit {
                            rlim_cur: timeout_secs as libc::rlim_t,
                            rlim_max: timeout_secs as libc::rlim_t,
                        };
                        let _ = libc::setrlimit(libc::RLIMIT_CPU, &limit);

                        // File descriptor limit
                        let limit = libc::rlimit { rlim_cur: 256, rlim_max: 256 };
                        let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &limit);

                        // Process limit (prevent fork bombs).
                        // Skip on macOS: RLIMIT_NPROC is per-user and interferes with
                        // pyenv and other multi-process toolchains.
                        #[cfg(target_os = "linux")]
                        {
                            let limit = libc::rlimit { rlim_cur: 64, rlim_max: 64 };
                            let _ = libc::setrlimit(libc::RLIMIT_NPROC, &limit);
                        }

                        Ok(())
                    }
                });
            req.pre_exec = Some(pre_exec);
        }

        let mut child = crate::tools::process_runner::spawn(&req)
            .await
            .map_err(|e| {
                let msg = match e {
                    ProcessError::Spawn { source, .. } => {
                        format!("Failed to spawn Python: {}", source)
                    }
                    other => format!("Failed to spawn Python: {}", other),
                };
                crate::error::SyscityError::Internal(msg)
            })?;

        // Wait for execution with timeout
        let timeout_duration = Duration::from_secs(timeout_secs);
        let result = timeout(timeout_duration, async {
            let stdout = child.take_stdout().ok_or_else(|| {
                crate::error::SyscityError::Internal("stdout pipe missing".into())
            })?;
            let stderr = child.take_stderr().ok_or_else(|| {
                crate::error::SyscityError::Internal("stderr pipe missing".into())
            })?;

            let mut stdout_reader = tokio::io::BufReader::new(stdout);
            let mut stderr_reader = tokio::io::BufReader::new(stderr);

            let mut stdout_buf = Vec::new();
            let mut stderr_buf = Vec::new();

            // Read stdout and stderr concurrently
            let (stdout_res, stderr_res) = tokio::join!(
                stdout_reader.read_to_end(&mut stdout_buf),
                stderr_reader.read_to_end(&mut stderr_buf)
            );

            if let Err(e) = stdout_res {
                return Err(crate::error::SyscityError::Internal(format!(
                    "Failed to read stdout: {}",
                    e
                )));
            }
            if let Err(e) = stderr_res {
                return Err(crate::error::SyscityError::Internal(format!(
                    "Failed to read stderr: {}",
                    e
                )));
            }

            // Wait for process to complete
            let status = child.wait().await.map_err(|e| {
                crate::error::SyscityError::Internal(format!("Failed to wait for process: {}", e))
            })?;

            let stdout_str = String::from_utf8_lossy(&stdout_buf).to_string();
            let stderr_str = String::from_utf8_lossy(&stderr_buf).to_string();

            let ptc_result = ptc_result_from(&stdout_str, status.success());

            Ok(CodeResult {
                stdout: stdout_str,
                stderr: stderr_str,
                exit_code: status.code().unwrap_or(-1),
                result: ptc_result,
            })
        })
        .await;

        match result {
            // A fenced command the fence refused fails like any other failure;
            // ask a human whether to run it again outside the fence, exactly
            // as `shell` does. Declining leaves the original result alone.
            Ok(Ok(result)) if result.exit_code != 0 => {
                let Some(reason) = crate::tools::escalation::fence_denial_reason(&result.stderr)
                else {
                    return Ok(result);
                };
                let args = json!({ "code": code });
                match crate::tools::escalation::escalate_and_rerun(
                    context,
                    "execute_code",
                    &args,
                    &req,
                    reason,
                )
                .await
                {
                    Some(retry) => {
                        let stdout = retry.stdout_string();
                        let stderr = retry.stderr_string();
                        let exit_code = retry.exit_code().unwrap_or(-1);
                        Ok(CodeResult {
                            result: ptc_result_from(&stdout, retry.success()),
                            // The model reads this: say the run happened, but
                            // not under the rules the rest of the turn uses.
                            stdout: format!(
                                "(ran outside the workspace fence, approved by the operator)\n{stdout}"
                            ),
                            stderr,
                            exit_code,
                        })
                    }
                    None => Ok(result),
                }
            }
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // Timeout - kill the process
                if let Err(e) = child.kill().await {
                    warn!("Failed to kill timed-out code execution process: {}", e);
                }
                Err(crate::error::SyscityError::Internal(format!(
                    "Code execution timed out after {} seconds",
                    timeout_secs
                )))
            }
        }
    }

    /// Execute a wasm32-wasi command module inside a sandboxed wasmtime
    /// instance (§4.5).
    ///
    /// `code` carries base64-encoded module bytes (or WAT text — wasmtime
    /// parses both). The guest must export `_start`. Guest stdout/stderr are
    /// captured into bounded in-memory pipes and returned in the result.
    ///
    /// Execution is bounded by wasmtime fuel metering scaled to the requested
    /// timeout; exhausting fuel traps the guest as a timeout (fuel is the
    /// primary bound — every wasm instruction burns fuel, so a runaway guest
    /// is always interrupted). The guest gets stdin/stdout/stderr only: no
    /// filesystem, network, or env preopens.
    #[cfg(feature = "plugins")]
    async fn execute_wasm(&self, code: &str, timeout_secs: u64) -> crate::Result<CodeResult> {
        use base64::Engine;

        let wasm_bytes = base64::engine::general_purpose::STANDARD
            .decode(code.trim())
            .map_err(|e| {
                crate::error::SyscityError::Validation(format!(
                    "language=wasm expects base64-encoded wasm32-wasi module bytes: {}",
                    e
                ))
            })?;
        if wasm_bytes.len() > 2 * 1024 * 1024 {
            return Err(crate::error::SyscityError::Validation(
                "WASM module exceeds the 2 MiB size limit".to_string(),
            ));
        }

        // Capture guest output into bounded in-memory pipes instead of
        // inheriting stdio, so results return to the tool caller. Writes past
        // the pipe capacity trap the guest, enforcing the output budget.
        let stdout_pipe =
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(self.config.max_output_size);
        let stderr_pipe =
            wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(self.config.max_output_size);

        // Compile on the calling thread — bounded by the 2 MiB size cap
        // above. wasmtime parses both the wasm binary and WAT text formats.
        // Fuel metering bounds execution time; the stack cap bounds recursion.
        // No strategy override — the default engine (Cranelift JIT on desktop,
        // Pulley interpreter on iOS) is chosen by the platform.
        let mut config = wasmtime::Config::default();
        config.consume_fuel(true);
        config.max_wasm_stack(512 * 1024);
        let engine = wasmtime::Engine::new(&config).map_err(|e| {
            crate::error::SyscityError::Internal(format!("WASM engine init failed: {}", e))
        })?;
        let module = wasmtime::Module::new(&engine, &wasm_bytes).map_err(|e| {
            crate::error::SyscityError::Validation(format!("Invalid WASM module: {}", e))
        })?;

        // wasmtime's WASI preview1 shim drives async host streams through
        // `in_tokio`, which `block_on`s the ambient runtime handle. From
        // inside a tokio task that `block_on` panics ("cannot start a runtime
        // from within a runtime"), so the (blocking) wasm call runs on a
        // blocking thread where there is no ambient runtime. Fuel metering is
        // the hard execution bound; the tokio timeout below is a caller-side
        // upper bound.
        let stdout_writer = stdout_pipe.clone();
        let stderr_writer = stderr_pipe.clone();
        let call_handle = tokio::task::spawn_blocking(move || {
            let wasi_ctx = wasmtime_wasi::WasiCtxBuilder::new()
                .stdout(stdout_writer)
                .stderr(stderr_writer)
                .build_p1();
            let mut store = wasmtime::Store::new(&engine, wasi_ctx);
            // ~1e8 instructions per second is a conservative fuel burn rate.
            store.set_fuel(timeout_secs.saturating_mul(100_000_000))?;
            let mut linker = wasmtime::Linker::new(&engine);
            wasmtime_wasi::p1::add_to_linker_sync(
                &mut linker,
                |ctx: &mut wasmtime_wasi::p1::WasiP1Ctx| ctx,
            )?;
            let instance = linker.instantiate(&mut store, &module)?;
            // The error for a missing export names `_start`, which the caller
            // surfaces in the result.
            let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
            start.call(&mut store, ())
        });

        // `timeout` awaits the JoinHandle itself, so the value here is
        // `Result<Result<(), wasmtime::Error>, JoinError>`.
        let join_result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs.saturating_add(5)),
            call_handle,
        )
        .await
        .map_err(|_| {
            crate::error::SyscityError::Internal(format!(
                "Code execution timed out after {} seconds",
                timeout_secs
            ))
        })?;

        let call_result = join_result.map_err(|e| {
            crate::error::SyscityError::Internal(format!("WASM execution task failed: {}", e))
        })?;

        let mut stderr_str = match call_result {
            Ok(()) => String::new(),
            Err(e) => {
                if e.downcast_ref::<wasmtime::Trap>()
                    .is_some_and(|t| matches!(t, wasmtime::Trap::OutOfFuel))
                {
                    return Err(crate::error::SyscityError::Internal(format!(
                        "Code execution timed out after {} seconds",
                        timeout_secs
                    )));
                }
                format!("WASM execution failed: {}", e)
            }
        };
        let exit_code = if stderr_str.is_empty() { 0 } else { -1 };

        let stdout_str = String::from_utf8_lossy(stdout_pipe.contents().as_ref()).to_string();
        if stderr_str.is_empty() {
            stderr_str = String::from_utf8_lossy(stderr_pipe.contents().as_ref()).to_string();
        }

        // Parse a trailing `__PTC_RESULT__` JSON marker if the guest emitted
        // one (same contract as the Python path).
        let ptc_result = if let Some(idx) = stdout_str.find("__PTC_RESULT__") {
            let json_part = &stdout_str[idx + "__PTC_RESULT__".len()..];
            serde_json::from_str(json_part.trim())
                .unwrap_or_else(|_| json!({"success": exit_code == 0}))
        } else {
            json!({"success": exit_code == 0})
        };

        Ok(CodeResult {
            stdout: stdout_str,
            stderr: stderr_str,
            exit_code,
            result: ptc_result,
        })
    }
}

/// Result of code execution
/// Parse the `__PTC_RESULT__` trailer the Python wrapper prints, if present.
///
/// Absent or unparseable, the answer is `{"success": <process succeeded>}`:
/// the caller asked for a result object and gets one that says nothing more
/// than the exit code did.
fn ptc_result_from(stdout: &str, success: bool) -> serde_json::Value {
    match stdout.find("__PTC_RESULT__") {
        Some(idx) => serde_json::from_str(stdout[idx + "__PTC_RESULT__".len()..].trim())
            .unwrap_or_else(|_| json!({ "success": success, "error": null })),
        None => json!({ "success": success, "error": null }),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeResult {
    /// Standard output
    pub stdout: String,
    /// Standard error
    pub stderr: String,
    /// Exit code
    pub exit_code: i32,
    /// Structured result
    pub result: serde_json::Value,
}

#[async_trait]
impl Tool for CodeExecutionTool {
    fn name(&self) -> &str {
        "execute_code"
    }

    fn description(&self) -> &str {
        r#"Execute Python code in a sandboxed environment.
On mobile, `language=wasm` additionally runs base64-encoded wasm32-wasi modules
in an in-process sandbox (the desktop subprocess interpreter is unavailable).

Use this tool to:
- Perform calculations or data processing
- Transform data formats
- Generate code snippets
- Test algorithms
- Process text or structured data

The code runs in a restricted environment with:
- 5-minute execution timeout
- 50KB output limit
- No network access
- Restricted imports (no subprocess, os.system, etc.)
- Memory limits (256MB)

The code output is returned as stdout. For structured results,
you can print JSON at the end of your script.

If the code needs to write outside the workspace, the fence refuses it and
the run fails with a permission error; say so in the call with
"permissions": {"require_escalated": true, "justification": "why"} and a
human is asked before it runs.

Example:
```python
# Process data
data = [1, 2, 3, 4, 5]
result = sum(data) / len(data)
print(f"Average: {result}")

# You can also return structured data
import json
print("__PTC_RESULT__")
print(json.dumps({"average": result, "count": len(data)}))
```"#
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let mut languages = vec!["python".to_string()];
        // Mobile exposes the in-process WASM engine (§4.5); desktop keeps the
        // subprocess Python interpreter.
        #[cfg(feature = "plugins")]
        languages.push("wasm".to_string());

        json!({
            "type": "object",
            "properties": {
                "language": {
                    "type": "string",
                    "enum": languages,
                    "description": "Programming language",
                    "default": "python"
                },
                "code": {
                    "type": "string",
                    "description": "Python source to execute, or (language=wasm) base64-encoded wasm32-wasi module bytes"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Custom timeout in seconds (max 300)",
                    "minimum": 1,
                    "maximum": 300
                }
            },
            "required": ["code"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let code = args["code"].as_str().ok_or_else(|| {
            crate::error::SyscityError::Validation("code parameter is required".to_string())
        })?;

        let language = args["language"].as_str().unwrap_or("python");

        // Python source is statically screened for forbidden imports/patterns
        // before it is handed to the interpreter. WASM input is base64 text the
        // Python checks do not apply to — the wasmtime sandbox is its security
        // boundary.
        if language == "python" {
            match self.validate_code(code) {
                Ok(()) => {}
                Err(violations) => {
                    return Ok(ToolExecutionResult::error(format!(
                        "Code validation failed:\n{}",
                        violations.join("\n")
                    )));
                }
            }
        }

        let timeout_secs = args["timeout"].as_u64().unwrap_or(self.config.timeout_secs);

        info!("Executing {} code ({} bytes)", language, code.len());
        debug!("Code: {}", code.chars().take(200).collect::<String>());

        // Execute the code
        let result = match language {
            "python" => self.execute_python(code, context, timeout_secs).await,
            "wasm" => {
                #[cfg(feature = "plugins")]
                {
                    self.execute_wasm(code, timeout_secs).await
                }
                #[cfg(not(feature = "plugins"))]
                {
                    Err(crate::error::SyscityError::Validation(
                        "`wasm` execution requires the `plugins` feature".to_string(),
                    ))
                }
            }
            other => {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Unsupported language: {}",
                    other
                )));
            }
        };

        match result {
            Ok(result) => {
                let success =
                    result.exit_code == 0 && result.result["success"].as_bool().unwrap_or(true);

                let mut output = format!(
                    "Exit code: {}\n\n## stdout\n{}\n\n## stderr\n{}",
                    result.exit_code, result.stdout, result.stderr
                );

                // Truncate if too long
                if output.len() > self.config.max_output_size {
                    output = format!(
                        "{}\n\n[Output truncated - exceeded {} bytes]",
                        &output[..self.config.max_output_size],
                        self.config.max_output_size
                    );
                }

                if success {
                    Ok(ToolExecutionResult::success(output).with_data(result.result))
                } else {
                    Ok(ToolExecutionResult::error(output).with_data(result.result))
                }
            }
            Err(e) => {
                error!("Code execution failed: {}", e);
                Ok(ToolExecutionResult::error(format!("Execution failed: {}", e)))
            }
        }
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: crate::tools::approval::RiskLevel::High,
            categories: vec!["system".to_string(), "exec".to_string()],
            idempotent: false,
            compensation: None,
            ..ToolCapabilities::default()
        }
    }

    fn is_available(&self, _context: &ToolContext) -> bool {
        if cfg!(mobile_os) {
            // Mobile has no subprocess interpreters — availability comes from
            // the in-process wasmtime engine (§4.5), which the `plugins`
            // feature pulls in (the mobile profile enables it).
            cfg!(feature = "plugins")
        } else {
            // Desktop: subprocess Python interpreter.
            true
        }
    }
}

impl Default for CodeExecutionTool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn test_code_validation() {
        let tool = CodeExecutionTool::new();

        // Valid code
        let valid = "x = 1 + 2\nprint(x)";
        assert!(tool.validate_code(valid).is_ok());

        // Forbidden import
        let invalid = "import subprocess\nsubprocess.run(['ls'])";
        assert!(tool.validate_code(invalid).is_err());

        // Forbidden eval
        let invalid = "eval('1 + 1')";
        assert!(tool.validate_code(invalid).is_err());
    }

    #[test]
    fn test_sandbox_config_default() {
        let config = SandboxConfig::default();
        assert_eq!(config.timeout_secs, 300);
        assert_eq!(config.max_output_size, 50_000);
        assert!(!config.allow_network);
        assert_eq!(config.max_memory_mb, 256);
    }

    #[test]
    fn test_validate_code_forbidden_imports() {
        let tool = CodeExecutionTool::new();

        assert!(tool.validate_code("import subprocess").is_err());
        assert!(tool.validate_code("import os.system").is_err());
        assert!(tool.validate_code("import socket").is_err());
        assert!(tool.validate_code("import ctypes").is_err());
        assert!(tool.validate_code("from subprocess import call").is_err());
    }

    #[test]
    fn test_validate_code_dangerous_patterns() {
        let tool = CodeExecutionTool::new();

        assert!(tool.validate_code("exec('print(1)')").is_err());
        assert!(tool.validate_code("eval('1+1')").is_err());
        assert!(tool.validate_code("__import__('os')").is_err());
        assert!(tool
            .validate_code("compile('x=1', '<string>', 'exec')")
            .is_err());
    }

    #[test]
    fn test_validate_code_case_insensitive() {
        let tool = CodeExecutionTool::new();
        assert!(tool.validate_code("EVAL('1+1')").is_err());
        assert!(tool.validate_code("Eval('1+1')").is_err());
        assert!(tool.validate_code("IMPORT subprocess").is_err());
    }

    #[test]
    fn test_validate_code_valid_patterns() {
        let tool = CodeExecutionTool::new();
        assert!(tool.validate_code("x = 1 + 2").is_ok());
        assert!(tool.validate_code("import json\nimport math").is_ok());
        assert!(tool.validate_code("def hello():\n    pass").is_ok());
    }

    #[test]
    fn test_sandbox_config_serde() {
        let config = SandboxConfig {
            timeout_secs: 60,
            max_output_size: 1000,
            allowed_imports: vec!["json".to_string()],
            forbidden_imports: vec!["os".to_string()],
            allow_network: true,
            max_memory_mb: 512,
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: SandboxConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.timeout_secs, 60);
        assert_eq!(restored.max_output_size, 1000);
        assert_eq!(restored.allowed_imports, vec!["json"]);
        assert_eq!(restored.forbidden_imports, vec!["os"]);
        assert!(restored.allow_network);
        assert_eq!(restored.max_memory_mb, 512);
    }

    #[test]
    fn test_code_execution_tool_new() {
        let tool = CodeExecutionTool::new();
        assert_eq!(tool.config.timeout_secs, 300);
    }

    #[test]
    fn test_code_execution_tool_default() {
        let tool: CodeExecutionTool = Default::default();
        assert_eq!(tool.config.max_memory_mb, 256);
    }

    #[test]
    fn test_code_execution_tool_with_config() {
        let config = SandboxConfig {
            timeout_secs: 10,
            ..Default::default()
        };
        let tool = CodeExecutionTool::with_config(config);
        assert_eq!(tool.config.timeout_secs, 10);
    }

    #[test]
    fn test_code_result_creation() {
        let result = CodeResult {
            stdout: "hello".to_string(),
            stderr: "".to_string(),
            exit_code: 0,
            result: serde_json::json!({"success": true}),
        };
        assert_eq!(result.stdout, "hello");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn test_tool_name() {
        let tool = CodeExecutionTool::new();
        assert_eq!(tool.name(), "execute_code");
    }

    #[test]
    fn test_tool_description_not_empty() {
        let tool = CodeExecutionTool::new();
        assert!(!tool.description().is_empty());
        assert!(tool.description().contains("Python"));
    }

    #[test]
    fn test_tool_parameters_schema() {
        let tool = CodeExecutionTool::new();
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
        assert!(schema.get("required").is_some());
    }

    #[tokio::test]
    async fn test_execute_missing_code() {
        let tool = CodeExecutionTool::new();
        let ctx = ToolContext::new("user1", "conv1");
        let args = serde_json::json!({"language": "python"});
        let result = tool.execute(args, &ctx).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("code parameter is required"));
    }

    #[tokio::test]
    async fn test_execute_unsupported_language() {
        let tool = CodeExecutionTool::new();
        let ctx = ToolContext::new("user1", "conv1");
        let args = serde_json::json!({"code": "print(1)", "language": "ruby"});
        let result = tool.execute(args, &ctx).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Unsupported language"));
    }

    #[tokio::test]
    async fn test_execute_validation_failure() {
        let tool = CodeExecutionTool::new();
        let ctx = ToolContext::new("user1", "conv1");
        let args = serde_json::json!({"code": "import subprocess"});
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_ref()
            .unwrap()
            .contains("Code validation failed"));
    }

    #[tokio::test]
    async fn test_execute_python_simple() {
        let tool = CodeExecutionTool::new();
        let ctx = ToolContext::new("user1", "conv1");
        let args = serde_json::json!({"code": "print('hello world')"});
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("hello world"));
    }

    #[tokio::test]
    async fn test_execute_python_error() {
        let tool = CodeExecutionTool::new();
        let ctx = ToolContext::new("user1", "conv1");
        let args = serde_json::json!({"code": "raise ValueError('boom')"});
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.success);
        let err_text = result.error.as_ref().unwrap();
        assert!(err_text.contains("boom") || err_text.contains("ValueError"));
    }

    #[tokio::test]
    async fn test_execute_python_timeout() {
        // Skip if python3 is not available in the test environment.
        let python_check =
            crate::tools::process_runner::run(&ProcessRequest::argv(&["python3", "--version"]))
                .await;
        if python_check.map(|o| !o.success()).unwrap_or(true) {
            return;
        }

        let tool = CodeExecutionTool::new();
        let ctx = ToolContext::new("user1", "conv1");
        let args = serde_json::json!({
            "code": "import time\ntime.sleep(100)",
            "timeout": 1
        });

        let start = Instant::now();
        let result = tool.execute(args, &ctx).await.unwrap();
        let elapsed = start.elapsed();

        assert!(!result.success, "timed-out code execution should not succeed");
        assert!(
            result.error.as_ref().unwrap().contains("timed out"),
            "expected timeout error, got {:?}",
            result.error
        );
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "code execution was not killed within timeout: {:?}",
            elapsed
        );
    }

    // ── WASM in-process engine (P3.3, §4.5) ──

    /// wasm32-wasi command that writes "hello from wasm\n" to stdout via
    /// fd_write (fd 1), then exits normally. Passed as WAT — wasmtime's
    /// `Module::new` parses the text format directly.
    #[cfg(feature = "plugins")]
    fn wasm_hello_wat() -> String {
        r#"
(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 8) "hello from wasm\n")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 8))
    (i32.store (i32.const 4) (i32.const 16))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 32)))))
"#
        .to_string()
    }

    /// wasm32-wasi command with a tight infinite loop — exercises the fuel
    /// metering timeout.
    #[cfg(feature = "plugins")]
    fn wasm_infinite_loop_wat() -> String {
        r#"
(module
  (func (export "_start")
    (loop (br 0))))
"#
        .to_string()
    }

    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn test_execute_wasm_hello() {
        use base64::Engine;
        let tool = CodeExecutionTool::new();
        let ctx = ToolContext::new("user1", "conv1");
        let code = base64::engine::general_purpose::STANDARD.encode(wasm_hello_wat());
        let args = serde_json::json!({"language": "wasm", "code": code});
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(result.success, "expected success, got {:?}", result.error);
        assert!(result.output.contains("hello from wasm"));
    }

    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn test_execute_wasm_timeout() {
        use base64::Engine;
        let tool = CodeExecutionTool::new();
        let ctx = ToolContext::new("user1", "conv1");
        let code = base64::engine::general_purpose::STANDARD.encode(wasm_infinite_loop_wat());
        let args = serde_json::json!({"language": "wasm", "code": code, "timeout": 1});
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.success, "infinite loop should not succeed");
        assert!(
            result.error.as_ref().unwrap().contains("timed out"),
            "expected fuel-exhaustion timeout, got {:?}",
            result.error
        );
    }

    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn test_execute_wasm_invalid_module() {
        use base64::Engine;
        let tool = CodeExecutionTool::new();
        let ctx = ToolContext::new("user1", "conv1");
        let code = base64::engine::general_purpose::STANDARD.encode(b"not a wasm module");
        let args = serde_json::json!({"language": "wasm", "code": code});
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_ref()
                .unwrap()
                .contains("Invalid WASM module"),
            "got {:?}",
            result.error
        );
    }

    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn test_execute_wasm_missing_start() {
        use base64::Engine;
        let tool = CodeExecutionTool::new();
        let ctx = ToolContext::new("user1", "conv1");
        // Valid module, but not a command — no `_start` export.
        let wat = "(module (memory (export \"memory\") 1))";
        let code = base64::engine::general_purpose::STANDARD.encode(wat);
        let args = serde_json::json!({"language": "wasm", "code": code});
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("_start"), "got {:?}", result.error);
    }
}
