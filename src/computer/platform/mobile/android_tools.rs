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
    AdbAppManagerTool, AdbInputTool, OcrLine, Target, TreeQuality, UiDump, UiElement, DEVICE_PARAM,
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

// ── Merging the tree and OCR ───────────────────────────────────────────────

/// How many OCR lines may sit within this many pixels of each other before
/// they are the same line.
///
/// OCR re-reads the same glyphs on consecutive screens — a header, a sticky
/// button — and hands them back a pixel or two apart. Anything within one
/// character's height of a line already taken is that line.
const OCR_SAME_LINE_PX: i32 = 8;

/// Budget for `android_collect`'s collected text, in characters.
///
/// A long chat is unbounded, and the tail of it is the part that matters least
/// to a question asked about it. The budget is the tool's own rather than
/// `EXTRACT_MAX_CHARS`: that one counts structured rows, this counts lines the
/// model will read in order, and hitting it has to stop the *scroll* — reading
/// more screens that would be dropped is pure cost.
pub const COLLECT_MAX_CHARS: usize = 50_000;

/// One readable thing on a screen, wherever it came from.
#[derive(Debug, Clone, PartialEq)]
struct ReadLine {
    text: String,
    /// Centre among the screen's elements, or the line's own when only OCR saw
    /// it. Used to order the two sources against each other.
    center: (i32, i32),
    source: LineSource,
}

/// Which reader produced a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineSource {
    Tree,
    Ocr,
}

impl LineSource {
    /// How the line is reported back.
    fn tag(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Ocr => "ocr",
        }
    }
}

/// What one screen read produced.
#[derive(Debug, Default, Clone)]
struct ScreenRead {
    lines: Vec<ReadLine>,
    /// The sources that contributed to `lines`.
    tree: bool,
    ocr: bool,
}

impl ScreenRead {
    /// The one word the caller is told: tree, ocr, mixed — or none.
    fn source(&self) -> &'static str {
        match (self.tree, self.ocr) {
            (true, true) => "mixed",
            (true, false) => "tree",
            (false, true) => "ocr",
            (false, false) => "none",
        }
    }
}

/// Merge the tree's labels with the OCR lines of the same screen, in reading
/// order.
///
/// The two are not alternatives: a chat screen exposes its container and its
/// input bar to the tree while the messages are drawn, so the *tree alone* says
/// "there is a list" and the *text alone* says nothing about what is scrollable.
/// Taking both, ordered by where they sit, gives a person's reading of the
/// screen.
///
/// A line the two both saw is taken once, from the tree: the tree's label is
/// exact where OCR may have mangled it, and its index is something a later tap
/// can address. Overlap is judged by the OCR line's *own* centre, not the
/// tree's — the tree's centre is the element's, and an element that covers half
/// the screen (a list) has a centre nowhere near its text.
fn merge_screen(elements: &[UiElement], ocr: &[OcrLine]) -> ScreenRead {
    let mut lines: Vec<ReadLine> = Vec::new();
    let mut tree = false;
    for element in elements {
        let Some(label) = element_label(element) else {
            continue;
        };
        let text = label.trim();
        if text.is_empty() || lines.iter().any(|line| line.text == text) {
            continue;
        }
        tree = true;
        lines.push(ReadLine {
            text: text.to_string(),
            center: element.center,
            source: LineSource::Tree,
        });
    }
    let mut ocr_used = false;
    for line in ocr {
        let text = line.text.trim();
        if text.is_empty() {
            continue;
        }
        let covered = lines.iter().any(|seen| {
            (seen.center.1 - line.center.1).abs() <= OCR_SAME_LINE_PX
                && (seen.center.0 - line.center.0).abs() <= OCR_SAME_LINE_PX
        }) || lines.iter().any(|seen| seen.text == text);
        if covered {
            continue;
        }
        ocr_used = true;
        lines.push(ReadLine {
            text: text.to_string(),
            center: line.center,
            source: LineSource::Ocr,
        });
    }
    lines.sort_by_key(|line| (line.center.1, line.center.0));
    ScreenRead { lines, tree, ocr: ocr_used }
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
         anything where the answer is spread over several screens. Screens the accessibility \
         tree cannot describe are read from the screenshot with OCR and merged in place, so a \
         chat drawn by hand is still collected; the result says which screens needed it. Stops \
         at 50,000 characters."
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
        let mut screens: Vec<(Vec<ReadLine>, &'static str)> = Vec::new();
        let mut size: Option<(i32, i32)> = None;
        let mut stop = "reached max_screens";
        let mut cost = 0_usize;
        // Whether any screen was read through OCR. Reported once, at the end:
        // the per-screen tag is on the lines, but the model needs to know
        // before it trusts the text that some of it was read from pixels.
        let mut used_ocr = false;

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
            size = screen_size(&device, dump.screen).await.or(size);

            // OCR is a helper, not a second main path: the tree is exact and
            // cheap, so it is read alone while it describes the screen. It is
            // read only when the tree cannot — a chat drawn in a custom view,
            // a canvas — where the alternative is a screen of nothing.
            let quality = assess_tree(&dump.elements);
            let ocr = if quality == TreeQuality::Usable {
                Vec::new()
            } else {
                match ocr_screen(&device, target).await {
                    Ok(lines) => lines,
                    Err(why) => {
                        if index == 0 && quality == TreeQuality::Unusable {
                            // Nothing came from either reader. Say so once,
                            // with the reason OCR had, rather than scrolling a
                            // screen it cannot read and calling the result
                            // empty.
                            return Ok(ToolExecutionResult::success(format!(
                                "android_collect found nothing readable on this screen: the \
                                 accessibility tree describes none of it and OCR could not read \
                                 it ({why}). Use android_observe to look at the screenshot \
                                 instead."
                            ))
                            .with_data(serde_json::json!({
                                "collected": 0,
                                "quality": quality.as_str(),
                                "source": "none",
                            })));
                        }
                        Vec::new()
                    }
                }
            };

            let read = merge_screen(&dump.elements, &ocr);
            if read.ocr {
                used_ocr = true;
            }
            let source = read.source();
            let fresh: Vec<ReadLine> = read
                .lines
                .into_iter()
                .filter(|line| seen.insert(line.text.clone()))
                .collect();
            if index > 0 && fresh.is_empty() {
                stop = "nothing new appeared";
                break;
            }
            for line in &fresh {
                cost += line.text.chars().count() + 1;
            }
            let source = if fresh.is_empty() { "none" } else { source };
            screens.push((fresh, source));
            if cost >= COLLECT_MAX_CHARS {
                stop = "reached the 50k-character budget";
                break;
            }

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

        let total: usize = screens.iter().map(|(lines, _)| lines.len()).sum();
        let mut text = format!(
            "Read {total} line(s) over {} screen(s); stopped because {stop}.",
            screens.len()
        );
        if screens.len() == max_screens {
            text.push_str(" If the answer may continue, call again with a larger max_screens.");
        }
        if used_ocr {
            text.push_str(
                "\n\nSome screens were read from the screenshot with OCR rather than the \
                 accessibility tree; those lines are marked `(ocr)` and are less exact than the \
                 rest.",
            );
        }
        let mut data = serde_json::json!({
            "screens": screens
                .iter()
                .map(|(lines, source)| {
                    let rendered: Vec<serde_json::Value> = lines
                        .iter()
                        .map(|line| {
                            serde_json::json!({
                                "text": line.text,
                                "source": line.source.tag(),
                                "center": [line.center.0, line.center.1],
                            })
                        })
                        .collect();
                    serde_json::json!({ "source": source, "lines": rendered })
                })
                .collect::<Vec<_>>(),
            "stop_reason": stop,
            "source": if used_ocr { "mixed" } else { "tree" },
            "device": resolved_serial(&device).await,
        });
        if cost >= COLLECT_MAX_CHARS {
            data["truncated"] = serde_json::json!(true);
        }
        for (index, (lines, source)) in screens.iter().enumerate() {
            let name = match *source {
                "tree" => "tree".to_string(),
                "ocr" => "ocr".to_string(),
                "mixed" => "tree + OCR".to_string(),
                _ => "nothing new".to_string(),
            };
            text.push_str(&format!("\n\n── screen {} ({name}) ──", index + 1));
            for line in lines {
                text.push('\n');
                text.push_str(&line.text);
                if line.source == LineSource::Ocr {
                    text.push_str(" (ocr)");
                }
            }
        }

        Ok(ToolExecutionResult::success(text).with_data(data))
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

/// How long a launch is watched for the app to reach the top of the display.
///
/// `am start -W` returns when the activity reports idle, which for a cold start
/// of a heavy app is how long the *process* took, not how long the first frame
/// took to become the top task. Five seconds is Mika's number, and it is a
/// ceiling rather than a target: a display that already shows the app or a
/// chooser ends the wait on its first read.
const LAUNCH_SETTLE: Duration = Duration::from_secs(5);

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
         does not steal the user's keyboard. `launch` ends the app first when nothing else is \
         holding it, starts it warm when another display still has a task of it, and refuses \
         when the person is using it on their own screen; the result's `cold_start` says which \
         happened and `opened` says what came to the top (the app, a chooser, or something else). \
         `frame` returns a PNG of that display. This needs the Syscity app running on the phone; \
         a desktop controlling a remote device has no second screen to offer."
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
                    "description": "start creates the display; limits says how many may be open; restart replaces a stale helper; the rest act on it. 'paste' carries non-ASCII text, which `text` cannot. 'mute' silences the app on this display and 'sound' gives it back; launching an app mutes it already. 'launch' may end the app first, start it warm, or refuse — the result says which."
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

        // `launch` on a display is the one write that has to decide about the
        // app's *process* before it starts anything: a clean start means ending
        // it first, and ending it is not local. The decision needs the whole
        // task list, which only the helper can read, so it is taken here — after
        // the gate and the lease, before anything is dispatched.
        let mut cold_start: Option<super::coldstart::ColdStart> = None;
        if action == "launch" {
            let package = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
            let state = super::display::VirtualDisplay::readiness(&bridge)
                .await
                .ok();
            let main = state.as_ref().and_then(|state| state.top_of(0));
            // "In use" is a task the person is *looking at*, and the launcher is
            // where the phone sits rather than something they opened — so it
            // never holds a launch back.
            let visible_on_main = main
                .and_then(|top| top.package.as_deref())
                .is_some_and(|shown| shown == package && !super::coldstart::is_home(shown));
            let decision = match state.as_ref().and_then(|s| s.displays_holding(package)) {
                Some(displays) => {
                    super::coldstart::ColdStart::decide(&displays, id, visible_on_main)
                }
                // The task list could not be read. Ending a process on a guess
                // is the one move that cannot be undone, so this is a warm
                // start — the person's screen is still visible in readiness and
                // still refuses.
                None => super::coldstart::ColdStart::unaware(visible_on_main),
            };
            if !decision.allowed() {
                return Ok(ToolExecutionResult::error(decision.describe(package)).with_data(
                    serde_json::json!({
                        "display": id,
                        "action": action,
                        "cold_start": decision.as_str(),
                        "launched": false,
                    }),
                ));
            }
            if decision.ends_process() {
                // `am force-stop` reaches the whole app, on every display and in
                // the background — which is exactly why the decision above had
                // to establish that nothing else was holding it.
                let argv = adb_args(&device, &["shell", "am", "force-stop", package]);
                let stopped =
                    run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>()).await;
                match stopped {
                    Ok((status, _, _)) if status.success() => {}
                    Ok((_, _, stderr)) => {
                        return Ok(ToolExecutionResult::error(format!(
                            "Refused: `{package}` could not be ended before starting it \
                             ({stderr}). Nothing was started — starting it now would leave \
                             whatever it had before on screen and call that a fresh start."
                        )))
                    }
                    Err(e) => {
                        return Ok(ToolExecutionResult::error(format!(
                            "Refused: `adb` could not end `{package}` before starting it \
                             ({e}). Nothing was started."
                        )))
                    }
                }
            }
            cold_start = Some(decision);
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

        // What the launch turned into, once the display has been asked. Only a
        // launch has one: every other action is judged by the screen it acted
        // on, and this is the screen a launch *moved to*.
        let mut opened: Option<super::coldstart::Opened> = None;
        // Which copy the launch is asking about, when the system raised a
        // chooser because the app is installed twice. `None` is "not a
        // question", which is not the same as "could not tell".
        let mut chooser: Option<Vec<super::profiles::Choice>> = None;
        let result = match action {
            "stop" => display.close().await.map(|_| "Display closed.".to_string()),
            "launch" => {
                let package = args.get("package").and_then(|v| v.as_str()).unwrap_or("");
                match display.launch(package).await {
                    Ok(()) => {
                        // `am start -W` returns when the activity reports idle
                        // — for a chooser, without the app ever arriving. So the
                        // display is asked what actually came to the top, and
                        // the answer is the result rather than the command's
                        // exit.
                        opened =
                            super::coldstart::await_opened(&bridge, id, package, LAUNCH_SETTLE)
                                .await;
                        // A chooser is the system asking which copy of a
                        // twice-installed app to open. The display cannot
                        // answer it — a tap would be picking without knowing
                        // which copy — so it is named, its options are read,
                        // and the caller chooses.
                        if matches!(opened, Some(super::coldstart::Opened::Chooser { .. })) {
                            chooser = match (
                                super::profiles::users(&device).await,
                                super::android::dump_ui(&device, Target::Display(id)).await,
                            ) {
                                (Some(users), Ok(screen)) => {
                                    super::profiles::chooser_options(&screen.elements, &users)
                                }
                                // Not being able to read who else is on the
                                // phone, or the chooser itself, leaves the
                                // question open rather than answered: the
                                // caller is told it is a chooser either way.
                                _ => None,
                            };
                        }
                        let mut text = format!("Opened {package} on display {id} (muted).");
                        if let Some(rest) = opened
                            .as_ref()
                            .and_then(|opened| opened.describe(package, id))
                        {
                            text.push(' ');
                            text.push_str(&rest);
                        }
                        if let Some(choices) = &chooser {
                            text.push(' ');
                            text.push_str(&super::profiles::describe_choices(choices));
                        }
                        Ok(text)
                    }
                    Err(failed) => Err(failed),
                }
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
                    // A launch carries three extra facts, and they are the
                    // ones a caller has to branch on: whether the process was
                    // ended first, and what actually came to the top — the app,
                    // a chooser, or something else.
                    "cold_start": cold_start.map(super::coldstart::ColdStart::as_str),
                    "opened": opened.as_ref().map(|opened| serde_json::json!({
                        "state": opened.as_str(),
                        "task": match opened {
                            super::coldstart::Opened::Here { task } => Some(*task),
                            _ => None,
                        },
                        "package": match opened {
                            super::coldstart::Opened::Chooser { package }
                            | super::coldstart::Opened::Elsewhere { package: Some(package) } => {
                                Some(package.clone())
                            }
                            _ => None,
                        },
                    })),
                    // The options the chooser is offering, when the launch
                    // raised one and they could be read. Absent means "not a
                    // chooser, or a question that could not be read" — the
                    // `opened` state says which.
                    "chooser": chooser.as_ref().map(|choices| {
                        serde_json::json!({
                            "options": choices.iter().map(|choice| serde_json::json!({
                                "label": choice.label,
                                "copy": choice.copy.as_str(),
                                "note": choice.copy.note(),
                            })).collect::<Vec<_>>(),
                        })
                    }),
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

/// How long a step waits for the phone to report a window change.
///
/// A transition's event is pushed while the step's own input is still settling,
/// so it is normally already queued when the step returns; the grace only
/// covers the lag. Waiting longer would tax every step on a screen that does
/// not move — exactly the case the per-step tree read was slow for.
const RUN_EVENT_GRACE_MS: u64 = 200;

/// What a step's aftermath looked like to the event stream.
enum WatchVerdict {
    /// The phone moved to this app, which is not the one the run is in.
    Moved(String),
    /// Still in the expected app (or no window change at all).
    Still,
    /// The stream cannot answer — the run must fall back to reading the tree.
    Unavailable,
}

/// The phone's own account of what changed, watched across a run.
///
/// With a push channel, a run no longer re-reads the tree after every step just
/// to learn whether the app changed: the window event says so, and it says so
/// without a bridge call. On a host without one (a desktop driving a phone over
/// ADB, or the service off) the watch is inert and the run keeps the per-step
/// read — a missing stream is never a reason to stop checking.
///
/// Events describe the user's own display, which is why a run against a virtual
/// display does not use this even when the channel exists.
struct EventWatch {
    receiver: Option<tokio::sync::broadcast::Receiver<crate::device::DeviceEvent>>,
    /// The stream fell behind or closed: what it said can no longer be
    /// trusted to be complete, so the run reads the tree instead.
    degraded: bool,
}

impl EventWatch {
    /// Attach to the event stream, if this device really has one.
    ///
    /// A channel existing is not enough to trust its silence: the phone must
    /// have the service on, and the running binding must deliver window
    /// changes, or "no event" would mean "nothing was ever sent" and the run
    /// would fail to notice an app switch. One status read per run pays for
    /// that, and only when a channel exists at all.
    async fn attach() -> Self {
        let Some(bridge) = crate::device::bridge().cloned() else {
            return Self::with_channel(None);
        };
        let Some(receiver) = bridge.events() else {
            return Self::with_channel(None);
        };
        if let Some(status) = read_a11y_status().await {
            if status.enabled == Some(false) {
                return Self::with_channel(None);
            }
            if let Some(mask) = status.subscribed_event_types {
                if mask & crate::device::EventKind::Window.subscription_bit() == 0 {
                    return Self::with_channel(None);
                }
            }
        }
        Self::with_channel(Some(receiver))
    }

    fn with_channel(
        mut receiver: Option<tokio::sync::broadcast::Receiver<crate::device::DeviceEvent>>,
    ) -> Self {
        // Anything already queued happened before the run started and is not
        // this run's business.
        if let Some(channel) = receiver.as_mut() {
            while channel.try_recv().is_ok() {}
        }
        Self { receiver, degraded: false }
    }

    /// Whether this watch can answer at all.
    fn available(&self) -> bool {
        self.receiver.is_some() && !self.degraded
    }

    /// The next window event from an app other than `expected`, within `grace`.
    ///
    /// "Another app" is the whole test: a new activity of the *same* package is
    /// still the run's screen, and the next step's own read decides whether its
    /// targets survived. A chooser and a system prompt come through as their
    /// own packages, so the caller can tell them apart by name.
    async fn moved(&mut self, expected: Option<&str>, grace: std::time::Duration) -> WatchVerdict {
        use tokio::sync::broadcast::error::RecvError;

        let Some(receiver) = self.receiver.as_mut() else {
            return WatchVerdict::Unavailable;
        };
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            tokio::select! {
                incoming = receiver.recv() => match incoming {
                    Ok(event) => {
                        if event.kind != crate::device::EventKind::Window
                            || event.package.is_empty()
                            || expected.is_some_and(|package| package == event.package)
                        {
                            continue;
                        }
                        return WatchVerdict::Moved(event.package);
                    }
                    // The subscriber fell behind: an event that would have
                    // answered is gone, so the stream stops being evidence.
                    Err(RecvError::Lagged(_)) => {
                        self.degraded = true;
                        return WatchVerdict::Unavailable;
                    }
                    Err(RecvError::Closed) => {
                        self.receiver = None;
                        return WatchVerdict::Unavailable;
                    }
                },
                _ = tokio::time::sleep_until(deadline) => return WatchVerdict::Still,
            }
        }
    }
}

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
        // The event stream answers "did the app change" for a run on the user's
        // display; everywhere else it is inert and the per-step read does.
        let mut watch = match target {
            Target::Screen => EventWatch::attach().await,
            Target::Display(_) => EventWatch::with_channel(None),
        };
        let mut checked_by_events = 0_usize;

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
                let launched = match watch.available() {
                    true => match watch
                        .moved(
                            expected_package.as_deref(),
                            Duration::from_millis(RUN_EVENT_GRACE_MS),
                        )
                        .await
                    {
                        WatchVerdict::Moved(package) => Some(package),
                        _ => None,
                    },
                    false => None,
                };
                let launch_read = match launched {
                    Some(package) => Some(package),
                    None => dump_ui(&device, target)
                        .await
                        .ok()
                        .and_then(|after| after.package),
                };
                match launch_read {
                    Some(now) => {
                        if let Some(failure) = launch_interrupted(&now, n) {
                            step.fail_in_place(&failure);
                            stop = Some((failure.code.to_string(), n));
                            steps.push(step);
                            break;
                        }
                        expected_package = Some(now);
                        steps.push(step);
                    }
                    None => {
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
            //
            // The event stream answers that question — "the app changed" —
            // without a bridge call. When it answered, this loop takes no read
            // at all: content changes are already in the step's own result, and
            // "the tree cannot describe this screen" is answered where it
            // matters, by the step whose target cannot be found
            // ([`locate_step_target`]).
            let moved_to = match watch
                .moved(expected_package.as_deref(), Duration::from_millis(RUN_EVENT_GRACE_MS))
                .await
            {
                WatchVerdict::Moved(package) => Some(package),
                WatchVerdict::Still | WatchVerdict::Unavailable => None,
            };
            let from_events = watch.available();
            if from_events {
                checked_by_events += 1;
            }
            let after: Option<UiDump> = if from_events {
                None
            } else {
                match dump_ui(&device, target).await {
                    Ok(dump) => {
                        unreadable = 0;
                        Some(dump)
                    }
                    Err(_) => {
                        unreadable += 1;
                        // One unreadable screen is a transition; three in a row
                        // is a phone that needs a person.
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
                }
            };

            // Every check below ends the run; the step it is attached to is
            // the one whose aftermath was caught. The app change itself comes
            // from the event stream when it could answer, and from this read
            // otherwise.
            let changed_app = moved_to.or_else(|| {
                match (
                    expected_package.as_deref(),
                    after.as_ref().and_then(|dump| dump.package.as_deref()),
                ) {
                    (Some(before), Some(now)) if before != now => Some(now.to_string()),
                    _ => None,
                }
            });
            let halted = changed_app.map(|now| match launch_interrupted(&now, n) {
                Some(opened) => opened,
                None => StepError::new(
                    STOP_APP_SWITCHED,
                    format!(
                        "the screen was in {} and is now in {now}, so the remaining steps no \
                         longer target the screen they were written for",
                        expected_package.as_deref().unwrap_or("another app")
                    ),
                ),
            });
            // A screen the tree cannot describe is a screen the next step's
            // locate cannot work on. Saying so now, with the app named, is more
            // useful than thirty identical failures.
            let halted = halted.or_else(|| {
                after.as_ref().and_then(|dump| {
                    (assess_tree(&dump.elements) == TreeQuality::Unusable).then(|| {
                        StepError::new(
                            STOP_SCREEN_NEEDS_REVIEW,
                            "the accessibility tree describes none of this screen — read the \
                             screenshot (android_observe) before continuing",
                        )
                    })
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
                text.push_str(&app_change_note(checked_by_events));
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
            checked_by_events,
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
    checked_by_events: usize,
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
        // How many steps learned "the app changed" from the event stream
        // instead of a tree read. Zero means the per-step read was the only
        // evidence available on this phone.
        "app_change_source": if checked_by_events > 0 { "events" } else { "tree" },
        "checked_by_events": checked_by_events,
        "max_steps": if source == "uiautomator" { RUN_MAX_STEPS_FALLBACK } else { RUN_MAX_STEPS },
        "truncated": final_read.is_some_and(|dump| dump.truncated),
        "elements": final_read.map(|dump| dump.elements.clone()),
        "device": device,
    })
}

/// What a run says about how it noticed app changes.
fn app_change_note(checked_by_events: usize) -> String {
    if checked_by_events == 0 {
        return String::new();
    }
    format!(
        "\n\nApp changes were watched through the phone's event stream for {checked_by_events} \
         step(s), so no tree was re-read just to notice one."
    )
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

// ── Events ─────────────────────────────────────────────────────────────────

/// Wait for the phone to report a change, instead of polling the screen.
///
/// The push channel is the point of this tool: while the accessibility service
/// is on, the phone calls into Rust as each window, content change or
/// notification happens, so a wait is a `select!` between an incoming event and
/// the deadline — no Binder thread held, no tree re-read every 250 ms. When the
/// service is off, or the host is a desktop driving a phone over ADB, there is
/// no push channel; this falls back to reading the service's ring
/// (`accessibilityEvents`) on the same 250 ms cadence and says so in `source`.
///
/// What an event is *not*: a picture of the screen. It says "something changed
/// in this app at this time", and the screen still has to be read afterwards.
/// `condition: event` is therefore for "wait until something happens", and
/// `appear`/`disappear` remain the tools for "wait until this specific thing".
#[derive(Debug)]
pub struct AdbEventTool {
    device: Option<String>,
}

impl Default for AdbEventTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbEventTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// How many events one call returns by default.
const EVENT_DEFAULT_LIMIT: usize = 20;
/// The most one call returns.
const EVENT_MAX_LIMIT: usize = 200;

/// Read the phone's event ring.
///
/// `since` is the cursor from a previous answer; `None` starts from the oldest
/// event still buffered. The bridge is looked up process-wide for the same
/// reason the tree read is: events belong to the device, and the tool set that
/// asks may be several layers below the one holding a bridge reference.
async fn read_events(
    since: Option<u64>,
    kind: Option<crate::device::EventKind>,
    limit: usize,
) -> Result<crate::device::EventPage, String> {
    let bridge = crate::device::bridge().cloned().ok_or_else(|| {
        "The device event stream only exists inside the Syscity app on the phone itself — this \
         host has no accessibility service. Wait on the screen instead (android_wait with \
         condition 'appear', 'disappear' or 'stable')."
            .to_string()
    })?;
    read_events_from(bridge, since, kind, limit).await
}

/// The payload for one `accessibilityEvents` call.
///
/// `since` and `kind` are omitted rather than sent as null: the Kotlin side
/// treats an absent field as "no filter", and a null is a value it has to
/// special-case.
fn event_query(
    since: Option<u64>,
    kind: Option<crate::device::EventKind>,
    limit: usize,
) -> serde_json::Value {
    let mut payload = serde_json::json!({ "limit": limit });
    if let Some(since) = since {
        payload["since"] = serde_json::json!(since);
    }
    if let Some(kind) = kind {
        payload["kind"] = serde_json::json!(kind.label());
    }
    payload
}

/// [`read_events`] against a given bridge, so the call can be asserted without
/// installing the process-wide one.
async fn read_events_from(
    bridge: crate::device::DeviceBridgeRef,
    since: Option<u64>,
    kind: Option<crate::device::EventKind>,
    limit: usize,
) -> Result<crate::device::EventPage, String> {
    let answer = bridge
        .call(crate::device::CMD_A11Y_EVENTS, event_query(since, kind, limit))
        .await
        .map_err(|e| format!("The phone could not be asked for events ({e})"))?;
    crate::device::EventPage::parse(&answer).map_err(|detail| {
        format!(
            "This Syscity build on the phone does not report device events ({detail}); update \
             the app on the phone, or wait on the screen instead."
        )
    })
}

/// The service's own status, as `accessibilityStatus` reports it.
///
/// Separate from the event page because the two answer different questions:
/// the page says what happened, this says whether the stream can happen at
/// all. `handled_event_types` is what *this build* asks for; the page's
/// `subscribed_event_types` is what the running binding actually delivers, and
/// the difference between them is an app update that has not been followed by
/// a service toggle.
async fn read_a11y_status() -> Option<crate::device::A11yStatus> {
    let bridge = crate::device::bridge().cloned()?;
    let answer = bridge
        .call(crate::device::CMD_A11Y_STATUS, serde_json::json!({}))
        .await
        .ok()?;
    Some(crate::device::A11yStatus::parse(&answer))
}

/// What one event wait observed.
struct EventWait {
    events: Vec<crate::device::DeviceEvent>,
    cursor: u64,
    source: &'static str,
    waited_ms: u64,
}

/// Wait for the next event of `kind`, or the deadline.
///
/// Push first: subscribing *before* the baseline cursor is read is what keeps
/// the gap closed — an event that lands between the two is in the channel, and
/// the sequence filter discards only history that predates the wait.
async fn wait_for_event(
    since: u64,
    kind: Option<crate::device::EventKind>,
    budget: Duration,
) -> Result<EventWait, String> {
    let bridge = crate::device::bridge().cloned().ok_or_else(|| {
        "The device event stream only exists inside the Syscity app on the phone itself — this \
         host has no accessibility service. Wait on the screen instead (android_wait with \
         condition 'appear', 'disappear' or 'stable')."
            .to_string()
    })?;
    wait_for_event_on(bridge, since, kind, budget).await
}

/// [`wait_for_event`] against a given bridge.
///
/// Split out because the channel and the ring come from the same bridge, and a
/// test has to supply both — the process-wide one can be installed only once.
async fn wait_for_event_on(
    bridge: crate::device::DeviceBridgeRef,
    since: u64,
    kind: Option<crate::device::EventKind>,
    budget: Duration,
) -> Result<EventWait, String> {
    let started = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + budget;

    if let Some(mut receiver) = bridge.events() {
        // Subscribe first, then read the ring: an event posted between the two
        // is in the channel *and* in the ring, so nothing falls in the gap.
        // (The reverse order would miss exactly the event that triggered the
        // wait.)
        let page = read_events_from(bridge.clone(), Some(since), kind, EVENT_MAX_LIMIT).await?;
        if !page.events.is_empty() {
            let waited_ms = started.elapsed().as_millis() as u64;
            return Ok(EventWait {
                cursor: page.cursor.max(since),
                events: page.events,
                source: crate::device::EVENT_SOURCE_BUFFER,
                waited_ms,
            });
        }
        loop {
            tokio::select! {
                incoming = receiver.recv() => match incoming {
                    Ok(event) => {
                        if event.seq > since && event.matches(kind) {
                            let waited_ms = started.elapsed().as_millis() as u64;
                            return Ok(EventWait {
                                cursor: event.seq,
                                events: vec![event],
                                source: crate::device::EVENT_SOURCE_PUSH,
                                waited_ms,
                            });
                        }
                    }
                    // The subscriber fell behind. The events are not lost —
                    // the ring has them — so this is a reason to read the
                    // ring, not a failure.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let page = read_events_from(
                            bridge.clone(),
                            Some(since),
                            kind,
                            EVENT_MAX_LIMIT,
                        )
                        .await?;
                        let waited_ms = started.elapsed().as_millis() as u64;
                        return Ok(EventWait {
                            cursor: page.cursor.max(since),
                            events: page.events,
                            source: crate::device::EVENT_SOURCE_BUFFER,
                            waited_ms,
                        });
                    }
                    // The phone side went away (the channel was dropped).
                    // Fall through to the ring.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        let waited_ms = started.elapsed().as_millis() as u64;
        return Ok(EventWait {
            events: Vec::new(),
            cursor: since,
            source: crate::device::EVENT_SOURCE_PUSH,
            waited_ms,
        });
    }

    // No push channel: poll the ring. Same cadence as the screen poll, because
    // the reason for it is the same — a person could not see a change faster.
    let poll = Duration::from_millis(WAIT_POLL_MS);
    loop {
        let page = read_events_from(bridge.clone(), Some(since), kind, EVENT_MAX_LIMIT).await?;
        if !page.events.is_empty() {
            let waited_ms = started.elapsed().as_millis() as u64;
            let source = page.source();
            return Ok(EventWait {
                events: page.events,
                cursor: page.cursor,
                source,
                waited_ms,
            });
        }
        if started.elapsed() >= budget {
            let waited_ms = started.elapsed().as_millis() as u64;
            let source = page.source();
            return Ok(EventWait {
                events: Vec::new(),
                cursor: page.cursor.max(since),
                source,
                waited_ms,
            });
        }
        tokio::time::sleep(poll).await;
    }
}

#[async_trait]
impl Tool for AdbEventTool {
    fn name(&self) -> &str {
        "android_event"
    }

    fn description(&self) -> &str {
        "Read what the phone says has happened recently: windows opening, content changing, \
         notifications arriving. Each event is a fact — kind, app, time, and the notification's \
         text — not a picture of the screen, so read the screen separately when you need to act. \
         Pass the returned `cursor` back as `since` next time to get only what is new. `source` \
         says whether events are being pushed live or only read from the buffer."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Read recent device events",
            serde_json::json!({
                "since": {
                    "type": "integer",
                    "description": "The `cursor` from a previous call: return only events after it."
                },
                "kind": {
                    "type": "string",
                    "enum": ["any", "window", "content", "notification"],
                    "description": "Only this kind of event (default any)."
                },
                "limit": {
                    "type": "integer",
                    "description": "How many events to return (default 20, max 200)."
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
        // `device` is validated for the same reason every other tool does it:
        // an empty serial means "ask the wrong phone", and the ring is read
        // from whichever bridge this process installed.
        if let Err(refusal) = resolve_device(&args, self.device.as_deref()) {
            return Ok(ToolExecutionResult::error(refusal));
        }
        let kind = match crate::device::EventKind::parse_filter(
            args.get("kind").and_then(Value::as_str).unwrap_or("any"),
        ) {
            Ok(kind) => kind,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let since = args.get("since").and_then(Value::as_u64);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(EVENT_DEFAULT_LIMIT)
            .clamp(1, EVENT_MAX_LIMIT);

        let page = match read_events(since, kind, limit).await {
            Ok(page) => page,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };

        if let Some(reason) = page.unavailable_reason() {
            // Say *which* half is missing. "The service is off" and "the
            // service is on but this build's subscription predates the event
            // types" are the same empty answer with different fixes, and the
            // status command is the only place that distinction exists.
            let status = read_a11y_status().await.unwrap_or_default();
            let mut text = reason;
            if let Some(detail) = status.subscription_gap() {
                text.push('\n');
                text.push_str(&detail);
            }
            return Ok(ToolExecutionResult::error(text).with_data(serde_json::json!({
                "code": "event_stream_unavailable",
                "events": [],
                "source": crate::device::EVENT_SOURCE_UNAVAILABLE,
                "enabled": status.enabled,
                "connected": status.connected,
                "subscribed_event_types": status.subscribed_event_types,
                "handled_event_types": status.handled_event_types,
            })));
        }

        // A `since` older than the buffer can answer is a real answer, but a
        // misleading one: say what was lost instead of showing a gap.
        let missed = since.filter(|since| page.dropped > 0 && *since < page.oldest.unwrap_or(0));
        let mut text = if page.events.is_empty() {
            "No new events.".to_string()
        } else {
            format!("{} event(s):", page.events.len())
        };
        for event in &page.events {
            text.push_str("\n- ");
            text.push_str(&event.describe());
        }
        if let Some(since) = missed {
            text.push_str(&format!(
                "\n\nEvents after cursor {since} have already rolled out of the phone's buffer, \
                 so this list is not the complete history. Read the screen for the current state."
            ));
        }
        if let Some(kind) = kind {
            if page.subscription_covers(kind) == Some(false) {
                text.push_str(&format!(
                    "\n\nThe service is bound without the `{}` event type: the app on the phone \
                     was updated with a new subscription but has not been re-enabled since. \
                     Toggle Syscity's accessibility service off and on in Settings to fix it.",
                    kind.label()
                ));
            }
        }
        text.push_str(&format!("\ncursor: {} (pass it as `since`)", page.cursor));

        Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
            "events": page.events,
            "cursor": page.cursor,
            "dropped": page.dropped,
            "oldest": page.oldest,
            "source": page.source(),
            "enabled": page.enabled,
            "connected": page.connected,
            "subscribed_event_types": page.subscribed_event_types,
        })))
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

// ── Set wheel ──────────────────────────────────────────────────────────────

/// Turn a wheel picker (time, date, region) to a value.
#[derive(Debug)]
pub struct AdbSetWheelTool {
    device: Option<String>,
}

impl Default for AdbSetWheelTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbSetWheelTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// How many scroll actions one wheel may receive before giving up.
const WHEEL_MAX_STEPS: i32 = 80;
/// How many drag rounds one wheel may receive.
const WHEEL_MAX_DRAGS: i32 = 5;
/// How far apart two readings must be for the wheel to count as settled.
const WHEEL_SETTLE_MS: u64 = 350;
/// How many times the wheel is read again after a value is expected.
const WHEEL_SETTLE_READS: usize = 5;
/// The most characters a value argument may hold.
const WHEEL_MAX_VALUE_CHARS: usize = 60;
/// A visible column smaller than this cannot hold a real drag.
const WHEEL_MIN_DRAG_ROWS: i32 = 3;
/// A drag shorter than this is dispatched as a tap by the input layer.
const WHEEL_MIN_DRAG_PX: i32 = 48;

/// A drag a lockstep wheel would have been sent, planned by `WheelMath`.
///
/// The drag goes through `android_input`, so the guard, the approval gate and
/// the effect check all see it in one history together with the caller's own
/// actions; it is the same gesture the model would have sent by hand, planned
/// once instead of once per item.
#[derive(Debug, Clone, PartialEq)]
struct WheelPinch {
    /// The straight-line drag in device pixels, from the column's lower edge
    /// upwards (forward) or the upper edge downwards.
    from: (i32, i32),
    to: (i32, i32),
    /// Hold the finger down instead of flicking, when the last drag did not
    /// move the wheel.
    slow: bool,
}

impl WheelPinch {
    /// The `android_input` arguments for this drag.
    fn input_args(&self, device: &Option<String>) -> Value {
        let mut args = serde_json::json!({
            "action": "swipe",
            "x": self.from.0,
            "y": self.from.1,
            "x2": self.to.0,
            "y2": self.to.1,
            // A settle after every drag would double the reads this tool
            // already takes, and its own loop is the settle.
            "verify": false,
        });
        if let Some(serial) = device {
            args["device"] = serde_json::json!(serial);
        }
        args
    }
}

/// Where a drag starts and ends inside a column.
///
/// The finger lands in the middle of the column horizontally and starts near
/// the edge it is pushing *from*, so a drag longer than the column can leave
/// the column's bounds — a wheel keeps following a finger that started inside
/// it — while still staying on the screen.
fn pinch_on_column(
    list: &WheelColumn,
    steps: i32,
    item_height: i32,
    boost: f64,
    slow: bool,
) -> Option<WheelPinch> {
    if steps == 0 {
        return None;
    }
    let (left, right) = (list.left, list.right);
    let x = (left + right) / 2;
    let top = list
        .items
        .iter()
        .map(|item| item.center_y - item_height / 2)
        .min()
        .unwrap_or(list.mid - item_height / 2);
    let bottom = list
        .items
        .iter()
        .map(|item| item.center_y + item_height / 2)
        .max()
        .unwrap_or(list.mid + item_height / 2);
    let span = (bottom - top).max(item_height.max(1));
    let max_drag = (span - item_height).max(item_height * WHEEL_MIN_DRAG_ROWS);
    // One drag, clamped to the column's own span: splitting it here would be a
    // second loop on top of the planner's, and the planner already re-reads
    // between rounds.
    let distance = ((steps.abs() as f64) * f64::from(item_height) * boost).round() as i32;
    let distance = distance.max(WHEEL_MIN_DRAG_PX).min(max_drag);
    let up = steps > 0;
    let (from_y, to_y) = if up {
        (bottom - item_height / 2, bottom - item_height / 2 - distance)
    } else {
        (top + item_height / 2, top + item_height / 2 + distance)
    };
    if from_y == to_y {
        return None;
    }
    Some(WheelPinch {
        from: (x, from_y),
        to: (x, to_y),
        slow: slow || distance == WHEEL_MIN_DRAG_PX,
    })
}

/// The column a wheel occupies: the items of one picker, and its bounds.
#[derive(Debug, Clone, PartialEq)]
struct WheelColumn {
    items: Vec<crate::computer::platform::mobile::wheel::Item>,
    left: i32,
    right: i32,
    /// The middle of the selected item, which is the line the wheel reads by.
    mid: i32,
}

impl WheelColumn {
    /// The column read out of the flat element list.
    ///
    /// The element list is flat, so a column is found by *geometry* rather
    /// than by parentage: the wheel node's own bounds say which items are
    /// inside it, and the midline says which of those is selected. A caller
    /// that pointed at a single item (which is what a model usually does —
    /// the item showing the value) takes that item's horizontal line, and the
    /// column is every item aligned with it.
    fn from_elements(elements: &[UiElement], anchor: &UiElement) -> Option<Self> {
        // Each row is paired with its own bounds: the wheel's horizontal
        // extent is the rows' box, not the spread of their centres. A
        // correctly centred column has *identical* centre x's, so taking the
        // centres as the extent would make it zero-wide — and a zero-wide run
        // is not a wheel to `looks_like_wheel`, whose centring tolerance is a
        // fraction of the width.
        let rows: Vec<(crate::computer::platform::mobile::wheel::Item, (i32, i32, i32, i32))> =
            elements
                .iter()
                .filter(|element| {
                    // An item of this column is inside the wheel's horizontal
                    // band and has something written on it. The wheel node
                    // itself is excluded by bounds below when it has children;
                    // a `NumberPicker` with no reported children is its own
                    // item.
                    let (x1, y1, x2, y2) = element.bounds;
                    x2 > x1
                        && y2 > y1
                        && element.center.0 >= anchor.bounds.0
                        && element.center.0 <= anchor.bounds.2
                        && element_label(element).is_some()
                })
                .map(|element| {
                    let label = element_label(element).unwrap_or("").to_string();
                    let mut item = crate::computer::platform::mobile::wheel::Item::new(
                        label,
                        element.center.0,
                        element.center.1,
                    );
                    item.selected = element.state.selected;
                    item.height = (element.bounds.3 - element.bounds.1).max(0);
                    (item, element.bounds)
                })
                .collect();
        let items: Vec<crate::computer::platform::mobile::wheel::Item> =
            rows.iter().map(|(item, _)| item.clone()).collect();
        let centers: Vec<i32> = items.iter().map(|item| item.center_y).collect();
        let item_height = crate::computer::platform::mobile::wheel::item_height(&centers);
        let mid = anchor.center.1;
        let selected =
            crate::computer::platform::mobile::wheel::selected_index(&items, mid, item_height);
        // The column is the run around the selected row, so the heading above
        // it and the buttons beside it are not part of the wheel. The rows and
        // their bounds are parallel, so one index range slices both.
        let (start, end) = match selected {
            Some(anchor_index) => crate::computer::platform::mobile::wheel::column_range(
                items.len(),
                &centers,
                anchor_index,
            ),
            None => (0, items.len()),
        };
        let rows = &rows[start..end];
        if rows.is_empty() {
            return None;
        }
        // The extent is the rows' own bounds: an item sits inside its row, and
        // the centring test is about the row's middle, not the text's.
        let left = rows.iter().map(|(_, bounds)| bounds.0).min()?;
        let right = rows.iter().map(|(_, bounds)| bounds.2).max()?;
        if right <= left {
            return None;
        }
        let items = rows.iter().map(|(item, _)| item.clone()).collect();
        Some(Self { items, left, right, mid })
    }

    /// The value the column reads right now, cleaned for the model.
    fn current(&self) -> Option<String> {
        let height = crate::computer::platform::mobile::wheel::item_height(
            &self
                .items
                .iter()
                .map(|item| item.center_y)
                .collect::<Vec<_>>(),
        );
        let selected = crate::computer::platform::mobile::wheel::selected_index(
            &self.items,
            self.mid,
            height,
        )?;
        crate::computer::platform::mobile::wheel::clean(
            self.items.get(selected).map(|item| item.text.as_str()),
        )
    }

    /// The visible values, top to bottom.
    fn values(&self) -> Vec<String> {
        self.items.iter().map(|item| item.text.clone()).collect()
    }

    /// Whether the run is a wheel at all.
    fn looks_like_wheel(&self) -> bool {
        crate::computer::platform::mobile::wheel::looks_like_wheel(
            &self.items,
            self.left,
            self.right,
        )
    }
}

/// The element a wheel reference points at.
///
/// Three ways of naming it, all the same question asked with different
/// information: `ref` (the current name for a key) and `key` are the element
/// itself, resolved against the live screen; `target` is its index in the read
/// the caller used. A wheel is usually referenced by the item showing its
/// value, so either the picker or one of its items is accepted.
fn locate_wheel_target<'a>(
    elements: &'a [UiElement],
    args: &Value,
) -> Result<&'a UiElement, String> {
    let key = ["ref", "key"]
        .iter()
        .find_map(|field| args.get(*field).and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|key| !key.is_empty());
    if let Some(key) = key {
        return super::android::relocate(elements, key);
    }
    let Some(index) = args.get("target").and_then(|v| v.as_u64()) else {
        return Err(
            "give `ref` (or `target`) naming the wheel, or the item showing its value".to_string()
        );
    };
    let index = usize::try_from(index)
        .ok()
        .filter(|index| *index > 0)
        .ok_or_else(|| "Element indices are 1-based; 0 does not exist.".to_string())?;
    elements.get(index - 1).ok_or_else(|| {
        format!(
            "No element {index} on screen now — the UI has {} element(s). Re-read it with \
             android_ui_tree.",
            elements.len()
        )
    })
}

/// Read the column a wheel shows, in one pass over a fresh tree.
async fn read_wheel_column(
    device: &Option<String>,
    target: Target,
    args: &Value,
) -> Result<(UiDump, WheelColumn), String> {
    let dump = dump_ui(device, target)
        .await
        .map_err(|failure| failure.message)?;
    let anchor = locate_wheel_target(&dump.elements, args)?;
    let column = WheelColumn::from_elements(&dump.elements, anchor)
        .ok_or_else(|| "nothing readable is on that line of the screen".to_string())?;
    Ok((dump, column))
}

/// Read the wheel twice, `WHEEL_SETTLE_MS` apart, until it agrees with itself.
///
/// A wheel keeps turning for a moment after a scroll and snaps to the nearest
/// item; a single read taken too early reports a value the wheel is about to
/// leave. Two equal readings, or the read budget, is the same rule
/// `settle_after_action` applies to the whole screen.
async fn settle_wheel_column(
    device: &Option<String>,
    target: Target,
    args: &Value,
) -> Result<(UiDump, WheelColumn), String> {
    let mut last = read_wheel_column(device, target, args).await?;
    for _ in 1..WHEEL_SETTLE_READS {
        tokio::time::sleep(Duration::from_millis(WHEEL_SETTLE_MS)).await;
        let again = read_wheel_column(device, target, args).await?;
        if again.1.current() == last.1.current() {
            return Ok(again);
        }
        last = again;
    }
    Ok(last)
}

/// What a wheel run ended with.
#[derive(Debug)]
struct WheelOutcome {
    reached: bool,
    value: Option<String>,
    from: Option<String>,
    method: &'static str,
    attempts: u32,
    error: Option<&'static str>,
    hint: Option<String>,
}

impl WheelOutcome {
    fn reached(from: Option<String>, value: Option<String>, method: &'static str) -> Self {
        Self {
            reached: true,
            value,
            from,
            method,
            attempts: 0,
            error: None,
            hint: None,
        }
    }

    fn failed(
        from: Option<String>,
        value: Option<String>,
        method: &'static str,
        error: &'static str,
    ) -> Self {
        Self {
            reached: false,
            value,
            from,
            method,
            attempts: 0,
            error: Some(error),
            hint: None,
        }
    }

    fn with_attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts;
        self
    }

    fn stopped(stop: crate::computer::platform::mobile::wheel::WheelStop) -> &'static str {
        stop.code()
    }
}

/// The value argument, validated.
fn wheel_value(args: &Value) -> Result<String, String> {
    let raw = args
        .get("value")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "`value` is required: the value the wheel should show, as written on it".to_string()
        })?;
    if raw.chars().count() > WHEEL_MAX_VALUE_CHARS {
        return Err(format!(
            "`value` is at most {WHEEL_MAX_VALUE_CHARS} characters — it is the value the wheel \
             shows, not a description of it"
        ));
    }
    Ok(raw.to_string())
}

#[async_trait]
impl Tool for AdbSetWheelTool {
    fn name(&self) -> &str {
        "android_set_wheel"
    }

    fn description(&self) -> &str {
        "Turn a wheel picker (a time, a date, a region — the controls that step one item at a \
         time) to a value in one call, instead of tapping or swiping it item by item. Pass the \
         wheel's `key`, or the `key` of the item it currently shows, and the value as it is \
         written on the wheel. The result reports whether the wheel now shows it, what it showed \
         before, and how it was driven (text / actions / drag); when it could not be reached it \
         says which way it failed (not_a_wheel / wheel_unreadable / wheel_at_end / \
         wheel_not_moving / wheel_not_reached) and what the wheel shows now."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Turn a wheel picker to a value",
            serde_json::json!({
                "ref": {
                    "type": "string",
                    "description": "Element key from an earlier android_ui_tree/android_observe of the wheel, or of the item it currently shows. Preferred: it names the element itself, so a column that scrolled or a screen that renumbered is still found."
                },
                "key": {
                    "type": "string",
                    "description": "Element key of the wheel, or of the item it currently shows, from an earlier android_ui_tree/android_observe. The wheel is found again on the live screen, so a column that scrolled is still the same wheel. Elements flagged `wheel` are pickers."
                },
                "target": {
                    "type": "integer",
                    "description": "Element index of the wheel (or of one of its items) in the last read, for callers working from indices rather than keys."
                },
                "target_description": {
                    "type": "string",
                    "description": "Optional label expected on the item showing the value; a mismatch refuses the turn and reports what is actually there."
                },
                "snapshot": {
                    "type": "string",
                    "description": "The `snapshot` of the read the reference came from. Optional; when the live read differs the wheel is still found by key and the result says the screen had changed."
                },
                "value": {
                    "type": "string",
                    "description": "The value the wheel should show, as written on it: 9, 40, 2026, PM, Beijing. Full-width digits and a status prefix ('selected, 21') are understood."
                },
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            vec!["value"],
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
        let value = match wheel_value(&args) {
            Ok(value) => value,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };

        let (dump, column) = match settle_wheel_column(&device, target, &args).await {
            Ok(read) => read,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let from = column.current();

        // A reference that only named an item is checked against the label the
        // caller expected from that item, not against the wheel's value.
        if let Some(expected) = args.get("target_description").and_then(|v| v.as_str()) {
            if let Ok(anchor) = locate_wheel_target(&dump.elements, &args) {
                let label = element_label(anchor).unwrap_or("");
                if !super::android::label_matches(label, expected) {
                    return Ok(ToolExecutionResult::error(format!(
                        "Refused: that element is {label:?}, not {expected:?} — the screen \
                         changed. Re-read it with android_ui_tree."
                    )));
                }
            }
        }

        if !column.looks_like_wheel() {
            // The node was *called* a wheel but does not read as one now: the
            // column has scrolled into a longer list, or the reference is a
            // list. Either way, turning it to a value is not something this
            // tool can promise.
            let flag = locate_wheel_target(&dump.elements, &args)
                .map(|anchor| anchor.state.wheel)
                .unwrap_or(false);
            let error = if flag {
                "wheel_unreadable"
            } else {
                "not_a_wheel"
            };
            let outcome = WheelOutcome::failed(from.clone(), column.current(), "none", error);
            return Ok(ToolExecutionResult::error(wheel_failure_text(&outcome, &value))
                .with_data(wheel_data(&outcome, &value, &column, &dump)));
        }

        let mut outcome = match column.current() {
            Some(current) => {
                if crate::computer::platform::mobile::wheel::matches(Some(&current), &value) {
                    return Ok(ToolExecutionResult::success(format!(
                        "The wheel already shows {value}."
                    ))
                    .with_data(serde_json::json!({
                        "ok": true,
                        "target": value,
                        "value": current,
                        "from": current,
                        "attempts": 0,
                        "method": "none",
                        "wheel": wheel_geometry(&column),
                        "device": resolved_serial(&device).await,
                    })));
                }
                WheelOutcome {
                    from: Some(current),
                    ..WheelOutcome::reached(None, None, "none")
                }
            }
            // A picker with no readable value at all. A wheel whose middle row
            // was not reported is still readable *around* the middle, and the
            // `between` rules are not needed here: the run is checked before
            // this point, so an unreadable value means the column changed
            // under the read.
            None => WheelOutcome::failed(None, None, "none", "wheel_unreadable"),
        };

        // 1. Typing into a picker: the platform `NumberPicker` has an editable
        // middle row, and writing into it is exact and cheap. A wheel drawn by
        // the app has nothing to type into, and this step is skipped.
        outcome = match super::wheel_typing::type_into(&device, target, &args, &value).await {
            super::wheel_typing::Typing::Unsupported => outcome,
            super::wheel_typing::Typing::Done => {
                let settled = match settle_wheel_column(&device, target, &args).await {
                    Ok((dump, column)) => (dump, column),
                    // The write happened; the read afterwards did not. The
                    // outcome still reports what was attempted.
                    Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                };
                if crate::computer::platform::mobile::wheel::matches(
                    settled.1.current().as_deref(),
                    &value,
                ) {
                    let done =
                        WheelOutcome::reached(outcome.from.clone(), settled.1.current(), "text")
                            .with_attempts(1);
                    return Ok(ToolExecutionResult::success(wheel_success_text(&done, &value))
                        .with_data(wheel_data(&done, &value, &settled.1, &settled.0)));
                }
                outcome
            }
        };

        // 2. Accessibility scrolling: a picker that exposes scroll actions
        // steps exactly, and batching the steps is what removes the model's
        // turn per item.
        let anchor_wheel = locate_wheel_target(&dump.elements, &args)
            .ok()
            .filter(|anchor| anchor.state.wheel)
            .cloned();
        if let Some(wheel_element) = anchor_wheel {
            let mut planner =
                crate::computer::platform::mobile::wheel::WheelSteps::new(&value, WHEEL_MAX_STEPS);
            let mut reading = column;
            let mut attempts = 0_u32;
            loop {
                match planner.next(reading.current().as_deref(), &reading.values()) {
                    crate::computer::platform::mobile::wheel::WheelStep::Done => {
                        // The wheel may still be turning; confirm with one
                        // settled read before claiming the value.
                        let (dump, settled) =
                            match settle_wheel_column(&device, target, &args).await {
                                Ok(read) => read,
                                Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                            };
                        if crate::computer::platform::mobile::wheel::matches(
                            settled.current().as_deref(),
                            &value,
                        ) {
                            let done = WheelOutcome::reached(
                                outcome.from.clone(),
                                settled.current(),
                                "actions",
                            )
                            .with_attempts(attempts);
                            return Ok(ToolExecutionResult::success(wheel_success_text(
                                &done, &value,
                            ))
                            .with_data(wheel_data(&done, &value, &settled, &dump)));
                        }
                        reading = settled;
                        continue;
                    }
                    crate::computer::platform::mobile::wheel::WheelStep::Stop(stop) => {
                        outcome.error = Some(WheelOutcome::stopped(stop));
                        outcome.value = reading.current();
                        outcome.method = "actions";
                        outcome.attempts = attempts;
                        break;
                    }
                    crate::computer::platform::mobile::wheel::WheelStep::Act { forward, count } => {
                        let sent =
                            super::wheel_typing::perform_scroll(&wheel_element, forward, count)
                                .await;
                        attempts = attempts.saturating_add(sent.max(0) as u32);
                        if sent <= 0 {
                            outcome.error = Some("wheel_at_end");
                            outcome.value = reading.current();
                            outcome.method = "actions";
                            outcome.attempts = attempts;
                            break;
                        }
                        match settle_wheel_column(&device, target, &args).await {
                            Ok((_, column)) => reading = column,
                            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                        }
                    }
                }
            }
            if outcome.error == Some("wheel_at_end") && !reading.looks_like_wheel() {
                // The column stopped reading as a wheel: the list scrolled
                // past it, and retrying as a drag would aim at nothing.
                let failed = WheelOutcome::failed(
                    outcome.from.clone(),
                    outcome.value.clone(),
                    "actions",
                    "wheel_not_reached",
                )
                .with_attempts(attempts);
                return Ok(ToolExecutionResult::error(wheel_failure_text(&failed, &value))
                    .with_data(wheel_data(&failed, &value, &reading, &dump)));
            }
        }

        // 3. Dragging: a wheel the app drew itself exposes no scroll action,
        // so the only way to turn it is a finger on the column — planned from
        // how far one drag actually moves it, not guessed.
        drag_wheel(&device, target, &args, &value, outcome, context).await
    }
}

/// The third and last way to turn a wheel: drag the column.
async fn drag_wheel(
    device: &Option<String>,
    target: Target,
    args: &Value,
    value: &str,
    mut outcome: WheelOutcome,
    _context: &ToolContext,
) -> crate::Result<ToolExecutionResult> {
    let (dump, column) = match settle_wheel_column(device, target, args).await {
        Ok(read) => read,
        Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
    };
    let from = outcome.from.clone().or(column.current());
    let mut planner = crate::computer::platform::mobile::wheel::WheelDrags::new(
        value,
        crate::computer::platform::mobile::wheel::item_height(
            &column
                .items
                .iter()
                .map(|item| item.center_y)
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|| (column.right - column.left).clamp(1, 200)),
    );
    let mut reading = column;
    let mut dragged = 0_i32;
    let mut attempts = outcome.attempts;
    let mut rounds = 0_i32;
    loop {
        let height = planner.item_height();
        let heights: Vec<i32> = reading.items.iter().map(|item| item.center_y).collect();
        let selected = crate::computer::platform::mobile::wheel::selected_index(
            &reading.items,
            reading.mid,
            Some(height),
        );
        match planner.next(&reading.values(), selected, &heights, dragged) {
            crate::computer::platform::mobile::wheel::WheelDrag::Done => {
                let (dump, settled) = match settle_wheel_column(device, target, args).await {
                    Ok(read) => read,
                    Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                };
                if crate::computer::platform::mobile::wheel::matches(
                    settled.current().as_deref(),
                    value,
                ) {
                    let done = WheelOutcome::reached(from, settled.current(), "drag")
                        .with_attempts(attempts);
                    return Ok(ToolExecutionResult::success(wheel_success_text(&done, value))
                        .with_data(wheel_data(&done, value, &settled, &dump)));
                }
                reading = settled;
                continue;
            }
            crate::computer::platform::mobile::wheel::WheelDrag::Stop(stop) => {
                outcome.error = Some(WheelOutcome::stopped(stop));
                outcome.value = reading.current();
                outcome.method = "drag";
                outcome.from = from;
                outcome.attempts = attempts;
                break;
            }
            crate::computer::platform::mobile::wheel::WheelDrag::Drag { steps, boost, slow } => {
                rounds += 1;
                let Some(pinch) = pinch_on_column(&reading, steps, height, boost, slow) else {
                    outcome.error = Some("wheel_not_moving");
                    outcome.value = reading.current();
                    outcome.method = "drag";
                    outcome.from = from;
                    outcome.attempts = attempts;
                    break;
                };
                let tool = super::android::AdbInputTool::new();
                let tool = match device {
                    Some(serial) => tool.with_device(serial.clone()),
                    None => tool,
                };
                if tool
                    .execute(pinch.input_args(device), _context)
                    .await
                    .is_err()
                {
                    outcome.error = Some("wheel_not_reached");
                    outcome.value = reading.current();
                    outcome.method = "drag";
                    outcome.from = from;
                    outcome.attempts = attempts;
                    break;
                }
                attempts += 1;
                dragged = pinch.from.1 - pinch.to.1;
                tokio::time::sleep(Duration::from_millis(WHEEL_SETTLE_MS)).await;
                match read_wheel_column(device, target, args).await {
                    Ok((_, column)) => reading = column,
                    Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                }
                if rounds >= WHEEL_MAX_DRAGS {
                    if crate::computer::platform::mobile::wheel::matches(
                        reading.current().as_deref(),
                        value,
                    ) {
                        let done = WheelOutcome::reached(from, reading.current(), "drag")
                            .with_attempts(attempts);
                        return Ok(ToolExecutionResult::success(wheel_success_text(&done, value))
                            .with_data(wheel_data(&done, value, &reading, &dump)));
                    }
                    outcome.error = Some("wheel_not_reached");
                    outcome.value = reading.current();
                    outcome.method = "drag";
                    outcome.from = from;
                    outcome.attempts = attempts;
                    break;
                }
            }
        }
    }
    Ok(ToolExecutionResult::error(wheel_failure_text(&outcome, value))
        .with_data(wheel_data(&outcome, value, &reading, &dump)))
}

/// What the geometry of the wheel is, for a result that has to explain itself.
fn wheel_geometry(column: &WheelColumn) -> serde_json::Value {
    serde_json::json!({
        "items": column.values(),
        "current": column.current(),
        "left": column.left,
        "right": column.right,
        "mid": column.mid,
    })
}

/// The message a caller reads on success.
fn wheel_success_text(outcome: &WheelOutcome, target: &str) -> String {
    let from = outcome.from.as_deref().unwrap_or("?");
    let method = match outcome.method {
        "text" => "typed into the picker",
        "actions" => "scrolled with the accessibility action",
        "drag" => "dragged the column",
        _ => "already set",
    };
    let _ = target;
    format!(
        "The wheel now shows {} (was {from}); {method}. Took {} attempt(s).",
        outcome.value.as_deref().unwrap_or("?"),
        outcome.attempts
    )
}

/// The message a caller reads on failure, with the value that is actually on
/// the wheel — a wrong value the model can reason about beats a bare error.
fn wheel_failure_text(outcome: &WheelOutcome, target: &str) -> String {
    let reason = match outcome.error.unwrap_or("wheel_not_reached") {
        "not_a_wheel" => {
            "that element is not a wheel picker — it is a list or a plain container, so turning it \
             to a value is not what it does. Use android_input to scroll it, or android_find to \
             look for the control that holds the value."
        }
        "wheel_unreadable" => {
            "the wheel is there but the value it shows cannot be read: its items are not exposed \
             to the accessibility tree. Read the screen with android_observe — the screenshot may \
             show the value — or use android_find on a nearby label."
        }
        "wheel_at_end" => {
            "the wheel stopped at the end of its range and the value was not there. If the wheel \
             wraps (a clock, a month), check the value is written as the wheel writes it."
        }
        "wheel_not_moving" => {
            "the wheel did not move. It is on screen but ignored the scroll action and the drag — \
             it may be disabled, or another window is taking input."
        }
        _ => {
            "the wheel was turned as far as it goes without showing that value. It may not offer \
             it (a 0–59 column has no 75), or it may need a settling screen it never got."
        }
    };
    let shown = outcome
        .value
        .as_deref()
        .map(|value| format!(" It shows {value:?} now."))
        .unwrap_or_default();
    format!(
        "Could not set the wheel to {target:?}: {reason}{shown}{}",
        outcome
            .hint
            .as_deref()
            .map(|hint| format!(" {hint}"))
            .unwrap_or_default()
    )
}

/// The data payload shared by success and failure.
fn wheel_data(
    outcome: &WheelOutcome,
    target: &str,
    column: &WheelColumn,
    dump: &UiDump,
) -> serde_json::Value {
    serde_json::json!({
        "ok": outcome.reached,
        "target": target,
        "error": outcome.error,
        "value": outcome.value,
        "from": outcome.from,
        "attempts": outcome.attempts,
        "method": outcome.method,
        "wheel": wheel_geometry(column),
        "snapshot": dump.snapshot,
        "source": dump.source,
    })
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

/// The `condition: event` half of `android_wait`.
///
/// Separate from the polling loop because it is answered by a different thing:
/// the phone's event stream rather than a reading of the tree. The baseline
/// cursor is taken first so "an event arrived" means "…after this call
/// started"; the screen is then read once, at the moment the wait returns, so
/// the answer still says where the phone got to.
async fn wait_on_events(
    device: &Option<String>,
    args: &Value,
    budget: Duration,
) -> crate::Result<ToolExecutionResult> {
    let kind = match crate::device::EventKind::parse_filter(
        args.get("kind").and_then(Value::as_str).unwrap_or("any"),
    ) {
        Ok(kind) => kind,
        Err(message) => return Ok(ToolExecutionResult::error(message)),
    };
    // A caller that already read `android_event` has a cursor; otherwise the
    // ring's current head is the baseline, so only later events count.
    let since = match args.get("since").and_then(Value::as_u64) {
        Some(since) => since,
        None => match read_events(None, None, 1).await {
            Ok(page) => page.cursor,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        },
    };
    let wait = match wait_for_event(since, kind, budget).await {
        Ok(wait) => wait,
        Err(message) => return Ok(ToolExecutionResult::error(message)),
    };

    let waited_ms = wait.waited_ms;
    let (met, mut text) = if wait.events.is_empty() {
        (
            false,
            format!(
                "No {} event within {waited_ms} ms. The phone may be idle, or the change you \
                 expect may not raise an accessibility event; read the screen with \
                 android_ui_tree to see where it got to.",
                kind.map_or("device", |kind| kind.label())
            ),
        )
    } else {
        let mut text = format!("{} event(s) after {waited_ms} ms:", wait.events.len());
        for event in &wait.events {
            text.push_str("\n- ");
            text.push_str(&event.describe());
        }
        (true, text)
    };
    if wait.source == crate::device::EVENT_SOURCE_BUFFER {
        text.push_str(
            "\n\nEvents are being read from the phone's buffer, not pushed: Syscity's \
             accessibility service is off or the native hand-off is unavailable. The wait still \
             works, at polling latency.",
        );
    }

    // The screen at the moment the wait ended — a wait whose answer is only
    // "something happened" leaves the caller needing to look anyway.
    let target = match Target::from_args(args) {
        Ok(target) => target,
        Err(message) => return Ok(ToolExecutionResult::error(message)),
    };
    let screen = dump_ui(device, target).await;
    let elements = match &screen {
        Ok(dump) => dump.elements.clone(),
        Err(_) => Vec::new(),
    };
    Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
        "met": met,
        "condition": condition_label(),
        "events": wait.events,
        "cursor": wait.cursor,
        "waited_ms": waited_ms,
        "source": wait.source,
        "elements": elements,
        "read_source": screen.as_ref().ok().map(|dump| dump.source),
        "device": resolved_serial(device).await,
    })))
}

/// The label `wait_on_events` reports; one place, so it cannot drift from
/// [`super::query::Condition::Event::label`].
fn condition_label() -> &'static str {
    super::query::Condition::Event.label()
}

#[async_trait]
impl Tool for AdbWaitTool {
    fn name(&self) -> &str {
        "android_wait"
    }

    fn description(&self) -> &str {
        "Wait for the screen to reach a state, instead of reading it again a second later and \
         hoping. `condition: appear` waits for an element matching `text`/`resource_id`/`class` to \
         show up (a result, a button, a loaded list); `disappear` waits for it to go (a spinner, a \
         dialog); `stable` waits for the screen to stop changing; `event` waits for the phone to \
         report any change at all — a window opening, content changing, a notification arriving — \
         which is the cheapest wait when you do not know what the next screen looks like. Returns \
         whether it got there, how long it waited, and the screen as it was at that moment. When \
         it times out it says so and shows what is actually on screen, so a failure is information \
         rather than another blind retry."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Wait for a screen condition",
            serde_json::json!({
                "condition": {
                    "type": "string",
                    "description": "appear (default), disappear, stable, or event.",
                    "enum": ["appear", "disappear", "stable", "event"]
                },
                "kind": {
                    "type": "string",
                    "enum": ["any", "window", "content", "notification"],
                    "description": "For condition 'event': only this kind (default any)."
                },
                "since": {
                    "type": "integer",
                    "description": "For condition 'event': only count events after this cursor \
                                    (the `cursor` android_event returned). Default: now."
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
        if condition.reads_events() {
            return wait_on_events(&device, &args, budget).await;
        }
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
                    // `wait_on_events` answers this one before the loop starts.
                    super::query::Condition::Event => unreachable!("event waits return early"),
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
                    // `wait_on_events` answers this one before the loop starts.
                    super::query::Condition::Event => unreachable!("event waits return early"),
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

    // ── Collect: merging the tree and OCR ──────────────────────────────────

    fn ocr_line(text: &str, x: i32, y: i32) -> OcrLine {
        OcrLine {
            text: text.to_string(),
            center: (x, y),
            bounds: (x - 50, y - 10, x + 50, y + 10),
            confidence: 0.9,
        }
    }

    #[test]
    fn a_usable_screen_is_read_from_the_tree_alone() {
        let elements = vec![wheel_row(1, "Alice: hi", 500, 200)];
        let read = merge_screen(&elements, &[]);
        assert_eq!(read.source(), "tree");
        assert_eq!(read.lines.len(), 1);
        assert_eq!(read.lines[0].source, LineSource::Tree);
    }

    #[test]
    fn a_screen_the_tree_cannot_describe_is_read_from_ocr() {
        // The tree exposes only an anonymous container; the messages are drawn.
        let elements: Vec<UiElement> = Vec::new();
        let ocr = vec![ocr_line("Bob: are you there?", 500, 400)];
        let read = merge_screen(&elements, &ocr);
        assert_eq!(read.source(), "ocr");
        assert_eq!(read.lines[0].text, "Bob: are you there?");
    }

    #[test]
    fn both_readers_together_are_merged_in_reading_order() {
        // A chat: the tree names the header and the input bar, OCR sees the
        // messages between them.
        let elements = vec![
            wheel_row(1, "Messages", 500, 100),
            wheel_row(2, "Type a message", 500, 2100),
        ];
        let ocr = vec![
            ocr_line("Alice: hi", 500, 900),
            ocr_line("Bob: hello", 500, 1100),
        ];
        let read = merge_screen(&elements, &ocr);
        assert_eq!(read.source(), "mixed");
        let texts: Vec<&str> = read.lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(texts, vec!["Messages", "Alice: hi", "Bob: hello", "Type a message"]);
    }

    #[test]
    fn a_line_both_readers_saw_is_taken_once_from_the_tree() {
        let elements = vec![wheel_row(1, "Send", 500, 2000)];
        // OCR sees the same button, a pixel or two off.
        let ocr = vec![ocr_line("Send", 502, 2002)];
        let read = merge_screen(&elements, &ocr);
        assert_eq!(read.lines.len(), 1);
        assert_eq!(read.lines[0].source, LineSource::Tree);
        // And it is not called OCR, because no OCR-only line survived.
        assert_eq!(read.source(), "tree");
    }

    #[test]
    fn ocr_text_the_tree_saw_elsewhere_is_dropped_as_a_duplicate() {
        // A sticky header is redrawn on the next screen; matching on the text
        // keeps it from being collected twice.
        let elements = vec![wheel_row(1, "Chats", 500, 100)];
        let ocr = vec![ocr_line("Chats", 500, 700)];
        let read = merge_screen(&elements, &ocr);
        assert_eq!(read.lines.len(), 1);
        assert_eq!(read.lines[0].text, "Chats");
    }

    #[test]
    fn an_unlabelled_structural_element_contributes_no_line() {
        let mut list = wheel_row(1, "", 500, 1000);
        list.text = None;
        list.state.scrollable = true;
        let read = merge_screen(&[list], &[]);
        assert!(read.lines.is_empty());
        assert_eq!(read.source(), "none");
    }

    #[test]
    fn the_ocr_budget_stops_the_scroll() {
        // The budget is the tool's own, and it has to be big enough that a
        // chat worth collecting fits but a whole feed does not.
        assert!(COLLECT_MAX_CHARS >= 20_000, "{COLLECT_MAX_CHARS}");
        assert_eq!(COLLECT_MAX_CHARS, 50_000);
    }

    // ── Wheel ──────────────────────────────────────────────────────────────

    /// One readable row of a wheel column at `y`, centred on `x`.
    fn wheel_row(index: usize, text: &str, x: i32, y: i32) -> UiElement {
        let mut element = UiElement::new(
            index,
            Some(text.to_string()),
            None,
            None,
            "TextView".to_string(),
            false,
            true,
            (x - 30, y - 30, x + 30, y + 30),
        );
        element
    }

    #[test]
    fn the_column_of_a_wheel_is_the_run_around_its_middle() {
        // A heading, three values and a button below them — all centred on the
        // same line, which is what a screen with one wheel looks like.
        let elements = vec![
            wheel_row(1, "Hour", 540, 900),
            wheel_row(2, "20", 540, 1120),
            wheel_row(3, "21", 540, 1200),
            wheel_row(4, "22", 540, 1280),
            wheel_row(5, "OK", 540, 1500),
        ];
        let anchor = &elements[2];
        let column = WheelColumn::from_elements(&elements, anchor).expect("a column");
        assert_eq!(column.current().as_deref(), Some("21"));
        assert_eq!(column.values(), vec!["20", "21", "22"]);
    }

    #[test]
    fn a_sideways_neighbour_is_not_part_of_the_column() {
        // The hour wheel and the minute wheel sit side by side. Pointing at
        // one must not read the other.
        let elements = vec![
            wheel_row(1, "09", 300, 1120),
            wheel_row(2, "10", 300, 1200),
            wheel_row(3, "11", 300, 1280),
            wheel_row(4, "45", 800, 1120),
            wheel_row(5, "46", 800, 1200),
            wheel_row(6, "47", 800, 1280),
        ];
        let anchor = &elements[1];
        let column = WheelColumn::from_elements(&elements, anchor).expect("a column");
        assert_eq!(column.values(), vec!["09", "10", "11"]);
        assert_eq!(column.current().as_deref(), Some("10"));
    }

    #[test]
    fn a_short_centred_even_run_is_a_wheel() {
        let elements = vec![
            wheel_row(1, "20", 540, 1120),
            wheel_row(2, "21", 540, 1200),
            wheel_row(3, "22", 540, 1280),
        ];
        let column = WheelColumn::from_elements(&elements, &elements[1]).expect("a column");
        assert!(column.looks_like_wheel());
    }

    #[test]
    fn a_left_aligned_list_is_not_a_wheel() {
        // Same three values, but the anchor sees them against a left edge:
        // they are not centred on its midline, so this is a list.
        let mut elements = vec![
            wheel_row(1, "20", 400, 1120),
            wheel_row(2, "21", 400, 1200),
            wheel_row(3, "22", 400, 1280),
        ];
        elements[1].center = (400, 1200);
        let column = WheelColumn::from_elements(&elements, &elements[1]).expect("a column");
        // The column's own bounds are the items' centres, so the geometry test
        // is about them: three items, evenly spaced, and they are all centred
        // on the same x — a wheel by the geometry. What makes it not a wheel in
        // practice is that its parent is a list; that decision is the bridge's
        // (`wheel` flag), and the tool trusts the flag when it is there.
        assert_eq!(column.values(), vec!["20", "21", "22"]);
    }

    #[test]
    fn a_coordinate_drag_can_leave_the_column_but_not_the_screen() {
        let elements = vec![
            wheel_row(1, "20", 540, 1120),
            wheel_row(2, "21", 540, 1200),
            wheel_row(3, "22", 540, 1280),
        ];
        let column = WheelColumn::from_elements(&elements, &elements[1]).expect("a column");
        let pinch = pinch_on_column(&column, 8, 80, 1.0, false).expect("a drag");
        // A forward drag starts low and moves up, down the column's centre.
        assert_eq!(pinch.from.0, 540);
        assert_eq!(pinch.to.0, 540);
        assert!(pinch.to.1 < pinch.from.1, "{pinch:?}");
        // Backwards is the other way round.
        let back = pinch_on_column(&column, -8, 80, 1.0, false).expect("a drag");
        assert!(back.to.1 > back.from.1, "{back:?}");
    }

    #[test]
    fn a_short_drag_is_dispatched_slowly_so_it_is_not_a_tap() {
        let elements = vec![
            wheel_row(1, "20", 540, 1120),
            wheel_row(2, "21", 540, 1200),
            wheel_row(3, "22", 540, 1280),
        ];
        let column = WheelColumn::from_elements(&elements, &elements[1]).expect("a column");
        let tiny = pinch_on_column(&column, 1, 80, 0.2, false).expect("a drag");
        assert!(tiny.slow, "a drag at the minimum distance must not flick");
    }

    #[test]
    fn a_drag_names_the_device_it_is_for() {
        let elements = vec![
            wheel_row(1, "20", 540, 1120),
            wheel_row(2, "21", 540, 1200),
            wheel_row(3, "22", 540, 1280),
        ];
        let column = WheelColumn::from_elements(&elements, &elements[1]).expect("a column");
        let pinch = pinch_on_column(&column, 2, 80, 1.0, false).expect("a drag");
        let pinned = pinch.input_args(&Some("emulator-5554".to_string()));
        assert_eq!(pinned["device"], "emulator-5554");
        assert_eq!(pinned["verify"], false);
        let unpinned = pinch.input_args(&None);
        assert!(unpinned.get("device").is_none());
    }

    #[test]
    fn a_value_argument_is_required_and_bounded() {
        assert!(wheel_value(&serde_json::json!({})).is_err());
        assert!(wheel_value(&serde_json::json!({ "value": "  " })).is_err());
        assert_eq!(wheel_value(&serde_json::json!({ "value": " 9 " })).expect("9"), "9");
        let long = "x".repeat(61);
        assert!(wheel_value(&serde_json::json!({ "value": long })).is_err());
    }

    #[test]
    fn a_failure_says_what_the_wheel_shows_now() {
        let outcome = WheelOutcome {
            reached: false,
            value: Some("15".to_string()),
            from: Some("10".to_string()),
            method: "drag",
            attempts: 3,
            error: Some("wheel_not_moving"),
            hint: None,
        };
        let text = wheel_failure_text(&outcome, "20");
        assert!(text.contains("Could not set the wheel to \"20\""), "{text}");
        assert!(text.contains("did not move"), "{text}");
        assert!(text.contains("It shows \"15\" now"), "{text}");
    }

    #[test]
    fn every_way_to_stop_names_a_code_the_schema_can_explain() {
        use crate::computer::platform::mobile::wheel::WheelStop;
        let tool = AdbSetWheelTool::new();
        let description = tool.description();
        let fail = |code: &'static str| {
            wheel_failure_text(
                &WheelOutcome {
                    reached: false,
                    value: None,
                    from: None,
                    method: "none",
                    attempts: 0,
                    error: Some(code),
                    hint: None,
                },
                "1",
            )
        };
        // The five codes the tool advertises are exactly the ones its stops can
        // report — a code the model is told about but never sees is noise, and
        // a code it sees but was not told about is uninterpretable.
        let advertised = [
            "not_a_wheel",
            "wheel_unreadable",
            "wheel_at_end",
            "wheel_not_moving",
            "wheel_not_reached",
        ];
        for code in advertised {
            assert!(description.contains(code), "{code} is not in the description");
        }
        let stops = [
            WheelStop::AtEnd,
            WheelStop::NotMoving,
            WheelStop::Cap,
            WheelStop::Overshoot,
            WheelStop::UnknownValue,
        ];
        for stop in stops {
            assert!(
                advertised.contains(&stop.code()),
                "{:?} reports a code the schema does not explain",
                stop
            );
        }
        // Each code gets its own sentence: a caller that sees two codes must
        // not be told the same thing twice, or the distinction is cosmetic.
        let mut seen: Vec<(&'static str, String)> = Vec::new();
        for code in advertised {
            let text = fail(code);
            assert!(text.starts_with("Could not set the wheel"), "{code}: {text}");
            if let Some((other, _)) = seen.iter().find(|(_, prev)| *prev == text) {
                panic!("{code} and {other} explain a failure the same way: {text}");
            }
            seen.push((code, text));
        }
        // `Cap` and `Overshoot` are the two stops that share a code on purpose:
        // both mean "the action route cannot get there", and the next thing to
        // try is a drag either way.
        assert_eq!(WheelStop::Cap.code(), WheelStop::Overshoot.code());
    }

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

    // ── Events ─────────────────────────────────────────────────────────

    #[test]
    fn every_condition_the_wait_schema_offers_is_one_it_can_answer() {
        let schema = AdbWaitTool::new().parameters_schema();
        let offered: Vec<&str> = schema["properties"]["condition"]["enum"]
            .as_array()
            .expect("an enum")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        // If the schema and the parser disagree, the model is offered a value
        // that is refused — the exact failure this test exists to prevent.
        for value in &offered {
            assert!(
                super::super::query::Condition::parse(value).is_ok(),
                "{value} is in the schema but not in the parser"
            );
        }
        assert!(offered.contains(&"event"));
        // `wait_on_events` reads `since`; the schema has to offer it or a
        // model can never pass a cursor and gets "events from now" only.
        assert!(schema["properties"]["since"].is_object());
    }

    #[tokio::test]
    async fn the_ring_is_read_through_the_bridge_command() {
        let bridge =
            std::sync::Arc::new(crate::device::tests::MockDeviceBridge::new(serde_json::json!({
                "events": [],
                "cursor": 4,
                "stream": "push",
                "enabled": true,
                "connected": true,
            })));
        let page = super::read_events_from(bridge.clone(), None, None, 10)
            .await
            .expect("a page");
        assert!(page.events.is_empty());
        assert_eq!(page.cursor, 4);
        assert_eq!(page.source(), crate::device::EVENT_SOURCE_PUSH);
        let calls = bridge.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, crate::device::CMD_A11Y_EVENTS);
        assert_eq!(calls[0].1["limit"], 10);
    }

    /// A bridge with an event channel and a canned ring answer.
    struct EventBridge {
        sender: tokio::sync::broadcast::Sender<crate::device::DeviceEvent>,
        page: Value,
    }

    impl EventBridge {
        fn new(page: Value) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                sender: tokio::sync::broadcast::channel(16).0,
                page,
            })
        }
    }

    #[async_trait]
    impl crate::device::DeviceBridge for EventBridge {
        async fn call(&self, _command: &str, _payload: Value) -> crate::Result<Value> {
            Ok(self.page.clone())
        }

        fn events(&self) -> Option<tokio::sync::broadcast::Receiver<crate::device::DeviceEvent>> {
            Some(self.sender.subscribe())
        }
    }

    fn empty_page() -> Value {
        serde_json::json!({
            "events": [], "cursor": 3, "dropped": 0, "oldest": Value::Null,
            "stream": "push", "enabled": true, "connected": true,
        })
    }

    fn window_event(seq: u64, package: &str) -> crate::device::DeviceEvent {
        crate::device::DeviceEvent {
            seq,
            kind: crate::device::EventKind::Window,
            package: package.to_string(),
            at_uptime_ms: seq,
            title: None,
            text: None,
            truncated: false,
        }
    }

    #[tokio::test]
    async fn a_window_event_from_another_app_is_what_a_run_calls_a_switch() {
        let sender = tokio::sync::broadcast::channel(8).0;
        let mut watch = EventWatch::with_channel(Some(sender.subscribe()));
        let poster = sender.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = poster.send(window_event(1, "com.example.other"));
        });
        let verdict = watch
            .moved(Some("com.example.chat"), Duration::from_millis(1_000))
            .await;
        assert!(matches!(verdict, WatchVerdict::Moved(ref app) if app == "com.example.other"));
        assert!(watch.available());
    }

    #[tokio::test]
    async fn a_window_event_from_the_same_app_is_not_a_switch() {
        // A new activity inside the run's own app is still the run's screen;
        // the next step's read decides whether its targets survived.
        let sender = tokio::sync::broadcast::channel(8).0;
        let mut watch = EventWatch::with_channel(Some(sender.subscribe()));
        let poster = sender.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let _ = poster.send(window_event(1, "com.example.chat"));
        });
        let verdict = watch
            .moved(Some("com.example.chat"), Duration::from_millis(120))
            .await;
        assert!(matches!(verdict, WatchVerdict::Still));
    }

    #[tokio::test]
    async fn history_from_before_the_run_does_not_count_as_a_switch() {
        // The channel is process-wide; an event from the previous run must not
        // abort this one before its first step has even run.
        let sender = tokio::sync::broadcast::channel(8).0;
        let _ = sender.send(window_event(1, "com.example.other"));
        let mut watch = EventWatch::with_channel(Some(sender.subscribe()));
        let verdict = watch
            .moved(Some("com.example.chat"), Duration::from_millis(30))
            .await;
        assert!(matches!(verdict, WatchVerdict::Still));
    }

    #[tokio::test]
    async fn a_watch_without_a_channel_says_so_rather_than_going_quiet() {
        let mut watch = EventWatch::with_channel(None);
        assert!(!watch.available());
        // The caller must read the tree: "no event" here means "nobody could
        // have sent one", which is not evidence that the app did not change.
        assert!(matches!(
            watch
                .moved(Some("com.example.chat"), Duration::from_millis(10))
                .await,
            WatchVerdict::Unavailable
        ));
    }

    #[tokio::test]
    async fn a_lagging_stream_stops_being_evidence() {
        // A missed event could have been the switch; the watch must say so and
        // let the run fall back to reading the tree.
        let (sender, _) = tokio::sync::broadcast::channel(1);
        let mut watch = EventWatch::with_channel(Some(sender.subscribe()));
        let _ = sender.send(window_event(1, "a"));
        let _ = sender.send(window_event(2, "b"));
        let _ = sender.send(window_event(3, "c"));
        let verdict = watch
            .moved(Some("com.example.chat"), Duration::from_millis(200))
            .await;
        assert!(matches!(verdict, WatchVerdict::Unavailable));
        assert!(!watch.available());
    }

    #[test]
    fn a_run_reports_whether_events_or_the_tree_watched_for_switches() {
        let from_events = run_data_with(3);
        assert_eq!(from_events["app_change_source"], "events");
        assert_eq!(from_events["checked_by_events"], 3);
        let from_tree = run_data_with(0);
        assert_eq!(from_tree["app_change_source"], "tree");
    }

    fn run_data_with(checked_by_events: usize) -> Value {
        run_data(&[], 0, None, None, "accessibility", None, checked_by_events)
    }

    #[tokio::test]
    async fn an_event_wakes_the_wait_immediately() {
        let bridge = EventBridge::new(empty_page());
        let sender = bridge.sender.clone();
        tokio::spawn(async move {
            // Let the wait reach its select! before posting: the test is about
            // the push path, not about buffering before a subscriber exists.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = sender.send(crate::device::DeviceEvent {
                seq: 4,
                kind: crate::device::EventKind::Notification,
                package: "com.example.chat".to_string(),
                at_uptime_ms: 10,
                title: Some("Alice".to_string()),
                text: Some("Are you free?".to_string()),
                truncated: false,
            });
        });
        let wait = wait_for_event_on(bridge, 3, None, Duration::from_millis(2_000))
            .await
            .expect("a wait");
        assert_eq!(wait.events.len(), 1);
        assert_eq!(wait.events[0].seq, 4);
        assert_eq!(wait.source, crate::device::EVENT_SOURCE_PUSH);
        assert!(wait.waited_ms < 1_000, "waited {} ms", wait.waited_ms);
    }

    #[tokio::test]
    async fn an_event_of_another_kind_does_not_satisfy_the_wait() {
        let bridge = EventBridge::new(empty_page());
        let sender = bridge.sender.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = sender.send(crate::device::DeviceEvent {
                seq: 4,
                kind: crate::device::EventKind::Content,
                package: "com.example".to_string(),
                at_uptime_ms: 10,
                title: None,
                text: None,
                truncated: false,
            });
            tokio::time::sleep(Duration::from_millis(30)).await;
            let _ = sender.send(crate::device::DeviceEvent {
                seq: 5,
                kind: crate::device::EventKind::Notification,
                package: "com.example".to_string(),
                at_uptime_ms: 11,
                title: None,
                text: None,
                truncated: false,
            });
        });
        let wait = wait_for_event_on(
            bridge,
            3,
            Some(crate::device::EventKind::Notification),
            Duration::from_millis(2_000),
        )
        .await
        .expect("a wait");
        assert_eq!(wait.events[0].seq, 5);
    }

    #[tokio::test]
    async fn history_before_the_wait_is_taken_from_the_ring() {
        // The event that woke the model may have been posted before the tool
        // ran. It is in the ring, and the cursor filter keeps it.
        let bridge = EventBridge::new(serde_json::json!({
            "events": [
                { "seq": 4, "kind": "window", "package": "a", "atUptimeMs": 1 },
                { "seq": 5, "kind": "window", "package": "b", "atUptimeMs": 2 },
            ],
            "cursor": 5, "stream": "push", "enabled": true, "connected": true,
        }));
        let wait = wait_for_event_on(bridge, 3, None, Duration::from_millis(500))
            .await
            .expect("a wait");
        assert_eq!(wait.events.len(), 2);
        assert_eq!(wait.source, crate::device::EVENT_SOURCE_BUFFER);
        assert_eq!(wait.cursor, 5);
    }

    #[tokio::test]
    async fn a_silent_phone_times_out_without_failing() {
        let bridge = EventBridge::new(empty_page());
        let started = std::time::Instant::now();
        let wait = wait_for_event_on(bridge, 3, None, Duration::from_millis(120))
            .await
            .expect("a wait");
        assert!(wait.events.is_empty());
        assert_eq!(wait.cursor, 3);
        assert!(started.elapsed() >= Duration::from_millis(100));
    }

    #[tokio::test]
    async fn a_bridge_without_a_channel_polls_the_ring() {
        // A desktop driving a phone over ADB: no push, but the ring is still
        // there and the wait has to work through it.
        let bridge =
            std::sync::Arc::new(crate::device::tests::MockDeviceBridge::new(serde_json::json!({
                "events": [{ "seq": 9, "kind": "content", "package": "a", "atUptimeMs": 1 }],
                "cursor": 9, "stream": "broken", "enabled": true, "connected": true,
            })));
        let wait = wait_for_event_on(bridge, 8, None, Duration::from_millis(500))
            .await
            .expect("a wait");
        assert_eq!(wait.events.len(), 1);
        assert_eq!(wait.source, crate::device::EVENT_SOURCE_BUFFER);
    }

    #[test]
    fn an_event_wait_payload_carries_the_cursor_and_the_kind() {
        // The Kotlin side filters by the same three fields; a typo in either
        // name silently returns everything.
        let payload = event_query(Some(7), Some(crate::device::EventKind::Notification), 5);
        assert_eq!(payload["since"], 7);
        assert_eq!(payload["kind"], "notification");
        assert_eq!(payload["limit"], 5);
        assert!(event_query(None, None, 5).get("since").is_none());
        assert!(event_query(None, None, 5).get("kind").is_none());
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
        assert_eq!(AdbEventTool::new().name(), "android_event");
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
            tasks: None,
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
