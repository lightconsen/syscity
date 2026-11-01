//! A second Android display the agent drives while the user keeps the phone.
//!
//! Everything here is a client for a helper process that runs as **shell**
//! (uid 2000) on the device. It has to: `DisplayManager.createVirtualDisplay`
//! requires `CAPTURE_VIDEO_OUTPUT`, which is `signature|privileged` — an app
//! cannot create a display no matter what it declares. Syscity already reaches
//! a shell on its own device through the loopback-ADB pairing (§4.5), so the
//! helper is launched with `app_process`, which inherits that shell identity.
//!
//! The split is deliberate:
//!
//! * **Helper (shell, Kotlin, `helper/HelperMain.kt`)** — owns the display, the
//!   `ImageReader` behind it, and the commands that need shell identity.
//! * **App (Kotlin, `VDisplayClient.kt`)** — a `LocalSocket` client for it,
//!   reached from Rust through the device bridge as the `virtualDisplay`
//!   command.
//! * **Rust (this file)** — typed requests, response validation, and the
//!   `app_process` command line.
//!
//! Unavailable is a normal state, not an error: the helper is not running until
//! something starts it, and a phone without the pairing has no way to start it.
//! Callers fall back to driving the default display over ADB.

use serde_json::{json, Value};

use crate::device::{DeviceBridgeRef, CMD_VIRTUAL_DISPLAY};

/// The helper is not running, and could not be started.
pub const DISPLAY_UNAVAILABLE: &str = "DISPLAY_UNAVAILABLE";
/// The helper answered with something this code does not understand.
pub const DISPLAY_BAD_RESPONSE: &str = "DISPLAY_BAD_RESPONSE";
/// The helper refused the request (a permission the shell lacks, a bad display).
pub const DISPLAY_HELPER_ERROR: &str = "DISPLAY_HELPER_ERROR";

/// The helper's main class, as launched by `app_process`.
pub const HELPER_CLASS: &str = "net.syscity.desktop.helper.HelperMain";

/// Why a display operation did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayError {
    /// One of the constants above.
    pub code: &'static str,
    pub message: String,
}

impl DisplayError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl std::fmt::Display for DisplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// One frame from the virtual display.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayFrame {
    /// PNG bytes, already base64-decoded.
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A live display the agent owns.
///
/// Holding one of these means the helper is up and a display exists; dropping
/// it does not tear anything down (a dropped future cannot await), so callers
/// that own one must call [`VirtualDisplay::close`] — the tool does.
#[derive(Clone)]
pub struct VirtualDisplay {
    bridge: DeviceBridgeRef,
    /// The helper's id for this display.
    pub id: i32,
    pub width: u32,
    pub height: u32,
}

impl VirtualDisplay {
    /// Create a display and put `package` on it.
    ///
    /// `package` is optional: a display with nothing on it is a valid state
    /// (the user may be asked what to open), and the launcher is the app's
    /// decision, not this module's.
    pub async fn open(
        bridge: DeviceBridgeRef,
        size: Option<(u32, u32)>,
        package: Option<&str>,
    ) -> Result<Self, DisplayError> {
        let mut request = json!({ "op": "create" });
        if let Some((width, height)) = size {
            request["width"] = json!(width);
            request["height"] = json!(height);
        }
        let answer = call(&bridge, request).await?;
        let id = answer
            .get("display")
            .and_then(Value::as_i64)
            .and_then(|id| i32::try_from(id).ok())
            .ok_or_else(|| {
                DisplayError::new(
                    DISPLAY_BAD_RESPONSE,
                    "the helper created no display (its answer carried no display id)",
                )
            })?;
        let width = answer
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|w| u32::try_from(w).ok())
            .ok_or_else(|| DisplayError::new(DISPLAY_BAD_RESPONSE, "the display has no width"))?;
        let height = answer
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|h| u32::try_from(h).ok())
            .ok_or_else(|| DisplayError::new(DISPLAY_BAD_RESPONSE, "the display has no height"))?;

        let display = Self { bridge, id, width, height };
        if let Some(package) = package {
            display.launch(package).await?;
        }
        Ok(display)
    }

    /// Address a display the helper already owns.
    ///
    /// Used when the id came back from an earlier call: every op after `create`
    /// is keyed by id alone, so there is nothing to re-derive. Size is reported
    /// as unknown until a frame arrives with it.
    pub fn attach(bridge: DeviceBridgeRef, id: i32) -> Self {
        Self {
            bridge,
            id,
            width: 0,
            height: 0,
        }
    }

    /// Open `package` on this display.
    pub async fn launch(&self, package: &str) -> Result<(), DisplayError> {
        call(&self.bridge, json!({ "op": "launch", "display": self.id, "package": package }))
            .await
            .map(|_| ())
    }

    /// The current frame, as PNG.
    ///
    /// The helper owns the `ImageReader` and encodes the frame, because a
    /// `Surface` cannot cross a `LocalSocket`; the cost is one PNG per read,
    /// which is the right trade for an agent loop (one observation per turn,
    /// not a video stream).
    pub async fn screenshot(&self) -> Result<DisplayFrame, DisplayError> {
        let answer = call(&self.bridge, json!({ "op": "screenshot", "display": self.id })).await?;
        let encoded = answer
            .get("png")
            .and_then(Value::as_str)
            .ok_or_else(|| DisplayError::new(DISPLAY_BAD_RESPONSE, "the frame carried no image"))?;
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| {
                DisplayError::new(
                    DISPLAY_BAD_RESPONSE,
                    format!("the frame was not valid base64 ({e})"),
                )
            })?;
        let width = answer.get("width").and_then(Value::as_u64).unwrap_or(0);
        let height = answer.get("height").and_then(Value::as_u64).unwrap_or(0);
        Ok(DisplayFrame {
            png,
            width: u32::try_from(width).unwrap_or(0),
            height: u32::try_from(height).unwrap_or(0),
        })
    }

    /// Tap a point on this display, in its own pixel space.
    pub async fn tap(&self, x: i32, y: i32) -> Result<(), DisplayError> {
        call(&self.bridge, json!({ "op": "tap", "display": self.id, "x": x, "y": y }))
            .await
            .map(|_| ())
    }

    /// Swipe on this display.
    pub async fn swipe(
        &self,
        from: (i32, i32),
        to: (i32, i32),
        duration_ms: u32,
    ) -> Result<(), DisplayError> {
        call(
            &self.bridge,
            json!({
                "op": "swipe",
                "display": self.id,
                "x": from.0, "y": from.1,
                "x2": to.0, "y2": to.1,
                "duration": duration_ms,
            }),
        )
        .await
        .map(|_| ())
    }

    /// Type into whatever has focus on this display.
    pub async fn text(&self, text: &str) -> Result<(), DisplayError> {
        call(&self.bridge, json!({ "op": "text", "display": self.id, "text": text }))
            .await
            .map(|_| ())
    }

    /// Press a key on this display.
    pub async fn key(&self, keycode: &str) -> Result<(), DisplayError> {
        call(&self.bridge, json!({ "op": "key", "display": self.id, "keycode": keycode }))
            .await
            .map(|_| ())
    }

    /// Tear the display down and drop whatever was on it.
    ///
    /// Everything the helper started for this display dies with it: a display
    /// left behind would keep an app running that the user never opened.
    pub async fn close(self) -> Result<(), DisplayError> {
        call(&self.bridge, json!({ "op": "destroy", "display": self.id }))
            .await
            .map(|_| ())
    }

    /// What the helper says it can do, and on which API level.
    pub async fn status(bridge: &DeviceBridgeRef) -> Result<Value, DisplayError> {
        call(bridge, json!({ "op": "ping" })).await
    }
}

/// Send one op to the helper and validate the envelope.
async fn call(bridge: &DeviceBridgeRef, request: Value) -> Result<Value, DisplayError> {
    let answer = bridge
        .call(CMD_VIRTUAL_DISPLAY, request)
        .await
        .map_err(|e| {
            DisplayError::new(
                DISPLAY_UNAVAILABLE,
                format!(
                    "the display helper is not running — start it with `adb shell {}`, which \
                     needs the loopback-ADB pairing ({e})",
                    launch_command("<apk path>")
                ),
            )
        })?;

    match answer.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(answer),
        Some(false) => {
            let code = answer
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("HELPER_ERROR");
            let message = answer
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the display helper refused the request");
            // A helper that starts up and then reports a failure is not
            // "unavailable" — it ran, and what it said matters.
            Err(DisplayError::new(DISPLAY_HELPER_ERROR, format!("{message} ({code})")))
        }
        None => Err(DisplayError::new(
            DISPLAY_BAD_RESPONSE,
            "the display helper answered without ok/error",
        )),
    }
}

/// The shell command that starts the helper, given the app's APK path.
///
/// `app_process` runs a Java class with no Activity and no app sandbox: it
/// inherits the identity of whatever started it, so starting it from `adb
/// shell` gives the display the `CAPTURE_VIDEO_OUTPUT` it needs. The classpath
/// is the installed APK — `pm path` reports it, and the shell can read it.
///
/// The helper runs in the `shell` SELinux domain. An untagged app cannot open a
/// unix socket there (`avc: denied { connectto } … untrusted_app → shell`), so
/// what the app can reach of it is decided by the transport, not by this
/// command line — see the module note.
pub fn launch_command(apk_path: &str) -> String {
    format!("CLASSPATH={} app_process /system/bin {}", apk_path, HELPER_CLASS)
}

/// Whether the helper answers at all.
pub async fn available(bridge: &DeviceBridgeRef) -> bool {
    VirtualDisplay::status(bridge).await.is_ok()
}

/// The Android package whose APK carries the helper class.
pub const ANDROID_PACKAGE: &str = "net.syscity.desktop";

/// Start the helper if it is not already running.
///
/// It is launched **detached**: `app_process ... &` inside one `adb shell`
/// command, with its output discarded. A foreground launch would block that
/// adb call for as long as the helper lives, and the helper is meant to live
/// for the whole session.
///
/// The APK path comes from `pm path` rather than being guessed: it moves
/// between installs (`/data/app/~~<hash>/…/base.apk`) and the shell needs the
/// real one to build a classpath.
pub async fn ensure_helper(
    device: &Option<String>,
    package: &str,
    bridge: &DeviceBridgeRef,
) -> Result<(), DisplayError> {
    if available(bridge).await {
        return Ok(());
    }
    // Before anything else, is there a device to talk to at all? Without this
    // the first adb command fails with its own words (`libadb.so: device
    // offline` — the prefix is adb's log tag, taken from the binary's name,
    // not a serial), which reads like a bug in the display code.
    let devices = super::adb_devices().await.map_err(|e| {
        DisplayError::new(
            DISPLAY_UNAVAILABLE,
            format!("the app's bundled adb could not be queried ({e})"),
        )
    })?;
    connected_device_note(&devices).map_err(|note| DisplayError::new(DISPLAY_UNAVAILABLE, note))?;

    // Which device to aim at. On the phone this is the loopback connection it
    // paired with; but an emulator also exposes a transport, and adb refuses a
    // command that does not say which of the two it means ("more than one
    // device/emulator"). Prefer the loopback one — that is the one the app owns.
    let device = match device {
        Some(explicit) => Some(explicit.clone()),
        None => preferred_serial(&devices),
    };

    let apk = apk_path(&device, package).await?;
    let command = format!("{} >/dev/null 2>&1 &", launch_command(&apk));
    let args = super::android::adb_args(&device, &["shell", &command]);
    let (status, _, stderr) =
        super::run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
            .await
            .map_err(|e| {
                DisplayError::new(
                    DISPLAY_UNAVAILABLE,
                    format!("could not reach adb to start the display helper ({e})"),
                )
            })?;
    if !status.success() {
        return Err(DisplayError::new(
            DISPLAY_UNAVAILABLE,
            format!("starting the display helper failed: {}", stderr.trim()),
        ));
    }

    // `app_process` takes a moment to bind the socket; poll rather than sleep
    // for a fixed time, so a fast device is not made to wait and a slow one is
    // not declared broken.
    for _ in 0..25 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if available(bridge).await {
            return Ok(());
        }
    }
    Err(DisplayError::new(
        DISPLAY_UNAVAILABLE,
        "the display helper was started but never answered on its socket — check `adb shell \
         logcat -s syscity` on the device",
    ))
}

/// Whether adb has a device it can actually talk to, and what to do when it
/// does not.
///
/// `has_adb()` only answers "is the binary there". Between that and a usable
/// device are two states a person can fix in seconds but a stack trace cannot
/// explain: not paired at all, and paired but not authorised.
fn connected_device_note(devices: &[Value]) -> Result<(), String> {
    let state_of = |device: &Value| {
        device
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string()
    };
    if devices.iter().any(|device| state_of(device) == "device") {
        return Ok(());
    }
    let states: Vec<String> = devices.iter().map(state_of).collect();
    Err(match states.first().map(String::as_str) {
        None => "the app's bundled adb has no device connected, so nothing can be run on the \
                phone. Pair it once in Syscity's pairing screen (Settings › System › Developer \
                options › Wireless debugging), or `adb connect` to it; the display helper is \
                started through that connection."
            .to_string(),
        Some("unauthorized") => "adb sees the phone but this computer is not authorised: accept \
                                 the “Allow USB debugging?” prompt on the phone, then try again."
            .to_string(),
        Some("offline") => "adb reports the phone as offline — usually a stale connection. Turn \
                            wireless debugging off and on (or pair again), then try again."
            .to_string(),
        Some(other) => format!(
            "adb cannot use the phone (state {other}); pair it again in Syscity's pairing screen."
        ),
    })
}

/// Which connected device to address when the caller did not name one.
///
/// The loopback serial (`localhost:…` / `127.0.0.1:…`) is the connection the
/// app paired with — its own phone. A single device is unambiguous however it
/// is named; several with no loopback among them is left to adb to complain
/// about, because guessing there would mean running a command on someone
/// else's device.
fn preferred_serial(devices: &[Value]) -> Option<String> {
    let usable = devices
        .iter()
        .filter(|device| device.get("state").and_then(Value::as_str) == Some("device"));
    let serials: Vec<String> = usable
        .filter_map(|device| device.get("serial").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    serials
        .iter()
        .find(|serial| serial.starts_with("localhost:") || serial.starts_with("127.0.0.1:"))
        .cloned()
        .or_else(|| {
            if serials.len() == 1 {
                serials.first().cloned()
            } else {
                None
            }
        })
}

/// The installed APK path for `package`, from `pm path`.
async fn apk_path(device: &Option<String>, package: &str) -> Result<String, DisplayError> {
    let args = super::android::adb_args(device, &["shell", "pm", "path", package]);
    let (status, stdout, stderr) =
        super::run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
            .await
            .map_err(|e| {
                DisplayError::new(
                    DISPLAY_UNAVAILABLE,
                    format!("could not ask the device for {package}'s APK ({e})"),
                )
            })?;
    if !status.success() {
        return Err(DisplayError::new(
            DISPLAY_UNAVAILABLE,
            format!("pm path {package} failed: {}", stderr.trim()),
        ));
    }
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("package:"))
        .map(str::to_string)
        .ok_or_else(|| {
            DisplayError::new(
                DISPLAY_UNAVAILABLE,
                format!("{package} is not installed, so the display helper cannot be started"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::tests::MockDeviceBridge;
    use std::sync::Arc;

    fn bridge(answer: Value) -> DeviceBridgeRef {
        Arc::new(MockDeviceBridge::new(answer))
    }

    #[test]
    fn the_launch_command_runs_the_helper_as_the_shell() {
        let command = launch_command("/data/app/~~x/base.apk");
        assert!(command.starts_with("CLASSPATH=/data/app/~~x/base.apk app_process /system/bin"));
        assert!(command.ends_with(HELPER_CLASS), "{command}");
    }

    #[tokio::test]
    async fn a_missing_helper_is_unavailable_not_a_crash() {
        struct Down;
        #[async_trait::async_trait]
        impl crate::device::DeviceBridge for Down {
            async fn call(&self, _command: &str, _payload: Value) -> crate::Result<Value> {
                Err(crate::error::SyscityError::Internal("no socket".into()))
            }
        }
        let bridge: DeviceBridgeRef = Arc::new(Down);
        let failed = VirtualDisplay::status(&bridge).await.expect_err("down");
        assert_eq!(failed.code, DISPLAY_UNAVAILABLE);
        assert!(failed.message.contains("app_process"), "{}", failed.message);
        assert!(!available(&bridge).await);
    }

    #[tokio::test]
    async fn a_refusal_is_reported_with_its_own_code() {
        let bridge = bridge(json!({ "ok": false, "code": "NO_SHELL", "error": "not a shell" }));
        let failed = VirtualDisplay::status(&bridge).await.expect_err("refused");
        assert_eq!(failed.code, DISPLAY_HELPER_ERROR);
        assert!(failed.message.contains("NO_SHELL"), "{}", failed.message);
    }

    #[tokio::test]
    async fn an_answer_without_ok_is_a_bad_response() {
        let bridge = bridge(json!({ "display": 3 }));
        let failed = VirtualDisplay::status(&bridge).await.expect_err("bad");
        assert_eq!(failed.code, DISPLAY_BAD_RESPONSE);
    }

    #[tokio::test]
    async fn opening_a_display_returns_its_id_and_size() {
        let bridge = bridge(json!({ "ok": true, "display": 7, "width": 1080, "height": 2400 }));
        let display = VirtualDisplay::open(bridge, Some((1080, 2400)), None)
            .await
            .expect("display");
        assert_eq!(display.id, 7);
        assert_eq!((display.width, display.height), (1080, 2400));
    }

    #[tokio::test]
    async fn a_display_without_an_id_is_rejected() {
        let bridge = bridge(json!({ "ok": true, "width": 1080, "height": 2400 }));
        let failed = match VirtualDisplay::open(bridge, None, None).await {
            Ok(display) => panic!("a display without an id must be rejected, got {}", display.id),
            Err(failed) => failed,
        };
        assert_eq!(failed.code, DISPLAY_BAD_RESPONSE);
    }

    #[tokio::test]
    async fn a_frame_is_decoded_from_base64() {
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n");
        let bridge = bridge(json!({ "ok": true, "png": png, "width": 10, "height": 20 }));
        let display = VirtualDisplay {
            bridge,
            id: 1,
            width: 10,
            height: 20,
        };
        let frame = display.screenshot().await.expect("frame");
        assert_eq!(frame.png, b"\x89PNG\r\n");
        assert_eq!((frame.width, frame.height), (10, 20));
    }

    #[test]
    fn the_loopback_serial_wins_when_several_are_connected() {
        let devices = vec![
            json!({ "serial": "emulator-5554", "state": "device" }),
            json!({ "serial": "localhost:5555", "state": "device" }),
        ];
        assert_eq!(preferred_serial(&devices).as_deref(), Some("localhost:5555"));
    }

    #[test]
    fn a_single_device_is_taken_however_it_is_named() {
        let devices = vec![json!({ "serial": "ABC123", "state": "device" })];
        assert_eq!(preferred_serial(&devices).as_deref(), Some("ABC123"));
        let offline = vec![json!({ "serial": "ABC123", "state": "offline" })];
        assert_eq!(preferred_serial(&offline), None);
    }

    #[test]
    fn several_devices_without_a_loopback_are_left_to_adb() {
        let devices = vec![
            json!({ "serial": "emulator-5554", "state": "device" }),
            json!({ "serial": "ABC123", "state": "device" }),
        ];
        assert_eq!(preferred_serial(&devices), None);
    }

    #[test]
    fn a_connected_device_passes_quietly() {
        let devices = vec![json!({ "serial": "127.0.0.1:5555", "state": "device" })];
        assert!(connected_device_note(&devices).is_ok());
    }

    #[test]
    fn no_device_says_how_to_pair() {
        let note = connected_device_note(&[]).expect_err("no device");
        assert!(note.contains("pair"), "{note}");
        assert!(note.contains("Wireless debugging"), "{note}");
    }

    #[test]
    fn unauthorized_says_to_accept_the_prompt() {
        let devices = vec![json!({ "serial": "127.0.0.1:5555", "state": "unauthorized" })];
        let note = connected_device_note(&devices).expect_err("unauthorised");
        assert!(note.contains("Allow USB debugging"), "{note}");
    }

    #[test]
    fn offline_says_it_is_a_stale_connection() {
        let devices = vec![json!({ "serial": "127.0.0.1:5555", "state": "offline" })];
        let note = connected_device_note(&devices).expect_err("offline");
        assert!(note.contains("offline"), "{note}");
    }

    #[tokio::test]
    async fn attach_addresses_an_existing_display_by_id() {
        let bridge = bridge(json!({ "ok": true }));
        let display = VirtualDisplay::attach(bridge, 9);
        assert_eq!(display.id, 9);
        // Size is unknown until a frame reports it — the id is the address.
        assert_eq!((display.width, display.height), (0, 0));
    }

    #[tokio::test]
    async fn a_frame_that_is_not_base64_is_rejected() {
        let bridge = bridge(json!({ "ok": true, "png": "not base64 !!" }));
        let display = VirtualDisplay {
            bridge,
            id: 1,
            width: 1,
            height: 1,
        };
        let failed = display.screenshot().await.expect_err("bad png");
        assert_eq!(failed.code, DISPLAY_BAD_RESPONSE);
    }
}
