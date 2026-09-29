//! Device native bridge.
//!
//! On mobile (Android/iOS) Syscity runs inside the OS sandbox without the
//! process-level abilities of a desktop host (no arbitrary subprocesses, no
//! window system, restricted file access). The [`DeviceBridge`] trait is the
//! single seam through which the Rust runtime reaches the platform's native
//! APIs — camera, geolocation, notifications, haptics, SAF file picking,
//! loopback-ADB pairing (mobile-migration §4.1/§4.2/§4.5), the
//! Shortcuts/AppIntents bus (§4.6), and URL handoff to the OS (`tel:`,
//! `mailto:`, `sms:`, `https?` — the dialer/mail/messages/browser route,
//! where the user confirms in the target app).
//!
//! Desktop builds never construct a bridge: `GatewayState.device.bridge`
//! stays `None`, every `device_*` tool reports unavailable, and each
//! `device.*` WS method returns `UNSUPPORTED_PLATFORM`. Nothing in the
//! desktop path changes behaviour.
// INVARIANTS-NONE: bridge trait and drivers; no local persistent state.

mod tools;

pub(crate) use tools::NO_BRIDGE_MSG;
pub use tools::{
    DeviceCameraTool, DeviceGeolocateTool, DeviceHapticTool, DeviceNotifyTool, DeviceOpenUrlTool,
    DevicePickFileTool, DeviceShortcutInboxTool, DeviceShortcutResultsTool, DeviceShortcutRunTool,
};

use std::sync::Arc;

// ─────────────────────────────────────────────
// Command names — shared by the Rust callers, the
// Kotlin `DevicePlugin` `@Command` methods, and the
// WS handlers.
//
// IMPORTANT: the value must be the exact Kotlin
// `@Command` method name. `run_mobile_plugin_async`
// passes the string to JNI verbatim and the plugin
// dispatches by method name (`commands[command]`),
// so no prefix stripping or case conversion happens.
// Keep them lowerCamelCase to match Kotlin.
// ─────────────────────────────────────────────

/// Capture a photo with the device camera (4.1).
pub const CMD_CAPTURE_CAMERA: &str = "captureCamera";
/// Return the current GPS / network location fix (4.1).
pub const CMD_GET_LOCATION: &str = "getLocation";
/// Post a heads-up notification (4.1).
pub const CMD_NOTIFY: &str = "showNotification";
/// Trigger a haptic vibration (4.1).
pub const CMD_HAPTIC: &str = "vibrate";
/// SAF content:// file picker (4.2).
pub const CMD_PICK_FILE: &str = "pickFile";
/// Report the grant state of a runtime permission.
pub const CMD_PERMISSION_STATUS: &str = "permissionStatus";
/// Request a runtime permission from the user.
pub const CMD_REQUEST_PERMISSION: &str = "requestPermission";
// NOTE: loopback-ADB pairing (4.5) is deliberately NOT a bridge command.
// The bundled adb client runs inside the Rust process via the platform
// process_runner (AndroidShellRunner resolves <nativeLibDir>/adb), shared by
// the `device_adb_pair`/`device_adb_status` agent tools and the `device.adb.*`
// WS handlers (src/computer/platform/mobile/mod.rs). The Kotlin plugin never
// execs adb.
/// Sync the cron schedule into WorkManager for background wake (4.3).
pub const CMD_CRON_SYNC: &str = "syncCronSchedule";
// NOTE: 4.6 uses an AppIntent result channel: a shortcut's final step runs
// Syscity's own AppIntent (`SyscityOutputIntent`), whose Swift `perform()`
// writes `{output, at_ms}` into `<SYSCITY_HOME>/shortcuts/` — no external
// dependency. `runShortcut` hand-off and the two read commands below are the
// only bridge surface Rust needs.
/// Hand off to the Shortcuts app with a shortcut name + input (4.6).
pub const CMD_RUN_SHORTCUT: &str = "runShortcut";
/// List and consume completed shortcut outputs (4.6).
pub const CMD_SHORTCUT_RESULTS: &str = "shortcutResults";
/// List and consume the AskSyscity prompt inbox (4.6).
pub const CMD_SHORTCUT_INBOX: &str = "shortcutInbox";
/// Hand a URL to the OS (`tel:`/`mailto:`/`sms:`/`https?` → dialer, mail
/// composer, messages, browser). The user confirms in the target app.
pub const CMD_OPEN_URL: &str = "openUrl";

/// The native device bridge.
///
/// Implementations forward `call` to platform code and return a
/// JSON value. The concrete payloads are defined per-command in the
/// Kotlin plugin; Rust treats them opaquely (validated at each call site).
#[async_trait::async_trait]
pub trait DeviceBridge: Send + Sync {
    /// Whether the bridge is wired up and can serve commands.
    ///
    /// A bridge instance is only ever constructed on platforms that
    /// support it, so the default is `true`; a future no-op bridge may
    /// override this to `false`.
    fn available(&self) -> bool {
        true
    }

    /// Execute a native command with the given payload.
    async fn call(
        &self,
        command: &str,
        payload: serde_json::Value,
    ) -> crate::Result<serde_json::Value>;
}

/// A wrapper so tools can cheaply pass an optional bridge around.
pub type DeviceBridgeRef = Arc<dyn DeviceBridge>;

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A canned in-memory bridge for Rust unit tests.
    ///
    /// Records every `call` (command + payload) into a log so callers can
    /// assert on what was forwarded, and returns `response` verbatim.
    pub(crate) struct MockDeviceBridge {
        pub(crate) response: serde_json::Value,
        pub(crate) log: Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl MockDeviceBridge {
        pub(crate) fn new(response: serde_json::Value) -> Self {
            Self {
                response,
                log: Mutex::new(Vec::new()),
            }
        }

        /// All commands invoked, in order: `(command, payload)`.
        pub(crate) fn calls(&self) -> Vec<(String, serde_json::Value)> {
            self.log.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl DeviceBridge for MockDeviceBridge {
        async fn call(
            &self,
            command: &str,
            payload: serde_json::Value,
        ) -> crate::Result<serde_json::Value> {
            self.log
                .lock()
                .unwrap()
                .push((command.to_string(), payload));
            Ok(self.response.clone())
        }
    }

    #[test]
    fn test_bridge_default_available() {
        let bridge = MockDeviceBridge::new(serde_json::json!({}));
        assert!(bridge.available());
    }

    #[tokio::test]
    async fn test_bridge_forwards_command_and_payload() {
        let bridge = MockDeviceBridge::new(serde_json::json!({ "ok": true }));
        let result = bridge
            .call(CMD_CAPTURE_CAMERA, serde_json::json!({ "q": 1 }))
            .await
            .unwrap();
        assert_eq!(result, serde_json::json!({ "ok": true }));
        assert_eq!(bridge.calls().len(), 1);
        assert_eq!(bridge.calls()[0].0, CMD_CAPTURE_CAMERA);
        assert_eq!(bridge.calls()[0].1, serde_json::json!({ "q": 1 }));
    }

    /// Payloads must serialize — any caller passing an unserializable
    /// payload (e.g. a non-object) is a bug caught here.
    #[tokio::test]
    async fn test_bridge_accepts_opaque_payload() {
        let bridge = MockDeviceBridge::new(serde_json::json!({}));
        let payload = serde_json::to_value(42i32).unwrap();
        bridge.call(CMD_HAPTIC, payload).await.unwrap();
        assert_eq!(bridge.calls()[0].1, serde_json::json!(42));
    }
}
