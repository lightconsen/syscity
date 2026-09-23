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

/// The `[security] fence_network` seccomp filter: the program builder here is
/// platform-free so it can be simulated by tests, while only the installer is
/// Linux-only. `cfg`-gated to Linux *and* tests so the program stays
/// verifiable on the host it is authored on.
#[cfg(any(target_os = "linux", test))]
mod seccomp {
    /// One classic-BPF instruction, in the shape the kernel takes but free of
    /// libc types so the program can be built and simulated on any platform.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct BpfInsn {
        pub(super) code: u16,
        pub(super) jt: u8,
        pub(super) jf: u8,
        pub(super) k: u32,
    }

    // Classic-BPF encoding (linux/bpf_common.h, linux/filter.h).
    pub(super) const BPF_LD: u16 = 0x00;
    pub(super) const BPF_W: u16 = 0x00;
    pub(super) const BPF_ABS: u16 = 0x20;
    pub(super) const BPF_JMP: u16 = 0x05;
    pub(super) const BPF_JEQ: u16 = 0x10;
    pub(super) const BPF_K: u16 = 0x00;
    pub(super) const BPF_RET: u16 = 0x06;
    // seccomp return values (linux/seccomp.h).
    pub(super) const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    pub(super) const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    pub(super) const SECCOMP_EPERM: u32 = 1;
    // struct seccomp_data offsets: nr at 0, arch at 4, args[0] at 16.
    pub(super) const SECCOMP_OFF_NR: u32 = 0;
    pub(super) const SECCOMP_OFF_ARCH: u32 = 4;
    pub(super) const SECCOMP_OFF_ARG0: u32 = 16;
    pub(super) const AF_INET_NR: u32 = 2;
    pub(super) const AF_INET6_NR: u32 = 10;

    /// The `[security] fence_network` filter, as an instruction list.
    ///
    /// Denies `socket(2)` for `AF_INET`/`AF_INET6` with `EPERM` and allows
    /// everything else — in particular `AF_UNIX`, because unix sockets are how a
    /// great deal of ordinary local tooling talks (dbus, systemd, docker) and a
    /// network posture is not a reason to break them. Denying the socket's
    /// creation is enough on its own: the child inherits only its stdio
    /// descriptors, so there is no pre-existing internet socket to fall back on.
    ///
    /// `audit_arch` and `nr_socket` are parameters because both are
    /// architecture-specific and the filter validates them: a program built for
    /// one architecture must not be interpreted as another.
    pub(super) fn network_filter_program(audit_arch: u32, nr_socket: u32) -> Vec<BpfInsn> {
        let stmt = |code: u16, jt: u8, jf: u8, k: u32| BpfInsn { code, jt, jf, k };
        // Jump offsets resolved by hand against this exact list:
        //   arch != ours                   -> allow
        //   nr != socket                   -> allow
        //   domain == AF_INET || AF_INET6  -> EPERM
        //   otherwise                      -> allow
        vec![
            stmt(BPF_LD | BPF_W | BPF_ABS, 0, 0, SECCOMP_OFF_ARCH),
            // wrong architecture: not our syscall numbers, so allow — six slots
            // down, landing on the final allow (not the EPERM before it)
            stmt(BPF_JMP | BPF_JEQ | BPF_K, 0, 6, audit_arch),
            stmt(BPF_LD | BPF_W | BPF_ABS, 0, 0, SECCOMP_OFF_NR),
            // not socket(2): allow, four slots down
            stmt(BPF_JMP | BPF_JEQ | BPF_K, 0, 4, nr_socket),
            stmt(BPF_LD | BPF_W | BPF_ABS, 0, 0, SECCOMP_OFF_ARG0),
            // AF_INET: skip the AF_INET6 test, landing on the EPERM
            stmt(BPF_JMP | BPF_JEQ | BPF_K, 1, 0, AF_INET_NR),
            // AF_INET6: EPERM; anything else: the allow below
            stmt(BPF_JMP | BPF_JEQ | BPF_K, 0, 1, AF_INET6_NR),
            stmt(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ERRNO | SECCOMP_EPERM),
            stmt(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW),
        ]
    }

    /// `(AUDIT_ARCH, __NR_socket)` for a target architecture, or `None` when the
    /// filter has no numbers for it.
    ///
    /// A function rather than a `cfg`-gated constant so every entry can be
    /// asserted from any host: the architecture this runs on and the architecture
    /// the test runs on are rarely the same machine.
    pub(super) fn seccomp_constants(arch: &str) -> Option<(u32, u32)> {
        match arch {
            // EM_X86_64 (62) | __AUDIT_ARCH_64BIT | __AUDIT_ARCH_LE, and
            // `__NR_socket` from asm/unistd_64.h.
            "x86_64" => Some((0xC000_003E, 41)),
            // EM_AARCH64 (183) | the same two flags, and `__NR_socket` from
            // asm-generic/unistd.h.
            "aarch64" => Some((0xC000_00B7, 198)),
            _ => None,
        }
    }

    // The escape-vector filter's return errnos. EACCES, not EPERM: the
    // escalation classifier (`escalation::fence_denial_reason`) deliberately
    // treats "Operation not permitted" as *the fence refused, offer to re-run
    // outside it* — the right recovery for a blocked network connect, but the
    // wrong recovery for `ptrace`/`mount`, which are never legitimate work for
    // a fenced command and must stay silently refused. EACCES ("Permission
    // denied") is unclassified for exactly that reason (see the Landlock note
    // there). ENOSYS for `clone3` makes libc fall back to filterable `clone`.
    pub(super) const SECCOMP_EACCES: u32 = 13;
    pub(super) const SECCOMP_ENOSYS: u32 = 38;
    // Classic-BPF jump opcodes (linux/bpf_common.h): the op field is bits
    // 4-6 (0x70) — JEQ 0x10, JSET 0x40 — and bit 3 (0x08) is the K/X source
    // selector, not an opcode.
    pub(super) const BPF_JSET: u16 = 0x40;

    /// Every syscall number the escape filter needs, per architecture.
    ///
    /// Hardcoded for the same reason [`seccomp_constants`] is: the numbers must
    /// be visible to the simulator on the host the tests run on. On Linux a
    /// contract test (`escape_syscall_table_matches_libc`) asserts every entry
    /// against the `libc::SYS_*` constant of the same name, so a typo here
    /// fails CI rather than silently denying the wrong syscall.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct EscapeTable {
        pub(super) audit_arch: u32,
        /// `(name, __NR_*)` pairs denied with EACCES.
        pub(super) denied: &'static [(&'static str, u32)],
        pub(super) nr_clone: u32,
        pub(super) nr_clone3: u32,
    }

    /// The one mask that means "this clone is a namespace escape attempt".
    ///
    /// `NEWNS | NEWCGROUP | NEWUTS | NEWIPC | NEWUSER | NEWPID | NEWNET`
    /// (linux/sched.h) — values are architecture-independent.
    pub(super) const CLONE_NEW_MASK: u32 = 0x7E02_0000;

    /// `None` on architectures the filter has no numbers for — the installer
    /// fails closed rather than guess.
    pub(super) fn escape_constants(arch: &str) -> Option<EscapeTable> {
        match arch {
            // asm/unistd_64.h. `ioperm`/`iopl` are x86-only — no I/O ports on
            // ARM64, so they are simply absent from this table.
            "x86_64" => Some(EscapeTable {
                audit_arch: 0xC000_003E,
                denied: &[
                    ("ptrace", 101),
                    ("process_vm_readv", 310),
                    ("process_vm_writev", 311),
                    ("kcmp", 312),
                    ("process_madvise", 440),
                    ("bpf", 321),
                    ("perf_event_open", 298),
                    ("userfaultfd", 323),
                    ("kexec_load", 246),
                    ("kexec_file_load", 320),
                    ("open_by_handle_at", 304),
                    ("name_to_handle_at", 303),
                    ("lookup_dcookie", 212),
                    ("ioperm", 173),
                    ("iopl", 172),
                    ("swapon", 167),
                    ("swapoff", 168),
                    ("quotactl", 179),
                    ("acct", 163),
                    ("reboot", 169),
                    ("keyctl", 250),
                    ("add_key", 248),
                    ("request_key", 249),
                    ("init_module", 175),
                    ("finit_module", 313),
                    ("delete_module", 176),
                    ("mount", 165),
                    ("umount2", 166),
                    ("pivot_root", 155),
                    ("unshare", 272),
                    ("setns", 308),
                    ("fsopen", 430),
                    ("fsconfig", 431),
                    ("fsmount", 432),
                    ("fspick", 433),
                    ("move_mount", 429),
                    ("open_tree", 428),
                    ("mount_setattr", 442),
                    ("remap_file_pages", 216),
                ],
                nr_clone: 56,
                nr_clone3: 435,
            }),
            // asm-generic/unistd.h.
            "aarch64" => Some(EscapeTable {
                audit_arch: 0xC000_00B7,
                denied: &[
                    ("ptrace", 117),
                    ("process_vm_readv", 270),
                    ("process_vm_writev", 271),
                    ("kcmp", 272),
                    ("process_madvise", 440),
                    ("bpf", 280),
                    ("perf_event_open", 241),
                    ("userfaultfd", 282),
                    ("kexec_load", 104),
                    ("kexec_file_load", 294),
                    ("open_by_handle_at", 265),
                    ("name_to_handle_at", 264),
                    ("lookup_dcookie", 18),
                    ("swapon", 224),
                    ("swapoff", 225),
                    ("quotactl", 60),
                    ("acct", 89),
                    ("reboot", 142),
                    ("keyctl", 219),
                    ("add_key", 217),
                    ("request_key", 218),
                    ("init_module", 105),
                    ("finit_module", 273),
                    ("delete_module", 106),
                    ("mount", 40),
                    ("umount2", 39),
                    ("pivot_root", 41),
                    ("unshare", 97),
                    ("setns", 268),
                    ("fsopen", 430),
                    ("fsconfig", 431),
                    ("fsmount", 432),
                    ("fspick", 433),
                    ("move_mount", 429),
                    ("open_tree", 428),
                    ("mount_setattr", 442),
                    ("remap_file_pages", 234),
                ],
                nr_clone: 220,
                nr_clone3: 435,
            }),
            _ => None,
        }
    }

    /// Named fixup targets for the two-pass assembler below. Jump offsets in a
    /// ~50-instruction deny chain are unmanageable by hand — each conditional
    /// jump names the label it jumps to and the builder patches real offsets at
    /// `resolve`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Label {
        DenyEacces,
        DenyEnosys,
        /// Wrong-architecture lane.
        Allow,
        /// A syscall that matched no chain entry.
        ChainAllow,
        /// A `clone` whose flags carry no `CLONE_NEW*` bit.
        FlagAllow,
        CloneFlags,
    }

    /// Tiny classic-BPF assembler: emit instructions, name forward jumps,
    /// resolve on build. Classic-BPF jump offsets are unsigned — every jump
    /// must land *after* its source — so the program below is laid out with
    /// its decision points behind the chain, and `resolve` fails loudly on a
    /// distance that does not fit.
    struct ProgramBuilder {
        program: Vec<BpfInsn>,
        jt_fixups: Vec<(usize, Label)>,
        jf_fixups: Vec<(usize, Label)>,
        labels: Vec<(Label, usize)>,
    }

    impl ProgramBuilder {
        fn new() -> Self {
            Self {
                program: Vec::new(),
                jt_fixups: Vec::new(),
                jf_fixups: Vec::new(),
                labels: Vec::new(),
            }
        }

        fn emit(&mut self, code: u16, jt: u8, jf: u8, k: u32) -> usize {
            self.program.push(BpfInsn { code, jt, jf, k });
            self.program.len() - 1
        }

        fn mark(&mut self, label: Label) {
            self.labels.push((label, self.program.len()));
        }

        /// `JEQ K`: jump to `target` when the accumulator equals `k`, fall
        /// through to the next instruction otherwise.
        fn jeq(&mut self, k: u32, target: Label) {
            let at = self.emit(BPF_JMP | BPF_JEQ | BPF_K, 0, 0, k);
            self.jt_fixups.push((at, target));
        }

        /// `JEQ K` that falls through when equal and jumps when not — the
        /// architecture gate, whose mismatch lane is the exception.
        fn jeq_else(&mut self, k: u32, not_equal: Label) {
            let at = self.emit(BPF_JMP | BPF_JEQ | BPF_K, 0, 0, k);
            self.jf_fixups.push((at, not_equal));
        }

        /// `JSET K`: jump to `hit` when `accumulator & k` is non-zero, jump to
        /// `miss` otherwise.
        fn jset(&mut self, k: u32, hit: Label, miss: Label) {
            let at = self.emit(BPF_JMP | BPF_JSET | BPF_K, 0, 0, k);
            self.jt_fixups.push((at, hit));
            self.jf_fixups.push((at, miss));
        }

        /// Position of a marked label. Every label used by a jump is marked
        /// before `resolve`; an unmarked one is an assembler bug the
        /// simulator tests catch, so panicking is the correct behavior.
        #[allow(clippy::expect_used)] // assembler invariant, not input validation
        fn position(&self, label: Label) -> usize {
            self.labels
                .iter()
                .find(|(l, _)| *l == label)
                .map(|(_, at)| *at)
                .expect("label marked")
        }

        /// Classic-BPF jump offsets are single bytes; a distance that does
        /// not fit means the program layout grew past what one filter can
        /// express, and panicking here is the point (the tests would catch
        /// it long before a kernel sees the program).
        #[allow(clippy::expect_used)] // layout bug, not input validation
        fn resolve(mut self) -> Vec<BpfInsn> {
            let jt_fixups = std::mem::take(&mut self.jt_fixups);
            let jf_fixups = std::mem::take(&mut self.jf_fixups);
            for (at, label) in jt_fixups {
                let off = u8::try_from(self.position(label) - at - 1)
                    .expect("jump distance exceeds one byte");
                self.program[at].jt = off;
            }
            for (at, label) in jf_fixups {
                let off = u8::try_from(self.position(label) - at - 1)
                    .expect("jump distance exceeds one byte");
                self.program[at].jf = off;
            }
            self.program
        }
    }

    /// The escape-vector filter, as an instruction list.
    ///
    /// Architecture gate → linear syscall chain (one `JEQ` per denied call,
    /// hit = EACCES) → allow. `clone` is pulled out of the chain: its denial
    /// is conditional on carrying a `CLONE_NEW*` flag, checked with `JSET` on
    /// `args[0]`; `clone3` is denied with `ENOSYS` outright so libc falls back
    /// to a `clone` whose flags this filter can actually see. Ordinary work —
    /// `execve`, `openat`, an un-namespaced `clone` — passes untouched: this
    /// is an escape-vector deny list, not a jail. (There are three allow
    /// returns rather than one because classic-BPF jumps only go forward.)
    pub(super) fn escape_filter_program(table: &EscapeTable) -> Vec<BpfInsn> {
        let mut b = ProgramBuilder::new();
        // Load and check the audit architecture first: a program built for one
        // architecture must not deny another's syscalls (same argument as
        // [`network_filter_program`]; at runtime the numbers come from
        // `std::env::consts::ARCH`, so the mismatch lane is unreachable and
        // errs toward allowing).
        b.emit(BPF_LD | BPF_W | BPF_ABS, 0, 0, SECCOMP_OFF_ARCH);
        b.jeq_else(table.audit_arch, Label::Allow);
        b.emit(BPF_LD | BPF_W | BPF_ABS, 0, 0, SECCOMP_OFF_NR);
        for (_, nr) in table.denied {
            b.jeq(*nr, Label::DenyEacces);
        }
        b.jeq(table.nr_clone3, Label::DenyEnosys);
        b.jeq(table.nr_clone, Label::CloneFlags);
        // No chain entry matched: an ordinary syscall.
        b.mark(Label::ChainAllow);
        b.emit(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW);
        b.mark(Label::CloneFlags);
        // clone: the decision rides on the flags, args[0] in seccomp_data.
        b.emit(BPF_LD | BPF_W | BPF_ABS, 0, 0, SECCOMP_OFF_ARG0);
        b.jset(CLONE_NEW_MASK, Label::DenyEacces, Label::FlagAllow);
        b.mark(Label::DenyEacces);
        b.emit(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ERRNO | SECCOMP_EACCES);
        b.mark(Label::DenyEnosys);
        b.emit(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ERRNO | SECCOMP_ENOSYS);
        b.mark(Label::Allow);
        b.emit(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW);
        b.mark(Label::FlagAllow);
        b.emit(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW);
        b.resolve()
    }

    /// Walk the program the way the kernel would, for one syscall.
    ///
    /// Test-only: jump arithmetic is the easiest thing in this file to get
    /// wrong by one slot, so the programs are interpreted here rather than
    /// trusted. Lives in this module (not in a per-platform test module) so
    /// both the network and the escape program can be simulated on any host.
    #[cfg(test)]
    pub(super) fn simulate(prog: &[BpfInsn], arch: u32, nr: u32, arg0: u32) -> u32 {
        let mut a = 0u32;
        let mut pc = 0usize;
        loop {
            let insn = prog[pc];
            match insn.code & 0x07 {
                0x00 => {
                    a = match insn.k {
                        SECCOMP_OFF_ARCH => arch,
                        SECCOMP_OFF_NR => nr,
                        SECCOMP_OFF_ARG0 => arg0,
                        other => panic!("unexpected load offset {other}"),
                    };
                    pc += 1;
                }
                0x05 => {
                    // The op field is bits 4-6: JEQ (0x10) and JSET (0x40)
                    // both live in the JMP class and differ by op bits.
                    let taken = match insn.code & 0x70 {
                        BPF_JSET => a & insn.k != 0,
                        // BPF_JEQ (and every other jump, which the programs
                        // here do not emit).
                        _ => a == insn.k,
                    };
                    pc += 1 + if taken {
                        insn.jt as usize
                    } else {
                        insn.jf as usize
                    };
                }
                0x06 => return insn.k,
                other => panic!("unexpected instruction class {other}"),
            }
        }
    }
}

/// Linux: install [`network_filter_program`] on the calling thread.
///
/// Runs in the child after the Landlock restrict, which has already set
/// `no_new_privs` — the kernel refuses a filter without it.
#[cfg(target_os = "linux")]
fn install_network_seccomp() -> std::io::Result<()> {
    // Fail closed rather than install a filter whose syscall numbers are
    // guesses: a network posture that silently does nothing is worse than one
    // that refuses to start. `consts::ARCH` is the compile-time target, so
    // this cannot mismatch the binary.
    let Some((audit_arch, nr_socket)) = seccomp::seccomp_constants(std::env::consts::ARCH) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no seccomp network filter for this architecture",
        ));
    };
    install_seccomp_program(&seccomp::network_filter_program(audit_arch, nr_socket))
}

/// Linux: install [`escape_filter_program`] on the calling thread.
///
/// Unlike the network posture this is not optional: every fenced run gets the
/// escape-vector deny list, because `ptrace` and friends are not work a fenced
/// command has any legitimate use for. Unknown architectures fail closed,
/// matching the network filter.
#[cfg(target_os = "linux")]
fn install_escape_seccomp() -> std::io::Result<()> {
    let Some(table) = seccomp::escape_constants(std::env::consts::ARCH) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no seccomp escape filter for this architecture",
        ));
    };
    install_seccomp_program(&seccomp::escape_filter_program(&table))
}

/// Convert a `BpfInsn` program to the kernel's `sock_fprog` shape and load it
/// with `seccomp(SECCOMP_SET_MODE_FILTER)`.
#[cfg(target_os = "linux")]
fn install_seccomp_program(program: &[seccomp::BpfInsn]) -> std::io::Result<()> {
    use libc::{sock_filter, sock_fprog};

    const SECCOMP_SET_MODE_FILTER: libc::c_uint = 1;

    {
        let filter: Vec<sock_filter> = program
            .iter()
            .map(|i| sock_filter {
                code: i.code,
                jt: i.jt,
                jf: i.jf,
                k: i.k,
            })
            .collect();
        let mut prog = sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_ptr() as *mut sock_filter,
        };

        // `no_new_privs` is set by the Landlock restrict before this runs;
        // repeating it is idempotent and keeps the filter installable if the
        // order ever changes.
        #[allow(unsafe_code)] // prctl has no safe wrapper in libc; no memory is touched
        let prctl_rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if prctl_rc != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: `prog` points at `filter`, which outlives this call, and the
        // kernel copies the program during the syscall — nothing is retained.
        // Variadic args are widened explicitly (see the mount_setattr note).
        #[allow(unsafe_code)]
        let rc = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                SECCOMP_SET_MODE_FILTER as libc::c_long,
                0u64,
                &mut prog as *mut sock_fprog,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
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
        let deny_network = fence.deny_network;
        let namespaces_posture = fence.namespaces;
        fenced.fence = None;
        let fence = fence.clone();
        let existing_pre = req.pre_exec.clone();
        fenced.pre_exec = Some(Arc::new(move || {
            if let Some(pre) = &existing_pre {
                pre()?;
            }
            // The view first: it needs capabilities in a fresh user namespace
            // and mount writes, both of which are gone once Landlock
            // restricts. Its /proc writes (the identity maps) likewise must
            // precede the restriction, which handles file writes.
            namespaces::setup(namespaces_posture, &fence)?;
            landlock_restrict(&fence)?;
            // The escape-vector deny list rides along on every fenced run:
            // Landlock fences writes but says nothing about `ptrace`, and a
            // "seccomp only when denying network" rule would leave the whole
            // process-memory attack surface open in the default posture.
            install_escape_seccomp()?;
            if deny_network {
                // After Landlock: the restrict is what sets `no_new_privs`,
                // which the kernel requires before it will take a filter.
                install_network_seccomp()?;
            }
            Ok(())
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
    // `/dev/null` is granted exactly the way the Seatbelt profile allows it
    // literally: the ubiquitous `2>/dev/null` idiom is ordinary work, not a
    // fence escape (a bit bucket accepts nothing but bits). The namespace
    // view, when present, re-binds `/dev` read-write on top of the read-only
    // root so the mount layer allows what Landlock grants here.
    let fd = PathFd::new("/dev/null")
        .map_err(|e| std::io::Error::other(format!("cannot open /dev/null: {e}")))?;
    let rule = PathBeneath::new(fd, landlock_write_rights());
    ruleset = ruleset
        .add_rule(rule)
        .map_err(|e| std::io::Error::other(format!("landlock rule for /dev/null: {e}")))?;
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

/// Linux: build a private filesystem view around a fenced command — a
/// read-only root with the working trees re-bound on top — and deny the
/// syscalls that could escape it.
///
/// Three layers now protect a fenced run, strongest first:
///
/// 1. **The view** (this module, [`NamespacePosture`]-gated). An unprivileged
///    user namespace plus a private mount namespace: the root is re-bound and
///    marked read-only recursively, `/tmp` is covered with a fresh tmpfs (no
///    more reading other sessions' leftovers — the fence has no read rules,
///    so a shared `/tmp` was an information leak), and the working trees plus
///    `/dev`, `/run` and `/proc` are re-bound read-write on top. The IPC
///    re-binds matter: an `AF_UNIX` connect needs write access to the socket
///    file, so a wholesale read-only root would break dbus/systemd/docker,
///    which the network posture deliberately leaves alone. There is
///    deliberately **no PID namespace**: `pre_exec` runs once between fork
///    and exec, and `unshare(CLONE_NEWPID)` only affects the *next* fork —
///    putting the command itself into a new PID namespace would take a second
///    fork (a sandbox-binary shape) and break the runner's wait/kill
///    semantics. Same-uid `/proc` entries therefore stay visible; their
///    memory does not (the deny list below).
/// 2. **Landlock** (`landlock_restrict`): the write rules, unchanged.
/// 3. **seccomp** (`install_escape_seccomp`): the escape-vector deny list.
///
/// Failures degrade per [`NamespacePosture`]: `Off` skips the view entirely,
/// `Auto` warns and continues with layers 2–3 (the common failure is
/// unprivileged user namespaces switched off — a container or a hardened
/// sysctl is an environment fact, not an attack), and `Require` fails the
/// spawn. A failure *while* the view is half-built rolls the mounts back; if
/// even the rollback fails the spawn fails closed rather than exec into a
/// mangled filesystem.
#[cfg(target_os = "linux")]
mod namespaces {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::RawFd;
    use std::path::Path;

    use super::{NamespacePosture, WriteFence};

    /// fcntl.h: apply `mount_setattr` to the whole mount subtree. Not yet in
    /// libc; a local constant keeps the raw call honest, same practice as the
    /// local `SECCOMP_SET_MODE_FILTER`. Needs kernel ≥ 5.12.
    const AT_RECURSIVE: libc::c_uint = 0x8000;

    /// Why the view could not be built, and how bad that is.
    enum BuildError {
        /// Nothing (or a fully torn-down view) is left mounted; the shared
        /// host view is intact and the run can safely continue without it.
        Failed(io::Error),
        /// A half-built view could not be torn down — the process must not
        /// exec into it, whatever the posture.
        Mangled(io::Error),
    }

    /// Entry point from the runner's `pre_exec` hook. See the module docs for
    /// the posture semantics.
    pub(super) fn setup(posture: NamespacePosture, fence: &WriteFence) -> io::Result<()> {
        if posture == NamespacePosture::Off {
            return Ok(());
        }
        match build_view(fence) {
            Ok(()) => Ok(()),
            Err(BuildError::Mangled(e)) => Err(e),
            Err(BuildError::Failed(e)) => {
                if posture == NamespacePosture::Require {
                    return Err(e);
                }
                tracing::warn!(
                    error = %e,
                    "namespace view unavailable; continuing with Landlock + seccomp only"
                );
                Ok(())
            }
        }
    }

    /// A tree to re-bind read-write once the root is read-only, held open so
    /// the read-only flip and the `/tmp` cover cannot hide it.
    struct Rebind {
        target: &'static Path,
        /// O_PATH fd taken before any mount changed. The bind source is
        /// `/proc/self/fd/<fd>`, which `/proc` — itself re-bound RW below —
        /// serves.
        fd: RawFd,
    }

    /// A working tree (workspace root, allowed path, cwd) to re-bind onto its
    /// own absolute path once `/tmp` is covered.
    struct WorkingRebind {
        target: std::path::PathBuf,
        fd: RawFd,
    }

    fn build_view(fence: &WriteFence) -> Result<(), BuildError> {
        // Open everything that must survive the read-only flip or the /tmp
        // cover *before* any mount changes.
        let mut working: Vec<WorkingRebind> = Vec::new();
        push_unique(&mut working, fence.workspace_root.clone()).map_err(BuildError::Failed)?;
        for allowed in &fence.allowed_paths {
            push_unique(&mut working, allowed.clone()).map_err(BuildError::Failed)?;
        }
        if let Ok(cwd) = std::env::current_dir() {
            push_unique(&mut working, cwd).map_err(BuildError::Failed)?;
        }
        let ipc: Vec<Rebind> = ["/dev", "/run", "/proc"]
            .iter()
            .map(|t| open_o_path(Path::new(t)).map(|fd| Rebind { target: Path::new(t), fd }))
            .collect::<io::Result<_>>()
            .map_err(BuildError::Failed)?;

        // ②③ The namespaces themselves. Both fail before anything is mounted,
        // so they are clean degradations (the common one: unprivileged user
        // namespaces switched off).
        user_namespace().map_err(BuildError::Failed)?;
        mount_namespace().map_err(BuildError::Failed)?;

        // Past this point we are inside our own mount namespace and a failure
        // must not leave a half-built view behind to exec into.
        let mut stacked = false;
        let outcome = (|| -> io::Result<()> {
            // ④ Read-only root. Kernel ≥ 5.12 for the recursive setattr; on
            // older kernels this degrades to the plain copy and Landlock
            // carries the write fence alone.
            match root_read_only() {
                Ok(()) => stacked = true,
                Err(e) => tracing::warn!(
                    error = %e,
                    "read-only root unavailable; relying on Landlock for outside writes"
                ),
            }
            // ⑤ IPC trees back on top, read-write.
            for h in &ipc {
                bind_back(h.target, h.fd).map_err(|e| {
                    io::Error::other(format!("re-bind {}: {e}", h.target.display()))
                })?;
            }
            // ⑥ Private /tmp, then the working trees back onto their own
            // paths — a workspace under /tmp survives the cover.
            private_tmp().map_err(|e| io::Error::other(format!("private /tmp: {e}")))?;
            for w in &working {
                bind_back(&w.target, w.fd).map_err(|e| {
                    io::Error::other(format!("re-bind {}: {e}", w.target.display()))
                })?;
            }
            Ok(())
        })();
        match outcome {
            Ok(()) => Ok(()),
            // Nothing was stacked at "/": the shared view is untouched.
            Err(e) if !stacked => Err(BuildError::Failed(e)),
            Err(e) => match umount_slash() {
                Ok(()) => Err(BuildError::Failed(e)),
                Err(rb) => {
                    tracing::warn!(error = %rb, "namespace view rollback failed");
                    Err(BuildError::Mangled(e))
                }
            },
        }
    }

    /// Hold one O_PATH reference per distinct tree; a path already held is
    /// kept (binding the same tree twice is pointless, and two fds to one
    /// tree are wasteful).
    fn push_unique(list: &mut Vec<WorkingRebind>, target: std::path::PathBuf) -> io::Result<()> {
        if target == Path::new("/") {
            // A workspace/cwd of `/` would re-bind the whole tree read-write
            // on top of the read-only root, undoing it. Skip the re-bind —
            // Landlock still fences writes, and `/` is not a workspace.
            tracing::warn!("workspace or cwd is `/`; skipping its view re-bind");
            return Ok(());
        }
        if list.iter().any(|w| w.target == target) {
            return Ok(());
        }
        let fd = open_o_path(&target)?;
        list.push(WorkingRebind { target, fd });
        Ok(())
    }

    /// `unshare(CLONE_NEWUSER)` plus the single-identity map that gives us
    /// capabilities in the new namespace (and nobody else anything).
    fn user_namespace() -> io::Result<()> {
        // Read the identity BEFORE the unshare: inside the new (empty)
        // namespace `getuid()` reports the overflow uid (65534), and a map
        // written from that reading would be refused.
        // SAFETY: plain uid/gid reads; libc marks them unsafe (they can be
        // overridden), but they touch no memory.
        #[allow(unsafe_code)]
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        // SAFETY: `unshare` has no invariant beyond the flags; on failure it
        // leaves the caller untouched.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::unshare(libc::CLONE_NEWUSER) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        write_id_maps(uid, gid)
    }

    /// Map our own uid/gid 1:1 — an unprivileged process may map exactly its
    /// own identity, which is all the view needs (files we own stay ours;
    /// everyone else's stay inaccessible).
    fn write_id_maps(uid: u32, gid: u32) -> io::Result<()> {
        // The kernel requires `setgroups` to be denied before an unprivileged
        // gid_map write; the file is absent on kernels old enough that the
        // restriction predates it.
        if let Err(e) = std::fs::write("/proc/self/setgroups", "deny") {
            if e.kind() != io::ErrorKind::NotFound {
                return Err(e);
            }
        }
        std::fs::write("/proc/self/uid_map", format!("0 {uid} 1\n"))?;
        std::fs::write("/proc/self/gid_map", format!("0 {gid} 1\n"))?;
        Ok(())
    }

    /// `unshare(CLONE_NEWNS)` plus private propagation, so nothing the view
    /// mounts leaks back to the host's peer groups (or inherits their events).
    fn mount_namespace() -> io::Result<()> {
        // SAFETY: as with the user namespace — flags only, self-contained.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::unshare(libc::CLONE_NEWNS) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // Cut shared propagation so nothing the view mounts leaks back to the
        // host's peer groups. Recursive first; some container runtimes refuse
        // the recursive form over their locked submounts (EINVAL), and the
        // plain form still makes *our* mounts private — anything mounted
        // under the private root inherits it.
        // SAFETY: mount with null src/fstype/data flips propagation only.
        #[allow(unsafe_code)]
        let rc = unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                (libc::MS_REC | libc::MS_PRIVATE) as libc::c_ulong,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            // SAFETY: as above.
            #[allow(unsafe_code)]
            let plain = unsafe {
                libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_PRIVATE as libc::c_ulong,
                    std::ptr::null(),
                )
            };
            if plain != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Bind `/` onto itself recursively and mark the copy read-only.
    fn root_read_only() -> io::Result<()> {
        // SAFETY: bind of an existing path onto itself; kernel copies the
        // path strings during the call.
        #[allow(unsafe_code)]
        let rc = unsafe {
            libc::mount(
                c"/".as_ptr(),
                c"/".as_ptr(),
                std::ptr::null(),
                (libc::MS_BIND | libc::MS_REC) as libc::c_ulong,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let attrs = libc::mount_attr {
            attr_set: libc::MOUNT_ATTR_RDONLY | libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV,
            attr_clr: 0,
            propagation: 0,
            userns_fd: 0,
        };
        // SAFETY: `attr` outlives the call; the kernel copies it during the
        // syscall. Variadic args are widened to `c_long` explicitly — an int
        // left to C's default promotions leaves the register's upper half
        // undefined, and the kernel reads each syscall argument as 64 bits.
        #[allow(unsafe_code)]
        let rc = unsafe {
            libc::syscall(
                libc::SYS_mount_setattr,
                libc::AT_FDCWD as libc::c_long,
                c"/".as_ptr(),
                AT_RECURSIVE as libc::c_long,
                &attrs as *const libc::mount_attr,
                std::mem::size_of::<libc::mount_attr>(),
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Cover `/tmp` with a fresh tmpfs. `1777` keeps the sticky semantics
    /// mktemp expects; no `noexec` — extracting a binary into /tmp is
    /// ordinary tooling work.
    fn private_tmp() -> io::Result<()> {
        // SAFETY: string constants and flags; the kernel copies the data blob.
        #[allow(unsafe_code)]
        let rc = unsafe {
            libc::mount(
                c"tmpfs".as_ptr(),
                c"/tmp".as_ptr(),
                c"tmpfs".as_ptr(),
                (libc::MS_NOSUID | libc::MS_NODEV) as libc::c_ulong,
                c"mode=1777".as_ptr().cast(),
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Re-bind a pre-opened tree onto its own absolute path. Under the fresh
    /// `/tmp` the path has to be re-created first; anywhere else the
    /// read-only view preserved it — and creating it is impossible, which is
    /// the point.
    fn bind_back(target: &Path, fd: RawFd) -> io::Result<()> {
        if target.starts_with("/tmp") {
            std::fs::create_dir_all(target)?;
        }
        let src = CString::new(format!("/proc/self/fd/{fd}"))?;
        let dst = CString::new(target.as_os_str().as_bytes())?;
        // Recursive first (the tree's own submounts come along); container
        // runtimes lock their submounts and refuse the recursive form, and a
        // plain bind of the top mount still re-exposes what matters
        // (`/dev/null` lives on `/dev` itself). Submounts left behind stay on
        // the read-only layer — reads are unaffected and writes there were
        // Landlock-denied regardless.
        // SAFETY: both paths outlive the call; a plain bind, no data.
        #[allow(unsafe_code)]
        let rc = unsafe {
            libc::mount(
                src.as_ptr(),
                dst.as_ptr(),
                std::ptr::null(),
                (libc::MS_BIND | libc::MS_REC) as libc::c_ulong,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            // SAFETY: as above.
            #[allow(unsafe_code)]
            let plain = unsafe {
                libc::mount(
                    src.as_ptr(),
                    dst.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND as libc::c_ulong,
                    std::ptr::null(),
                )
            };
            if plain != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Detach whatever is stacked at `/` — with `MNT_DETACH` the read-only
    /// copy and everything mounted on top of it go together, restoring the
    /// untouched host-view copy the mount namespace started with. Harmless
    /// (EINVAL) when nothing is stacked.
    fn umount_slash() -> io::Result<()> {
        // SAFETY: a flag-only umount of a fixed path.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::umount2(c"/".as_ptr(), libc::MNT_DETACH) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn open_o_path(path: &Path) -> io::Result<RawFd> {
        let c = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: open of a caller-owned path; the fd is closed with the
        // process at exec (O_CLOEXEC) — nothing to leak or double-close.
        #[allow(unsafe_code)]
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
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

    /// The fence descriptor is platform-neutral (`WriteFence::new` computes
    /// the protected carve-outs for every runner), so its shape is asserted
    /// here rather than only in the per-platform fence modules.
    #[test]
    fn write_fence_new_computes_protected_paths() {
        let f = WriteFence::new(
            std::path::PathBuf::from("/tmp/ws"),
            vec![std::path::PathBuf::from("/tmp/extra")],
            false,
            NamespacePosture::Auto,
        );
        for root in ["/tmp/ws", "/tmp/extra"] {
            for name in PROTECTED_WRITE_NAMES {
                assert!(
                    f.protected_paths
                        .contains(&std::path::PathBuf::from(format!("{root}/{name}"))),
                    "protected path {root}/{name} missing from {:?}",
                    f.protected_paths
                );
            }
        }
        assert!(!f.deny_network, "the default posture leaves the network alone");
        assert_eq!(f.namespaces, NamespacePosture::Auto);

        let denied = WriteFence::new(
            std::path::PathBuf::from("/tmp/ws"),
            Vec::new(),
            true,
            NamespacePosture::Require,
        );
        assert!(denied.deny_network, "the requested posture is carried through");
        assert_eq!(denied.namespaces, NamespacePosture::Require);
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
    /// The seccomp filter's deny logic, simulated.
    ///
    /// Jump arithmetic is the easiest thing in this file to get wrong by one
    /// slot, and the platform it actually runs on is not the platform this
    /// test runs on — so the program is interpreted here rather than trusted.
    mod seccomp_program {
        use super::super::seccomp::{
            network_filter_program, seccomp_constants, simulate, AF_INET6_NR, AF_INET_NR,
            SECCOMP_EPERM, SECCOMP_RET_ALLOW, SECCOMP_RET_ERRNO,
        };

        const X86_64_ARCH: u32 = 0xC000_003E;
        const X86_64_SOCKET: u32 = 41;
        const NR_READ: u32 = 0;

        #[test]
        fn internet_sockets_are_denied_and_unix_sockets_are_not() {
            let prog = network_filter_program(X86_64_ARCH, X86_64_SOCKET);
            let run = |arch: u32, nr: u32, arg0: u32| simulate(&prog, arch, nr, arg0);
            let denied = SECCOMP_RET_ERRNO | SECCOMP_EPERM;

            assert_eq!(run(X86_64_ARCH, X86_64_SOCKET, AF_INET_NR), denied);
            assert_eq!(run(X86_64_ARCH, X86_64_SOCKET, AF_INET6_NR), denied);

            // A network posture is not a reason to break local IPC.
            assert_eq!(run(X86_64_ARCH, X86_64_SOCKET, 1), SECCOMP_RET_ALLOW);
            // Other syscalls are untouched — this is a network clause, not a jail.
            assert_eq!(run(X86_64_ARCH, NR_READ, AF_INET_NR), SECCOMP_RET_ALLOW);
            // A filter built for another architecture must not match ours.
            assert_eq!(run(0xDEAD_BEEF, X86_64_SOCKET, AF_INET_NR), SECCOMP_RET_ALLOW);
        }

        /// The per-architecture constants are the whole reason the filter takes
        /// them as parameters; a wrong `__NR_socket` would silently deny (or
        /// not deny) the wrong syscall. `libc` only carries the AUDIT_ARCH
        /// constants on Linux and this test only runs on macOS, so the table
        /// is asserted against its documented derivation instead.
        #[test]
        fn architecture_constants_are_the_documented_ones() {
            const EM_X86_64: u32 = 62;
            const EM_AARCH64: u32 = 183;
            const AUDIT_FLAGS: u32 = 0x8000_0000 | 0x4000_0000; // 64BIT | LE

            assert_eq!(seccomp_constants("x86_64"), Some((EM_X86_64 | AUDIT_FLAGS, 41)));
            assert_eq!(seccomp_constants("aarch64"), Some((EM_AARCH64 | AUDIT_FLAGS, 198)));
            // An architecture the filter has no numbers for must refuse to
            // install rather than guess.
            assert_eq!(seccomp_constants("riscv64"), None);
            assert_eq!(seccomp_constants(""), None);

            let (aarch64_arch, aarch64_socket) = seccomp_constants("aarch64").expect("aarch64");
            assert_eq!(X86_64_ARCH, EM_X86_64 | AUDIT_FLAGS);
            assert_eq!(X86_64_SOCKET, 41);

            let aarch64 = network_filter_program(aarch64_arch, aarch64_socket);
            assert_eq!(
                simulate(&aarch64, aarch64_arch, aarch64_socket, AF_INET_NR),
                SECCOMP_RET_ERRNO | SECCOMP_EPERM
            );
        }
    }

    /// The escape-vector filter's deny logic, simulated (see
    /// [`seccomp::simulate`] for why). Ungated on purpose: unlike the network
    /// posture — whose tests live behind the macOS-authored `seccomp_program`
    /// module — this filter rides on every fenced Linux run, so CI's ubuntu
    /// runner must exercise it too, not just the dev mac.
    mod escape_program {
        use super::super::seccomp::{
            escape_constants, escape_filter_program, simulate, EscapeTable, CLONE_NEW_MASK,
            SECCOMP_EACCES, SECCOMP_ENOSYS, SECCOMP_RET_ALLOW, SECCOMP_RET_ERRNO,
        };

        const NR_EXECVE_X86: u32 = 59;
        const NR_EXECVE_AARCH64: u32 = 221;
        const NR_OPENAT_X86: u32 = 257;
        const SIGCHLD: u32 = 17;
        const CLONE_FS: u32 = 0x0000_0200;

        fn table(arch: &str) -> EscapeTable {
            escape_constants(arch).unwrap_or_else(|| panic!("{arch} must have an escape table"))
        }

        #[test]
        fn every_deny_list_entry_is_denied_with_eacces() {
            for arch in ["x86_64", "aarch64"] {
                let table = table(arch);
                let prog = escape_filter_program(&table);
                let denied = SECCOMP_RET_ERRNO | SECCOMP_EACCES;
                for (name, nr) in table.denied {
                    assert_eq!(
                        simulate(&prog, table.audit_arch, *nr, 0),
                        denied,
                        "{arch}: {name} ({nr}) must be denied"
                    );
                }
            }
        }

        #[test]
        fn ordinary_work_passes_untouched() {
            let t = table("x86_64");
            let prog = escape_filter_program(&t);
            let run = |nr: u32, arg0: u32| simulate(&prog, t.audit_arch, nr, arg0);

            assert_eq!(run(NR_EXECVE_X86, 0), SECCOMP_RET_ALLOW);
            assert_eq!(run(NR_OPENAT_X86, 0), SECCOMP_RET_ALLOW);
            assert_eq!(run(0, 0), SECCOMP_RET_ALLOW, "read");
            // clone without any CLONE_NEW* flag is just a fork.
            assert_eq!(run(t.nr_clone, SIGCHLD), SECCOMP_RET_ALLOW);
            assert_eq!(run(t.nr_clone, SIGCHLD | CLONE_FS), SECCOMP_RET_ALLOW);

            let a = table("aarch64");
            let prog = escape_filter_program(&a);
            assert_eq!(simulate(&prog, a.audit_arch, NR_EXECVE_AARCH64, 0), SECCOMP_RET_ALLOW);
        }

        #[test]
        fn clone3_is_refused_so_libc_falls_back_to_filterable_clone() {
            for arch in ["x86_64", "aarch64"] {
                let t = table(arch);
                let prog = escape_filter_program(&t);
                assert_eq!(
                    simulate(&prog, t.audit_arch, t.nr_clone3, 0),
                    SECCOMP_RET_ERRNO | SECCOMP_ENOSYS,
                    "{arch}: clone3"
                );
            }
        }

        #[test]
        fn namespaced_clone_is_denied_by_flags_not_number() {
            for arch in ["x86_64", "aarch64"] {
                let t = table(arch);
                let prog = escape_filter_program(&t);
                let run = |flags: u32| simulate(&prog, t.audit_arch, t.nr_clone, flags);
                let denied = SECCOMP_RET_ERRNO | SECCOMP_EACCES;

                assert_eq!(run(CLONE_NEW_MASK), denied, "{arch}: full mask");
                // Each namespace bit on its own — a single wrong constant in
                // the mask must not hide behind the rest of the mask.
                for bit in [
                    0x0002_0000u32,
                    0x0200_0000,
                    0x0400_0000,
                    0x0800_0000,
                    0x1000_0000,
                    0x2000_0000,
                    0x4000_0000,
                ] {
                    assert_eq!(run(bit), denied, "{arch}: CLONE_NEW bit {bit:#x}");
                }
            }
        }

        #[test]
        fn wrong_architecture_lane_allows() {
            let t = table("x86_64");
            let prog = escape_filter_program(&t);
            let (_, nr) = t.denied[0];
            assert_eq!(
                simulate(&prog, 0xDEAD_BEEF, nr, 0),
                SECCOMP_RET_ALLOW,
                "a program built for one arch must not deny another's syscalls"
            );
        }

        #[test]
        fn tables_are_well_formed() {
            for arch in ["x86_64", "aarch64"] {
                let t = table(arch);
                assert!(t.denied.len() >= 35, "{arch}: deny list suspiciously short");
                let mut seen = Vec::new();
                for (name, nr) in t.denied {
                    assert!(!seen.contains(nr), "{arch}: duplicate number for {name}");
                    seen.push(*nr);
                }
                assert_ne!(t.nr_clone, t.nr_clone3, "{arch}");
            }
            // An architecture the table has no numbers for must come back
            // None so the installer fails closed.
            assert!(escape_constants("riscv64").is_none());
            assert!(escape_constants("").is_none());
        }
    }

    /// The escape table's hardcoded numbers against the `libc` constants of
    /// the same name. A typo'd number denies the wrong syscall, which no
    /// simulation can catch — the simulator trusts the table it is given — so
    /// this contract test is the real guard. Linux-only because that is where
    /// the constants exist.
    #[cfg(target_os = "linux")]
    mod escape_contract {
        use super::super::seccomp::escape_constants;

        fn libc_nr(name: &str) -> libc::c_long {
            match name {
                "ptrace" => libc::SYS_ptrace,
                "process_vm_readv" => libc::SYS_process_vm_readv,
                "process_vm_writev" => libc::SYS_process_vm_writev,
                "kcmp" => libc::SYS_kcmp,
                "process_madvise" => libc::SYS_process_madvise,
                "bpf" => libc::SYS_bpf,
                "perf_event_open" => libc::SYS_perf_event_open,
                "userfaultfd" => libc::SYS_userfaultfd,
                "kexec_load" => libc::SYS_kexec_load,
                "kexec_file_load" => libc::SYS_kexec_file_load,
                "open_by_handle_at" => libc::SYS_open_by_handle_at,
                "name_to_handle_at" => libc::SYS_name_to_handle_at,
                "lookup_dcookie" => libc::SYS_lookup_dcookie,
                // x86-only port I/O: the syscalls (and libc's constants for
                // them) do not exist on ARM64, where the escape table does
                // not list them either.
                #[cfg(target_arch = "x86_64")]
                "ioperm" => libc::SYS_ioperm,
                #[cfg(target_arch = "x86_64")]
                "iopl" => libc::SYS_iopl,
                "swapon" => libc::SYS_swapon,
                "swapoff" => libc::SYS_swapoff,
                "quotactl" => libc::SYS_quotactl,
                "acct" => libc::SYS_acct,
                "reboot" => libc::SYS_reboot,
                "keyctl" => libc::SYS_keyctl,
                "add_key" => libc::SYS_add_key,
                "request_key" => libc::SYS_request_key,
                "init_module" => libc::SYS_init_module,
                "finit_module" => libc::SYS_finit_module,
                "delete_module" => libc::SYS_delete_module,
                "mount" => libc::SYS_mount,
                "umount2" => libc::SYS_umount2,
                "pivot_root" => libc::SYS_pivot_root,
                "unshare" => libc::SYS_unshare,
                "setns" => libc::SYS_setns,
                "fsopen" => libc::SYS_fsopen,
                "fsconfig" => libc::SYS_fsconfig,
                "fsmount" => libc::SYS_fsmount,
                "fspick" => libc::SYS_fspick,
                "move_mount" => libc::SYS_move_mount,
                "open_tree" => libc::SYS_open_tree,
                "mount_setattr" => libc::SYS_mount_setattr,
                "remap_file_pages" => libc::SYS_remap_file_pages,
                other => unreachable!("escape-table entry without a libc mapping: {other}"),
            }
        }

        #[test]
        fn escape_syscall_tables_match_libc() {
            // libc's SYS_* constants describe the architecture this test
            // compiles for, so only the running arch's table can be checked
            // against them; the other tables are covered by the simulator's
            // documented-derivation tests.
            let arch = std::env::consts::ARCH;
            let t = escape_constants(arch).expect("the compiling arch must have a table");
            for (name, nr) in t.denied {
                assert_eq!(*nr as libc::c_long, libc_nr(name), "{arch}: {name}");
            }
            assert_eq!(t.nr_clone as libc::c_long, libc::SYS_clone, "{arch}");
            assert_eq!(t.nr_clone3 as libc::c_long, libc::SYS_clone3, "{arch}");
        }
    }

    /// The kernel's seccomp verifier enforces structural rules the simulator
    /// does not model (instruction whitelist, jump-shape checks), so the real
    /// programs must be installed on the real kernel once. seccomp filters
    /// apply to the calling thread only — sibling tests are untouched.
    #[cfg(target_os = "linux")]
    #[test]
    fn seccomp_programs_pass_the_kernel_verifier() {
        install_escape_seccomp().expect("kernel must accept the escape-vector filter");
        install_network_seccomp().expect("kernel must accept the network filter");
    }

    #[cfg(target_os = "macos")]
    mod seatbelt {
        use super::*;

        fn fence(workspace_root: &str, allowed: &[&str]) -> WriteFence {
            WriteFence::new(
                std::path::PathBuf::from(workspace_root),
                allowed.iter().map(std::path::PathBuf::from).collect(),
                false,
                // The namespace view is Linux-only; Seatbelt ignores it.
                NamespacePosture::Off,
            )
        }

        /// Same, with the network posture flipped on.
        fn fence_no_network(workspace_root: &str) -> WriteFence {
            WriteFence::new(
                std::path::PathBuf::from(workspace_root),
                Vec::new(),
                true,
                NamespacePosture::Off,
            )
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

        /// Landlock in isolation (`Off`): the namespace view's private /tmp
        /// would swallow writes aimed at host /tmp and the read-only root
        /// would deny them for the wrong reason. The view itself has its own
        /// test module below.
        fn fence(workspace_root: &str, allowed: &[&str]) -> WriteFence {
            WriteFence::new(
                std::path::PathBuf::from(workspace_root),
                allowed.iter().map(std::path::PathBuf::from).collect(),
                false,
                NamespacePosture::Off,
            )
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

    /// The namespace view end to end against the real kernel: user namespace,
    /// read-only root, private `/tmp`, the working trees re-bound on top, and
    /// the seccomp escape deny list beneath it all. Skips when the
    /// environment cannot build the view — unprivileged user namespaces
    /// switched off is a legitimate deployment, and the guard is exactly the
    /// degradation the posture promises.
    #[cfg(target_os = "linux")]
    mod namespace_view {
        use super::*;

        fn fence_auto(workspace_root: &str, allowed: &[&str]) -> WriteFence {
            WriteFence::new(
                std::path::PathBuf::from(workspace_root),
                allowed.iter().map(std::path::PathBuf::from).collect(),
                false,
                NamespacePosture::Auto,
            )
        }

        fn fence_off(workspace_root: &str) -> WriteFence {
            WriteFence::new(
                std::path::PathBuf::from(workspace_root),
                Vec::new(),
                false,
                NamespacePosture::Off,
            )
        }

        /// Whether this environment can create an unprivileged user namespace
        /// at all. `unshare` ships in util-linux on every test platform; a
        /// missing binary counts as unavailable.
        /// Whether the FULL namespace view builds end to end in this
        /// environment: a `Require`-posture run only spawns when every step —
        /// user namespace, private mount namespace, read-only root, re-binds —
        /// succeeds. Container runtimes lock their `/dev` submounts, which the
        /// re-bind cannot cross, so there the view degrades to Landlock-only
        /// and these assertions skip rather than test a degraded environment.
        async fn full_view_available() -> bool {
            let tmp = match tempfile::tempdir() {
                Ok(t) => t,
                Err(_) => return false,
            };
            let ws = match std::fs::canonicalize(tmp.path()) {
                Ok(w) => w,
                Err(_) => return false,
            };
            let req = ProcessRequest {
                argv: vec!["/bin/true".to_string()],
                fence: Some(WriteFence::new(ws, Vec::new(), false, NamespacePosture::Require)),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            LandlockRunner::default()
                .run(&req)
                .await
                .map(|out| out.success())
                .unwrap_or(false)
        }

        fn runner() -> LandlockRunner {
            LandlockRunner::default()
        }

        /// An outside write must be refused by the read-only root — saying
        /// "Read-only file system" (EROFS), not Landlock's EACCES, is what
        /// distinguishes the view from the rules beneath it.
        #[tokio::test]
        async fn outside_writes_hit_the_read_only_root() {
            if !full_view_available().await {
                eprintln!("skipping: full namespace view unavailable here");
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            let target = format!("/etc/syscity_ns_view_{}", std::process::id());
            let _ = std::fs::remove_file(&target);
            let req = ProcessRequest {
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    format!("echo x > {target}"),
                ],
                fence: Some(fence_auto(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            let out = runner().run(&req).await.unwrap();
            assert!(
                !out.success(),
                "write outside the workspace must hit the read-only root: {}",
                out.stderr_string()
            );
            assert!(
                out.stderr_string()
                    .to_lowercase()
                    .contains("read-only file system"),
                "expected EROFS from the view, got: {}",
                out.stderr_string()
            );
            assert!(!std::path::Path::new(&target).exists());
        }

        /// `/tmp` is a fresh tmpfs: the host's leftovers are invisible to the
        /// child. (The other direction is fenced by Landlock itself — the
        /// child cannot write host `/tmp` at all, so there is nothing to
        /// leak.)
        #[tokio::test]
        async fn host_tmp_is_invisible_under_the_view() {
            if !full_view_available().await {
                eprintln!("skipping: full namespace view unavailable here");
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            let host_marker = format!("/tmp/syscity_ns_host_{}", std::process::id());
            std::fs::write(&host_marker, "host").unwrap();
            let script = format!("test -e {host_marker} && echo LEAK; echo done");
            let req = ProcessRequest {
                argv: vec!["/bin/sh".to_string(), "-c".to_string(), script],
                fence: Some(fence_auto(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            let out = runner().run(&req).await.unwrap();
            assert!(
                out.success(),
                "the view must not break ordinary work: {}",
                out.stderr_string()
            );
            assert!(
                !out.stdout_string().contains("LEAK"),
                "the child must not see the host's /tmp: {}",
                out.stdout_string()
            );
            let _ = std::fs::remove_file(&host_marker);
        }

        /// `2>/dev/null` is the ubiquitous idiom: the `/dev/null` grant
        /// (Landlock) plus the `/dev` re-bind (mount layer) together keep it
        /// working across the read-only flip.
        #[tokio::test]
        async fn dev_null_still_swallows_output() {
            if !full_view_available().await {
                eprintln!("skipping: full namespace view unavailable here");
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            let req = ProcessRequest {
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo stdout-ok; echo hidden 2>/dev/null; cat /dev/null; echo done".to_string(),
                ],
                fence: Some(fence_auto(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            let out = runner().run(&req).await.unwrap();
            assert!(
                out.success(),
                "/dev/null must stay usable under the view: {}",
                out.stderr_string()
            );
            assert!(out.stdout_string().contains("stdout-ok"));
            assert!(
                !out.stderr_string().contains("hidden"),
                "the redirect to /dev/null must swallow stderr"
            );
        }

        /// A workspace under `/tmp` survives the tmpfs cover: the write lands
        /// in the real workspace, visible to the host.
        #[tokio::test]
        async fn workspace_under_tmp_survives_the_cover() {
            if !full_view_available().await {
                eprintln!("skipping: full namespace view unavailable here");
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            let file = ws.join("f");
            let req = ProcessRequest {
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    format!("echo x > '{}'", file.display()),
                ],
                fence: Some(fence_auto(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            let out = runner().run(&req).await.unwrap();
            assert!(
                out.success(),
                "workspace writes must work under the view: {}",
                out.stderr_string()
            );
            assert!(file.exists(), "the re-bound workspace must be the real directory");
        }

        /// Landlock still fences beneath the view: `/tmp` is writable again
        /// (a fresh tmpfs the view owns), so the ONLY thing that can refuse a
        /// write to a `/tmp` path outside the grant is Landlock.
        #[tokio::test]
        async fn landlock_holds_beneath_the_view() {
            if !full_view_available().await {
                eprintln!("skipping: full namespace view unavailable here");
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            let target = format!("/tmp/syscity_ns_other_{}", std::process::id());
            let _ = std::fs::remove_file(&target);
            let req = ProcessRequest {
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    format!("echo x > {target}"),
                ],
                fence: Some(fence_auto(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            let out = runner().run(&req).await.unwrap();
            assert!(
                !out.success(),
                "the view must not widen the workspace grant: {}",
                out.stderr_string()
            );
            assert!(!std::path::Path::new(&target).exists());
        }

        /// The seccomp deny list is real at runtime: `unshare` is on it, and
        /// the util-linux CLI hits exactly that syscall.
        #[tokio::test]
        async fn seccomp_denies_unshare_at_runtime() {
            if !full_view_available().await {
                eprintln!("skipping: full namespace view unavailable here");
                return;
            }
            if !std::path::Path::new("/usr/bin/unshare").is_file() {
                eprintln!("skipping: no unshare binary");
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            let req = ProcessRequest {
                argv: vec!["unshare".to_string(), "-m".to_string(), "true".to_string()],
                fence: Some(fence_auto(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            let out = runner().run(&req).await.unwrap();
            assert!(
                !out.success(),
                "unshare must be denied inside the fence: {}",
                out.stderr_string()
            );
        }

        /// `Off` is the regression baseline: the shared /tmp is back (host
        /// markers visible) and Landlock alone fences outside writes.
        #[tokio::test]
        async fn off_keeps_landlock_only() {
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            let host_marker = format!("/tmp/syscity_ns_off_{}", std::process::id());
            std::fs::write(&host_marker, "host").unwrap();
            let req = ProcessRequest {
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    format!("test -e {host_marker}"),
                ],
                fence: Some(fence_off(ws.to_str().unwrap())),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            let out = runner().run(&req).await.unwrap();
            assert!(out.success(), "Off must keep the shared /tmp: {}", out.stderr_string());
            let _ = std::fs::remove_file(&host_marker);

            let target = format!("/tmp/syscity_ns_off_write_{}", std::process::id());
            let _ = std::fs::remove_file(&target);
            let req = ProcessRequest {
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    format!("echo x > {target}"),
                ],
                fence: Some(fence_off(ws.to_str().unwrap())),
                timeout: Some(Duration::from_secs(10)),
                ..Default::default()
            };
            let out = runner().run(&req).await.unwrap();
            assert!(
                !out.success(),
                "Landlock must still deny outside writes with Off: {}",
                out.stderr_string()
            );
            assert!(!std::path::Path::new(&target).exists());
        }

        /// The timeout group kill works through the view: the sandboxed child
        /// and its descendants die promptly, partial output survives.
        #[tokio::test]
        async fn run_collect_timeout_kills_through_the_view() {
            if !full_view_available().await {
                eprintln!("skipping: full namespace view unavailable here");
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            let ws = std::fs::canonicalize(tmp.path()).unwrap();
            // `sleep 5` is backgrounded by sh; without the group kill the
            // orphan keeps the shared stdout pipe open and the drain below
            // blocks ~5s past the 300ms timeout.
            let req = ProcessRequest {
                argv: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo partial-out; sleep 5 & wait".to_string(),
                ],
                fence: Some(fence_auto(ws.to_str().unwrap(), &[])),
                timeout: Some(Duration::from_millis(300)),
                ..Default::default()
            };
            let started = std::time::Instant::now();
            let out = runner().run_collect(&req).await.unwrap();
            assert!(out.timed_out, "run_collect should report the timeout");
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
    }

    #[cfg(target_os = "windows")]
    mod appcontainer {
        use super::*;

        fn fence(workspace_root: &str, allowed: &[&str]) -> WriteFence {
            WriteFence::new(
                std::path::PathBuf::from(workspace_root),
                allowed.iter().map(std::path::PathBuf::from).collect(),
                false,
                // The namespace view is Linux-only; AppContainer ignores it.
                NamespacePosture::Off,
            )
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
