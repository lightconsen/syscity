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
pub const HELPER_CLASS: &str = "net.syscity.desktop.shell.HelperMain";

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
        // The helper wrote a PNG and told us where. Reading the file beats
        // base64 over the bridge by a copy per frame, which is the difference
        // between a few hundred microseconds and a few milliseconds once a
        // long task is taking one observation per step.
        let path = answer
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| DisplayError::new(DISPLAY_BAD_RESPONSE, "the frame carried no path"))?;
        let png = std::fs::read(path).map_err(|e| {
            DisplayError::new(
                DISPLAY_BAD_RESPONSE,
                format!("the frame at {path} could not be read ({e})"),
            )
        })?;
        let (width, height) = super::android::png_dimensions(&png).unwrap_or((0, 0));
        Ok(DisplayFrame { png, width, height })
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

    /// What the user's own screen has on top, as `(taskId, package)`.
    ///
    /// `None` when there is nothing to report. An action that starts an app
    /// from a background service — a share sheet, a chooser, an app that
    /// ignores `--display` — lands it here, not on the agent's display, and
    /// from the agent's side that is indistinguishable from "nothing happened".
    pub async fn main_screen_top(&self) -> Result<Option<(i32, Option<String>)>, DisplayError> {
        let answer = call(&self.bridge, json!({ "op": "main_screen_top" })).await?;
        let raw = answer.get("top").and_then(Value::as_str).unwrap_or("");
        if raw.is_empty() {
            return Ok(None);
        }
        let (task, package) = match raw.split_once('|') {
            Some((task, package)) => (task, package),
            None => (raw, ""),
        };
        let task_id = task.parse::<i32>().map_err(|_| {
            DisplayError::new(
                DISPLAY_BAD_RESPONSE,
                format!("the helper reported a task that is not a number ({task})"),
            )
        })?;
        let package = (!package.is_empty()).then(|| package.to_string());
        Ok(Some((task_id, package)))
    }

    /// Pull a task from the user's screen back onto this display.
    pub async fn move_task_here(&self, task_id: i32) -> Result<(), DisplayError> {
        call(&self.bridge, json!({ "op": "move_task", "display": self.id, "task": task_id }))
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
                    "no display helper is attached. It is started by `ensure_helper` through \
                     the loopback-ADB pairing, and it hands its Binder back — a helper started \
                     by hand cannot do that ({e})"
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

/// The command that starts the helper, given the app's APK path.
///
/// The helper is `app_process`, which inherits the identity of whatever started
/// it — `adb shell`, uid 2000 — and that identity is the whole point:
/// `CAPTURE_VIDEO_OUTPUT` is unreachable from an app.
///
/// It cannot then talk to the app over a socket: SELinux forbids an
/// `untrusted_app` opening a unix socket in the `shell` domain. It hands a
/// Binder to a ContentProvider the app exports instead, which is the direction
/// the policy allows.
///
/// The nonce arrives in an **environment variable**, not on stdin and not in
/// argv. argv is out because `/proc/<pid>/cmdline` is world-readable. Stdin is
/// out because this command is passed through `adb shell sh -c "…"`, and the
/// quoting `$n` needs to survive two shells here is exactly the kind of thing
/// that silently becomes the empty string — which is what happened. `adb`
/// forwards `VAR=value` prefixes to the device shell's environment directly, so
/// the value needs no quoting at all.
///
/// `setsid` detaches the helper from the adb stream's process group, which
/// `adbd` kills when the stream closes; the trailing `read _` holds `sh` open
/// until then, so `setsid` has taken effect first.
pub fn launch_command(apk_path: &str, package: &str, app_uid: u32, nonce: &str) -> String {
    format!(
        // No `--nice-name`: on this platform it goes through the same
        // `setuid` path as `--uid`, which SELinux refuses even for a root adbd
        // (`failed to change user ID: operation not permitted`). The process
        // name is a log nicety; starting at all is not.
        "CLASSPATH={apk} setsid env SYSCITY_HANDOFF_NONCE={nonce} \
SYSCITY_HANDOFF_PACKAGE={pkg} app_process /system/bin {class} {pkg} {app_uid} \
>/dev/null 2>&1 & sleep 0.2",
        nonce = sh_quote(nonce),
        apk = sh_quote(apk_path),
        class = HELPER_CLASS,
        pkg = sh_quote(package),
    )
}

/// Ask the app's hub for a nonce it will accept.
async fn handoff_nonce(bridge: &DeviceBridgeRef) -> Result<String, DisplayError> {
    let answer = bridge
        .call(CMD_VIRTUAL_DISPLAY, json!({ "op": "nonce" }))
        .await
        .map_err(|e| {
            DisplayError::new(
                DISPLAY_UNAVAILABLE,
                format!("the app could not issue a handoff nonce ({e})"),
            )
        })?;
    answer
        .get("nonce")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| DisplayError::new(DISPLAY_BAD_RESPONSE, "the app returned no handoff nonce"))
}

/// A fresh single-use nonce, 32 random bytes as hex.
///
/// `rand` is already a dependency (jitter, ids); this uses the OS source
/// through it rather than pulling in `getrandom` directly.
pub fn new_nonce() -> String {
    use rand::RngExt;
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes[..]);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Quote a string as one shell word.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
    let app_uid = app_uid(&device, package).await?;
    // The nonce comes from the app's hub, which is what will accept it. Minting
    // one here produced a value the hub had never issued, and the handoff was
    // refused as `BadNonce` — correctly.
    let nonce = handoff_nonce(bridge).await?;
    let command = launch_command(&apk, package, app_uid, &nonce);

    // `sh -c` with the script as one word: it contains `&` and a pipeline, and
    // handed over as separate argv entries it would be split locally. `adb` as
    // argv[0] because `adb_args` returns only the arguments — `run_cmd`
    // prepends the program name, but this builds the request by hand.
    let mut argv: Vec<String> = vec!["adb".into()];
    argv.extend(super::android::adb_args(&device, &["shell", &command]));
    let request = crate::tools::process_runner::ProcessRequest::argv(
        &argv.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let (status, _, stderr) = super::run_cmd_with(&request).await.map_err(|e| {
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

    // The helper calls back in when it is ready. Poll the bridge — the handoff
    // happens on the Kotlin side.
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

/// The uid the app runs as, from `pm list packages -U`.
///
/// Not `stat /data/data/<pkg>`: that directory has not been readable by the
/// shell since Android 9, and this runs *as* the shell — the obvious call
/// answers "Permission denied".
async fn app_uid(device: &Option<String>, package: &str) -> Result<u32, DisplayError> {
    let args =
        super::android::adb_args(device, &["shell", "pm", "list", "packages", "-U", package]);
    let (status, stdout, stderr) =
        super::run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
            .await
            .map_err(|e| {
                DisplayError::new(
                    DISPLAY_UNAVAILABLE,
                    format!("could not read {package}'s uid ({e})"),
                )
            })?;
    if !status.success() {
        return Err(DisplayError::new(
            DISPLAY_UNAVAILABLE,
            format!("could not read {package}'s uid: {}", stderr.trim()),
        ));
    }
    // `package:net.syscity.desktop uid:10207`
    stdout
        .split_whitespace()
        .find_map(|field| field.strip_prefix("uid:"))
        .and_then(|uid| uid.parse().ok())
        .ok_or_else(|| {
            DisplayError::new(
                DISPLAY_UNAVAILABLE,
                format!("{package} reported no uid ({})", stdout.trim()),
            )
        })
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
    fn the_launch_command_detaches_and_carries_the_nonce() {
        let command =
            launch_command("/data/app/~~x/base.apk", "net.syscity.desktop", 10207, "deadbeef");
        // Detached: adbd kills the stream's process group when it closes, and
        // the stream closes as soon as the handoff succeeds.
        assert!(command.contains("setsid env"), "{command}");
        assert!(command.contains(HELPER_CLASS), "{command}");
        assert!(command.contains("sleep 0.2"), "{command}");
        // The nonce travels in the environment, once, through `env` — a bare
        // prefix would apply to `sh` and not reach the program `setsid` execs.
        assert_eq!(command.matches("SYSCITY_HANDOFF_NONCE=").count(), 1, "{command}");
        assert!(command.contains("app_process /system/bin"), "{command}");
        assert!(command.contains("10207"), "{command}");
    }

    #[test]
    fn the_nonce_travels_in_the_environment_not_argv_or_stdin() {
        let command = launch_command("/data/app/x/base.apk", "net.syscity.desktop", 1, "abc123");
        // `env` after `setsid`: a plain VAR=value prefix applies to `sh`, and
        // `setsid` does not pass it on to the program it execs.
        assert!(command.contains("setsid env SYSCITY_HANDOFF_NONCE='abc123'"), "{command}");
        // Nothing else may carry it: /proc cmdline is world-readable, and stdin
        // does not survive the quoting this string goes through.
        assert_eq!(command.matches("abc123").count(), 1, "{command}");
    }

    #[test]
    fn a_nonce_is_32_random_bytes_as_hex() {
        let first = new_nonce();
        let second = new_nonce();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second, "two nonces must not collide");
    }

    #[test]
    fn a_quote_in_the_apk_path_cannot_break_out_of_the_command() {
        let command = launch_command("/data/app/a'b/base.apk", "pkg", 1, "n");
        // The classic single-quote escape: close, emit an escaped quote, reopen.
        assert!(command.contains("'\\''"), "{command}");
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
        // The hint names where the helper comes from, not a command the
        // caller cannot run: the launch needs a nonce only the app knows.
        assert!(failed.message.contains("ensure_helper"), "{}", failed.message);
        assert!(failed.message.contains("Binder"), "{}", failed.message);
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
    async fn a_frame_is_read_from_the_path_the_helper_named() {
        // A real 1x1 PNG header is enough: the size comes from the file, not
        // from the answer, so a wrong width in the payload cannot mislead.
        let dir = std::env::temp_dir().join(format!("syscity-frame-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("frame-1.png");
        // IHDR: width 3, height 5.
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&[0, 0, 0, 13, b'I', b'H', b'D', b'R']);
        png.extend_from_slice(&3u32.to_be_bytes());
        png.extend_from_slice(&5u32.to_be_bytes());
        std::fs::write(&path, &png).expect("write frame");

        let bridge = bridge(json!({ "ok": true, "path": path.to_string_lossy() }));
        let display = VirtualDisplay::attach(bridge, 1);
        let frame = display.screenshot().await.expect("frame");
        assert_eq!(frame.png, png);
        assert_eq!((frame.width, frame.height), (3, 5));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_frame_path_that_does_not_exist_is_a_bad_response() {
        let bridge = bridge(json!({ "ok": true, "path": "/nonexistent/frame.png" }));
        let display = VirtualDisplay::attach(bridge, 1);
        let failed = display.screenshot().await.expect_err("missing file");
        assert_eq!(failed.code, DISPLAY_BAD_RESPONSE);
        assert!(failed.message.contains("could not be read"), "{}", failed.message);
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
