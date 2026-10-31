//! Higher-level Android tools built on the same ADB reads as `android.rs`.
//!
//! Three of them exist to remove round trips rather than to add capability:
//! `android_collect` reads a long list screen by screen, `android_find` scrolls
//! to a label the model already knows it wants, and `android_dialog` reads the
//! dialog that is in the way instead of making the model describe it. The fourth
//! is `android_system_action`, which goes through Android's own intents for
//! alarms, timers, calendar events, shares and settings — those are one stable
//! call instead of a sequence of taps whose layout changes between OEM builds.
//!
//! The last is `android_ocr`, the screenshot reader, for the case where the
//! accessibility tree describes nothing.

use async_trait::async_trait;
use serde_json::Value;

use super::android::{
    adb_args, assess_tree, dump_ui, element_label, format_ocr_lines, format_ui_summary, ocr_screen,
    resolve_device, resolved_serial, settle_after_action, sh_quote, tree_fingerprint, TreeQuality,
    UiElement, DEVICE_PARAM, EFFECT_UNCHANGED,
};
use super::run_cmd;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

// ── Shared helpers ─────────────────────────────────────────────────────────

/// Which way a scroll moves the content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Down,
    Up,
}

impl Direction {
    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("down").to_ascii_lowercase().as_str() {
            "down" | "forward" => Ok(Self::Down),
            "up" | "back" | "backward" => Ok(Self::Up),
            other => Err(format!("direction must be 'down' or 'up', not {other:?}")),
        }
    }
}

/// The device's screen size, from the dump or from `wm size`.
async fn screen_size(device: &Option<String>, from_dump: Option<(i32, i32)>) -> Option<(i32, i32)> {
    if let Some(size) = from_dump {
        return Some(size);
    }
    let args = adb_args(device, &["shell", "wm", "size"]);
    let (status, stdout, _) = run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .await
        .ok()?;
    if !status.success() {
        return None;
    }
    // `Physical size: 1080x2340` (an `Override size:` line may follow).
    stdout.lines().find_map(|line| {
        let value = line.split(':').nth(1)?.trim();
        let (width, height) = value.split_once('x')?;
        Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
    })
}

/// Scroll one screenful with a swipe down the middle of the display.
///
/// A swipe rather than a scrollable-node action: the node is often the list, but
/// just as often the list is custom-drawn and the scrollable node is a container
/// the swipe is more reliable against.
async fn scroll_once(
    device: &Option<String>,
    size: (i32, i32),
    direction: Direction,
) -> Result<(), String> {
    let (width, height) = size;
    let x = width / 2;
    let (from_y, to_y) = match direction {
        Direction::Down => (height * 3 / 4, height / 4),
        Direction::Up => (height / 4, height * 3 / 4),
    };
    let command = format!("input swipe {x} {from_y} {x} {to_y} 300");
    let args = adb_args(device, &["shell", &command]);
    let (status, _, stderr) = run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .await
        .map_err(|e| format!("could not run adb to scroll: {e}"))?;
    if !status.success() {
        return Err(format!("scrolling failed: {}", stderr.trim()));
    }
    Ok(())
}

/// Labels of the elements a person could read on this screen.
fn labels(elements: &[UiElement]) -> Vec<String> {
    elements
        .iter()
        .filter_map(|element| element_label(element).map(str::to_string))
        .collect()
}

// ── Collect ────────────────────────────────────────────────────────────────

/// Walk a scrollable screen and return everything readable on it.
#[derive(Debug)]
pub struct AdbCollectTool {
    device: Option<String>,
}

impl Default for AdbCollectTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbCollectTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbCollectTool {
    fn name(&self) -> &str {
        "android_collect"
    }

    fn description(&self) -> &str {
        "Read everything on a long screen (a chat, a list, settings) by scrolling and merging. \
         One call replaces a read-scroll-read loop: it returns the text in screen order, with \
         the reason it stopped. Use it for 'message me everything', 'what are my orders' and \
         anything where the answer is spread over several screens."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Collect the readable text of a scrollable screen",
            serde_json::json!({
                "max_screens": {
                    "type": "integer",
                    "description": "How many screens to read at most (default 5, max 15)."
                },
                "direction": {
                    "type": "string",
                    "description": "down (default) reads towards older/newer content; up reverses.",
                    "enum": ["down", "up"]
                },
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
        let _target = crate::tools::target_lock::TargetLocks::global()
            .lock(crate::tools::target_lock::ANDROID)
            .await;
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let max_screens = args
            .get("max_screens")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 15) as usize;
        let direction = match args.get("direction").and_then(|v| v.as_str()) {
            Some(value) => match Direction::parse(Some(value)) {
                Ok(direction) => direction,
                Err(message) => return Ok(ToolExecutionResult::error(message)),
            },
            None => Direction::Down,
        };

        let mut seen = std::collections::HashSet::new();
        let mut screens: Vec<Vec<String>> = Vec::new();
        let mut size: Option<(i32, i32)> = None;
        let mut stop = "reached max_screens";

        for index in 0..max_screens {
            let dump = match dump_ui(&device).await {
                Ok(dump) => dump,
                Err(failure) => {
                    if screens.is_empty() {
                        return Ok(ToolExecutionResult::error(failure.message)
                            .with_data(serde_json::json!({ "code": failure.code })));
                    }
                    stop = "the tree stopped being readable";
                    break;
                }
            };
            if index == 0 && assess_tree(&dump.elements) == TreeQuality::Unusable {
                return Ok(ToolExecutionResult::success(
                    "android_collect needs the accessibility tree, and this screen has none — a \
                     game, a canvas, or a secure surface. Use android_observe (which falls back to \
                     OCR) and read the screenshot instead."
                        .to_string(),
                )
                .with_data(serde_json::json!({ "collected": 0, "quality": "unusable" })));
            }
            size = screen_size(&device, dump.screen).await.or(size);

            let fresh: Vec<String> = labels(&dump.elements)
                .into_iter()
                .filter(|label| seen.insert(label.clone()))
                .collect();
            if index > 0 && fresh.is_empty() {
                stop = "nothing new appeared";
                break;
            }
            screens.push(fresh);

            if index + 1 >= max_screens {
                break;
            }
            let Some(size) = size else {
                stop = "the screen size is unknown, so it cannot be scrolled";
                break;
            };
            if let Err(message) = scroll_once(&device, size, direction).await {
                stop = "scrolling failed";
                if screens.len() == 1 {
                    return Ok(ToolExecutionResult::error(message));
                }
                break;
            }
            let before = tree_fingerprint(&dump.elements);
            let settle = settle_after_action(&device, Some(before), 900).await;
            if settle.effect == EFFECT_UNCHANGED {
                stop = "the screen stopped moving";
                break;
            }
        }

        let total: usize = screens.iter().map(Vec::len).sum();
        let mut text = format!(
            "Read {total} line(s) over {} screen(s); stopped because {stop}.",
            screens.len()
        );
        if screens.len() == max_screens {
            text.push_str(" If the answer may continue, call again with a larger max_screens.");
        }
        for (index, screen) in screens.iter().enumerate() {
            text.push_str(&format!("\n\n── screen {} ──", index + 1));
            for line in screen {
                text.push('\n');
                text.push_str(line);
            }
        }

        Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
            "screens": screens,
            "stop_reason": stop,
            "device": resolved_serial(&device).await,
        })))
    }
}

// ── Find ───────────────────────────────────────────────────────────────────

/// Scroll until a label appears, and return where it is.
#[derive(Debug)]
pub struct AdbFindTool {
    device: Option<String>,
}

impl Default for AdbFindTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbFindTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbFindTool {
    fn name(&self) -> &str {
        "android_find"
    }

    fn description(&self) -> &str {
        "Scroll until an element with this text appears and return its coordinates, so you can \
         act without reading every screen in between. Matching is case-insensitive and looks at \
         the element's text or content description. Says plainly when it was not found, and how \
         far it looked."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Find an element by scrolling",
            serde_json::json!({
                "text": {
                    "type": "string",
                    "description": "Text the element shows, or its content description."
                },
                "max_scrolls": {
                    "type": "integer",
                    "description": "How many screens to scroll before giving up (default 8, max 30)."
                },
                "direction": {
                    "type": "string",
                    "description": "down (default) or up.",
                    "enum": ["down", "up"]
                },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            vec!["text"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let _target = crate::tools::target_lock::TargetLocks::global()
            .lock(crate::tools::target_lock::ANDROID)
            .await;
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let needle = args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase();
        if needle.is_empty() {
            return Ok(ToolExecutionResult::error("'text' must be non-empty"));
        }
        let max_scrolls = args
            .get("max_scrolls")
            .and_then(|v| v.as_u64())
            .unwrap_or(8)
            .clamp(0, 30) as usize;
        let direction = match args.get("direction").and_then(|v| v.as_str()) {
            Some(value) => match Direction::parse(Some(value)) {
                Ok(direction) => direction,
                Err(message) => return Ok(ToolExecutionResult::error(message)),
            },
            None => Direction::Down,
        };

        let mut size: Option<(i32, i32)> = None;
        for step in 0..=max_scrolls {
            let dump = match dump_ui(&device).await {
                Ok(dump) => dump,
                Err(failure) => {
                    return Ok(ToolExecutionResult::error(failure.message)
                        .with_data(serde_json::json!({ "code": failure.code })))
                }
            };
            size = screen_size(&device, dump.screen).await.or(size);

            if let Some(found) = dump.elements.iter().find(|element| {
                element_label(element).is_some_and(|label| label.to_lowercase().contains(&needle))
            }) {
                return Ok(ToolExecutionResult::success(format!(
                    "Found after {} scroll(s): [{}] {} \"{}\" center=({},{}). Tap it with \
                     android_input using these coordinates, or re-read with android_ui_tree for \
                     its current index.",
                    step,
                    found.index,
                    found.class,
                    element_label(found).unwrap_or("-"),
                    found.center.0,
                    found.center.1
                ))
                .with_data(serde_json::json!({
                    "found": found,
                    "scrolls": step,
                    "device": resolved_serial(&device).await,
                })));
            }

            if step == max_scrolls {
                break;
            }
            let Some(size) = size else {
                return Ok(ToolExecutionResult::error(
                    "the screen size is unknown, so it cannot be scrolled; scroll by hand with \
                     android_input action 'swipe' and read again",
                ));
            };
            if let Err(message) = scroll_once(&device, size, direction).await {
                return Ok(ToolExecutionResult::error(message));
            }
            let before = tree_fingerprint(&dump.elements);
            let settle = settle_after_action(&device, Some(before), 900).await;
            if settle.effect == EFFECT_UNCHANGED {
                return Ok(ToolExecutionResult::success(format!(
                    "Not found: the screen stopped moving after {step} scroll(s), so there is \
                     nothing more in this direction. It may be on another tab or page."
                ))
                .with_data(serde_json::json!({
                    "found": Value::Null,
                    "scrolls": step,
                    "device": resolved_serial(&device).await,
                })));
            }
        }

        Ok(ToolExecutionResult::success(format!(
            "Not found after {max_scrolls} scroll(s). The list may have ended, or the text may \
             be drawn rather than exposed — read the screen with android_observe and look at the \
             screenshot."
        ))
        .with_data(serde_json::json!({
            "found": Value::Null,
            "scrolls": max_scrolls,
            "device": resolved_serial(&device).await,
        })))
    }
}

// ── Dialog ─────────────────────────────────────────────────────────────────

/// Read the dialog that is blocking the screen, optionally pressing one button.
#[derive(Debug)]
pub struct AdbDialogTool {
    device: Option<String>,
}

impl Default for AdbDialogTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbDialogTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// Classes that mean "this node is (part of) a dialog".
fn is_dialog_class(class: &str) -> bool {
    let lowered = class.to_ascii_lowercase();
    lowered.contains("dialog") || lowered.contains("popupwindow") || lowered.contains("alert")
}

#[async_trait]
impl Tool for AdbDialogTool {
    fn name(&self) -> &str {
        "android_dialog"
    }

    fn description(&self) -> &str {
        "Read the dialog on screen — its text and its buttons — and optionally press one. Pass \
         `button` only when you know which one: it is pressed only if exactly one button matches, \
         which is what stops a dialog from being dismissed with the wrong answer. Without \
         `button` this only reads."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Read (and optionally answer) a dialog",
            serde_json::json!({
                "button": {
                    "type": "string",
                    "description": "Button text to press. Refused unless exactly one button on the dialog matches."
                },
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
        let _target = crate::tools::target_lock::TargetLocks::global()
            .lock(crate::tools::target_lock::ANDROID)
            .await;
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let dump = match dump_ui(&device).await {
            Ok(dump) => dump,
            Err(failure) => {
                return Ok(ToolExecutionResult::error(failure.message)
                    .with_data(serde_json::json!({ "code": failure.code })))
            }
        };

        // A dialog is reported by its own node classes; the buttons are the
        // clickable, labelled elements sitting inside those nodes' rectangle.
        let roots: Vec<&UiElement> = dump
            .elements
            .iter()
            .filter(|element| is_dialog_class(&element.class))
            .collect();
        if roots.is_empty() {
            return Ok(ToolExecutionResult::success(format!(
                "No dialog detected — the screen holds no dialog or popup window. Here is what is \
                 on it:\n\n{}",
                format_ui_summary(&dump.elements)
            ))
            .with_data(serde_json::json!({
                "dialog": Value::Null,
                "device": resolved_serial(&device).await,
            })));
        }

        let region = roots
            .iter()
            .fold((i32::MAX, i32::MAX, i32::MIN, i32::MIN), |acc, element| {
                let (x1, y1, x2, y2) = element.bounds;
                (acc.0.min(x1), acc.1.min(y1), acc.2.max(x2), acc.3.max(y2))
            });
        let inside = |element: &UiElement| {
            let (x1, y1, x2, y2) = element.bounds;
            x1 >= region.0 && y1 >= region.1 && x2 <= region.2 && y2 <= region.3
        };

        let members: Vec<&UiElement> = dump
            .elements
            .iter()
            .filter(|element| inside(element))
            .collect();
        let buttons: Vec<&UiElement> = members
            .iter()
            .copied()
            .filter(|element| element.clickable && element_label(element).is_some())
            .collect();
        let message = members
            .iter()
            .filter(|element| !element.clickable && element_label(element).is_some())
            .filter_map(|element| element_label(element))
            .collect::<Vec<_>>()
            .join(" ");

        let summary = format!(
            "Dialog: \"{}\"\nButtons: {}",
            if message.is_empty() {
                "(no text)"
            } else {
                message.as_str()
            },
            if buttons.is_empty() {
                "(none found)".to_string()
            } else {
                buttons
                    .iter()
                    .filter_map(|element| element_label(element))
                    .collect::<Vec<_>>()
                    .join(" | ")
            }
        );

        let wanted = args
            .get("button")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(wanted) = wanted else {
            return Ok(ToolExecutionResult::success(summary).with_data(serde_json::json!({
                "message": message,
                "buttons": buttons.iter().filter_map(|e| element_label(e)).collect::<Vec<_>>(),
                "device": resolved_serial(&device).await,
            })));
        };

        let needle = wanted.to_lowercase();
        let matching: Vec<&&UiElement> = buttons
            .iter()
            .filter(|element| {
                element_label(element).is_some_and(|label| label.to_lowercase().contains(&needle))
            })
            .collect();
        let target = match matching.as_slice() {
            [only] => *only,
            [] => {
                return Ok(ToolExecutionResult::success(format!(
                    "{summary}\n\nNo button matches {wanted:?}, so nothing was pressed."
                ))
                .with_data(serde_json::json!({
                    "pressed": false,
                    "reason": "no matching button",
                    "device": resolved_serial(&device).await,
                })))
            }
            many => {
                let candidates: Vec<&str> = many
                    .iter()
                    .filter_map(|element| element_label(element))
                    .collect();
                return Ok(ToolExecutionResult::success(format!(
                    "{summary}\n\n{wanted:?} matches {} buttons ({}), so nothing was pressed — \
                     say which one exactly.",
                    candidates.len(),
                    candidates.join(" | ")
                ))
                .with_data(serde_json::json!({
                    "pressed": false,
                    "reason": "ambiguous button",
                    "candidates": candidates,
                    "device": resolved_serial(&device).await,
                })));
            }
        };

        let before = tree_fingerprint(&dump.elements);
        let command = format!("input tap {} {}", target.center.0, target.center.1);
        let argv = adb_args(&device, &["shell", &command]);
        let (status, _, stderr) =
            run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: "adb tap failed".to_string(),
                    cause: Some(Box::new(e)),
                })?;
        if !status.success() {
            return Ok(ToolExecutionResult::error(format!(
                "pressing {wanted:?} failed: {}",
                stderr.trim()
            )));
        }
        let settle = settle_after_action(&device, Some(before), 1200).await;

        Ok(
            ToolExecutionResult::success(format!("Pressed {wanted:?}. {}", settle.verdict()))
                .with_data(serde_json::json!({
                    "pressed": true,
                    "effect": settle.effect,
                    "elements": settle.elements,
                    "device": resolved_serial(&device).await,
                })),
        )
    }
}

// ── System actions ─────────────────────────────────────────────────────────

/// Alarm, timer, calendar, share and settings, through Android's own intents.
#[derive(Debug)]
pub struct AdbSystemActionTool {
    device: Option<String>,
}

impl Default for AdbSystemActionTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbSystemActionTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// The settings screens this tool will open, mapped to their intent actions.
///
/// An explicit list rather than whatever the model names: an arbitrary
/// `android.settings.*` string is a way to open screens the task has no business
/// opening, and a typo silently does nothing.
const SETTINGS_SCREENS: &[(&str, &str)] = &[
    ("wifi", "android.settings.WIFI_SETTINGS"),
    ("bluetooth", "android.settings.BLUETOOTH_SETTINGS"),
    ("airplane", "android.settings.AIRPLANE_MODE_SETTINGS"),
    ("data_usage", "android.settings.DATA_USAGE_SETTINGS"),
    ("battery", "android.settings.BATTERY_SAVER_SETTINGS"),
    ("display", "android.settings.DISPLAY_SETTINGS"),
    ("sound", "android.settings.SOUND_SETTINGS"),
    ("apps", "android.settings.APPLICATION_SETTINGS"),
    ("notifications", "android.settings.NOTIFICATION_SETTINGS"),
    ("storage", "android.settings.INTERNAL_STORAGE_SETTINGS"),
    ("location", "android.settings.LOCATION_SOURCE_SETTINGS"),
    ("developer", "android.settings.APPLICATION_DEVELOPMENT_SETTINGS"),
    ("accessibility", "android.settings.ACCESSIBILITY_SETTINGS"),
    ("date", "android.settings.DATE_SETTINGS"),
    ("language", "android.settings.LOCALE_SETTINGS"),
];

/// Build the `am start` argument list for one system action.
///
/// Split out from the tool so the intent construction is testable without a
/// device: the extras' names are Android's own constants and a typo in one is a
/// silent no-op on the phone.
/// Append `parts` to an argument list.
fn extend(out: &mut Vec<String>, parts: &[&str]) {
    out.extend(parts.iter().map(|s| s.to_string()));
}

fn system_action_args(action: &str, args: &Value) -> Result<Vec<String>, String> {
    let text = |key: &str| args.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let number = |key: &str| args.get(key).and_then(|v| v.as_i64());
    let flag =
        |key: &str, default: bool| args.get(key).and_then(|v| v.as_bool()).unwrap_or(default);

    let mut out: Vec<String> = Vec::new();

    match action {
        "set_alarm" => {
            let hour = number("hour").ok_or("set_alarm needs 'hour' (0-23)")?;
            let minute = number("minute").ok_or("set_alarm needs 'minute' (0-59)")?;
            if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
                return Err(format!("hour {hour} / minute {minute} is not a time of day"));
            }
            extend(
                &mut out,
                &[
                    "-a",
                    "android.intent.action.SET_ALARM",
                    "--ei",
                    "android.intent.extra.alarm.HOUR",
                    &hour.to_string(),
                    "--ei",
                    "android.intent.extra.alarm.MINUTES",
                    &minute.to_string(),
                ],
            );
            if let Some(label) = text("label") {
                out.push("--es".into());
                out.push("android.intent.extra.alarm.MESSAGE".into());
                out.push(sh_quote(&label));
            }
            if let Some(days) = args.get("days").and_then(|v| v.as_array()) {
                let joined = days
                    .iter()
                    .filter_map(|d| d.as_i64())
                    .filter(|d| (1..=7).contains(d))
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                if !joined.is_empty() {
                    out.push("--esa".into());
                    out.push("android.intent.extra.alarm.DAYS".into());
                    out.push(sh_quote(&joined));
                }
            }
            out.push("--ez".into());
            out.push("android.intent.extra.alarm.SKIP_UI".into());
            out.push(flag("skip_ui", true).to_string());
        }
        "set_timer" => {
            let seconds = number("seconds").ok_or("set_timer needs 'seconds'")?;
            if seconds <= 0 {
                return Err("'seconds' must be positive".to_string());
            }
            extend(
                &mut out,
                &[
                    "-a",
                    "android.intent.action.SET_TIMER",
                    "--ei",
                    "android.intent.extra.alarm.LENGTH",
                    &seconds.to_string(),
                ],
            );
            if let Some(label) = text("label") {
                out.push("--es".into());
                out.push("android.intent.extra.alarm.MESSAGE".into());
                out.push(sh_quote(&label));
            }
            out.push("--ez".into());
            out.push("android.intent.extra.alarm.SKIP_UI".into());
            out.push(flag("skip_ui", true).to_string());
        }
        "show_alarms" => extend(&mut out, &["-a", "android.intent.action.SHOW_ALARMS"]),
        "insert_calendar" => {
            let title = text("title").ok_or("insert_calendar needs 'title'")?;
            let begin = number("begin_ms").ok_or("insert_calendar needs 'begin_ms' (epoch ms)")?;
            let end = number("end_ms").ok_or("insert_calendar needs 'end_ms' (epoch ms)")?;
            if end <= begin {
                return Err("'end_ms' must be after 'begin_ms'".to_string());
            }
            extend(
                &mut out,
                &[
                    "-a",
                    "android.intent.action.INSERT",
                    "-t",
                    "vnd.android.cursor.dir/event",
                    "--es",
                    "title",
                    &sh_quote(&title),
                    "--el",
                    "beginTime",
                    &begin.to_string(),
                    "--el",
                    "endTime",
                    &end.to_string(),
                ],
            );
            out.push("--ez".into());
            out.push("allDay".into());
            out.push(flag("all_day", false).to_string());
            for (key, extra) in [
                ("location", "eventLocation"),
                ("description", "description"),
            ] {
                if let Some(value) = text(key) {
                    out.push("--es".into());
                    out.push(extra.into());
                    out.push(sh_quote(&value));
                }
            }
        }
        "share_text" => {
            let body = text("text").ok_or("share_text needs 'text'")?;
            extend(
                &mut out,
                &[
                    "-a",
                    "android.intent.action.SEND",
                    "-t",
                    "text/plain",
                    "--es",
                    "android.intent.extra.TEXT",
                    &sh_quote(&body),
                ],
            );
            if let Some(package) = text("package") {
                out.push("-p".into());
                out.push(sh_quote(&package));
            }
            // Opening the share sheet is the point; the send itself stays a
            // model decision on the screen that appears.
            out.push("--ez".into());
            out.push("android.intent.extra.NO_PREVIEW".into());
            out.push("false".into());
        }
        "open_settings" => {
            let which = text("which").ok_or("open_settings needs 'which'")?;
            let action = SETTINGS_SCREENS
                .iter()
                .find(|(name, _)| *name == which.to_ascii_lowercase())
                .map(|(_, action)| *action)
                .ok_or_else(|| {
                    let known: Vec<&str> = SETTINGS_SCREENS.iter().map(|(name, _)| *name).collect();
                    format!("unknown settings screen {which:?}; known: {}", known.join(", "))
                })?;
            extend(&mut out, &["-a", action]);
            if let Some(package) = text("package") {
                out.push("--es".into());
                out.push("android.provider.extra.APP_PACKAGE".into());
                out.push(sh_quote(&package));
            }
        }
        "open_url" => {
            let url = text("url").ok_or("open_url needs 'url'")?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(format!(
                    "open_url takes an http(s) URL; {url:?} would be a deep link into another app"
                ));
            }
            extend(
                &mut out,
                &[
                    "-a",
                    "android.intent.action.VIEW",
                    "-d",
                    &sh_quote(&url),
                    "-f",
                    "0x10000000",
                ],
            );
        }
        other => {
            return Err(format!(
                "unknown action {other:?}; use set_alarm, set_timer, show_alarms, \
                 insert_calendar, share_text, open_settings or open_url"
            ))
        }
    }
    Ok(out)
}

#[async_trait]
impl Tool for AdbSystemActionTool {
    fn name(&self) -> &str {
        "android_system_action"
    }

    fn description(&self) -> &str {
        "Do a phone-wide action through Android's own intents instead of tapping through its UI: \
         set an alarm, set a timer, add a calendar event, share text into an app, open a settings \
         screen, or open a web link. These are one stable call where the UI differs between \
         vendors and versions; preferring them over tapping is the difference between working on \
         one phone and working on any phone."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Run an Android system action",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "enum": [
                        "set_alarm", "set_timer", "show_alarms",
                        "insert_calendar", "share_text", "open_settings", "open_url"
                    ],
                    "description": "Which intent to send."
                },
                "hour": { "type": "integer", "description": "set_alarm: 0-23" },
                "minute": { "type": "integer", "description": "set_alarm: 0-59" },
                "days": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "set_alarm: repeat on these days, 1=Monday … 7=Sunday."
                },
                "label": { "type": "string", "description": "set_alarm / set_timer: the text shown with it." },
                "seconds": { "type": "integer", "description": "set_timer: duration." },
                "skip_ui": { "type": "boolean", "description": "set_alarm / set_timer: create it without opening the clock app (default true)." },
                "title": { "type": "string", "description": "insert_calendar: event title." },
                "begin_ms": { "type": "integer", "description": "insert_calendar: start, epoch milliseconds." },
                "end_ms": { "type": "integer", "description": "insert_calendar: end, epoch milliseconds." },
                "all_day": { "type": "boolean", "description": "insert_calendar: all-day event." },
                "location": { "type": "string", "description": "insert_calendar: location." },
                "description": { "type": "string", "description": "insert_calendar: notes." },
                "text": { "type": "string", "description": "share_text: the text to share." },
                "package": { "type": "string", "description": "share_text / open_settings: target app package." },
                "which": { "type": "string", "description": "open_settings: wifi, bluetooth, airplane, data_usage, battery, display, sound, apps, notifications, storage, location, developer, accessibility, date, language." },
                "url": { "type": "string", "description": "open_url: http(s) URL." },
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
        let _target = crate::tools::target_lock::TargetLocks::global()
            .lock(crate::tools::target_lock::ANDROID)
            .await;
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let intent = match system_action_args(action, &args) {
            Ok(intent) => intent,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };

        let mut argv: Vec<String> = Vec::new();
        if let Some(serial) = &device {
            argv.push("-s".into());
            argv.push(serial.clone());
        }
        argv.push("shell".into());
        argv.push("am".into());
        argv.push("start".into());
        argv.extend(intent);

        let rendered = std::iter::once("adb")
            .chain(argv.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        let (status, stdout, stderr) =
            run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .await
                .map_err(|e| crate::error::SyscityError::ExternalService {
                    source: "adb am start failed".to_string(),
                    cause: Some(Box::new(e)),
                })?;
        if !status.success() || stdout.contains("Error:") {
            return Ok(ToolExecutionResult::error(format!(
                "the system action was rejected by the phone: {}",
                if stderr.trim().is_empty() {
                    stdout.trim()
                } else {
                    stderr.trim()
                }
            ))
            .with_data(serde_json::json!({ "command": rendered })));
        }

        Ok(ToolExecutionResult::success(format!(
            "Sent {action}: the phone handled it through its own app (no UI was tapped).\n{rendered}"
        ))
        .with_data(serde_json::json!({
            "action": action,
            "command": rendered,
            "output": stdout.trim(),
            "device": resolved_serial(&device).await,
        })))
    }
}

// ── OCR ────────────────────────────────────────────────────────────────────

/// Read the screen's text from the screenshot.
#[derive(Debug)]
pub struct AdbOcrTool {
    device: Option<String>,
}

impl Default for AdbOcrTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbOcrTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbOcrTool {
    fn name(&self) -> &str {
        "android_ocr"
    }

    fn description(&self) -> &str {
        "Read the screen's text with the OCR model when the accessibility tree cannot describe \
         it. android_ui_tree and android_observe already fall back to this on their own; call it \
         directly when you want only the text, or to repeat a read after the screen changed. Each \
         line comes with the centre coordinates to tap."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Read text from the screen with OCR",
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
        match ocr_screen(&device).await {
            Ok(lines) => Ok(ToolExecutionResult::success(format_ocr_lines(&lines)).with_data(
                serde_json::json!({
                    "lines": lines,
                    "count": lines.len(),
                    "device": resolved_serial(&device).await,
                }),
            )),
            Err(message) => {
                Ok(ToolExecutionResult::error(format!("OCR is unavailable: {message}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directions_parse_both_spellings() {
        assert_eq!(Direction::parse(Some("down")).expect("down"), Direction::Down);
        assert_eq!(Direction::parse(Some("UP")).expect("up"), Direction::Up);
        assert_eq!(Direction::parse(None).expect("default"), Direction::Down);
        assert!(Direction::parse(Some("sideways")).is_err());
    }

    #[test]
    fn alarm_intent_carries_the_time_and_skips_the_ui() {
        let args = serde_json::json!({ "action": "set_alarm", "hour": 7, "minute": 30,
                                       "label": "stand up" });
        let built = system_action_args("set_alarm", &args).expect("intent");
        let joined = built.join(" ");
        assert!(joined.contains("android.intent.action.SET_ALARM"), "{joined}");
        assert!(joined.contains("android.intent.extra.alarm.HOUR 7"), "{joined}");
        assert!(joined.contains("android.intent.extra.alarm.MINUTES 30"), "{joined}");
        assert!(joined.contains("'stand up'"), "{joined}");
        assert!(joined.contains("android.intent.extra.alarm.SKIP_UI true"), "{joined}");
    }

    #[test]
    fn alarm_rejects_an_impossible_time() {
        let args = serde_json::json!({ "hour": 25, "minute": 0 });
        assert!(system_action_args("set_alarm", &args).is_err());
    }

    #[test]
    fn calendar_rejects_an_end_before_the_start() {
        let args = serde_json::json!({ "title": "x", "begin_ms": 200, "end_ms": 100 });
        assert!(system_action_args("insert_calendar", &args).is_err());
    }

    #[test]
    fn share_text_quotes_the_body() {
        let args = serde_json::json!({ "text": "it's fine; rm -rf /", "package": "com.x" });
        let built = system_action_args("share_text", &args).expect("intent");
        let joined = built.join(" ");
        assert!(joined.contains("android.intent.action.SEND"), "{joined}");
        assert!(joined.contains("'it'\\''s fine; rm -rf /'"), "{joined}");
        assert!(joined.contains("-p 'com.x'"), "{joined}");
    }

    #[test]
    fn open_url_refuses_a_non_http_scheme() {
        let args = serde_json::json!({ "url": "content://secret" });
        assert!(system_action_args("open_url", &args).is_err());
    }

    #[test]
    fn open_settings_only_accepts_known_screens() {
        let ok = serde_json::json!({ "which": "wifi" });
        assert!(system_action_args("open_settings", &ok)
            .expect("wifi")
            .join(" ")
            .contains("android.settings.WIFI_SETTINGS"));
        let bad = serde_json::json!({ "which": "root_kitchen" });
        assert!(system_action_args("open_settings", &bad).is_err());
    }

    #[test]
    fn unknown_action_names_the_alternatives() {
        let err = system_action_args("reboot", &serde_json::json!({})).expect_err("rejected");
        assert!(err.contains("set_alarm"), "{err}");
    }

    #[test]
    fn dialog_classes_are_recognised() {
        assert!(is_dialog_class("android.app.AlertDialog"));
        assert!(is_dialog_class("com.x.DialogTitle"));
        assert!(!is_dialog_class("android.widget.Button"));
    }

    #[test]
    fn tool_names_are_stable() {
        assert_eq!(AdbCollectTool::new().name(), "android_collect");
        assert_eq!(AdbFindTool::new().name(), "android_find");
        assert_eq!(AdbDialogTool::new().name(), "android_dialog");
        assert_eq!(AdbSystemActionTool::new().name(), "android_system_action");
        assert_eq!(AdbOcrTool::new().name(), "android_ocr");
    }
}
