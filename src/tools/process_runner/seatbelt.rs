//! macOS: Seatbelt (`sandbox-exec`) write fence around the platform runner.
//!
//! Split out of the old single-file `process_runner`; behavior is unchanged.

use async_trait::async_trait;

use super::{
    CommandOutput, ProcessChild, ProcessError, ProcessRequest, ProcessRunner, StdProcessRunner,
    WriteFence,
};

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
    pub(super) fn fence(&self, req: &ProcessRequest) -> Result<ProcessRequest, ProcessError> {
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
pub(super) fn seatbelt_profile(fence: &WriteFence) -> String {
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
pub(super) fn can_sandbox() -> bool {
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
