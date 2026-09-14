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
                "device": {
                    "type": "string",
                    "description": "Optional device serial number",
                }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let png = match capture_png(&self.device).await {
            Ok(png) => png,
            Err(e) => return Ok(ToolExecutionResult::error(e.to_string())),
        };
        let base64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);

        Ok(
            ToolExecutionResult::success("Screenshot captured").with_data(serde_json::json!({
                "base64": base64,
                "format": "png",
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
         disagree about the screen size, which would make the element coordinates unusable."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Observe the Android device",
            serde_json::json!({
                "device": { "type": "string", "description": "Optional device serial" }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let png = match capture_png(&self.device).await {
            Ok(png) => png,
            Err(e) => return Ok(ToolExecutionResult::error(e.to_string())),
        };
        let dump = match dump_ui(&self.device).await {
            Ok(dump) => dump,
            Err(e) => return Ok(ToolExecutionResult::error(e.to_string())),
        };

        let shot = png_dimensions(&png);
        let consistent = coordinate_space_consistent(dump.screen, shot);

        let mut summary = format_ui_summary(&dump.elements);
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

        let as_size = |dims: Option<(u32, u32)>| {
            dims.map(|(w, h)| serde_json::json!({ "width": w, "height": h }))
        };
        Ok(ToolExecutionResult::success(summary).with_data(serde_json::json!({
            "count": dump.elements.len(),
            "elements": dump.elements,
            "screenshot_base64": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &png,
            ),
            "screenshot": as_size(shot),
            "screen": dump.screen.map(|(w, h)| serde_json::json!({ "width": w, "height": h })),
            "coordinates_consistent": consistent,
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

    fn adb_args(&self, base: &[&str]) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(d) = &self.device {
            args.push("-s".to_string());
            args.push(d.clone());
        }
        for a in base {
            args.push(a.to_string());
        }
        args
    }

    /// Whether the device's active input method is ADBKeyboard.
    ///
    /// ADBKeyboard accepts text over a broadcast, which is the only way to type
    /// non-ASCII without a UI: `input text` goes through the device's key
    /// character map and drops or mangles anything outside ASCII (CJK in
    /// particular). Costs one extra `adb` round trip, so it is only consulted
    /// for text that actually needs it.
    async fn adbkeyboard_is_active(&self) -> bool {
        let args = self.adb_args(&["shell", "settings", "get", "secure", "default_input_method"]);
        match run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>()).await {
            Ok((status, stdout, _)) if status.success() => {
                stdout.to_lowercase().contains("adbkeyboard")
            }
            _ => false,
        }
    }

    /// Re-read the tree and turn an element index into a tap point.
    ///
    /// Costs one extra dump, which is the point: tapping the coordinates from an
    /// earlier read silently hits whatever has moved into place since.
    async fn resolve_tap_target(
        &self,
        index: usize,
        description: Option<&str>,
    ) -> Result<(i32, i32), String> {
        let elements = dump_ui_elements(&self.device)
            .await
            .map_err(|e| e.to_string())?;
        validate_target(&elements, index, description).map(|e| e.center)
    }
}

/// Quote a string as a single shell word for the *device* shell.
///
/// A single-quoted word may not contain a quote, so an embedded `'` has to close
/// the word, emit an escaped quote and reopen it (`'\''`) — the only escape a
/// single-quoted shell word supports. Without this a quote in the text ends the
/// command early and everything after it is parsed as further commands.
fn sh_quote(s: &str) -> String {
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
         need to hit something that will disappear before your next turn."
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
                "device": { "type": "string", "description": "Optional device serial" }
            }),
            vec!["action"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("tap");

        let shell_cmd = match action {
            "tap" => {
                match args.get("target").and_then(|v| v.as_u64()) {
                    // Preferred: an element index, re-checked against the live
                    // screen before anything is dispatched.
                    Some(index) => {
                        let description = args.get("target_description").and_then(|v| v.as_str());
                        match self.resolve_tap_target(index as usize, description).await {
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
                } else if self.adbkeyboard_is_active().await {
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

        let adb_args = self.adb_args(&["shell", &shell_cmd]);
        let (status, _stdout, stderr) =
            run_cmd("adb", &adb_args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: "adb input failed".to_string(),
                    cause: Some(Box::new(e)),
                })?;

        if !status.success() {
            return Ok(ToolExecutionResult::error(format!("adb input failed: {}", stderr)));
        }

        Ok(ToolExecutionResult::success(format!("Input '{}' sent", action)))
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

    fn adb_args(&self, base: &[&str]) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(d) = &self.device {
            args.push("-s".to_string());
            args.push(d.clone());
        }
        for a in base {
            args.push(a.to_string());
        }
        args
    }
}

#[async_trait]
impl Tool for AdbAppManagerTool {
    fn name(&self) -> &str {
        "android_app_manager"
    }

    fn description(&self) -> &str {
        "Install, launch, force-stop, or list apps on an Android device."
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
                "device": { "type": "string", "description": "Optional device serial" }
            }),
            vec!["action"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");

        let adb_args = match action {
            "install" => {
                let path = args.get("apk_path").and_then(|v| v.as_str()).unwrap_or("");
                self.adb_args(&["install", "-r", path])
            }
            "launch" => {
                let pkg = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                let activity = args.get("activity").and_then(|v| v.as_str());
                let component = match activity {
                    Some(a) => format!("{}/{}", pkg, a),
                    None => format!("{}/.MainActivity", pkg),
                };
                self.adb_args(&["shell", "am", "start", "-n", &component])
            }
            "force_stop" => {
                let pkg = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                self.adb_args(&["shell", "am", "force-stop", pkg])
            }
            "list_packages" => self.adb_args(&["shell", "pm", "list", "packages"]),
            _ => return Ok(ToolExecutionResult::error(format!("Unknown action: {}", action))),
        };

        let (status, stdout, stderr) =
            run_cmd("adb", &adb_args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: "adb app manager failed".to_string(),
                    cause: Some(Box::new(e)),
                })?;

        if !status.success() {
            return Ok(ToolExecutionResult::error(format!("adb app manager failed: {}", stderr)));
        }

        Ok(ToolExecutionResult::success(format!(
            "Action '{}' completed\n{}",
            action, stdout
        )))
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
    let mut args: Vec<String> = Vec::new();
    if let Some(serial) = device {
        args.push("-s".to_string());
        args.push(serial.clone());
    }
    args.extend(
        ["exec-out", "screencap", "-p"]
            .iter()
            .map(|s| s.to_string()),
    );

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

/// Turn a `uiautomator dump` failure into something actionable.
///
/// Its stderr is cryptic and looks the same across causes that need completely
/// different responses — a device that is gone, a screen that never settles, and
/// a secure window all just "fail". The fallback is always available though: the
/// screenshot still works when the element tree does not.
fn classify_dump_failure(stderr: &str) -> String {
    let lowered = stderr.to_lowercase();
    // adb puts the serial in the middle: `device 'emulator-5554' not found`.
    let device_gone = (lowered.contains("device") && lowered.contains("not found"))
        || lowered.contains("device offline")
        || lowered.contains("no devices/emulators found")
        || lowered.contains("unauthorized");
    let symptom = if device_gone {
        "the device is not connected (or has gone offline) — reconnect it and try again"
    } else if lowered.contains("could not get idle state") {
        "the screen never settled (something on it keeps animating)"
    } else if lowered.contains("null root node") || lowered.contains("could not get root node") {
        "no accessibility root node was available — likely a secure window (a password or \
         banking screen), or another app is holding the UiAutomation connection"
    } else if lowered.contains("permission denial") {
        "the accessibility service is not enabled"
    } else {
        "uiautomator reported an error"
    };

    let raw = stderr.trim();
    if raw.is_empty() {
        format!(
            "Could not read the UI tree: {symptom}. Work from a screenshot instead — \
             android_observe returns one alongside the tree."
        )
    } else {
        format!(
            "Could not read the UI tree: {symptom}. Raw output: {raw}. Work from a screenshot \
             instead — android_observe returns one alongside the tree."
        )
    }
}

/// One line per element, for the model to read directly.
fn format_ui_summary(elements: &[UiElement]) -> String {
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

async fn dump_ui_elements(device: &Option<String>) -> crate::Result<Vec<UiElement>> {
    Ok(dump_ui(device).await?.elements)
}

/// Dump, pull and parse the tree in one go.
async fn dump_ui(device: &Option<String>) -> crate::Result<UiDump> {
    let xml = pull_ui_xml(device).await?;
    let elements = parse_uiautomator_xml(&xml)
        .map_err(|source| crate::error::SyscityError::ExternalService { source, cause: None })?;
    Ok(UiDump {
        elements,
        screen: parse_screen_bounds(&xml),
    })
}

/// Run `uiautomator dump` on the device and pull the file back.
async fn pull_ui_xml(device: &Option<String>) -> crate::Result<String> {
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
    let (status, _, stderr) =
        run(dump)
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: "adb uiautomator dump failed".to_string(),
                cause: Some(Box::new(e)),
            })?;
    if !status.success() {
        return Err(crate::error::SyscityError::ExternalService {
            source: classify_dump_failure(&stderr),
            cause: None,
        });
    }

    let pull = argv(&["pull", "/sdcard/window_dump.xml", "-"]);
    let (status, stdout, stderr) =
        run(pull)
            .await
            .map_err(|e| crate::error::SyscityError::ExternalService {
                source: "adb pull failed".to_string(),
                cause: Some(Box::new(e)),
            })?;
    if !status.success() {
        return Err(crate::error::SyscityError::ExternalService {
            source: format!("adb pull failed: {stderr}"),
            cause: None,
        });
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
         containers are omitted."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Get Android UI tree",
            serde_json::json!({
                "device": { "type": "string", "description": "Optional device serial" }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        _args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // The raw dump is an order of magnitude larger than what the model can
        // use, and the model would have to parse `bounds` itself to act on it.
        // Hand back an indexed list instead; the XML stays out of the context.
        let elements = match dump_ui_elements(&self.device).await {
            Ok(e) => e,
            Err(e) => return Ok(ToolExecutionResult::error(e.to_string())),
        };
        let summary = format_ui_summary(&elements);
        Ok(ToolExecutionResult::success(summary).with_data(serde_json::json!({
            "count": elements.len(),
            "elements": elements,
        })))
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
        let offline = classify_dump_failure("error: device 'emulator-5554' not found");
        assert!(offline.contains("not connected"), "{offline}");

        let busy = classify_dump_failure("ERROR: could not get idle state.");
        assert!(busy.contains("never settled"), "{busy}");

        let secure =
            classify_dump_failure("ERROR: null root node returned by UiTestAutomationBridge.");
        assert!(secure.contains("secure window"), "{secure}");

        let disabled = classify_dump_failure("java.lang.SecurityException: Permission Denial");
        assert!(disabled.contains("not enabled"), "{disabled}");
    }

    #[test]
    fn every_dump_failure_points_at_the_screenshot() {
        for stderr in [
            "device offline",
            "ERROR: could not get idle state.",
            "something nobody has seen before",
            "",
        ] {
            let msg = classify_dump_failure(stderr);
            assert!(msg.contains("screenshot"), "{msg}");
            assert!(msg.contains("android_observe"), "{msg}");
        }
    }

    #[test]
    fn raw_output_is_kept_when_there_is_any() {
        assert!(classify_dump_failure("weird failure").contains("weird failure"));
        assert!(!classify_dump_failure("   ").contains("Raw output"));
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
}
