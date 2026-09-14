//! Mobile device bridge — control Android and iOS devices from Syscity.
//!
//! This module provides `PlatformToolSet`s for mobile automation via:
//! - **Android**: ADB (Android Debug Bridge)
//! - **iOS**: libimobiledevice (idevice*) tools
//!
//! Both sets are platform-agnostic — they can run on any host OS as long as
//! the corresponding CLI tools are installed.

pub mod android;
pub mod ios;

pub use android::AndroidToolset;
pub use ios::IosToolset;

/// Check whether a command is available on PATH.
fn has_command(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The directory holding bundled native binaries on mobile, when set.
///
/// Android hosts set `SYSCITY_NATIVE_LIB_DIR` to the APK's extracted
/// `nativeLibraryDir` (see `MainActivity.kt`); the bundled `adb` client for
/// §4.5 self-pairing lives there (installed by `scripts/fetch-android-adb.sh`).
pub fn bundled_native_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("SYSCITY_NATIVE_LIB_DIR")
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
}

/// Check whether `adb` is available: the mobile-bundled client takes
/// precedence, falling back to PATH (desktop bridge).
pub fn has_adb() -> bool {
    bundled_native_dir()
        .map(|dir| dir.join("adb").is_file())
        .unwrap_or(false)
        || has_command("adb")
}

/// Check whether `idevice_id` (libimobiledevice) is available on PATH.
pub fn has_idevice() -> bool {
    has_command("idevice_id")
}

/// Shared helper: run a command and return (success, stdout, stderr).
///
/// Routes through the platform-abstracted [`crate::tools::process_runner`]
/// so the same call resolves the mobile-bundled `adb` (AndroidShellRunner)
/// or, on desktop, behaves exactly as today's `tokio::process::Command`
/// (spawn errors surface as `io::Error`, no timeout, output captured).
pub async fn run_cmd(
    cmd: &str,
    args: &[&str],
) -> std::io::Result<(std::process::ExitStatus, String, String)> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(cmd.to_string());
    argv.extend(args.iter().map(|s| s.to_string()));

    let req = crate::tools::process_runner::ProcessRequest::argv(
        &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
    );
    let out = crate::tools::process_runner::run(&req).await.map_err(|e| {
        // Preserve today's error mapping: a spawn failure surfaces the
        // underlying `io::Error` so callers can wrap it in `ExternalService`.
        match e {
            crate::tools::process_runner::ProcessError::Spawn { source, .. } => source,
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

    let req = crate::tools::process_runner::ProcessRequest::argv(
        &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
    );
    let out = crate::tools::process_runner::run(&req)
        .await
        .map_err(|e| match e {
            crate::tools::process_runner::ProcessError::Spawn { source, .. } => source,
            other => std::io::Error::other(format!("{other}")),
        })?;
    let status = out
        .status
        .ok_or_else(|| std::io::Error::other("process aborted without a status"))?;
    let stderr = out.stderr_string();
    Ok((status, out.stdout, stderr))
}

/// List devices as seen by the local adb client (§4.5).
///
/// Each entry is `{serial, state}` where state is one of `device`,
/// `offline`, `unauthorized`, … (`adb devices -l`). After loopback pairing
/// the phone appears as `localhost:<port>`.
pub async fn adb_devices() -> crate::Result<Vec<serde_json::Value>> {
    let (status, stdout, stderr) = run_cmd("adb", &["devices", "-l"]).await.map_err(|e| {
        crate::error::SyscityError::ExternalService {
            source: "adb devices failed".to_string(),
            cause: Some(Box::new(e)),
        }
    })?;
    if !status.success() {
        return Err(crate::error::SyscityError::ExternalService {
            source: format!("adb devices failed: {stderr}"),
            cause: None,
        });
    }
    let devices = stdout
        .lines()
        .skip(1) // "List of devices attached"
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?;
            let state = parts.next()?;
            Some(serde_json::json!({ "serial": serial, "state": state }))
        })
        .collect();
    Ok(devices)
}

/// Report loopback pairing status (§4.5).
pub async fn adb_status() -> crate::Result<serde_json::Value> {
    let devices = adb_devices().await?;
    Ok(serde_json::json!({
        "paired": !devices.is_empty(),
        "devices": devices,
    }))
}

/// Pair with and connect to the phone's own wireless-debugging adbd (§4.5).
///
/// `port` is the pairing port shown in the "Pair device with pairing code"
/// dialog; `connect_port` (defaults to `port`) is the connect target shown
/// on the wireless-debugging screen. `adb pair` registers the app's key with
/// adbd; `adb connect` then opens the session over loopback.
pub async fn adb_pair(
    port: u16,
    code: &str,
    connect_port: Option<u16>,
) -> crate::Result<serde_json::Value> {
    let connect = connect_port.unwrap_or(port);

    let (pair_status, _pair_out, pair_err) =
        run_cmd("adb", &["pair", format!("localhost:{port}").as_str(), code])
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: "adb pair failed".to_string(),
                cause: Some(Box::new(e)),
            })?;

    let (connect_status, _connect_out, connect_err) =
        run_cmd("adb", &["connect", format!("localhost:{connect}").as_str()])
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: "adb connect failed".to_string(),
                cause: Some(Box::new(e)),
            })?;

    let devices = adb_devices().await?;
    Ok(serde_json::json!({
        "paired": pair_status.success(),
        "connected": connect_status.success(),
        "pair_output": pair_err,
        "connect_output": connect_err,
        "devices": devices,
    }))
}
