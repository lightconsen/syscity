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

use serde_json::Value;

use super::{has_adb, run_cmd, run_cmd_bytes};
use crate::computer::platform::{OsControlScope, PlatformConstraints, PlatformToolSet};
use crate::tools::Tool;

pub mod app;
pub mod input;
pub mod observe;
pub mod pairing;
pub mod screenshot;
pub mod ui_tree;
pub mod verify;

#[cfg(test)]
mod tests;

pub use app::AdbAppManagerTool;
pub use input::AdbInputTool;
pub use observe::AdbObserveTool;
pub use pairing::{AdbPairTool, AdbStatusTool};
pub use screenshot::AdbScreenshotTool;
pub use ui_tree::AdbUiTreeTool;
pub use verify::AdbVerifyTool;

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
const DEVICE_PARAM: &str =
    "Device serial to act on. Omit it and the call uses the only attached device — the result \
     reports which serial was actually used, since that is whatever is attached at the time.";

/// `adb` argv prefix for the target device — no `-s` when none is pinned, in
/// which case adb resolves the device itself.
fn adb_args(device: &Option<String>, base: &[&str]) -> Vec<String> {
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
fn resolve_device(args: &Value, configured: Option<&str>) -> Result<Option<String>, String> {
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
async fn resolved_serial(device: &Option<String>) -> Option<String> {
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

async fn dump_ui_elements(device: &Option<String>) -> Result<Vec<UiElement>, DumpFailure> {
    Ok(dump_ui(device).await?.elements)
}

/// Dump, pull and parse the tree in one go.
async fn dump_ui(device: &Option<String>) -> Result<UiDump, DumpFailure> {
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
