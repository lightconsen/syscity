//! Android device control via ADB.
//!
//! Provides screenshot, tap, swipe, text input, key events, app installation,
//! launch, force-stop, and a structured UI element list.
//!
//! Observation returns an indexed element list parsed from the platform's
//! `uiautomator dump`, not the raw XML: the dump is an order of magnitude
//! larger, and the model would otherwise have to parse `bounds` to act on it.
//! Text input escapes for the *device* shell (a plain `input text '<text>'`
//! breaks on an embedded quote) and routes non-ASCII through ADBKeyboard or,
//! failing that, through a clipboard paste by the phone's own helper — `input
//! text` types through the key character map and cannot carry it.
//!
//! Two things make the loop shorter rather than just correct. Every action is
//! followed by a re-read that answers "did the screen change" ([`EFFECT_CHANGED`]
//! / [`EFFECT_UNCHANGED`] / [`EFFECT_UNKNOWN`]), so the model does not need a
//! separate observation to learn what its tap did. And every tree read is judged
//! ([`assess_tree`]): a screen the accessibility API describes nothing useful on
//! falls back to OCR instead of handing back an empty list that reads like a
//! blank screen.

use async_trait::async_trait;
use serde_json::Value;

use super::{has_adb, run_cmd, run_cmd_bytes};
use crate::computer::platform::{OsControlScope, PlatformConstraints, PlatformToolSet};
use crate::tools::target_lock::TargetLease;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

// ── Android Capability Set ─────────────────────────────────────────────────

/// Android device platform tool set (requires `adb` on PATH).
pub struct AndroidToolset;

impl Default for AndroidToolset {
    fn default() -> Self {
        Self::new()
    }
}

impl AndroidToolset {
    pub fn new() -> Self {
        Self
    }
}

impl PlatformToolSet for AndroidToolset {
    fn id(&self) -> &str {
        "android"
    }

    fn name(&self) -> &str {
        "Android Device Bridge"
    }

    fn description(&self) -> &str {
        "Control Android devices via ADB: screenshot, tap, type, install, launch apps"
    }

    fn constraints(&self) -> &PlatformConstraints {
        // Availability is determined by `has_adb()` at runtime.
        static CONSTRAINTS: std::sync::OnceLock<PlatformConstraints> = std::sync::OnceLock::new();
        CONSTRAINTS.get_or_init(|| PlatformConstraints {
            target_os: Vec::<String>::new(), // any OS
            requires_gui: false,
            requires_services: Vec::<String>::new(),
        })
    }

    fn scope(&self) -> OsControlScope {
        OsControlScope::UserSpace
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(AdbObserveTool::new()),
            Box::new(AdbScreenshotTool::new()),
            Box::new(AdbInputTool::new()),
            Box::new(AdbAppManagerTool::new()),
            Box::new(AdbUiTreeTool::new()),
            Box::new(AdbVerifyTool::new()),
            // Higher-level tools: one call where the model would otherwise
            // spend several turns reading, scrolling and tapping.
            Box::new(super::android_tools::AdbCollectTool::new()),
            Box::new(super::android_tools::AdbFindTool::new()),
            // The two cheap reads: find one control without reading the whole
            // tree, and wait for the screen instead of observing it again.
            Box::new(super::android_tools::AdbQueryTool::new()),
            Box::new(super::android_tools::AdbWaitTool::new()),
            // One call for several known steps. Each step still goes through
            // `android_input`/`android_app_manager`, so nothing a single call
            // checks is skipped — what it saves is the model's turns.
            Box::new(super::android_tools::AdbRunTool::new()),
            // The same screen, read as data rather than as tap targets.
            Box::new(super::android_tools::AdbExtractTool::new()),
            // What the phone says has happened, without reading the screen:
            // windows, content changes and notifications, as facts.
            Box::new(super::android_tools::AdbEventTool::new()),
            // One call to turn a wheel (a time, a date, a region): the control
            // a model otherwise drives one tap at a time.
            Box::new(super::android_tools::AdbSetWheelTool::new()),
            Box::new(super::android_tools::AdbDialogTool::new()),
            Box::new(super::android_tools::AdbSystemActionTool::new()),
            Box::new(super::android_tools::AdbOcrTool::new()),
            // The second screen: the user keeps the phone while a task runs,
            // and a step they have to do themselves goes back to them.
            Box::new(super::android_tools::AdbDisplayTool::new()),
            Box::new(super::android_tools::AdbHandToUserTool::new()),
            // Loopback self-pairing (§4.5): pair the phone with its own
            // wireless-debugging adbd and report pairing state.
            Box::new(AdbPairTool::new()),
            Box::new(AdbStatusTool::new()),
        ]
    }

    fn is_available(&self) -> bool {
        has_adb()
    }
}

// ── Device targeting ───────────────────────────────────────────────────────

/// Schema text for the `device` argument the targeting tools share.
/// Shared description for the `display` argument.
pub(super) const DISPLAY_PARAM: &str =
    "Optional: id from android_display start. Omit to act on the phone's own screen.";

pub(super) const DEVICE_PARAM: &str =
    "Device serial to act on. Omit it and the call uses the only attached device — the result \
     reports which serial was actually used, since that is whatever is attached at the time.";

pub(super) const SINCE_PARAM: &str =
    "Optional: the `snapshot` of an earlier read, to get only what changed since then (what \
     appeared, left, moved) instead of the whole screen. Falls back to the whole screen when the \
     named read is not one of the last few, was of a different app, or when more than half the \
     screen is different — the result says which.";

/// `adb` argv prefix for the target device — no `-s` when none is pinned, in
/// which case adb resolves the device itself.
pub(super) fn adb_args(device: &Option<String>, base: &[&str]) -> Vec<String> {
    let mut args = Vec::with_capacity(base.len() + 2);
    if let Some(serial) = device {
        args.push("-s".to_string());
        args.push(serial.clone());
    }
    args.extend(base.iter().map(|s| s.to_string()));
    args
}

/// The device a call acts on: the caller's argument when given, else the serial
/// the toolset was configured with.
///
/// Omitting `device` does not mean "any device" — adb resolves it to whichever
/// single device is attached at that moment, so a call made after a swap lands
/// on the new one without saying so. Passing it pins this call's target.
pub(super) fn resolve_device(
    args: &Value,
    configured: Option<&str>,
) -> Result<Option<String>, String> {
    match args.get("device") {
        // Absent, or explicitly null: fall back to the configured serial.
        None | Some(Value::Null) => Ok(configured.map(str::to_string)),
        Some(Value::String(serial)) if !serial.trim().is_empty() => {
            Ok(Some(serial.trim().to_string()))
        }
        // An empty or non-string serial must not quietly become "no device":
        // that sends the command to a target the caller did not ask for, which
        // is the whole failure this argument exists to prevent.
        Some(Value::String(_)) => {
            Err("`device` was given but is empty — omit it to use the only attached device"
                .to_string())
        }
        Some(other) => Err(format!("`device` must be a serial string, got {other}")),
    }
}

/// The serial this call actually addressed, for the result to report.
///
/// A pinned serial is known for free; otherwise ask adb. Naming the device is
/// what turns a silent swap into a visible line in the transcript. Best-effort
/// by design — nothing about labelling the target is worth failing a call over —
/// so a failure here surfaces as a null in the result rather than as silence.
pub(super) async fn resolved_serial(device: &Option<String>) -> Option<String> {
    if let Some(serial) = device {
        return Some(serial.clone());
    }
    let (status, stdout, _) = run_cmd("adb", &["get-serialno"]).await.ok()?;
    if !status.success() {
        return None;
    }
    let serial = stdout.trim();
    if serial.is_empty() || serial == "unknown" {
        None
    } else {
        Some(serial.to_string())
    }
}

/// The key to hold while acting on `serial`.
///
/// A pinned serial is a phone of its own, and two phones on one host are not
/// each other's problem. Without a serial the call addresses whatever adb has
/// attached, which is not a device this process can name — so it takes the
/// kind-wide lock, which is exactly what the old coarse key was, and for the
/// right reason.
pub(crate) fn device_lock_key(device: &Option<String>) -> String {
    match device {
        Some(serial) => crate::tools::target_lock::android_device(serial),
        None => crate::tools::target_lock::ANDROID.to_string(),
    }
}

/// Wait for exclusive use of the device this call is about to address.
///
/// `&str` rather than the key itself: the guard does not borrow it, so the key
/// may be built in the match arm that resolves the device.
///
/// Taken across processes: two `adb` drivers on one host — the gateway and the
/// CLI — are two writers to one phone. See [`TargetLease`].
pub(super) async fn lock_device(device: &Option<String>) -> TargetLease {
    let key = device_lock_key(device);
    crate::tools::target_lock::TargetLocks::global()
        .lock_across_processes(&key)
        .await
}

/// Wait for exclusive use of the thing this call is about to address.
///
/// The user's screen has one pointer and one focus, so a write to it takes the
/// device. A virtual display has a focus of its own: two of them are two
/// places, and making them wait for each other would be cost with no
/// correctness behind it. The app they share is guarded separately, by
/// [`lease_app`].
pub(super) async fn lock_target(device: &Option<String>, target: Target) -> TargetLease {
    match target {
        Target::Screen => lock_device(device).await,
        Target::Display(id) => {
            let key = match device {
                Some(serial) => crate::tools::target_lock::android_display(serial, id),
                None => format!("android-display:any:{id}"),
            };
            crate::tools::target_lock::TargetLocks::global()
                .lock_across_processes(&key)
                .await
        }
    }
}

/// Wait for exclusive use of one app on one phone, for the length of a write.
///
/// The device lock serialises input on a phone, and on the user's screen that
/// is all there is to it: one focus, one screen. A virtual display is not the
/// user's screen though — it has its own focus — and the helper lets two of
/// them run at once. Two tasks can then drive the *same app* on *two displays*
/// of one phone, which the display-side checks cannot see: the app is one
/// process, one account, one set of unsaved edits, and two writers interleaving
/// in it produce a state neither of them would have reached alone.
///
/// So a write also takes this, keyed by `(serial, package)`. It is taken after
/// the pre-action read (the package is what that read names) and released when
/// the action finishes. Order is always the display-or-device lock first and
/// this second, so the two cannot deadlock.
///
/// [`stale`](AppLease::stale) covers the gap the ordering leaves: the read that
/// named the package happened *before* the lock, so another write to the same
/// app may have landed in between. When it says so, the caller re-reads.
pub(super) type AppLease = crate::tools::target_lock::AppLease;

/// Take the app lease for the package the pre-action read named.
///
/// An unreadable tree names no package, and there is no key to take: the write
/// proceeds under the display lock only, as it did before. That is a real gap
/// — a blind action is exactly one you cannot serialise — but inventing a key
/// would serialise every app on the phone against every other, which is the
/// coarse lock this whole module is trying not to be.
pub(super) async fn lease_app(
    device: &Option<String>,
    package: Option<&str>,
    seen_completed: u64,
) -> AppLease {
    let Some(package) = package.map(str::trim).filter(|p| !p.is_empty()) else {
        return AppLease::none();
    };
    let key = crate::tools::target_lock::android_app(device.as_deref().unwrap_or("-"), package);
    crate::tools::target_lock::AppGates::global()
        .lease(&key, seen_completed)
        .await
}

/// How many writes to `package` have finished, for [`lease_app`]'s staleness
/// check. Zero when no package is known, which never reports stale.
pub(super) fn app_completed(device: &Option<String>, package: Option<&str>) -> u64 {
    let Some(package) = package.map(str::trim).filter(|p| !p.is_empty()) else {
        return 0;
    };
    crate::tools::target_lock::AppGates::global().completed(
        &crate::tools::target_lock::android_app(device.as_deref().unwrap_or("-"), package),
    )
}

// ── ADB Screenshot Tool ────────────────────────────────────────────────────

/// Capture device screen via `adb exec-out screencap -p`.
#[derive(Debug)]
pub struct AdbScreenshotTool {
    device: Option<String>,
}

impl Default for AdbScreenshotTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbScreenshotTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbScreenshotTool {
    fn name(&self) -> &str {
        "android_screenshot"
    }

    fn description(&self) -> &str {
        "Capture a screenshot of the connected Android device and return it as base64 PNG."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Capture Android device screenshot",
            serde_json::json!({
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // Resolved before the lock, because the lock is the device: a call that
        // names a serial does not wait for one that names another.
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let _target = lock_device(&device).await;
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let png = match capture_png(&device, target).await {
            Ok(png) => png,
            Err(e) => return Ok(ToolExecutionResult::error(e.to_string())),
        };
        let base64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);

        Ok(
            ToolExecutionResult::success("Screenshot captured").with_data(serde_json::json!({
                "base64": base64,
                "format": "png",
                "device": resolved_serial(&device).await,
            })),
        )
    }
}

/// What an action *is*, for the repetition guard.
///
/// Not the arguments: a tap by index and the same tap by key are the same
/// action, and a model that alternates between the two would otherwise reset
/// the count while doing exactly the same thing. The signature names the
/// element where one is known, so scrolling the same list and tapping the same
/// row is still "the same action", and separately names the operation where it
/// is not.
fn action_signature(op: &InputOp, before: Option<&[UiElement]>) -> String {
    match op {
        InputOp::Taps(points) if points.len() == 1 => {
            let (x, y) = points[0];
            let label = before
                .and_then(|elements| {
                    elements
                        .iter()
                        .filter(|element| {
                            let (x1, y1, x2, y2) = element.bounds;
                            x >= x1 && y >= y1 && x <= x2 && y <= y2
                        })
                        .min_by_key(|element| {
                            let (x1, y1, x2, y2) = element.bounds;
                            (x2 - x1) as i64 * (y2 - y1) as i64
                        })
                })
                .and_then(element_label)
                .unwrap_or("");
            if label.is_empty() {
                format!("tap@{x},{y}")
            } else {
                format!("tap:{label}")
            }
        }
        // A sequence is several taps in one call; what it *is* is the gesture
        // as a whole, so two identical sequences are the same repeat.
        InputOp::Taps(points) => format!("sequence@{}", points.len()),
        // Both ends: a short nudge and a long drag are different actions, and
        // calling them the same repeat would refuse a gesture that had not yet
        // been tried.
        InputOp::Swipe { from, to } => {
            format!("swipe@{}:{}>{}:{}", from.0, from.1, to.0, to.1)
        }
        InputOp::Text(text) => format!("text:{}", text.chars().take(40).collect::<String>()),
        InputOp::Key(keycap) => format!("key:{keycap}"),
        InputOp::Gesture { paths, .. } => format!("gesture@{}", paths.len()),
    }
}

// ── Touch tracking ─────────────────────────────────────────────────────────

/// One change to the screen, as the post-action check saw it.
///
/// `effect` is [`EFFECT_CHANGED`], [`EFFECT_UNCHANGED`] or [`EFFECT_UNKNOWN`],
/// as the post-action check reported it.
pub(super) fn guard_outcome(effect: &str) -> super::guard::Outcome {
    match effect {
        EFFECT_CHANGED => super::guard::Outcome::Changed,
        EFFECT_UNCHANGED => super::guard::Outcome::Unchanged,
        _ => super::guard::Outcome::Unknown,
    }
}

/// What the model is told when it is on a lower rung of the ladder.
fn ladder_note(verdict: &super::guard::Verdict) -> Option<String> {
    verdict.note().map(str::to_string)
}

// ── ADB Observe Tool ───────────────────────────────────────────────────────

/// Screenshot **and** UI tree from one call, with the coordinate space made
/// explicit.
///
/// Two separate adb reads cannot be simultaneous, but doing them back to back in
/// one call shrinks the window — and, more usefully, the result states what was
/// actually captured. If the screenshot's real pixel size and the rectangle the
/// element tree describes disagree (a rotation, a scaled capture), then every
/// coordinate in the tree is wrong for the image the model is looking at, and it
/// is told so instead of discovering it by tapping the wrong place.
#[derive(Debug)]
pub struct AdbObserveTool {
    device: Option<String>,
}

impl Default for AdbObserveTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbObserveTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbObserveTool {
    fn name(&self) -> &str {
        "android_observe"
    }

    fn description(&self) -> &str {
        "Look at the Android device: returns the numbered actionable elements and a screenshot \
         together, so the elements and the image describe the same moment. Preferred over \
         calling android_ui_tree and android_screenshot separately. Says so when the two \
         disagree about the screen size, which would make the element coordinates unusable. When \
         the UI tree cannot be read at all it still returns the screenshot and says why: in that \
         case the elements are absent from the result rather than empty, because an empty list \
         means the tree was read and held nothing actionable. Pass `since` with the snapshot of \
         an earlier read to get only what changed instead of the whole list; the screenshot is \
         still returned, because a canvas can change while the tree stays silent."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Observe the Android device",
            serde_json::json!({
                "since": { "type": "string", "description": SINCE_PARAM },
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // Resolved before the lock, because the lock is the device: a call that
        // names a serial does not wait for one that names another.
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let _target = lock_device(&device).await;
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let since = super::diff::since_arg(&args).map(str::to_string);
        let png = match capture_png(&device, target).await {
            Ok(png) => png,
            Err(e) => return Ok(ToolExecutionResult::error(e.to_string())),
        };
        let as_size = |dims: Option<(u32, u32)>| {
            dims.map(|(w, h)| serde_json::json!({ "width": w, "height": h }))
        };

        let dump = match dump_ui(&device, target).await {
            Ok(dump) => dump,
            // A tree that cannot be read does not fail this call. The screenshot
            // is already in hand and is most of what the tool is for, and every
            // message about a failed dump tells the model to work from the
            // screenshot that android_observe returns — which an error result
            // would throw away, since its payload does not reach the model. So
            // the observation comes back with the image, no elements, and the
            // failure's code for a caller to branch on.
            Err(failure) => {
                let shot = png_dimensions(&png);
                let size = shot
                    .map(|(w, h)| format!("{w}x{h}"))
                    .unwrap_or_else(|| "unknown size".to_string());

                let mut data = serde_json::Map::new();
                data.insert("tree_available".to_string(), serde_json::json!(false));
                data.insert("code".to_string(), serde_json::json!(failure.code));
                data.insert(
                    "screenshot".to_string(),
                    as_size(shot).unwrap_or(serde_json::Value::Null),
                );
                data.insert(
                    "screenshot_base64".to_string(),
                    serde_json::json!(base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &png,
                    )),
                );
                data.insert(
                    "device".to_string(),
                    serde_json::json!(resolved_serial(&device).await),
                );

                return Ok(ToolExecutionResult::success(format!(
                    "Screenshot captured ({size}), but the UI tree could not be read, so there are \
                     no elements in this result — absent, not empty. {}",
                    failure.message
                ))
                .with_data(serde_json::Value::Object(data)));
            }
        };

        // A delta replaces the element list, not the screenshot: the picture is
        // how a caller learns that a canvas changed while the tree stayed
        // silent, so a delta answer carries the image too. A *fallback* is left
        // to the normal path below, which runs the quality check and OCR — the
        // stricter read, and the one that would otherwise be skipped.
        let mut fallback = None;
        let since_delta = match since_answer(device.as_deref(), target, since.as_deref(), &dump) {
            Some(SinceAnswer::Delta { text, data }) => Some((text, data)),
            Some(SinceAnswer::Full(reason)) => {
                fallback = Some(reason);
                None
            }
            None => None,
        };
        let shot = png_dimensions(&png);
        if let Some((text, mut data)) = since_delta {
            let quality = assess_tree(&dump.elements);
            data["quality"] = serde_json::json!(quality.as_str());
            data["screenshot_base64"] = serde_json::json!(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &png,
            ));
            data["screenshot"] = as_size(shot).unwrap_or(serde_json::Value::Null);
            data["screen"] = serde_json::json!(dump
                .screen
                .map(|(w, h)| serde_json::json!({ "width": w, "height": h })));
            data["coordinates_consistent"] =
                serde_json::json!(coordinate_space_consistent(dump.screen, shot));
            data["source"] = serde_json::json!(dump.source);
            data["device"] = serde_json::json!(resolved_serial(&device).await);
            return Ok(ToolExecutionResult::success(text).with_data(data));
        }
        let consistent = coordinate_space_consistent(dump.screen, shot);

        let quality = assess_tree(&dump.elements);
        let mut summary = format_ui_summary(&dump.elements);
        let mut ocr_text = None;
        // The screenshot is already in hand, so reading it costs no extra
        // capture — only the OCR pass. Do it when the tree describes little or
        // nothing, and hand back the text as tappable lines.
        if quality != TreeQuality::Usable {
            match ocr_png(&png).await {
                Ok(lines) => {
                    summary.push_str("\n\n");
                    summary.push_str(&format_ocr_lines(&lines));
                    ocr_text = Some(lines);
                }
                Err(why) => {
                    if let Some(advice) = quality.advice() {
                        summary.push_str(&format!("\n\n{advice} (OCR unavailable: {why})"));
                    }
                }
            }
        }
        if !consistent {
            let (sw, sh) = dump.screen.unwrap_or((0, 0));
            let (iw, ih) = shot.unwrap_or((0, 0));
            summary.push_str(&format!(
                "\n\nWARNING: the screenshot ({iw}x{ih}) and the UI tree ({sw}x{sh}) do not describe \
                 the same screen — it most likely rotated or was scaled between the two reads. \
                 The element coordinates above may not match the image: observe again before \
                 tapping."
            ));
        }

        let mut data = serde_json::json!({
            "count": dump.elements.len(),
            "quality": quality.as_str(),
            "advice": quality.advice(),
            "ocr": ocr_text,
            "elements": dump.elements,
            "screenshot_base64": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &png,
            ),
            "source": dump.source,
            "snapshot": dump.snapshot,
            "screenshot": as_size(shot),
            "screen": dump.screen.map(|(w, h)| serde_json::json!({ "width": w, "height": h })),
            "coordinates_consistent": consistent,
            "device": resolved_serial(&device).await,
        });
        if let Some(reason) = fallback {
            let note = since_fallback(reason);
            data["baseline"] = note["baseline"].clone();
            data["fallback"] = note["fallback"].clone();
            summary.push_str(&format!("\n\n{}", reason.describe()));
        }
        Ok(ToolExecutionResult::success(summary).with_data(data))
    }
}

// ── Incremental reads ──────────────────────────────────────────────────────

/// What a read asked to be compared with an earlier one found.
enum SinceAnswer {
    /// The comparison held: report the delta instead of the whole screen.
    Delta {
        text: String,
        data: serde_json::Value,
    },
    /// It did not: report the whole screen, and say why it was not compared.
    Full(super::diff::FullReason),
}

/// Answer a `since` argument against the read that has just been taken.
///
/// `None` means the caller did not ask for a delta at all. Otherwise this is
/// either the delta — what appeared, what left, what moved — or the reason the
/// whole list is the better answer ([`super::diff::FullReason`]).
fn since_answer(
    device: Option<&str>,
    target: Target,
    since: Option<&str>,
    dump: &UiDump,
) -> Option<SinceAnswer> {
    let since = since?;
    match super::diff::compare(
        device,
        target.display_id(),
        since,
        dump.package.as_deref(),
        &dump.elements,
    ) {
        super::diff::ScreenDiff::Diff(delta) => {
            // Only what is on the screen now is addressable: an added row's
            // index works, and a removed row's does not.
            let mut present: Vec<&UiElement> = delta.added.iter().collect();
            present.extend(delta.moved.iter());
            let data = serde_json::json!({
                "since": since,
                "baseline": true,
                "diff": {
                    "added": delta.added.len(),
                    "removed": delta.removed.len(),
                    "moved": delta.moved.len(),
                    "unchanged": delta.unchanged,
                    "before": delta.before_len,
                    "after": delta.after_len,
                },
                "elements": present,
                "count": present.len(),
                "snapshot": dump.snapshot,
            });
            Some(SinceAnswer::Delta {
                text: super::diff::describe(&delta),
                data,
            })
        }
        super::diff::ScreenDiff::Full(reason) => Some(SinceAnswer::Full(reason)),
    }
}

/// The `data` field naming why a read was not compared.
fn since_fallback(reason: super::diff::FullReason) -> serde_json::Value {
    serde_json::json!({
        "baseline": false,
        "fallback": match reason {
            crate::computer::platform::mobile::diff::FullReason::NoBaseline => "no_baseline",
            crate::computer::platform::mobile::diff::FullReason::AppChanged => "app_changed",
            crate::computer::platform::mobile::diff::FullReason::MostlyChanged => "mostly_changed",
        },
    })
}

// ── ADB Input Tool ─────────────────────────────────────────────────────────

/// Tap, swipe, type, and press keys on the device.
#[derive(Debug)]
pub struct AdbInputTool {
    device: Option<String>,
}

impl Default for AdbInputTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbInputTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }

    /// Whether the device's active input method is ADBKeyboard.
    ///
    /// ADBKeyboard accepts text over a broadcast, which is the only way to type
    /// non-ASCII without a UI: `input text` goes through the device's key
    /// character map and drops or mangles anything outside ASCII (CJK in
    /// particular). Costs one extra `adb` round trip, so it is only consulted
    /// for text that actually needs it.
    async fn adbkeyboard_is_active(&self, device: &Option<String>) -> bool {
        let args =
            adb_args(device, &["shell", "settings", "get", "secure", "default_input_method"]);
        match run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>()).await {
            Ok((status, stdout, _)) if status.success() => {
                stdout.to_lowercase().contains("adbkeyboard")
            }
            _ => false,
        }
    }
}

/// The one spelling of a keycode every dispatcher understands.
///
/// `input keyevent` accepts either a name (`KEYCODE_ENTER`, `ENTER`) or a
/// number, and so did this tool by passing the string straight through. The
/// display helper does not: it reflects the named field on `android.view.
/// KeyEvent` (`DevicePlugin.kt`, `op: "key"`), so a bare `ENTER` is an
/// `NoSuchFieldException` there. Normalising at the one place the key op is
/// built means a name works on both paths, and a number is left alone because
/// both accept it.
fn normalize_keycode(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().all(|c| c.is_ascii_digit()) {
        return trimmed.to_string();
    }
    let upper = trimmed.to_ascii_uppercase();
    if upper.starts_with("KEYCODE_") {
        upper
    } else {
        format!("KEYCODE_{upper}")
    }
}

/// Quote a string as a single shell word for the *device* shell.
///
/// A single-quoted word may not contain a quote, so an embedded `'` has to close
/// the word, emit an escaped quote and reopen it (`'\''`) — the only escape a
/// single-quoted shell word supports. Without this a quote in the text ends the
/// command early and everything after it is parsed as further commands.
pub(super) fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Apply `input text`'s own escapes: `%s` means space and `%%` a literal `%`.
///
/// A literal `%` must be doubled *before* spaces become `%s`, otherwise the `%`
/// introduced here would itself be doubled.
pub(super) fn input_encode(s: &str) -> String {
    s.replace('%', "%%").replace(' ', "%s")
}

/// Build a tap sequence: several taps dispatched in a single shell command.
///
/// For a control that is about to auto-fade (a transient toolbar, a toast with a
/// button), tapping it, waiting for a model turn and tapping the next thing is
/// usually too slow — the control is gone. Chaining the taps into one dispatch
/// closes that window.
///
/// Deliberately coordinate-based: the later taps usually target a control that
/// only appears *because* an earlier one revealed it, so it cannot be validated
/// against the current tree. Each `input` is its own process on the device,
/// which spaces the taps by a few tens of milliseconds without needing `sleep`.
fn encode_tap_sequence(steps: &[(i32, i32)]) -> String {
    steps
        .iter()
        .map(|(x, y)| format!("input tap {} {}", x, y))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Read a `steps` argument as a list of `{ x, y }` taps.
fn parse_tap_steps(value: Option<&Value>) -> Result<Vec<(i64, i64)>, String> {
    let Some(Value::Array(items)) = value else {
        return Err("action 'sequence' requires a 'steps' array of { x, y } taps".to_string());
    };
    if items.is_empty() {
        return Err("action 'sequence' needs at least one step".to_string());
    }
    // A sequence is one dispatch, so the ladder, the gate and the pace all see
    // it as one action — but it lands as many taps. Unbounded, it is a way to
    // do fifty taps in the shape of one before any of them has an effect to
    // read. The cap is the per-call budget a person could watch.
    if items.len() > MAX_SEQUENCE_STEPS {
        return Err(format!(
            "action 'sequence' takes at most {MAX_SEQUENCE_STEPS} steps, got {} — split it, \
             checking the screen between the halves",
            items.len()
        ));
    }
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let x = item.get("x").and_then(Value::as_i64);
            let y = item.get("y").and_then(Value::as_i64);
            match (x, y) {
                (Some(x), Some(y)) => Ok((x, y)),
                _ => Err(format!("step {i} needs integer 'x' and 'y'")),
            }
        })
        .collect()
}

/// The most taps one `sequence` may dispatch.
///
/// The same ceiling `android_run` puts on a command list: beyond it a single
/// call is no longer something the screen's own feedback can keep up with.
pub(super) const MAX_SEQUENCE_STEPS: usize = 30;

/// Build the device-shell command(s) that type `text`.
///
/// `input text` cannot produce a newline, so every line break becomes an
/// `input keyevent 66` (Enter) *between* the lines, which also makes blank lines
/// come out as paragraph breaks. A single-line text is one command.
fn encode_input_text(text: &str) -> String {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();

    let mut commands: Vec<String> = Vec::with_capacity(lines.len() * 2);
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            commands.push("input keyevent 66".to_string());
        }
        if !line.is_empty() {
            commands.push(format!("input text {}", sh_quote(&input_encode(line))));
        }
    }
    commands.join("; ")
}

#[async_trait]
impl Tool for AdbInputTool {
    fn name(&self) -> &str {
        "android_input"
    }

    fn description(&self) -> &str {
        "Send tap, swipe, text, or key events to an Android device via ADB. For taps prefer \
         `target` (an element index from android_ui_tree): it is re-read from the live screen \
         before dispatch, so a screen that changed under you is reported instead of silently \
         tapping the wrong thing. Use action 'sequence' with several coordinate steps when you \
         need to hit something that will disappear before your next turn. The result reports \
         whether the screen actually changed afterwards (effect: changed / unchanged / unknown) \
         and what is on it now, so a separate android_observe is usually unnecessary. Set \
         verify=false when the action is not meant to change the screen and you want to skip \
         the extra reads. On a second display, 'gesture' sends one or two fingers (a pinch, a \
         curved drag), and non-ASCII text is typed by pasting it — both are things `input` \
         cannot do. A tap named by `target` or `key` is pressed as a control first (an \
         accessibility action on the element itself), then as a service gesture, and only then \
         by shell injection; the result's `via` says which was used."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Send input to Android device",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "tap | sequence | swipe | text | key | gesture",
                    "enum": ["tap", "sequence", "swipe", "text", "key", "gesture"]
                },
                "paths": {
                    "type": "array",
                    "description": "For 'gesture': one or two fingers, each a list of 2–32 [x, y] points in the display's pixels. Two fingers moving apart is a pinch; one finger following several points is a curved drag. Only on a second display.",
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "array",
                            "items": { "type": "integer" }
                        }
                    }
                },
                "duration_ms": {
                    "type": "integer",
                    "description": "For 'gesture': how long the whole gesture takes (default 500, max 5000)."
                },
                "steps": {
                    "type": "array",
                    "description": "For 'sequence': taps to dispatch back to back in one call, for a control that will auto-fade before your next turn. Each is { \"x\": int, \"y\": int }.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer" },
                            "y": { "type": "integer" }
                        },
                        "required": ["x", "y"]
                    }
                },
                "target": {
                    "type": "integer",
                    "description": "Element index from android_ui_tree. Preferred over x/y for taps: the element is re-read from the live screen and the tap is refused if it moved, changed, or is not clickable."
                },
                "target_description": {
                    "type": "string",
                    "description": "Optional label you expect at `target`. A mismatch refuses the tap and reports what is actually there."
                },
                "key": {
                    "type": "string",
                    "description": "Element key from the `key` field of an earlier android_ui_tree/android_observe. Preferred over `target`: the key is what the element *is*, so it is looked up again in the live screen even after the list scrolled or a row appeared. Refused with STALE_REF if it is gone, or if several elements are now indistinguishable — the refusal lists them."
                },
                "snapshot": {
                    "type": "string",
                    "description": "The `snapshot` of the read the reference came from. Optional; when the live read has a different snapshot the reference is still resolved by key, and the result says the screen had changed."
                },
                "x": { "type": "integer", "description": "X coordinate (tap/swipe). No safety net — prefer `target`." },
                "y": { "type": "integer", "description": "Y coordinate (tap/swipe). No safety net — prefer `target`." },
                "x2": { "type": "integer", "description": "End X (swipe)" },
                "y2": { "type": "integer", "description": "End Y (swipe)" },
                "text": { "type": "string", "description": "Text to type" },
                "keycode": { "type": "string", "description": "Android keycode name or number" },
                "verify": {
                    "type": "boolean",
                    "description": "Default true: re-read the screen after acting and report whether it changed. Costs two to three extra tree reads; set false to skip them."
                },
                "settle_ms": {
                    "type": "integer",
                    "description": "How long to wait for the screen to settle when verifying (default 1200, max 4000). Raise it for a slow screen transition."
                },
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            vec!["action"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };

        // Which lock is the right one depends on where the action lands.
        //
        // The user's own screen has one pointer and one focus, so everything
        // addressed to it waits on the device. A virtual display has a focus of
        // its own, so two of them are two places to type and do not need to
        // wait for each other — but the *app* they are driving is shared, and
        // that is what the lease below is for.
        let _target = lock_target(&device, target).await;

        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("tap");

        let verify = args.get("verify").and_then(|v| v.as_bool()).unwrap_or(true);
        let settle_budget = args
            .get("settle_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(SETTLE_DEFAULT_MS);

        // One read serves both purposes: it validates an indexed tap (the
        // safety net) and it is the picture the effect is judged against. A
        // failed read only refuses when the caller pointed at an element —
        // every other action still dispatches, and reports its effect as
        // unknown rather than pretending it worked.
        // The dump's package is only used by the outward pacing, but it has to
        // be read before the action: afterwards the screen belongs to whatever
        // the action opened.
        let mut before_package: Option<String> = None;
        let mut before_snapshot: Option<String> = None;
        let pointed_at_an_element = args.get("target").is_some() || args.get("key").is_some();
        let mut before = match dump_ui(&device, target).await {
            Ok(dump) => {
                before_package = dump.package;
                before_snapshot = Some(dump.snapshot);
                Some(dump.elements)
            }
            Err(failed) => {
                if action == "tap" && pointed_at_an_element {
                    return Ok(ToolExecutionResult::error(failed.message));
                }
                None
            }
        };

        // The read named the app; now take exclusive use of it. This is the
        // half the display lock cannot give: two displays, one app. It is taken
        // after the read because the read is what names the app, so the read
        // itself may predate the lease — `stale` is that, and the answer is to
        // read again rather than act on a screen that has moved.
        let held = app_completed(&device, before_package.as_deref());
        let lease = lease_app(&device, before_package.as_deref(), held).await;
        if lease.stale {
            match dump_ui(&device, target).await {
                // Somebody wrote to this app while this call was waiting. The
                // tree it read a moment ago is no longer the screen; use this
                // one, and if the app itself changed, stop rather than take a
                // second lease on a different app while holding this one.
                Ok(dump) if dump.package == before_package => {
                    before_package = dump.package;
                    before_snapshot = Some(dump.snapshot);
                    before = Some(dump.elements);
                }
                Ok(dump) => {
                    return Ok(ToolExecutionResult::error(format!(
                        "Refused: the screen changed app while this call waited for another \
                         writer — it was {:?}, it is now {:?}. Nothing was done. Read the screen \
                         again and retry.",
                        before_package.as_deref().unwrap_or("unknown"),
                        dump.package.as_deref().unwrap_or("unknown"),
                    )));
                }
                Err(_) => {
                    // Nothing to act on: the screen cannot be read now, and the
                    // read in hand describes a moment before somebody else's
                    // write.
                    return Ok(ToolExecutionResult::error(
                        "Refused: another writer changed this app while this call was waiting, \
                         and the screen could not be re-read afterwards. Nothing was done. Read \
                         the screen again and retry.",
                    ));
                }
            }
        }

        // The app's own screens are the controls that govern the agent:
        // settings, pairing, the display list. Driving them is the agent
        // reaching for its own levers, and it is never what the task meant.
        // Refused here rather than in the gate because this is not a question
        // of judgement — there is no answer a person could give that makes it
        // right. Reads stay allowed: the agent may look at its own screen to
        // report on it.
        if let Some(package) = before_package.as_deref() {
            if super::takeover::is_own_app(package) {
                return Ok(ToolExecutionResult::error(format!(
                    "Refused: this would write to Syscity's own screen ({package}). The app's \
                     settings, pairing and display pages are the controls that govern the agent, \
                     so the agent does not operate them — a person does. Nothing was done."
                )));
            }
        }

        // Whether the phone is still in the state this action was computed
        // for. This runs before the ladder, because the ladder counts an action
        // that did not work, and an action that was never taken is not evidence
        // of anything — least of all that it should be tried again.
        let readiness = super::ready::probe().await;
        if let Err(blocked) =
            super::ready::check(readiness.as_ref(), target, before_package.as_deref())
        {
            return Ok(ToolExecutionResult::error(super::ready::describe(&blocked)));
        }

        // A reference from an earlier read is resolved against this one before
        // anything is dispatched, and the two ways of naming an element are the
        // same question asked with different information: an index is a
        // position in *that* read, a key is the element itself.
        let snapshot_note = match args
            .get("snapshot")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            Some(was) if before_snapshot.as_deref() != Some(was) => Some(
                "The screen had changed since that read; the element was found again by its key, \
                 not by where it used to be.",
            ),
            _ => None,
        };

        // The action is resolved once, into something that both dispatchers
        // understand. Where it goes — the device's screen or the agent's — is
        // the only thing that differs afterwards.
        //
        // A tap named by element may already have been delivered by a rung
        // above the shell (the control itself, or a service gesture), which is
        // what `pressed` records: the resolved op is then only the fallback the
        // safety checks below still reason about. `press_target` is the element
        // it would be pressed on, remembered rather than pressed — the gate and
        // the approval re-check run before anything leaves this process.
        let mut pressed: Option<(Via, (i32, i32))> = None;
        let mut press_target: Option<UiElement> = None;
        let op = match action {
            "tap" => {
                if let Some(key) = args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .filter(|k| !k.trim().is_empty())
                {
                    let elements = before.as_deref().unwrap_or(&[]);
                    match relocate(elements, key.trim()) {
                        Ok(element) => {
                            if let Some(description) =
                                args.get("target_description").and_then(|v| v.as_str())
                            {
                                let label = element_label(element).unwrap_or("");
                                if !label_matches(label, description) {
                                    return Ok(ToolExecutionResult::error(format!(
                                        "Refused: that key is now {label:?}, not {description:?} — \
                                         the screen changed. Re-read it with android_ui_tree."
                                    )));
                                }
                            }
                            match check_tappable(element, element.index) {
                                Ok(element) => {
                                    // Remembered, not pressed yet: the gate and
                                    // the approval re-check below run before
                                    // anything leaves this process.
                                    press_target = Some(element.clone());
                                    InputOp::Taps(vec![element.center])
                                }
                                Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                            }
                        }
                        Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                    }
                } else {
                    match args.get("target").and_then(|v| v.as_u64()) {
                        // An element index, checked against the read taken
                        // immediately before dispatch.
                        Some(index) => {
                            let description =
                                args.get("target_description").and_then(|v| v.as_str());
                            let elements = before.as_deref().unwrap_or(&[]);
                            match validate_target(
                                elements,
                                usize::try_from(index).unwrap_or(0),
                                description,
                            ) {
                                Ok(element) => {
                                    press_target = Some(element.clone());
                                    InputOp::Taps(vec![element.center])
                                }
                                Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                            }
                        }
                        // Raw coordinates: no safety net, the caller is on its own.
                        None => {
                            let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
                            let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
                            InputOp::Taps(vec![(
                                i32::try_from(x).unwrap_or(0),
                                i32::try_from(y).unwrap_or(0),
                            )])
                        }
                    }
                }
            }
            "swipe" => {
                let value = |key: &str| {
                    i32::try_from(args.get(key).and_then(|v| v.as_i64()).unwrap_or(0)).unwrap_or(0)
                };
                InputOp::Swipe {
                    from: (value("x"), value("y")),
                    to: (value("x2"), value("y2")),
                }
            }
            "text" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if text.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "action 'text' requires a non-empty 'text' argument",
                    ));
                }
                InputOp::Text(text.to_string())
            }
            "sequence" => match parse_tap_steps(args.get("steps")) {
                Ok(steps) => InputOp::Taps(
                    steps
                        .into_iter()
                        .map(|(x, y)| {
                            (i32::try_from(x).unwrap_or(0), i32::try_from(y).unwrap_or(0))
                        })
                        .collect(),
                ),
                Err(msg) => return Ok(ToolExecutionResult::error(msg)),
            },
            "key" => InputOp::Key(normalize_keycode(
                args.get("keycode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("HOME"),
            )),
            "gesture" => match parse_gesture_paths(args.get("paths")) {
                Ok(paths) => InputOp::Gesture {
                    paths,
                    duration_ms: u32::try_from(
                        args.get("duration_ms")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(500),
                    )
                    .unwrap_or(500)
                    .clamp(50, 5000),
                },
                Err(message) => return Ok(ToolExecutionResult::error(message)),
            },
            _ => return Ok(ToolExecutionResult::error(format!("Unknown action: {}", action))),
        };

        // What is about to be pressed, before it is pressed.
        //
        // Only a tap can reach past the phone, and only a tap has a label to
        // judge: a swipe and a key event do not. The classification happens
        // here, after the tree read, because the tree is what names the
        // element — and it happens before the dispatch, because a send that is
        // refused must not leave the app.
        //
        // The repetition guard runs *before* the gate. Asking a person to
        // approve the twelfth identical send would be asking them to endorse a
        // loop the agent should have noticed itself; the ladder refuses that
        // without troubling anyone.
        let signature = action_signature(&op, before.as_deref());
        let guard_key = super::guard::Guard::bucket_key(
            &context.conversation_id,
            &format!("{}:{}", device.as_deref().unwrap_or("-"), target.display_id()),
        );
        let ladder = super::guard::Guard::global().check(&guard_key, &signature);
        if let super::guard::Verdict::Refuse(reason) = &ladder {
            return Ok(ToolExecutionResult::error(reason.clone()));
        }

        let clearance = match &op {
            InputOp::Taps(points) if points.len() == 1 => {
                let offer = match args.get("target").and_then(|v| v.as_u64()) {
                    Some(index) => super::risk::classify_tap(
                        before.as_deref().unwrap_or(&[]),
                        usize::try_from(index).ok(),
                    ),
                    None => super::risk::classify_point(
                        before.as_deref().unwrap_or(&[]),
                        points[0].0,
                        points[0].1,
                    ),
                };
                let subject = offer
                    .as_ref()
                    .and_then(|o| o.element.clone())
                    .map(|label| format!("tap \"{label}\""))
                    .unwrap_or_else(|| format!("tap ({}, {})", points[0].0, points[0].1));
                let subject = match before_package.as_deref() {
                    Some(package) => format!("{subject} in {package}"),
                    None => subject,
                };
                match super::gate::check(offer, context, &subject, before_package.as_deref()).await
                {
                    Ok(clearance) => clearance,
                    Err(refusal) => return Ok(ToolExecutionResult::error(refusal.reason)),
                }
            }
            _ => {
                // A multi-point `sequence` is a series of taps at raw
                // coordinates with no element to name them by; the other ops
                // (swipe, text, key, gesture) are not taps at all.
                super::gate::Clearance {
                    offer: None,
                    asked: false,
                    granted: None,
                }
            }
        };

        // The question was open for as long as the person took to read it, and
        // a phone screen moves under a question: a message arrives and the row
        // under the finger becomes a different row. What they approved was the
        // button described in the question. Before dispatching, check that the
        // same thing is still under the finger — one re-read, and only when
        // somebody actually said yes.
        if clearance.granted.is_some() {
            let fresh = match dump_ui(&device, target).await {
                Ok(fresh) => fresh,
                Err(failed) => {
                    return Ok(ToolExecutionResult::error(format!(
                        "\"{}\" was approved, but the screen could not be read again to confirm \
                         it is still there ({}). Nothing was done. Read the screen with \
                         android_observe and retry if it is still wanted.",
                        clearance
                            .granted
                            .as_ref()
                            .map(|grant| grant.subject.clone())
                            .unwrap_or_default(),
                        failed.message
                    )))
                }
            };
            let now = match &op {
                InputOp::Taps(points) if points.len() == 1 => {
                    let recorded = points[0];
                    match args.get("target").and_then(|v| v.as_u64()) {
                        Some(index) => {
                            super::risk::classify_tap(&fresh.elements, usize::try_from(index).ok())
                        }
                        None => {
                            super::risk::classify_point(&fresh.elements, recorded.0, recorded.1)
                        }
                    }
                }
                _ => None,
            };
            if let Err(withheld) =
                super::gate::verify(&clearance, now.as_ref(), fresh.package.as_deref())
            {
                return Ok(ToolExecutionResult::error(withheld.reason));
            }
        }

        // Two things have to be read *before* the action, not after: the
        // picture (the tree cannot see a canvas change) and the user's screen
        // (an app that lands there has to be told apart from one the user
        // opened). Both are only worth taking on a display, where the action
        // is not supposed to touch the user's screen at all.
        //
        // The user's own screen is different: the only way to see its pixels is
        // a raw `screencap`, a second ADB round trip carrying ~10 MB, and an
        // action on it has no spill to watch for. So its picture is taken only
        // when the tree has already said "unchanged" — after the action, once
        // — and the tree is read again from that same moment. The cost is one
        // extra round trip and one extra read, and only on the path where
        // without them the answer would have been a bare "unchanged".
        let watched = match target {
            Target::Display(id) if verify => {
                let pixels = display_pixels(id).await;
                let watch = match display_bridge() {
                    Ok(bridge) => {
                        let display = super::display::VirtualDisplay::attach(bridge, id);
                        match display.watch_main_screen(SPILL_WINDOW_MS as u32).await {
                            Ok(watch) => Some(watch),
                            Err(failed) => {
                                // Without the window there is no spill check at
                                // all, so a failure here has to be visible.
                                tracing::warn!(
                                    display = id,
                                    error = %failed,
                                    "could not start watching the user's screen; \
                                     a spill during this action will go unreported",
                                );
                                None
                            }
                        }
                    }
                    Err(_) => None,
                };
                Some((pixels, watch))
            }
            _ => None,
        };

        // The event subscription is taken here, before the action, so an event
        // the action causes is still in the channel when the settle starts.
        // Taking it afterwards would miss exactly the event wanted. It is
        // detached from what the channel already holds by the settle itself.
        let mut settle_stream = if verify { settle_events(target) } else { None };
        // Whatever the channel held before the action is history: a content
        // event from the previous screen would otherwise make the settle
        // return at its first quiet moment and never see this action's.
        if let Some(receiver) = settle_stream.as_mut() {
            while receiver.try_recv().is_ok() {}
        }

        // The press itself, now that it is allowed to happen and the picture
        // the effect is judged against has been taken: the ladder is tried in
        // order and `pressed` records the rung that delivered it. A shell rung
        // sends nothing here — the dispatcher below is the only one that can
        // reach a virtual display.
        if let Some(element) = press_target.as_ref() {
            let (via, refusal) =
                press_element(element, false, matches!(target, Target::Screen)).await;
            if let Some(refusal) = refusal {
                return Ok(ToolExecutionResult::error(refusal));
            }
            pressed = Some((via, element.center));
        }

        // Which rung pressed it — only a press has an answer. A swipe or a
        // typed line is a shell injection, and saying "shell" there would read
        // as if the ladder had been tried and failed.
        let via = (action == "tap" || pressed.is_some())
            .then(|| pressed.as_ref().map(|(via, _)| *via).unwrap_or(Via::Shell));

        // Whether the text went in by clipboard rather than by keystrokes —
        // the result says which, because the two fail in different ways.
        let pasted = match target {
            Target::Screen => {
                if let InputOp::Gesture { .. } = &op {
                    // `input` has no multi-finger form at all, and the
                    // accessibility service is the only thing that could
                    // dispatch one — but only to a display it owns. Saying so
                    // beats sending one finger of a pinch.
                    return Ok(ToolExecutionResult::error(
                        "Multi-finger gestures are only dispatched on a second display \
                         (android_display start). On the user's screen use 'swipe', or run the \
                         task on a display where the gesture can be sent properly.",
                    ));
                }
                // Set when the text went in by clipboard paste rather than by
                // `input text`: the result line says so, and the readback below
                // is the only confirmation a paste gets.
                let mut pasted = false;
                let shell_cmd = match &op {
                    // Rejected above: no shell form exists for two fingers.
                    InputOp::Gesture { .. } => {
                        return Ok(ToolExecutionResult::error(
                            "Multi-finger gestures are only dispatched on a second display \
                             (android_display start).",
                        ))
                    }
                    InputOp::Taps(steps) => encode_tap_sequence(steps),
                    InputOp::Swipe { from, to } => {
                        format!("input swipe {} {} {} {}", from.0, from.1, to.0, to.1)
                    }
                    InputOp::Key(keycode) => format!("input keyevent {keycode}"),
                    InputOp::Text(text) if text.is_ascii() => encode_input_text(text),
                    InputOp::Text(text) => {
                        if self.adbkeyboard_is_active(&device).await {
                            // ADBKeyboard takes the text base64-encoded over a
                            // broadcast, which carries any character the device
                            // can render.
                            let payload = base64::Engine::encode(
                                &base64::engine::general_purpose::STANDARD,
                                text.as_bytes(),
                            );
                            format!("am broadcast -a ADB_INPUT_B64 --es msg '{}'", payload)
                        } else {
                            // Two ways without ADBKeyboard, tried in that order:
                            // the app's own helper (uid 2000, which can inject
                            // Ctrl+V on the user's screen and can put the
                            // person's clipboard back afterwards), and nothing.
                            //
                            // `input text` types through the device's key
                            // character map, so non-ASCII (CJK in particular)
                            // comes out dropped or mangled. Failing loudly is
                            // still better than silently typing the wrong thing
                            // — but only once the paste has been ruled out.
                            match paste_on_user_screen(text).await {
                                Ok(()) => {
                                    pasted = true;
                                    String::new()
                                }
                                Err(_) => {
                                    return Ok(ToolExecutionResult::error(
                                        "Cannot type non-ASCII text: `input text` only types \
                                         ASCII reliably, ADBKeyboard is not the active input \
                                         method, and the display helper is not available on this \
                                         device to paste it. Install/enable ADBKeyboard \
                                         (com.android.adbkeyboard) and select it as the input \
                                         method, or keep the text ASCII.",
                                    ))
                                }
                            }
                        }
                    }
                };
                if !pasted {
                    let argv = adb_args(&device, &["shell", &shell_cmd]);
                    // A press already delivered by a rung above the shell is
                    // not sent again: what the safety net still owes the
                    // caller is the *effect*, not a second tap.
                    if pressed.is_none() {
                        let (status, _stdout, stderr) =
                            run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                                .await
                                .map_err(|e| crate::error::SyscityError::ExternalService {
                                    source: "adb input failed".to_string(),
                                    cause: Some(Box::new(e)),
                                })?;
                        if !status.success() {
                            return Ok(ToolExecutionResult::error(format!(
                                "adb input failed: {stderr}"
                            )));
                        }
                    }
                }
                pasted
            }
            Target::Display(id) => {
                let mut pasted = false;
                if let InputOp::Text(text) = &op {
                    if !text.is_ascii() {
                        // `input -d text` goes through the key character map and
                        // mangles anything outside ASCII. The helper's clipboard
                        // paste carries whatever the app can render, and puts
                        // the user's own clipboard back afterwards.
                        if let Err(message) = paste_on_display(id, text).await {
                            return Ok(ToolExecutionResult::error(message));
                        }
                        pasted = true;
                    }
                }
                // A press already delivered by a rung above the shell is not
                // sent again, and a pasted text is already in.
                if !pasted && pressed.is_none() {
                    if let Err(message) = dispatch_display(id, &op).await {
                        return Ok(ToolExecutionResult::error(message));
                    }
                }
                pasted
            }
        };

        // The action is now out. Record how far it reached, so the next one in
        // the same app is spaced like a person's would be. The package is the
        // one read *before* the action: after it, the screen may belong to
        // whatever the action opened.
        let pace_note = if clearance.reaches_outward() && context.mobile_control().pace_outward {
            Some(note_outward(before_package.as_deref()).await)
        } else {
            None
        };

        // A write is the one action whose *content* can be checked, and "the
        // shell accepted the keystrokes" is not "the field holds the text" — a
        // message that never lands is the failure mode that matters. Read it
        // back here, before the settle re-read: if the text opened a suggestion
        // list, the field is still there now and may not be after settling.
        let readback = match (&op, verify) {
            (InputOp::Text(the_text), true) => {
                Some(read_back_write(&device, target, the_text, before.as_deref()).await)
            }
            _ => None,
        };
        let text_len = match &op {
            InputOp::Text(the_text) => the_text.chars().count(),
            _ => 0,
        };

        let serial = resolved_serial(&device).await;
        if !verify {
            // No re-read was asked for, so the screen did not move as far as
            // this call knows — recorded as unknown, which the ladder does not
            // count as "no effect".
            super::guard::Guard::global().record(
                &guard_key,
                &signature,
                super::guard::Outcome::Unknown,
            );
            let mut text = format!("Input '{action}' sent (effect not checked).");
            if let Some(note) = via.and_then(Via::note) {
                text.push_str(&format!(" {note}"));
            }
            if pasted {
                text.push_str(&format!(
                    " The text went in by clipboard paste ({} character(s)) — `input text` \
                     cannot carry non-ASCII text. The person's own clipboard was put back.",
                    text_len
                ));
            }
            if let Some(note) = snapshot_note {
                text.push_str(&format!(" {note}"));
            }
            if let Some(note) = ladder_note(&ladder) {
                text.push_str(&format!(" {note}"));
            }
            if let Some(note) = clearance.note() {
                text.push_str(&format!(" Judged {note}."));
            }
            return Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
                "device": serial,
                "effect": "unverified",
                "via": via.map(Via::as_str),
                "risk": clearance.note(),
                "risk_approved": clearance.asked,
                "paced": pace_note,
                "reference_moved": snapshot_note.is_some(),
            })));
        }

        // Judge the effect here rather than leaving it to the model: a dispatch
        // that was accepted says nothing about whether the tap landed.
        let before_fingerprint = before.as_ref().map(|elements| tree_fingerprint(elements));
        let before_copy = before
            .as_ref()
            .map(|elements| (before_fingerprint.unwrap_or_default(), elements.clone()));
        let before_pixels = watched.as_ref().and_then(|(pixels, _)| pixels.as_ref());
        let mut settle = settle_after_action_with_pixels_and_events_from(
            &device,
            target,
            before_copy,
            before_pixels,
            settle_stream,
            settle_budget,
        )
        .await;

        // The tree said nothing changed, and on the user's own screen there was
        // no before-frame to compare against. Some screens — a game, a map, a
        // camera preview — change without changing what the tree exposes, so
        // "unchanged" there is not yet an answer. Take the picture once, and
        // only now: this is a raw `screencap`, a second ADB round trip of ~10 MB,
        // and paying it for every tap would make every tap twice as slow for an
        // answer only canvases need. Two reads a moment apart are enough to
        // tell a picture that moved from one that blinked (a cursor, a clock).
        if matches!(target, Target::Screen)
            && settle.effect == EFFECT_UNCHANGED
            && before_package.is_some()
        {
            if let Some(first) = user_screen_signature(&device).await {
                tokio::time::sleep(std::time::Duration::from_millis(SETTLE_POLL_MS)).await;
                if let Some(second) = user_screen_signature(&device).await {
                    settle.pixel_changed = Some(first.differs(&second));
                    if settle.pixel_changed == Some(true) {
                        settle.effect = EFFECT_SMALL_CHANGE;
                    }
                }
            }
        }

        // Now that both reads are in hand, the place-change question can be
        // answered — it needs the package from *before* the action, which only
        // this call has.
        if settle.effect != EFFECT_UNKNOWN {
            if let (Some(before), Some(after)) =
                (before_package.as_deref(), settle.package.as_deref())
            {
                if before != after {
                    settle.effect = EFFECT_PLACE_CHANGED;
                }
            }
        }

        // The ladder learns what this did. It counts the screen's answer, not
        // the action's: three different taps that all leave the same screen are
        // one stuck screen, and the streak is what says so.
        super::guard::Guard::global().record(&guard_key, &signature, guard_outcome(settle.effect));

        let mut text = format!("Input '{action}' sent. {}", settle.verdict());
        if let Some(note) = via.and_then(Via::note) {
            text.push_str(&format!(" {note}"));
        }
        if pasted {
            text.push_str(&format!(
                " The text went in by clipboard paste ({} character(s)) — `input text` cannot \
                 carry non-ASCII text.",
                text_len
            ));
        }
        if let Some(note) = readback_note(readback.as_ref()) {
            text.push_str(&format!(" {note}"));
        }
        if let Some(note) = snapshot_note {
            text.push_str(&format!(" {note}"));
        }
        if let Some(note) = ladder_note(&ladder) {
            text.push_str(&format!(" {note}"));
        }
        if let Some(note) = clearance.note() {
            text.push_str(&format!(" Judged {note}."));
        }
        if let Some(note) = &pace_note {
            text.push_str(&format!(" {note}"));
        }
        if settle.pixel_changed == Some(true) && settle.elements.is_empty() {
            text.push_str(
                " The picture changed even though the tree names nothing here — this screen draws \
                 itself.",
            );
        }

        // An action can start something that lands on the user's screen — a
        // share sheet, a chooser, an app that ignores `--display`. The agent's
        // display then just stops changing, which reads as "nothing happened".
        //
        // The window taken before the action is what tells a spill apart from
        // an app the user opened themselves. Without it there is only "what is
        // on top now", which cannot know who put it there.
        let mut spills = Vec::new();
        if let Target::Display(id) = target {
            if let Some((_, Some((top, since)))) = watched.as_ref() {
                if let Ok(bridge) = display_bridge() {
                    spills = super::takeover::watch_and_check_spill(
                        &bridge,
                        id,
                        *top,
                        *since,
                        SPILL_WINDOW_MS,
                        &[],
                        true,
                    )
                    .await;
                }
            }
        }
        for spill in &spills {
            text.push_str(&format!(
                "\n\nWarning: {} ended up on the user's screen ({}) rather than this display — \
                 {}. The screen above is this display; the app above is not.",
                spill.package, spill.task_id, spill.reason
            ));
        }
        if !settle.elements.is_empty() {
            text.push_str("\n\n");
            text.push_str(&format_ui_summary_limited(&settle.elements, 60));
        }

        Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
            "device": serial,
            "effect": settle.effect,
            "via": via.map(Via::as_str),
            "pixels_changed": settle.pixel_changed,
            // What answered first: `events` (a content event, then quiet) or
            // `tree` (the poll). The model does not need it to act, but it is
            // how "the stream is working on this phone" is visible at all.
            "first_signal": settle.first_signal,
            "spill": spills.first().map(|s| serde_json::json!({
                "task": s.task_id,
                "package": s.package,
                "recovered": s.recovered,
                "reason": s.reason,
            })),
            "spills": spills.iter().map(|s| serde_json::json!({
                "task": s.task_id,
                "package": s.package,
                "recovered": s.recovered,
                "reason": s.reason,
            })).collect::<Vec<_>>(),
            "settled_ms": settle.elapsed_ms,
            "elements": settle.elements,
            "readback": readback.as_ref().map(|read| serde_json::json!({
                "verdict": read.verdict,
                "actual": read.actual,
                "note": read.note,
            })),
            "risk": clearance.note(),
            "risk_approved": clearance.asked,
            "paced": pace_note,
            "reference_moved": snapshot_note.is_some(),
            "guard": ladder.note(),
        })))
    }
}

/// How long an action's spill window stays open.
///
/// Long enough for a chooser to appear after the tap that opened it, short
/// enough that a task the user starts themselves a minute later is not blamed
/// on the agent.
const SPILL_WINDOW_MS: u64 = 3000;

/// Record an outward action for the pace and describe the pause it took.
///
/// `package` is where the action happened. When the read that preceded it named
/// no app (an empty tree, a failed accessibility fetch), the history is keyed
/// on `"?"` rather than skipped: a pace that does not apply to the app the tree
/// failed on would be no pace at all for the very screens that are hardest to
/// read.
pub(super) async fn note_outward(package: Option<&str>) -> String {
    let package = package.unwrap_or("?");
    let waited = super::pacing::Pacing::global().pace(package).await;
    if waited.is_zero() {
        "Next outward action here can go straight away.".to_string()
    } else {
        format!(
            "Waited {} ms so the next outward action in this app is not back-to-back.",
            waited.as_millis()
        )
    }
}

// ── ADB App Manager Tool ───────────────────────────────────────────────────

/// How long `am start -W` may wait for the activity to report idle.
///
/// `-W` is what turns "the command was dispatched" into "the app is up", and
/// the cost of that is paid by the device lock this call holds. An app that
/// never reports idle would otherwise hold the lock forever: `ProcessRequest`
/// defaults to `timeout: None`, which means exactly that, so the timeout is
/// not optional here.
const LAUNCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long a `resolve-activity` / `monkey` probe may take.
///
/// Neither waits on an app, so a tight bound is right: this is a lookup, and a
/// lookup that has not answered in ten seconds has failed.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Parse the `-brief` output of `cmd package resolve-activity`.
///
/// The command prints the component as the last non-empty line
/// (`com.example/.MainActivity`) after a `priority=…` line and, on some
/// releases, a `packageName=…` line. Returning the input verbatim when no
/// component-shaped line is found is deliberate: the caller then falls back to
/// `monkey` instead of launching whatever word happened to be first.
pub(super) fn parse_resolve_activity(stdout: &str) -> Option<String> {
    // `-brief` output is one line on API 26+, but older platform builds print
    // `priority=N`/`packageName=…` preamble as well, so scan for the shape
    // rather than trusting the last line.
    for line in stdout.lines().rev() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("priority=") || line.starts_with("packageName=") {
            continue;
        }
        // A component is `package/class` (or `package/package.class`).
        let (pkg, class) = match line.split_once('/') {
            Some(parts) => parts,
            None => continue,
        };
        if pkg.is_empty() || class.is_empty() || pkg.contains(' ') {
            continue;
        }
        return Some(line.to_string());
    }
    None
}

/// What `am start -W` said about the launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LaunchOutcome {
    /// `Status: ok` — the activity is up.
    Started,
    /// The task was already in front; Android reports this on stderr and does
    /// not start the activity again. The app *is* on screen, so this counts as
    /// a success even though no `Status:` line appears.
    AlreadyInFront,
    /// `Error: …`, or `Status:` with something other than `ok`.
    Failed,
    /// No recognisable status: the command exited zero but said nothing about
    /// the activity. Reported as unknown rather than assumed to be fine.
    Unknown,
}

/// The phrase Android prints instead of `Status:` when the task is already top.
const BROUGHT_TO_FRONT: &str =
    "Warning: Activity not started, its current task has been brought to the front";

/// Read the verdict out of `am start -W`'s combined output.
///
/// Both streams are passed in because the "already in front" warning goes to
/// stderr on some releases and stdout on others, and an `Error:` line is what
/// decides failure — a `Status: ok` that follows an `Error:` in the same run
/// must not mask it.
pub(super) fn parse_am_start_output(stdout: &str, stderr: &str) -> LaunchOutcome {
    let combined = format!("{stdout}\n{stderr}");
    for line in combined.lines() {
        let line = line.trim();
        if line.starts_with("Error:") || line.starts_with("Exception:") {
            return LaunchOutcome::Failed;
        }
        if line.contains(BROUGHT_TO_FRONT) {
            return LaunchOutcome::AlreadyInFront;
        }
        if let Some(rest) = line.strip_prefix("Status:") {
            // `Status: ok` / `Status: timeout` / `Status: warning`
            return if rest.trim().eq_ignore_ascii_case("ok") {
                LaunchOutcome::Started
            } else {
                LaunchOutcome::Failed
            };
        }
        if let Some(rest) = line.strip_prefix("Activity:") {
            // `Activity: com.example/.Main` — the launch reached the activity
            // manager, but this is not itself a verdict; keep scanning.
            let _ = rest;
        }
    }
    LaunchOutcome::Unknown
}

/// Install, launch, force-stop, and list Android apps.
#[derive(Debug)]
pub struct AdbAppManagerTool {
    device: Option<String>,
}

impl Default for AdbAppManagerTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbAppManagerTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }

    /// Launch `package`, resolving its entry point instead of guessing.
    ///
    /// The old code sent `am start -n pkg/.MainActivity` whenever the caller did
    /// not name an activity, which is true of almost no app: the launcher entry
    /// is whatever the app declares, often `…ui.LauncherActivity` or a
    /// Flutter/Compose shell that is not called `MainActivity` at all. So:
    ///
    /// 1. `cmd package resolve-activity --brief -c android.intent.category.LAUNCHER`
    ///    — what the system itself would open. The `-c` is load-bearing: without
    ///    the category this resolves the *default* activity, which may be an
    ///    activity with no launcher entry at all.
    /// 2. `monkey -p <pkg> 1`, which asks the launcher machinery to open the
    ///    app's main entry. It exists precisely for the case where the package
    ///    manager cannot name a component.
    /// 3. Only then report `launch_no_entry`.
    ///
    /// The start itself uses `-W` and parses the verdict, because "the command
    /// was dispatched" and "the app is on screen" are different facts and the
    /// caller is about to act on the second one. `-W` waits for the activity to
    /// report idle, which some apps take seconds to do — hence [`LAUNCH_TIMEOUT`]
    /// rather than the default (unbounded) process timeout.
    async fn launch(
        &self,
        device: &Option<String>,
        args: &Value,
    ) -> crate::Result<ToolExecutionResult> {
        let pkg = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
        if pkg.trim().is_empty() {
            return Ok(ToolExecutionResult::error("launch needs `package` — the app to start"));
        }

        let component = match args.get("activity").and_then(|v| v.as_str()) {
            Some(a) if !a.trim().is_empty() => Some(format!("{}/{}", pkg, a.trim())),
            Some(_) => Some(format!("{}/{}", pkg, ".MainActivity")),
            None => self.resolve_component(device, pkg).await,
        };

        let Some(component) = component else {
            return Ok(ToolExecutionResult::error(format!(
                "launch_no_entry: no launcher activity found for `{pkg}` — it may have none \
                 (a service, a shortcut-only app), or it may not be installed. \
                 `android_app_manager action=list_packages` shows what is."
            ))
            .with_data(serde_json::json!({
                "device": resolved_serial(device).await,
                "reason": "launch_no_entry",
            })));
        };

        let argv = adb_args(device, &["shell", "am", "start", "-W", "-n", &component]);
        let outcome = super::run_cmd_timeout(
            "adb",
            &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            LAUNCH_TIMEOUT,
        )
        .await;

        let (verdict, detail, timed_out) = match outcome {
            super::RunOutcome::Finished { status, stdout, stderr } => {
                let _ = status;
                let verdict = parse_am_start_output(&stdout, &stderr);
                let detail = if stderr.trim().is_empty() {
                    stdout.trim().to_string()
                } else {
                    format!("{}\n{}", stdout.trim(), stderr.trim())
                };
                (verdict, detail, false)
            }
            super::RunOutcome::TimedOut { stdout, stderr } => (
                LaunchOutcome::Unknown,
                format!(
                    "no answer within {} ms (the launch may still complete)\n{}\n{}",
                    LAUNCH_TIMEOUT.as_millis(),
                    stdout.trim(),
                    stderr.trim()
                ),
                true,
            ),
            super::RunOutcome::Failed(why) => {
                return Ok(ToolExecutionResult::error(format!("could not run `adb`: {why}")))
            }
        };

        // "Already in front" is Android declining to restart an app that is
        // already open. The app is on screen, which is all the caller asked for.
        let started = matches!(verdict, LaunchOutcome::Started | LaunchOutcome::AlreadyInFront);

        // An install in two profiles is two copies of one package, and Android
        // answers a launch of it by asking which copy to open instead of
        // opening one. `am start` reports `Status: ok` for that question — the
        // activity it started is the chooser — so the launch is not finished
        // until the screen says what rose to the top. Read it here rather than
        // reporting "started" and leaving the caller to discover the question
        // on its next observation.
        let chooser = if started {
            self.launch_chooser(device).await
        } else {
            None
        };

        let sentence = match (&chooser, verdict) {
            (Some(choices), _) => format!(
                "`{pkg}` is up only as a question: {}. Pick one copy with \
                 `android_dialog` before going on.",
                super::profiles::describe_choices(choices)
            ),
            (None, LaunchOutcome::Started) => format!("`{pkg}` is up ({component})."),
            (None, LaunchOutcome::AlreadyInFront) => {
                format!("`{pkg}` was already in front; Android did not restart it ({component}).")
            }
            (None, LaunchOutcome::Failed) => format!("launch failed for `{pkg}` ({component})."),
            (None, LaunchOutcome::Unknown) if timed_out => format!(
                "`adb` did not confirm `{pkg}` within {} ms; it may still come up. Observe before acting.",
                LAUNCH_TIMEOUT.as_millis()
            ),
            (None, LaunchOutcome::Unknown) => {
                format!("`adb` exited without a status for `{pkg}`; observe before acting.")
            }
        };

        let status_word = match (&chooser, verdict) {
            (Some(_), _) => "chooser",
            (None, LaunchOutcome::Started) => "ok",
            (None, LaunchOutcome::AlreadyInFront) => "already_in_front",
            (None, LaunchOutcome::Failed) => "error",
            (None, LaunchOutcome::Unknown) => "unknown",
        };
        let data = serde_json::json!({
            "device": resolved_serial(device).await,
            "component": component,
            // A chooser is not the app: `launched` stays true because the
            // start was accepted, and `chooser` is the fact the caller has to
            // act on.
            "launched": started,
            "timed_out": timed_out,
            "status": status_word,
            "chooser": chooser.map(|choices| serde_json::json!({
                "package": pkg,
                "options": choices.iter().map(|choice| serde_json::json!({
                    "label": choice.label,
                    "copy": choice.copy.as_str(),
                    "note": choice.copy.note(),
                })).collect::<Vec<_>>(),
            })),
        });

        Ok(if started {
            ToolExecutionResult::success(format!("{sentence}\n{detail}")).with_data(data)
        } else {
            ToolExecutionResult::error(format!("{sentence}\n{detail}")).with_data(data)
        })
    }

    /// Whether what came to the front after a launch is the system asking
    /// which copy of an app to open, and what the options are.
    ///
    /// Best-effort in both directions. The tree may be unreadable (a phone
    /// whose accessibility service is off, a screen the platform hides); the
    /// user list may be unreadable (no shell). Neither is a launch failure, and
    /// neither is a reason to guess: the answer is `None` and the launch is
    /// reported as the `am start` verdict said.
    async fn launch_chooser(
        &self,
        device: &Option<String>,
    ) -> Option<Vec<super::profiles::Choice>> {
        let users = super::profiles::users(device).await?;
        // `am start` without `--display` puts the launch on whichever screen
        // the person is using, so that is the tree the question is in.
        let dump = dump_ui(device, Target::Screen).await.ok()?;
        let current = super::profiles::current_user(device)
            .await
            .unwrap_or(super::profiles::OWNER);
        let others: Vec<super::profiles::UserProfile> = users
            .into_iter()
            .filter(|user| user.id != current)
            .collect();
        super::profiles::chooser_options(&dump.elements, &others)
    }

    /// List the apps installed here, and which of them are also installed in
    /// another profile.
    ///
    /// The plain `pm list packages` answers for the user the shell is in, which
    /// on a phone with a work profile or a clone is one of several package
    /// lists. An app that is in two of them is *the same package name*, so a
    /// caller reading only its own list cannot tell the two copies apart — and
    /// a launch of it raises the system's "which one?" question instead of
    /// opening the app. `alsoIn` is that second answer.
    ///
    /// The extra reads are best-effort: on a phone with one user there is
    /// nothing to ask, and on a phone whose `pm` refuses `--user` the list is
    /// still the right answer to "what is installed here". Neither is a reason
    /// to fail the listing, so the result reports `profiles_known`.
    async fn list_packages(&self, device: &Option<String>) -> crate::Result<ToolExecutionResult> {
        let argv = adb_args(device, &["shell", "pm", "list", "packages"]);
        let (status, stdout, stderr) =
            run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: "adb app manager failed".to_string(),
                    cause: Some(Box::new(e)),
                })?;
        if !status.success() {
            return Ok(ToolExecutionResult::error(format!(
                "listing the apps failed: {}",
                stderr.trim()
            )));
        }
        let own = super::profiles::parse_packages(&stdout);
        let current = super::profiles::current_user(device)
            .await
            .unwrap_or(super::profiles::OWNER);
        let shared = super::profiles::other_profiles(device, current)
            .await
            .map(|others| super::profiles::also_in(&others))
            .unwrap_or_default();
        let profiles_known = super::profiles::users(device).await.is_some();

        let apps: Vec<Value> = own
            .iter()
            .map(|package| {
                let mut entry = serde_json::json!({ "package": package });
                if let Some(profiles) = shared.get(package) {
                    entry["also_in"] = serde_json::json!(profiles);
                }
                entry
            })
            .collect();
        let text = if shared.is_empty() {
            format!(
                "{} app(s) installed{}.\n{}",
                own.len(),
                if profiles_known {
                    ""
                } else {
                    " (no other profile but the phone's own)"
                },
                stdout.trim()
            )
        } else {
            let mut lines = vec![format!(
                "{} app(s) installed; {} also have a copy in another profile:",
                own.len(),
                shared.len()
            )];
            for (package, profiles) in &shared {
                lines.push(format!("  {package} — also in {}", profiles.join(", ")));
            }
            lines.join("\n")
        };

        Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
            "device": resolved_serial(device).await,
            "apps": apps,
            "also_in": shared,
            "profiles_known": profiles_known,
        })))
    }

    /// Ask the phone which activity `package` launches through.
    ///
    /// Tries the package manager first, then `monkey`, which is the launcher's
    /// own entry path. Both are read-only probes on a bounded clock: a lookup
    /// that has not answered in ten seconds has failed.
    async fn resolve_component(&self, device: &Option<String>, pkg: &str) -> Option<String> {
        let argv = adb_args(
            device,
            &[
                "shell",
                "cmd",
                "package",
                "resolve-activity",
                "--brief",
                "-c",
                "android.intent.category.LAUNCHER",
                pkg,
            ],
        );
        if let super::RunOutcome::Finished { status, stdout, .. } = super::run_cmd_timeout(
            "adb",
            &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            PROBE_TIMEOUT,
        )
        .await
        {
            if status.success() {
                if let Some(component) = parse_resolve_activity(&stdout) {
                    return Some(component);
                }
            }
            // `Error: …` on stdout is how the package manager says it could not
            // resolve; it is not worth logging, `monkey` is the next answer.
        }

        // `monkey -p pkg 1` prints the component it chose in its events dump.
        let argv = adb_args(
            device,
            &[
                "shell",
                "monkey",
                "-p",
                pkg,
                "-c",
                "android.intent.category.LAUNCHER",
                "1",
            ],
        );
        if let super::RunOutcome::Finished { stdout, .. } = super::run_cmd_timeout(
            "adb",
            &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            PROBE_TIMEOUT,
        )
        .await
        {
            // `  // com.example/.MainActivity` — the chosen component, indented
            // under `cmp=`. Older releases print it as `cmp=com.example/.MainActivity`.
            for line in stdout.lines() {
                let line = line.trim();
                let candidate = line
                    .strip_prefix("// ")
                    .or_else(|| line.strip_prefix("cmp="))
                    .unwrap_or(line);
                if let Some((p, c)) = candidate.split_once('/') {
                    if p == pkg && !c.is_empty() && !c.contains(' ') {
                        return Some(candidate.to_string());
                    }
                }
            }
        }
        None
    }
}

#[async_trait]
impl Tool for AdbAppManagerTool {
    fn name(&self) -> &str {
        "android_app_manager"
    }

    fn description(&self) -> &str {
        "Install, launch, force-stop, or list apps on an Android device. 'completed' means the \
         adb command was dispatched, not that the app started — observe afterwards to see. \
         `list_packages` marks an app also installed in another profile (`also_in`); `launch` of \
         such an app can raise the system's \"which copy?\" question, reported as `chooser`."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Manage Android apps",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "install | launch | force_stop | list_packages",
                    "enum": ["install", "launch", "force_stop", "list_packages"]
                },
                "package": { "type": "string", "description": "Package name (e.g. com.example.app)" },
                "activity": { "type": "string", "description": "Activity class (launch)" },
                "apk_path": { "type": "string", "description": "Local path to APK (install)" },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            vec!["action"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // One device: two conversations must not type into it at once. The
        // lock is per serial. See `tools::target_lock`.
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let _target = lock_device(&device).await;
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");

        // `launch` is not a plain argv: it has to find the entry point, which is
        // a query of its own, and then wait for the activity. It reports what it
        // learned rather than a single line of stdout.
        if action == "launch" {
            return self.launch(&device, &args).await;
        }
        if action == "list_packages" {
            return self.list_packages(&device).await;
        }

        let argv = match action {
            "install" => {
                let path = args.get("apk_path").and_then(|v| v.as_str()).unwrap_or("");
                adb_args(&device, &["install", "-r", path])
            }
            "force_stop" => {
                let pkg = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                adb_args(&device, &["shell", "am", "force-stop", pkg])
            }
            _ => return Ok(ToolExecutionResult::error(format!("Unknown action: {}", action))),
        };

        let (status, stdout, stderr) =
            run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: "adb app manager failed".to_string(),
                    cause: Some(Box::new(e)),
                })?;

        if !status.success() {
            return Ok(ToolExecutionResult::error(format!("adb app manager failed: {}", stderr)));
        }

        Ok(
            ToolExecutionResult::success(format!("Action '{}' completed\n{}", action, stdout))
                .with_data(serde_json::json!({ "device": resolved_serial(&device).await })),
        )
    }
}

// ── ADB UI Tree Tool ───────────────────────────────────────────────────────

/// Dump the Android accessibility UI tree via `uiautomator`.
#[derive(Debug)]
pub struct AdbUiTreeTool {
    device: Option<String>,
}

impl Default for AdbUiTreeTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbUiTreeTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// What a node *is*, as opposed to what it says.
///
/// Separate from [`UiElement`] and deliberately outside [`element_key`]: these
/// are states that change while the element stays the same element (a checkbox
/// gets ticked, a field takes focus). Identity must not move when they do.
///
/// Two sources fill this in: the accessibility service hands most of it over
/// directly (`A11yBridge.kt`), and `uiautomator dump` carries the same
/// attributes for everything except `editable`, which is inferred from the
/// class because the XML has no such attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct NodeState {
    /// The node itself scrolls — a list, a pager, a wheel picker.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub scrollable: bool,
    /// Text can be typed into it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub editable: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub checkable: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub checked: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub selected: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub focused: bool,
    /// A password field: its contents must never be summarised to the model.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub password: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub long_clickable: bool,
    /// `ACTION_SCROLL_FORWARD` / `ACTION_SCROLL_BACKWARD` are available. A
    /// scrollable node without them can only be moved by a gesture.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub scroll_forward: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub scroll_backward: bool,
    /// This node reads as a wheel picker: a short, centred, evenly spaced run
    /// of values (a time, a date, a region) rather than a list.
    ///
    /// Decided by the source that has the children, not by this struct — see
    /// `wheel::looks_like_wheel`. The flag is what lets
    /// `android_set_wheel` find a picker without the model having to describe
    /// its geometry, and its absence is why a wheel used to be driven by
    /// guessing which way a swipe turns it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub wheel: bool,
    /// Distance from the root of the read it came from.
    ///
    /// The element list is flat, so depth is the only surviving trace of the
    /// hierarchy: it is what lets a scroll aim at the innermost list containing
    /// the point rather than at any scrollable container on the screen.
    #[serde(skip_serializing_if = "is_zero")]
    pub depth: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl NodeState {
    /// The node is structurally meaningful without carrying a name.
    ///
    /// An unnamed clickable `View` is a tap target nobody described; an unnamed
    /// list, field or toggle still tells the reader where it is and what can be
    /// done there. [`assess_tree`] uses this so a blank-looking list screen is
    /// not written off as undescribable.
    pub fn is_structural(&self) -> bool {
        self.scrollable || self.editable || self.checkable || self.wheel
    }
}

/// One node of the Android accessibility tree, flattened for the model.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct UiElement {
    /// 1-based index the model refers to this element by.
    pub index: usize,
    /// What this element *is*, independent of where it sits.
    ///
    /// An index is only meaningful against the read it came from: the screen
    /// scrolls, a row appears, and "element 7" is now something else. The key
    /// is built from what identifies the element (its resource id, label and
    /// class) and not from its position, so a later call can find the same
    /// element again in a tree it has never seen. Duplicates are possible and
    /// are reported rather than guessed at — see [`relocate`].
    #[serde(skip_serializing_if = "String::is_empty")]
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_desc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    /// Fully-qualified class, shortened to the last segment.
    pub class: String,
    pub clickable: bool,
    pub enabled: bool,
    /// What the node is beyond its label — never part of its identity.
    #[serde(flatten)]
    pub state: NodeState,
    /// `(x1, y1, x2, y2)` in device pixels.
    pub bounds: (i32, i32, i32, i32),
    /// Centre of `bounds` — what a tap should target.
    pub center: (i32, i32),
    /// The accessibility node this element came from, when the source keeps
    /// one (`a11y::NodeRef`).
    ///
    /// Not part of the element's identity and never sent to the model: it is a
    /// handle into the phone's own process that expires with the node, whereas
    /// `key` is what survives a read. It exists so an action can be addressed to
    /// the control itself instead of to a point on the screen.
    #[serde(skip)]
    pub node_ref: Option<super::a11y::NodeRef>,
}

impl UiElement {
    /// An element with its key computed from the fields that identify it.
    ///
    /// Construction goes through here so a key can never be a stale copy of
    /// fields that were changed after it was computed.
    ///
    /// Test-only: both real sources know something about what the node is, so
    /// they go through [`with_state`](Self::with_state).
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        index: usize,
        text: Option<String>,
        content_desc: Option<String>,
        resource_id: Option<String>,
        class: String,
        clickable: bool,
        enabled: bool,
        bounds: (i32, i32, i32, i32),
    ) -> Self {
        Self::with_state(
            index,
            text,
            content_desc,
            resource_id,
            class,
            clickable,
            enabled,
            NodeState::default(),
            bounds,
        )
    }

    /// [`new`](Self::new) for a source that also reports what the node is.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn with_state(
        index: usize,
        text: Option<String>,
        content_desc: Option<String>,
        resource_id: Option<String>,
        class: String,
        clickable: bool,
        enabled: bool,
        state: NodeState,
        bounds: (i32, i32, i32, i32),
    ) -> Self {
        let (x1, y1, x2, y2) = bounds;
        let mut element = Self {
            index,
            key: String::new(),
            text,
            content_desc,
            resource_id,
            class,
            clickable,
            enabled,
            state,
            bounds,
            center: ((x1 + x2) / 2, (y1 + y2) / 2),
            node_ref: None,
        };
        element.key = element_key(&element);
        element
    }
}

/// Which rung of the delivery ladder pressed something.
///
/// The order is fixed and it matters: a node action tells the *control* it was
/// pressed and reaches widgets that ignore injected events; a service gesture
/// is a real touch stream and reaches canvases and sliders; a shell injection
/// is the last resort and works everywhere `adb` does. The result reports
/// which one was used, because the three fail in different ways and a caller
/// reading "the screen did not change" needs to know what was tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Via {
    /// `AccessibilityNodeInfo.performAction` on the node the read named.
    Semantic,
    /// `dispatchGesture` through the accessibility service.
    Gesture,
    /// `input tap` / the display helper's `MotionEvent`.
    Shell,
    /// Nothing was dispatched: the caller was refused.
    None,
}

impl Via {
    /// The word the result carries.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Semantic => "semantic",
            Self::Gesture => "gesture",
            Self::Shell => "shell",
            Self::None => "none",
        }
    }

    /// The clause the result sentence carries.
    pub(super) fn explain(self) -> &'static str {
        match self {
            Self::Semantic => {
                "It was pressed as a control (an accessibility action on the node), \
                              not as a point on the screen."
            }
            Self::Gesture => {
                "It was pressed with a touch gesture sent by the accessibility \
                              service, not by shell injection."
            }
            Self::Shell => "It was pressed by shell injection at its coordinates.",
            Self::None => "",
        }
    }
    /// The clause the sentence carries, when the rung is news.
    ///
    /// A shell press is what every result was before this ladder existed and
    /// says nothing new; a control action or a service gesture is the case a
    /// caller reading "the screen did not change" needs to see.
    pub(super) fn note(self) -> Option<&'static str> {
        match self {
            Self::Semantic | Self::Gesture => Some(self.explain()),
            Self::Shell | Self::None => None,
        }
    }
}

/// Press the element the read named, the best way this phone can.
///
/// The ladder is tried in order and each rung says whether the next one should
/// be tried. A node that is *gone* ends the ladder: pressing its old
/// coordinates would hit whatever has since moved into place, which is the
/// mistake the reference exists to prevent — the caller re-reads instead.
/// A node that merely refuses (no such action, the action did not take) falls
/// through, because the control is still there and a gesture or a tap lands on
/// the thing that was meant.
pub(super) async fn press_element(
    element: &UiElement,
    long: bool,
    allow_gesture: bool,
) -> (Via, Option<String>) {
    // Rung one: the control itself. Only possible when the read came from the
    // accessibility service — `uiautomator` has no way to address a node.
    if let Some(node) = element.node_ref {
        if let Some(bridge) = crate::device::bridge() {
            let action = super::a11y::NodeAction::press(long);
            if let Some(answer) = super::a11y::act(bridge, node, action, None, 1).await {
                if answer.ok {
                    return (Via::Semantic, None);
                }
                if answer.node_is_gone() {
                    return (
                        Via::None,
                        Some(format!(
                            "Refused: {} was found in the live screen, but the control it named \
                             is gone — the screen changed between the read and the press. Nothing \
                             was pressed: a coordinate tap would have hit whatever took its \
                             place. Re-read the screen with android_ui_tree and retry.",
                            element_label(element).unwrap_or("that element")
                        )),
                    );
                }
                // Rung two: a real touch stream at the element, which reaches a
                // view that ignores `input tap` (a canvas, a game surface).
                // Only the *user's* screen: a virtual display's gestures are
                // the display helper's job, and the service's gesture injection
                // is not tied to a display.
                if allow_gesture {
                    if let Some(gesture) =
                        super::a11y::gesture(bridge, &press_paths(element), PRESS_GESTURE_MS).await
                    {
                        if gesture.ok {
                            return (Via::Gesture, None);
                        }
                        if gesture.no_service() {
                            // The service stopped between the read and now; the
                            // shell route below still works.
                            tracing::debug!(
                                "accessibility gesture unavailable: the service is not running"
                            );
                        }
                    }
                }
            }
        }
    }
    (Via::Shell, None)
}

/// How long a press sent as a gesture takes.
///
/// Short enough to read as a tap rather than a hold (`ViewConfiguration`'s own
/// long-press threshold is 500 ms), long enough that the platform sees more
/// than one frame of contact.
pub(super) const PRESS_GESTURE_MS: u32 = 80;

/// The finger path for a press on `element`: down and up on the same point.
///
/// A gesture of one point would be a contact the platform sees once; three
/// points over the same coordinate read as a press the way a finger does.
pub(super) fn press_paths(element: &UiElement) -> Vec<Vec<(i32, i32)>> {
    let point = element.center;
    vec![vec![point, point, point]]
}

/// The identity of an element, for finding it again on a later read.
///
/// Deliberately not a hash of the whole element: bounds and index are position,
/// and an element that scrolled is the same element. Two elements that agree on
/// everything here are interchangeable to a tap, which is why [`relocate`]
/// refuses to choose between them.
pub fn element_key(element: &UiElement) -> String {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    element.resource_id.hash(&mut hasher);
    element.text.hash(&mut hasher);
    element.content_desc.hash(&mut hasher);
    element.class.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// Find the element a key names in the tree as it is now.
///
/// Exactly one match relocates. None, or several identical ones, does not: the
/// alternative is tapping the first of two rows that look the same, which is
/// precisely the silent wrong-target failure the key exists to prevent. The
/// refusal names the candidates so the caller can pick by index from a fresh
/// read instead of guessing.
pub fn relocate<'a>(elements: &'a [UiElement], key: &str) -> Result<&'a UiElement, String> {
    let matches: Vec<&UiElement> = elements
        .iter()
        .filter(|element| !element.key.is_empty() && element.key == key)
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(
            "STALE_REF: nothing with that key is on the screen now. The element was replaced, \
             scrolled away, or the screen changed — re-read it with android_ui_tree and use the \
             new index or key."
                .to_string(),
        ),
        many => {
            let choices: Vec<String> = many
                .iter()
                .map(|element| {
                    format!(
                        "[{}] {} {:?} center=({},{})",
                        element.index,
                        element.class,
                        element_label(element).unwrap_or("-"),
                        element.center.0,
                        element.center.1
                    )
                })
                .collect();
            Err(format!(
                "STALE_REF: {} elements match that key and none of them is distinguishably the \
                 one you meant:\n{}\nUse the index of the right one from android_ui_tree.",
                many.len(),
                choices.join("\n")
            ))
        }
    }
}

/// Parse a `bounds="[x1,y1][x2,y2]"` attribute.
fn parse_bounds(raw: &str) -> Option<(i32, i32, i32, i32)> {
    let rest = raw.strip_prefix('[')?;
    let (first, rest) = rest.split_once("][")?;
    let second = rest.strip_suffix(']')?;
    let mut a = first.split(',');
    let (x1, y1) = (a.next()?.trim().parse().ok()?, a.next()?.trim().parse().ok()?);
    let mut b = second.split(',');
    let (x2, y2) = (b.next()?.trim().parse().ok()?, b.next()?.trim().parse().ok()?);
    Some((x1, y1, x2, y2))
}

/// Flatten a `uiautomator dump` document into an indexed element list.
///
/// Only nodes the model can act on or read are kept — those with text, a
/// content description or a resource id, plus the ones that are actionable or
/// structural (clickable, scrollable, editable, checkable). Bare layout
/// containers carry nothing actionable and cost context, and the raw XML is
/// ~10x the size of this list.
///
/// `index` is assigned here, and is what `android_input` should be pointed at
/// via `center` rather than making the model read `bounds` itself.
pub fn parse_uiautomator_xml(xml: &str) -> Result<Vec<UiElement>, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("invalid UI tree XML: {e}"))?;

    let mut out = Vec::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("node")) {
        let attr = |key: &str| {
            node.attribute(key)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let flag = |key: &str| attr(key).as_deref() == Some("true");
        let text = attr("text");
        let content_desc = attr("content-desc");
        let resource_id = attr("resource-id");
        let clickable = flag("clickable");
        let long_clickable = flag("long-clickable");
        let scrollable = flag("scrollable");
        let checkable = flag("checkable");
        let password = flag("password");
        let class = attr("class")
            .map(|c| c.rsplit('.').next().unwrap_or(&c).to_string())
            .unwrap_or_default();
        // The dump has no `editable` attribute; the class is the honest signal,
        // and the same list `A11yBridge` reports through `isEditable` in
        // practice. Text *content* is not evidence — a label is not a field.
        let editable = matches!(
            class.as_str(),
            "EditText" | "AutoCompleteTextView" | "TextInputEditText" | "SearchView"
        );

        let actionable = clickable || long_clickable || scrollable || checkable || editable;
        if text.is_none() && content_desc.is_none() && resource_id.is_none() && !actionable {
            continue;
        }
        let Some(bounds) = attr("bounds").as_deref().and_then(parse_bounds) else {
            continue;
        };

        let state = NodeState {
            scrollable,
            editable,
            checkable,
            checked: flag("checked"),
            selected: flag("selected"),
            focused: flag("focused"),
            password,
            long_clickable,
            // uiautomator reports `scrollable` but not which directions, so a
            // node it calls scrollable is assumed to accept both.
            scroll_forward: scrollable,
            scroll_backward: scrollable,
            // `uiautomator` says nothing about wheels, and the dump has no
            // children to judge a run of values by; the class is the one
            // honest signal, and it is the same one the bridge trusts first.
            wheel: class.ends_with("NumberPicker"),
            depth: node_depth(node),
        };

        out.push(UiElement::with_state(
            out.len() + 1,
            text,
            content_desc,
            resource_id,
            class,
            clickable,
            attr("enabled").as_deref() != Some("false"),
            state,
            bounds,
        ));
    }
    Ok(out)
}

/// How many `node` ancestors a node has.
///
/// `uiautomator` nests every element in `<node>` elements all the way up, so
/// counting ancestors is the depth. The bridge reports depth directly; both
/// have to mean the same thing or a scroll would pick a different container
/// depending on which source answered.
fn node_depth(node: roxmltree::Node<'_, '_>) -> u32 {
    node.ancestors()
        // `ancestors()` starts at the node itself; the first real ancestor is
        // its parent, and the root `node` element is depth 1 — the same as the
        // bridge reports, where the root window is 0.
        .skip(1)
        .filter(|ancestor| ancestor.has_tag_name("node"))
        .count() as u32
}

/// Capture the device screen as raw PNG bytes.
///
/// Uses the byte-safe runner: the PNG has to reach the encoder exactly as the
/// device produced it, and the `String` variant of `run_cmd` would replace every
/// non-UTF-8 byte with U+FFFD and corrupt the image.
async fn capture_png(device: &Option<String>, target: Target) -> crate::Result<Vec<u8>> {
    if let Target::Display(id) = target {
        // The helper owns the display's surface, so the frame comes from it
        // rather than from `screencap` (which would photograph the user's
        // screen). It is a PNG either way.
        let bridge = display_bridge().map_err(|message| {
            crate::error::SyscityError::ExternalService { source: message, cause: None }
        })?;
        let display = super::display::VirtualDisplay::attach(bridge, id);
        return display
            .screenshot()
            .await
            .map(|frame| frame.png)
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: e.to_string(),
                cause: None,
            });
    }
    let args = adb_args(device, &["exec-out", "screencap", "-p"]);

    let (status, stdout, stderr) =
        run_cmd_bytes("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: "adb screencap failed".to_string(),
                cause: Some(Box::new(e)),
            })?;
    if !status.success() {
        return Err(crate::error::SyscityError::ExternalService {
            source: format!("adb screencap failed: {stderr}"),
            cause: None,
        });
    }
    Ok(stdout)
}

/// The user's screen as a brightness grid, for screens the tree cannot see.
///
/// The helper's `frameSignature` only covers displays it created, so display 0
/// has to come from `screencap` — and deliberately **without** `-p`. Without
/// it `screencap` emits the raw framebuffer: some little-endian `u32` header
/// fields (width, height, format, and from API 28 a colorspace) followed by
/// `width * height * 4` bytes of RGBA. That is exactly what a 12×24 luma grid
/// needs, and it costs no PNG decoder — which matters because the phone build
/// has no `vision` feature and therefore no `image` crate.
///
/// The header length is derived rather than assumed (`12` on API 26/27, `16`
/// on 28+), and the `format` field is checked: a non-RGBA_8888 frame would have
/// the same length with a different byte layout, and reading it as RGBA would
/// produce a grid of noise that "differs" every time.
///
/// This is a second ADB round trip, not free: 1080×2400 is ~10 MB uncompressed.
/// Callers must treat it as the expensive path it is.
async fn user_screen_signature(device: &Option<String>) -> Option<super::display::FrameSignature> {
    let args = adb_args(device, &["exec-out", "screencap"]);
    let (status, bytes, _stderr) =
        run_cmd_bytes("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
            .await
            .ok()?;
    if !status.success() {
        return None;
    }
    raw_screencap_signature(&bytes)
}

/// Turn a raw `screencap` frame into a brightness grid.
///
/// `None` when the frame is truncated, its format is not RGBA_8888, or its
/// dimensions are too small to divide into a grid — every one of those is a
/// reason not to guess about the picture rather than a reason to fail the
/// action.
fn raw_screencap_signature(frame: &[u8]) -> Option<super::display::FrameSignature> {
    // API 26/27 write 12 bytes, API 28+ write 16. Both are ruled out by the
    // pixel count, so the header length is whatever is left over. Sixteen is
    // tried first: a 12-byte frame under a 16-byte reading is four bytes short
    // and is rejected, while a 16-byte frame under a 12-byte reading is not —
    // the leftover four bytes just shift the pixels, which is the one wrong
    // answer this check exists to avoid.
    const RGBA_8888: u32 = 1;
    let read_u32 = |offset: usize| -> Option<u32> {
        frame
            .get(offset..offset + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    for header in [16usize, 12] {
        if frame.len() < header {
            continue;
        }
        let Some(width) = read_u32(0) else { continue };
        let Some(height) = read_u32(4) else { continue };
        let Some(format) = read_u32(8) else { continue };
        if width == 0 || height == 0 || format != RGBA_8888 {
            continue;
        }
        let pixels = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if frame.len() != header + pixels {
            continue;
        }
        return luma_grid_from_rgba(&frame[header..], width, height);
    }
    None
}

/// Average luminance per cell, sampled the way the helper samples it.
///
/// Same grid, same 4×4 sample points inside each cell, same `(3r + 6g + b) / 10`
/// weights as `FrameSignature.lumaGrid` on the Kotlin side. The two producers
/// have to agree: a grid from `screencap` is compared against one the helper
/// produced for a virtual display whenever an action on a display is checked.
pub fn luma_grid_from_rgba(
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Option<super::display::FrameSignature> {
    const GRID_W: u32 = 12;
    const GRID_H: u32 = 24;
    const SAMPLES: u32 = 4;
    let cell_width = width / GRID_W;
    let cell_height = height / GRID_H;
    if cell_width == 0 || cell_height == 0 {
        return None;
    }
    if rgba.len() < (width as usize) * (height as usize) * 4 {
        return None;
    }
    let luma_at = |x: u32, y: u32| -> u32 {
        let offset = ((y as usize) * (width as usize) + (x as usize)) * 4;
        let red = u32::from(rgba[offset]);
        let green = u32::from(rgba[offset + 1]);
        let blue = u32::from(rgba[offset + 2]);
        (red * 3 + green * 6 + blue) / 10
    };
    let mut cells = Vec::with_capacity((GRID_W * GRID_H) as usize);
    for gy in 0..GRID_H {
        for gx in 0..GRID_W {
            let mut sum = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    sum += luma_at(
                        gx * cell_width + sx * cell_width / SAMPLES,
                        gy * cell_height + sy * cell_height / SAMPLES,
                    );
                }
            }
            cells.push((sum / (SAMPLES * SAMPLES)).min(255) as u8);
        }
    }
    Some(super::display::FrameSignature {
        columns: GRID_W,
        rows: GRID_H,
        cells,
    })
}

/// The screen rectangle a dump describes: the root `<node>`'s bounds.///
/// `uiautomator dump` wraps everything in `<hierarchy>` with a single root node
/// whose bounds are the full display. Comparing that against the screenshot's
/// pixel size is how a coordinate-space mismatch is caught.
pub fn parse_screen_bounds(xml: &str) -> Option<(i32, i32)> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let root = doc.descendants().find(|n| {
        n.has_tag_name("node") && n.parent().is_some_and(|p| p.has_tag_name("hierarchy"))
    })?;
    parse_bounds(root.attribute("bounds")?).map(|(_, _, x2, y2)| (x2, y2))
}

/// The package the root node belongs to, from a `uiautomator` dump.
///
/// Read from the first node that names one: the hierarchy element sometimes
/// carries the package itself, and the root node always does.
fn parse_package(xml: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    doc.descendants()
        .filter(|n| n.has_tag_name("node"))
        .find_map(|n| n.attribute("package"))
        .map(str::to_string)
        .filter(|p| !p.is_empty())
}

/// Width and height from a PNG's IHDR chunk.
pub(super) fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.len() < 24 || bytes[..8] != SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((width, height))
}

/// Whether the element coordinates and the screenshot describe the same space.
///
/// They come from two separate reads, so a rotation — or a scaled capture —
/// between them makes every coordinate in the tree wrong for the image the model
/// is looking at, and taps land somewhere else. Comparing the sizes is cheap and
/// catches it; a rotation is expected to simply swap the axes.
///
/// When either side is unknown there is nothing to compare, and claiming a
/// mismatch would be worse than staying quiet.
fn coordinate_space_consistent(screen: Option<(i32, i32)>, shot: Option<(u32, u32)>) -> bool {
    match (screen, shot) {
        (Some((sw, sh)), Some((iw, ih))) => {
            let (sw, sh) = (sw.max(0) as u32, sh.max(0) as u32);
            (sw, sh) == (iw, ih) || (sh, sw) == (iw, ih)
        }
        _ => true,
    }
}

/// A UI dump that could not be read, with a stable code for the cause.
///
/// The code is the part a caller branches on; the message is what the model
/// reads. `WebErrorCode` in `tools::web` is the same idea, and the wire values
/// belong to the tool contract in the same way: they ride on the result as
/// `data.code` so a caller does not have to parse the sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpFailure {
    /// One of the constants below.
    pub code: &'static str,
    /// What happened, and what to do instead.
    pub message: String,
}

/// The device is gone: unplugged, offline, or not authorised.
pub const DEVICE_GONE: &str = "DEVICE_GONE";
/// The screen never stopped changing, so uiautomator could not settle it.
pub const SCREEN_NEVER_IDLE: &str = "SCREEN_NEVER_IDLE";
/// No accessibility root: a secure window, or another holder of UiAutomation.
pub const SECURE_WINDOW: &str = "SECURE_WINDOW";
/// The accessibility service is not enabled.
pub const ACCESSIBILITY_DISABLED: &str = "ACCESSIBILITY_DISABLED";
/// The dump failed for a reason this code does not name.
pub const DUMP_FAILED: &str = "DUMP_FAILED";
/// A virtual display's tree could not be read (no service, or API < 30).
pub const DISPLAY_TREE_UNAVAILABLE: &str = "DISPLAY_TREE_UNAVAILABLE";

impl DumpFailure {
    fn new(code: &'static str, message: String) -> Self {
        Self { code, message }
    }

    /// Classify what `uiautomator` said.
    ///
    /// Its stderr is cryptic and looks the same across causes that need
    /// completely different responses — a device that is gone, a screen that
    /// never settles, and a secure window all just "fail". The fallback is
    /// always available though: the screenshot still works when the element
    /// tree does not.
    fn classify(stderr: &str) -> Self {
        let lowered = stderr.to_lowercase();
        // adb puts the serial in the middle: `device 'emulator-5554' not found`.
        let device_gone = (lowered.contains("device") && lowered.contains("not found"))
            || lowered.contains("device offline")
            || lowered.contains("no devices/emulators found")
            || lowered.contains("unauthorized");
        let (code, symptom) = if device_gone {
            (
                DEVICE_GONE,
                "the device is not connected (or has gone offline) — reconnect it and try again",
            )
        } else if lowered.contains("could not get idle state") {
            (SCREEN_NEVER_IDLE, "the screen never settled (something on it keeps animating)")
        } else if lowered.contains("null root node") || lowered.contains("could not get root node")
        {
            (
                SECURE_WINDOW,
                "no accessibility root node was available — likely a secure window (a password or \
                 banking screen), or another app is holding the UiAutomation connection",
            )
        } else if lowered.contains("permission denial") {
            (ACCESSIBILITY_DISABLED, "the accessibility service is not enabled")
        } else {
            (DUMP_FAILED, "uiautomator reported an error")
        };

        let raw = stderr.trim();
        let message = if raw.is_empty() {
            format!(
                "Could not read the UI tree: {symptom}. Work from a screenshot instead — \
                 android_observe returns one alongside the tree."
            )
        } else {
            format!(
                "Could not read the UI tree: {symptom}. Raw output: {raw}. Work from a screenshot \
                 instead — android_observe returns one alongside the tree."
            )
        };
        Self::new(code, message)
    }
}

impl std::fmt::Display for DumpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// One line per element, for the model to read directly.
pub(super) fn format_ui_summary(elements: &[UiElement]) -> String {
    if elements.is_empty() {
        // A dump that succeeds but yields nothing is not a failure to report —
        // it is a screen the accessibility tree cannot describe (a game, a
        // canvas, a secure surface). Saying so, and pointing at the screenshot,
        // is the difference between the model retrying the tree forever and it
        // switching approach.
        return "The UI tree has no actionable elements — this screen is probably a game, a \
                canvas, or a secure surface that exposes no accessibility nodes. Work from the \
                screenshot instead (android_observe returns one alongside this list)."
            .to_string();
    }
    let mut lines = vec![format!("{} actionable element(s):", elements.len())];
    lines.extend(elements.iter().map(format_ui_element));
    lines.join("\n")
}

/// One element as the model reads it: `[3] Button "Send" clickable center=(1,2)`.
///
/// The index is the element's own, so a line rendered on its own from a diff
/// is still something a later call can point at.
pub(super) fn format_ui_element(e: &UiElement) -> String {
    // A password field's contents must not reach the model — not as a label,
    // and not as a resource id it could be read back through.
    let label = if e.state.password {
        "[password]".to_string()
    } else {
        e.text
            .as_deref()
            .or(e.content_desc.as_deref())
            .map(|s| format!("{s:?}"))
            .or_else(|| e.resource_id.clone())
            .unwrap_or_else(|| "-".to_string())
    };
    let mut flags = String::new();
    if e.clickable {
        flags.push_str(" clickable");
    }
    if !e.enabled {
        flags.push_str(" disabled");
    }
    if e.state.scrollable {
        flags.push_str(" scrollable");
    }
    if e.state.editable {
        flags.push_str(" editable");
    }
    if e.state.checkable {
        flags.push_str(if e.state.checked {
            " checked"
        } else {
            " unchecked"
        });
    }
    if e.state.wheel {
        flags.push_str(" wheel");
    }
    if e.state.selected {
        flags.push_str(" selected");
    }
    if e.state.focused {
        flags.push_str(" focused");
    }
    format!(
        "[{}] {} {}{} center=({},{})",
        e.index, e.class, label, flags, e.center.0, e.center.1
    )
}

// ── Targets ────────────────────────────────────────────────────────────────

/// Where a read or an action goes.
///
/// Most calls mean "the phone's own screen". The other case is a virtual
/// display the agent created for itself so the user keeps using the phone —
/// there, the tree comes from the accessibility service scoped to that display,
/// the picture from the helper's frame, and the input from the helper too
/// (`input -d`), because ADB's own `input` only reaches the default display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The device's own screen.
    Screen,
    /// A virtual display, by the id `android_display start` returned.
    Display(i32),
}

impl Target {
    /// This target's display id, for calls that are addressed by number.
    ///
    /// The main screen is 0 to the platform, so the conversion is not a
    /// judgement about which screen a caller meant.
    pub fn display_id(self) -> i32 {
        match self {
            Self::Screen => super::takeover::MAIN_DISPLAY,
            Self::Display(id) => id,
        }
    }

    /// Read the optional `display` argument. Absent means the user's screen.
    pub(super) fn from_args(args: &Value) -> Result<Self, String> {
        match args.get("display") {
            None | Some(Value::Null) => Ok(Self::Screen),
            Some(value) => {
                let id = value
                    .as_i64()
                    .ok_or_else(|| "`display` must be an integer id".to_string())?;
                i32::try_from(id)
                    .map(Self::Display)
                    .map_err(|_| format!("display id {id} is out of range"))
            }
        }
    }
}

/// One input already resolved into device-independent coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InputOp {
    /// One or more taps, dispatched back to back.
    Taps(Vec<(i32, i32)>),
    Swipe {
        from: (i32, i32),
        to: (i32, i32),
    },
    /// One or two fingers, each a path of points.
    Gesture {
        paths: Vec<Vec<(i32, i32)>>,
        duration_ms: u32,
    },
    Text(String),
    Key(String),
}

/// The bridge, or the reason a display operation cannot happen here.
pub(super) fn display_bridge() -> Result<crate::device::DeviceBridgeRef, String> {
    crate::device::bridge().cloned().ok_or_else(|| {
        "A virtual display only exists inside the Syscity app on the phone itself; this host has \
         no display helper. Use the display-less actions instead."
            .to_string()
    })
}

/// Send a resolved input to the agent's own display.
async fn dispatch_display(id: i32, op: &InputOp) -> Result<(), String> {
    let bridge = display_bridge()?;
    let display = super::display::VirtualDisplay::attach(bridge, id);
    let failed = |e: super::display::DisplayError| e.to_string();
    match op {
        InputOp::Taps(points) => {
            for (x, y) in points {
                display.tap(*x, *y).await.map_err(failed)?;
            }
            Ok(())
        }
        InputOp::Swipe { from, to } => display.swipe(*from, *to, 300).await.map_err(failed),
        InputOp::Gesture { paths, duration_ms } => {
            // Interleave the fingers point by point: the helper builds one
            // `MotionEvent` per step with every finger's position, so the two
            // have to arrive together rather than one path after the other.
            let steps = paths.iter().map(Vec::len).max().unwrap_or(0);
            let mut points = Vec::with_capacity(steps * paths.len());
            for step in 0..steps {
                for (finger, path) in paths.iter().enumerate() {
                    let (x, y) = path[step.min(path.len().saturating_sub(1))];
                    points.push((finger as i32, x, y));
                }
            }
            display.gesture(&points, *duration_ms).await.map_err(failed)
        }
        InputOp::Text(text) => display.text(text).await.map_err(failed),
        InputOp::Key(keycode) => display.key(keycode).await.map_err(failed),
    }
}

/// Read a written field back and judge whether the text arrived.
///
/// `before` is the tree from just before the write: it is what names the field
/// (see [`locate_for_readback`]) and what says whether the field's answer has
/// moved at all.
///
/// Returns `None` when there is nothing to check — no text, or the caller
/// turned verification off. Every other outcome is a [`Readback`], including
/// the ones that could not decide: silence with a reason is what lets the model
/// tell "the field is a password box" from "the text did not go in".
async fn read_back_write(
    device: &Option<String>,
    target: Target,
    text: &str,
    before: Option<&[UiElement]>,
) -> Readback {
    if text.trim().is_empty() {
        return Readback::unknown("nothing was typed");
    }
    let Some(before) = before else {
        return Readback::unknown(
            "the screen could not be read before the write, so there is nothing to read back from",
        );
    };
    // The write point is the focused/nearest field's centre; the element that
    // was written into is the one a person would look at.
    let Some(field) = locate_for_readback(before, write_point(before)) else {
        return Readback::unknown(
            "no text field was on the screen before the write, so the text went wherever focus \
             happened to be and cannot be read back",
        );
    };
    if field.state.password {
        return Readback::unknown(
            "that is a password field — Android does not expose its contents, so the write \
             cannot be verified",
        );
    }
    let baseline = field.text.clone();

    // A beat, then one read. The delay is on the accessibility path too: the
    // input has travelled through another process before the tree sees it.
    tokio::time::sleep(std::time::Duration::from_millis(READBACK_DELAY_MS)).await;
    let after = match dump_ui(device, target).await {
        Ok(dump) => dump,
        Err(failed) => {
            return Readback::unknown(format!(
                "the screen could not be read back after the write ({})",
                failed.message
            ))
        }
    };

    // The same field, found again by what did not change: an editable node in
    // the same place with the same resource id and class. The key cannot be
    // used — it hashes the text, which is exactly what just changed.
    let now = after
        .elements
        .iter()
        .find(|element| {
            (element.state.editable || element.state.password)
                && element.resource_id == field.resource_id
                && element.class == field.class
                && intersects(element.bounds, field.bounds)
        })
        .or_else(|| locate_for_readback(&after.elements, field.center));
    let Some(now) = now else {
        return Readback::unknown(
            "the field is no longer on the screen — the screen moved on after the write",
        );
    };
    let actual = now.text.clone().unwrap_or_default();
    let reacted = screen_echoed(before, &after.elements, text);
    let verdict = readback_verdict(text, &actual, baseline.as_deref(), reacted);
    Readback {
        verdict,
        actual: Some(actual),
        note: (verdict == READBACK_UNKNOWN).then(|| {
            "the field still reads what it did before the write, but the screen shows the typed \
             text, so the app received it and this field cannot confirm where it landed"
                .to_string()
        }),
    }
}

/// Where the write was aimed, for choosing the field to read back.
///
/// The focused field's centre when the tree names one — that is where the text
/// went regardless of what else is on screen — and otherwise the centre of the
/// tree, which is the point every one of these tools would have used.
fn write_point(before: &[UiElement]) -> (i32, i32) {
    before
        .iter()
        .find(|element| element.state.editable && element.state.focused)
        .map(|element| element.center)
        .unwrap_or_else(|| {
            let (x1, y1) = before
                .first()
                .map(|e| (e.bounds.0, e.bounds.1))
                .unwrap_or((0, 0));
            let (x2, y2) = before
                .last()
                .map(|e| (e.bounds.2, e.bounds.3))
                .unwrap_or((0, 0));
            ((x1 + x2) / 2, (y1 + y2) / 2)
        })
}

/// Whether two rectangles overlap at all.
fn intersects(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
}

/// The sentence a write's result ends with, when it has one.
fn readback_note(readback: Option<&Readback>) -> Option<String> {
    let read = readback?;
    match read.verdict {
        READBACK_MATCH => Some("Read back: the field holds the text.".to_string()),
        READBACK_MISMATCH => Some(format!(
            "Read back: the field reads {:?} instead — the text did not go in as typed. Do not \
             press Enter; check the field and retry.",
            read.actual.as_deref().unwrap_or("")
        )),
        _ => read
            .note
            .as_ref()
            .map(|note| format!("Read back: not verified — {note}")),
    }
}

/// Paste `text` into whatever has focus on the display.
async fn paste_on_display(display_id: i32, text: &str) -> Result<(), String> {
    let bridge = display_bridge()?;
    super::display::VirtualDisplay::attach(bridge, display_id)
        .paste_text(text)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Type `text` onto the user's own screen by pasting it through the helper.
///
/// This is the fallback for non-ASCII text when ADBKeyboard is not installed:
/// `input text` types through the key character map and mangles everything
/// outside ASCII, so a Chinese message would arrive as nothing. The helper runs
/// as uid 2000, which can inject the Ctrl+V pair onto display 0 and can put the
/// person's own clipboard back afterwards — neither of which a caller outside
/// the phone can do.
async fn paste_on_user_screen(text: &str) -> Result<(), String> {
    let bridge = display_bridge()?;
    // Asking only works when a helper is already there — `ensure_helper` needs
    // the nonce the app's own hub mints, which is what makes it callable from
    // the phone and nowhere else. Typing must not spawn a shell process on
    // somebody's phone as a side effect anyway, so a helper that is not running
    // is a "no", not a "start one".
    if !super::display::available(&bridge).await {
        return Err("the display helper is not running on this device".to_string());
    }
    // Display 0 is the user's own; the helper reaches it through injected keys
    // rather than through a virtual display record. Appending, not replacing:
    // a replace would wipe whatever they had already typed into the field.
    super::display::VirtualDisplay::attach(bridge, 0)
        .paste_text_after(text)
        .await
        .map_err(|e| e.to_string())
}

/// The brightness grid of a display, or `None` when it has drawn nothing.
async fn display_pixels(display_id: i32) -> Option<super::display::FrameSignature> {
    let bridge = display_bridge().ok()?;
    super::display::VirtualDisplay::attach(bridge, display_id)
        .frame_signature()
        .await
        .ok()
        .flatten()
}

/// Parse the `paths` argument of a `gesture` action.
///
/// Accepted: one or two paths, each 2–32 `[x, y]` points. Two points is the
/// shortest thing that is a gesture; more than two fingers has no use on a
/// phone and would only make the injection wrong in a way that is hard to see.
fn parse_gesture_paths(value: Option<&Value>) -> Result<Vec<Vec<(i32, i32)>>, String> {
    let raw = value
        .and_then(Value::as_array)
        .ok_or_else(|| "'gesture' needs 'paths': an array of one or two point lists".to_string())?;
    if raw.is_empty() || raw.len() > 2 {
        return Err(format!("'paths' must contain one or two fingers, not {}", raw.len()));
    }
    let mut paths = Vec::with_capacity(raw.len());
    for (index, path) in raw.iter().enumerate() {
        let points = path
            .as_array()
            .ok_or_else(|| format!("paths[{index}] must be an array of [x, y] points"))?;
        if !(2..=32).contains(&points.len()) {
            return Err(format!("paths[{index}] must have 2–32 points, not {}", points.len()));
        }
        let mut parsed = Vec::with_capacity(points.len());
        for (position, point) in points.iter().enumerate() {
            let pair = point
                .as_array()
                .filter(|pair| pair.len() == 2)
                .ok_or_else(|| format!("paths[{index}][{position}] must be [x, y]"))?;
            let x = pair[0]
                .as_i64()
                .and_then(|v| i32::try_from(v).ok())
                .ok_or_else(|| format!("paths[{index}][{position}][0] must be an integer"))?;
            let y = pair[1]
                .as_i64()
                .and_then(|v| i32::try_from(v).ok())
                .ok_or_else(|| format!("paths[{index}][{position}][1] must be an integer"))?;
            parsed.push((x, y));
        }
        paths.push(parsed);
    }
    Ok(paths)
}

// ── Tree quality ───────────────────────────────────────────────────────────

/// How much of this screen the accessibility tree actually describes.
///
/// `uiautomator dump` succeeds on almost anything, including screens it
/// describes nothing useful on: a game surface, a Flutter canvas, a WebView
/// exposing one unnamed container. To the model an empty list and a list of
/// anonymous boxes looked the same, so it re-read the tree instead of switching
/// to the screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeQuality {
    /// Enough named, actionable nodes to work from.
    Usable,
    /// Readable, but part of the screen is absent from it.
    Partial,
    /// Nothing the model can address.
    Unusable,
}

impl TreeQuality {
    /// Stable name for the tool result and for tests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usable => "usable",
            Self::Partial => "partial",
            Self::Unusable => "unusable",
        }
    }

    /// What to tell the model, or `None` when the tree is fine.
    pub fn advice(self) -> Option<&'static str> {
        match self {
            Self::Usable => None,
            Self::Partial => Some(
                "Part of this screen is missing from the tree (unnamed containers, custom-drawn \
                 views). Anything you cannot find here may still be visible in the screenshot — \
                 read it, or tap by coordinates.",
            ),
            Self::Unusable => Some(
                "The accessibility tree describes none of this screen — a game, a canvas, or a \
                 secure surface. Do not re-read the tree; work from the screenshot and tap by \
                 coordinates.",
            ),
        }
    }
}

/// The label a person would read for this element, if it has one.
pub fn element_label(element: &UiElement) -> Option<&str> {
    element
        .text
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            element
                .content_desc
                .as_deref()
                .filter(|s| !s.trim().is_empty())
        })
}

/// Judge a parsed tree.
///
/// The bar is deliberately low: named nodes at all, and named nodes being a
/// meaningful share of what was found. Thin trees are normal on Android; the
/// case worth flagging is a tree that describes *none* of the screen.
///
/// A node with no name but real structure (a list, a field, a toggle) counts as
/// describing something: a message list is often a scrollable container full of
/// custom-drawn rows, and calling that screen undescribable sends the model to
/// OCR when the one thing it needed — the list to scroll — was right there.
pub fn assess_tree(elements: &[UiElement]) -> TreeQuality {
    if elements.is_empty() {
        return TreeQuality::Unusable;
    }
    let described = elements
        .iter()
        .filter(|e| element_label(e).is_some() || e.state.is_structural())
        .count();
    if described == 0 {
        return TreeQuality::Unusable;
    }
    let share = described as f32 / elements.len() as f32;
    if described < 3 || share < 0.25 {
        TreeQuality::Partial
    } else {
        TreeQuality::Usable
    }
}

/// A cheap identity of what is on screen.
///
/// Used to answer "did my action change anything" without a second tool call.
/// Bounds are part of it because a list that scrolls keeps the same labels in
/// different places, and that is a change.
pub fn tree_fingerprint(elements: &[UiElement]) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    elements.len().hash(&mut hasher);
    for element in elements {
        element_label(element).unwrap_or_default().hash(&mut hasher);
        element.class.hash(&mut hasher);
        element.bounds.hash(&mut hasher);
        element.clickable.hash(&mut hasher);
    }
    hasher.finish()
}

// ── Write readback ─────────────────────────────────────────────────────────

/// The field read back after `this` much silence, or `None` to skip the check.
///
/// A short pause is enough on a local accessibility read and is a wasted second
/// on the ADB path, so it is only spent when the caller asked for verification
/// and expects to read back.
const READBACK_DELAY_MS: u64 = 250;

/// The text the screen actually holds matches what was typed.
pub const READBACK_MATCH: &str = "match";
/// It holds something else.
pub const READBACK_MISMATCH: &str = "mismatch";
/// The field could not be read back — no field was found, it is a password
/// box, or the tree could not be read at all.
pub const READBACK_UNKNOWN: &str = "unknown";

/// One field's answer after a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readback {
    pub verdict: &'static str,
    /// What the field holds now, when it could be read.
    pub actual: Option<String>,
    /// Why the verdict is unknown, when it is.
    pub note: Option<String>,
}

impl Readback {
    fn unknown(note: impl Into<String>) -> Self {
        Self {
            verdict: READBACK_UNKNOWN,
            actual: None,
            note: Some(note.into()),
        }
    }
}

/// Compare what was typed with what the field holds.
///
/// Whitespace is stripped on both sides: an auto-formatted phone number inserts
/// spaces as it goes, and that is the field formatting, not the text being
/// wrong. Case and punctuation are compared exactly — "Hello" and "hello" are
/// two different messages, and the field is where the difference matters.
pub fn readback_matches(written: &str, read: &str) -> bool {
    let norm = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    norm(written) == norm(read)
}

/// The field a write should be read back from, given the tree *before* the
/// write and the point that was typed into.
///
/// The element key is the wrong tool here: it hashes the text, so a field that
/// has just received text has a different key from the one that was typed
/// into — `relocate` would report `STALE_REF` on the very field that worked.
/// The stable handles are the resource id and the class, and position is used
/// only to choose between fields that share them (a form with six identical
/// inputs): the one whose bounds contain the write point, else the nearest.
///
/// A password field is returned too, so the caller can say *why* it is not
/// being read rather than claiming no field was there.
pub fn locate_for_readback(before: &[UiElement], point: (i32, i32)) -> Option<&UiElement> {
    let is_field = |element: &&UiElement| element.state.editable || element.state.password;
    let contains = |element: &&UiElement| {
        let (x1, y1, x2, y2) = element.bounds;
        point.0 >= x1 && point.0 <= x2 && point.1 >= y1 && point.1 <= y2
    };
    // Focus beats geometry: a field the platform says is focused is where the
    // text went, whichever field the coordinates landed in (keyboard overlay,
    // hint text, padding).
    before
        .iter()
        .filter(is_field)
        .find(|element| element.state.focused && contains(element))
        .or_else(|| before.iter().filter(is_field).find(contains))
        .or_else(|| {
            before.iter().filter(is_field).min_by_key(|element| {
                let (cx, cy) = element.center;
                (i64::from(cx) - i64::from(point.0)).pow(2)
                    + (i64::from(cy) - i64::from(point.1)).pow(2)
            })
        })
}

/// Decide the verdict from what was written, what the field reads now, and
/// what it read before the write.
///
/// A field that answers the same thing it did before is normally a mismatch —
/// the write did not take. It is `unknown` instead when the screen meanwhile
/// showed text containing what was typed (an autocomplete suggestion, a
/// counter, the text echoed in a preview): the app clearly received the input,
/// and this field just cannot confirm the content. Claiming a mismatch there
/// would send a caller round a loop of retries that write the text twice.
pub fn readback_verdict(
    written: &str,
    actual: &str,
    baseline: Option<&str>,
    reacted: bool,
) -> &'static str {
    if readback_matches(written, actual) {
        return READBACK_MATCH;
    }
    if baseline.is_some_and(|base| base == actual) && reacted {
        return READBACK_UNKNOWN;
    }
    READBACK_MISMATCH
}

/// Whether the screen after the write shows text derived from what was typed.
///
/// The evidence that an unreadable field nevertheless received the input: the
/// app echoed it somewhere — a suggestion strip, a preview, a character count.
/// Only labels that *contain* the typed text count, and only ones that were not
/// on the screen before, so a static label that happens to match is not
/// mistaken for a response.
fn screen_echoed(before: &[UiElement], after: &[UiElement], written: &str) -> bool {
    let needle: String = written.chars().filter(|c| !c.is_whitespace()).collect();
    if needle.is_empty() {
        return false;
    }
    let seen: std::collections::HashSet<&str> = before
        .iter()
        .filter_map(element_label)
        .map(str::trim)
        .collect();
    after.iter().filter_map(element_label).any(|label| {
        let normalized: String = label.chars().filter(|c| !c.is_whitespace()).collect();
        normalized.contains(&needle) && !seen.contains(label.trim())
    })
}

// ── Action effect ──────────────────────────────────────────────────────────

/// The screen was different after the action.
pub const EFFECT_CHANGED: &str = "changed";
/// The screen was the same after the action.
pub const EFFECT_UNCHANGED: &str = "unchanged";
/// The effect could not be judged — the tree could not be read.
pub const EFFECT_UNKNOWN: &str = "unknown";
/// Something moved, but the app stayed and the screen's structure did not
/// change — a tick, a highlight, a toast, a spinner.
///
/// Worth its own word: `unchanged` tells a caller the action missed, and
/// `changed` tells it the screen went somewhere. A tap that ticked a checkbox
/// is neither, and folding it into `changed` was hiding the degree.
pub const EFFECT_SMALL_CHANGE: &str = "small_change";
/// The action moved the task to a different app or a different place.
///
/// Only knowable when the foreground package was read before and after. A
/// screen that changed without the package changing is [`EFFECT_CHANGED`].
pub const EFFECT_PLACE_CHANGED: &str = "place_changed";

/// How the element list changed between two reads.
///
/// Counts rather than a yes/no: "three rows appeared" and "the whole page was
/// replaced" are different answers, and only the second means the action left
/// this screen. Elements are identified by [`element_key`] — the part that
/// survives the write that is being judged — and matched as a multiset, so two
/// identical rows are two rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TreeDiff {
    pub added: usize,
    pub removed: usize,
    /// Same key, different bounds.
    pub moved: usize,
    /// Same key, different label.
    pub edited: usize,
}

impl TreeDiff {
    /// Total number of elements that are not in the same place, or not the
    /// same, as before.
    pub fn changed(&self) -> usize {
        self.added + self.removed + self.moved + self.edited
    }

    /// One line for the result text, or `None` when nothing changed.
    pub fn describe(&self) -> Option<String> {
        if self.changed() == 0 {
            return None;
        }
        let mut parts = Vec::new();
        for (count, what) in [
            (self.added, "appeared"),
            (self.removed, "left"),
            (self.moved, "moved"),
            (self.edited, "changed text"),
        ] {
            if count > 0 {
                parts.push(format!("{count} {what}"));
            }
        }
        Some(format!("Compared with before: {}.", parts.join(", ")))
    }
}

/// Compare two reads of the screen by element identity.
///
/// Two passes, because one is not enough when several rows share a key (a list
/// of identical rows has one key, and that is the common case): first pair the
/// rows that are in exactly the same place, then treat what is left as movement
/// before calling it gone. Pairing straight by list order would report every
/// row as moved the moment one row was inserted at the top, which is the
/// difference between "a message arrived" and "the whole list is new".
pub fn tree_diff(before: &[UiElement], after: &[UiElement]) -> TreeDiff {
    use std::collections::HashMap;

    let mut diff = TreeDiff::default();
    let mut unclaimed: HashMap<&str, Vec<&UiElement>> = HashMap::new();
    for element in before {
        if !element.key.is_empty() {
            unclaimed.entry(&element.key).or_default().push(element);
        }
    }

    // Pass one: same key, same place.
    let mut leftover_after: Vec<&UiElement> = Vec::new();
    for element in after {
        let previous = unclaimed.get_mut(element.key.as_str()).and_then(|list| {
            let position = list
                .iter()
                .position(|other| other.bounds == element.bounds)?;
            Some(list.swap_remove(position))
        });
        match previous {
            Some(previous) => {
                if previous.text != element.text || previous.content_desc != element.content_desc {
                    diff.edited += 1;
                }
            }
            None => leftover_after.push(element),
        }
    }

    // Pass two: same key, somewhere else. Two rows with the same label but no
    // shared place are the same row that moved.
    for element in leftover_after {
        let pool = unclaimed.get_mut(element.key.as_str());
        match pool.and_then(|list| (!list.is_empty()).then(|| list.swap_remove(0))) {
            Some(previous) => {
                diff.moved += 1;
                if previous.text != element.text || previous.content_desc != element.content_desc {
                    diff.edited += 1;
                }
            }
            None => diff.added += 1,
        }
    }

    diff.removed = unclaimed.values().map(Vec::len).sum();
    diff
}

/// How many changed elements make a screen "changed" rather than "small".
///
/// Proportional with a floor: three changed elements is a real change on a
/// short screen, and ten percent of a long one. Below either, the screen moved
/// but did not go anywhere.
fn changed_threshold(total: usize) -> usize {
    (total * 10).div_ceil(100).max(3)
}

/// The effect verdict, from what was observed after the action.
///
/// Ordered so the most specific answer wins: a new app is a place change even
/// if the tree also changed; a canvas that moved under a motionless tree is a
/// small change; and a tree that could not be read twice is unknown, not
/// unchanged.
pub fn classify_effect(
    before_package: Option<&str>,
    after_package: Option<&str>,
    diff: Option<&TreeDiff>,
    shown: usize,
    pixels_changed: Option<bool>,
    before_fingerprint: Option<u64>,
    after_fingerprint: Option<u64>,
) -> &'static str {
    if matches!((before_package, after_package), (Some(before), Some(after)) if before != after) {
        return EFFECT_PLACE_CHANGED;
    }
    let Some(diff) = diff else {
        return EFFECT_UNKNOWN;
    };
    let same_tree = before_fingerprint.is_some() && before_fingerprint == after_fingerprint;
    if same_tree {
        return if pixels_changed == Some(true) {
            EFFECT_SMALL_CHANGE
        } else {
            EFFECT_UNCHANGED
        };
    }
    if before_fingerprint.is_none() {
        return EFFECT_UNKNOWN;
    }
    if diff.changed() >= changed_threshold(shown) {
        EFFECT_CHANGED
    } else {
        EFFECT_SMALL_CHANGE
    }
}

/// How often the screen is polled while waiting for it to settle.
const SETTLE_POLL_MS: u64 = 150;
/// A screen that keeps this value for one poll is considered settled, rather
/// than merely "nothing has happened yet".
const SETTLE_MIN_MS: u64 = 250;
/// Default budget: long enough for a screen transition, short enough that a
/// static screen costs one extra read rather than a second.
pub const SETTLE_DEFAULT_MS: u64 = 1200;
/// Upper bound on what a caller may ask to wait.
pub const SETTLE_MAX_MS: u64 = 4000;

/// What the screen looked like after an action.
#[derive(Debug)]
pub struct Settle {
    /// One of the `EFFECT_*` constants.
    pub effect: &'static str,
    pub elapsed_ms: u64,
    pub elements: Vec<UiElement>,
    /// Why the effect is unknown, when it is.
    pub note: Option<String>,
    /// Whether the picture changed, when it was compared. `None` when the
    /// caller did not supply a before-picture or the display produced none.
    pub pixel_changed: Option<bool>,
    /// How the tree moved, when both reads could be compared.
    pub diff: Option<TreeDiff>,
    /// The app on screen when the screen settled.
    pub package: Option<String>,
    /// Who said the screen changed, when the event stream answered first:
    /// `events` (a content event, then silence) or `tree` (the poll). `None`
    /// when no stream was available and nothing was claimed about it.
    pub first_signal: Option<&'static str>,
}

impl Settle {
    /// A one-line verdict for the tool result.
    pub fn verdict(&self) -> String {
        let structure = self
            .diff
            .as_ref()
            .and_then(TreeDiff::describe)
            .map(|line| format!(" {line}"))
            .unwrap_or_default();
        match self.effect {
            EFFECT_CHANGED => format!(
                "The screen changed (settled in {} ms, {} elements now).{structure}",
                self.elapsed_ms,
                self.elements.len()
            ),
            EFFECT_PLACE_CHANGED => format!(
                "The action opened something else — the foreground app changed (settled in {} ms, \
                 {} elements now).{structure}",
                self.elapsed_ms,
                self.elements.len()
            ),
            EFFECT_SMALL_CHANGE => format!(
                "Something changed, but the screen is still the same page (settled in {} ms).\
                 {structure} The action landed; the screen did not go anywhere.",
                self.elapsed_ms
            ),
            EFFECT_UNCHANGED => format!(
                "The screen looks unchanged after {} ms. The action may have missed, or this \
                 screen changes without changing what the tree exposes (a canvas, or a scroll \
                 that keeps the same rows). Check the screenshot before repeating it.",
                self.elapsed_ms
            ),
            _ => format!(
                "Effect unknown: the screen could not be re-read{}. Read it with android_observe \
                 to see what happened.",
                self.note
                    .as_deref()
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default()
            ),
        }
    }
}

/// Watch the screen until it stops changing, or the budget runs out.
///
/// `before` is the fingerprint taken before the action, so "changed" is a fact
/// about the screen rather than an assumption from the command having been
/// accepted. Two consecutive equal readings mean the screen has settled.
pub async fn settle_after_action(
    device: &Option<String>,
    target: Target,
    before: Option<u64>,
    budget_ms: u64,
) -> Settle {
    settle_after_action_from(
        device,
        target,
        before.map(|fingerprint| (fingerprint, Vec::new())),
        budget_ms,
    )
    .await
}

/// The same wait, started from the element list rather than only its
/// fingerprint.
///
/// The list is what makes a *degree* of change possible: a fingerprint says
/// "different", the element diff says "two rows appeared". Callers that only
/// have the fingerprint pass an empty list and get the old three-way answer
/// with an empty diff.
pub async fn settle_after_action_from(
    device: &Option<String>,
    target: Target,
    before: Option<(u64, Vec<UiElement>)>,
    budget_ms: u64,
) -> Settle {
    settle_after_action_and_events_from(device, target, before, settle_events(target), budget_ms)
        .await
}

/// [`settle_after_action_from`] with the event stream passed in.
///
/// The caller must have subscribed *before* the action: events are pushed, so
/// what the action changed is already in the channel when this is called, and
/// an event that arrived while nobody was subscribed was dropped by the
/// channel. Split out so a test can supply its own stream.
pub async fn settle_after_action_and_events_from(
    device: &Option<String>,
    target: Target,
    before: Option<(u64, Vec<UiElement>)>,
    events: Option<tokio::sync::broadcast::Receiver<crate::device::DeviceEvent>>,
    budget_ms: u64,
) -> Settle {
    let started = std::time::Instant::now();
    let budget = budget_ms.clamp(SETTLE_MIN_MS, SETTLE_MAX_MS);
    let before_fingerprint = before.as_ref().map(|(fingerprint, _)| *fingerprint);
    let before_elements = before.as_ref().map(|(_, elements)| elements.as_slice());
    let mut previous: Option<u64> = None;
    let mut last: Option<Vec<UiElement>> = None;
    let mut after_package: Option<String> = None;
    let mut failure: Option<String> = None;

    // The phone's own account of what changed, when it can be had: a content
    // event starts the wait immediately, and the quiet that follows it says
    // the screen has stopped moving — both without reading the tree. The tree
    // is then read *once*, to say what the screen is now, instead of twice to
    // prove it stopped changing. The subscription is taken by the caller
    // *before* the action, because an event posted between the action and this
    // call has already been delivered (or dropped if nobody was listening).
    //
    // Silence alone proves nothing: a stream that is not delivering content
    // events is silent on a screen that moved, too. So the stream is only
    // allowed to end the wait once it has *spoken* — `announced_at` stays
    // `None` otherwise, and the two-identical-reads rule answers as before.
    let mut first_signal: Option<&'static str> = None;
    let mut read_taken = false;
    let mut events = events;
    let mut announced_at: Option<std::time::Instant> = events.as_mut().and_then(|receiver| {
        // What the action caused may already be queued.
        let mut last = None;
        loop {
            match receiver.try_recv() {
                Ok(event) => {
                    if settle_event_matters(&event, None) {
                        last = Some(started);
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => last = Some(started),
                Err(_) => break,
            }
        }
        last
    });

    loop {
        // With a stream, a content change is the first signal: the screen
        // announced it, so the tree does not have to be polled to discover it.
        // Quiet for `SETTLE_CONTENT_QUIET_MS` after that announcement is the
        // second signal — the screen stopped changing, which is what the
        // polling loop waits for two identical reads to conclude.
        if let Some(receiver) = events.as_mut() {
            loop {
                match receiver.try_recv() {
                    Ok(event) => {
                        if settle_event_matters(&event, after_package.as_deref()) {
                            announced_at = Some(std::time::Instant::now());
                        }
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                        announced_at = Some(std::time::Instant::now());
                    }
                    // Nothing queued right now: the stream is live.
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    // The phone side went away; polling is all that is left.
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        events = None;
                        break;
                    }
                }
            }
        }
        // The stream has spoken, the screen has been read once *since* it
        // spoke, and nothing new has been said: the screen stopped moving.
        // One read already says what it stopped on — no second read to prove
        // it is still the same.
        //
        // The first pass is exempt: an action can leave the channel silent for
        // a moment before the phone starts drawing, and ending the wait on
        // that would be the old "unchanged" in a new dress.
        let quiet = read_taken
            && announced_at.is_some_and(|at| {
                at.elapsed() >= std::time::Duration::from_millis(SETTLE_CONTENT_QUIET_MS)
            });

        match dump_ui(device, target).await {
            Ok(dump) => {
                let fingerprint = tree_fingerprint(&dump.elements);
                let stable = previous == Some(fingerprint);
                previous = Some(fingerprint);
                after_package = dump.package;
                last = Some(dump.elements);
                if quiet {
                    first_signal = Some("events");
                    break;
                }
                if stable && started.elapsed().as_millis() as u64 >= SETTLE_MIN_MS {
                    first_signal = Some("tree");
                    break;
                }
                read_taken = true;
            }
            Err(failed) => {
                failure = Some(failed.code.to_string());
                break;
            }
        }
        if started.elapsed().as_millis() as u64 >= budget {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(SETTLE_POLL_MS)).await;
    }

    let elapsed_ms = started.elapsed().as_millis() as u64;
    match last {
        Some(elements) => {
            let after_fingerprint = tree_fingerprint(&elements);
            let diff = before_elements
                .filter(|before| !before.is_empty())
                .map(|before| tree_diff(before, &elements));
            let effect = classify_effect(
                None,
                after_package.as_deref(),
                diff.as_ref(),
                elements.len(),
                None,
                before_fingerprint,
                Some(after_fingerprint),
            );
            Settle {
                effect,
                elapsed_ms,
                elements,
                note: None,
                pixel_changed: None,
                diff: diff.filter(|diff| diff.changed() > 0),
                package: after_package,
                first_signal,
            }
        }
        None => Settle {
            effect: EFFECT_UNKNOWN,
            elapsed_ms,
            elements: Vec::new(),
            note: failure,
            pixel_changed: None,
            diff: None,
            package: None,
            first_signal,
        },
    }
}

/// How long the screen must stay quiet after a content event before it counts
/// as settled.
///
/// Cancels the animation burst: a list that slides in raises several content
/// events over a few hundred milliseconds, and only the last one is followed by
/// silence. Shorter than the polling rule's quarter second because the event
/// already proved that something happened — this only waits for it to stop.
const SETTLE_CONTENT_QUIET_MS: u64 = 400;

/// The event stream a settle may consult, if this device has a usable one.
///
/// Only the user's own display raises these events; a virtual display is
/// invisible to the accessibility service that reports them, so a settle there
/// keeps polling. `None` means "no stream": the poll is then the only signal.
pub fn settle_events(
    target: Target,
) -> Option<tokio::sync::broadcast::Receiver<crate::device::DeviceEvent>> {
    match target {
        Target::Screen => Some(crate::device::bridge()?.events()?),
        Target::Display(_) => None,
    }
}

/// Whether an event says the screen the action was on moved.
///
/// A window change is a different place and is left to the caller's own
/// package check; a content change is the screen redrawing. Notifications from
/// other apps appear without the screen changing, so they are not evidence
/// either way.
fn settle_event_matters(event: &crate::device::DeviceEvent, package: Option<&str>) -> bool {
    match event.kind {
        crate::device::EventKind::Content => {
            package.is_none_or(|package| event.package.is_empty() || event.package == package)
        }
        crate::device::EventKind::Window => false,
        crate::device::EventKind::Notification => false,
    }
}

/// The same wait, with the picture as a second signal.
///
/// A tree fingerprint says nothing about a game, a map, a camera preview, or a
/// canvas: those screens change without changing what the tree exposes, and
/// every action on them reads as "unchanged". The brightness grid is what sees
/// it.
///
/// `before_pixels` is the grid taken before the action. When either side is
/// missing the tree decides alone, exactly as [`settle_after_action`] does.
pub async fn settle_after_action_with_pixels(
    device: &Option<String>,
    target: Target,
    before: Option<u64>,
    before_pixels: Option<&super::display::FrameSignature>,
    budget_ms: u64,
) -> Settle {
    settle_after_action_with_pixels_from(
        device,
        target,
        before.map(|fingerprint| (fingerprint, Vec::new())),
        before_pixels,
        budget_ms,
    )
    .await
}

/// [`settle_after_action_with_pixels`] with the element list before the
/// action, so the change can be measured as well as noted.
pub async fn settle_after_action_with_pixels_from(
    device: &Option<String>,
    target: Target,
    before: Option<(u64, Vec<UiElement>)>,
    before_pixels: Option<&super::display::FrameSignature>,
    budget_ms: u64,
) -> Settle {
    settle_after_action_with_pixels_and_events_from(
        device,
        target,
        before,
        before_pixels,
        settle_events(target),
        budget_ms,
    )
    .await
}

/// [`settle_after_action_with_pixels_from`] with the event stream passed in,
/// so an action can subscribe before it fires (see
/// [`settle_after_action_and_events_from`]).
pub async fn settle_after_action_with_pixels_and_events_from(
    device: &Option<String>,
    target: Target,
    before: Option<(u64, Vec<UiElement>)>,
    before_pixels: Option<&super::display::FrameSignature>,
    events: Option<tokio::sync::broadcast::Receiver<crate::device::DeviceEvent>>,
    budget_ms: u64,
) -> Settle {
    let mut settled =
        settle_after_action_and_events_from(device, target, before, events, budget_ms).await;

    let Some(before_pixels) = before_pixels else {
        return settled;
    };
    if settled.effect == EFFECT_CHANGED {
        return settled;
    }

    // One read of the picture, after the tree has settled. Polling it would
    // cost a frame per poll, and the frame is only consulted to overturn an
    // "unchanged" the tree could not have seen.
    let Some(after) = display_frame(target).await else {
        return settled;
    };
    let pixels_changed = before_pixels.differs(&after);
    settled.pixel_changed = Some(pixels_changed);
    if pixels_changed && settled.effect == EFFECT_UNCHANGED {
        settled.effect = EFFECT_SMALL_CHANGE;
    }
    settled
}

/// The current picture of whatever screen the action targeted.
///
/// Two producers, because the two screens have different owners: a virtual
/// display's frames come from the helper, and the user's own screen is the
/// device's — the helper never mirrors display 0, so `screencap` is the only
/// source there. Both produce the same grid, which is what makes the two
/// comparable.
async fn display_frame(target: Target) -> Option<super::display::FrameSignature> {
    if let Target::Display(id) = target {
        let bridge = display_bridge().ok()?;
        let display = super::display::VirtualDisplay::attach(bridge, id);
        // `Ok(None)` means the display has not drawn since it was created,
        // which is its own answer and not something to guess about.
        return display.frame_signature().await.ok().flatten();
    }
    user_screen_signature(&None).await
}

/// [`format_ui_summary`] with a ceiling, for results that are a side effect of
/// another tool rather than the thing the model asked for.
fn format_ui_summary_limited(elements: &[UiElement], limit: usize) -> String {
    if elements.len() <= limit {
        return format_ui_summary(elements);
    }
    let mut text = format_ui_summary(&elements[..limit]);
    text.push_str(&format!(
        "\n… and {} more element(s); call android_ui_tree for the full list.",
        elements.len() - limit
    ));
    text
}

// ── OCR fallback ───────────────────────────────────────────────────────────

/// A line of text read from the screenshot, for screens the accessibility tree
/// cannot describe.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OcrLine {
    pub text: String,
    /// Centre in device pixels — the same space as [`UiElement::center`], so a
    /// line can be tapped by passing these coordinates.
    pub center: (i32, i32),
    /// `(x1, y1, x2, y2)` in device pixels.
    pub bounds: (i32, i32, i32, i32),
    pub confidence: f32,
}

/// Turn OCR blocks into the tap-addressable shape above.
#[cfg(feature = "vision")]
fn ocr_lines(blocks: Vec<crate::computer::vision::TextBlock>) -> Vec<OcrLine> {
    blocks
        .into_iter()
        .map(|block| {
            let rect = block.bounds;
            let x1 = rect.x;
            let y1 = rect.y;
            let x2 = rect.x + rect.width as i32;
            let y2 = rect.y + rect.height as i32;
            OcrLine {
                text: block.text,
                center: ((x1 + x2) / 2, (y1 + y2) / 2),
                bounds: (x1, y1, x2, y2),
                confidence: block.confidence,
            }
        })
        .collect()
}

/// Read text out of an already-captured screenshot with the ONNX OCR model.
///
/// The engine is loaded once and kept: constructing it reads two model files,
/// and the models download on first use.
#[cfg(feature = "vision")]
pub async fn ocr_png(png: &[u8]) -> Result<Vec<OcrLine>, String> {
    use crate::computer::types::Screenshot;
    use crate::computer::vision::ocr_rapid::RapidOcr;
    use base64::Engine as _;

    static ENGINE: tokio::sync::Mutex<Option<RapidOcr>> = tokio::sync::Mutex::const_new(None);

    let (width, height) =
        png_dimensions(png).ok_or_else(|| "the screenshot's size could not be read".to_string())?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    let screenshot = Screenshot::new(encoded, width, height);

    let mut guard = ENGINE.lock().await;
    if guard.is_none() {
        let engine = RapidOcr::new_auto()
            .await
            .map_err(|e| format!("the OCR models could not be loaded ({e})"))?;
        *guard = Some(engine);
    }
    let engine = guard
        .as_mut()
        .ok_or_else(|| "the OCR engine is unavailable".to_string())?;
    let blocks = engine
        .detect_text(&screenshot)
        .await
        .map_err(|e| format!("OCR failed ({e})"))?;
    Ok(ocr_lines(blocks))
}

/// Read text out of a screenshot in a build without an ONNX engine.
///
/// On the phone itself there is a perfectly good recogniser in the platform,
/// so this is only "no OCR" when *neither* path exists: the mobile feature
/// profile drops `vision` (no ORT, no `image` crate), and the phone build
/// answers through its own ML Kit bridge instead. A desktop build without
/// `vision` installs no bridge, so it still fails here — explicitly, rather
/// than returning an empty list, which would read like "the screen is blank"
/// and send the model looking for a problem that is not there.
#[cfg(not(feature = "vision"))]
pub async fn ocr_png(png: &[u8]) -> Result<Vec<OcrLine>, String> {
    let Some(bridge) = crate::device::bridge() else {
        return Err("this build has no OCR engine (compiled without the `vision` feature, and no \
             on-device bridge)"
            .to_string());
    };
    use base64::Engine as _;
    let payload = serde_json::json!({
        "pngBase64": base64::engine::general_purpose::STANDARD.encode(png),
    });
    // The bridge round trip is a JNI hop into a blocking recogniser. It has no
    // timeout of its own, and a stuck model would otherwise freeze the whole
    // read for the rest of the turn.
    let answer = match tokio::time::timeout(
        OCR_BRIDGE_TIMEOUT,
        bridge.call(crate::device::CMD_OCR, payload),
    )
    .await
    {
        Err(_) => {
            return Err(format!(
                "{}: the on-device OCR did not answer within {} s",
                OCR_UNAVAILABLE,
                OCR_BRIDGE_TIMEOUT.as_secs()
            ))
        }
        Ok(Err(error)) => return Err(format!("{OCR_UNAVAILABLE}: {error}")),
        Ok(Ok(answer)) => answer,
    };
    parse_ocr_answer(&answer)
}

/// How long the on-device OCR may take before the read is abandoned.
///
/// A full-screen recognition takes a few hundred milliseconds on a mid-range
/// phone; anything near five seconds means the model is wedged, and waiting
/// longer only makes the turn slower to fail.
#[cfg_attr(feature = "vision", allow(dead_code))]
const OCR_BRIDGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The error code the bridge uses for "the engine could not run".
#[cfg_attr(feature = "vision", allow(dead_code))]
const OCR_UNAVAILABLE: &str = "ocr_unavailable";

/// Parse the bridge's answer into lines, refusing the "no engine" sentinel.
///
/// `ok: false` carries `code: "ocr_unavailable"`; turning that into an empty
/// list would tell the model the screen is blank, which is a different (and
/// wrong) statement. A missing `lines` array is a malformed payload and fails
/// the same way despite `ok: true`.
///
/// Lives outside the `not(vision)` gate so the default test run exercises it;
/// the phone build is the one that calls it.
#[cfg_attr(feature = "vision", allow(dead_code))]
fn parse_ocr_answer(answer: &serde_json::Value) -> Result<Vec<OcrLine>, String> {
    if !answer.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        let code = answer
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or(OCR_UNAVAILABLE);
        let detail = answer
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the on-device OCR engine could not run");
        return Err(format!("{code}: {detail}"));
    }
    let lines = answer
        .get("lines")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{OCR_UNAVAILABLE}: the OCR answer carried no line list"))?;
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let Some(text) = line.get("text").and_then(Value::as_str) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let Some(bounds) = line.get("bounds").and_then(Value::as_array) else {
            continue;
        };
        if bounds.len() != 4 {
            continue;
        }
        let mut rect = [0i32; 4];
        let mut readable = true;
        for (slot, value) in rect.iter_mut().zip(bounds) {
            match value.as_i64().and_then(|v| i32::try_from(v).ok()) {
                Some(v) => *slot = v,
                None => {
                    readable = false;
                    break;
                }
            }
        }
        if !readable {
            continue;
        }
        let (x1, y1, x2, y2) = (rect[0], rect[1], rect[2], rect[3]);
        if x2 <= x1 || y2 <= y1 {
            // A zero-area box cannot be tapped, so it is not a usable line.
            continue;
        }
        out.push(OcrLine {
            text: text.to_string(),
            center: ((x1 + x2) / 2, (y1 + y2) / 2),
            bounds: (x1, y1, x2, y2),
            confidence: line
                .get("confidence")
                .and_then(Value::as_f64)
                .map(|c| c as f32)
                .unwrap_or(0.0),
        });
    }
    Ok(out)
}

/// Read text out of whatever `target` points at.
pub async fn ocr_screen(device: &Option<String>, target: Target) -> Result<Vec<OcrLine>, String> {
    let png = capture_png(device, target)
        .await
        .map_err(|e| e.to_string())?;
    ocr_png(&png).await
}

/// One line per recognised text block, for the model to read.
pub fn format_ocr_lines(lines: &[OcrLine]) -> String {
    if lines.is_empty() {
        return "OCR read no text from this screen.".to_string();
    }
    let mut out = vec![format!(
        "{} text line(s) read from the screenshot:",
        lines.len()
    )];
    for line in lines {
        out.push(format!("{} (center=({},{}))", line.text, line.center.0, line.center.1));
    }
    out.join("\n")
}

/// Loose label comparison for the safety net.
///
/// The model's description is free text, so accept a case-insensitive substring
/// match in either direction. An element with no label at all cannot be checked
/// and passes — refusing there would block icon buttons the model identified
/// from the screenshot.
pub(super) fn label_matches(label: &str, description: &str) -> bool {
    let label = label.trim();
    if label.is_empty() {
        return true;
    }
    let description = description.trim();
    if description.is_empty() {
        return false;
    }
    let (l, d) = (label.to_lowercase(), description.to_lowercase());
    l.contains(&d) || d.contains(&l)
}

/// Resolve an element index against a freshly-read tree, without any I/O.
///
/// This is the safety net: the index came from an earlier `android_ui_tree`
/// result, and the screen may have changed since. Tapping the *coordinates* of a
/// stale read silently hits whatever has since moved into place, so an indexed
/// tap re-reads and validates first, and refuses with a reason the model can act
/// on rather than clicking blind.
pub(super) fn validate_target<'a>(
    elements: &'a [UiElement],
    index: usize,
    description: Option<&str>,
) -> Result<&'a UiElement, String> {
    if index == 0 {
        return Err("Element indices are 1-based; 0 does not exist.".to_string());
    }
    let element = elements.get(index - 1).ok_or_else(|| {
        format!(
            "No element {index} on screen now — the UI has {} element(s). Re-read it with \
             android_ui_tree.",
            elements.len()
        )
    })?;

    if let Some(description) = description {
        let label = element
            .text
            .as_deref()
            .or(element.content_desc.as_deref())
            .unwrap_or("");
        if !label_matches(label, description) {
            return Err(format!(
                "Refused: element {index} is {label:?}, not {description:?} — the screen changed. \
                 Re-read it with android_ui_tree rather than tapping stale coordinates."
            ));
        }
    }
    check_tappable(element, index)
}

/// The part of a target check that does not depend on how it was named.
fn check_tappable(element: &UiElement, index: usize) -> Result<&UiElement, String> {
    if !element.clickable {
        return Err(format!("Refused: element {index} ({}) is not clickable.", element.class));
    }
    if !element.enabled {
        return Err(format!("Refused: element {index} is disabled."));
    }
    Ok(element)
}

/// Dump and parse the device's current UI tree.
///
/// Shared by `android_ui_tree` and the safety net in `android_input` so both see
/// the same enumeration.
/// A parsed `uiautomator dump`: the actionable elements plus the screen
/// rectangle the dump describes.
pub struct UiDump {
    pub elements: Vec<UiElement>,
    /// Full-display size as the dump reports it (`None` if it had no root bounds).
    pub screen: Option<(i32, i32)>,
    /// Where the tree came from: `accessibility` (in-process) or `uiautomator`
    /// (ADB). Reported so a surprising read can be traced to its source.
    pub source: &'static str,
    /// The app that owns the focused window, when the platform said.
    ///
    /// It is what the outward pacing is bucketed by: two sends to the same app
    /// belong to the same history whether or not the screen changed.
    pub package: Option<String>,
    /// The name of *the screen this read saw*, quoted back by a later call.
    ///
    /// It is the tree's fingerprint in hex, so two reads of an unchanged screen
    /// carry the same name — which is what makes it usable as the `since`
    /// baseline of an incremental read ([`super::diff`]). It is not a version
    /// of the app, and it is not what an *index* refers to: indices belong to
    /// one enumeration, so a caller holding them is working from this read
    /// whether or not the screen has changed since. An element *key* is what
    /// survives the screen changing.
    pub snapshot: String,
    /// The source capped the node list, so this tree is a prefix of the screen.
    ///
    /// Only the in-process accessibility walk caps (`A11yBridge.MAX_NODES`);
    /// `uiautomator dump` writes whatever it saw. A caller looking for an
    /// element in a truncated tree has not seen the whole screen, and a caller
    /// matching more than the cap cannot count the matches.
    pub truncated: bool,
}

/// Name a reading of the tree.
///
/// The name *is* the fingerprint, in hex: two reads with the same elements get
/// the same name, which is what makes a name usable as the baseline for
/// [`super::diff`] — a later read can ask "what changed since this screen".
/// A read counter was the obvious alternative and is wrong: it would name the
/// same screen differently every time, so a caller diffing against it would
/// always be comparing with a baseline that cannot be found.
///
/// It is still not a version of the app, and still not what an *index* refers
/// to: indices belong to one enumeration. `key` is what survives a change.
fn snapshot_name(fingerprint: u64) -> String {
    format!("{fingerprint:x}")
}

pub(super) async fn dump_ui_elements(
    device: &Option<String>,
    target: Target,
) -> Result<Vec<UiElement>, DumpFailure> {
    Ok(dump_ui(device, target).await?.elements)
}

/// Dump, pull and parse the tree in one go.
///
/// The in-process accessibility tree is preferred whenever it is available: one
/// bridge call instead of a subprocess plus an `/sdcard` round trip, and the
/// only source that can see a display other than the default one. It exists
/// only on the phone itself and only while Syscity's accessibility service is
/// on, so a desktop driving a remote device — and any phone with the service
/// off — falls through to ADB exactly as before.
pub(super) async fn dump_ui(
    device: &Option<String>,
    target: Target,
) -> Result<UiDump, DumpFailure> {
    if let Target::Display(id) = target {
        // A virtual display is reachable only through the accessibility
        // service, which is the only reader that can be pointed at it. There is
        // no `uiautomator` fallback: that dumps the default display, which
        // would silently describe the wrong screen.
        let bridge = display_bridge()
            .map_err(|message| DumpFailure::new(DISPLAY_TREE_UNAVAILABLE, message))?;
        return match super::a11y::fetch(&bridge, Some(id)).await {
            Ok(snapshot) => {
                let fingerprint = tree_fingerprint(&snapshot.elements);
                crate::computer::platform::mobile::diff::remember(
                    device.as_deref(),
                    target.display_id(),
                    fingerprint,
                    snapshot.package.as_deref(),
                    &snapshot.elements,
                );
                Ok(UiDump {
                    elements: snapshot.elements,
                    screen: snapshot.display,
                    source: "accessibility",
                    package: snapshot.package,
                    snapshot: snapshot_name(fingerprint),
                    truncated: snapshot.truncated,
                })
            }
            Err(unavailable) => Err(DumpFailure::new(
                DISPLAY_TREE_UNAVAILABLE,
                format!(
                    "Could not read display {id}: {}. Work from the frame instead —                      android_display action 'frame' returns one.",
                    unavailable.message
                ),
            )),
        };
    }

    if device.is_none() {
        if let Some(bridge) = crate::device::bridge() {
            match super::a11y::fetch(bridge, None).await {
                Ok(snapshot) if snapshot.is_addressable() => {
                    let fingerprint = tree_fingerprint(&snapshot.elements);
                    crate::computer::platform::mobile::diff::remember(
                        device.as_deref(),
                        target.display_id(),
                        fingerprint,
                        snapshot.package.as_deref(),
                        &snapshot.elements,
                    );
                    return Ok(UiDump {
                        elements: snapshot.elements,
                        screen: snapshot.display,
                        source: "accessibility",
                        package: snapshot.package,
                        snapshot: snapshot_name(fingerprint),
                        truncated: snapshot.truncated,
                    });
                }
                Ok(_) => {
                    tracing::debug!(
                        "accessibility tree had no addressable nodes; falling back to uiautomator"
                    );
                }
                Err(unavailable) => {
                    tracing::debug!(
                        code = unavailable.code,
                        "accessibility tree unavailable: {}",
                        unavailable.message
                    );
                }
            }
        }
    }

    let xml = pull_ui_xml(device).await?;
    let elements = parse_uiautomator_xml(&xml).map_err(|source| {
        DumpFailure::new(
            DUMP_FAILED,
            format!("Could not read the UI tree: the dump did not parse ({source})"),
        )
    })?;
    let fingerprint = tree_fingerprint(&elements);
    let package = parse_package(&xml);
    crate::computer::platform::mobile::diff::remember(
        device.as_deref(),
        target.display_id(),
        fingerprint,
        package.as_deref(),
        &elements,
    );
    Ok(UiDump {
        elements,
        screen: parse_screen_bounds(&xml),
        source: "uiautomator",
        package,
        snapshot: snapshot_name(fingerprint),
        // The dump writes what the window held; it has no cap of its own.
        truncated: false,
    })
}

/// Run `uiautomator dump` on the device and pull the file back.
async fn pull_ui_xml(device: &Option<String>) -> Result<String, DumpFailure> {
    let mut prefix: Vec<String> = Vec::new();
    if let Some(serial) = device {
        prefix.push("-s".to_string());
        prefix.push(serial.clone());
    }
    let argv = |base: &[&str]| -> Vec<String> {
        let mut v = prefix.clone();
        v.extend(base.iter().map(|s| s.to_string()));
        v
    };
    let run = |args: Vec<String>| {
        let args: Vec<String> = args;
        async move { run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>()).await }
    };

    // `uiautomator dump` writes to a device file, which then has to be pulled.
    let dump = argv(&["shell", "uiautomator", "dump", "/sdcard/window_dump.xml"]);
    let (status, _, stderr) = run(dump).await.map_err(|e| {
        DumpFailure::new(DUMP_FAILED, format!("Could not run adb to read the UI tree: {e}"))
    })?;
    if !status.success() {
        return Err(DumpFailure::classify(&stderr));
    }

    let pull = argv(&["pull", "/sdcard/window_dump.xml", "-"]);
    let (status, stdout, stderr) = run(pull).await.map_err(|e| {
        DumpFailure::new(DUMP_FAILED, format!("Could not read the UI tree: adb pull failed ({e})"))
    })?;
    if !status.success() {
        // A failed pull reports its own reason on stderr ("no such file", a
        // device that went away between the two commands), so it goes through
        // the same classifier rather than a second vocabulary.
        return Err(DumpFailure::classify(&stderr));
    }

    Ok(stdout)
}

#[async_trait]
impl Tool for AdbUiTreeTool {
    fn name(&self) -> &str {
        "android_ui_tree"
    }

    fn description(&self) -> &str {
        "Read the Android device's UI: a numbered list of the elements you can act on, each with \
         its text or content description, whether it is clickable, and the screen coordinates of \
         its centre. Tap an element with android_input by passing that centre as x/y. Bare layout \
         containers are omitted. The result says how much of the screen the tree actually \
         describes (quality: usable / partial / unusable); when it is not usable this reads the \
         screenshot with OCR instead and returns those lines, so do not re-read hoping for more — \
         act on the OCR text or the coordinates it gives. Pass `since` with the snapshot of an \
         earlier read to get only what changed — the rows that appeared, left or moved — instead \
         of the whole screen."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Get Android UI tree",
            serde_json::json!({
                "since": { "type": "string", "description": SINCE_PARAM },
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        // The raw dump is an order of magnitude larger than what the model can
        // use, and the model would have to parse `bounds` itself to act on it.
        // Hand back an indexed list instead; the XML stays out of the context.
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let since = super::diff::since_arg(&args).map(str::to_string);
        let dump = match dump_ui(&device, target).await {
            Ok(dump) => dump,
            Err(failure) => {
                return Ok(ToolExecutionResult::error(failure.message)
                    .with_data(serde_json::json!({ "code": failure.code })))
            }
        };
        if let Some(answer) = since_answer(device.as_deref(), target, since.as_deref(), &dump) {
            let (text, data) = match answer {
                SinceAnswer::Delta { text, data } => (text, data),
                SinceAnswer::Full(reason) => {
                    (format!("{} {}", reason.describe(), format_ui_summary(&dump.elements)), {
                        let mut data = since_fallback(reason);
                        data["elements"] = serde_json::json!(dump.elements);
                        data["snapshot"] = serde_json::json!(dump.snapshot);
                        data
                    })
                }
            };
            return Ok(ToolExecutionResult::success(text).with_data(data));
        }
        let elements = dump.elements;
        let quality = assess_tree(&elements);
        let mut summary = format_ui_summary(&elements);
        let mut ocr_text = None;
        // When the tree describes little or nothing, reading the screenshot is
        // the only way to say what is actually on the screen — and saying it
        // here is what stops the model from re-reading the tree forever.
        if quality != TreeQuality::Usable {
            match ocr_screen(&device, target).await {
                Ok(lines) => {
                    summary.push_str("\n\n");
                    summary.push_str(&format_ocr_lines(&lines));
                    ocr_text = Some(lines);
                }
                Err(why) => {
                    if let Some(advice) = quality.advice() {
                        summary.push_str(&format!("\n\n{advice} (OCR unavailable: {why})"));
                    }
                }
            }
        }
        Ok(ToolExecutionResult::success(summary).with_data(serde_json::json!({
            "count": elements.len(),
            "quality": quality.as_str(),
            "advice": quality.advice(),
            "ocr": ocr_text,
            "source": dump.source,
            "snapshot": dump.snapshot,
            "elements": elements,
            "device": resolved_serial(&device).await,
        })))
    }
}

// ── ADB Verify Tool ────────────────────────────────────────────────────────

/// The three answers a verification can give.
///
/// `unknown` never means success: it means the check could not be made, and the
/// result says why.
pub const VERDICT_SATISFIED: &str = "satisfied";
pub const VERDICT_UNSATISFIED: &str = "unsatisfied";
pub const VERDICT_UNKNOWN: &str = "unknown";

/// What a verification is looking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expectation {
    /// Some element's text or content description contains this.
    TextContains(String),
    /// The element at this index is still there, still labelled this way.
    ElementAt {
        index: usize,
        description: Option<String>,
    },
}

impl Expectation {
    /// What was checked, in the caller's terms, for the result to report.
    fn describe(&self) -> String {
        match self {
            Expectation::TextContains(text) => format!("the screen shows text containing {text:?}"),
            Expectation::ElementAt {
                index,
                description: Some(label),
            } => format!("element {index} is still {label:?}"),
            Expectation::ElementAt { index, description: None } => {
                format!("element {index} is still there")
            }
        }
    }

    fn parse(args: &Value) -> Result<Self, String> {
        if let Some(text) = args.get("text").and_then(Value::as_str) {
            let text = text.trim();
            if text.is_empty() {
                return Err("`text` was given but is empty".to_string());
            }
            return Ok(Expectation::TextContains(text.to_string()));
        }
        if let Some(index) = args.get("target").and_then(Value::as_u64) {
            return Ok(Expectation::ElementAt {
                index: index as usize,
                description: args
                    .get("target_description")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|label| !label.is_empty())
                    .map(str::to_string),
            });
        }
        Err(
            "give one of `text` (something the screen should now show) or `target` (an element \
             index from android_ui_tree that should still be there)"
                .to_string(),
        )
    }
}

/// Decide the verdict from the elements a dump produced.
///
/// The evidence is the accessibility tree and nothing else — not a screenshot
/// comparison, and not the fact that an action was dispatched earlier. That is
/// the whole reason to ask separately.
fn evaluate(expectation: &Expectation, elements: &[UiElement]) -> &'static str {
    let satisfied = match expectation {
        Expectation::TextContains(text) => {
            let needle = text.to_lowercase();
            elements.iter().any(|element| {
                [element.text.as_deref(), element.content_desc.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|label| label.to_lowercase().contains(&needle))
            })
        }
        // Index 0 does not exist (indices are 1-based), and an index past the
        // end is an element that is gone — both are "not satisfied", not
        // "unverifiable": the tree was read and the element is not in it.
        Expectation::ElementAt { index, description } if *index > 0 => {
            match elements.get(index - 1) {
                Some(element) => description.as_deref().is_none_or(|wanted| {
                    let label = element
                        .text
                        .as_deref()
                        .or(element.content_desc.as_deref())
                        .unwrap_or_default();
                    label_matches(label, wanted)
                }),
                None => false,
            }
        }
        Expectation::ElementAt { .. } => false,
    };

    if satisfied {
        VERDICT_SATISFIED
    } else {
        VERDICT_UNSATISFIED
    }
}

/// Check that the screen shows what was expected.
#[derive(Debug)]
pub struct AdbVerifyTool {
    device: Option<String>,
}

impl Default for AdbVerifyTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbVerifyTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbVerifyTool {
    fn name(&self) -> &str {
        "android_verify"
    }

    fn description(&self) -> &str {
        "Check that the device's screen now shows what you expect, after an action. Answers \
         `satisfied`, `unsatisfied` or `unknown` — and `unknown` never means success: it means the \
         check could not be made, and the result says why. The evidence is the accessibility tree, \
         nothing else: not a screenshot comparison, and not the fact that an action was dispatched \
         earlier. Ask this when the question is whether something worked; android_observe is for \
         looking around."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Verify the Android screen",
            serde_json::json!({
                "text": { "type": "string", "description": "The screen should show this text, in some element's text or content description" },
                "target": { "type": "integer", "description": "An element index from android_ui_tree that should still be there" },
                "target_description": { "type": "string", "description": "Optional label you expect at `target`" },
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let expectation = match Expectation::parse(&args) {
            Ok(expectation) => expectation,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let checked = expectation.describe();

        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        match dump_ui_elements(&device, target).await {
            Ok(elements) => {
                let verdict = evaluate(&expectation, &elements);
                Ok(ToolExecutionResult::success(format!(
                    "{verdict}: checked that {checked} (read {} element(s) from the tree)",
                    elements.len()
                ))
                .with_data(serde_json::json!({
                    "verdict": verdict,
                    "checked": checked,
                    "elements_read": elements.len(),
                    "device": resolved_serial(&device).await,
                })))
            }
            Err(failure) => Ok(ToolExecutionResult::success(format!(
                "{VERDICT_UNKNOWN}: could not check that {checked} — {}",
                failure.message
            ))
            .with_data(serde_json::json!({
                "verdict": VERDICT_UNKNOWN,
                "checked": checked,
                "code": failure.code,
                "device": resolved_serial(&device).await,
            }))),
        }
    }
}

// ── ADB Pairing Tools (§4.5) ───────────────────────────────────────────────

/// Pair the phone with its own wireless-debugging adbd over loopback (§4.5).
///
/// Requires the bundled adb client (scripts/fetch-android-adb.sh). The agent
/// first pairs with the "Pair device with pairing code" dialog's port + code,
/// then connects to the connect port shown on the wireless-debugging screen.
#[derive(Debug)]
pub struct AdbPairTool {
    device: Option<String>,
}

impl Default for AdbPairTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbPairTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbPairTool {
    fn name(&self) -> &str {
        "device_adb_pair"
    }

    fn description(&self) -> &str {
        "Pair this phone with its own wireless-debugging adb server over loopback. Pass the pairing port and code from the 'Pair device with pairing code' dialog, plus the connect port shown on the Wireless debugging screen. Returns whether pairing and connecting succeeded and the device list."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Pair loopback adb",
            serde_json::json!({
                "port": {
                    "type": "integer",
                    "description": "Pairing port from the 'Pair device with pairing code' dialog"
                },
                "code": {
                    "type": "string",
                    "description": "Six-digit pairing code from the dialog"
                },
                "connect_port": {
                    "type": "integer",
                    "description": "Optional connect port from the Wireless debugging screen (defaults to port)"
                }
            }),
            vec!["port", "code"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let port = args.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
        let code = args
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let connect_port = args
            .get("connect_port")
            .and_then(|v| v.as_u64())
            .map(|p| p as u16);

        let data = super::adb_pair(port, &code, connect_port).await?;
        Ok(
            ToolExecutionResult::success("ADB pairing attempted").with_data(serde_json::json!({
                "paired": data.get("paired").and_then(|v| v.as_bool()).unwrap_or(false),
                "connected": data.get("connected").and_then(|v| v.as_bool()).unwrap_or(false),
                "pair_output": data.get("pair_output"),
                "connect_output": data.get("connect_output"),
                "devices": data.get("devices"),
            })),
        )
    }
}

/// Report loopback adb pairing status (§4.5).
#[derive(Debug)]
pub struct AdbStatusTool {
    device: Option<String>,
}

impl Default for AdbStatusTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbStatusTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbStatusTool {
    fn name(&self) -> &str {
        "device_adb_status"
    }

    fn description(&self) -> &str {
        "Report whether this phone is paired with its own adb server and list the devices adb can see."
    }

    fn parameters_schema(&self) -> Value {
        create_schema("Loopback adb status", serde_json::json!({}), Vec::<String>::new())
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let data = super::adb_status().await?;
        Ok(ToolExecutionResult::success("ADB status").with_data(
            serde_json::json!({ "paired": data.get("paired"), "devices": data.get("devices") }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Launch entry resolution ─────────────────────────────────────────
    //
    // `launch` used to guess `pkg/.MainActivity`. These cover the two parsers
    // that replaced the guess: what the package manager answers, and what
    // `am start -W` says about whether it worked. Both are pure so they run
    // without a device — which is the only way they get run at all.

    #[test]
    fn resolve_activity_reads_the_brief_form() {
        // API 26+ `--brief` output, verbatim.
        assert_eq!(
            parse_resolve_activity("com.tencent.mm/.ui.LauncherUI\n"),
            Some("com.tencent.mm/.ui.LauncherUI".to_string())
        );
    }

    #[test]
    fn resolve_activity_skips_the_preamble() {
        let out = "priority=0\npackageName=com.example\ncom.example/.MainActivity\n";
        assert_eq!(parse_resolve_activity(out), Some("com.example/.MainActivity".to_string()));
    }

    #[test]
    fn resolve_activity_returns_none_for_an_error() {
        // What the package manager prints when nothing matches.
        assert_eq!(parse_resolve_activity("Error: Activity not found\n"), None);
        assert_eq!(parse_resolve_activity(""), None);
    }

    #[test]
    fn resolve_activity_ignores_lines_that_are_not_components() {
        let out = "priority=0\nsome warning\ncom.example/.Main\n";
        assert_eq!(parse_resolve_activity(out), Some("com.example/.Main".to_string()));
    }

    #[test]
    fn am_start_ok_is_started() {
        let out = "Starting: Intent { cmp=com.example/.Main }\nStatus: ok\nActivity: com.example/.Main\nThisTime: 412\n";
        assert_eq!(parse_am_start_output(out, ""), LaunchOutcome::Started);
    }

    #[test]
    fn already_in_front_counts_as_started() {
        // Android prints this instead of `Status:` when the task is already top.
        // Treating it as failure would report every second launch of a warm app
        // as broken.
        let warning =
            "Warning: Activity not started, its current task has been brought to the front";
        assert_eq!(parse_am_start_output("", warning), LaunchOutcome::AlreadyInFront);
    }

    #[test]
    fn error_wins_over_a_later_status() {
        let out = "Error: Activity class {com.example/.Main} does not exist.\nStatus: ok\n";
        assert_eq!(parse_am_start_output(out, ""), LaunchOutcome::Failed);
    }

    #[test]
    fn status_other_than_ok_is_failed() {
        assert_eq!(parse_am_start_output("Status: timeout\n", ""), LaunchOutcome::Failed);
    }

    #[test]
    fn silent_success_is_unknown_not_ok() {
        // Exit zero with no verdict means the command ran and told us nothing.
        // Assuming success here is exactly the "dispatched, not started" bug.
        assert_eq!(parse_am_start_output("", ""), LaunchOutcome::Unknown);
        assert_eq!(
            parse_am_start_output("Starting: Intent { cmp=a/b }\n", ""),
            LaunchOutcome::Unknown
        );
    }

    // ── Stable references ───────────────────────────────────────────────
    //
    // An index is a position in one reading of the screen; a key is the
    // element itself. The tests below are about the property that matters: the
    // same element found again after the list moved, and a refusal rather than
    // a guess when it cannot be.

    /// A labelled button at `bounds`, numbered as the parser would number it.
    fn labelled_at(index: usize, text: &str, bounds: (i32, i32, i32, i32)) -> UiElement {
        UiElement::new(
            index,
            Some(text.to_string()),
            None,
            Some(format!("com.x:id/{}", text.replace(' ', "_"))),
            "Button".to_string(),
            true,
            true,
            bounds,
        )
    }

    #[test]
    fn a_key_does_not_depend_on_where_the_element_is() {
        let top = labelled_at(1, "Alice", (0, 0, 100, 100));
        let scrolled = labelled_at(1, "Alice", (0, 400, 100, 500));
        assert_eq!(top.key, scrolled.key);
        assert_ne!(top.center, scrolled.center, "the test needs them to have moved");
    }

    #[test]
    fn different_elements_have_different_keys() {
        assert_ne!(
            labelled_at(1, "Alice", (0, 0, 100, 100)).key,
            labelled_at(1, "Bob", (0, 0, 100, 100)).key
        );
    }

    #[test]
    fn a_key_is_found_again_after_the_list_scrolled() {
        let read_before = vec![
            labelled_at(1, "Alice", (0, 0, 100, 100)),
            labelled_at(2, "Bob", (0, 100, 100, 200)),
        ];
        let key = read_before[1].key.clone();
        // A new row arrived above, so Bob is element 3 now and is elsewhere.
        let read_after = vec![
            labelled_at(1, "Zed", (0, 0, 100, 100)),
            labelled_at(2, "Alice", (0, 100, 100, 200)),
            labelled_at(3, "Bob", (0, 200, 100, 300)),
        ];
        let found = relocate(&read_after, &key).expect("relocated");
        assert_eq!(element_label(found), Some("Bob"));
        assert_eq!(found.index, 3);
    }

    #[test]
    fn a_key_that_is_gone_is_a_refusal_not_a_guess() {
        let elements = vec![labelled_at(1, "Alice", (0, 0, 100, 100))];
        let err = relocate(&elements, "deadbeef").expect_err("refused");
        assert!(err.starts_with("STALE_REF"), "{err}");
        assert!(err.contains("re-read"), "{err}");
    }

    #[test]
    fn two_indistinguishable_elements_are_refused_and_listed() {
        // Two rows with the same label, class and resource id: picking the
        // first would be the silent wrong-target failure the key exists for.
        let elements = vec![
            labelled_at(1, "Row", (0, 0, 100, 100)),
            labelled_at(2, "Row", (0, 200, 100, 300)),
        ];
        let err = relocate(&elements, &elements[0].key).expect_err("refused");
        assert!(err.contains("2 elements match"), "{err}");
        assert!(err.contains("[1]"), "{err}");
        assert!(err.contains("[2]"), "{err}");
    }

    #[test]
    fn an_empty_key_never_matches_anything() {
        let mut element = labelled_at(1, "Alice", (0, 0, 100, 100));
        element.key.clear();
        assert!(relocate(&[element], "").is_err());
    }

    // ── Text input encoding ─────────────────────────────────────────────
    //
    // `adb shell <cmd>` hands the string to the *device* shell, so the text has
    // to survive two layers: the shell's word parsing, then `input`'s own `%s`
    // convention. Getting either wrong used to send a malformed command.

    #[test]
    fn plain_text_is_a_single_quoted_word() {
        assert_eq!(encode_input_text("hello"), "input text 'hello'");
    }

    #[test]
    fn spaces_use_inputs_percent_s_convention() {
        assert_eq!(encode_input_text("hello world"), "input text 'hello%sworld'");
    }

    #[test]
    fn a_literal_percent_is_doubled() {
        assert_eq!(encode_input_text("100%"), "input text '100%%'");
        // Doubling happens before spaces are converted, so the `%` that
        // introduces `%s` is not itself doubled.
        assert_eq!(encode_input_text("50% off"), "input text '50%%%soff'");
    }

    #[test]
    fn an_embedded_quote_cannot_end_the_command() {
        // The old encoding produced `input text 'it's'`, which the device shell
        // reads as the command `input text 'it'` followed by `s'` — the text
        // after the quote was interpreted as shell, not typed.
        assert_eq!(encode_input_text("it's"), r#"input text 'it'\''s'"#);
    }

    #[test]
    fn shell_metacharacters_are_inert() {
        let cmd = encode_input_text("a; rm -rf / && echo $(whoami) `id` | tee > /tmp/x");
        // Exactly one command, wrapped as one quoted word: the metacharacters are
        // data, not syntax.
        assert_eq!(cmd.matches("input text").count(), 1);
        assert!(cmd.starts_with("input text '"), "got {cmd}");
        assert!(cmd.ends_with('\''), "got {cmd}");
        assert!(cmd.contains("$(whoami)"), "got {cmd}");
    }

    #[test]
    fn a_keycode_name_works_on_both_dispatch_paths() {
        // `input keyevent` takes both spellings, the display helper only takes
        // the field name — so the bare name is normalised where the op is built.
        assert_eq!(normalize_keycode("ENTER"), "KEYCODE_ENTER");
        assert_eq!(normalize_keycode(" enter "), "KEYCODE_ENTER");
        assert_eq!(normalize_keycode("KEYCODE_ENTER"), "KEYCODE_ENTER");
        // A number is accepted by both dispatchers and must not be prefixed.
        assert_eq!(normalize_keycode("66"), "66");
        assert_eq!(normalize_keycode(""), "");
    }

    #[test]
    fn newlines_become_enter_keyevents() {
        assert_eq!(
            encode_input_text("line1\nline2"),
            "input text 'line1'; input keyevent 66; input text 'line2'"
        );
    }

    #[test]
    fn blank_lines_survive_as_paragraph_breaks() {
        assert_eq!(
            encode_input_text("a\n\nb"),
            "input text 'a'; input keyevent 66; input keyevent 66; input text 'b'"
        );
    }

    #[test]
    fn carriage_returns_from_crlf_are_stripped() {
        assert_eq!(
            encode_input_text("a\r\nb"),
            "input text 'a'; input keyevent 66; input text 'b'"
        );
    }

    #[test]
    fn non_ascii_is_not_silently_mangled_here() {
        // The helper passes non-ASCII through unchanged; deciding what to *do*
        // with it (ADBKeyboard vs an explicit error) happens in `execute`, which
        // needs a device. This pins that the helper itself does not corrupt it.
        let cmd = encode_input_text("登录");
        assert_eq!(cmd, "input text '登录'");
    }

    // ── UI tree parsing ─────────────────────────────────────────────────

    /// A dump in the shape `uiautomator dump` actually produces: nested
    /// containers, a text field, a labelled button, a node whose label lives in
    /// `content-desc`, an escaped entity, and bare layout containers that carry
    /// nothing actionable.
    const DUMP: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?>
<hierarchy rotation="0">
  <node index="0" class="android.widget.FrameLayout" text="" resource-id="" bounds="[0,0][1080,2340]">
    <node index="1" class="android.widget.TextView" text="登录 &amp; 注册" resource-id="com.app:id/title" bounds="[100,200][980,300]" enabled="true" clickable="false" />
    <node index="2" class="android.widget.Button" text="登录" resource-id="com.app:id/login" bounds="[120,840][360,920]" enabled="true" clickable="true" />
    <node index="3" class="android.widget.ImageButton" text="" content-desc="返回" resource-id="" bounds="[0,100][100,200]" enabled="true" clickable="true" />
    <node index="4" class="android.widget.EditText" text="" resource-id="com.app:id/phone" bounds="[120,500][960,580]" enabled="false" clickable="true" />
    <node index="5" class="android.view.View" text="" resource-id="" bounds="[0,300][1080,400]" enabled="true" clickable="false" scrollable="false" />
    <node index="6" class="android.widget.EditText" text="" resource-id="com.app:id/code" bounds="[120,600][960,680]" enabled="true" clickable="false" password="true" focused="true" />
    <node index="7" class="androidx.recyclerview.widget.RecyclerView" text="" resource-id="" bounds="[0,1000][1080,2000]" enabled="true" clickable="false" scrollable="true">
      <node index="0" class="android.widget.CheckBox" text="记住我" resource-id="" bounds="[120,1020][400,1100]" enabled="true" clickable="true" checkable="true" checked="true" />
    </node>
  </node>
</hierarchy>"#;

    #[test]
    fn parses_bounds_attribute() {
        assert_eq!(parse_bounds("[0,0][1080,2340]"), Some((0, 0, 1080, 2340)));
        assert_eq!(parse_bounds("[120,840][360,920]"), Some((120, 840, 360, 920)));
        assert_eq!(parse_bounds("garbage"), None);
        assert_eq!(parse_bounds("[1,2]"), None);
    }

    #[test]
    fn keeps_only_actionable_or_labelled_nodes() {
        let els = parse_uiautomator_xml(DUMP).expect("parses");
        // The bare FrameLayout and the unlabelled non-clickable View are dropped.
        let classes: Vec<&str> = els.iter().map(|e| e.class.as_str()).collect();
        assert!(!classes.contains(&"FrameLayout"), "container kept: {classes:?}");
        // The unnamed code field and the scroll list are kept despite having no
        // name: one is where a write goes, the other is where a scroll goes.
        assert!(classes.contains(&"EditText"), "{classes:?}");
        assert!(classes.contains(&"RecyclerView"), "{classes:?}");
        assert_eq!(els.len(), 7, "{classes:?}");
    }

    #[test]
    fn xml_semantics_reach_the_element() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        let code = els
            .iter()
            .find(|e| e.resource_id.as_deref() == Some("com.app:id/code"))
            .unwrap();
        assert!(code.state.password);
        assert!(code.state.focused);
        assert!(code.state.editable);
        let list = els.iter().find(|e| e.class == "RecyclerView").unwrap();
        assert!(list.state.scrollable);
        assert!(list.state.scroll_forward && list.state.scroll_backward);
        let check = els.iter().find(|e| e.class == "CheckBox").unwrap();
        assert!(check.state.checkable && check.state.checked);
    }

    #[test]
    fn depth_is_counted_from_the_root_node() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        let title = els.iter().find(|e| e.class == "TextView").unwrap();
        let list = els.iter().find(|e| e.class == "RecyclerView").unwrap();
        let check = els.iter().find(|e| e.class == "CheckBox").unwrap();
        assert_eq!(title.state.depth, 1);
        assert_eq!(list.state.depth, 1);
        // The checkbox sits under the list, so a scroll aimed at the midpoint
        // would pick the list and not the row.
        assert_eq!(check.state.depth, 2);
    }

    #[test]
    fn a_password_field_is_not_read_out() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        let summary = format_ui_summary(&els);
        assert!(summary.contains("[password]"), "{summary}");
        assert!(!summary.contains("com.app:id/code"), "{summary}");
        assert!(summary.contains("editable"), "{summary}");
        assert!(summary.contains("checked"), "{summary}");
        assert!(summary.contains("scrollable"), "{summary}");
    }

    #[test]
    fn indices_are_one_based_and_contiguous() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        for (i, e) in els.iter().enumerate() {
            assert_eq!(e.index, i + 1);
        }
    }

    #[test]
    fn class_names_are_shortened() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        assert_eq!(els[0].class, "TextView");
        assert_eq!(els[1].class, "Button");
    }

    #[test]
    fn xml_entities_are_decoded() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        assert_eq!(els[0].text.as_deref(), Some("登录 & 注册"));
    }

    #[test]
    fn empty_attributes_become_none() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        // The image button has text="" but a content-desc.
        let back = els.iter().find(|e| e.class == "ImageButton").unwrap();
        assert_eq!(back.text, None);
        assert_eq!(back.content_desc.as_deref(), Some("返回"));
    }

    #[test]
    fn center_is_derived_from_bounds() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        let login = els
            .iter()
            .find(|e| e.text.as_deref() == Some("登录"))
            .unwrap();
        assert_eq!(login.bounds, (120, 840, 360, 920));
        assert_eq!(login.center, (240, 880));
    }

    #[test]
    fn disabled_flag_is_carried() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        let phone = els.iter().find(|e| e.class == "EditText").unwrap();
        assert!(!phone.enabled);
        assert!(els[1].enabled);
    }

    #[test]
    fn summary_lists_elements_with_their_index_and_center() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        let summary = format_ui_summary(&els);
        assert!(summary.starts_with("7 actionable element(s):"), "{summary}");
        assert!(summary.contains(r#"[2] Button "登录" clickable center=(240,880)"#), "{summary}");
        assert!(summary.contains("disabled"), "{summary}");
    }

    #[test]
    fn malformed_xml_is_an_error_not_a_panic() {
        assert!(parse_uiautomator_xml("<hierarchy><node").is_err());
        assert!(parse_uiautomator_xml("").is_err());
    }

    #[test]
    fn empty_hierarchy_summarises_clearly() {
        let els = parse_uiautomator_xml("<hierarchy rotation=\"0\"/>").unwrap();
        assert!(els.is_empty());
        let msg = format_ui_summary(&els);
        assert!(msg.contains("no actionable elements"), "{msg}");
        // An empty tree is not a dead end: the screenshot still describes the
        // screen, so the summary has to say so.
        assert!(msg.contains("screenshot"), "{msg}");
    }

    // ── Tap safety net ──────────────────────────────────────────────────
    //
    // An indexed tap re-reads the tree and validates before dispatching. All of
    // that decision-making is `validate_target`, which needs no device.

    fn element(index: usize, text: Option<&str>, class: &str) -> UiElement {
        let mut element = UiElement::new(
            index,
            text.map(str::to_string),
            None,
            None,
            class.to_string(),
            true,
            true,
            (0, 0, 10, 10),
        );
        element
    }

    #[test]
    fn valid_index_resolves_to_its_center() {
        let els = vec![element(1, Some("登录"), "Button")];
        let target = validate_target(&els, 1, None).expect("valid");
        assert_eq!(target.center, (5, 5));
    }

    #[test]
    fn index_zero_is_rejected() {
        let els = vec![element(1, Some("登录"), "Button")];
        let err = validate_target(&els, 0, None).unwrap_err();
        assert!(err.contains("1-based"), "{err}");
    }

    #[test]
    fn an_index_past_the_end_says_how_many_there_are() {
        let els = vec![element(1, Some("a"), "Button")];
        let err = validate_target(&els, 7, None).unwrap_err();
        assert!(err.contains("No element 7"), "{err}");
        assert!(err.contains("1 element(s)"), "{err}");
    }

    #[test]
    fn a_description_mismatch_refuses_and_names_both() {
        // This is the case the safety net exists for: the model is aiming at
        // something that is no longer at that index.
        let els = vec![element(1, Some("注册"), "Button")];
        let err = validate_target(&els, 1, Some("登录")).unwrap_err();
        assert!(err.contains("Refused"), "{err}");
        assert!(err.contains("\"注册\""), "{err}");
        assert!(err.contains("\"登录\""), "{err}");
    }

    #[test]
    fn a_matching_description_passes_loosely() {
        let els = vec![element(1, Some("Login with phone"), "Button")];
        assert!(validate_target(&els, 1, Some("login")).is_ok(), "case-insensitive substring");
        assert!(validate_target(&els, 1, Some("Login with phone")).is_ok(), "exact");
    }

    #[test]
    fn an_unlabelled_element_cannot_be_checked_but_passes() {
        // An icon button identified from the screenshot has no text to match;
        // refusing there would block a legitimate tap.
        let els = vec![element(1, None, "ImageButton")];
        assert!(validate_target(&els, 1, Some("back arrow")).is_ok());
    }

    #[test]
    fn non_clickable_and_disabled_targets_are_refused() {
        let mut no_click = element(1, Some("标题"), "TextView");
        no_click.clickable = false;
        let err = validate_target(&[no_click], 1, None).unwrap_err();
        assert!(err.contains("not clickable"), "{err}");

        let mut disabled = element(1, Some("提交"), "Button");
        disabled.enabled = false;
        let err = validate_target(&[disabled], 1, None).unwrap_err();
        assert!(err.contains("disabled"), "{err}");
    }

    // ── Delivery ladder ─────────────────────────────────────────────────
    //
    // The rungs themselves need a device; what decides between them is a pure
    // function and is tested here.

    #[test]
    fn a_press_gesture_is_a_still_finger_on_the_element() {
        let element = element(1, Some("Play"), "Button");
        let paths = press_paths(&element);
        assert_eq!(paths.len(), 1);
        // Three points on the same coordinate: a contact the platform sees for
        // more than one frame, which is what a tap is.
        assert_eq!(paths[0], vec![(5, 5), (5, 5), (5, 5)]);
    }

    #[test]
    fn only_a_control_press_is_news_in_the_result() {
        // A shell press is what every result was before the ladder existed.
        assert!(Via::Shell.note().is_none());
        assert!(Via::None.note().is_none());
        assert!(Via::Semantic.note().unwrap().contains("control"));
        assert!(Via::Gesture.note().unwrap().contains("gesture"));
        assert_eq!(Via::Semantic.as_str(), "semantic");
        assert_eq!(Via::Gesture.as_str(), "gesture");
        assert_eq!(Via::Shell.as_str(), "shell");
    }

    // ── Tap sequences ───────────────────────────────────────────────────

    #[test]
    fn a_sequence_becomes_one_shell_command() {
        // One dispatch, not N: every extra round trip is a model turn during
        // which the transient control has already faded.
        assert_eq!(encode_tap_sequence(&[(10, 20), (30, 40)]), "input tap 10 20; input tap 30 40");
        assert_eq!(encode_tap_sequence(&[(5, 6)]), "input tap 5 6");
    }

    #[test]
    fn steps_parse_from_the_argument() {
        let args = serde_json::json!([{ "x": 1, "y": 2 }, { "x": 3, "y": 4 }]);
        assert_eq!(parse_tap_steps(Some(&args)).unwrap(), vec![(1, 2), (3, 4)]);
    }

    #[test]
    fn malformed_steps_are_rejected_with_a_reason() {
        assert!(parse_tap_steps(None)
            .unwrap_err()
            .contains("requires a 'steps' array"));
        assert!(parse_tap_steps(Some(&serde_json::json!([])))
            .unwrap_err()
            .contains("at least one"));
        let missing_y = serde_json::json!([{ "x": 1 }]);
        assert!(parse_tap_steps(Some(&missing_y))
            .unwrap_err()
            .contains("step 0"));
        let wrong_type = serde_json::json!([{ "x": "1", "y": 2 }]);
        assert!(parse_tap_steps(Some(&wrong_type)).is_err());
    }

    /// A binary payload must survive as bytes.
    ///
    /// `screencap -p` returns a PNG, and the screenshot path used to base64 the
    /// result of `run_cmd`, whose stdout is `String::from_utf8_lossy` — every
    /// non-UTF-8 byte became U+FFFD, so the "screenshot" was not a decodable
    /// image. This uses a local `printf` rather than adb, so the fix is verified
    /// here rather than only on a device.
    #[tokio::test]
    async fn binary_stdout_survives_as_bytes() {
        let (status, bytes, _) = run_cmd_bytes("printf", &["\\377\\376\\101"]).await.unwrap();
        assert!(status.success());
        assert_eq!(bytes, vec![0xFF, 0xFE, 0x41]);
        // The lossy path would have replaced the invalid byte sequences.
        assert_ne!(String::from_utf8_lossy(&bytes).as_bytes(), bytes.as_slice());
    }

    // ── Failure classification (B4) ─────────────────────────────────────
    //
    // `uiautomator`'s stderr looks the same across causes that need different
    // responses, and the fallback ("use the screenshot") applies to all of them.

    #[test]
    fn dump_failures_are_told_apart() {
        // The code is the half a caller branches on, so it has to be right as
        // well as the sentence.
        let offline = DumpFailure::classify("error: device 'emulator-5554' not found");
        assert_eq!(offline.code, DEVICE_GONE);
        assert!(offline.message.contains("not connected"), "{offline}");

        let busy = DumpFailure::classify("ERROR: could not get idle state.");
        assert_eq!(busy.code, SCREEN_NEVER_IDLE);
        assert!(busy.message.contains("never settled"), "{busy}");

        let secure =
            DumpFailure::classify("ERROR: null root node returned by UiTestAutomationBridge.");
        assert_eq!(secure.code, SECURE_WINDOW);
        assert!(secure.message.contains("secure window"), "{secure}");

        let disabled = DumpFailure::classify("java.lang.SecurityException: Permission Denial");
        assert_eq!(disabled.code, ACCESSIBILITY_DISABLED);
        assert!(disabled.message.contains("not enabled"), "{disabled}");

        // Anything unrecognised still gets a code, so a caller never has to
        // handle "no code at all".
        assert_eq!(DumpFailure::classify("something nobody has seen before").code, DUMP_FAILED);
    }

    #[test]
    fn every_dump_failure_points_at_the_screenshot() {
        for stderr in [
            "device offline",
            "ERROR: could not get idle state.",
            "something nobody has seen before",
            "",
        ] {
            let failure = DumpFailure::classify(stderr);
            let msg = &failure.message;
            assert!(msg.contains("screenshot"), "{msg}");
            assert!(msg.contains("android_observe"), "{msg}");
            assert!(
                [
                    DEVICE_GONE,
                    SCREEN_NEVER_IDLE,
                    SECURE_WINDOW,
                    ACCESSIBILITY_DISABLED,
                    DUMP_FAILED
                ]
                .contains(&failure.code),
                "unknown code {}",
                failure.code
            );
        }
    }

    #[test]
    fn raw_output_is_kept_when_there_is_any() {
        assert!(DumpFailure::classify("weird failure")
            .message
            .contains("weird failure"));
        assert!(!DumpFailure::classify("   ").message.contains("Raw output"));
    }

    #[test]
    fn png_dimensions_reads_the_ihdr_chunk() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&13u32.to_be_bytes()); // IHDR length (unused)
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1080u32.to_be_bytes());
        png.extend_from_slice(&2340u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), Some((1080, 2340)));

        assert_eq!(png_dimensions(b"not a png"), None);
        assert_eq!(png_dimensions(&[0x89, b'P']), None);
    }

    #[test]
    fn coordinate_space_check_catches_a_rotation_or_scale() {
        // Matching, including the axes swapped by a rotation.
        assert!(coordinate_space_consistent(Some((1080, 2340)), Some((1080, 2340))));
        assert!(coordinate_space_consistent(Some((1080, 2340)), Some((2340, 1080))));
        // A scaled capture, or a stale tree from before a rotation.
        assert!(!coordinate_space_consistent(Some((1080, 2340)), Some((540, 1170))));
        assert!(!coordinate_space_consistent(Some((1080, 2340)), Some((1080, 1080))));
        // Nothing to compare is not a mismatch.
        assert!(coordinate_space_consistent(None, Some((1080, 2340))));
        assert!(coordinate_space_consistent(Some((1080, 2340)), None));
    }

    #[test]
    fn screen_bounds_come_from_the_root_node() {
        assert_eq!(parse_screen_bounds(DUMP), Some((1080, 2340)));
        assert_eq!(parse_screen_bounds("<hierarchy rotation=\"0\"/>"), None);
        assert_eq!(parse_screen_bounds("garbage"), None);
    }

    #[test]
    fn test_adb_screenshot_tool_name() {
        let tool = AdbScreenshotTool::new();
        assert_eq!(tool.name(), "android_screenshot");
    }

    #[test]
    fn test_adb_input_tool_schema() {
        let tool = AdbInputTool::new();
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }

    #[test]
    fn test_adb_app_manager_tool_name() {
        let tool = AdbAppManagerTool::new();
        assert_eq!(tool.name(), "android_app_manager");
    }

    // ── Tree quality ────────────────────────────────────────────────────
    //
    // `uiautomator dump` succeeds on a game, a canvas and a normal screen
    // alike; what differs is whether the tree describes any of it.

    fn node(class: &str, text: Option<&str>, bounds: (i32, i32, i32, i32)) -> UiElement {
        UiElement::new(
            1,
            text.map(str::to_string),
            None,
            None,
            class.to_string(),
            true,
            true,
            bounds,
        )
    }

    // ── Incremental reads ──────────────────────────────────────────────

    fn row(index: usize, key: &str, y: i32) -> UiElement {
        let mut element = node("android.widget.TextView", Some(key), (0, y, 200, y + 80));
        element.index = index;
        element.key = key.to_string();
        element
    }

    fn ui_dump(elements: Vec<UiElement>, package: &str) -> UiDump {
        let fingerprint = tree_fingerprint(&elements);
        UiDump {
            elements,
            screen: Some((1080, 2340)),
            source: "accessibility",
            package: Some(package.to_string()),
            snapshot: snapshot_name(fingerprint),
            truncated: false,
        }
    }

    #[test]
    fn a_snapshot_is_the_same_name_for_the_same_screen() {
        // Diffing against a name only works if the name is the screen, not the
        // enumeration: two identical reads must be the same baseline.
        let first = ui_dump(
            vec![node("android.widget.TextView", Some("a"), (0, 0, 100, 50))],
            "com.example",
        );
        let second = ui_dump(
            vec![node("android.widget.TextView", Some("a"), (0, 0, 100, 50))],
            "com.example",
        );
        assert_eq!(first.snapshot, second.snapshot);
        // And different screens must not share one.
        let other = ui_dump(
            vec![node("android.widget.TextView", Some("b"), (0, 0, 100, 50))],
            "com.example",
        );
        assert_ne!(first.snapshot, other.snapshot);
    }

    #[test]
    fn no_since_means_a_full_read() {
        let dump = ui_dump(vec![node("android.widget.TextView", Some("a"), (0, 0, 100, 50))], "x");
        assert!(since_answer(None, Target::Screen, None, &dump).is_none());
    }

    #[test]
    fn a_named_previous_read_yields_only_what_changed() {
        let before = vec![row(1, "a", 0), row(2, "b", 80)];
        crate::computer::platform::mobile::diff::remember(
            None,
            0,
            tree_fingerprint(&before),
            Some("com.example"),
            &before,
        );
        let after = vec![row(1, "a", 0), row(2, "b", 80), row(3, "c", 160)];
        let dump = ui_dump(after, "com.example");
        // The baseline name is the fingerprint of the *earlier* read.
        let since = format!("{:x}", tree_fingerprint(&before));
        let answer = since_answer(None, Target::Screen, Some(&since), &dump).expect("an answer");
        match answer {
            SinceAnswer::Delta { text, data } => {
                assert!(text.contains("1 appeared"), "{text}");
                let added = data["diff"]["added"].as_u64().expect("a count");
                assert_eq!(added, 1);
                // Only the new row is addressable now, so only it is listed.
                let listed = data["elements"].as_array().expect("rows");
                assert_eq!(listed.len(), 1);
                assert_eq!(listed[0]["key"], "c");
                assert_eq!(data["baseline"], true);
            }
            SinceAnswer::Full(reason) => panic!("expected a delta, got {reason:?}"),
        }
    }

    #[test]
    fn an_unknown_baseline_falls_back_and_says_why() {
        let dump = ui_dump(vec![node("android.widget.TextView", Some("a"), (0, 0, 100, 50))], "x");
        let answer =
            since_answer(None, Target::Screen, Some("deadbeef"), &dump).expect("an answer");
        match answer {
            SinceAnswer::Full(reason) => {
                assert_eq!(reason, crate::computer::platform::mobile::diff::FullReason::NoBaseline);
                assert!(reason.describe().contains("whole screen"));
            }
            SinceAnswer::Delta { .. } => panic!("expected a fallback"),
        }
    }

    #[test]
    fn a_different_app_is_not_compared() {
        let before = vec![row(1, "a", 0)];
        crate::computer::platform::mobile::diff::remember(
            None,
            0,
            0x1234,
            Some("com.one"),
            &before,
        );
        let dump = ui_dump(vec![row(1, "a", 0)], "com.two");
        let answer = since_answer(None, Target::Screen, Some("1234"), &dump).expect("an answer");
        assert!(matches!(
            answer,
            SinceAnswer::Full(crate::computer::platform::mobile::diff::FullReason::AppChanged)
        ));
    }

    #[test]
    fn a_fallback_names_its_reason_in_the_data() {
        let data =
            since_fallback(crate::computer::platform::mobile::diff::FullReason::MostlyChanged);
        assert_eq!(data["baseline"], false);
        assert_eq!(data["fallback"], "mostly_changed");
    }

    #[test]
    fn an_empty_tree_is_unusable() {
        assert_eq!(assess_tree(&[]), TreeQuality::Unusable);
    }

    #[test]
    fn a_tree_of_anonymous_boxes_is_unusable() {
        let elements = vec![
            node("android.widget.FrameLayout", None, (0, 0, 100, 100)),
            node("android.widget.FrameLayout", None, (0, 0, 100, 50)),
        ];
        assert_eq!(assess_tree(&elements), TreeQuality::Unusable);
    }

    #[test]
    fn a_mostly_nameless_tree_is_partial() {
        let elements = vec![
            node("android.widget.Button", Some("OK"), (0, 0, 10, 10)),
            node("android.view.View", None, (0, 0, 10, 10)),
            node("android.view.View", None, (0, 0, 10, 10)),
            node("android.view.View", None, (0, 0, 10, 10)),
            node("android.view.View", None, (0, 0, 10, 10)),
        ];
        assert_eq!(assess_tree(&elements), TreeQuality::Partial);
    }

    #[test]
    fn a_named_screen_is_usable_and_says_nothing() {
        let elements = (0..5)
            .map(|i| node("android.widget.TextView", Some("row"), (0, i * 10, 100, i * 10 + 9)))
            .collect::<Vec<_>>();
        assert_eq!(assess_tree(&elements), TreeQuality::Usable);
        assert_eq!(TreeQuality::Usable.advice(), None);
        assert!(TreeQuality::Unusable.advice().is_some());
    }

    #[test]
    fn structural_nodes_keep_a_nameless_list_screen_usable() {
        // A message list whose rows are custom-drawn: the only thing the tree
        // says is "there is a list here", and that is exactly what a read needs.
        let elements = (0..4)
            .map(|i| {
                let mut element = node("android.view.View", None, (0, i * 10, 100, i * 10 + 9));
                element.state.scrollable = i == 0;
                element
            })
            .collect::<Vec<_>>();
        assert_eq!(assess_tree(&elements), TreeQuality::Partial);
        let mut list_only = elements[..1].to_vec();
        list_only[0].state.scrollable = true;
        assert_eq!(assess_tree(&list_only), TreeQuality::Partial);
        // Enough structure and the screen is judged readable even unnamed.
        let structured = (0..3)
            .map(|i| {
                let mut element =
                    node("android.widget.EditText", None, (0, i * 10, 100, i * 10 + 9));
                element.state.editable = true;
                element
            })
            .collect::<Vec<_>>();
        assert_eq!(assess_tree(&structured), TreeQuality::Usable);
    }

    #[test]
    fn a_tree_of_unstructured_unnamed_boxes_is_still_unusable() {
        let mut clickable = node("android.view.View", None, (0, 0, 100, 100));
        clickable.clickable = true;
        let elements = vec![
            clickable,
            node("android.view.View", None, (0, 0, 100, 50)),
            node("android.view.View", None, (0, 50, 100, 100)),
        ];
        assert_eq!(assess_tree(&elements), TreeQuality::Unusable);
    }

    #[test]
    fn content_description_counts_as_a_name() {
        let mut described = node("android.widget.ImageButton", None, (0, 0, 10, 10));
        described.content_desc = Some("Back".to_string());
        assert_eq!(element_label(&described), Some("Back"));
    }

    #[test]
    fn blank_labels_do_not_count() {
        let blank = node("android.widget.TextView", Some("   "), (0, 0, 10, 10));
        assert_eq!(element_label(&blank), None);
    }

    // ── Write readback ─────────────────────────────────────────────────────

    fn field(text: Option<&str>, rid: Option<&str>, bounds: (i32, i32, i32, i32)) -> UiElement {
        let mut element = UiElement::new(
            1,
            text.map(str::to_string),
            None,
            rid.map(str::to_string),
            "EditText".to_string(),
            true,
            true,
            bounds,
        );
        element.state.editable = true;
        element
    }

    #[test]
    fn readback_ignores_whitespace_the_field_inserted() {
        assert!(readback_matches("13800138000", "138 0013 8000"));
        assert!(readback_matches("hello world", "helloworld"));
        // Case and punctuation are the message, not formatting.
        assert!(!readback_matches("Hello", "hello"));
        assert!(!readback_matches("done.", "done"));
    }

    #[test]
    fn the_field_holding_the_text_is_a_match() {
        assert_eq!(readback_verdict("hi", "hi", None, false), READBACK_MATCH);
        assert_eq!(readback_verdict("hi", "hi", Some(""), false), READBACK_MATCH);
    }

    #[test]
    fn different_text_is_a_mismatch() {
        assert_eq!(readback_verdict("hi", "ho", None, false), READBACK_MISMATCH);
    }

    #[test]
    fn a_field_that_did_not_move_but_saw_the_text_is_unknown_not_a_mismatch() {
        // The field answers the same hint whatever is typed, while the screen
        // echoed the text: the input arrived, this field cannot confirm it, and
        // calling that a mismatch would loop the caller into typing twice.
        assert_eq!(
            readback_verdict("hi", "Type a message", Some("Type a message"), true),
            READBACK_UNKNOWN
        );
        // Same stuck field without any echo: the write really did nothing.
        assert_eq!(
            readback_verdict("hi", "Type a message", Some("Type a message"), false),
            READBACK_MISMATCH
        );
    }

    #[test]
    fn the_write_point_is_the_focused_field() {
        let mut first = field(None, Some("id/a"), (0, 0, 400, 100));
        let focused = field(None, Some("id/b"), (0, 200, 400, 300));
        first.state.focused = false;
        let mut focused = focused;
        focused.state.focused = true;
        let elements = vec![first, focused];
        assert_eq!(write_point(&elements), (200, 250));
    }

    #[test]
    fn the_field_is_located_by_place_not_by_key_which_the_write_changed() {
        let before = vec![
            field(Some("old"), Some("id/a"), (0, 0, 400, 100)),
            field(None, Some("id/b"), (0, 200, 400, 300)),
        ];
        let found = locate_for_readback(&before, (200, 250)).expect("a field");
        assert_eq!(found.resource_id.as_deref(), Some("id/b"));
        // And after the write the same field has a different key — which is
        // exactly why the key cannot be what finds it.
        let mut after = field(Some("new"), Some("id/b"), (0, 200, 400, 300));
        after.key = "different".to_string();
        assert_ne!(found.key, after.key);
    }

    #[test]
    fn a_focused_field_wins_over_one_the_point_merely_lands_in() {
        let mut focused = field(None, Some("id/a"), (0, 0, 400, 100));
        focused.state.focused = true;
        let other = field(None, Some("id/b"), (0, 0, 400, 100));
        let elements = vec![other, focused];
        assert_eq!(
            locate_for_readback(&elements, (200, 50))
                .expect("a field")
                .resource_id
                .as_deref(),
            Some("id/a")
        );
    }

    #[test]
    fn a_password_field_is_still_found_so_the_refusal_can_name_it() {
        let mut secret = field(None, Some("id/pw"), (0, 0, 400, 100));
        secret.state.password = true;
        secret.state.editable = false;
        let elements = vec![secret];
        let found = locate_for_readback(&elements, (200, 50)).expect("a field");
        assert!(found.state.password);
    }

    #[test]
    fn a_screen_without_a_field_has_nothing_to_read_back() {
        assert!(locate_for_readback(&[], (0, 0)).is_none());
        let label = node("android.widget.TextView", Some("hi"), (0, 0, 100, 100));
        assert!(locate_for_readback(&[label], (10, 10)).is_none());
    }

    #[test]
    fn an_echo_is_a_label_the_screen_did_not_have_before() {
        let before = vec![node(
            "android.widget.TextView",
            Some("Message"),
            (0, 0, 100, 40),
        )];
        let after = vec![
            node("android.widget.TextView", Some("Message"), (0, 0, 100, 40)),
            node("android.widget.TextView", Some("say hello now"), (0, 40, 100, 80)),
        ];
        assert!(screen_echoed(&before, &after, "hello"));
        // The same label was already there: not evidence of this write.
        assert!(!screen_echoed(&before, &after, "Message"));
        assert!(!screen_echoed(&before, &before, "hello"));
    }

    // ── Targets ─────────────────────────────────────────────────────────

    #[test]
    fn no_display_argument_means_the_users_screen() {
        let args = serde_json::json!({ "action": "tap", "x": 1, "y": 2 });
        assert_eq!(Target::from_args(&args), Ok(Target::Screen));
        let explicit_null = serde_json::json!({ "display": null });
        assert_eq!(Target::from_args(&explicit_null), Ok(Target::Screen));
    }

    #[test]
    fn a_display_id_targets_that_display() {
        let args = serde_json::json!({ "display": 3 });
        assert_eq!(Target::from_args(&args), Ok(Target::Display(3)));
    }

    #[test]
    fn a_non_integer_display_is_refused_with_a_reason() {
        let args = serde_json::json!({ "display": "three" });
        let message = Target::from_args(&args).expect_err("refused");
        assert!(message.contains("integer"), "{message}");
    }

    // ── Action effect ───────────────────────────────────────────────────

    #[test]
    fn fingerprint_tracks_labels_and_positions() {
        let base = vec![node("android.widget.TextView", Some("a"), (0, 0, 10, 10))];
        let same = base.clone();
        let moved = vec![node("android.widget.TextView", Some("a"), (0, 50, 10, 60))];
        let renamed = vec![node("android.widget.TextView", Some("b"), (0, 0, 10, 10))];

        assert_eq!(tree_fingerprint(&base), tree_fingerprint(&same));
        assert_ne!(tree_fingerprint(&base), tree_fingerprint(&moved));
        assert_ne!(tree_fingerprint(&base), tree_fingerprint(&renamed));
    }

    #[test]
    fn an_unknown_effect_says_so_instead_of_claiming_success() {
        let settle = Settle {
            effect: EFFECT_UNKNOWN,
            elapsed_ms: 300,
            elements: Vec::new(),
            note: Some("SCREEN_NEVER_IDLE".to_string()),
            pixel_changed: None,
            diff: None,
            package: None,
            first_signal: None,
        };
        let verdict = settle.verdict();
        assert!(verdict.starts_with("Effect unknown"), "{verdict}");
        assert!(verdict.contains("SCREEN_NEVER_IDLE"), "{verdict}");
    }

    #[test]
    fn only_a_content_event_from_this_screen_ends_a_settle_early() {
        let event = |kind, package: &str| crate::device::DeviceEvent {
            seq: 1,
            kind,
            package: package.to_string(),
            at_uptime_ms: 1,
            title: None,
            text: None,
            truncated: false,
        };
        use crate::device::EventKind;
        // The screen redrawing is the signal; a window change is a different
        // place (the caller's package check owns that) and a notification from
        // another app appears without the screen moving.
        assert!(settle_event_matters(&event(EventKind::Content, "com.a"), Some("com.a")));
        assert!(!settle_event_matters(&event(EventKind::Window, "com.b"), Some("com.a")));
        assert!(!settle_event_matters(&event(EventKind::Notification, "com.a"), Some("com.a")));
        // A content event from a different app is that app redrawing, not this
        // screen moving — and an app that *came to the top* raises a window
        // event, which the caller's own package check handles.
        assert!(!settle_event_matters(&event(EventKind::Content, "com.b"), Some("com.a")));
        // Before the first read the package is unknown, so any content event
        // counts — dropping it would miss the action's own first frame.
        assert!(settle_event_matters(&event(EventKind::Content, "com.b"), None));
    }

    #[test]
    fn an_unchanged_screen_points_at_the_screenshot() {
        let settle = Settle {
            effect: EFFECT_UNCHANGED,
            elapsed_ms: 1200,
            elements: Vec::new(),
            note: None,
            pixel_changed: None,
            diff: None,
            package: None,
            first_signal: None,
        };
        let verdict = settle.verdict();
        assert!(verdict.contains("unchanged"), "{verdict}");
        assert!(verdict.contains("screenshot"), "{verdict}");
    }

    #[test]
    fn a_changed_screen_reports_how_long_it_took() {
        let settle = Settle {
            effect: EFFECT_CHANGED,
            elapsed_ms: 420,
            elements: vec![node("android.widget.TextView", Some("a"), (0, 0, 10, 10))],
            note: None,
            pixel_changed: None,
            diff: None,
            package: None,
            first_signal: None,
        };
        let verdict = settle.verdict();
        assert!(verdict.contains("changed"), "{verdict}");
        assert!(verdict.contains("420 ms"), "{verdict}");
        assert!(verdict.contains("1 elements"), "{verdict}");
    }

    // ── Gesture paths ───────────────────────────────────────────────────

    #[test]
    fn a_two_finger_pinch_parses_into_two_paths() {
        let args = serde_json::json!({
            "paths": [
                [[540, 1200], [540, 900]],
                [[540, 1200], [540, 1500]],
            ]
        });
        let paths = parse_gesture_paths(args.get("paths")).expect("paths");
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], vec![(540, 1200), (540, 900)]);
        assert_eq!(paths[1], vec![(540, 1200), (540, 1500)]);
    }

    #[test]
    fn a_gesture_without_paths_is_refused() {
        let args = serde_json::json!({});
        let message = parse_gesture_paths(args.get("paths")).expect_err("refused");
        assert!(message.contains("paths"), "{message}");
    }

    #[test]
    fn a_third_finger_is_refused() {
        let args = serde_json::json!({
            "paths": [
                [[0, 0], [1, 1]],
                [[0, 0], [1, 1]],
                [[0, 0], [1, 1]],
            ]
        });
        let message = parse_gesture_paths(args.get("paths")).expect_err("refused");
        assert!(message.contains("one or two"), "{message}");
    }

    #[test]
    fn a_single_point_is_not_a_gesture() {
        let args = serde_json::json!({ "paths": [[[10, 10]]] });
        let message = parse_gesture_paths(args.get("paths")).expect_err("refused");
        assert!(message.contains("2"), "{message}");
    }

    #[test]
    fn a_point_that_is_not_a_pair_is_named() {
        let args = serde_json::json!({ "paths": [[[10, 10], [20]]] });
        let message = parse_gesture_paths(args.get("paths")).expect_err("refused");
        assert!(message.contains("paths[0][1]"), "{message}");
    }

    #[test]
    fn a_second_display_is_display_zero_only_when_it_is_the_screen() {
        assert_eq!(Target::Screen.display_id(), 0);
        assert_eq!(Target::Display(7).display_id(), 7);
    }

    #[test]
    fn a_pixel_change_overturns_an_unchanged_tree() {
        // The tree said nothing changed; the picture says otherwise. A canvas
        // draws without touching the tree, and that is the case this exists for.
        let before = super::super::display::FrameSignature {
            columns: 2,
            rows: 2,
            cells: vec![0, 0, 0, 0],
        };
        let after = super::super::display::FrameSignature {
            columns: 2,
            rows: 2,
            cells: vec![255, 255, 255, 255],
        };
        assert!(before.differs(&after));
    }

    #[test]
    fn a_settle_without_a_pixel_signal_leaves_it_unknown() {
        let settle = Settle {
            effect: EFFECT_UNCHANGED,
            elapsed_ms: 1200,
            elements: Vec::new(),
            note: None,
            pixel_changed: None,
            diff: None,
            package: None,
            first_signal: None,
        };
        assert_eq!(settle.pixel_changed, None);
        assert_eq!(settle.effect, EFFECT_UNCHANGED);
    }

    // ── Effect classification ──────────────────────────────────────────────

    #[test]
    fn a_row_appearing_is_a_small_change_not_the_screen_changing() {
        let before: Vec<_> = (0..20)
            .map(|i| node("android.widget.TextView", Some("row"), (0, i * 10, 100, i * 10 + 9)))
            .collect();
        let mut after = before.clone();
        // One label changed: the key hashes the text, so it reads as one gone
        // and one new — beneath max(3, 10% of 20) and therefore small.
        after[0] = node("android.widget.TextView", Some("row2"), (0, 0, 100, 9));
        let diff = tree_diff(&before, &after);
        assert_eq!(diff.removed, 1);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.changed(), 2);
        assert_eq!(
            classify_effect(None, None, Some(&diff), after.len(), None, Some(1), Some(2)),
            EFFECT_SMALL_CHANGE
        );
    }

    #[test]
    fn three_changed_elements_are_a_real_change_on_a_short_screen() {
        let before = vec![
            node("android.widget.Button", Some("a"), (0, 0, 100, 50)),
            node("android.widget.Button", Some("b"), (0, 50, 100, 100)),
            node("android.widget.Button", Some("c"), (0, 100, 100, 150)),
        ];
        let after = vec![
            node("android.widget.Button", Some("x"), (0, 0, 100, 50)),
            node("android.widget.Button", Some("y"), (0, 50, 100, 100)),
            node("android.widget.Button", Some("z"), (0, 100, 100, 150)),
        ];
        let diff = tree_diff(&before, &after);
        // Three labels replaced read as three gone and three new: six changed
        // against a threshold of three.
        assert_eq!(diff.changed(), 6);
        assert_eq!(
            classify_effect(None, None, Some(&diff), after.len(), None, Some(1), Some(2)),
            EFFECT_CHANGED
        );
    }

    #[test]
    fn a_moved_element_is_counted_once() {
        let before = vec![node("android.widget.TextView", Some("a"), (0, 0, 100, 50))];
        let after = vec![node("android.widget.TextView", Some("a"), (0, 40, 100, 90))];
        let diff = tree_diff(&before, &after);
        assert_eq!(
            diff,
            TreeDiff {
                added: 0,
                removed: 0,
                moved: 1,
                edited: 0
            }
        );
    }

    #[test]
    fn duplicate_rows_are_counted_as_a_multiset() {
        // Two identical rows, one of them gone: "one left", not "nothing
        // happened because the other one still matches".
        let before = vec![
            node("android.widget.TextView", Some("same"), (0, 0, 100, 50)),
            node("android.widget.TextView", Some("same"), (0, 50, 100, 100)),
        ];
        let after = vec![node(
            "android.widget.TextView",
            Some("same"),
            (0, 0, 100, 50),
        )];
        let diff = tree_diff(&before, &after);
        assert_eq!(diff.removed, 1);
        assert_eq!(diff.added, 0);
    }

    #[test]
    fn text_that_changed_under_the_same_key_is_edited() {
        // A counter going 3 → 4 keeps its resource id and class; only the text
        // moved, and the key is built from it, so this shows up as a pair.
        let before = vec![node("android.widget.TextView", Some("3"), (0, 0, 100, 50))];
        let after = vec![node("android.widget.TextView", Some("4"), (0, 0, 100, 50))];
        let diff = tree_diff(&before, &after);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
    }

    #[test]
    fn a_canvas_that_moved_under_a_motionless_tree_is_a_small_change() {
        assert_eq!(
            classify_effect(
                None,
                None,
                Some(&TreeDiff::default()),
                0,
                Some(true),
                Some(7),
                Some(7)
            ),
            EFFECT_SMALL_CHANGE
        );
        assert_eq!(
            classify_effect(
                None,
                None,
                Some(&TreeDiff::default()),
                0,
                Some(false),
                Some(7),
                Some(7)
            ),
            EFFECT_UNCHANGED
        );
    }

    #[test]
    fn a_new_app_is_a_place_change_even_when_the_tree_changed() {
        assert_eq!(
            classify_effect(
                Some("com.a"),
                Some("com.b"),
                Some(&TreeDiff {
                    added: 9,
                    ..TreeDiff::default()
                }),
                9,
                None,
                Some(1),
                Some(2)
            ),
            EFFECT_PLACE_CHANGED
        );
    }

    #[test]
    fn a_tree_that_was_never_read_before_is_unknown() {
        assert_eq!(classify_effect(None, None, None, 0, None, None, Some(3)), EFFECT_UNKNOWN);
    }

    // ── Raw screencap ──────────────────────────────────────────────────────

    fn raw_frame(width: u32, height: u32, rgba: [u8; 4], header: usize) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes()); // RGBA_8888
        out.extend(std::iter::repeat_n(0u8, header - 12));
        out.extend(std::iter::repeat_n(rgba, (width * height) as usize).flatten());
        out
    }

    #[test]
    fn a_raw_frame_becomes_a_grid() {
        // 120×240 gives 10×10 pixel cells, so every cell is the same colour.
        for header in [12, 16] {
            let frame = raw_frame(120, 240, [255, 0, 0, 255], header);
            let signature = raw_screencap_signature(&frame).expect("a grid");
            assert_eq!((signature.columns, signature.rows), (12, 24));
            assert_eq!(signature.cells.len(), 12 * 24);
            // (3*255 + 0 + 0) / 10 = 76
            assert!(signature.cells.iter().all(|&cell| cell == 76), "{header}");
        }
    }

    #[test]
    fn a_frame_in_another_format_is_refused_rather_than_misread() {
        let mut frame = raw_frame(120, 240, [0, 255, 0, 255], 16);
        // Not RGBA_8888: the byte count matches but the layout does not, which
        // is exactly the case that would silently produce a grid of noise.
        frame[8..12].copy_from_slice(&0x11u32.to_le_bytes());
        assert!(raw_screencap_signature(&frame).is_none());
    }

    #[test]
    fn a_truncated_frame_is_refused() {
        let mut frame = raw_frame(120, 240, [0, 0, 0, 255], 16);
        frame.truncate(frame.len() - 400);
        assert!(raw_screencap_signature(&frame).is_none());
    }

    #[test]
    fn a_frame_too_small_for_the_grid_is_refused() {
        // Fewer columns than grid cells: there is no honest average to take.
        let frame = raw_frame(8, 8, [0, 0, 0, 255], 16);
        assert!(raw_screencap_signature(&frame).is_none());
    }

    #[test]
    fn the_summary_ceiling_truncates_instead_of_dumping_everything() {
        let elements = (0..70)
            .map(|i| node("android.widget.TextView", Some("row"), (0, i * 10, 100, i * 10 + 9)))
            .collect::<Vec<_>>();
        let text = format_ui_summary_limited(&elements, 60);
        assert!(text.contains("10 more element(s)"), "{text}");
        assert!(format_ui_summary_limited(&elements[..3], 60).contains("3 actionable element(s)"));
    }

    #[test]
    fn ocr_lines_carry_tappable_centres() {
        let lines = [OcrLine {
            text: "Settings".to_string(),
            center: (50, 25),
            bounds: (0, 0, 100, 50),
            confidence: 0.9,
        }];
        let text = format_ocr_lines(&lines);
        assert!(text.contains("Settings"), "{text}");
        assert!(text.contains("center=(50,25)"), "{text}");
        assert!(format_ocr_lines(&[]).contains("no text"));
    }

    #[test]
    fn a_bridge_ocr_answer_becomes_tappable_lines() {
        let answer = serde_json::json!({
            "ok": true,
            "width": 1080,
            "height": 2400,
            "lines": [
                { "text": "设置", "bounds": [10, 20, 110, 60], "confidence": 0.93 },
                { "text": "  确定  ", "bounds": [900, 2200, 1060, 2320], "confidence": 0.81 },
            ],
        });
        let lines = parse_ocr_answer(&answer).expect("lines");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "设置");
        assert_eq!(lines[0].center, (60, 40));
        assert_eq!(lines[1].text, "确定");
        // The tap target has to be inside the line's own box.
        let (cx, cy) = lines[1].center;
        assert!(cx > 900 && cx < 1060 && cy > 2200 && cy < 2320);
        assert!((lines[0].confidence - 0.93).abs() < 1e-6);
    }

    #[test]
    fn an_unloadable_engine_is_never_an_empty_screen() {
        // The whole point of the sentinel: an empty list would tell the model
        // the screen is blank, and it would stop looking.
        let answer = serde_json::json!({
            "ok": false,
            "code": "ocr_unavailable",
            "error": "the on-device OCR engine could not be loaded",
        });
        let refused = parse_ocr_answer(&answer).expect_err("no engine");
        assert!(refused.starts_with("ocr_unavailable"), "{refused}");
        assert!(refused.contains("could not be loaded"), "{refused}");
    }

    #[test]
    fn a_success_answer_without_lines_is_malformed_not_blank() {
        let refused =
            parse_ocr_answer(&serde_json::json!({ "ok": true })).expect_err("no line list");
        assert!(refused.starts_with("ocr_unavailable"), "{refused}");
    }

    #[test]
    fn unusable_ocr_lines_are_skipped_but_the_rest_survive() {
        let answer = serde_json::json!({
            "ok": true,
            "lines": [
                { "text": "kept", "bounds": [0, 0, 100, 40] },
                { "text": "   ", "bounds": [0, 0, 100, 40] },
                { "text": "no bounds" },
                { "text": "short bounds", "bounds": [0, 0, 100] },
                { "text": "not numbers", "bounds": ["a", 0, 1, 1] },
                { "text": "flat", "bounds": [10, 10, 10, 40] },
                { "text": "no confidence", "bounds": [0, 50, 100, 90] },
            ],
        });
        let lines = parse_ocr_answer(&answer).expect("lines");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].text, "kept");
        assert_eq!(lines[1].text, "no confidence");
        // Absent confidence is reported as zero, not as a negative sentinel.
        assert_eq!(lines[1].confidence, 0.0);
    }

    #[test]
    fn the_input_tool_documents_the_effect_report() {
        // The model only learns that `verify` exists from the description; a
        // silent removal would make it spend a turn on android_observe again.
        let described = AdbInputTool::new().description().to_string();
        assert!(described.contains("effect"), "{described}");
        assert!(described.contains("verify=false"), "{described}");
    }

    #[test]
    fn test_android_toolset_id() {
        let set = AndroidToolset::new();
        assert_eq!(set.id(), "android");
    }

    #[test]
    fn test_adb_pair_tool_name_and_schema() {
        let tool = AdbPairTool::new();
        assert_eq!(tool.name(), "device_adb_pair");
        let schema = tool.parameters_schema();
        let props = schema.get("properties").unwrap();
        assert!(props.get("port").is_some());
        assert!(props.get("code").is_some());
        assert!(props.get("connect_port").is_some());
    }

    #[test]
    fn test_adb_status_tool_name() {
        let tool = AdbStatusTool::new();
        assert_eq!(tool.name(), "device_adb_status");
    }

    #[test]
    fn test_android_toolset_includes_pairing_tools() {
        let names: Vec<String> = AndroidToolset::new()
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&"device_adb_pair".to_string()));
        assert!(names.contains(&"device_adb_status".to_string()));
    }

    #[test]
    fn test_android_toolset_exposes_the_combined_observe_tool() {
        let names: Vec<String> = AndroidToolset::new()
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&"android_observe".to_string()), "{names:?}");
    }

    // ── Device targeting ────────────────────────────────────────────────
    //
    // A call with no `device` is not "any device": adb resolves it to whichever
    // single device is attached at that moment, so a swap between two calls
    // retargets silently. The argument therefore has to be honoured when given,
    // refused when it cannot be a serial, and the result has to name what was
    // actually addressed.

    #[test]
    fn the_callers_device_argument_beats_the_configured_serial() {
        let args = serde_json::json!({ "device": "emulator-5554" });
        assert_eq!(
            resolve_device(&args, Some("R58M111")).unwrap(),
            Some("emulator-5554".to_string())
        );
    }

    #[test]
    fn omitting_device_falls_back_to_the_configured_serial() {
        assert_eq!(
            resolve_device(&serde_json::json!({}), Some("R58M111")).unwrap(),
            Some("R58M111".to_string())
        );
        assert_eq!(resolve_device(&serde_json::json!({}), None).unwrap(), None);
        // JSON null is an omission, not a serial: that is what a serializer
        // emits for a `None`, so it must fall back like an absent key.
        assert_eq!(
            resolve_device(&serde_json::json!({ "device": null }), Some("R58M111")).unwrap(),
            Some("R58M111".to_string())
        );
    }

    #[test]
    fn a_device_that_is_not_a_serial_is_refused_not_ignored() {
        // Falling back to the configured serial here would act on a device the
        // caller did not name — the exact silent retarget this argument exists
        // to prevent, so it has to fail instead of quietly doing something.
        for args in [
            serde_json::json!({ "device": "" }),
            serde_json::json!({ "device": "   " }),
            serde_json::json!({ "device": 7 }),
            serde_json::json!({ "device": ["emulator-5554"] }),
        ] {
            assert!(resolve_device(&args, Some("R58M111")).is_err(), "{args}");
        }
    }

    #[test]
    fn a_serial_is_trimmed_before_use() {
        let args = serde_json::json!({ "device": " emulator-5554 " });
        assert_eq!(resolve_device(&args, None).unwrap(), Some("emulator-5554".to_string()));
    }

    #[tokio::test]
    async fn a_pinned_serial_is_reported_without_asking_adb() {
        // There is no adb in this test, and the pinned case must not need one:
        // the serial is already known, so it is reported without a round trip.
        assert_eq!(
            resolved_serial(&Some("emulator-5554".to_string())).await,
            Some("emulator-5554".to_string())
        );
    }

    // ── Verification verdicts ────────────────────────────────────────────
    //
    // The tree is the evidence, and there are three answers rather than two:
    // "could not check" is neither a yes nor a no, and folding it into either
    // one invents a fact.

    #[test]
    fn a_verification_answers_with_evidence_from_the_tree() {
        let elements = vec![element(1, Some("Inbox"), "android.widget.TextView")];
        assert_eq!(
            evaluate(&Expectation::TextContains("inbox".into()), &elements),
            VERDICT_SATISFIED,
            "the match is case-insensitive, like the tap path's"
        );
        assert_eq!(
            evaluate(&Expectation::TextContains("Sent".into()), &elements),
            VERDICT_UNSATISFIED
        );
        // An empty screen is not a pass.
        assert_eq!(
            evaluate(&Expectation::TextContains("anything".into()), &[]),
            VERDICT_UNSATISFIED
        );
    }

    #[test]
    fn an_element_that_is_not_there_is_unsatisfied_not_unknown() {
        // The tree was read and the element is not in it: that is an answer.
        let elements = vec![element(1, Some("Inbox"), "android.widget.TextView")];
        assert_eq!(
            evaluate(&Expectation::ElementAt { index: 1, description: None }, &elements),
            VERDICT_SATISFIED
        );
        assert_eq!(
            evaluate(&Expectation::ElementAt { index: 7, description: None }, &elements),
            VERDICT_UNSATISFIED
        );
        // Indices are 1-based, so 0 does not exist.
        assert_eq!(
            evaluate(&Expectation::ElementAt { index: 0, description: None }, &elements),
            VERDICT_UNSATISFIED
        );
    }

    #[test]
    fn a_relabelled_element_fails_the_label_check() {
        let elements = vec![element(1, Some("Inbox"), "android.widget.TextView")];
        assert_eq!(
            evaluate(
                &Expectation::ElementAt {
                    index: 1,
                    description: Some("Sent".into())
                },
                &elements
            ),
            VERDICT_UNSATISFIED
        );
        assert_eq!(
            evaluate(
                &Expectation::ElementAt {
                    index: 1,
                    description: Some("inbo".into())
                },
                &elements
            ),
            VERDICT_SATISFIED
        );
    }

    #[test]
    fn a_verification_must_name_something_to_check() {
        assert!(Expectation::parse(&serde_json::json!({})).is_err());
        assert!(Expectation::parse(&serde_json::json!({ "text": "   " })).is_err());
        assert!(matches!(
            Expectation::parse(&serde_json::json!({ "text": "Inbox" })).unwrap(),
            Expectation::TextContains(_)
        ));
        assert!(matches!(
            Expectation::parse(&serde_json::json!({ "target": 3 })).unwrap(),
            Expectation::ElementAt { index: 3, .. }
        ));
    }
}
