//! Device native bridge.
//!
//! On mobile (Android/iOS) Syscity runs inside the OS sandbox without the
//! process-level abilities of a desktop host (no arbitrary subprocesses, no
//! window system, restricted file access). The [`DeviceBridge`] trait is the
//! single seam through which the Rust runtime reaches the platform's native
//! APIs — camera, geolocation, notifications, haptics, SAF file picking,
//! loopback-ADB pairing (mobile-migration §4.1/§4.2/§4.5), the
//! Shortcuts/AppIntents bus (§4.6), URL handoff to the OS (`tel:`,
//! `mailto:`, `sms:`, `https?` — the dialer/mail/messages/browser route,
//! where the user confirms in the target app), and the accessibility tree
//! the Android toolset reads other apps through.
//!
//! Desktop builds never construct a bridge: `GatewayState.device.bridge`
//! stays `None`, every `device_*` tool reports unavailable, and each
//! `device.*` WS method returns `UNSUPPORTED_PLATFORM`. Nothing in the
//! desktop path changes behaviour.
// INVARIANTS-NONE: bridge trait and drivers; no local persistent state.

mod events;
mod tools;

pub use events::{
    A11yStatus, DeviceEvent, EventKind, EventPage, EventStream, EVENT_SOURCE_BUFFER,
    EVENT_SOURCE_PUSH, EVENT_SOURCE_UNAVAILABLE,
};
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

/// Dump the current screen's accessibility tree (Android, in-process only).
///
/// The tree is what the platform's own accessibility API sees, which is both
/// richer than `uiautomator dump` (every node, not only the actionable ones)
/// and cheaper (no subprocess, no `/sdcard` round trip). It is only available
/// while Syscity's own [`SyscityAccessibilityService`] is enabled — the
/// Kotlin side answers `{"enabled": false}` otherwise, and the caller falls
/// back to ADB.
pub const CMD_A11Y_TREE: &str = "accessibilityTree";
/// Whether the accessibility service is enabled and connected.
pub const CMD_A11Y_STATUS: &str = "accessibilityStatus";
/// Recent device events from the accessibility service's ring (see [`events`]).
///
/// The push half of the same stream is not a command: it is a JNI callback from
/// the phone's own Kotlin side (`NativeEvents.onDeviceEvent`), received in the
/// `desktop` crate and published through [`DeviceBridge::events`]. This command
/// is the buffer half — history from before a waiter started listening, and the
/// only path when the callback is missing.
pub const CMD_A11Y_EVENTS: &str = "accessibilityEvents";
/// Read text out of a base64 PNG with the on-device ML Kit model.
///
/// Only the mobile build has no `vision` feature, so this is how OCR works on
/// the phone itself: the desktop agent's ONNX engine is not compiled in there.
/// Answers `{"ok": false, "code": "ocr_unavailable"}` when the model cannot
/// load — an explicit failure, never an empty line list.
pub const CMD_OCR: &str = "ocr";
/// Open the system accessibility settings, where the user enables the service.
pub const CMD_A11Y_SETTINGS: &str = "openAccessibilitySettings";

/// Forward one op to the virtual-display helper (see
/// [`crate::computer::platform::mobile::display`]).
///
/// The payload is the op itself (`{"op": "create", ...}`) and the answer is the
/// helper's, so the Kotlin side stays a pass-through and the protocol lives in
/// one place — the Rust client that validates it.
pub const CMD_VIRTUAL_DISPLAY: &str = "virtualDisplay";

/// The bridge, for code that cannot be handed one.
///
/// The Android toolset reads the accessibility tree from deep inside its read
/// path (`dump_ui` → `settle_after_action` → the effect check), which is far
/// below where a bridge could sensibly be threaded through. On desktop this
/// stays empty — nothing installs a bridge there — so the lookup costs a
/// `OnceLock::get` and changes nothing.
static BRIDGE: std::sync::OnceLock<DeviceBridgeRef> = std::sync::OnceLock::new();

/// Publish `bridge` process-wide. Idempotent: the first call wins.
pub fn install_bridge(bridge: DeviceBridgeRef) {
    let _ = BRIDGE.set(bridge);
}

/// The process-wide bridge, if one was installed.
pub fn bridge() -> Option<&'static DeviceBridgeRef> {
    BRIDGE.get()
}

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

    /// A subscription to events pushed by the platform, if this bridge has one.
    ///
    /// `None` — the default — means "this bridge has no push channel", and a
    /// caller must fall back to polling the `accessibilityEvents` ring (which
    /// [`DeviceEvent`] history is still available through) or the screen. It
    /// is *not* an error: a desktop driving a phone over ADB has no
    /// accessibility service to push from, and a phone whose native hand-off
    /// failed still has the ring.
    ///
    /// `broadcast` drops events when nobody is subscribed: that is deliberate,
    /// because an event nobody was waiting for is served by the ring instead.
    /// Subscribe *before* triggering whatever the wait is for, or the first
    /// event can land in the gap.
    fn events(&self) -> Option<tokio::sync::broadcast::Receiver<DeviceEvent>> {
        None
    }
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
