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
    resolve_device, resolved_serial, settle_after_action, sh_quote, tree_fingerprint, Target,
    TreeQuality, UiDump, UiElement, DEVICE_PARAM, DISPLAY_PARAM, EFFECT_UNCHANGED,
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
    let (width, height) = size;
    // A container gets the gesture *inside itself*, split across its height, so
    // the drag stays within the thing that is supposed to move.
    let (x, from_y, to_y) = match container {
        Some(node) => {
            let (x1, y1, x2, y2) = node.bounds;
            let cx = node.center.0;
            let span = y2 - y1;
            let (from, to) = match direction {
                Direction::Down => (y1 + span * 3 / 4, y1 + span / 4),
                Direction::Up => (y1 + span / 4, y1 + span * 3 / 4),
            };
            // A node too short to hold a real drag (a two-row list, a sliver)
            // cannot express a screenful; the whole screen still can.
            if span >= 200 && x2 > x1 {
                (cx, from, to)
            } else {
                let (from, to) = match direction {
                    Direction::Down => (height * 3 / 4, height / 4),
                    Direction::Up => (height / 4, height * 3 / 4),
                };
                (width / 2, from, to)
            }
        }
        None => {
            let (from, to) = match direction {
                Direction::Down => (height * 3 / 4, height / 4),
                Direction::Up => (height / 4, height * 3 / 4),
            };
            (width / 2, from, to)
        }
    };
    if let Target::Display(id) = target {
        // A swipe aimed at the agent's display goes through the helper: ADB's
        // own `input` only reaches the default display.
        let bridge = display_bridge()?;
        let display = super::display::VirtualDisplay::attach(bridge.clone(), id);
        return display
            .swipe((x, from_y), (x, to_y), 300)
            .await
            .map_err(|e| e.to_string());
    }
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

        let text_out = if total == 0 {
            format!(
                "No element on this screen matches. The screen has {} element(s); read it with \
                 android_ui_tree if the label may be worded differently.",
                dump.elements.len()
            )
        } else {
            let mut lines = vec![format!(
                "{total} match(es){}:",
                if total > found.len() {
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
            lines.join("\n")
        };

        Ok(ToolExecutionResult::success(text_out).with_data(serde_json::json!({
            "total": total,
            "returned": found.len(),
            "matches": found,
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
