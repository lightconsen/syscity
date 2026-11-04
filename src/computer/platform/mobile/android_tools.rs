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
use std::time::Duration;

use super::android::display_bridge;
use super::android::{
    adb_args, assess_tree, dump_ui, element_label, format_ocr_lines, format_ui_summary, ocr_screen,
    resolve_device, resolved_serial, settle_after_action, sh_quote, tree_fingerprint,
    AdbAppManagerTool, AdbInputTool, Target, TreeQuality, UiDump, UiElement, DEVICE_PARAM,
    DISPLAY_PARAM, EFFECT_UNCHANGED,
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

/// Scroll one screenful, aiming at the scrollable container when the tree named
/// one and falling back to a swipe down the middle of the display.
///
/// The container wins when there is one: a swipe starts at a point, and on a
/// screen with a nested list, a bottom bar or a sideways carousel that point
/// may land on something that also consumes drags — the scroll then moves the
/// wrong thing, or nothing. A container is addressed as a whole, so where
/// inside it the gesture starts stops mattering. When the tree names none (the
/// list is custom-drawn, or the tree could not be read) the swipe is still the
/// only option, and it is what every scroll did before this.
async fn scroll_once(
    device: &Option<String>,
    size: (i32, i32),
    direction: Direction,
    target: Target,
    container: Option<&UiElement>,
) -> Result<(), String> {
    let (from, to) = scroll_points(size, direction, container);
    if let Target::Display(id) = target {
        // A swipe aimed at the agent's display goes through the helper: ADB's
        // own `input` only reaches the default display.
        let bridge = display_bridge()?;
        let display = super::display::VirtualDisplay::attach(bridge.clone(), id);
        return display
            .swipe(from, to, 300)
            .await
            .map_err(|e| e.to_string());
    }
    let command = format!("input swipe {} {} {} {} 300", from.0, from.1, to.0, to.1);
    let args = adb_args(device, &["shell", &command]);
    let (status, _, stderr) = run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .await
        .map_err(|e| format!("could not run adb to scroll: {e}"))?;
    if !status.success() {
        return Err(format!("scrolling failed: {}", stderr.trim()));
    }
    Ok(())
}

/// Where one screenful's drag starts and ends.
///
/// A container gets the gesture *inside itself*, split across its height, so the
/// drag stays within the thing that is supposed to move. Shared with
/// `android_run`, which scrolls through `android_input` so the step's guard and
/// effect check still apply to it.
fn scroll_points(
    size: (i32, i32),
    direction: Direction,
    container: Option<&UiElement>,
) -> ((i32, i32), (i32, i32)) {
    let (width, height) = size;
    let full_screen = || {
        let (from, to) = match direction {
            Direction::Down => (height * 3 / 4, height / 4),
            Direction::Up => (height / 4, height * 3 / 4),
        };
        ((width / 2, from), (width / 2, to))
    };
    match container {
        Some(node) => {
            let (x1, y1, x2, y2) = node.bounds;
            let span = y2 - y1;
            // A node too short to hold a real drag (a two-row list, a sliver)
            // cannot express a screenful; the whole screen still can.
            if span < 200 || x2 <= x1 {
                return full_screen();
            }
            let (from, to) = match direction {
                Direction::Down => (y1 + span * 3 / 4, y1 + span / 4),
                Direction::Up => (y1 + span / 4, y1 + span * 3 / 4),
            };
            ((node.center.0, from), (node.center.0, to))
        }
        None => full_screen(),
    }
}

/// The scrollable container a scroll should be aimed at, if the tree names one.
///
/// Innermost first (deepest wins) among those that cover the middle of the
/// screen: the outer container of a chat screen covers everything, including
/// the input bar, while the message list is the inner one — and it is the
/// message list that a "scroll down" is about. Ties go to the larger node,
/// which is the one holding more content.
fn scroll_container(
    elements: &[UiElement],
    size: (i32, i32),
    direction: Direction,
) -> Option<&UiElement> {
    let (_, height) = size;
    let midline = height / 2;
    elements
        .iter()
        .filter(|element| {
            element.state.scrollable && element.bounds.1 <= midline && element.bounds.3 >= midline
        })
        .filter(|element| match direction {
            Direction::Down => element.state.scroll_forward,
            Direction::Up => element.state.scroll_backward,
        })
        .max_by_key(|element| {
            let (x1, y1, x2, y2) = element.bounds;
            (element.state.depth, (x2 - x1) as i64 * (y2 - y1) as i64)
        })
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
        let _target = super::android::lock_device(&device).await;
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
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
            let dump = match dump_ui(&device, target).await {
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
            let container = scroll_container(&dump.elements, size, direction);
            if let Err(message) = scroll_once(&device, size, direction, target, container).await {
                stop = "scrolling failed";
                if screens.len() == 1 {
                    return Ok(ToolExecutionResult::error(message));
                }
                break;
            }
            let before = tree_fingerprint(&dump.elements);
            let settle = settle_after_action(&device, target, Some(before), 900).await;
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
                "display": { "type": "integer", "description": DISPLAY_PARAM },
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
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let _target = super::android::lock_device(&device).await;
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
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
            let dump = match dump_ui(&device, target).await {
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
            let container = scroll_container(&dump.elements, size, direction);
            if let Err(message) = scroll_once(&device, size, direction, target, container).await {
                return Ok(ToolExecutionResult::error(message));
            }
            let before = tree_fingerprint(&dump.elements);
            let settle = settle_after_action(&device, target, Some(before), 900).await;
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
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            Vec::<String>::new(),
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
        // A dialog on a display is a write to that display's own focus, not to
        // the user's screen; the same split as `android_input` applies here.
        let _target = super::android::lock_target(&device, target).await;
        let dump = match dump_ui(&device, target).await {
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
        let chosen = match matching.as_slice() {
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

        // A person may have locked the phone, and the task may have left the
        // display while the model was thinking. Either way the button being
        // pressed is not the button that was read.
        let readiness = super::ready::probe().await;
        if let Err(blocked) =
            super::ready::check(readiness.as_ref(), target, dump.package.as_deref())
        {
            return Ok(ToolExecutionResult::error(super::ready::describe(&blocked)));
        }

        // The dialog's own button is a fresh decision point: a confirmation
        // prompt reading "Send 3 photos?" ends in a send whatever opened it.
        let offer = chosen
            .text
            .as_deref()
            .or(chosen.content_desc.as_deref())
            .and_then(super::risk::classify_named);
        let subject = format!("press \"{wanted}\" on a dialog");
        let clearance =
            match super::gate::check(offer, context, &subject, dump.package.as_deref()).await {
                Ok(clearance) => clearance,
                Err(refusal) => return Ok(ToolExecutionResult::error(refusal.reason)),
            };

        // Pressing the same dialog button again and again is the loop the
        // ladder exists for, and it runs after the gate so that "the person
        // said yes" is not silently spent on a refusal.
        let guard_key = super::guard::Guard::bucket_key(
            &context.conversation_id,
            &format!("{}:{}", device.as_deref().unwrap_or("-"), target.display_id()),
        );
        let signature = format!("dialog:{wanted}");
        let ladder = super::guard::Guard::global().check(&guard_key, &signature);
        if let super::guard::Verdict::Refuse(reason) = &ladder {
            return Ok(ToolExecutionResult::error(if clearance.asked {
                format!("{reason} (An approval was given but unused; nothing was pressed.)")
            } else {
                reason.clone()
            }));
        }

        // The question was open for as long as the person took to read it, and
        // a dialog is exactly the kind of screen that changes underneath one:
        // a countdown closes it, a message arrives behind it. What was approved
        // was the button that was described; before pressing, check that it is
        // still that button in that app. The re-read only happens when somebody
        // was actually asked — otherwise there is nothing to check.
        let mut fresh_dump: Option<UiDump> = None;
        if clearance.asked {
            let fresh = match super::android::dump_ui(&device, target).await {
                Ok(fresh) => fresh,
                Err(failed) => {
                    return Ok(ToolExecutionResult::error(format!(
                        "Refused: \"{subject}\" was approved, but the screen could not be read \
                         again to confirm it is still there ({}). Nothing was pressed.",
                        failed.message
                    )))
                }
            };
            let still_there = fresh.elements.iter().any(|element| {
                element.clickable
                    && super::android::element_label(element)
                        .is_some_and(|label| label.trim() == wanted.trim())
            });
            if !still_there {
                return Ok(ToolExecutionResult::error(format!(
                    "Refused: \"{subject}\" was approved, and by then the dialog no longer had \
                     that button. Nothing was pressed. Read the screen again."
                )));
            }
            let now = super::risk::classify_named(wanted);
            if let Err(withheld) =
                super::gate::verify(&clearance, now.as_ref(), fresh.package.as_deref())
            {
                return Ok(ToolExecutionResult::error(withheld.reason));
            }
            fresh_dump = Some(fresh);
        }
        let before = tree_fingerprint(&fresh_dump.as_ref().unwrap_or(&dump).elements);
        // The button's own position may have moved with its container; press
        // where it is *now*, not where it was before the question.
        let at = fresh_dump
            .as_ref()
            .and_then(|fresh| {
                fresh
                    .elements
                    .iter()
                    .find(|element| {
                        element.clickable
                            && super::android::element_label(element)
                                .is_some_and(|label| label.trim() == wanted.trim())
                    })
                    .map(|element| element.center)
            })
            .unwrap_or(chosen.center);
        let pressed = match target {
            Target::Screen => {
                let command = format!("input tap {} {}", at.0, at.1);
                let argv = adb_args(&device, &["shell", &command]);
                run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                    .await
                    .map(|(status, _, stderr)| (status.success(), stderr))
                    .map_err(|e| e.to_string())
            }
            Target::Display(id) => match display_bridge() {
                Ok(bridge) => super::display::VirtualDisplay::attach(bridge, id)
                    .tap(at.0, at.1)
                    .await
                    .map(|()| (true, String::new()))
                    .map_err(|e| e.to_string()),
                Err(message) => Err(message),
            },
        };
        match pressed {
            Ok((true, _)) => {}
            Ok((false, stderr)) => {
                return Ok(ToolExecutionResult::error(format!(
                    "pressing {wanted:?} failed: {}",
                    stderr.trim()
                )))
            }
            Err(message) => {
                return Ok(ToolExecutionResult::error(format!(
                    "pressing {wanted:?} failed: {message}"
                )))
            }
        }
        let settle = settle_after_action(&device, target, Some(before), 1200).await;

        // The ladder learns what the press did, judged by the screen rather
        // than the dispatch: a dialog that reappears is the same screen, and
        // that is the stuck state this counts.
        super::guard::Guard::global().record(
            &guard_key,
            &signature,
            super::android::guard_outcome(settle.effect),
        );

        // A dialog button that reaches outward counts toward the pace exactly
        // as an `android_input` tap would — otherwise the two paths would be
        // two different speeds through the same app.
        let pace_note = if clearance.reaches_outward() && context.mobile_control().pace_outward {
            let package = dump.package.as_deref();
            Some(super::android::note_outward(package).await)
        } else {
            None
        };
        let mut text = format!("Pressed {wanted:?}. {}", settle.verdict());
        if let Some(note) = &pace_note {
            text.push_str(&format!(" {note}"));
        }
        if let Some(note) = ladder.note() {
            text.push(' ');
            text.push_str(note);
        }

        Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
            "pressed": true,
            "effect": settle.effect,
            "elements": settle.elements,
            "device": resolved_serial(&device).await,
            "risk": clearance.note(),
            "risk_approved": clearance.asked,
            "paced": pace_note,
            "guard": ladder.note(),
        })))
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
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let _target = super::android::lock_device(&device).await;
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
        "Read the screen's text with OCR when the accessibility tree cannot describe it. \
         android_ui_tree and android_observe already fall back to this on their own; call it \
         directly when you want only the text, or to repeat a read after the screen changed. Each \
         line comes with the centre coordinates to tap. The engine differs by host: on the phone \
         itself it is ML Kit's bundled Chinese model (offline, on-device), while a desktop driving \
         a device uses its own ONNX model — so the same screen can read slightly differently."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Read text from the screen with OCR",
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
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        match ocr_screen(&device, target).await {
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

// ── Takeover ───────────────────────────────────────────────────────────────

/// Wait until a steer arrives, or forever when nothing can steer.
///
/// `select!` needs a future in every arm; outside an agent turn there is no
/// controller, and "nothing will ever arrive" is the honest shape of that.
async fn wait_for_steer(controller: Option<&crate::acp::ExecutionController>) {
    match controller {
        Some(controller) => controller.steer_notified().await,
        None => std::future::pending().await,
    }
}

/// Hand the agent's display to the person, wait, and take it back.
#[derive(Debug)]
pub struct AdbHandToUserTool {
    device: Option<String>,
}

impl Default for AdbHandToUserTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbHandToUserTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbHandToUserTool {
    fn name(&self) -> &str {
        "android_hand_to_user"
    }

    fn description(&self) -> &str {
        "Stop and ask the person to do a step you cannot: a login, a captcha, a payment \
         confirmation. Their app is brought onto their own screen so they can touch it, the \
         question tells them what is needed, and when they answer it comes back to you — with \
         the screen re-read, so you can see whether it actually changed before acting on their \
         word. Use this instead of guessing at a password field or tapping through a captcha; \
         use it once per blockage, not as a retry loop."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Ask the person to take over",
            serde_json::json!({
                "display": {
                    "type": "integer",
                    "description": "The display the agent is working on (from android_display start)."
                },
                "question": {
                    "type": "string",
                    "description": "What you need them to do, in their words. Say what to do and how to tell you it is finished."
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "How long to wait (default 300, max 1800)."
                },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            vec!["display", "question"],
        )
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let Some(display_id) = args.get("display").and_then(|v| v.as_i64()) else {
            return Ok(ToolExecutionResult::error(
                "`display` is required — the id android_display start returned",
            ));
        };
        let display_id = i32::try_from(display_id).unwrap_or(-1);
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("Please finish this step, then say what you did.")
            .to_string();
        let timeout_secs = args
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(300)
            .clamp(30, 1800);

        // The display is only reachable from the app itself; a desktop driving
        // a remote phone has no screen to hand over.
        let Ok(bridge) = display_bridge() else {
            return Ok(ToolExecutionResult::error(
                "handing a screen to the user needs the Syscity app on the phone; this host has \
                 no display helper",
            ));
        };

        // No human, no takeover. Refusing beats blocking a background turn for
        // five minutes waiting for someone who was never there.
        let Some(queue) = context.ask_queue.clone() else {
            return Ok(ToolExecutionResult::error(
                "there is nobody to hand this to in this context (no interactive channel)",
            ));
        };

        // The fingerprint *before* the hand-back is what makes "I'm done" a
        // checkable claim.
        let before = super::android::dump_ui_elements(
            &self.device,
            super::android::Target::Display(display_id),
        )
        .await
        .ok()
        .map(|elements| super::android::tree_fingerprint(&elements));

        let conversation_id = context.conversation_id.clone();
        // A steer that arrives while the question is open is the person still
        // talking about the step they were handed — they typed at the agent
        // rather than pressing a button. It belongs with the handoff's result,
        // not after it, so the wait watches for it and leaves it as a note.
        let notices_context = context.clone();
        let ask = Box::new(move || {
            Box::pin(async move {
                use crate::tools::ask_user::AskRequest;
                use crate::tools::ask_user::PendingQuestion;
                let (tx, rx) = tokio::sync::oneshot::channel();
                let id = format!("takeover-{}", uuid::Uuid::new_v4());
                queue
                    .submit(PendingQuestion {
                        request: AskRequest {
                            id: id.clone(),
                            conversation_id,
                            question,
                            options: vec![
                                "Done".to_string(),
                                "I'll do the rest myself".to_string(),
                                "Skip this step".to_string(),
                            ],
                            required: true,
                            default: None,
                        },
                        response_tx: Some(tx),
                    })
                    .await;
                let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
                tokio::pin!(deadline);
                let mut answer = rx;
                let controller = notices_context.steers.clone();
                // Waiting for the answer and for anything typed at the agent
                // are the same wait: the person may answer with either. What
                // they typed is moved out of the controller here rather than
                // left for the next iteration, which would be too late — the
                // loop only drains between tool calls.
                //
                // Drained both before sleeping and again after waking, so a
                // message that arrived before this tool started is not left
                // behind either. The remaining gap — a steer landing between
                // the drain and the wait registering — is not a loss: it stays
                // in the queue and reaches the model at the top of the next
                // iteration, which is where it would have gone anyway.
                loop {
                    if let Some(controller) = controller.as_deref() {
                        for steer in controller.drain_steers() {
                            notices_context.note(steer);
                        }
                    }
                    tokio::select! {
                        answered = &mut answer => {
                            return answered.ok();
                        }
                        _ = &mut deadline => {
                            queue.cancel(&id).await;
                            return None;
                        }
                        _ = wait_for_steer(controller.as_deref()) => {}
                    }
                }
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        });

        // The hold outlives the question by a minute: the question's own
        // timeout ends it for the caller, and this covers the gap between that
        // and taking the display back. Without it a cancelled turn would leave
        // the display unwritable until the helper died.
        //
        // No `TargetLocks` guard is taken here, deliberately: it would block
        // this phone for as long as the person takes. What holds the display
        // instead is the helper's own takeover flag (`giveToUser`), which every
        // write checks on the device — so it covers a session on another host,
        // or one that connected while this handoff was already in flight.
        let hold = std::time::Duration::from_secs(timeout_secs + 60);
        let outcome =
            match super::takeover::hand_to_user(&bridge, display_id, before, hold, ask).await {
                Ok(outcome) => outcome,
                Err(failed) => return Ok(ToolExecutionResult::error(failed.to_string())),
            };

        Ok(ToolExecutionResult::success(super::takeover::describe(&outcome)).with_data(
            serde_json::json!({
                "verdict": outcome.verdict,
                "answer": outcome.answer,
                "screen_changed": outcome.screen_changed,
                "display": display_id,
                "elements": outcome.elements,
            }),
        ))
    }
}

// ── Virtual display ────────────────────────────────────────────────────────

/// Create and drive a second display, so the task does not take the phone.
#[derive(Debug)]
pub struct AdbDisplayTool {
    device: Option<String>,
}

impl Default for AdbDisplayTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbDisplayTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    /// Kill whatever helper is running on the device.
    ///
    /// Best effort: a device without `pkill` is not a reason to refuse.
    async fn kill_helper(&self, device: &Option<String>, class: &str) -> bool {
        let args = adb_args(device, &["shell", "pkill", "-f", class]);
        super::run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
            .await
            .is_ok_and(|(status, _, _)| status.success())
    }
}

#[async_trait]
impl Tool for AdbDisplayTool {
    fn name(&self) -> &str {
        "android_display"
    }

    fn description(&self) -> &str {
        "Run a task on a second screen so the user keeps using the phone. `start` creates the \
         display and returns its id; every later call passes that id. Apps opened on it do not \
         appear on the user's screen, and on Android 14+ it holds its own focus, so typing there \
         does not steal the user's keyboard. `frame` returns a PNG of that display. This needs \
         the Syscity app running on the phone; a desktop controlling a remote device has no \
         second screen to offer."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Create and drive a second Android display",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "enum": [
                        "start", "restart", "stop", "status", "limits", "frame", "launch",
                        "tap", "swipe", "text", "paste", "key", "mute", "sound"
                    ],
                    "description": "start creates the display; limits says how many may be open; restart replaces a stale helper; the rest act on it. 'paste' carries non-ASCII text, which `text` cannot. 'mute' silences the app on this display and 'sound' gives it back; launching an app mutes it already."
                },
                "display": {
                    "type": "integer",
                    "description": "Display id returned by `start`. Required except for start/status."
                },
                "package": {
                    "type": "string",
                    "description": "start / launch: the app to open on the display; mute / sound: the app whose sound to change."
                },
                "width": { "type": "integer", "description": "start: display width (default 1080)." },
                "height": { "type": "integer", "description": "start: display height (default 2400)." },
                "x": { "type": "integer", "description": "tap / swipe start X." },
                "y": { "type": "integer", "description": "tap / swipe start Y." },
                "x2": { "type": "integer", "description": "swipe end X." },
                "y2": { "type": "integer", "description": "swipe end Y." },
                "duration": { "type": "integer", "description": "swipe duration in ms (default 300)." },
                "text": { "type": "string", "description": "text to type on the display." },
                "keycode": { "type": "string", "description": "key to press, e.g. BACK or ENTER." },
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
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        // Not the device lock: a display is its own focus, so two displays on
        // one phone are two places to write and do not need to wait for each
        // other. Which display this call is about is often only known after
        // the action is parsed (`start` names none, `stop` names one), so the
        // lock for a named display is taken below, once the id is in hand.
        // Actions that are about the phone as a whole (`start`, `restart`,
        // `status`, `limits`) take the device.
        let named_display = args
            .get("display")
            .and_then(|v| v.as_i64())
            .and_then(|id| i32::try_from(id).ok())
            .filter(|_| action != "start" && action != "restart");
        let _target = match named_display {
            Some(id) => super::android::lock_target(&device, Target::Display(id)).await,
            None => super::android::lock_device(&device).await,
        };
        let bridge = match display_bridge() {
            Ok(bridge) => bridge,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };

        if action == "status" {
            // The helper's own description, and the state that decides whether
            // a write still means anything: locked screen, which app each
            // display holds. Reported together because "it is running" is not
            // the same answer as "it is running somewhere useful".
            return Ok(match super::display::VirtualDisplay::status(&bridge).await {
                Ok(answer) => {
                    let mut text = format!("Display helper is running: {answer}");
                    if let Ok(state) = super::display::VirtualDisplay::readiness(&bridge).await {
                        text.push('\n');
                        text.push_str(&describe_readiness(&state));
                    }
                    ToolExecutionResult::success(text).with_data(answer)
                }
                Err(failed) => ToolExecutionResult::error(failed.to_string()),
            });
        }

        if action == "limits" {
            // Whether another display may be opened at all, and why not. A
            // caller that starts three on a phone that allows one gets two
            // failures and a half-driven screen; this answers first.
            return Ok(match super::display::VirtualDisplay::parallel_limit(&bridge).await {
                Ok(limits) => {
                    let mut text = format!(
                        "{} display(s) may be open at once; {} already are.",
                        limits.limit, limits.open
                    );
                    if let Some(reason) = &limits.reason {
                        text.push(' ');
                        text.push_str(reason);
                    }
                    ToolExecutionResult::success(text).with_data(serde_json::json!({
                        "limit": limits.limit,
                        "open": limits.open,
                        "remaining": limits.remaining(),
                        "reason": limits.reason,
                    }))
                }
                Err(failed) => ToolExecutionResult::error(failed.to_string()),
            });
        }

        if action == "restart" {
            // Kill whatever helper is running before starting a fresh one.
            //
            // A helper can outlive the app that started it (`app_process` is
            // detached on purpose) and then hold the socket name with a
            // process from an older build — reachable only if its identity
            // still matches. This clears that without needing shell on the
            // host.
            let killed = self
                .kill_helper(&device, super::display::HELPER_CLASS)
                .await;
            return Ok(
                match super::display::ensure_helper(
                    &device,
                    super::display::ANDROID_PACKAGE,
                    &bridge,
                )
                .await
                {
                    Ok(()) => ToolExecutionResult::success(format!(
                        "Display helper restarted{} and is answering.",
                        if killed {
                            ""
                        } else {
                            " (nothing was running to kill)"
                        }
                    )),
                    Err(failed) => ToolExecutionResult::error(failed.to_string()),
                },
            );
        }

        if action == "start" {
            // The helper is started on demand: nothing creates a shell process
            // on the phone until a task actually needs a display.
            if let Err(failed) =
                super::display::ensure_helper(&device, super::display::ANDROID_PACKAGE, &bridge)
                    .await
            {
                return Ok(ToolExecutionResult::error(failed.to_string()));
            }
            let size = match (
                args.get("width").and_then(|v| v.as_u64()),
                args.get("height").and_then(|v| v.as_u64()),
            ) {
                (Some(width), Some(height)) => {
                    Some((u32::try_from(width).unwrap_or(0), u32::try_from(height).unwrap_or(0)))
                }
                _ => None,
            };
            let package = args.get("package").and_then(|v| v.as_str());
            return Ok(match super::display::VirtualDisplay::open(bridge, size, package).await {
                Ok(display) => {
                    let text = format!(
                        "Second display ready: id {}, {}x{}. Open an app with action 'launch', \
                         read it with 'frame'. The user's screen is untouched.",
                        display.id, display.width, display.height
                    );
                    ToolExecutionResult::success(text).with_data(serde_json::json!({
                        "display": display.id,
                        "width": display.width,
                        "height": display.height,
                    }))
                }
                Err(failed) => ToolExecutionResult::error(failed.to_string()),
            });
        }

        let Some(id) = args.get("display").and_then(|v| v.as_i64()) else {
            return Ok(ToolExecutionResult::error(
                "this action needs the `display` id that `start` returned",
            ));
        };
        let id = i32::try_from(id).unwrap_or(-1);
        // The display is addressed by id alone for every later call: the helper
        // owns the record, and reconstructing a handle here would need its size.
        let display = super::display::VirtualDisplay::attach(bridge.clone(), id);

        // A tap on this display can still end somewhere irreversible: the
        // display runs a copy of the app, and the app's send button is the
        // app's send button. The tree for the display is read here because
        // the coordinates alone say nothing.
        let mut before_package: Option<String> = None;
        let mut before_fingerprint: Option<u64> = None;
        let tap_point = (
            i32::try_from(args.get("x").and_then(|v| v.as_i64()).unwrap_or(0)).unwrap_or(0),
            i32::try_from(args.get("y").and_then(|v| v.as_i64()).unwrap_or(0)).unwrap_or(0),
        );
        let clearance = match action {
            "tap" => {
                let (x, y) = tap_point;
                let offer = match super::android::dump_ui(&device, Target::Display(id)).await {
                    Ok(dump) => {
                        before_package = dump.package;
                        before_fingerprint = Some(super::android::tree_fingerprint(&dump.elements));
                        super::risk::classify_point(&dump.elements, x, y)
                    }
                    // A read that failed is not a reason to refuse a tap the
                    // agent has already located. It does mean the judgement
                    // could not be made, which the reply carries no `risk` for.
                    Err(_) => None,
                };
                let subject = match &offer {
                    Some(offer) => format!(
                        "tap \"{}\" on display {id}",
                        offer.element.as_deref().unwrap_or("that")
                    ),
                    None => format!("tap on display {id}"),
                };
                super::gate::check(offer, context, &subject, before_package.as_deref()).await
            }
            // `launch` starts an app, so the app it is about to open is the app
            // this write will be in — named by the argument, since there is no
            // screen to read it from yet.
            "launch" => {
                before_package = args
                    .get("package")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string);
                Ok(super::gate::Clearance {
                    offer: None,
                    asked: false,
                    granted: None,
                })
            }
            // Nothing a paste types is what commits it; the button afterwards
            // is the tap, and that tap is classified on its own.
            _ => Ok(super::gate::Clearance {
                offer: None,
                asked: false,
                granted: None,
            }),
        };
        let clearance = match clearance {
            Ok(clearance) => clearance,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal.reason)),
        };

        // What was approved was a description, and the screen has had the whole
        // question's lifetime to move. One re-read, only when somebody was
        // actually asked: a clock has closed and a message has arrived behind
        // it, and "tap 发送" means nothing unless the 发送 that is there now is
        // the one that was described.
        if clearance.asked {
            let fresh = match super::android::dump_ui(&device, Target::Display(id)).await {
                Ok(fresh) => fresh,
                Err(failed) => {
                    return Ok(ToolExecutionResult::error(format!(
                        "Refused: the tap on display {id} was approved, but the display could \
                         not be read again to confirm it is still there ({}). Nothing was done.",
                        failed.message
                    )))
                }
            };
            let now = super::risk::classify_point(&fresh.elements, tap_point.0, tap_point.1);
            if let Err(withheld) =
                super::gate::verify(&clearance, now.as_ref(), fresh.package.as_deref())
            {
                return Ok(ToolExecutionResult::error(withheld.reason));
            }
            // The press is computed against the screen as it is now, not as it
            // was when the question was asked.
            before_package = fresh.package;
            before_fingerprint = Some(super::android::tree_fingerprint(&fresh.elements));
        }

        // Two displays on one phone are two places to write, but the app on
        // them is one app: one process, one account, one set of unsaved edits.
        // `launch` names its app in the argument; every other action finds out
        // from the screen it just read. `stop` touches no app at all.
        let lease_package = if action == "stop" {
            None
        } else {
            before_package.clone()
        };
        let seen = super::android::app_completed(&device, lease_package.as_deref());
        let _lease = super::android::lease_app(&device, lease_package.as_deref(), seen).await;
        if _lease.stale {
            return Ok(ToolExecutionResult::error(format!(
                "Refused: another writer changed {} while this call waited for it — the screen \
                 read a moment ago no longer describes what is there. Nothing was done. Read the \
                 display again and retry.",
                lease_package.as_deref().unwrap_or("this app")
            )));
        }

        // The app's own screens are the controls that govern the agent; no
        // approval makes operating them right. Only `launch` names a package
        // without reading one, and the guard covers it too.
        if let Some(package) = before_package.as_deref() {
            if super::takeover::is_own_app(package) {
                return Ok(ToolExecutionResult::error(format!(
                    "Refused: this would write to Syscity's own screen ({package}). The app's \
                     settings, pairing and display pages are the controls that govern the \
                     agent, so the agent does not operate them — a person does. Nothing was \
                     done."
                )));
            }
        }

        // Whether the phone is still in the state this write was computed for:
        // the helper still alive, the person's screen unlocked, this display
        // still holding the app it held. `launch` is exempt — it is the
        // ordinary way back into a display whose app has gone, so "this display
        // no longer shows what it did" is the starting condition, not a fault.
        if action != "launch" {
            if let Ok(readiness) = super::display::VirtualDisplay::readiness(&bridge).await {
                if let Err(blocked) =
                    super::ready::check(Some(&readiness), Target::Display(id), None)
                {
                    return Ok(ToolExecutionResult::error(super::ready::describe(&blocked)));
                }
            }
        }

        // What the model is about to do, named so that "the same action again"
        // is a question with an answer. A tap is worth more than its
        // coordinates: the label of what is under it, from the read above.
        let reads_only =
            matches!(action, "frame" | "status" | "limits" | "stop" | "mute" | "sound");
        let signature = match action {
            "tap" => format!("tap@{}:{}", tap_point.0, tap_point.1),
            "swipe" => {
                let value = |key: &str| args.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
                format!("swipe@{}:{}>{}:{}", value("x"), value("y"), value("x2"), value("y2"))
            }
            "text" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                format!("text:{}", text.chars().take(40).collect::<String>())
            }
            "paste" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                format!("paste:{}", text.chars().take(40).collect::<String>())
            }
            "key" => format!("key:{}", args.get("keycode").and_then(|v| v.as_str()).unwrap_or("")),
            _ => format!("{action}:{id}"),
        };
        let guard_key = super::guard::Guard::bucket_key(
            &context.conversation_id,
            &format!("{}:{}", device.as_deref().unwrap_or("-"), id),
        );
        let ladder = if reads_only {
            super::guard::Verdict::Clear
        } else {
            super::guard::Guard::global().check(&guard_key, &signature)
        };
        if let super::guard::Verdict::Refuse(reason) = &ladder {
            // A refusal to take an action that was already approved has to say
            // so: the person is not being asked again, and nothing was done.
            return Ok(ToolExecutionResult::error(if clearance.asked {
                format!("{reason} (An approval was given but unused; nothing was sent.)")
            } else {
                reason.clone()
            }));
        }

        let result = match action {
            "stop" => display.close().await.map(|_| "Display closed.".to_string()),
            "launch" => {
                let package = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                display
                    .launch(package)
                    .await
                    .map(|_| format!("Opened {package} on display {id} (muted)."))
            }
            "frame" => match display.screenshot().await {
                Ok(frame) => {
                    let encoded = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &frame.png,
                    );
                    return Ok(ToolExecutionResult::success("Frame captured").with_data(
                        serde_json::json!({
                            "base64": encoded,
                            "format": "png",
                            "width": frame.width,
                            "height": frame.height,
                            "display": id,
                        }),
                    ));
                }
                Err(failed) => Err(failed),
            },
            "tap" => {
                let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
                let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
                display
                    .tap(i32::try_from(x).unwrap_or(0), i32::try_from(y).unwrap_or(0))
                    .await
                    .map(|_| format!("Tapped ({x}, {y}) on display {id}."))
            }
            "swipe" => {
                let value = |key: &str, default: i64| {
                    args.get(key).and_then(|v| v.as_i64()).unwrap_or(default)
                };
                let duration = u32::try_from(value("duration", 300).max(1)).unwrap_or(300);
                display
                    .swipe(
                        (
                            i32::try_from(value("x", 0)).unwrap_or(0),
                            i32::try_from(value("y", 0)).unwrap_or(0),
                        ),
                        (
                            i32::try_from(value("x2", 0)).unwrap_or(0),
                            i32::try_from(value("y2", 0)).unwrap_or(0),
                        ),
                        duration,
                    )
                    .await
                    .map(|_| format!("Swiped on display {id}."))
            }
            "text" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                display.text(text).await.map(|_| {
                    format!("Typed {} character(s) on display {id}.", text.chars().count())
                })
            }
            "paste" => {
                // Bypasses the key character map, so CJK and emoji arrive as
                // they are. `text` above remains the ASCII path.
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                display.paste_text(text).await.map(|_| {
                    format!(
                        "Pasted {} character(s) into the focused field on display {id}.",
                        text.chars().count()
                    )
                })
            }
            "key" => {
                let keycode = args
                    .get("keycode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("BACK");
                display
                    .key(keycode)
                    .await
                    .map(|_| format!("Pressed {keycode} on display {id}."))
            }
            "mute" | "sound" => {
                let package = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                if package.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "`package` is required: say which app's sound to change",
                    ));
                }
                let muted = action == "mute";
                match display.set_audio_muted(package, muted).await {
                    Ok(true) => Ok(if muted {
                        format!(
                            "{package} is silent on display {id}; its sound returns when it leaves."
                        )
                    } else {
                        // True also for an app the agent never muted: nothing is
                        // being held down, and saying what it was set back to
                        // would imply it had been changed.
                        format!("{package}'s sound is no longer held down by the agent.")
                    }),
                    Ok(false) => Err(super::display::DisplayError::new(
                        super::display::DISPLAY_HELPER_ERROR,
                        format!(
                            "the sound of {package} was not changed: the helper records what \
                             the mode was before silencing an app, and will not touch one it \
                             cannot read first. The app is running; it is merely audible."
                        ),
                    )),
                    Err(failed) => Err(failed),
                }
            }
            other => {
                return Ok(ToolExecutionResult::error(format!(
                    "unknown action {other:?}; use start, stop, status, limits, frame, launch, \
                     tap, swipe, text, paste, key, mute or sound"
                )))
            }
        };

        // The ladder learns what this did, from the screen's own answer rather
        // than the action's: a tap the helper accepted is not a tap that
        // landed. Only a tap took a read before it, so only a tap has a
        // fingerprint to compare against; the rest are recorded as unseen,
        // which the ladder does not count as "nothing happened".
        if !reads_only {
            let outcome = match before_fingerprint {
                Some(was) => match super::android::dump_ui(&device, Target::Display(id)).await {
                    Ok(now) if super::android::tree_fingerprint(&now.elements) == was => {
                        super::guard::Outcome::Unchanged
                    }
                    Ok(_) => super::guard::Outcome::Changed,
                    Err(_) => super::guard::Outcome::Unknown,
                },
                None => super::guard::Outcome::Unknown,
            };
            super::guard::Guard::global().record(&guard_key, &signature, outcome);
        }

        // Same pace as `android_input`, and likewise after the action: the
        // display is another screen running the same app, so a second send
        // there is a second send.
        let pace_note = if clearance.reaches_outward() && context.mobile_control().pace_outward {
            Some(super::android::note_outward(before_package.as_deref()).await)
        } else {
            None
        };

        Ok(match result {
            Ok(message) => {
                let mut text = message;
                if let Some(note) = ladder.note() {
                    text.push(' ');
                    text.push_str(note);
                }
                ToolExecutionResult::success(text).with_data(serde_json::json!({
                    "display": id,
                    "action": action,
                    "risk": clearance.note(),
                    "risk_approved": clearance.asked,
                    "paced": pace_note,
                    "guard": ladder.note(),
                }))
            }
            Err(failed) => ToolExecutionResult::error(failed.to_string()),
        })
    }
}

// ── Run ────────────────────────────────────────────────────────────────────

/// The most steps one `android_run` may take.
///
/// A step is a full turn's worth of work — find the target, act, read the
/// screen back — and past about thirty of them one tool call is no longer
/// something a reader can follow when it goes wrong. The same ceiling
/// `android_input action=sequence` puts on a chain of taps.
pub const RUN_MAX_STEPS: usize = super::android::MAX_SEQUENCE_STEPS;

/// The most steps a run may take when every read costs a `uiautomator` dump.
///
/// The in-process accessibility snapshot is one bridge call and answers in
/// milliseconds. `uiautomator dump` is a subprocess plus an `/sdcard` round
/// trip, and a run reads at least twice per step — once to find the target,
/// once to see what happened. Ten steps on that path already cost about what
/// thirty do in-process, so the ceiling follows the read path instead of
/// pretending the two are the same. The result says which path it was on.
pub const RUN_MAX_STEPS_FALLBACK: usize = 10;

/// The most characters one `clear` will backspace.
///
/// A field holding a book is not what "clear" is for, and every key is a real
/// key event on the phone: after a point, selecting everything and deleting is
/// the honest answer, and the model can ask for that itself.
const RUN_CLEAR_MAX_KEYS: usize = 200;

/// The most a step's own `wait` may take.
const RUN_INLINE_WAIT_MS: u64 = 10_000;

/// Why a run stopped before its last command.
///
/// Every one of these is a fact about the phone, not about the step: the step
/// may well have worked, and the next one is what would be wrong.
pub const STOP_APP_SWITCHED: &str = "app_switched";
pub const STOP_OPENED_ELSEWHERE: &str = "opened_elsewhere";
pub const STOP_SCREEN_NEEDS_REVIEW: &str = "screen_needs_review";
pub const STOP_AMBIGUOUS_TARGET: &str = "ambiguous_target";
pub const STOP_TARGET_NOT_FOUND: &str = "target_not_found";
pub const STOP_FAILED: &str = "failed";
pub const STOP_CONDITION_UNMET: &str = "condition_unmet";
pub const STOP_FINAL_READ_FAILED: &str = "final_read_failed";
/// The run asked for more steps than the slow read path can afford.
pub const STOP_STEP_BUDGET: &str = "step_budget_exceeded";

/// A step that did not do what it was asked, with the reason in the run's own
/// vocabulary so the caller can act on the code rather than parse a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StepError {
    code: &'static str,
    detail: String,
}

impl StepError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self { code, detail: detail.into() }
    }
}

/// One command in a run, as it turned out.
#[derive(Debug, Clone, PartialEq)]
pub struct RunStep {
    /// 1-based, so it matches how the model numbers things.
    pub n: usize,
    pub op: String,
    /// Ran and had its intended effect.
    pub ok: bool,
    /// Ran at all: `false` when the run stopped before reaching it.
    pub executed: bool,
    /// What the screen did afterwards, when the step changed it.
    pub effect: Option<String>,
    /// The machine code for a failure, one of the `STOP_*` constants.
    pub code: Option<String>,
    /// Why it did not do what it was asked, in words.
    pub error: Option<String>,
}

impl RunStep {
    fn new(n: usize, op: &str) -> Self {
        Self {
            n,
            op: op.to_string(),
            ok: false,
            executed: false,
            effect: None,
            code: None,
            error: None,
        }
    }

    fn executed(mut self, effect: Option<&str>) -> Self {
        self.executed = true;
        self.ok = true;
        self.effect = effect.map(str::to_string);
        self
    }

    fn failed(mut self, failure: &StepError) -> Self {
        self.executed = true;
        self.code = Some(failure.code.to_string());
        self.error = Some(failure.detail.clone());
        self
    }

    /// A step that was never reached, and why.
    fn not_run(mut self, reason: &str) -> Self {
        self.executed = false;
        self.ok = false;
        self.error = Some(reason.to_string());
        self
    }

    /// Mark a step that ran but whose aftermath says the run must not go on.
    ///
    /// The step itself may have worked — the reason is about the phone — so the
    /// code and sentence are attached without touching `executed`.
    fn fail_in_place(&mut self, failure: &StepError) {
        self.ok = false;
        self.code = Some(failure.code.to_string());
        self.error = Some(failure.detail.clone());
    }

    fn describe(&self) -> String {
        let state = if !self.executed {
            "not run"
        } else if self.ok {
            "ok"
        } else {
            "failed"
        };
        let mut line = format!("{}. {} — {state}", self.n, self.op);
        if let Some(effect) = &self.effect {
            line.push_str(&format!(", effect={effect}"));
        }
        if let Some(error) = &self.error {
            line.push_str(&format!(": {error}"));
        }
        line
    }
}

/// What a step may name as its target.
///
/// The same three criteria `android_query` takes, because that is how a step
/// finds its target again on the screen it is actually on — not by the index
/// it had in a read three steps ago.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StepTarget {
    text: Option<String>,
    resource_id: Option<String>,
    class: Option<String>,
}

impl StepTarget {
    fn from_command(command: &Value) -> Option<Self> {
        let field = |key: &str| {
            command
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let target = Self {
            text: field("targetText"),
            resource_id: field("resourceId"),
            class: field("class"),
        };
        (target.text.is_some() || target.resource_id.is_some() || target.class.is_some())
            .then_some(target)
    }

    fn query(&self) -> Option<super::query::Query> {
        super::query::Query::new(
            self.text.as_deref(),
            self.resource_id.as_deref(),
            self.class.as_deref(),
        )
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(text) = &self.text {
            parts.push(format!("text {text:?}"));
        }
        if let Some(id) = &self.resource_id {
            parts.push(format!("resourceId {id:?}"));
        }
        if let Some(class) = &self.class {
            parts.push(format!("class {class:?}"));
        }
        parts.join(" + ")
    }
}

/// Locate a step's target on the screen as it is now.
///
/// The distinction that matters is between "nothing matched" and "several
/// matched": tapping the first of two identical rows is the silent
/// wrong-target failure, so ambiguity stops the run with the candidates named
/// rather than picking one.
fn locate_step_target(
    dump: &UiDump,
    target: &StepTarget,
    editable: bool,
) -> Result<UiElement, StepError> {
    let Some(query) = target.query() else {
        return Err(StepError::new(
            STOP_TARGET_NOT_FOUND,
            "the step names no targetText, resourceId or class",
        ));
    };
    let mut candidates: Vec<&UiElement> = dump
        .elements
        .iter()
        .filter(|element| query.matches(element))
        .filter(|element| element.enabled)
        .collect();
    // A write needs a field and a tap needs something to press. When the query
    // already names exactly one node, that node is the answer even if the
    // class was not one this code knows — the model can see more than the
    // class table does.
    if editable {
        let fields: Vec<&UiElement> = candidates
            .iter()
            .copied()
            .filter(|e| e.state.editable || e.state.password)
            .collect();
        if !fields.is_empty() {
            candidates = fields;
        }
    } else {
        let clickable: Vec<&UiElement> =
            candidates.iter().copied().filter(|e| e.clickable).collect();
        if !clickable.is_empty() {
            candidates = clickable;
        }
    }
    // An exact label wins over a superset: "发送" should not be ambiguous
    // because "发送图片" is also on the screen.
    if let Some(exact) = target.text.as_deref() {
        let exact_hits: Vec<&UiElement> = candidates
            .iter()
            .copied()
            .filter(|element| {
                element_label(element).is_some_and(|label| label.eq_ignore_ascii_case(exact))
            })
            .collect();
        if !exact_hits.is_empty() {
            candidates = exact_hits;
        }
    }
    match candidates.as_slice() {
        [] => Err(StepError::new(
            STOP_TARGET_NOT_FOUND,
            format!(
                "nothing on the screen matches {} ({} element(s) were read)",
                target.describe(),
                dump.elements.len()
            ),
        )),
        [one] => Ok((*one).clone()),
        many => {
            let names: Vec<String> = many
                .iter()
                .take(6)
                .map(|element| {
                    format!(
                        "[{}] {} {:?}",
                        element.index,
                        element.class,
                        element_label(element).unwrap_or("-")
                    )
                })
                .collect();
            Err(StepError::new(
                STOP_AMBIGUOUS_TARGET,
                format!(
                    "{} elements match {}{} — narrow it with resourceId or class, or take one \
                     of: {}",
                    many.len(),
                    target.describe(),
                    if dump.truncated {
                        " (and the tree was truncated, so this is not the whole screen)"
                    } else {
                        ""
                    },
                    names.join("; ")
                ),
            ))
        }
    }
}

/// An `android_input` call pinned to this run's device.
fn input_tool(device: &Option<String>) -> AdbInputTool {
    match device {
        Some(serial) => AdbInputTool::new().with_device(serial.clone()),
        None => AdbInputTool::new(),
    }
}

/// An `android_app_manager` call pinned to this run's device.
fn app_manager_tool(device: &Option<String>) -> AdbAppManagerTool {
    match device {
        Some(serial) => AdbAppManagerTool::new().with_device(serial.clone()),
        None => AdbAppManagerTool::new(),
    }
}

/// An `android_wait` call pinned to this run's device.
fn wait_tool(device: &Option<String>) -> AdbWaitTool {
    match device {
        Some(serial) => AdbWaitTool::new().with_device(serial.clone()),
        None => AdbWaitTool::new(),
    }
}

/// The `data` object the run's arguments carry, with the device and display
/// naming the target.
fn step_args(device: &Option<String>, display: Option<i64>) -> serde_json::Map<String, Value> {
    let mut args = serde_json::Map::new();
    if let Some(serial) = device {
        args.insert("device".to_string(), Value::String(serial.clone()));
    }
    if let Some(display) = display {
        args.insert("display".to_string(), Value::Number(display.into()));
    }
    args
}

/// Run one delegated tool and turn a refusal into a step error.
async fn call_tool(
    result: crate::Result<ToolExecutionResult>,
    op: &str,
) -> Result<ToolExecutionResult, StepError> {
    match result {
        Ok(result) if result.success => Ok(result),
        Ok(result) => Err(StepError::new(STOP_FAILED, result.error.unwrap_or(result.output))),
        Err(error) => Err(StepError::new(STOP_FAILED, format!("{op} could not run: {error}"))),
    }
}

/// Read `effect` out of a delegated tool's result.
fn effect_of(result: &ToolExecutionResult) -> Option<String> {
    result
        .data
        .as_ref()
        .and_then(|data| data.get("effect"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The `Target` a step addresses.
fn step_target(display: Option<i64>) -> Target {
    match display.and_then(|id| i32::try_from(id).ok()) {
        Some(id) => Target::Display(id),
        None => Target::Screen,
    }
}

/// Execute one command against a screen and say what happened.
///
/// Every op goes through the tool that already implements it, so a step cannot
/// do anything a single call could not: the repetition ladder, the approval
/// gate, the app lease and the safety net all still apply, one step at a time.
/// What a run saves is the model's turns, not the phone's checks.
async fn run_step(
    context: &ToolContext,
    device: &Option<String>,
    display: Option<i64>,
    command: &Value,
) -> Result<Option<String>, StepError> {
    let op = command.get("op").and_then(Value::as_str).unwrap_or("");
    let target = step_target(display);

    match op {
        "launch" => {
            let package = command
                .get("package")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| StepError::new(STOP_FAILED, "launch needs `package`"))?;
            let mut args = step_args(device, None);
            args.insert("action".to_string(), Value::String("launch".to_string()));
            args.insert("package".to_string(), Value::String(package.to_string()));
            if let Some(activity) = command.get("activity").and_then(Value::as_str) {
                args.insert("activity".to_string(), Value::String(activity.to_string()));
            }
            let result = call_tool(
                app_manager_tool(device)
                    .execute(Value::Object(args), context)
                    .await,
                "launch",
            )
            .await?;
            // `am start -W` already waited for the activity to report idle, so
            // the screen this reports is the app's first frame rather than
            // whatever was there before it.
            Ok(effect_of(&result).or_else(|| Some("launched".to_string())))
        }
        "tap" | "type" | "clear" => {
            // Each of these needs a fresh read to find its target, and that
            // read is also the picture the effect is judged against — so the
            // step reads once here and hands the input tool a point. The input
            // tool's own safety net works on indices from *its* read; a point
            // is what a run legitimately has after locating by label.
            let Some(target_spec) = StepTarget::from_command(command) else {
                return Err(StepError::new(
                    STOP_TARGET_NOT_FOUND,
                    format!("{op} needs targetText, resourceId or class"),
                ));
            };
            let dump = dump_ui(device, target).await.map_err(|failure| {
                StepError::new(
                    STOP_FAILED,
                    format!("could not read the screen: {}", failure.message),
                )
            })?;
            let editable = op != "tap";
            let element = locate_step_target(&dump, &target_spec, editable)?;
            let (x, y) = element.center;

            let mut tap_args = step_args(device, display);
            tap_args.insert("action".to_string(), Value::String("tap".to_string()));
            tap_args.insert("x".to_string(), Value::Number(x.into()));
            tap_args.insert("y".to_string(), Value::Number(y.into()));
            if op != "tap" {
                // For a write the tap only moves focus; the text step below is
                // what confirms the screen, so the two extra reads a tap's own
                // effect check would take are pure cost.
                tap_args.insert("verify".to_string(), Value::Bool(false));
            }
            let tapped = call_tool(
                input_tool(device)
                    .execute(Value::Object(tap_args), context)
                    .await,
                op,
            )
            .await?;
            if op == "tap" {
                return Ok(effect_of(&tapped));
            }

            let mut last_effect = effect_of(&tapped);
            if op == "clear" {
                let length = element
                    .text
                    .as_deref()
                    .map(str::chars)
                    .map(Iterator::count)
                    .unwrap_or(0)
                    .min(RUN_CLEAR_MAX_KEYS);
                if length > 0 {
                    // `input keyevent` takes several codes at once, so the whole
                    // clear is one dispatch rather than one per character —
                    // which also stops the repetition ladder from reading it as
                    // sixty identical actions.
                    let mut clear_args = step_args(device, display);
                    clear_args.insert("action".to_string(), Value::String("key".to_string()));
                    clear_args.insert(
                        "keycode".to_string(),
                        Value::String(vec!["KEYCODE_DEL"; length].join(" ")),
                    );
                    let cleared = call_tool(
                        input_tool(device)
                            .execute(Value::Object(clear_args), context)
                            .await,
                        "clear",
                    )
                    .await?;
                    last_effect = effect_of(&cleared).or(last_effect);
                }
                return Ok(last_effect);
            }

            let text = command
                .get("text")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| StepError::new(STOP_FAILED, "type needs `text`"))?;
            let mut type_args = step_args(device, display);
            type_args.insert("action".to_string(), Value::String("text".to_string()));
            type_args.insert("text".to_string(), Value::String(text.to_string()));
            let typed = call_tool(
                input_tool(device)
                    .execute(Value::Object(type_args), context)
                    .await,
                "type",
            )
            .await?;
            Ok(effect_of(&typed).or(last_effect))
        }
        "back" | "key" => {
            let keycode = if op == "back" {
                "KEYCODE_BACK".to_string()
            } else {
                command
                    .get("keycode")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| StepError::new(STOP_FAILED, "key needs `keycode`, e.g. ENTER"))?
                    .to_string()
            };
            let mut args = step_args(device, display);
            args.insert("action".to_string(), Value::String("key".to_string()));
            args.insert("keycode".to_string(), Value::String(keycode));
            let result = call_tool(
                input_tool(device)
                    .execute(Value::Object(args), context)
                    .await,
                op,
            )
            .await?;
            Ok(effect_of(&result))
        }
        "swipe" => {
            let mut args = step_args(device, display);
            args.insert("action".to_string(), Value::String("swipe".to_string()));
            for key in ["x", "y", "x2", "y2"] {
                let value = command.get(key).and_then(Value::as_i64).ok_or_else(|| {
                    StepError::new(STOP_FAILED, format!("swipe needs integer `{key}`"))
                })?;
                args.insert(key.to_string(), Value::Number(value.into()));
            }
            let result = call_tool(
                input_tool(device)
                    .execute(Value::Object(args), context)
                    .await,
                op,
            )
            .await?;
            Ok(effect_of(&result))
        }
        "scroll" => {
            let direction = Direction::parse(command.get("direction").and_then(Value::as_str))
                .map_err(|message| StepError::new(STOP_FAILED, message))?;
            let amount = command
                .get("amount")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, 20);
            let mut last = None;
            for _ in 0..amount {
                let dump = dump_ui(device, target).await.map_err(|failure| {
                    StepError::new(
                        STOP_FAILED,
                        format!("could not read the screen: {}", failure.message),
                    )
                })?;
                let size = screen_size(device, dump.screen).await.ok_or_else(|| {
                    StepError::new(STOP_FAILED, "the screen size is unknown, so it cannot scroll")
                })?;
                let container = scroll_container(&dump.elements, size, direction);
                let (from, to) = scroll_points(size, direction, container);
                let mut args = step_args(device, display);
                args.insert("action".to_string(), Value::String("swipe".to_string()));
                args.insert("x".to_string(), Value::Number(from.0.into()));
                args.insert("y".to_string(), Value::Number(from.1.into()));
                args.insert("x2".to_string(), Value::Number(to.0.into()));
                args.insert("y2".to_string(), Value::Number(to.1.into()));
                let result = call_tool(
                    input_tool(device)
                        .execute(Value::Object(args), context)
                        .await,
                    "scroll",
                )
                .await?;
                let effect = effect_of(&result);
                // The end of the list is not a failure, but scrolling further
                // is wasted: report where the run got to and move on.
                if effect.as_deref() == Some(EFFECT_UNCHANGED) {
                    return Ok(effect);
                }
                last = effect;
            }
            Ok(last)
        }
        "wait" => {
            let Some(query) = StepTarget::from_command(command) else {
                return Err(StepError::new(
                    STOP_CONDITION_UNMET,
                    "wait needs targetText, resourceId or class",
                ));
            };
            let present = command
                .get("present")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let timeout = command
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .unwrap_or(5_000)
                .min(RUN_INLINE_WAIT_MS);
            let mut args = step_args(device, display);
            args.insert(
                "condition".to_string(),
                Value::String(if present { "appear" } else { "disappear" }.to_string()),
            );
            if let Some(text) = &query.text {
                args.insert("text".to_string(), Value::String(text.clone()));
            }
            if let Some(id) = &query.resource_id {
                args.insert("resource_id".to_string(), Value::String(id.clone()));
            }
            if let Some(class) = &query.class {
                args.insert("class".to_string(), Value::String(class.clone()));
            }
            args.insert("timeout_ms".to_string(), Value::Number(timeout.into()));
            let result = call_tool(
                wait_tool(device)
                    .execute(Value::Object(args), context)
                    .await,
                "wait",
            )
            .await?;
            let met = result
                .data
                .as_ref()
                .and_then(|data| data.get("met"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if met {
                Ok(None)
            } else {
                Err(StepError::new(
                    STOP_CONDITION_UNMET,
                    format!(
                        "{} did not happen within {timeout} ms",
                        if present { "appearing" } else { "disappearing" }
                    ),
                ))
            }
        }
        "assert" => {
            let Some(query) = StepTarget::from_command(command) else {
                return Err(StepError::new(
                    STOP_CONDITION_UNMET,
                    "assert needs targetText, resourceId or class",
                ));
            };
            let present = command
                .get("present")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let dump = dump_ui(device, target).await.map_err(|failure| {
                StepError::new(
                    STOP_FAILED,
                    format!("could not read the screen: {}", failure.message),
                )
            })?;
            let found = query
                .query()
                .is_some_and(|query| query.count(&dump.elements) > 0);
            if found == present {
                Ok(None)
            } else {
                Err(StepError::new(
                    STOP_CONDITION_UNMET,
                    format!(
                        "expected {} to be on the screen, and it is {}",
                        query.describe(),
                        if found { "" } else { "not" }
                    ),
                ))
            }
        }
        "observe" => Ok(None),
        other => Err(StepError::new(STOP_FAILED, format!("unknown op {other:?}"))),
    }
}

/// Why the screen is not the app the step asked for, when that is the cause.
///
/// Two interruptions are not "the wrong app": a chooser means the step wanted a
/// choice nobody made, and a platform prompt means a person has to answer. They
/// are reported with their own codes because each sends the caller to a
/// different move — answer the sheet, or hand the phone to its owner — while a
/// plain app switch just means the remaining steps were written for a screen
/// that is no longer there.
fn launch_interrupted(package: &str, step: usize) -> Option<StepError> {
    if super::takeover::is_chooser(package) {
        return Some(StepError::new(
            STOP_OPENED_ELSEWHERE,
            format!(
                "step {step} opened a chooser ({package}), so the steps after it would act on \
                 the sheet — answer it yourself (android_dialog), then start a new run"
            ),
        ));
    }
    if super::takeover::is_system_overlay(package) {
        return Some(StepError::new(
            STOP_SCREEN_NEEDS_REVIEW,
            format!(
                "the system put up a prompt ({package}) — a person has to answer it \
                 (android_hand_to_user), then the run can continue"
            ),
        ));
    }
    None
}

/// Every op a command list accepts.
const RUN_OPS: [&str; 11] = [
    "launch", "tap", "type", "clear", "scroll", "swipe", "key", "back", "wait", "assert", "observe",
];

/// Run several commands in one call, re-locating each target first.
#[derive(Debug)]
pub struct AdbRunTool {
    device: Option<String>,
}

impl Default for AdbRunTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbRunTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbRunTool {
    fn name(&self) -> &str {
        "android_run"
    }

    fn description(&self) -> &str {
        "Run up to 30 phone commands in one call, finding each target on the live screen before \
         acting: one conversation turn instead of one per tap. Each step is a full read → find → \
         act → check cycle, so the result says what the screen did after every step. It stops \
         early when the app changes, an action fails, a target is missing or ambiguous, or the \
         step's condition is not met — and says which step and why. Every step still goes through \
         the same risk checks, approval gate and repetition guard that a single android_input \
         call does. Prefer this over a chain of single calls when the steps are known in advance; \
         use single calls when the next step depends on what you see."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Run several phone commands in one call",
            serde_json::json!({
                "commands": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": RUN_MAX_STEPS,
                    "description": "Ordered steps. Each target is found again on the current screen before acting.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": {
                                "type": "string",
                                "enum": ["launch", "tap", "type", "clear", "scroll", "swipe", "key", "back", "wait", "assert", "observe"],
                                "description": "What to do."
                            },
                            "targetText": {
                                "type": "string",
                                "description": "Substring of the target's text or content description."
                            },
                            "resourceId": {
                                "type": "string",
                                "description": "Substring of the target's resource id."
                            },
                            "class": {
                                "type": "string",
                                "description": "Substring of the target's class."
                            },
                            "text": { "type": "string", "description": "For 'type': the text to enter." },
                            "package": { "type": "string", "description": "For 'launch': the app to start." },
                            "activity": { "type": "string", "description": "For 'launch': an explicit activity class." },
                            "direction": {
                                "type": "string",
                                "enum": ["up", "down"],
                                "description": "For 'scroll'."
                            },
                            "amount": {
                                "type": "integer",
                                "description": "For 'scroll': how many screenfuls (default 1, max 20)."
                            },
                            "x": { "type": "integer", "description": "For 'swipe': start x." },
                            "y": { "type": "integer", "description": "For 'swipe': start y." },
                            "x2": { "type": "integer", "description": "For 'swipe': end x." },
                            "y2": { "type": "integer", "description": "For 'swipe': end y." },
                            "keycode": {
                                "type": "string",
                                "description": "For 'key': the Android keycode name, e.g. ENTER."
                            },
                            "timeoutMs": {
                                "type": "integer",
                                "description": "For 'wait': how long to wait (default 5000, max 10000)."
                            },
                            "present": {
                                "type": "boolean",
                                "description": "For 'wait' and 'assert': whether the target should be there (default true)."
                            }
                        },
                        "required": ["op"]
                    }
                },
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            vec!["commands"],
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
        let display = match target {
            Target::Display(id) => Some(i64::from(id)),
            Target::Screen => None,
        };
        let Some(commands) = args.get("commands").and_then(Value::as_array) else {
            return Ok(ToolExecutionResult::error("`commands` must be an array of steps"));
        };
        if commands.is_empty() {
            return Ok(ToolExecutionResult::error("`commands` needs at least one step"));
        }
        if commands.len() > RUN_MAX_STEPS {
            return Ok(ToolExecutionResult::error(format!(
                "`commands` takes at most {RUN_MAX_STEPS} steps, got {} — split the work into \
                 smaller runs and check the screen between them",
                commands.len()
            )));
        }
        if let Some(bad) = commands.iter().find(|command| {
            command
                .get("op")
                .and_then(Value::as_str)
                .is_none_or(|op| !RUN_OPS.contains(&op))
        }) {
            return Ok(ToolExecutionResult::error(format!(
                "every step needs an `op`, one of {}; got {bad}",
                RUN_OPS.join(", ")
            )));
        }

        // The first read is the baseline: the app the run started in is what
        // "the app changed" is measured against, and the path it was read
        // through decides how many steps this run can afford.
        let opening = match dump_ui(&device, target).await {
            Ok(dump) => dump,
            Err(failure) => {
                return Ok(ToolExecutionResult::error(format!(
                    "could not read the screen to start the run: {}",
                    failure.message
                ))
                .with_data(serde_json::json!({ "code": failure.code })))
            }
        };
        let source = opening.source;
        if source == "uiautomator" && commands.len() > RUN_MAX_STEPS_FALLBACK {
            return Ok(ToolExecutionResult::error(format!(
                "This phone is being read through `uiautomator`, which costs a subprocess and an \
                 SD-card round trip per read, so a run is limited to {RUN_MAX_STEPS_FALLBACK} \
                 steps (got {}). Turn Syscity's accessibility service on for the cheap path, or \
                 split this into runs of at most {RUN_MAX_STEPS_FALLBACK}.",
                commands.len()
            ))
            .with_data(serde_json::json!({
                "code": STOP_STEP_BUDGET,
                "read_source": source,
                "max_steps": RUN_MAX_STEPS_FALLBACK,
            })));
        }

        let mut expected_package = opening.package.clone();
        let mut steps: Vec<RunStep> = Vec::with_capacity(commands.len());
        let mut stop: Option<(String, usize)> = None;
        let mut unreadable = 0_usize;

        for (index, command) in commands.iter().enumerate() {
            let n = index + 1;
            let op = command
                .get("op")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            let mut step = match run_step(context, &device, display, command).await {
                Ok(effect) => RunStep::new(n, &op).executed(effect.as_deref()),
                Err(failure) => {
                    let code = failure.code.to_string();
                    steps.push(RunStep::new(n, &op).failed(&failure));
                    stop = Some((code, n));
                    break;
                }
            };

            if op == "observe" {
                steps.push(step);
                continue;
            }

            // A `launch` is *supposed* to change the app, so it is the one step
            // whose aftermath cannot be judged against the app it started in.
            // The new app is checked for the two ways a launch is not what was
            // asked for — a chooser it cannot answer, a prompt only a person
            // can — and otherwise becomes the baseline for the steps after it.
            if op == "launch" {
                match dump_ui(&device, target).await {
                    Ok(after) => {
                        if let Some(now) = after.package.as_deref() {
                            if let Some(failure) = launch_interrupted(now, n) {
                                step.fail_in_place(&failure);
                                stop = Some((failure.code.to_string(), n));
                                steps.push(step);
                                break;
                            }
                            expected_package = Some(now.to_string());
                        }
                        steps.push(step);
                    }
                    Err(_) => {
                        // A launch that cannot be read back changed nothing for
                        // this run's purposes; the next step's own read will say
                        // where it got to. Three in a row is a dead phone.
                        unreadable += 1;
                        if unreadable >= 3 {
                            let failure = StepError::new(
                                STOP_SCREEN_NEEDS_REVIEW,
                                "the screen stopped being readable for three steps in a row",
                            );
                            step.fail_in_place(&failure);
                            stop = Some((STOP_SCREEN_NEEDS_REVIEW.to_string(), n));
                            steps.push(step);
                            break;
                        }
                        steps.push(step);
                    }
                }
                continue;
            }

            // After an acting step, ask the phone whether the run can go on.
            // Doing this per step is what makes the next step's read safe: the
            // whole premise of a run is that each target is found again, and a
            // different app is a different set of targets.
            let after = match dump_ui(&device, target).await {
                Ok(dump) => {
                    unreadable = 0;
                    dump
                }
                Err(_) => {
                    unreadable += 1;
                    // One unreadable screen is a transition; three in a row is
                    // a phone that needs a person.
                    if unreadable >= 3 {
                        let failure = StepError::new(
                            STOP_SCREEN_NEEDS_REVIEW,
                            "the screen stopped being readable for three steps in a row",
                        );
                        step.fail_in_place(&failure);
                        stop = Some((STOP_SCREEN_NEEDS_REVIEW.to_string(), n));
                    }
                    steps.push(step);
                    if stop.is_some() {
                        break;
                    }
                    continue;
                }
            };

            // Every check below ends the run; the step it is attached to is
            // the one whose aftermath was caught.
            let halted = match (expected_package.as_deref(), after.package.as_deref()) {
                (Some(before), Some(now)) if before != now => match launch_interrupted(now, n) {
                    Some(opened) => Some(opened),
                    None => Some(StepError::new(
                        STOP_APP_SWITCHED,
                        format!(
                            "the screen was in {before} and is now in {now}, so the remaining \
                             steps no longer target the screen they were written for"
                        ),
                    )),
                },
                _ => None,
            };
            // A screen the tree cannot describe is a screen the next step's
            // locate cannot work on. Saying so now, with the app named, is more
            // useful than thirty identical failures.
            let halted = halted.or_else(|| {
                (assess_tree(&after.elements) == TreeQuality::Unusable).then(|| {
                    StepError::new(
                        STOP_SCREEN_NEEDS_REVIEW,
                        "the accessibility tree describes none of this screen — read the \
                         screenshot (android_observe) before continuing",
                    )
                })
            });
            // An action on a display can start something that lands on the
            // user's own screen: theirs stops being private, and this run stops
            // being about the display it was given.
            let halted = match halted {
                Some(halted) => Some(halted),
                None => match target {
                    Target::Display(id) => match display_bridge() {
                        Ok(bridge) => {
                            match super::takeover::check_spill(&bridge, id, None, false).await {
                                Some(spill) => Some(StepError::new(
                                    STOP_OPENED_ELSEWHERE,
                                    format!(
                                        "{} ended up on the user's screen ({}) rather than \
                                         display {id}",
                                        spill.package, spill.reason
                                    ),
                                )),
                                None => None,
                            }
                        }
                        Err(_) => None,
                    },
                    Target::Screen => None,
                },
            };

            match halted {
                Some(failure) => {
                    step.fail_in_place(&failure);
                    stop = Some((failure.code.to_string(), n));
                    steps.push(step);
                    break;
                }
                None => steps.push(step),
            }
        }

        // The commands the run never reached are still part of its shape: a
        // caller reading a stop needs to see what was left undone, not infer it
        // from a short list.
        if let Some((code, at)) = &stop {
            let why = format!("not run: the run stopped at step {at} ({code})");
            for (index, command) in commands.iter().enumerate().skip(*at) {
                let op = command.get("op").and_then(Value::as_str).unwrap_or("");
                steps.push(RunStep::new(index + 1, op).not_run(&why));
            }
        }

        // The last read is what the caller acts on next, so it is taken even
        // when the run reached its last command cleanly.
        let final_read = dump_ui(&device, target).await;
        let serial = resolved_serial(&device).await;
        let total = commands.len();
        let stopped = stop.as_ref().map(|(code, at)| (code.as_str(), *at));

        let (success, text) = match (&final_read, &stop) {
            (Ok(dump), None) => {
                let mut text = format!(
                    "Ran all {total} step(s) through the {source} read path:\n{}",
                    steps
                        .iter()
                        .map(RunStep::describe)
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                if source == "uiautomator" {
                    text.push_str(
                        "\n\nThis run read the screen through `uiautomator`, which is the slow \
                         path — turning Syscity's accessibility service on makes runs cheaper and \
                         longer.",
                    );
                }
                text.push_str("\n\nThe screen is now:\n\n");
                text.push_str(&format_ui_summary(&dump.elements));
                if dump.truncated {
                    text.push_str(
                        "\n\nThe tree was truncated (more nodes than one read carries), so \
                         elements below are missing.",
                    );
                }
                (true, text)
            }
            (_, Some((code, at))) => {
                let mut text = format!("Stopped at step {at} of {total}: {}.", stop_sentence(code));
                text.push_str(&format!(
                    "\n\n{}",
                    steps
                        .iter()
                        .map(RunStep::describe)
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
                match &final_read {
                    Ok(dump) => {
                        text.push_str("\n\nThe screen is now:\n\n");
                        text.push_str(&format_ui_summary(&dump.elements));
                    }
                    Err(failure) => text.push_str(&format!(
                        "\n\nThe screen could not be read afterwards ({}).",
                        failure.message
                    )),
                }
                (false, text)
            }
            (Err(failure), None) => (
                false,
                format!(
                    "Ran all {total} step(s), but the screen could not be read afterwards ({}).",
                    failure.message
                ),
            ),
        };

        let result = if success {
            ToolExecutionResult::success(text)
        } else {
            ToolExecutionResult::error(text)
        };
        Ok(result.with_data(run_data(
            &steps,
            total,
            stopped,
            final_read.as_ref().ok(),
            source,
            serial,
        )))
    }
}

/// The JSON a run reports, beside its text answer.
fn run_data(
    steps: &[RunStep],
    total: usize,
    stopped: Option<(&str, usize)>,
    final_read: Option<&UiDump>,
    source: &'static str,
    device: Option<String>,
) -> Value {
    serde_json::json!({
        "ok": stopped.is_none(),
        "stopped": stopped.map(|(reason, step)| {
            serde_json::json!({ "reason": reason, "step": step })
        }),
        "completed": steps.iter().filter(|step| step.ok).count(),
        "total": total,
        "steps": steps.iter().map(|step| serde_json::json!({
            "n": step.n,
            "op": step.op,
            "ok": step.ok,
            "executed": step.executed,
            "effect": step.effect,
            "code": step.code,
            "error": step.error,
        })).collect::<Vec<_>>(),
        // Which reader the run worked through. `accessibility` is the phone's
        // own service and costs a bridge call per read; `uiautomator` is ADB's
        // dump and costs a subprocess.
        "read_source": source,
        "max_steps": if source == "uiautomator" { RUN_MAX_STEPS_FALLBACK } else { RUN_MAX_STEPS },
        "truncated": final_read.is_some_and(|dump| dump.truncated),
        "elements": final_read.map(|dump| dump.elements.clone()),
        "device": device,
    })
}

/// What a stop reason means, in a sentence.
fn stop_sentence(code: &str) -> &'static str {
    match code {
        STOP_APP_SWITCHED => {
            "the screen changed app, so the rest of the steps no longer target the screen they \
             were written for"
        }
        STOP_OPENED_ELSEWHERE => "an app opened on the user's screen rather than this display",
        STOP_SCREEN_NEEDS_REVIEW => {
            "the accessibility tree cannot describe this screen — read it (the screenshot, or OCR) \
             before continuing"
        }
        STOP_AMBIGUOUS_TARGET => {
            "more than one element matched, so acting on the first would be a guess — narrow the \
             target with text, resourceId and class together"
        }
        STOP_TARGET_NOT_FOUND => "the target was not on the screen this step read",
        STOP_CONDITION_UNMET => "the screen was not in the state the step needed",
        STOP_FINAL_READ_FAILED => "the screen could not be read after the last step",
        STOP_STEP_BUDGET => "the run is longer than the slow read path can afford",
        _ => "a step failed",
    }
}

// ── Extract ────────────────────────────────────────────────────────────────

/// The most characters an extract may return.
///
/// A long chat list can hold far more text than a turn can usefully carry, and
/// a tool result that fills the context crowds out the task itself. Over the
/// ceiling the answer is cut at a line boundary and says so, which is a fact
/// the caller can act on — reading the rest with a scroll — rather than a
/// silent shortening.
pub const EXTRACT_MAX_CHARS: usize = 50_000;

/// One row of a list, as several elements that share a horizontal band.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ExtractRow {
    /// 1-based, top to bottom.
    pub index: usize,
    /// Centre y, the band the row occupies.
    pub y: i32,
    /// Every readable label in the row, left to right.
    pub texts: Vec<String>,
    /// Resource ids of the elements that carried them, deduplicated and in the
    /// same order as `texts`.
    pub resource_ids: Vec<String>,
}

/// One field of a form.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FormField {
    /// 1-based, top to bottom.
    pub index: usize,
    /// What the field is called, from the label beside it — or, when the screen
    /// has none, from the field's own hint / resource id.
    pub label: String,
    /// Text can be typed into it.
    pub editable: bool,
    /// What kind of value it expects, inferred from the label.
    pub kind: &'static str,
    /// What it holds now. Omitted for a password field: its contents are never
    /// part of a read, whatever the model means to do with them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// The centre, for a tap that focuses it.
    pub center: (i32, i32),
    pub resource_id: Option<String>,
}

/// How rows are grouped: two elements share a row when their vertical extents
/// overlap by at least half of the shorter one.
///
/// The centre alone is too strict on Android, where a tall label and a short
/// icon in the same row can have centres a hundred pixels apart; the extents
/// say what a person sees, which is that the two sit on one line.
fn same_row(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    let overlap = a.3.min(b.3) - a.1.max(b.1);
    let shorter = (a.3 - a.1).min(b.3 - b.1).max(1);
    overlap * 2 >= shorter
}

/// Group the readable elements of a tree into rows, top to bottom.
pub fn extract_rows(elements: &[UiElement]) -> Vec<ExtractRow> {
    let mut named: Vec<&UiElement> = elements
        .iter()
        .filter(|element| !element.state.password)
        .filter(|element| element_label(element).is_some())
        .collect();
    named.sort_by_key(|element| (element.bounds.1, element.bounds.0));

    let mut rows: Vec<(i32, Vec<&UiElement>)> = Vec::new();
    for element in named {
        match rows.iter_mut().find(|(_, members)| {
            members
                .last()
                .is_some_and(|last| same_row(last.bounds, element.bounds))
        }) {
            Some((_, members)) => members.push(element),
            None => rows.push((element.bounds.1, vec![element])),
        }
    }

    rows.iter()
        .enumerate()
        .map(|(index, (_, members))| {
            let texts: Vec<String> = members
                .iter()
                .filter_map(|element| element_label(element).map(str::to_string))
                .collect();
            let mut resource_ids: Vec<String> = Vec::new();
            for id in members
                .iter()
                .filter_map(|element| element.resource_id.clone())
            {
                if !resource_ids.contains(&id) {
                    resource_ids.push(id);
                }
            }
            let y = members
                .iter()
                .map(|element| element.center.1)
                .min()
                .unwrap_or_default();
            ExtractRow {
                index: index + 1,
                y,
                texts,
                resource_ids,
            }
        })
        .collect()
}

/// What kind of value a field asks for, from the words naming it.
///
/// Deliberately small: a hint that guesses wrong is worse than "text", because
/// the caller then writes the wrong shape into the field.
pub fn infer_field_kind(label: &str) -> &'static str {
    let lowered = label.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| lowered.contains(needle));
    if has(&["密码", "password", "passcode"]) {
        "password"
    } else if has(&["手机", "电话", "号码", "phone", "mobile"]) {
        "phone"
    } else if has(&["邮箱", "邮件", "email", "e-mail"]) {
        "email"
    } else if has(&["验证码", "code", "captcha"]) {
        "code"
    } else if has(&["搜索", "查找", "search", "find"]) {
        "search"
    } else if has(&[
        "数量", "金额", "价格", "number", "amount", "quantity", "price",
    ]) {
        "number"
    } else {
        "text"
    }
}

/// The label that belongs to a field, found on its own row or just above it.
fn field_label(field: &UiElement, elements: &[UiElement]) -> Option<String> {
    let mut best: Option<(&UiElement, i32)> = None;
    for other in elements {
        if std::ptr::eq(other, field) || element_label(other).is_none() {
            continue;
        }
        let distance = if same_row(field.bounds, other.bounds) {
            // On the same row, the label is the one to its left.
            if other.bounds.2 <= field.bounds.0 {
                field.bounds.0 - other.bounds.2
            } else if other.bounds.0 >= field.bounds.2 {
                // A control to the right is not this field's label.
                continue;
            } else {
                0
            }
        } else if other.bounds.3 <= field.bounds.1 {
            // Above it: how far above, but only a line or two.
            let gap = field.bounds.1 - other.bounds.3;
            if gap > 120 {
                continue;
            }
            gap + 1_000
        } else {
            continue;
        };
        if best.is_none_or(|(_, current)| distance < current) {
            best = Some((other, distance));
        }
    }
    best.and_then(|(element, _)| element_label(element))
        .map(str::to_string)
}

/// The fields of a form, top to bottom.
pub fn extract_fields(elements: &[UiElement]) -> Vec<FormField> {
    let mut fields: Vec<&UiElement> = elements
        .iter()
        .filter(|element| element.state.editable || element.state.password)
        .collect();
    fields.sort_by_key(|element| (element.bounds.1, element.bounds.0));
    fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let label = field_label(field, elements)
                .or_else(|| {
                    field
                        .resource_id
                        .as_deref()
                        .and_then(|id| id.rsplit('/').next())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| format!("field {}", index + 1));
            FormField {
                index: index + 1,
                // The node's own flag is authoritative: a password box is a
                // password box whatever its label says (and its label is often
                // "请输入" on every field).
                kind: if field.state.password {
                    "password"
                } else {
                    infer_field_kind(&label)
                },
                label,
                editable: field.state.editable,
                current: if field.state.password {
                    None
                } else {
                    field.text.clone().filter(|text| !text.is_empty())
                },
                center: field.center,
                resource_id: field.resource_id.clone(),
            }
        })
        .collect()
}

/// The tree names a list rather than a form.
fn looks_like_a_list(elements: &[UiElement]) -> bool {
    elements.iter().any(|element| {
        element.state.scrollable
            || element.class.contains("RecyclerView")
            || element.class.contains("ListView")
            || element.class.contains("LazyColumn")
    })
}

/// Read a screen as text, as rows, or as form fields.
#[derive(Debug)]
pub struct AdbExtractTool {
    device: Option<String>,
}

impl Default for AdbExtractTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbExtractTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbExtractTool {
    fn name(&self) -> &str {
        "android_extract"
    }

    fn description(&self) -> &str {
        "Read the current screen as structured data instead of a list of elements. \
         `mode: text` returns the readable text in reading order; `list` returns one entry per \
         row of a list, with all the text on that row together; `form` returns one entry per \
         input field, with the label beside it and what kind of value it expects. Use this when \
         the answer is 'what does this screen say' rather than 'where do I tap'."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Extract the screen as text, rows or form fields",
            serde_json::json!({
                "mode": {
                    "type": "string",
                    "enum": ["text", "list", "form"],
                    "description": "text: readable lines in order. list: one entry per row, requiring a scrolling list. form: one entry per input field."
                },
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
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let mode = args.get("mode").and_then(Value::as_str).unwrap_or("text");
        if !matches!(mode, "text" | "list" | "form") {
            return Ok(ToolExecutionResult::error(format!(
                "`mode` must be text, list or form, not {mode:?}"
            )));
        }
        let dump = match dump_ui(&device, target).await {
            Ok(dump) => dump,
            Err(failure) => {
                return Ok(ToolExecutionResult::error(failure.message)
                    .with_data(serde_json::json!({ "code": failure.code })))
            }
        };
        if dump.elements.is_empty() {
            return Ok(ToolExecutionResult::success(
                "This screen has no elements to extract — read the screenshot with android_observe \
                 (it falls back to OCR) instead.",
            )
            .with_data(serde_json::json!({ "mode": mode, "count": 0 })));
        }

        let serial = resolved_serial(&device).await;
        match mode {
            "list" => {
                if !looks_like_a_list(&dump.elements) {
                    return Ok(ToolExecutionResult::success(
                        "This screen is not a scrolling list — no scrollable node, RecyclerView or \
                         ListView is present. Use mode 'text' for reading order, 'form' for input \
                         fields, or android_query to find a specific element.",
                    )
                    .with_data(serde_json::json!({
                        "mode": mode,
                        "items": [],
                        "device": serial,
                    })));
                }
                let rows = extract_rows(&dump.elements);
                let (items, truncated) = cap_rows(&rows, EXTRACT_MAX_CHARS);
                let text = list_text(&items, truncated);
                Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
                    "mode": mode,
                    "count": items.len(),
                    "items": items,
                    "truncated": truncated,
                    "source": dump.source,
                    "device": serial,
                })))
            }
            "form" => {
                let fields = extract_fields(&dump.elements);
                if fields.is_empty() {
                    return Ok(ToolExecutionResult::success(
                        "This screen has no input fields — nothing to fill in here.",
                    )
                    .with_data(serde_json::json!({
                        "mode": mode,
                        "fields": [],
                        "device": serial,
                    })));
                }
                let mut text = format!("{} field(s):", fields.len());
                for field in &fields {
                    text.push_str(&format!("\n{}. {} [{}]", field.index, field.label, field.kind));
                    if let Some(current) = &field.current {
                        text.push_str(&format!(" = {current:?}"));
                    } else if field.kind == "password" {
                        text.push_str(" (password — contents not read)");
                    } else {
                        text.push_str(" (empty)");
                    }
                    text.push_str(&format!(" center=({},{})", field.center.0, field.center.1));
                }
                let (fields, truncated) = cap_fields(fields, EXTRACT_MAX_CHARS);
                Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
                    "mode": mode,
                    "count": fields.len(),
                    "fields": fields,
                    "truncated": truncated,
                    "source": dump.source,
                    "device": serial,
                })))
            }
            _ => {
                let lines: Vec<String> = {
                    let rows = extract_rows(&dump.elements);
                    let mut seen = std::collections::HashSet::new();
                    rows.into_iter()
                        .flat_map(|row| row.texts)
                        .filter(|line| seen.insert(line.clone()))
                        .collect()
                };
                let (lines, truncated) = cap_lines(lines, EXTRACT_MAX_CHARS);
                let mut text = format!("{} line(s):", lines.len());
                for line in &lines {
                    text.push('\n');
                    text.push_str(line);
                }
                if truncated {
                    text.push_str(
                        "\n\n[truncated — the screen holds more text than one extract carries; \
                         scroll and extract again for the rest]",
                    );
                }
                Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
                    "mode": mode,
                    "count": lines.len(),
                    "lines": lines,
                    "truncated": truncated,
                    "source": dump.source,
                    "device": serial,
                })))
            }
        }
    }
}

/// How many characters a value costs in the result, for the budget.
fn cost(text: &str) -> usize {
    text.chars().count() + 1
}

/// Cut a list of lines at the character budget.
fn cap_lines(lines: Vec<String>, budget: usize) -> (Vec<String>, bool) {
    let mut kept = Vec::new();
    let mut used = 0;
    for line in lines {
        let this = cost(&line);
        if used + this > budget {
            return (kept, true);
        }
        used += this;
        kept.push(line);
    }
    (kept, false)
}

/// Cut rows at the character budget.
fn cap_rows(rows: &[ExtractRow], budget: usize) -> (Vec<ExtractRow>, bool) {
    let mut kept = Vec::new();
    let mut used = 0;
    for row in rows {
        let this: usize = row.texts.iter().map(|text| cost(text)).sum();
        if used + this > budget {
            return (kept, true);
        }
        used += this;
        kept.push(row.clone());
    }
    (kept, false)
}

/// Cut form fields at the character budget.
fn cap_fields(fields: Vec<FormField>, budget: usize) -> (Vec<FormField>, bool) {
    let mut kept = Vec::new();
    let mut used = 0;
    for field in fields {
        let this = cost(&field.label) + field.current.as_deref().map(cost).unwrap_or(0);
        if used + this > budget {
            return (kept, true);
        }
        used += this;
        kept.push(field);
    }
    (kept, false)
}

/// The result text for a set of rows.
fn list_text(rows: &[ExtractRow], truncated: bool) -> String {
    if rows.is_empty() {
        return "The list is on screen but carries no readable rows.".to_string();
    }
    let mut text = format!("{} row(s):", rows.len());
    for row in rows {
        text.push_str(&format!("\n{}. {}", row.index, row.texts.join(" · ")));
    }
    if truncated {
        text.push_str(
            "\n\n[truncated — the list holds more rows than one extract carries; scroll and \
             extract again for the rest]",
        );
    }
    text
}

// ── Query ──────────────────────────────────────────────────────────────────

/// Find controls on the current screen without reading the whole tree.
#[derive(Debug)]
pub struct AdbQueryTool {
    device: Option<String>,
}

impl Default for AdbQueryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbQueryTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// How many matches a query returns by default.
const QUERY_DEFAULT_LIMIT: usize = 30;
/// The most a query will return.
const QUERY_MAX_LIMIT: usize = 100;

/// Longest label quoted back in a query's text answer.
const QUERY_LABEL_CHARS: usize = 80;

#[async_trait]
impl Tool for AdbQueryTool {
    fn name(&self) -> &str {
        "android_query"
    }

    fn description(&self) -> &str {
        "Find elements on the current screen by text, resource id or class, without reading the \
         whole UI tree into the conversation. Every criterion is a case-insensitive substring and \
         they are combined, so `text: \"alice\"` and `class: \"TextView\"` narrows a name to the \
         row that draws it. Returns each match with its index, label, class and centre, plus how \
         many matched in total. Use it to check whether something is on screen, to count the rows \
         of a list, or to get the coordinates of one control — it is much cheaper than \
         android_ui_tree when you know what you are looking for."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Find elements on the current screen",
            serde_json::json!({
                "text": {
                    "type": "string",
                    "description": "Substring of the element's text or content description (case-insensitive)."
                },
                "resource_id": {
                    "type": "string",
                    "description": "Substring of the element's Android resource id, e.g. 'id/send'."
                },
                "class": {
                    "type": "string",
                    "description": "Substring of the element's class, e.g. 'Button' or 'TextView'."
                },
                "limit": {
                    "type": "integer",
                    "description": "How many matches to return (default 30, max 100). The total is reported either way."
                },
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
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let text = args.get("text").and_then(|v| v.as_str());
        let resource_id = args.get("resource_id").and_then(|v| v.as_str());
        let class = args.get("class").and_then(|v| v.as_str());
        let Some(query) = super::query::Query::new(text, resource_id, class) else {
            return Ok(ToolExecutionResult::error(
                "give at least one of `text`, `resource_id` or `class` — with none of them every \
                 element matches and the answer would be the whole screen",
            ));
        };
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n.clamp(1, QUERY_MAX_LIMIT as u64) as usize)
            .unwrap_or(QUERY_DEFAULT_LIMIT);

        let dump = match dump_ui(&device, target).await {
            Ok(dump) => dump,
            Err(failure) => {
                return Ok(ToolExecutionResult::error(failure.message)
                    .with_data(serde_json::json!({ "code": failure.code })))
            }
        };
        let total = query.count(&dump.elements);
        let found = query.find(&dump.elements, limit);
        // Two different truncations, and a caller has to tell them apart:
        // `matched` means more matched than `limit` returned, `truncated` means
        // the *tree itself* was a prefix of the screen, so `total` is a lower
        // bound and may not even contain the element being looked for.
        let matched_more = total > found.len();

        let text_out = if total == 0 {
            format!(
                "No element on this screen matches. The screen has {} element(s); read it with \
                 android_ui_tree if the label may be worded differently.",
                dump.elements.len()
            )
        } else {
            let mut lines = vec![format!(
                "{total} match(es){}:",
                if matched_more {
                    format!(", the first {}", found.len())
                } else {
                    String::new()
                }
            )];
            for element in &found {
                let label = element_label(element).unwrap_or("-");
                let label: String = label.chars().take(QUERY_LABEL_CHARS).collect();
                lines.push(format!(
                    "[{}] {} {:?} center=({},{}){}",
                    element.index,
                    element.class,
                    label,
                    element.center.0,
                    element.center.1,
                    if element.clickable { " clickable" } else { "" }
                ));
            }
            if dump.truncated {
                lines.push(
                    "The tree was truncated (the screen has more nodes than one read carries), \
                     so this count may be short and a match may be missing."
                        .to_string(),
                );
            }
            lines.join("\n")
        };

        Ok(ToolExecutionResult::success(text_out).with_data(serde_json::json!({
            "total": total,
            "returned": found.len(),
            // More matched than were returned — the caller narrowed with
            // `limit`, not with the query.
            "matched_more": matched_more,
            // The source capped the tree, so the screen itself was not fully
            // seen. Ambiguity detection has to treat this as "unknown", not as
            // "exactly one".
            "truncated": dump.truncated,
            "matches": found,
            "source": dump.source,
            "device": resolved_serial(&device).await,
        })))
    }
}

// ── Wait ───────────────────────────────────────────────────────────────────

/// Wait for a control to appear or go, or for the screen to settle.
#[derive(Debug)]
pub struct AdbWaitTool {
    device: Option<String>,
}

impl Default for AdbWaitTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbWaitTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// How often the screen is re-read while waiting.
///
/// The tree is the only signal: polling faster than a person could see a change
/// just spends reads, and the whole point of the tool is to replace the model's
/// own "observe, then observe again" loop.
const WAIT_POLL_MS: u64 = 250;
/// Default budget for a wait.
const WAIT_DEFAULT_MS: u64 = 10_000;
/// Upper bound on a wait.
///
/// Deliberately under the 120 s tool budget an agent's context carries, so a
/// wait that runs out still returns an answer the model can act on.
const WAIT_MAX_MS: u64 = 30_000;

#[async_trait]
impl Tool for AdbWaitTool {
    fn name(&self) -> &str {
        "android_wait"
    }

    fn description(&self) -> &str {
        "Wait for the screen to reach a state, instead of reading it again a second later and \
         hoping. `condition: appear` waits for an element matching `text`/`resource_id`/`class` to \
         show up (a result, a button, a loaded list); `disappear` waits for it to go (a spinner, a \
         dialog); `stable` waits for the screen to stop changing. Returns whether it got there, \
         how long it waited, and the screen as it was at that moment. When it times out it says \
         so and shows what is actually on screen, so a failure is information rather than another \
         blind retry."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Wait for a screen condition",
            serde_json::json!({
                "condition": {
                    "type": "string",
                    "description": "appear (default), disappear, or stable.",
                    "enum": ["appear", "disappear", "stable"]
                },
                "text": {
                    "type": "string",
                    "description": "For appear/disappear: substring of the element's text or content description."
                },
                "resource_id": {
                    "type": "string",
                    "description": "For appear/disappear: substring of the element's resource id."
                },
                "class": {
                    "type": "string",
                    "description": "For appear/disappear: substring of the element's class."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "How long to wait (default 10000, max 30000)."
                },
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
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let condition = match super::query::Condition::parse(
            args.get("condition")
                .and_then(|v| v.as_str())
                .unwrap_or("appear"),
        ) {
            Ok(condition) => condition,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let query = super::query::Query::new(
            args.get("text").and_then(|v| v.as_str()),
            args.get("resource_id").and_then(|v| v.as_str()),
            args.get("class").and_then(|v| v.as_str()),
        );
        if condition.needs_query() && query.is_none() {
            return Ok(ToolExecutionResult::error(format!(
                "condition {:?} needs something to look for: set `text`, `resource_id` or \
                 `class`. Use condition 'stable' to wait for the screen to stop changing.",
                condition.label()
            )));
        }
        let budget = Duration::from_millis(
            args.get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(WAIT_DEFAULT_MS)
                .clamp(WAIT_POLL_MS, WAIT_MAX_MS),
        );
        let poll = Duration::from_millis(WAIT_POLL_MS);
        let started = std::time::Instant::now();
        let mut previous: Option<u64> = None;
        let mut reads = 0_u32;

        loop {
            let dump = match dump_ui(&device, target).await {
                Ok(dump) => dump,
                Err(failure) => {
                    return Ok(ToolExecutionResult::error(format!(
                        "could not read the screen while waiting: {}",
                        failure.message
                    ))
                    .with_data(serde_json::json!({ "code": failure.code })))
                }
            };
            reads += 1;
            let matches = query.as_ref().map_or(0, |q| q.count(&dump.elements));
            let fingerprint = tree_fingerprint(&dump.elements);
            let seen_twice = previous == Some(fingerprint);
            previous = Some(fingerprint);

            if condition.met(matches, seen_twice) {
                let waited = started.elapsed().as_millis() as u64;
                let mut text = match condition {
                    super::query::Condition::Appear => {
                        format!("It appeared after {waited} ms: {matches} element(s) match.")
                    }
                    super::query::Condition::Disappear => {
                        format!("It is gone (after {waited} ms): nothing matches now.")
                    }
                    super::query::Condition::Stable => format!(
                        "The screen settled after {waited} ms ({} element(s)).",
                        dump.elements.len()
                    ),
                };
                if let Some(found) = query.as_ref().and_then(|q| q.find(&dump.elements, 1).pop()) {
                    text.push_str(&format!(
                        "\n[{}] {} {:?} center=({},{})",
                        found.index,
                        found.class,
                        element_label(found).unwrap_or("-"),
                        found.center.0,
                        found.center.1
                    ));
                }
                return Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
                    "met": true,
                    "condition": condition.label(),
                    "matches": matches,
                    "waited_ms": waited,
                    "reads": reads,
                    "elements": dump.elements,
                    "device": resolved_serial(&device).await,
                })));
            }

            if started.elapsed() >= budget {
                let waited = started.elapsed().as_millis() as u64;
                let text = match condition {
                    super::query::Condition::Appear => format!(
                        "It did not appear within {waited} ms. The screen shows {} element(s); \
                         read it with android_ui_tree to see what is there.",
                        dump.elements.len()
                    ),
                    super::query::Condition::Disappear => format!(
                        "It is still there after {waited} ms ({matches} match(es)). The screen may \
                         be waiting on something else; read it with android_ui_tree."
                    ),
                    super::query::Condition::Stable => format!(
                        "The screen was still changing after {waited} ms; it may be animating or \
                         loading. Read it with android_ui_tree to see where it got to."
                    ),
                };
                return Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
                    "met": false,
                    "condition": condition.label(),
                    "matches": matches,
                    "waited_ms": waited,
                    "reads": reads,
                    "elements": dump.elements,
                    "device": resolved_serial(&device).await,
                })));
            }
            tokio::time::sleep(poll).await;
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

    fn scrollable(depth: u32, bounds: (i32, i32, i32, i32)) -> UiElement {
        let mut element =
            UiElement::new(1, None, None, None, "RecyclerView".to_string(), false, true, bounds);
        element.state.scrollable = true;
        element.state.scroll_forward = true;
        element.state.scroll_backward = true;
        element.state.depth = depth;
        element
    }

    #[test]
    fn a_scroll_prefers_the_innermost_container_covering_the_middle() {
        // An outer container covering the whole screen and the message list
        // inside it. A "scroll down" is about the list, not about everything.
        let outer = scrollable(1, (0, 0, 1080, 2340));
        let list = scrollable(3, (0, 300, 1080, 2000));
        let elements = vec![outer, list];
        let picked =
            scroll_container(&elements, (1080, 2340), Direction::Down).expect("a container");
        assert_eq!(picked.state.depth, 3);
    }

    #[test]
    fn a_container_that_does_not_cover_the_middle_is_not_picked() {
        // A sideways carousel near the top: it scrolls, but not the page.
        let carousel = scrollable(2, (0, 0, 1080, 400));
        let elements = vec![carousel];
        assert!(scroll_container(&elements, (1080, 2340), Direction::Down).is_none());
    }

    #[test]
    fn a_one_direction_container_only_takes_that_direction() {
        let mut up_only = scrollable(2, (0, 100, 1080, 2300));
        up_only.state.scroll_forward = false;
        let elements = vec![up_only];
        assert!(scroll_container(&elements, (1080, 2340), Direction::Down).is_none());
        assert!(scroll_container(&elements, (1080, 2340), Direction::Up).is_some());
    }

    #[test]
    fn a_tree_without_a_container_leaves_the_swipe_in_charge() {
        let mut label = UiElement::new(
            1,
            Some("Hello".to_string()),
            None,
            None,
            "TextView".to_string(),
            false,
            true,
            (0, 0, 200, 60),
        );
        label.state.depth = 2;
        assert!(scroll_container(&[label], (1080, 2340), Direction::Down).is_none());
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
        assert_eq!(AdbDisplayTool::new().name(), "android_display");
        assert_eq!(AdbQueryTool::new().name(), "android_query");
        assert_eq!(AdbWaitTool::new().name(), "android_wait");
        assert_eq!(AdbRunTool::new().name(), "android_run");
        assert_eq!(AdbExtractTool::new().name(), "android_extract");
    }

    fn element(
        index: usize,
        text: &str,
        class: &str,
        clickable: bool,
        bounds: (i32, i32, i32, i32),
    ) -> UiElement {
        UiElement::new(
            index,
            Some(text.to_string()),
            None,
            None,
            class.to_string(),
            clickable,
            true,
            bounds,
        )
    }

    fn dump(elements: Vec<UiElement>, truncated: bool) -> UiDump {
        UiDump {
            elements,
            screen: Some((1080, 2340)),
            source: "accessibility",
            package: Some("com.example".to_string()),
            snapshot: "s1-0".to_string(),
            truncated,
        }
    }

    #[test]
    fn a_step_target_reads_the_three_query_fields() {
        let target = StepTarget::from_command(&serde_json::json!({
            "op": "tap",
            "targetText": " 发送 ",
            "resourceId": "id/send",
            "class": "Button",
        }))
        .expect("a target");
        assert_eq!(target.text.as_deref(), Some("发送"));
        assert_eq!(target.resource_id.as_deref(), Some("id/send"));
        assert_eq!(target.class.as_deref(), Some("Button"));
        assert!(target.query().is_some());
    }

    #[test]
    fn a_step_without_a_target_has_none() {
        // Blank strings are not a target: `text: ""` means "any", which would
        // match the whole screen and tap the first thing on it.
        assert!(StepTarget::from_command(&serde_json::json!({
            "op": "tap",
            "targetText": "   ",
        }))
        .is_none());
        assert!(StepTarget::from_command(&serde_json::json!({ "op": "tap" })).is_none());
    }

    #[test]
    fn exactly_one_match_is_the_target() {
        let screen = dump(
            vec![
                element(1, "发送", "android.widget.Button", true, (900, 2100, 1050, 2200)),
                element(2, "取消", "android.widget.Button", true, (100, 2100, 250, 2200)),
            ],
            false,
        );
        let target = StepTarget::from_command(&serde_json::json!({ "targetText": "发送" }))
            .expect("a target");
        let found = locate_step_target(&screen, &target, false).expect("one match");
        assert_eq!(found.center, (975, 2150));
    }

    #[test]
    fn two_identical_matches_are_refused_not_guessed() {
        // Two rows that say the same thing: tapping the first is the silent
        // wrong-target failure, so the run stops and names both.
        let screen = dump(
            vec![
                element(1, "删除", "android.widget.Button", true, (0, 0, 100, 100)),
                element(2, "删除", "android.widget.Button", true, (0, 200, 100, 300)),
            ],
            false,
        );
        let target = StepTarget::from_command(&serde_json::json!({ "targetText": "删除" }))
            .expect("a target");
        let error = locate_step_target(&screen, &target, false).expect_err("ambiguous");
        assert_eq!(error.code, STOP_AMBIGUOUS_TARGET);
        assert!(error.detail.contains("2 elements"), "{}", error.detail);
    }

    #[test]
    fn an_exact_label_beats_a_longer_one_on_the_same_screen() {
        // "发送" and "发送图片" both contain the needle; the exact label is the
        // button, and the superset is decoration.
        let screen = dump(
            vec![
                element(1, "发送图片", "android.widget.Button", true, (0, 0, 100, 100)),
                element(2, "发送", "android.widget.Button", true, (800, 2100, 1000, 2200)),
            ],
            false,
        );
        let target = StepTarget::from_command(&serde_json::json!({ "targetText": "发送" }))
            .expect("a target");
        let found = locate_step_target(&screen, &target, false).expect("exact");
        assert_eq!(found.text.as_deref(), Some("发送"));
    }

    #[test]
    fn a_write_prefers_an_editable_field_over_a_label_with_the_same_text() {
        let mut field = element(2, "搜索", "android.widget.EditText", false, (0, 100, 1080, 220));
        field.state.editable = true;
        let label = element(1, "搜索", "android.widget.TextView", false, (0, 0, 200, 60));
        let screen = dump(vec![label, field], false);
        let target = StepTarget::from_command(&serde_json::json!({ "targetText": "搜索" }))
            .expect("a target");
        let found = locate_step_target(&screen, &target, true).expect("the field");
        assert_eq!(found.class, "android.widget.EditText");
    }

    #[test]
    fn a_target_that_is_not_on_the_screen_says_so() {
        let screen = dump(vec![element(1, "首页", "TextView", false, (0, 0, 100, 40))], false);
        let target = StepTarget::from_command(&serde_json::json!({ "targetText": "设置" }))
            .expect("a target");
        let error = locate_step_target(&screen, &target, false).expect_err("missing");
        assert_eq!(error.code, STOP_TARGET_NOT_FOUND);
        assert!(error.detail.contains("设置"), "{}", error.detail);
    }

    #[test]
    fn a_truncated_tree_that_named_one_match_is_still_the_match() {
        // Truncation only makes *ambiguity* unknowable further down; a single
        // hit is still a single hit, and refusing it would make the fallback
        // path unable to run at all.
        let screen = dump(
            vec![element(
                1,
                "发送",
                "android.widget.Button",
                true,
                (0, 0, 100, 100),
            )],
            true,
        );
        let target = StepTarget::from_command(&serde_json::json!({ "targetText": "发送" }))
            .expect("a target");
        assert!(locate_step_target(&screen, &target, false).is_ok());
    }

    #[test]
    fn a_run_is_shorter_when_every_read_is_a_uiautomator_dump() {
        // The fallback path costs a subprocess per read, so its ceiling is
        // lower — and lower by enough that the difference is worth reporting.
        assert!(RUN_MAX_STEPS_FALLBACK < RUN_MAX_STEPS);
        assert_eq!(RUN_MAX_STEPS, super::super::android::MAX_SEQUENCE_STEPS);
    }

    #[test]
    fn elements_that_share_a_line_become_one_row() {
        // A chat row is a name and a message side by side, drawn by two nodes.
        // The centres differ; the extents say they are one line.
        let mut name = element(1, "Alice", "android.widget.TextView", false, (20, 300, 200, 360));
        let mut body =
            element(2, "see you at six", "android.widget.TextView", false, (220, 300, 900, 360));
        name.state.depth = 4;
        body.state.depth = 5;
        let rows = extract_rows(&[name, body]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].texts, vec!["Alice", "see you at six"]);
    }

    #[test]
    fn rows_separated_vertically_stay_separate_and_are_numbered_top_down() {
        let first = element(1, "one", "TextView", false, (0, 100, 400, 160));
        let second = element(2, "two", "TextView", false, (0, 500, 400, 560));
        let rows = extract_rows(&[second, first]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].texts, vec!["one"]);
        assert_eq!(rows[1].index, 2);
    }

    #[test]
    fn a_password_value_is_never_part_of_a_row() {
        let mut secret = element(1, "hunter2", "android.widget.EditText", false, (0, 0, 400, 60));
        secret.state.editable = true;
        secret.state.password = true;
        assert!(extract_rows(&[secret]).is_empty());
    }

    #[test]
    fn a_field_takes_the_label_to_its_left() {
        let mut field = element(2, "", "android.widget.EditText", false, (300, 100, 1000, 160));
        field.state.editable = true;
        let label = element(1, "手机号", "android.widget.TextView", false, (20, 100, 280, 160));
        let fields = extract_fields(&[label, field]);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].label, "手机号");
        assert_eq!(fields[0].kind, "phone");
    }

    #[test]
    fn a_field_kind_is_inferred_from_its_label() {
        assert_eq!(infer_field_kind("邮箱地址"), "email");
        assert_eq!(infer_field_kind("Password"), "password");
        assert_eq!(infer_field_kind("验证码"), "code");
        assert_eq!(infer_field_kind("Search"), "search");
        assert_eq!(infer_field_kind("数量"), "number");
        assert_eq!(infer_field_kind("备注"), "text");
    }

    #[test]
    fn a_password_field_reports_no_current_value() {
        let mut field = element(1, "secret", "android.widget.EditText", false, (0, 0, 400, 60));
        field.state.editable = true;
        field.state.password = true;
        let fields = extract_fields(&[field]);
        assert_eq!(fields[0].kind, "password");
        assert!(fields[0].current.is_none());
    }

    #[test]
    fn the_extract_budget_cuts_at_a_line_and_says_so() {
        let lines = vec!["a".repeat(30), "b".repeat(30), "c".repeat(30)];
        // Each line costs its length plus the newline separator.
        let (kept, truncated) = cap_lines(lines, 62);
        assert_eq!(kept.len(), 2);
        assert!(truncated);
        // Exactly the budget is not a truncation.
        let (kept, truncated) = cap_lines(vec!["x".repeat(10)], 11);
        assert_eq!(kept.len(), 1);
        assert!(!truncated);
    }

    #[test]
    fn a_list_screen_is_recognised_by_its_container() {
        let list = scrollable(2, (0, 100, 1080, 2300));
        assert!(looks_like_a_list(&[list]));
        let plain = element(1, "设置", "TextView", true, (0, 0, 100, 40));
        assert!(!looks_like_a_list(&[plain]));
    }

    #[test]
    fn a_chooser_stops_a_run_before_it_acts_on_the_sheet() {
        // `launch` can open a chooser — two apps claim the link — and the steps
        // after it would target the sheet's rows, not the app's.
        let chooser = launch_interrupted("com.android.intentresolver", 3).expect("a stop");
        assert_eq!(chooser.code, STOP_OPENED_ELSEWHERE);
        assert!(chooser.detail.contains("step 3"), "{}", chooser.detail);
        // A permission prompt is the phone asking a person, not the agent
        // landing somewhere wrong.
        let prompt =
            launch_interrupted("com.google.android.permissioncontroller", 1).expect("a stop");
        assert_eq!(prompt.code, STOP_SCREEN_NEEDS_REVIEW);
        assert!(prompt.detail.contains("hand_to_user"), "{}", prompt.detail);
        // An ordinary app is not an interruption at all.
        assert!(launch_interrupted("com.example.chat", 1).is_none());
    }

    #[test]
    fn a_step_the_run_never_reached_says_so() {
        let step =
            RunStep::new(4, "tap").not_run("not run: the run stopped at step 3 (app_switched)");
        assert!(!step.executed);
        assert!(!step.ok);
        let line = step.describe();
        assert!(line.starts_with("4. tap — not run"), "{line}");
        assert!(line.contains("stopped at step 3"), "{line}");
    }

    #[test]
    fn a_step_that_ran_but_halted_the_run_keeps_its_effect() {
        // The step worked; what stopped the run was the phone afterwards. The
        // effect is still evidence, so it survives the failure.
        let mut step = RunStep::new(2, "tap").executed(Some("changed"));
        step.fail_in_place(&StepError::new(STOP_APP_SWITCHED, "it moved to another app"));
        assert!(step.executed);
        assert!(!step.ok);
        assert_eq!(step.effect.as_deref(), Some("changed"));
        assert_eq!(step.code.as_deref(), Some(STOP_APP_SWITCHED));
    }

    #[test]
    fn every_op_a_run_claims_to_accept_is_listed_once() {
        assert_eq!(RUN_OPS.len(), 11);
        assert!(RUN_OPS.contains(&"launch"));
        assert!(RUN_OPS.contains(&"observe"));
        // The schema's enum and the parser's list are the same contract; a
        // value in one and not the other is a step the model can write and the
        // run cannot execute.
        let schema = AdbRunTool::new().parameters_schema();
        let ops = schema
            .pointer("/properties/commands/items/properties/op/enum")
            .and_then(Value::as_array)
            .expect("an op enum");
        let declared: Vec<&str> = ops.iter().filter_map(Value::as_str).collect();
        assert_eq!(declared, RUN_OPS.to_vec());
    }

    #[test]
    fn a_scroll_aims_inside_the_container_when_the_tree_names_one() {
        let list = scrollable(3, (0, 300, 1080, 2000));
        let (from, to) = scroll_points((1080, 2340), Direction::Down, Some(&list));
        // Down means the finger moves up: content moves towards the top.
        assert!(from.1 > to.1, "{from:?} {to:?}");
        assert!(from.1 <= 2000 && to.1 >= 300, "the drag stays inside the list");
        assert_eq!(from.0, list.center.0);
    }

    #[test]
    fn a_scroll_without_a_container_uses_the_middle_of_the_screen() {
        let (from, to) = scroll_points((1080, 2340), Direction::Up, None);
        assert_eq!(from, (540, 585));
        assert_eq!(to, (540, 1755));
    }

    #[test]
    fn a_wait_without_a_condition_does_not_wait_forever_open() {
        // The poll budget clamps to what the tool budget can afford: a caller
        // asking for an hour gets 30 s, and one asking for 0 ms still gets one
        // poll rather than a busy loop.
        assert_eq!(WAIT_MAX_MS, 30_000);
        assert!(WAIT_DEFAULT_MS <= WAIT_MAX_MS);
        assert!(WAIT_POLL_MS <= WAIT_DEFAULT_MS);
    }
}

/// One line per fact the model needs before it writes to a display.
///
/// The three states are separate sentences because they call for three
/// different moves: unlock the phone, start the helper, or re-read the screen.
fn describe_readiness(state: &super::display::Readiness) -> String {
    let mut lines = Vec::new();
    lines.push(if state.locked {
        "The phone is locked: a tap on the user's screen lands on the lock screen, and an app \
         started behind it will not have changed."
            .to_string()
    } else {
        "The phone is unlocked.".to_string()
    });
    if let Some(top) = &state.main {
        lines.push(format!(
            "User's screen: {} (task {}).",
            top.package.as_deref().unwrap_or("an unnamed app"),
            top.task
        ));
    }
    if state.displays.is_empty() {
        lines.push("No displays are open.".to_string());
    } else {
        let shown: Vec<String> = state
            .displays
            .iter()
            .map(|top| match top.package.as_deref() {
                Some(package) => format!("display {} holds {package}", top.display),
                None => format!("display {} is empty", top.display),
            })
            .collect();
        lines.push(format!("{}.", shown.join("; ")));
    }
    lines.join(" ")
}

#[cfg(test)]
mod readiness_tests {
    use super::*;
    use crate::computer::platform::mobile::display::{Readiness, ScreenTop};

    fn state(locked: bool, displays: Vec<ScreenTop>) -> Readiness {
        Readiness {
            helper: true,
            locked,
            main: Some(ScreenTop {
                display: 0,
                task: 4,
                package: Some("com.example.chat".to_string()),
            }),
            displays,
        }
    }

    #[test]
    fn a_locked_phone_is_said_in_the_status_line() {
        let text = describe_readiness(&state(true, Vec::new()));
        assert!(text.contains("locked"), "{text}");
        assert!(text.contains("lock screen"), "{text}");
    }

    #[test]
    fn each_display_says_what_it_holds() {
        let text = describe_readiness(&state(
            false,
            vec![
                ScreenTop {
                    display: 3,
                    task: 9,
                    package: Some("com.example.mail".to_string()),
                },
                ScreenTop {
                    display: 4,
                    task: -1,
                    package: None,
                },
            ],
        ));
        assert!(text.contains("display 3 holds com.example.mail"), "{text}");
        assert!(text.contains("display 4 is empty"), "{text}");
        assert!(!text.contains("No displays"), "{text}");
    }

    #[test]
    fn no_displays_says_so_rather_than_nothing() {
        let text = describe_readiness(&state(false, Vec::new()));
        assert!(text.contains("No displays are open"), "{text}");
    }
}
