//! Platform-abstracted subprocess execution.
//!
//! Collects the scattered `Command::new` sites in `src/tools/*` behind a
//! single trait so that mobile builds route process execution through an
//! Android `sh`/bundled-native-binary launcher (or reject it outright on
//! iOS) while the desktop keeps today's `std::process` behavior unchanged
//! (docs/mobile-migration.md §4.3).
//!
//! Tools call [`run`] / [`spawn`] instead of constructing a
//! `tokio::process::Command` directly; the facade dispatches to the
//! platform-appropriate [`ProcessRunner`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

use async_trait::async_trait;
use std::process::Stdio;
use tokio::io::AsyncRead;
use tokio::process::{Child, Command};

#[cfg(target_os = "linux")]
use landlock::{
    AccessFs, BitFlags, CompatLevel, Compatible, LandlockStatus, PathBeneath, PathFd, Ruleset,
    RulesetAttr, RulesetCreated, RulesetCreatedAttr, RulesetError, ABI,
};

#[cfg(target_os = "windows")]
use crate::tools::win_appcontainer;

impl ProcessRequest {
    /// Build a request from a plain argv (no cwd/env/stdin/timeout).
    pub fn argv(argv: &[&str]) -> Self {
        Self {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }
}

/// How a [`ProcessRunner::spawn`]'d child has its standard streams wired.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StdioMode {
    /// All three streams point at /dev/null — detached background processes.
    #[default]
    Null,
    /// stdout/stderr are piped (readable from the returned `Child`);
    /// stdin is /dev/null.
    Piped,
}

/// Write-fence descriptor, platform-neutral.
///
/// When present on a [`ProcessRequest`], the platform runner confines the
/// child with a kernel write fence: all file-write operations outside
/// `workspace_root` / `allowed_paths` are denied, regardless of what the
/// command does. Enforcement is platform-specific — macOS wraps `argv` behind
/// `/usr/bin/sandbox-exec` ([`MacSeatbeltRunner`]); Linux restricts the child
/// in-place with Landlock ([`LandlockRunner`]). On platforms without a kernel
/// fence the field exists but is ignored.
#[derive(Debug, Clone)]
pub struct WriteFence {
    /// Directory the child may write into (recursively).
    pub workspace_root: std::path::PathBuf,
    /// Additional roots the child may write into (recursively).
    pub allowed_paths: Vec<std::path::PathBuf>,
    /// Deny outbound network for the fenced process.
    ///
    /// Off unless the deployment asked for it (`[security] fence_network`):
    /// `curl`, `git fetch` and package installs are ordinary work, so the
    /// network stays reachable by default and the fence constrains writes.
    pub deny_network: bool,
    /// Subpaths that stay unwritable even when they sit inside a granted
    /// root. Computed by [`WriteFence::new`]; see
    /// [`PROTECTED_WRITE_NAMES`] for why each is here.
    ///
    /// Enforced by Seatbelt (its profile is last-match-wins, so a deny after
    /// an allow wins) and left out on Linux — Landlock grants are additive
    /// and cannot subtract a subtree from a granted root. Windows enforces
    /// the roots but not these carve-outs yet.
    pub protected_paths: Vec<std::path::PathBuf>,
}

/// Directory names that are never writable, even inside a granted root.
///
/// `.git` is the sharp one: writing `.git/hooks/*` or `.git/config` gives code
/// execution on the next `git` command — which is exactly the escape a fence
/// exists to prevent. `.syscity` is this runtime's own per-workspace
/// metadata, which the agent has no business editing through a shell.
pub const PROTECTED_WRITE_NAMES: &[&str] = &[".git", ".syscity"];

impl WriteFence {
    /// A fence for one tool call: the granted roots plus every protected name
    /// that lands inside one of them.
    pub fn new(
        workspace_root: std::path::PathBuf,
        allowed_paths: Vec<std::path::PathBuf>,
        deny_network: bool,
    ) -> Self {
        let mut protected_paths = Vec::new();
        for root in std::iter::once(&workspace_root).chain(allowed_paths.iter()) {
            for name in PROTECTED_WRITE_NAMES {
                protected_paths.push(root.join(name));
            }
        }
        Self {
            workspace_root,
            allowed_paths,
            deny_network,
            protected_paths,
        }
    }
}

/// A subprocess spawn request, covering the knobs the tools actually use.
#[derive(Clone, Default)]
pub struct ProcessRequest {
    /// Program + arguments. `argv[0]` is the executable; empty => error.
    pub argv: Vec<String>,
    /// Optional kernel write fence applied around this process (platform
    /// runners enforce it only when it is Some and the platform supports it).
    pub fence: Option<WriteFence>,
    /// Working directory for the child.
    pub cwd: Option<PathBuf>,
    /// Clear the inherited environment before applying `env`.
    pub env_clear: bool,
    /// Extra environment variables to set on the child.
    pub env: HashMap<String, String>,
    /// Data written to the child's stdin (`None` => null stdin, matching
    /// `Command::output()` semantics).
    pub stdin: Option<Vec<u8>>,
    /// Wall-clock timeout for a [`ProcessRunner::run`]; `None` waits forever.
    pub timeout: Option<Duration>,
    /// Unix resource-limit hook applied in the child before `exec`
    /// (`setrlimit` etc.). Ignored on non-Unix platforms.
    pub pre_exec: Option<Arc<dyn Fn() -> std::io::Result<()> + Send + Sync>>,
    /// Stdio wiring for [`ProcessRunner::spawn`]. Ignored by
    /// [`ProcessRunner::run`], which always captures output.
    pub stdio: StdioMode,
}

/// Output of a completed [`ProcessRunner::run`].
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// `None` when the run was aborted (spawn failure or timeout).
    pub status: Option<ExitStatus>,
    /// Signal that terminated the process (`None` when it exited normally,
    /// when `status` is `None`, or on non-Unix platforms).
    ///
    /// Orthogonal to [`CommandOutput::timed_out`]: `signal` records *how* the
    /// process died whenever a signal was involved (an external kill or a
    /// kill from our own tooling observed through a completed wait), while
    /// `timed_out` records *that we cut the run short*. Our timeout kill
    /// discards the wait status (`timed_out: true`, `status: None`), so the
    /// `SIGKILL` we deliver ourselves is never reported through this field.
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// True when the run was cut short by its timeout. `status` is `None`
    /// and the buffers hold whatever partial output was collected.
    pub timed_out: bool,
}

impl CommandOutput {
    /// Assemble a completed-run output from a final wait status. On Unix the
    /// terminating signal (if any) is extracted via
    /// `std::os::unix::process::ExitStatusExt::signal`.
    fn from_status(status: ExitStatus, stdout: Vec<u8>, stderr: Vec<u8>) -> Self {
        Self {
            status: Some(status),
            #[cfg(unix)]
            signal: status.signal(),
            #[cfg(not(unix))]
            signal: None,
            stdout,
            stderr,
            timed_out: false,
        }
    }

    pub fn stdout_string(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_string(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// True when the process exited with a zero status (and actually ran).
    pub fn success(&self) -> bool {
        self.status.as_ref().is_some_and(|s| s.success())
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.status.and_then(|s| s.code())
    }
}

/// Failure modes for a subprocess run.
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("no executable specified")]
    EmptyArgv,
    #[error("failed to spawn '{program}': {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("process timed out after {duration:?}")]
    Timeout { duration: Duration },
    #[error("process execution is not supported on this platform")]
    Unsupported,
    #[error("sandbox unavailable: {reason}")]
    Sandbox { reason: String },
}

/// A spawned child process, abstracted from the concrete spawn mechanism.
///
/// Platform runners can return custom children — e.g. Windows fences spawn
/// AppContainer processes owned by a Job object and cannot go through
/// `tokio::process` at all — while the tools keep calling the same
/// `id`/`wait`/`kill`/pipe surface they used on the tokio `Child`.
#[async_trait]
pub trait ProcessChild: Send {
    /// The OS PID, if available.
    fn id(&self) -> Option<u32>;
    /// Take the child's stdout pipe, if piped (`None` for null/closed streams).
    fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Unpin + Send>>;
    /// Take the child's stderr pipe, if piped (`None` for null/closed streams).
    fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Unpin + Send>>;
    /// Wait for the child to exit and return its status.
    async fn wait(&mut self) -> std::io::Result<ExitStatus>;
    /// Terminate the child (and, for fenced Windows children, its whole job).
    async fn kill(&mut self) -> std::io::Result<()>;
}

/// Adapter exposing a `tokio::process::Child` through the [`ProcessChild`]
/// trait — the common case for runners that spawn via `tokio::process::Command`
/// ([`StdProcessRunner`] and the wrapper runners that delegate to it).
pub struct TokioProcessChild {
    inner: Child,
}

impl TokioProcessChild {
    fn new(inner: Child) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl ProcessChild for TokioProcessChild {
    fn id(&self) -> Option<u32> {
        self.inner.id()
    }

    fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Unpin + Send>> {
        self.inner
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn AsyncRead + Unpin + Send>)
    }

    fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Unpin + Send>> {
        self.inner
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn AsyncRead + Unpin + Send>)
    }

    async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.inner.wait().await
    }

    async fn kill(&mut self) -> std::io::Result<()> {
        self.inner.kill().await
    }
}

/// Platform-abstracted subprocess launcher.
#[async_trait]
pub trait ProcessRunner: Send + Sync {
    /// Run `req` to completion and capture stdout/stderr. On timeout the
    /// child's whole Unix process group is killed and
    /// [`ProcessError::Timeout`] is returned; partial output is discarded
    /// (use [`run_collect`](ProcessRunner::run_collect) to keep it).
    async fn run(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError>;

    /// Run `req` like [`run`](ProcessRunner::run), but on timeout the
    /// child's whole Unix process group is killed and whatever partial
    /// output was collected is returned with `timed_out: true` instead of
    /// an error.
    ///
    /// The default implementation preserves legacy `run` semantics (no
    /// partial capture) for runners that do not override it.
    async fn run_collect(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        match self.run(req).await {
            Ok(out) => Ok(out),
            Err(ProcessError::Timeout { .. }) => Ok(CommandOutput {
                status: None,
                signal: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                timed_out: true,
            }),
            Err(e) => Err(e),
        }
    }

    /// Spawn `req` and return a live [`ProcessChild`] for long-running process
    /// management (tracked status, detached wait). Stdio follows
    /// `ProcessRequest::stdio` ([`StdioMode::Null`] by default).
    async fn spawn(&self, req: &ProcessRequest) -> Result<Box<dyn ProcessChild>, ProcessError>;
}

/// Desktop: spawns through `tokio::process` exactly as the tools did before
/// the abstraction (env, cwd, pipes, timeouts, error mapping unchanged).
#[derive(Debug, Clone, Copy, Default)]
pub struct StdProcessRunner;

#[async_trait]
impl ProcessRunner for StdProcessRunner {
    async fn run(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        // Delegate to `run_collect` so both entry points share a single
        // spawn/collect/kill implementation (one timeout kill path to keep
        // group-kill semantics correct); `run` preserves its legacy contract
        // of surfacing a timeout as an error instead of partial output.
        let out = self.run_collect(req).await?;
        if out.timed_out {
            return Err(ProcessError::Timeout {
                duration: req.timeout.unwrap_or_default(),
            });
        }
        Ok(out)
    }

    async fn spawn(&self, req: &ProcessRequest) -> Result<Box<dyn ProcessChild>, ProcessError> {
        let mut cmd = build_command(req)?;
        match req.stdio {
            StdioMode::Null => {
                cmd.stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .stdin(Stdio::null());
            }
            StdioMode::Piped => {
                cmd.stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .stdin(Stdio::null());
            }
        }
        cmd.spawn()
            .map(TokioProcessChild::new)
            .map(|c| Box::new(c) as Box<dyn ProcessChild>)
            .map_err(|source| spawn_err(req, source))
    }

    async fn run_collect(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut cmd = build_command(req)?;
        let mut child = cmd
            .stdin(if req.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| spawn_err(req, source))?;

        if let Some(input) = &req.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                // Closing stdin on drop signals EOF to the child.
                let _ = stdin.write_all(input).await;
            }
        }

        let mut out_pipe = child
            .stdout
            .take()
            .ok_or_else(|| spawn_err(req, std::io::Error::other("stdout pipe missing")))?;
        let mut err_pipe = child
            .stderr
            .take()
            .ok_or_else(|| spawn_err(req, std::io::Error::other("stderr pipe missing")))?;
        // Pump pipes in background tasks so they keep draining across the
        // timeout boundary; after exit/kill they hit EOF and return their
        // buffers.
        let out_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let res = out_pipe.read_to_end(&mut buf).await;
            res.map(|_| buf)
        });
        let err_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let res = err_pipe.read_to_end(&mut buf).await;
            res.map(|_| buf)
        });

        let status = match req.timeout {
            Some(duration) => match tokio::time::timeout(duration, child.wait()).await {
                Ok(res) => Some(res.map_err(|source| spawn_err(req, source))?),
                Err(_) => {
                    // Kill the runaway tree (tokio does not kill on drop) and
                    // reap it before collecting partial output. Every Unix
                    // child is its own process-group leader (`build_command`
                    // sets pgid), so one negative-pid SIGKILL reaches the
                    // command's own descendants too — e.g. the `sleep` in
                    // `sh -c 'sleep 30 &'` would otherwise survive as an
                    // orphan holding the output pipes open. Windows has no
                    // process group on unfenced spawns: fall back to killing
                    // the direct child only (the fenced Windows path
                    // terminates its Job-object tree separately).
                    #[cfg(unix)]
                    if let Some(pid) = child.id() {
                        // Best-effort: ESRCH just means the tree is gone.
                        #[allow(unsafe_code)] // signal delivery only; no shared state touched
                        unsafe {
                            libc::kill(-(pid as i32), libc::SIGKILL);
                        }
                    }
                    #[cfg(not(unix))]
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    None
                }
            },
            None => Some(
                child
                    .wait()
                    .await
                    .map_err(|source| spawn_err(req, source))?,
            ),
        };

        let stdout = out_task.await.ok().and_then(|r| r.ok()).unwrap_or_default();
        let stderr = err_task.await.ok().and_then(|r| r.ok()).unwrap_or_default();

        Ok(match status {
            Some(status) => CommandOutput::from_status(status, stdout, stderr),
            // Timeout path: our kill discards the wait status; only the
            // partial output collected so far is reported.
            None => CommandOutput {
                status: None,
                signal: None,
                stdout,
                stderr,
                timed_out: true,
            },
        })
    }
}

/// macOS: wraps an argv in `/usr/bin/sandbox-exec` so `workspace_only`
/// becomes a kernel write fence instead of a parent-process path check.
///
/// The runner is a platform singleton, so whether to fence is decided
/// per-request via [`ProcessRequest::fence`]; this runner only wraps when
/// the request asks for it and Seatbelt is usable.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, Default)]
pub struct MacSeatbeltRunner {
    inner: StdProcessRunner,
}

#[cfg(target_os = "macos")]
impl MacSeatbeltRunner {
    /// Rewrite `req.argv` behind `sandbox-exec` when the request asks for the
    /// fence. Hard-fails (fail-closed) when Seatbelt is unavailable — missing
    /// binary, running as root, or a functional probe failure.
    fn fence(&self, req: &ProcessRequest) -> Result<ProcessRequest, ProcessError> {
        let Some(fence) = &req.fence else {
            return Ok(req.clone());
        };
        if !can_sandbox() {
            return Err(ProcessError::Sandbox {
                reason: "Seatbelt write fence requested but /usr/bin/sandbox-exec is not usable \
                         (missing binary, running as root, or probe failed)"
                    .to_string(),
            });
        }
        let mut fenced = req.clone();
        fenced.fence = None;
        let mut argv = Vec::with_capacity(req.argv.len() + 3);
        argv.push("/usr/bin/sandbox-exec".to_string());
        argv.push("-p".to_string());
        argv.push(seatbelt_profile(fence));
        argv.extend_from_slice(&req.argv);
        fenced.argv = argv;
        // No extra pre_exec needed here: `sandbox-exec` forks before exec'ing
        // the real command, and both the wrapper and its sandboxed
        // grandchild share the process group that `build_command` puts every
        // child into, so the base runner's negative-pid timeout kill reaches
        // the whole tree.
        Ok(fenced)
    }
}

#[cfg(target_os = "macos")]
#[async_trait]
impl ProcessRunner for MacSeatbeltRunner {
    async fn run(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        let fenced = self.fence(req)?;
        self.inner.run(&fenced).await
    }

    async fn run_collect(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        // Delegate so partial output survives a timeout (the trait default
        // returns empty buffers on timeout).
        let fenced = self.fence(req)?;
        self.inner.run_collect(&fenced).await
    }

    async fn spawn(&self, req: &ProcessRequest) -> Result<Box<dyn ProcessChild>, ProcessError> {
        let fenced = self.fence(req)?;
        self.inner.spawn(&fenced).await
    }
}

/// Build a Seatbelt write-fence profile (last-match-wins): allow everything
/// by default, deny all file writes, then re-open writes to the workspace,
/// the extra allowed paths, and `/dev/null` (needed for the ubiquitous
/// `2>/dev/null` idiom).
#[cfg(target_os = "macos")]
fn seatbelt_profile(fence: &WriteFence) -> String {
    fn quote(path: &std::path::Path) -> String {
        // Canonicalize so `/var/...` -> `/private/var/...` symlinks don't
        // silently break subpath matching.
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let escaped = canonical
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        format!("\"{escaped}\"")
    }

    // `(allow default)` first and denies after: Seatbelt is last-match-wins,
    // so every deny below carves itself out of the allowances above it.
    let mut out = String::from("(version 1)(allow default)");
    if fence.deny_network {
        // Denies every outbound connection — loopback included, which is why
        // the sandbox network test can assert denial without touching the
        // network at all.
        out.push_str("(deny network*)");
    }
    out.push_str("(deny file-write*)");
    out.push_str("(allow file-write* (subpath ");
    out.push_str(&quote(&fence.workspace_root));
    out.push_str("))");
    for path in &fence.allowed_paths {
        out.push_str("(allow file-write* (subpath ");
        out.push_str(&quote(path));
        out.push_str("))");
    }
    out.push_str("(allow file-write* (literal \"/dev/null\"))");
    // Carve-outs: `.git`/`.syscity` stay unwritable wherever they sit inside a
    // granted root, so a fenced shell cannot plant a git hook (code execution
    // on the next git command) or edit the runtime's own metadata.
    for path in &fence.protected_paths {
        out.push_str("(deny file-write* (subpath ");
        out.push_str(&quote(path));
        out.push_str("))");
    }
    out
}

/// Whether the macOS Seatbelt fence is usable, cached once. Checks the binary
/// exists, we are not root (Seatbelt does not confine euid 0), and a trivial
/// sandboxed probe exits successfully.
#[cfg(target_os = "macos")]
fn can_sandbox() -> bool {
    static AVAILABLE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        if !std::path::Path::new("/usr/bin/sandbox-exec").is_file() {
            return false;
        }
        let not_root = std::process::Command::new("/usr/bin/id")
            .arg("-u")
            .output()
            .is_ok_and(|out| {
                out.status.success()
                    && String::from_utf8_lossy(&out.stdout)
                        .trim()
                        .parse::<u32>()
                        .is_ok_and(|uid| uid != 0)
            });
        if !not_root {
            return false;
        }
        std::process::Command::new("/usr/bin/sandbox-exec")
            // `(allow default)` is required: a bare `(version 1)` profile
            // denies everything, which would block the probe's own exec.
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .status()
            .is_ok_and(|status| status.success())
    });
    *AVAILABLE
}

/// Linux: enforces the write fence in-place with Landlock, the kernel LSM.
///
/// Unlike macOS (which rewrites `argv` behind `sandbox-exec`), Landlock
/// restricts the child itself: the `pre_exec` hook calls
/// `landlock_restrict_self()` after fork, so there is no wrapper process (and
/// no process-group kill needed — the restricted child is the direct child).
/// Enforcement is a write fence: every handled write right is denied outside
/// `workspace_root` / `allowed_paths`. Unhandled rights (reads, exec, network)
/// stay unrestricted.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, Default)]
pub struct LandlockRunner {
    inner: StdProcessRunner,
}

#[cfg(target_os = "linux")]
impl LandlockRunner {
    /// Compose a `pre_exec` that applies the Landlock fence when the request
    /// asks for it. Hard-fails (fail-closed) when Landlock is unavailable —
    /// kernel without Landlock, unsupported write rights, or a fenced path
    /// that cannot be opened.
    fn fence(&self, req: &ProcessRequest) -> Result<ProcessRequest, ProcessError> {
        let Some(fence) = &req.fence else {
            return Ok(req.clone());
        };
        if !landlock_available(fence) {
            return Err(ProcessError::Sandbox {
                reason: "Landlock write fence requested but not usable (kernel without Landlock, \
                         unsupported write rights, or a fenced path cannot be opened)"
                    .to_string(),
            });
        }
        let mut fenced = req.clone();
        fenced.fence = None;
        let fence = fence.clone();
        let existing_pre = req.pre_exec.clone();
        fenced.pre_exec = Some(Arc::new(move || {
            if let Some(pre) = &existing_pre {
                pre()?;
            }
            landlock_restrict(&fence)
        }));
        Ok(fenced)
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl ProcessRunner for LandlockRunner {
    async fn run(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        let fenced = self.fence(req)?;
        self.inner.run(&fenced).await
    }

    async fn run_collect(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        let fenced = self.fence(req)?;
        self.inner.run_collect(&fenced).await
    }

    async fn spawn(&self, req: &ProcessRequest) -> Result<Box<dyn ProcessChild>, ProcessError> {
        let fenced = self.fence(req)?;
        self.inner.spawn(&fenced).await
    }
}

/// ABI-1 write rights: the irreducible write fence, hard-required (fail-closed
/// on kernels without Landlock).
#[cfg(target_os = "linux")]
fn landlock_required_rights() -> BitFlags<AccessFs> {
    AccessFs::from_write(ABI::V1)
}

/// Write rights added by later ABIs, best-effort (silently dropped on kernels
/// too old to express them): rename/link protection (Refer, ABI 2) and
/// truncate (Truncate, ABI 3).
#[cfg(target_os = "linux")]
fn landlock_optional_rights() -> BitFlags<AccessFs> {
    AccessFs::Refer | AccessFs::Truncate
}

/// The full handled-rights mask; also the access granted on fenced paths.
#[cfg(target_os = "linux")]
fn landlock_write_rights() -> BitFlags<AccessFs> {
    landlock_required_rights() | landlock_optional_rights()
}

/// Build a ruleset handling the write fence's access rights.
#[cfg(target_os = "linux")]
fn landlock_ruleset() -> Result<RulesetCreated, RulesetError> {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(landlock_required_rights())?
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(landlock_optional_rights())?
        .create()
}

/// Create a ruleset and add a write rule for the workspace and each allowed
/// path. The caller either discards it (availability probe) or restricts the
/// calling process with it.
#[cfg(target_os = "linux")]
fn landlock_build(fence: &WriteFence) -> std::io::Result<RulesetCreated> {
    let mut ruleset = landlock_ruleset()
        .map_err(|e| std::io::Error::other(format!("landlock unavailable: {e}")))?;
    for path in std::iter::once(&fence.workspace_root).chain(fence.allowed_paths.iter()) {
        let fd = PathFd::new(path)
            .map_err(|e| std::io::Error::other(format!("cannot open '{}': {e}", path.display())))?;
        let rule = PathBeneath::new(fd, landlock_write_rights());
        ruleset = ruleset.add_rule(rule).map_err(|e| {
            std::io::Error::other(format!("landlock rule for '{}': {e}", path.display()))
        })?;
    }
    Ok(ruleset)
}

/// Parent-side gate: ensures the workspace directory exists (so the child's
/// rule-open succeeds) and that a ruleset handling the write rights can be
/// created for every fenced path. Does NOT call `restrict_self` (irreversible);
/// enforcement happens in the child.
#[cfg(target_os = "linux")]
fn landlock_available(fence: &WriteFence) -> bool {
    let _ = std::fs::create_dir_all(&fence.workspace_root);
    landlock_build(fence).is_ok()
}

/// Apply the write fence to the calling process. Runs in the `pre_exec` hook
/// after fork: `landlock_restrict_self` confines this process and its
/// descendants, and because the restriction is irrevocable (and `no_new_privs`
/// is set), the exec'd command inherits the fence.
#[cfg(target_os = "linux")]
fn landlock_restrict(fence: &WriteFence) -> std::io::Result<()> {
    let ruleset = landlock_build(fence)?;
    let status = ruleset
        .restrict_self()
        .map_err(|e| std::io::Error::other(format!("landlock restrict_self: {e}")))?;
    match status.landlock {
        LandlockStatus::Available { .. } => Ok(()),
        other => Err(std::io::Error::other(format!("landlock not enforced: {other:?}"))),
    }
}

/// Windows: enforces the write fence by running the child as a Low-integrity
/// AppContainer process owned by a Job object.
///
/// The fence direction is inverted vs. Unix: an AppContainer token denies
/// writes nearly everywhere, so [`win_appcontainer::grant_write_acl`] *grants*
/// workspace writes (a DACL ACE + mandatory Low label), and the Job provides
/// whole-tree termination (`kill()` = `TerminateJobObject`; closing the last
/// job handle kills whatever is left). See the `win_appcontainer` module docs
/// for the full model.
#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsAppContainerRunner {
    inner: StdProcessRunner,
}

/// Per-request fenced state: the AppContainer profile, ready to hand to
/// [`win_appcontainer::launch_fenced`].
#[cfg(target_os = "windows")]
struct Prepared {
    profile: win_appcontainer::AppContainerProfile,
}

#[cfg(target_os = "windows")]
impl WindowsAppContainerRunner {
    /// Prepare a fenced run when the request asks for the fence; `Ok(None)`
    /// means "run unfenced" (delegate to the inner runner). Fail-closed when
    /// the fence is requested but AppContainer is not usable.
    fn prepare(&self, req: &ProcessRequest) -> Result<Option<Prepared>, ProcessError> {
        let Some(fence) = &req.fence else {
            return Ok(None);
        };
        if !win_appcontainer::available() {
            return Err(ProcessError::Sandbox {
                reason: "Windows AppContainer write fence requested but not usable (profile \
                         creation failed or the OS lacks AppContainer support)"
                    .to_string(),
            });
        }
        let profile = win_appcontainer::AppContainerProfile::create_or_open().map_err(|e| {
            ProcessError::Sandbox {
                reason: format!("AppContainer profile: {e}"),
            }
        })?;
        // The workspace must exist for the ACL+label to apply, and it is the
        // child's scratch area.
        let _ = std::fs::create_dir_all(&fence.workspace_root);
        for path in std::iter::once(&fence.workspace_root).chain(fence.allowed_paths.iter()) {
            win_appcontainer::grant_write_acl(path, profile.sid()).map_err(|e| {
                ProcessError::Sandbox {
                    reason: format!("grant write ACL on '{}': {e}", path.display()),
                }
            })?;
        }
        Ok(Some(Prepared { profile }))
    }
}

#[cfg(target_os = "windows")]
#[async_trait]
impl ProcessRunner for WindowsAppContainerRunner {
    async fn run(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        match self.prepare(req)? {
            None => self.inner.run(req).await,
            Some(prepared) => {
                let out = run_fenced(req, &prepared.profile).await?;
                if out.timed_out {
                    // `run_fenced` already killed the job tree on timeout;
                    // surface the timeout as an error per the trait contract
                    // (no partial output) while leaving no orphan behind.
                    Err(ProcessError::Timeout {
                        duration: req.timeout.unwrap_or_default(),
                    })
                } else {
                    Ok(out)
                }
            }
        }
    }

    async fn run_collect(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        match self.prepare(req)? {
            None => self.inner.run_collect(req).await,
            Some(prepared) => run_fenced(req, &prepared.profile).await,
        }
    }

    async fn spawn(&self, req: &ProcessRequest) -> Result<Box<dyn ProcessChild>, ProcessError> {
        match self.prepare(req)? {
            None => self.inner.spawn(req).await,
            Some(prepared) => {
                // Mirror StdProcessRunner::spawn: stdout/stderr are piped iff
                // requested, stdin is null — a spawned child must not block
                // reading an unwritten pipe.
                let mut eff = req.clone();
                eff.stdin = None;
                let capture = req.stdio == StdioMode::Piped;
                let launched = win_appcontainer::launch_fenced(&prepared.profile, &eff, capture)
                    .map_err(|e| ProcessError::Sandbox {
                        reason: format!("AppContainer spawn: {e}"),
                    })?;
                Ok(Box::new(launched.into_child()))
            }
        }
    }
}

/// Run a fenced AppContainer child to completion, capturing stdout/stderr and
/// killing the whole Job tree on timeout (partial output survives via
/// `timed_out`). Shared by `WindowsAppContainerRunner::run`/`run_collect`.
#[cfg(target_os = "windows")]
async fn run_fenced(
    req: &ProcessRequest,
    profile: &win_appcontainer::AppContainerProfile,
) -> Result<CommandOutput, ProcessError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let launched =
        win_appcontainer::launch_fenced(profile, req, true).map_err(|e| ProcessError::Sandbox {
            reason: format!("AppContainer run: {e}"),
        })?;
    let mut child = launched.into_child();

    if let Some(input) = &req.stdin {
        if let Some(mut stdin) = child.take_stdin() {
            let _ = stdin.write_all(input).await;
        }
    }

    let mut out_pipe = child
        .take_stdout()
        .ok_or_else(|| spawn_err(req, std::io::Error::other("stdout pipe missing")))?;
    let mut err_pipe = child
        .take_stderr()
        .ok_or_else(|| spawn_err(req, std::io::Error::other("stderr pipe missing")))?;
    // Pump pipes in background tasks so they keep draining across the timeout
    // boundary; after exit/kill they hit EOF and return their buffers.
    let out_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        out_pipe.read_to_end(&mut buf).await.map(|_| buf)
    });
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        err_pipe.read_to_end(&mut buf).await.map(|_| buf)
    });

    let (status, timed_out) = match req.timeout {
        Some(duration) => match tokio::time::timeout(duration, child.wait()).await {
            Ok(res) => (Some(res.map_err(|source| spawn_err(req, source))?), false),
            Err(_) => {
                // Whole-tree kill via the Job object. Dropping `child` would
                // also terminate everything (KILL_ON_JOB_CLOSE), but killing
                // now lets the pipe pumpers see EOF promptly.
                let _ = child.kill().await;
                let _ = child.wait().await;
                (None, true)
            }
        },
        None => (
            Some(
                child
                    .wait()
                    .await
                    .map_err(|source| spawn_err(req, source))?,
            ),
            false,
        ),
    };

    let stdout = out_task.await.ok().and_then(|r| r.ok()).unwrap_or_default();
    let stderr = err_task.await.ok().and_then(|r| r.ok()).unwrap_or_default();

    Ok(CommandOutput {
        status,
        // Windows has no signal concept; a job-tree kill on timeout is
        // reported through `timed_out` instead.
        signal: None,
        stdout,
        stderr,
        timed_out,
    })
}

/// Build the underlying `tokio::process::Command` from a request.
///
/// On Unix every child is additionally made its own process-group leader
/// (`setpgid(0, 0)` in a `pre_exec` hook, applied after any caller-supplied
/// hook) so that a timeout kill can take down the command's whole descendant
/// tree with one negative-pid signal (see
/// [`StdProcessRunner::run_collect`]). Windows non-fenced spawns stay
/// direct-child-only — there is no group equivalent; the fenced Windows path
/// uses a Job object for tree termination.
fn build_command(req: &ProcessRequest) -> Result<Command, ProcessError> {
    let program = req.argv.first().ok_or(ProcessError::EmptyArgv)?;
    let mut cmd = Command::new(program);
    cmd.args(&req.argv[1..]);
    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd);
    }
    if req.env_clear {
        cmd.env_clear();
    }
    for (key, value) in &req.env {
        cmd.env(key, value);
    }
    // `pre_exec` is a Unix-only API (it runs in the child after fork, before
    // exec). The field still exists on all platforms (ignored elsewhere) so
    // requests stay platform-neutral.
    #[cfg(unix)]
    {
        if let Some(pre) = &req.pre_exec {
            let pre = Arc::clone(pre);
            #[allow(unsafe_code)] // pre_exec runs in the child after fork; mirrors existing tools
            unsafe {
                cmd.pre_exec(move || pre());
            }
        }
    }
    #[cfg(unix)]
    {
        #[allow(unsafe_code)]
        // SAFETY: runs in the forked child before exec; `setpgid` is
        // async-signal-safe and touches no shared state.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    Ok(cmd)
}

fn spawn_err(req: &ProcessRequest, source: std::io::Error) -> ProcessError {
    ProcessError::Spawn {
        program: req.argv.first().cloned().unwrap_or_default(),
        source,
    }
}

/// The cached platform-appropriate runner.
pub fn process_runner() -> Arc<dyn ProcessRunner> {
    static RUNNER: std::sync::LazyLock<Arc<dyn ProcessRunner>> =
        std::sync::LazyLock::new(default_process_runner);
    Arc::clone(&RUNNER)
}

fn default_process_runner() -> Arc<dyn ProcessRunner> {
    #[cfg(target_os = "android")]
    {
        Arc::new(AndroidShellRunner::from_env())
    }
    #[cfg(target_os = "ios")]
    {
        Arc::new(IosProcessRunner)
    }
    #[cfg(all(target_os = "macos", not(mobile_os)))]
    {
        Arc::new(MacSeatbeltRunner::default())
    }
    #[cfg(all(target_os = "linux", not(mobile_os)))]
    {
        Arc::new(LandlockRunner::default())
    }
    #[cfg(target_os = "windows")]
    {
        Arc::new(WindowsAppContainerRunner::default())
    }
    #[cfg(not(any(
        mobile_os,
        target_os = "macos",
        target_os = "linux",
        target_os = "windows"
    )))]
    {
        Arc::new(StdProcessRunner)
    }
}

/// Run a subprocess to completion, capturing output. See [`ProcessRunner::run`].
pub async fn run(req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
    process_runner().run(req).await
}

/// Run a subprocess to completion via [`ProcessRunner::run_collect`]: on
/// timeout the child is killed and partial output is returned with
/// `timed_out: true` rather than an error.
pub async fn run_collect(req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
    process_runner().run_collect(req).await
}

/// Spawn a detached subprocess. See [`ProcessRunner::spawn`].
pub async fn spawn(req: &ProcessRequest) -> Result<Box<dyn ProcessChild>, ProcessError> {
    process_runner().spawn(req).await
}

/// Android: the only executable entry points are `/system/bin/sh` (which
/// resolves the toybox applets by its built-in PATH) and bundled native
/// binaries shipped in `jniLibs` and extracted to `nativeLibraryDir`.
/// SELinux blocks `exec` from the app-private `filesDir` for targetSdk 29+,
/// so everything else is rejected (docs/mobile-migration.md §3.1).
#[cfg(target_os = "android")]
#[derive(Debug, Clone)]
pub struct AndroidShellRunner {
    native_library_dir: Option<PathBuf>,
    whitelist: Arc<std::collections::HashSet<&'static str>>,
    inner: StdProcessRunner,
}

#[cfg(target_os = "android")]
const TOYBOX_APPLETS: &[&str] = &[
    "sh",
    "/bin/sh",
    "/system/bin/sh",
    "ls",
    "cat",
    "echo",
    "printf",
    "pwd",
    "cp",
    "mv",
    "rm",
    "rmdir",
    "mkdir",
    "touch",
    "chmod",
    "chown",
    "grep",
    "sed",
    "awk",
    "wc",
    "head",
    "tail",
    "sort",
    "uniq",
    "find",
    "xxd",
    "base64",
    "date",
    "seq",
    "tr",
    "cut",
    "paste",
    "dirname",
    "basename",
    "stat",
    "df",
    "du",
    "ps",
    "sleep",
    "test",
    "which",
];

#[cfg(target_os = "android")]
impl AndroidShellRunner {
    /// Build the Android runner. `nativeLibraryDir` is read from
    /// `SYSCITY_NATIVE_LIB_DIR` (set by `MainActivity.kt` next to
    /// `SYSCITY_HOME`); bundled binaries are exec'd from there.
    pub fn from_env() -> Self {
        Self {
            native_library_dir: std::env::var("SYSCITY_NATIVE_LIB_DIR")
                .ok()
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
            whitelist: Arc::new(TOYBOX_APPLETS.iter().copied().collect()),
            inner: StdProcessRunner,
        }
    }

    /// Rewrite the request argv to an executable form permitted on Android.
    fn resolve_argv(&self, req: &ProcessRequest) -> Result<Vec<String>, ProcessError> {
        let program = req.argv.first().ok_or(ProcessError::EmptyArgv)?;

        // Bundled native binary (exec from nativeLibraryDir is the only
        // allowed exec path for same-UID binaries on targetSdk 29+).
        if let Some(dir) = &self.native_library_dir {
            let bundled = dir.join(program);
            if bundled.exists() {
                let mut argv = vec![bundled.to_string_lossy().into_owned()];
                argv.extend(req.argv[1..].iter().cloned());
                return Ok(argv);
            }
        }

        // The shell itself, or a whitelisted toybox applet routed through it
        // so the applet resolves via sh's built-in PATH.
        if self.whitelist.contains(program.as_str()) {
            return Ok(req.argv.clone());
        }

        Err(ProcessError::Unsupported)
    }
}

#[cfg(target_os = "android")]
impl AndroidShellRunner {
    /// Point a bundled native binary at its sibling libraries.
    ///
    /// The bundled `adb` client (mobile-migration §4.5) is dynamically linked
    /// against `libprotobuf.so`, `libabsl_*.so`, … shipped alongside it in
    /// nativeLibraryDir; its DT_RUNPATH points at a Termux path that does not
    /// exist here. Bionic honors `LD_LIBRARY_PATH` for non-setuid app
    /// processes, so set it to nativeLibraryDir for the bundled-exec path.
    /// `sh`/toybox need nothing (they only use bionic) and are untouched.
    fn apply_bundled_library_path(&self, eff: &mut ProcessRequest) {
        let Some(dir) = &self.native_library_dir else {
            return;
        };
        let Some(program) = eff.argv.first().map(String::as_str) else {
            return;
        };
        if dir.join(program).exists() {
            eff.env
                .insert("LD_LIBRARY_PATH".to_string(), dir.to_string_lossy().into_owned());
        }
    }
}

#[cfg(target_os = "android")]
#[async_trait]
impl ProcessRunner for AndroidShellRunner {
    async fn run(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        let mut eff = req.clone();
        eff.argv = self.resolve_argv(req)?;
        self.apply_bundled_library_path(&mut eff);
        self.inner.run(&eff).await
    }

    async fn run_collect(&self, req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        let mut eff = req.clone();
        eff.argv = self.resolve_argv(req)?;
        self.apply_bundled_library_path(&mut eff);
        self.inner.run_collect(&eff).await
    }

    async fn spawn(&self, req: &ProcessRequest) -> Result<Box<dyn ProcessChild>, ProcessError> {
        let mut eff = req.clone();
        eff.argv = self.resolve_argv(req)?;
        self.apply_bundled_library_path(&mut eff);
        self.inner.spawn(&eff).await
    }
}

/// iOS: the sandbox forbids `fork`/`exec` for app code, so every process
/// call fails with [`ProcessError::Unsupported`] (docs/mobile-migration.md
/// §3.2).
#[cfg(target_os = "ios")]
#[derive(Debug, Clone, Copy, Default)]
pub struct IosProcessRunner;

#[cfg(target_os = "ios")]
#[async_trait]
impl ProcessRunner for IosProcessRunner {
    async fn run(&self, _req: &ProcessRequest) -> Result<CommandOutput, ProcessError> {
        Err(ProcessError::Unsupported)
    }

    async fn spawn(&self, _req: &ProcessRequest) -> Result<Box<dyn ProcessChild>, ProcessError> {
        Err(ProcessError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> ProcessRequest {
        ProcessRequest {
            argv: parts.iter().map(|s| s.to_string()).collect(),
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_run_captures_stdout() {
        let out = StdProcessRunner
            .run(&argv(&["/bin/echo", "hello"]))
            .await
            .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout_string().trim(), "hello");
    }

    #[tokio::test]
    async fn test_run_captures_stderr_and_exit_code() {
        let out = StdProcessRunner
            .run(&argv(&["/bin/sh", "-c", "echo err 1>&2; exit 3"]))
            .await
            .unwrap();
        assert!(!out.success());
        assert_eq!(out.exit_code(), Some(3));
        // Normal exit: no terminating signal, not a timeout.
        assert_eq!(out.signal, None);
        assert!(!out.timed_out);
        assert!(out.stderr_string().contains("err"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_reports_signal_when_child_is_killed() {
        let out = StdProcessRunner
            .run(&argv(&["/bin/sh", "-c", "kill -TERM $$"]))
            .await
            .unwrap();
        assert!(!out.success());
        // A signaled process has no exit code; the signal is surfaced
        // instead and is distinct from our own timeout kill (`timed_out`
        // stays false — this termination was requested by the child).
        assert_eq!(out.exit_code(), None);
        assert_eq!(out.signal, Some(libc::SIGTERM));
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn run_collect_timeout_kills_backgrounded_descendants() {
        // Unfenced spawn: `sleep 5` is backgrounded by sh. Without the
        // process-group kill the orphan keeps the shared stdout pipe open
        // and the pipe-drain below blocks ~5s past the 300ms timeout.
        let mut req = argv(&["/bin/sh", "-c", "echo partial-out; sleep 5 & wait"]);
        req.timeout = Some(Duration::from_millis(300));
        let started = std::time::Instant::now();
        let out = StdProcessRunner.run_collect(&req).await.unwrap();
        assert!(out.timed_out, "run_collect should report the timeout");
        // Our own SIGKILL discards the wait status, so no signal surfaces.
        assert_eq!(out.signal, None);
        assert!(
            out.stdout_string().contains("partial-out"),
            "partial output must survive the timeout: {:?}",
            out.stdout_string()
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "timeout must not wait for the orphaned sleep"
        );
    }

    #[tokio::test]
    async fn test_run_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let req = ProcessRequest {
            cwd: Some(tmp.path().to_path_buf()),
            ..argv(&["/bin/pwd"])
        };
        let out = StdProcessRunner.run(&req).await.unwrap();
        assert_eq!(
            std::fs::canonicalize(out.stdout_string().trim()).unwrap(),
            std::fs::canonicalize(tmp.path()).unwrap()
        );
    }

    #[tokio::test]
    async fn test_run_env_clear_and_env() {
        let req = ProcessRequest {
            env_clear: true,
            env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
            ..argv(&["/usr/bin/env"])
        };
        let out = StdProcessRunner.run(&req).await.unwrap();
        assert!(out.stdout_string().contains("FOO=bar"));
        // HOME is inherited only when not cleared; with env_clear it is gone.
        assert!(!out.stdout_string().contains("HOME="));
    }

    #[tokio::test]
    async fn test_run_stdin_piped() {
        let req = ProcessRequest {
            stdin: Some(b"ping\n".to_vec()),
            ..argv(&["/bin/cat"])
        };
        let out = StdProcessRunner.run(&req).await.unwrap();
        assert!(out.success());
        assert_eq!(out.stdout_string().trim(), "ping");
    }

    #[tokio::test]
    async fn test_run_timeout() {
        let req = ProcessRequest {
            timeout: Some(Duration::from_millis(100)),
            ..argv(&["/bin/sleep", "5"])
        };
        let err = StdProcessRunner.run(&req).await.unwrap_err();
        assert!(matches!(err, ProcessError::Timeout { .. }));
    }

    #[tokio::test]
    async fn test_run_empty_argv() {
        let req = ProcessRequest::default();
        let err = StdProcessRunner.run(&req).await.unwrap_err();
        assert!(matches!(err, ProcessError::EmptyArgv));
    }

    #[tokio::test]
    async fn test_spawn_and_wait() {
        let mut child = StdProcessRunner
            .spawn(&argv(&["/bin/echo", "hi"]))
            .await
            .unwrap();
        assert!(child.id().is_some());
        let status = child.wait().await.unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn test_pre_exec_does_not_break_basic_run() {
        let req = ProcessRequest {
            pre_exec: Some(Arc::new(|| Ok(()))),
            ..argv(&["/bin/echo", "ok"])
        };
        let out = StdProcessRunner.run(&req).await.unwrap();
        assert!(out.success());
    }

    #[cfg(target_os = "macos")]
    mod seatbelt {
        use super::*;

        fn fence(workspace_root: &str, allowed: &[&str]) -> WriteFence {
            WriteFence::new(
                std::path::PathBuf::from(workspace_root),
                allowed.iter().map(std::path::PathBuf::from).collect(),
                false,
            )
        }

        /// Same, with the network posture flipped on.
        fn fence_no_network(workspace_root: &str) -> WriteFence {
            WriteFence::new(std::path::PathBuf::from(workspace_root), Vec::new(), true)
        }

        #[test]
        fn profile_denies_writes_outside_workspace() {
            let p = seatbelt_profile(&fence("/tmp/ws", &[]));
            assert!(p.contains("(version 1)"));
            assert!(p.contains("(allow default)"));
            assert!(p.contains("(deny file-write*)"));
            assert!(p.contains("(allow file-write* (subpath \"/tmp/ws\"))"));
            assert!(p.contains("(allow file-write* (literal \"/dev/null\"))"));
        }

        #[test]
        fn profile_includes_allowed_paths() {
            let p = seatbelt_profile(&fence("/tmp/ws", &["/tmp/extra"]));
            assert!(p.contains("(allow file-write* (subpath \"/tmp/extra\"))"));
        }

        #[test]
        fn profile_denies_network_only_when_asked() {
            // The default posture leaves the network alone: `curl` and
            // `git fetch` are ordinary work.
            let permissive = seatbelt_profile(&fence("/tmp/ws", &[]));
            assert!(
                !permissive.contains("network"),
                "no network clause unless the deployment asked: {permissive}"
            );

            let denied = seatbelt_profile(&fence_no_network("/tmp/ws"));
            assert!(denied.contains("(deny network*)"), "got {denied}");
            // Order matters: the deny must come after `(allow default)`.
            let allow_at = denied.find("(allow default)").expect("allow default");
            let deny_at = denied.find("(deny network*)").expect("deny network");
            assert!(allow_at < deny_at, "last match wins, so the deny goes last");
        }

        #[test]
        fn profile_carves_git_and_syscity_out_of_the_workspace() {
            let f = fence("/tmp/ws", &["/tmp/extra"]);
            assert!(f
                .protected_paths
                .contains(&std::path::PathBuf::from("/tmp/ws/.git")));
            assert!(f
                .protected_paths
                .contains(&std::path::PathBuf::from("/tmp/extra/.git")));

            let p = seatbelt_profile(&f);
            let ws_allow = p
                .find("(allow file-write* (subpath \"/tmp/ws\"))")
                .expect("workspace allow");
            let git_deny = p
                .find("(deny file-write* (subpath \"/tmp/ws/.git\"))")
                .expect("git deny");
            assert!(ws_allow < git_deny, "the carve-out must follow the allow");
            assert!(p.contains("(deny file-write* (subpath \"/tmp/ws/.syscity\"))"));
            assert!(p.contains("(deny file-write* (subpath \"/tmp/extra/.git\"))"));
        }

        /// A fenced command cannot plant a `.git/hooks` payload even though
        /// the workspace it writes in is granted — that hook would run on the
        /// next `git` command, which is the escape the carve-out closes.
        #[tokio::test]
        async fn seatbelt_blocks_writes_into_git_metadata() {
            if !can_sandbox() {
                return;
            }
            let dir = tempfile::tempdir().expect("tempdir");
            let ws = dir.path().join("ws");
            std::fs::create_dir_all(ws.join(".git/hooks")).expect("mkdir");
            let target = ws.join(".git/hooks/pre-commit");
            let ordinary = ws.join("notes.md");

            let runner = MacSeatbeltRunner::default();
            let req = ProcessRequest {
                argv: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "echo pwned > {} ; echo fine > {}",
                        target.display(),
                        ordinary.display()
                    ),
                ],
                fence: Some(fence(&ws.to_string_lossy(), &[])),
                ..Default::default()
            };
            // The script's exit code is its last command's, so the refusal is
            // asserted on the file and the stderr rather than on `success()`.
            let out = runner.run(&req).await.expect("run");
            assert!(
                out.stderr_string().contains("not permitted"),
                "the .git write must be refused: {}",
                out.stderr_string()
            );
            assert!(!target.exists(), "no hook payload may land");
            assert!(ordinary.exists(), "ordinary workspace writes still work");
        }

        /// The network posture reaches a real child: with `deny_network` the
        /// sandbox refuses a loopback connect that succeeds without it.
        #[tokio::test]
        async fn seatbelt_denies_network_only_when_asked() {
            if !can_sandbox() {
                return;
            }
            // A listener makes the control case meaningful: without it both
            // runs fail and the test would pass for the wrong reason.
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();

            let connect = |port: u16| ProcessRequest {
                argv: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("exec 3<>/dev/tcp/127.0.0.1/{port}"),
                ],
                ..Default::default()
            };
            let dir = tempfile::tempdir().expect("tempdir");
            let runner = MacSeatbeltRunner::default();

            let mut allowed = connect(port);
            allowed.fence = Some(fence(&dir.path().to_string_lossy(), &[]));
            let out = runner.run(&allowed).await.expect("run");
            assert!(out.success(), "network is reachable by default: {out:?}");

            let mut denied = connect(port);
            denied.fence = Some(fence_no_network(&dir.path().to_string_lossy()));
            let out = runner.run(&denied).await.expect("run");
            assert!(!out.success(), "the deny must stop the connect: {out:?}");
            assert!(
                out.stderr_string().contains("not permitted"),
                "expected a denial, got: {}",
                out.stderr_string()
            );
        }

        #[test]
        fn profile_escapes_spaces_and_quotes() {
            let p = seatbelt_profile(&fence("/tmp/ws with space", &["/tmp/a\"b"]));
            assert!(p.contains("(subpath \"/tmp/ws with space\")"));
            assert!(p.contains("(subpath \"/tmp/a\\\"b\")"));
        }

        #[test]
        fn can_sandbox_is_usable_on_dev_machine() {
            assert!(can_sandbox(), "seatbelt must be usable on the dev mac");
        }

        #[test]
        fn fence_passes_through_without_seatbelt() {
            let runner = MacSeatbeltRunner::default();
            let req = argv(&["/bin/echo", "hi"]);
            let out = runner.fence(&req).unwrap();
            assert_eq!(out.argv, req.argv);
            assert!(out.fence.is_none());
        }

        #[test]
        fn fence_rewrites_argv_behind_sandbox_exec() {
            if !can_sandbox() {
                return;
            }
            let runner = MacSeatbeltRunner::default();
            let req = ProcessRequest {
                argv: vec!["/bin/echo".to_string(), "hi".to_string()],
                fence: Some(fence("/tmp/ws", &[])),
                ..Default::default()
            };
            let fenced = runner.fence(&req).unwrap();
            assert_eq!(fenced.argv[0], "/usr/bin/sandbox-exec");
            assert_eq!(fenced.argv[1], "-p");
            assert!(fenced.argv[2].contains("deny file-write*"));
            assert_eq!(&fenced.argv[3..], &["/bin/echo".to_string(), "hi".to_string()]);
            assert!(fenced.fence.is_none());
            // Group setup/teardown lives in the base runner now (`build_command`
            // puts every Unix child in its own group), so the fence no longer
            // composes any extra pre_exec hook of its own.
            assert!(fenced.pre_exec.is_none());
        }

        #[tokio::test]
        async fn seatbelt_blocks_write_outside_workspace() {
            if !can_sandbox() {
                return;
            }
            let runner = MacSeatbeltRunner::default();
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            let target = format!("/tmp/syscity_fence_{}", std::process::id());
            let _ = std::fs::remove_file(&target);
            let req = ProcessRequest {
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    format!("echo x > {target}"),
                ],
                fence: Some(fence(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            let out = runner.run(&req).await.unwrap();
            assert!(
                !out.success(),
                "write outside workspace must be denied: {}",
                out.stderr_string()
            );
            assert!(!std::path::Path::new(&target).exists());
        }

        #[tokio::test]
        async fn seatbelt_allows_write_inside_workspace() {
            if !can_sandbox() {
                return;
            }
            let runner = MacSeatbeltRunner::default();
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            let file = ws.join("f");
            let req = ProcessRequest {
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    format!("echo x > '{}'", file.display()),
                ],
                fence: Some(fence(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            let out = runner.run(&req).await.unwrap();
            assert!(
                out.success(),
                "write inside workspace must be allowed: {}",
                out.stderr_string()
            );
            assert!(file.exists());
        }

        #[tokio::test]
        async fn seatbelt_run_collect_timeout_kills_sandboxed_grandchild() {
            if !can_sandbox() {
                return;
            }
            let runner = MacSeatbeltRunner::default();
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            // `sleep 5` is a descendant of the sandboxed sh; a plain kill of
            // the wrapper would orphan it and leave the output pipe open.
            let req = ProcessRequest {
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo partial-out; sleep 5".to_string(),
                ],
                fence: Some(fence(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_millis(300)),
                ..Default::default()
            };
            let started = std::time::Instant::now();
            let out = runner.run_collect(&req).await.unwrap();
            assert!(out.timed_out, "run_collect should report the timeout");
            assert!(
                out.stdout_string().contains("partial-out"),
                "partial output must survive the timeout: {:?}",
                out.stdout_string()
            );
            // The group kill must reap the sandboxed grandchild promptly; if
            // the orphan still held the pipe, this await would block ~5s.
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "timeout must not wait for the orphaned sleep"
            );
        }
    }

    #[cfg(target_os = "linux")]
    mod landlock {
        use super::*;

        fn fence(workspace_root: &str, allowed: &[&str]) -> WriteFence {
            WriteFence {
                workspace_root: std::path::PathBuf::from(workspace_root),
                allowed_paths: allowed.iter().map(std::path::PathBuf::from).collect(),
            }
        }

        #[test]
        fn rights_mask_is_write_only() {
            let required = landlock_required_rights();
            assert!(required.contains(AccessFs::WriteFile));
            assert!(required.contains(AccessFs::MakeReg));
            assert!(required.contains(AccessFs::RemoveFile));
            assert!(required.contains(AccessFs::MakeDir));
            // Not a read fence: reads/exec stay unrestricted outside the
            // workspace.
            assert!(!required.contains(AccessFs::ReadFile));
            assert!(!required.contains(AccessFs::Execute));
            assert!(!required.contains(AccessFs::ReadDir));
            assert_eq!(landlock_optional_rights(), AccessFs::Refer | AccessFs::Truncate);
        }

        #[test]
        fn fence_passes_through_without_fence() {
            let runner = LandlockRunner::default();
            let req = argv(&["/bin/echo", "hi"]);
            let out = runner.fence(&req).unwrap();
            assert_eq!(out.argv, req.argv);
            assert!(out.fence.is_none());
            assert!(out.pre_exec.is_none());
        }

        #[test]
        fn fence_composes_pre_exec_without_argv_rewrite() {
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            if !landlock_available(&fence(ws.to_str().unwrap(), &[])) {
                eprintln!("skipping: Landlock unavailable");
                return;
            }
            let runner = LandlockRunner::default();
            let req = ProcessRequest {
                argv: vec!["/bin/echo".to_string(), "hi".to_string()],
                fence: Some(fence(ws.to_str().unwrap(), &[])),
                ..Default::default()
            };
            let fenced = runner.fence(&req).unwrap();
            // Unlike macOS there is no wrapper argv; the fence lives entirely
            // in the composed pre_exec hook.
            assert_eq!(fenced.argv, req.argv);
            assert!(fenced.fence.is_none());
            assert!(fenced.pre_exec.is_some());
        }

        #[tokio::test]
        async fn landlock_blocks_write_outside_workspace() {
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            if !landlock_available(&fence(ws.to_str().unwrap(), &[])) {
                eprintln!("skipping: Landlock unavailable");
                return;
            }
            let runner = LandlockRunner::default();
            let target = format!("/tmp/syscity_ll_{}", std::process::id());
            let _ = std::fs::remove_file(&target);
            let req = ProcessRequest {
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    format!("echo x > {target}"),
                ],
                fence: Some(fence(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            let out = runner.run(&req).await.unwrap();
            assert!(
                !out.success(),
                "write outside workspace must be denied: {}",
                out.stderr_string()
            );
            assert!(!std::path::Path::new(&target).exists());
        }

        #[tokio::test]
        async fn landlock_allows_write_inside_workspace() {
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            if !landlock_available(&fence(ws.to_str().unwrap(), &[])) {
                eprintln!("skipping: Landlock unavailable");
                return;
            }
            let runner = LandlockRunner::default();
            let file = ws.join("f");
            let req = ProcessRequest {
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    format!("echo x > '{}'", file.display()),
                ],
                fence: Some(fence(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            let out = runner.run(&req).await.unwrap();
            assert!(
                out.success(),
                "write inside workspace must be allowed: {}",
                out.stderr_string()
            );
            assert!(file.exists());
        }
    }

    #[cfg(target_os = "windows")]
    mod appcontainer {
        use super::*;

        fn fence(workspace_root: &str, allowed: &[&str]) -> WriteFence {
            WriteFence {
                workspace_root: std::path::PathBuf::from(workspace_root),
                allowed_paths: allowed.iter().map(std::path::PathBuf::from).collect(),
            }
        }

        fn win_argv(cmd: &str) -> ProcessRequest {
            ProcessRequest {
                argv: vec!["cmd".to_string(), "/C".to_string(), cmd.to_string()],
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            }
        }

        #[test]
        fn prepare_passes_through_without_fence() {
            let runner = WindowsAppContainerRunner::default();
            let req = win_argv("echo hi");
            // No fence on the request => delegate unfenced (Ok(None)), never
            // a Sandbox error.
            assert!(runner.prepare(&req).unwrap().is_none());
        }

        #[test]
        fn prepare_fails_closed_when_appcontainer_unavailable() {
            let runner = WindowsAppContainerRunner::default();
            let req = ProcessRequest {
                fence: Some(fence(r"Z:\syscity_nonexistent\ws", &[])),
                ..win_argv("echo hi")
            };
            // The fence is requested; the runner must never fall back to an
            // unfenced run. Either AppContainer is unavailable (Sandbox
            // error) or, on a machine that has it, the ACL grant on a
            // nonexistent Z: drive fails.
            assert!(runner.prepare(&req).is_err());
        }

        #[tokio::test]
        async fn fence_allows_write_inside_workspace() {
            if !win_appcontainer::available() {
                eprintln!("skipping: AppContainer unavailable");
                return;
            }
            let runner = WindowsAppContainerRunner::default();
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            let file = ws.join("f");
            let req = ProcessRequest {
                argv: vec![
                    "cmd".to_string(),
                    "/C".to_string(),
                    format!("echo x > \"{}\"", file.display()),
                ],
                fence: Some(fence(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            let out = runner.run(&req).await.unwrap();
            assert!(
                out.success(),
                "write inside workspace must be allowed: {}",
                out.stderr_string()
            );
            assert!(file.exists());
        }

        #[tokio::test]
        async fn fence_blocks_write_outside_workspace() {
            if !win_appcontainer::available() {
                eprintln!("skipping: AppContainer unavailable");
                return;
            }
            let runner = WindowsAppContainerRunner::default();
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            let target =
                std::env::temp_dir().join(format!("syscity_fence_{}.txt", std::process::id()));
            let _ = std::fs::remove_file(&target);
            let req = ProcessRequest {
                argv: vec![
                    "cmd".to_string(),
                    "/C".to_string(),
                    format!("echo x > \"{}\"", target.display()),
                ],
                fence: Some(fence(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            let out = runner.run(&req).await.unwrap();
            assert!(
                !out.success(),
                "write outside workspace must be denied: {}",
                out.stderr_string()
            );
            assert!(!target.exists());
        }

        #[tokio::test]
        async fn run_collect_timeout_kills_job_tree() {
            if !win_appcontainer::available() {
                eprintln!("skipping: AppContainer unavailable");
                return;
            }
            let runner = WindowsAppContainerRunner::default();
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            let req = ProcessRequest {
                argv: vec![
                    "cmd".to_string(),
                    "/C".to_string(),
                    "echo partial-out & timeout /t 5".to_string(),
                ],
                fence: Some(fence(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_millis(300)),
                ..Default::default()
            };
            let started = std::time::Instant::now();
            let out = runner.run_collect(&req).await.unwrap();
            assert!(out.timed_out, "run_collect should report the timeout");
            assert!(
                out.stdout_string().contains("partial-out"),
                "partial output must survive the timeout: {:?}",
                out.stdout_string()
            );
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "timeout must not wait for the orphaned child"
            );
        }

        #[tokio::test]
        async fn spawn_delegates_without_fence() {
            let runner = WindowsAppContainerRunner::default();
            let mut child = runner.spawn(&win_argv("echo hi")).await.unwrap();
            assert!(child.id().is_some());
            let status = child.wait().await.unwrap();
            assert!(status.success());
        }

        #[tokio::test]
        async fn spawn_returns_fenced_child_when_requested() {
            if !win_appcontainer::available() {
                eprintln!("skipping: AppContainer unavailable");
                return;
            }
            let runner = WindowsAppContainerRunner::default();
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            let req = ProcessRequest {
                fence: Some(fence(ws.to_str().unwrap(), &[])),
                ..win_argv("echo hi")
            };
            let mut child = runner.spawn(&req).await.unwrap();
            assert!(child.id().is_some());
            let status = child.wait().await.unwrap();
            assert!(status.success());
        }
    }
}
