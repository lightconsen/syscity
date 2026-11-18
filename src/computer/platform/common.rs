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

/// A screenshot tool's `region` argument, as a rectangle in screen points.
///
/// The computer adapters send the structured `{"x","y","width","height"}`
/// object, while every screenshot CLI these tools drive takes text in a
/// grammar of its own (`x,y,w,h`, `x,y WxH`, …). The *parsing* lives here
/// rather than in each platform so the shapes cannot drift: three adapters and
/// four tools all have to agree on what a region is, and a tool that silently
/// ignores one turns a close-up into a full-screen capture that the caller then
/// mistakes for a crop. Formatting for a particular CLI stays with that CLI,
/// because only it knows its separators.
///
/// `None` means "no usable region" — absent, malformed, or non-positive in
/// either dimension. Refusing is deliberate: a capture of a rectangle nobody
/// asked for is a wrong answer the caller cannot detect, while a missing crop
/// is at worst a wider view.
pub fn screenshot_region(args: &serde_json::Value) -> Option<(i64, i64, i64, i64)> {
    let region = args.get("region")?;
    if let Some(text) = region.as_str() {
        let parts: Vec<&str> = text.split([' ', ',', 'x']).map(str::trim).collect();
        let parts: Vec<&str> = parts.into_iter().filter(|p| !p.is_empty()).collect();
        if parts.len() != 4 {
            return None;
        }
        let values: Option<Vec<i64>> = parts.iter().map(|p| p.parse::<i64>().ok()).collect();
        return four(values?);
    }
    let x = region.get("x")?.as_i64()?;
    let y = region.get("y")?.as_i64()?;
    let width = region.get("width")?.as_i64()?;
    let height = region.get("height")?.as_i64()?;
    four(vec![x, y, width, height])
}

/// [`screenshot_region`], formatted as `x,y,w,h` — the form `maim -g`,
/// `import -crop` and `screencapture -R` all take.
pub fn screenshot_region_commas(args: &serde_json::Value) -> Option<String> {
    let (x, y, width, height) = screenshot_region(args)?;
    Some(format!("{x},{y},{width},{height}"))
}

/// [`screenshot_region`], formatted as `x,y WxH` — the form `grim -g` takes.
pub fn screenshot_region_grim(args: &serde_json::Value) -> Option<String> {
    let (x, y, width, height) = screenshot_region(args)?;
    Some(format!("{x},{y} {width}x{height}"))
}

/// Unpack four values and reject a rectangle with no area.
fn four(values: Vec<i64>) -> Option<(i64, i64, i64, i64)> {
    let [x, y, width, height]: [i64; 4] = values.try_into().ok()?;
    if width <= 0 || height <= 0 {
        return None;
    }
    Some((x, y, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_adapters_object_becomes_the_cli_text_form() {
        let args = serde_json::json!({
            "region": { "x": 100, "y": 50, "width": 300, "height": 200 }
        });
        assert_eq!(screenshot_region(&args), Some((100, 50, 300, 200)));
        assert_eq!(screenshot_region_commas(&args).as_deref(), Some("100,50,300,200"));
    }

    #[test]
    fn a_region_string_is_understood_in_every_cli_grammar() {
        // `maim -g` / `import -crop` / `screencapture -R`
        let commas = serde_json::json!({ "region": " 10, 20 , 30,40 " });
        assert_eq!(screenshot_region(&commas), Some((10, 20, 30, 40)));
        // `grim -g` writes the origin and the size with different separators.
        let grim = serde_json::json!({ "region": "10,20 30x40" });
        assert_eq!(screenshot_region(&grim), Some((10, 20, 30, 40)));
        assert_eq!(
            screenshot_region_grim(&grim).as_deref(),
            Some("10,20 30x40"),
            "grim's own grammar, not a comma-joined one"
        );
    }

    #[test]
    fn no_region_is_not_a_refusal() {
        assert_eq!(screenshot_region(&serde_json::json!({})), None);
        assert_eq!(screenshot_region_commas(&serde_json::json!({})), None);
    }

    #[test]
    fn a_rectangle_with_no_area_is_refused() {
        for bad in [
            serde_json::json!({ "region": { "x": 0, "y": 0, "width": 0, "height": 10 } }),
            serde_json::json!({ "region": { "x": 0, "y": 0, "width": -5, "height": 10 } }),
            serde_json::json!({ "region": { "x": 0, "y": 0, "width": 10 } }),
            serde_json::json!({ "region": "0,0,ten,10" }),
            serde_json::json!({ "region": "0,0,0,10" }),
            serde_json::json!({ "region": "0,0,10" }),
        ] {
            assert_eq!(screenshot_region(&bad), None, "must refuse: {bad}");
        }
    }
}
