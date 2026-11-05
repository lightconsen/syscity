//! Running another program, for the device sets that drive one.
//!
//! A device set controls something outside this process — a phone over ADB, an
//! iPhone over `idevice*`, a VM over HTTP — and almost all of that control ends
//! up as "run this binary and read what it said". Every set needs the same
//! three answers from that: did it finish, what did it print, and how do I get
//! the bytes rather than a `String` when the output is a PNG.
//!
//! Routing through [`crate::tools::process_runner`] rather than calling
//! `tokio::process::Command` directly is what makes the *same* call correct on
//! a desktop and inside the APK: on Android the runner resolves the bundled
//! `adb` out of `nativeLibraryDir` and refuses executables SELinux would block,
//! on iOS it refuses to fork at all, and on a desktop it is the ordinary spawn.
//! A set that spawns its own child bypasses all three.

use crate::tools::process_runner::{ProcessError, ProcessRequest};

/// Check whether a command answers on PATH.
///
/// `--version` is the probe because every tool these sets drive accepts it and
/// exits before doing anything. A tool that needs a different probe gets its
/// own check rather than a parameter here.
pub fn has_command(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Shared helper: run a command and return (success, stdout, stderr).
///
/// Spawn errors surface as the underlying `io::Error`; there is no timeout.
/// Anything that can hang must state its bound, which is [`run_cmd_timeout`].
pub async fn run_cmd(
    cmd: &str,
    args: &[&str],
) -> std::io::Result<(std::process::ExitStatus, String, String)> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(cmd.to_string());
    argv.extend(args.iter().map(|s| s.to_string()));
    let req = ProcessRequest::argv(&argv.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    run_cmd_with(&req).await
}

/// [`run_cmd`] with a wall-clock bound.
///
/// [`ProcessRequest::timeout`] defaults to `None`, which waits forever; the
/// device lock a caller holds is held for exactly as long as the process runs,
/// so anything that can hang must state its bound here. On expiry the child is
/// killed and this reports a timeout rather than an `io::Error`: "it did not
/// answer in time" is a result the caller can explain, not a transport fault.
pub async fn run_cmd_timeout(cmd: &str, args: &[&str], timeout: std::time::Duration) -> RunOutcome {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(cmd.to_string());
    argv.extend(args.iter().map(|s| s.to_string()));
    let mut req = ProcessRequest::argv(&argv.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    req.timeout = Some(timeout);

    match crate::tools::process_runner::run_collect(&req).await {
        Ok(out) if out.timed_out => RunOutcome::TimedOut {
            stdout: out.stdout_string(),
            stderr: out.stderr_string(),
        },
        Ok(out) => match out.status {
            Some(status) => RunOutcome::Finished {
                status,
                stdout: out.stdout_string(),
                stderr: out.stderr_string(),
            },
            None => RunOutcome::TimedOut {
                stdout: out.stdout_string(),
                stderr: out.stderr_string(),
            },
        },
        Err(e) => RunOutcome::Failed(e.to_string()),
    }
}

/// What [`run_cmd_timeout`] observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Finished {
        status: std::process::ExitStatus,
        stdout: String,
        stderr: String,
    },
    /// The child was killed at the deadline. Whatever it printed first is kept.
    TimedOut { stdout: String, stderr: String },
    /// It never ran (spawn failure, bad argv).
    Failed(String),
}

/// [`run_cmd`] for a request that carries more than argv — stdin, notably.
///
/// The display helper's launch sends a one-time nonce on stdin, and that is the
/// only way the app and the helper agree on it without putting it somewhere
/// another shell process can read.
pub async fn run_cmd_with(
    req: &ProcessRequest,
) -> std::io::Result<(std::process::ExitStatus, String, String)> {
    let out = crate::tools::process_runner::run(req).await.map_err(|e| {
        // Keep the spawn failure as the underlying `io::Error` so callers can
        // wrap it in `ExternalService` and keep the OS's message.
        match e {
            ProcessError::Spawn { source, .. } => source,
            other => std::io::Error::other(format!("{other}")),
        }
    })?;
    let status = out
        .status
        .ok_or_else(|| std::io::Error::other("process aborted without a status"))?;
    let stdout = out.stdout_string();
    let stderr = out.stderr_string();
    Ok((status, stdout, stderr))
}

/// Like [`run_cmd`], but returns stdout as raw bytes.
///
/// Required for anything binary: the `String` variant goes through
/// `from_utf8_lossy`, which replaces every non-UTF-8 byte with U+FFFD. That
/// silently corrupts a binary payload — `adb exec-out screencap -p` returns a
/// PNG, so base64-encoding the lossy string yields something that is not a
/// decodable image at all.
pub async fn run_cmd_bytes(
    cmd: &str,
    args: &[&str],
) -> std::io::Result<(std::process::ExitStatus, Vec<u8>, String)> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(cmd.to_string());
    argv.extend(args.iter().map(|s| s.to_string()));

    let req = ProcessRequest::argv(&argv.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    let out = crate::tools::process_runner::run(&req)
        .await
        .map_err(|e| match e {
            ProcessError::Spawn { source, .. } => source,
            other => std::io::Error::other(format!("{other}")),
        })?;
    let status = out
        .status
        .ok_or_else(|| std::io::Error::other("process aborted without a status"))?;
    let stderr = out.stderr_string();
    Ok((status, out.stdout, stderr))
}
