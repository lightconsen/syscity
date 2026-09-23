//! Windows: the AppContainer-backed [`ProcessRunner`]. The profile and ACL
//! machinery live in the sibling `win_appcontainer` module; this file holds
//! only the runner that prepares a profile per fenced request.

use std::sync::Arc;

use async_trait::async_trait;

use crate::tools::win_appcontainer;

use super::{
    CommandOutput, NamespacePosture, ProcessChild, ProcessError, ProcessRequest, ProcessRunner,
    StdProcessRunner, WriteFence,
};

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
pub(super) struct Prepared {
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
