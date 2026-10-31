//! Android device control via ADB.
//!
//! Provides screenshot, tap, swipe, text input, key events, app installation,
//! launch, force-stop, and a structured UI element list.
//!
//! Observation returns an indexed element list parsed from the platform's
//! `uiautomator dump`, not the raw XML: the dump is an order of magnitude
//! larger, and the model would otherwise have to parse `bounds` to act on it.
//! Text input escapes for the *device* shell (a plain `input text '<text>'`
//! breaks on an embedded quote) and routes non-ASCII through ADBKeyboard, which
//! `input text` cannot type.
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
            Box::new(super::android_tools::AdbDialogTool::new()),
            Box::new(super::android_tools::AdbSystemActionTool::new()),
            Box::new(super::android_tools::AdbOcrTool::new()),
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
pub(super) const DEVICE_PARAM: &str =
    "Device serial to act on. Omit it and the call uses the only attached device — the result \
     reports which serial was actually used, since that is whatever is attached at the time.";

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
        let png = match capture_png(&device).await {
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
         means the tree was read and held nothing actionable."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Observe the Android device",
            serde_json::json!({
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
        let png = match capture_png(&device).await {
            Ok(png) => png,
            Err(e) => return Ok(ToolExecutionResult::error(e.to_string())),
        };
        let as_size = |dims: Option<(u32, u32)>| {
            dims.map(|(w, h)| serde_json::json!({ "width": w, "height": h }))
        };

        let dump = match dump_ui(&device).await {
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

        let shot = png_dimensions(&png);
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

        Ok(ToolExecutionResult::success(summary).with_data(serde_json::json!({
            "count": dump.elements.len(),
            "quality": quality.as_str(),
            "advice": quality.advice(),
            "ocr": ocr_text,
            "elements": dump.elements,
            "screenshot_base64": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &png,
            ),
            "screenshot": as_size(shot),
            "screen": dump.screen.map(|(w, h)| serde_json::json!({ "width": w, "height": h })),
            "coordinates_consistent": consistent,
            "device": resolved_serial(&device).await,
        })))
    }
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

    /// Resolve an index against a tree the caller already read.
    ///
    /// The before-action read serves two purposes — validating the target and
    /// judging the effect — so it is done once, not twice.
    fn tap_point_from(
        elements: &[UiElement],
        index: usize,
        description: Option<&str>,
    ) -> Result<(i32, i32), String> {
        validate_target(elements, index, description).map(|e| e.center)
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
fn input_encode(s: &str) -> String {
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
fn encode_tap_sequence(steps: &[(i64, i64)]) -> String {
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
         the extra reads."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Send input to Android device",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "tap | sequence | swipe | text | key",
                    "enum": ["tap", "sequence", "swipe", "text", "key"]
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
        // One device: two conversations must not type into it at once. See
        // `tools::target_lock`.
        let _target = crate::tools::target_lock::TargetLocks::global()
            .lock(crate::tools::target_lock::ANDROID)
            .await;
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("tap");
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };

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
        let before = match dump_ui_elements(&device).await {
            Ok(elements) => Some(elements),
            Err(failed) => {
                if action == "tap" && args.get("target").is_some() {
                    return Ok(ToolExecutionResult::error(failed.message));
                }
                None
            }
        };

        let shell_cmd = match action {
            "tap" => {
                match args.get("target").and_then(|v| v.as_u64()) {
                    // Preferred: an element index, checked against the read
                    // taken immediately before dispatch.
                    Some(index) => {
                        let description = args.get("target_description").and_then(|v| v.as_str());
                        let elements = before.as_deref().unwrap_or(&[]);
                        match Self::tap_point_from(elements, index as usize, description) {
                            Ok((x, y)) => format!("input tap {} {}", x, y),
                            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                        }
                    }
                    // Raw coordinates: no safety net, the caller is on its own.
                    None => {
                        let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
                        let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
                        format!("input tap {} {}", x, y)
                    }
                }
            }
            "swipe" => {
                let x1 = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
                let y1 = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
                let x2 = args.get("x2").and_then(|v| v.as_i64()).unwrap_or(0);
                let y2 = args.get("y2").and_then(|v| v.as_i64()).unwrap_or(0);
                format!("input swipe {} {} {} {}", x1, y1, x2, y2)
            }
            "text" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if text.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "action 'text' requires a non-empty 'text' argument",
                    ));
                }
                if text.is_ascii() {
                    encode_input_text(text)
                } else if self.adbkeyboard_is_active(&device).await {
                    // ADBKeyboard takes the text base64-encoded over a broadcast,
                    // which carries any character the device can render.
                    let payload = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        text.as_bytes(),
                    );
                    format!("am broadcast -a ADB_INPUT_B64 --es msg '{}'", payload)
                } else {
                    // `input text` types through the device's key character map,
                    // so non-ASCII (CJK in particular) comes out dropped or
                    // mangled. Fail loudly rather than silently typing the wrong
                    // thing — the previous behaviour replaced only spaces and
                    // then sent the text as-is.
                    return Ok(ToolExecutionResult::error(
                        "Cannot type non-ASCII text: `input text` only types ASCII reliably and \
                         ADBKeyboard is not the active input method. Install/enable ADBKeyboard \
                         (com.android.adbkeyboard) and select it as the input method, or keep the \
                         text ASCII.",
                    ));
                }
            }
            "sequence" => match parse_tap_steps(args.get("steps")) {
                Ok(steps) => encode_tap_sequence(&steps),
                Err(msg) => return Ok(ToolExecutionResult::error(msg)),
            },
            "key" => {
                let keycode = args
                    .get("keycode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("HOME");
                format!("input keyevent {}", keycode)
            }
            _ => return Ok(ToolExecutionResult::error(format!("Unknown action: {}", action))),
        };

        let argv = adb_args(&device, &["shell", &shell_cmd]);
        let (status, _stdout, stderr) =
            run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: "adb input failed".to_string(),
                    cause: Some(Box::new(e)),
                })?;

        if !status.success() {
            return Ok(ToolExecutionResult::error(format!("adb input failed: {}", stderr)));
        }

        let serial = resolved_serial(&device).await;
        if !verify {
            return Ok(ToolExecutionResult::success(format!(
                "Input '{action}' sent (effect not checked)."
            ))
            .with_data(serde_json::json!({
                "device": serial,
                "effect": "unverified",
            })));
        }

        // Judge the effect here rather than leaving it to the model: a dispatch
        // that was accepted says nothing about whether the tap landed.
        let before_fingerprint = before.as_ref().map(|elements| tree_fingerprint(elements));
        let settle = settle_after_action(&device, before_fingerprint, settle_budget).await;

        let mut text = format!("Input '{action}' sent. {}", settle.verdict());
        if !settle.elements.is_empty() {
            text.push_str("\n\n");
            text.push_str(&format_ui_summary_limited(&settle.elements, 60));
        }

        Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
            "device": serial,
            "effect": settle.effect,
            "settled_ms": settle.elapsed_ms,
            "elements": settle.elements,
        })))
    }
}

// ── ADB App Manager Tool ───────────────────────────────────────────────────

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
}

#[async_trait]
impl Tool for AdbAppManagerTool {
    fn name(&self) -> &str {
        "android_app_manager"
    }

    fn description(&self) -> &str {
        "Install, launch, force-stop, or list apps on an Android device. 'completed' means the \
         adb command was dispatched, not that the app started — observe afterwards to see."
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
        // One device: two conversations must not type into it at once. See
        // `tools::target_lock`.
        let _target = crate::tools::target_lock::TargetLocks::global()
            .lock(crate::tools::target_lock::ANDROID)
            .await;
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };

        let argv = match action {
            "install" => {
                let path = args.get("apk_path").and_then(|v| v.as_str()).unwrap_or("");
                adb_args(&device, &["install", "-r", path])
            }
            "launch" => {
                let pkg = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                let activity = args.get("activity").and_then(|v| v.as_str());
                let component = match activity {
                    Some(a) => format!("{}/{}", pkg, a),
                    None => format!("{}/.MainActivity", pkg),
                };
                adb_args(&device, &["shell", "am", "start", "-n", &component])
            }
            "force_stop" => {
                let pkg = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                adb_args(&device, &["shell", "am", "force-stop", pkg])
            }
            "list_packages" => adb_args(&device, &["shell", "pm", "list", "packages"]),
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

/// One node of the Android accessibility tree, flattened for the model.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct UiElement {
    /// 1-based index the model refers to this element by.
    pub index: usize,
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
    /// `(x1, y1, x2, y2)` in device pixels.
    pub bounds: (i32, i32, i32, i32),
    /// Centre of `bounds` — what a tap should target.
    pub center: (i32, i32),
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
/// content description or a resource id, plus clickable ones. Bare layout
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
        let text = attr("text");
        let content_desc = attr("content-desc");
        let resource_id = attr("resource-id");
        let clickable = attr("clickable").as_deref() == Some("true");
        let scrollable = attr("scrollable").as_deref() == Some("true");

        let actionable = clickable || scrollable;
        if text.is_none() && content_desc.is_none() && resource_id.is_none() && !actionable {
            continue;
        }
        let Some(bounds) = attr("bounds").as_deref().and_then(parse_bounds) else {
            continue;
        };

        let class = attr("class")
            .map(|c| c.rsplit('.').next().unwrap_or(&c).to_string())
            .unwrap_or_default();

        out.push(UiElement {
            index: out.len() + 1,
            text,
            content_desc,
            resource_id,
            class,
            clickable,
            enabled: attr("enabled").as_deref() != Some("false"),
            center: ((bounds.0 + bounds.2) / 2, (bounds.1 + bounds.3) / 2),
            bounds,
        });
    }
    Ok(out)
}

/// Capture the device screen as raw PNG bytes.
///
/// Uses the byte-safe runner: the PNG has to reach the encoder exactly as the
/// device produced it, and the `String` variant of `run_cmd` would replace every
/// non-UTF-8 byte with U+FFFD and corrupt the image.
async fn capture_png(device: &Option<String>) -> crate::Result<Vec<u8>> {
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

/// The screen rectangle a dump describes: the root `<node>`'s bounds.
///
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

/// Width and height from a PNG's IHDR chunk.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
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
    for e in elements {
        let label = e
            .text
            .as_deref()
            .or(e.content_desc.as_deref())
            .map(|s| format!("{s:?}"))
            .or_else(|| e.resource_id.clone())
            .unwrap_or_else(|| "-".to_string());
        let mut flags = String::new();
        if e.clickable {
            flags.push_str(" clickable");
        }
        if !e.enabled {
            flags.push_str(" disabled");
        }
        lines.push(format!(
            "[{}] {} {}{} center=({},{})",
            e.index, e.class, label, flags, e.center.0, e.center.1
        ));
    }
    lines.join("\n")
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
pub fn assess_tree(elements: &[UiElement]) -> TreeQuality {
    if elements.is_empty() {
        return TreeQuality::Unusable;
    }
    let named = elements
        .iter()
        .filter(|e| element_label(e).is_some())
        .count();
    if named == 0 {
        return TreeQuality::Unusable;
    }
    let share = named as f32 / elements.len() as f32;
    if named < 3 || share < 0.25 {
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

// ── Action effect ──────────────────────────────────────────────────────────

/// The screen was different after the action.
pub const EFFECT_CHANGED: &str = "changed";
/// The screen was the same after the action.
pub const EFFECT_UNCHANGED: &str = "unchanged";
/// The effect could not be judged — the tree could not be read.
pub const EFFECT_UNKNOWN: &str = "unknown";

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
}

impl Settle {
    /// A one-line verdict for the tool result.
    pub fn verdict(&self) -> String {
        match self.effect {
            EFFECT_CHANGED => format!(
                "The screen changed (settled in {} ms, {} elements now).",
                self.elapsed_ms,
                self.elements.len()
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
    before: Option<u64>,
    budget_ms: u64,
) -> Settle {
    let started = std::time::Instant::now();
    let budget = budget_ms.clamp(SETTLE_MIN_MS, SETTLE_MAX_MS);
    let mut previous: Option<u64> = None;
    let mut changed = false;
    let mut last: Option<Vec<UiElement>> = None;
    let mut failure: Option<String> = None;

    loop {
        match dump_ui_elements(device).await {
            Ok(elements) => {
                let fingerprint = tree_fingerprint(&elements);
                if before.is_some_and(|before| before != fingerprint) {
                    changed = true;
                }
                let stable = previous == Some(fingerprint);
                previous = Some(fingerprint);
                last = Some(elements);
                if stable && started.elapsed().as_millis() as u64 >= SETTLE_MIN_MS {
                    break;
                }
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
        Some(elements) => Settle {
            effect: match (before, changed) {
                (None, _) => EFFECT_UNKNOWN,
                (Some(_), true) => EFFECT_CHANGED,
                (Some(_), false) => EFFECT_UNCHANGED,
            },
            elapsed_ms,
            elements,
            note: None,
        },
        None => Settle {
            effect: EFFECT_UNKNOWN,
            elapsed_ms,
            elements: Vec::new(),
            note: failure,
        },
    }
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

/// Read text out of a screenshot in a build without an OCR engine.
///
/// Says so rather than returning an empty list, which would read like "the
/// screen is blank" and send the model looking for a problem that is not there.
#[cfg(not(feature = "vision"))]
pub async fn ocr_png(_png: &[u8]) -> Result<Vec<OcrLine>, String> {
    Err("this build has no OCR engine (compiled without the `vision` feature)".to_string())
}

/// Read text out of the device's current screen.
pub async fn ocr_screen(device: &Option<String>) -> Result<Vec<OcrLine>, String> {
    let png = capture_png(device).await.map_err(|e| e.to_string())?;
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
fn label_matches(label: &str, description: &str) -> bool {
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
fn validate_target<'a>(
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
}

pub(super) async fn dump_ui_elements(
    device: &Option<String>,
) -> Result<Vec<UiElement>, DumpFailure> {
    Ok(dump_ui(device).await?.elements)
}

/// Dump, pull and parse the tree in one go.
pub(super) async fn dump_ui(device: &Option<String>) -> Result<UiDump, DumpFailure> {
    let xml = pull_ui_xml(device).await?;
    let elements = parse_uiautomator_xml(&xml).map_err(|source| {
        DumpFailure::new(
            DUMP_FAILED,
            format!("Could not read the UI tree: the dump did not parse ({source})"),
        )
    })?;
    Ok(UiDump {
        elements,
        screen: parse_screen_bounds(&xml),
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
         act on the OCR text or the coordinates it gives."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Get Android UI tree",
            serde_json::json!({
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
        let elements = match dump_ui_elements(&device).await {
            Ok(e) => e,
            Err(e) => {
                return Ok(ToolExecutionResult::error(e.message)
                    .with_data(serde_json::json!({ "code": e.code })))
            }
        };
        let quality = assess_tree(&elements);
        let mut summary = format_ui_summary(&elements);
        let mut ocr_text = None;
        // When the tree describes little or nothing, reading the screenshot is
        // the only way to say what is actually on the screen — and saying it
        // here is what stops the model from re-reading the tree forever.
        if quality != TreeQuality::Usable {
            match ocr_screen(&device).await {
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

        match dump_ui_elements(&device).await {
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
        assert_eq!(els.len(), 4, "{classes:?}");
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
        assert!(summary.starts_with("4 actionable element(s):"), "{summary}");
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
        UiElement {
            index,
            text: text.map(str::to_string),
            content_desc: None,
            resource_id: None,
            class: class.to_string(),
            clickable: true,
            enabled: true,
            bounds: (0, 0, 10, 10),
            center: (5, 5),
        }
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
        UiElement {
            index: 1,
            text: text.map(str::to_string),
            content_desc: None,
            resource_id: None,
            class: class.to_string(),
            clickable: true,
            enabled: true,
            bounds,
            center: ((bounds.0 + bounds.2) / 2, (bounds.1 + bounds.3) / 2),
        }
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
        };
        let verdict = settle.verdict();
        assert!(verdict.starts_with("Effect unknown"), "{verdict}");
        assert!(verdict.contains("SCREEN_NEVER_IDLE"), "{verdict}");
    }

    #[test]
    fn an_unchanged_screen_points_at_the_screenshot() {
        let settle = Settle {
            effect: EFFECT_UNCHANGED,
            elapsed_ms: 1200,
            elements: Vec::new(),
            note: None,
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
        };
        let verdict = settle.verdict();
        assert!(verdict.contains("changed"), "{verdict}");
        assert!(verdict.contains("420 ms"), "{verdict}");
        assert!(verdict.contains("1 elements"), "{verdict}");
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
