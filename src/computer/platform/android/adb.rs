//! The `adb` client: finding it, and the loopback pairing protocol (§4.5).
//!
//! The agent can drive a phone from two places: a desktop with `adb` on PATH,
//! or the Syscity app running *on* the phone, which bundles its own `adb` in
//! the APK's `jniLibs` and reaches its own adbd over loopback. This module is
//! the half of that which is about the client binary rather than about a
//! screen — locating it, asking it what it is attached to, and registering this
//! app's key with wireless debugging.

use super::super::common::run_cmd;

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
        // `libadb.so` is the packaged spelling: Android's APK pipeline keeps
        // only `*.so` in jniLibs, so a plain `adb` file never reaches the
        // device. `adb` is still checked for trees built before that.
        .map(|dir| dir.join("adb").is_file() || dir.join("libadb.so").is_file())
        .unwrap_or(false)
        || super::super::common::has_command("adb")
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
