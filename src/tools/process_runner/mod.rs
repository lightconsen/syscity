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
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::io::AsyncRead;
use tokio::process::{Child, Command};

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

/// When to build the Linux namespace view (read-only root, private `/tmp`)
/// around a fenced command, in addition to the Landlock write rules and the
/// seccomp escape-vector deny list.
///
/// Serialized into `[security]` and per-agent config; carried on
/// [`WriteFence`] so the per-call fence builder can read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NamespacePosture {
    /// Build the view when the kernel allows unprivileged user namespaces;
    /// degrade to the rules alone (with a `warn!`) when it does not — a
    /// container or a hardened sysctl is an environment fact, not an attack.
    /// The default.
    #[default]
    Auto,
    /// Never build the view. Landlock and seccomp still apply; only the
    /// read-only root and the private `/tmp` are given up. For deployments
    /// where the view gets in the way (shared bind mounts, `/tmp` handoffs).
    Off,
    /// Refuse to run the command at all when the view cannot be built —
    /// fail closed rather than run with less than the deployment asked for.
    Require,
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
    /// Whether the Linux runner builds a private filesystem view (read-only
    /// root, private `/tmp`) around the command. Linux-only; every other
    /// runner ignores it. See [`NamespacePosture`].
    pub namespaces: NamespacePosture,
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
        namespaces: NamespacePosture,
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
            namespaces,
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

#[cfg(target_os = "windows")]
mod appcontainer;
#[cfg(target_os = "linux")]
mod landlock;
#[cfg(any(target_os = "android", target_os = "ios"))]
mod mobile;
#[cfg(target_os = "linux")]
mod namespaces;
#[cfg(target_os = "macos")]
mod seatbelt;
#[cfg(any(target_os = "linux", test))]
mod seccomp;

#[cfg(test)]
mod tests;

#[cfg(all(target_os = "windows", not(mobile_os)))]
use appcontainer::WindowsAppContainerRunner;
#[cfg(all(target_os = "linux", not(mobile_os)))]
use landlock::LandlockRunner;
#[cfg(target_os = "android")]
use mobile::AndroidShellRunner;
#[cfg(target_os = "ios")]
use mobile::IosProcessRunner;
#[cfg(all(target_os = "macos", not(mobile_os)))]
use seatbelt::MacSeatbeltRunner;

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
