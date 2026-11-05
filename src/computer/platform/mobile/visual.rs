//! Finding a control the accessibility tree cannot name, and showing where it is.
//!
//! `android_ui_tree` names what the platform exposes. That covers buttons,
//! fields and list rows, and it covers nothing at all on a canvas, a game, a
//! custom-drawn toolbar, or an icon whose only label is a picture. `android_find`
//! already scrolls the *tree* looking for text; this asks the same question of
//! the screen itself, in the order that is cheapest and most precise first:
//!
//! 1. **The tree.** An exact label match, then a substring one. This is exact,
//!    free, and right whenever the platform describes the control.
//! 2. **OCR.** The screenshot's text, with the box each line was read in. This
//!    reaches text drawn into a canvas — the case the tree is silent about and
//!    the phone's own ML Kit engine (`android_ocr`) already solves.
//! 3. **A vision model.** Only when nothing above matched, and only through the
//!    provider the agent is configured with: a phone cannot run a UI-grounding
//!    model, so this is a remote call, one per miss. Where no provider is set
//!    the answer says so plainly instead of failing as if the control were not
//!    there.
//!
//! # Two screenshots, and what the second one proves
//!
//! The plan asks for a red box drawn on a second screenshot to confirm the
//! location, and that is what happens: the first capture happens *before* the
//! tree read and the second *after* a match, so the pair brackets everything
//! that was read. If the two pictures are not the same screen, it moved while it
//! was being read and the box would name whatever has since taken that place —
//! so nothing is returned as found. That refusal is the whole point of the
//! second capture; the red box itself is what makes the answer checkable by the
//! model (and by a person reading the transcript).
//!
//! "The same screen" is a 12×24 brightness grid, not an exact comparison: a
//! status-bar clock and a blinking cursor repaint between two captures of a
//! screen nobody touched, and an exact test would refuse every search. A grid
//! with the same tolerance `FrameSignature` uses still catches what matters — a
//! scroll, a dialog, an app switch — because those move whole cells.
//!
//! # The `@vN` handle
//!
//! A located control comes back as a handle (`@v1`, `@v2`, …) beside its
//! coordinates. The handle carries the screen print it was confirmed on, and
//! `android_input visual="@v1"` re-checks it before tapping: a screen that has
//! changed since the search is reported instead of being tapped at a place the
//! control has left. Handles live in this process and only the last few are
//! kept, so an expired one is a refusal rather than a guess — the caller
//! searches again, which is one call.
//!
//! # What is deliberately not here
//!
//! The tool does not press anything. A press has to go through the risk
//! classifier, the approval gate and the repetition ladder, all of which live in
//! `android_input` — and routing the handle back through that one tool is what
//! keeps a visual tap as governed as a tree tap.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::android::{
    capture_png, dump_ui, element_label, png_dimensions, resolve_device, resolved_serial, Target,
};
use super::wheel::normalize;
use crate::providers::{CompletionRequest, Message};
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

// ── Error codes ────────────────────────────────────────────────────────────

/// Nothing the tree, OCR or the vision model could see matches the target.
pub const VISUAL_NOT_FOUND: &str = "VISUAL_NOT_FOUND";
/// Several controls match equally well, so no one of them is the answer.
pub const VISUAL_AMBIGUOUS: &str = "VISUAL_AMBIGUOUS";
/// The screen changed between the read and its confirmation.
pub const VISUAL_MOVED: &str = "VISUAL_MOVED";
/// A handle whose screen is no longer what it was when it was issued.
pub const VISUAL_STALE: &str = "VISUAL_STALE";
/// The handle is not one this process issued, or it has expired.
pub const VISUAL_UNKNOWN: &str = "VISUAL_UNKNOWN";
/// No vision provider is configured, and the icon has no text to read.
pub const VISUAL_NO_PROVIDER: &str = "VISUAL_NO_PROVIDER";
/// The vision provider answered with an error, or with something unusable.
pub const VISUAL_PROVIDER_FAILED: &str = "VISUAL_PROVIDER_FAILED";
/// The screenshot could not be taken, read or drawn on.
pub const VISUAL_UNAVAILABLE: &str = "VISUAL_UNAVAILABLE";

/// How long the remote vision call may take.
///
/// One screenshot and one short answer; past this the turn is better served by
/// telling the model the icon route did not answer than by waiting.
const VISION_TIMEOUT: Duration = Duration::from_secs(25);

/// How much the vision model is allowed to write: one JSON object.
const VISION_MAX_TOKENS: u32 = 400;

/// How many located controls are remembered for later taps.
///
/// A model searches, then taps; the gap is a turn or two. Eight is more slack
/// than that and still bounded, so a runaway loop cannot grow the list.
const REGISTRY_CAPACITY: usize = 8;

// ── What was found, and how well ───────────────────────────────────────────

/// Where a match came from. Reported because the three fail differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The accessibility tree named it.
    Tree,
    /// OCR read its text off the screenshot.
    Ocr,
    /// The vision model boxed it in the picture.
    Vision,
}

impl Source {
    /// The word the result carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Ocr => "ocr",
            Self::Vision => "vision",
        }
    }
}

/// How closely a label matches the search.
///
/// Ordered so `Exact` beats `Partial`: a screen with "Send" and "Send later"
/// answers the search for "Send" with the first, not with both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Quality {
    Partial,
    Exact,
}

/// A rectangle in device pixels, `(x1, y1, x2, y2)`, the same space as
/// [`super::android::UiElement::bounds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BBox {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

impl BBox {
    /// A rectangle, or `None` when it has no area.
    pub fn new(x1: i32, y1: i32, x2: i32, y2: i32) -> Option<Self> {
        (x2 > x1 && y2 > y1).then_some(Self { x1, y1, x2, y2 })
    }

    /// The point a tap should go to.
    pub fn center(&self) -> (i32, i32) {
        ((self.x1 + self.x2) / 2, (self.y1 + self.y2) / 2)
    }

    /// Whether the rectangle lies inside an image of this size.
    ///
    /// This is not a tidiness check. A tree's bounds and a screenshot's pixels
    /// come from two different reads, and when they disagree — a rotation, a
    /// scaled capture, a virtual display — every coordinate the tree gives is
    /// wrong for the picture the model would be shown a box on. A box that
    /// falls outside the image is that disagreement, and drawing it clipped
    /// would present a wrong location as a confirmed one.
    pub fn inside(&self, width: u32, height: u32) -> bool {
        let (w, h) = (width as i32, height as i32);
        self.x1 >= 0 && self.y1 >= 0 && self.x2 <= w && self.y2 <= h
    }
}

/// One thing that matched, before it is known whether it is *the* answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub quality: Quality,
    /// The label as it was read.
    pub label: String,
    pub bounds: BBox,
    /// A tree candidate is pressable; an OCR line is only text on a picture.
    pub clickable: bool,
}

/// The outcome of one matching pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// Exactly one thing matches; this is it.
    Unique(Candidate),
    /// Several match equally well, so none of them is the answer.
    Ambiguous(Vec<Candidate>),
    /// Nothing matched.
    None,
}

/// The needle a search matches against, and the description it is known by.
///
/// `text` and `target` are different questions that are usually answered by the
/// same string. The plan's signature is `{ target, text? }`: `target` says what
/// the control *is* — what a person would call it, and what the vision model is
/// asked to box — while `text` is the literal on-screen text, for the caller
/// that already knows it. Without `text` the two are the same string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    /// The literal to look for in a label or an OCR line.
    pub needle: String,
    /// The control described in words, for the vision prompt and the result.
    pub description: String,
}

impl Search {
    /// Read the arguments' `target` and `text?`.
    pub fn from_args(args: &Value) -> Result<Self, String> {
        let target = args
            .get("target")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "'target' must describe the control you are looking for".to_string())?;
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        Ok(Self {
            needle: text.unwrap_or(target).to_string(),
            description: target.to_string(),
        })
    }

    /// How well `label` matches, if at all.
    ///
    /// Whitespace and case are folded away by [`normalize`] — OCR of a button
    /// reads `Sign  in` as often as `Sign in`, and a Hangul or CJK label is
    /// spaced by the layout rather than the word. Beyond that, an exact hit
    /// beats a substring one, and the two are kept apart so the caller's answer
    /// is the closest match on the screen rather than the first one.
    pub fn quality(&self, label: &str) -> Option<Quality> {
        let label = normalize(label);
        if label.is_empty() {
            return None;
        }
        let needle = normalize(&self.needle);
        if needle.is_empty() {
            return None;
        }
        if label == needle {
            Some(Quality::Exact)
        } else if label.contains(&needle) {
            Some(Quality::Partial)
        } else {
            None
        }
    }
}

/// Pick the answer out of what matched.
///
/// Two things decide it, in this order:
///
/// * the best quality tier that matched at all — an exact hit is never
///   reported as ambiguous beside a substring one;
/// * among equals, a pressable element beats an unpressed label. A card and the
///   text inside it often carry the same label, and only one of them is what a
///   person would press.
///
/// Anything still tied is refused. Handing back the first of several equal
/// matches is how a model ends up tapping the wrong row and then explaining a
/// screen it never asked for.
pub fn pick(mut candidates: Vec<Candidate>) -> Finding {
    if candidates.is_empty() {
        return Finding::None;
    }
    let best = candidates
        .iter()
        .map(|candidate| candidate.quality)
        .max()
        .unwrap_or(Quality::Partial);
    candidates.retain(|candidate| candidate.quality == best);
    if candidates.len() == 1 {
        return Finding::Unique(candidates.remove(0));
    }
    let mut pressable: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| candidate.clickable)
        .cloned()
        .collect();
    if pressable.len() == 1 {
        return Finding::Unique(pressable.remove(0));
    }
    if pressable.len() > 1 {
        return Finding::Ambiguous(pressable);
    }
    Finding::Ambiguous(candidates)
}

/// Match the search against a tree read.
pub fn match_elements(elements: &[super::android::UiElement], search: &Search) -> Finding {
    let candidates = elements
        .iter()
        .filter_map(|element| {
            let label = element_label(element)?;
            let quality = search.quality(label)?;
            Some(Candidate {
                quality,
                label: label.trim().to_string(),
                bounds: BBox::new(
                    element.bounds.0,
                    element.bounds.1,
                    element.bounds.2,
                    element.bounds.3,
                )?,
                clickable: element.clickable,
            })
        })
        .collect();
    pick(candidates)
}

/// Match the search against OCR lines.
pub fn match_lines(lines: &[super::android::OcrLine], search: &Search) -> Finding {
    let candidates = lines
        .iter()
        .filter_map(|line| {
            let quality = search.quality(&line.text)?;
            Some(Candidate {
                quality,
                label: line.text.trim().to_string(),
                bounds: BBox::new(line.bounds.0, line.bounds.1, line.bounds.2, line.bounds.3)?,
                // A line of recognised text is a picture of a word, not a
                // control: nothing here says where its button ends.
                clickable: false,
            })
        })
        .collect();
    pick(candidates)
}

// ── The vision pass ────────────────────────────────────────────────────────

/// What the vision model is asked, and in what shape to answer.
///
/// The answer is one JSON object because the alternative — prose about "upper
/// right" — has to be parsed by something, and a wrong guess at prose is a tap
/// on the wrong place. The prompt states the coordinate space explicitly
/// because the model cannot ask.
const VISION_PROMPT: &str = "\
You locate one control in a screenshot of an Android phone.

Answer with one JSON object and nothing else:
{\"found\": true, \"bbox\": {\"x1\": 0, \"y1\": 0, \"x2\": 0, \"y2\": 0}, \"label\": \"...\"}
or
{\"found\": false}

The box is the control's whole visible extent, including its icon and padding,
in device pixels of the image with the origin at its top-left corner.
\"label\" is the text you can read on the control, or a few words naming it.
Say found:false rather than guessing when the control is not on screen.";

/// A location the vision model reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisionHit {
    pub bounds: BBox,
    pub label: String,
}

/// Parse the provider's answer.
///
/// The object is found between the first `{` and the last `}` in what came
/// back: models wrap JSON in prose or a fenced block often enough that
/// requiring a bare object would refuse the common case. Every field is
/// checked, and an answer that cannot be trusted is an error rather than a
/// guess — this box decides where a finger lands.
pub fn parse_vision_answer(content: &str) -> Result<Option<VisionHit>, String> {
    let start = content.find('{');
    let end = content.rfind('}');
    let (Some(start), Some(end)) = (start, end) else {
        return Err(format!(
            "{VISUAL_PROVIDER_FAILED}: the vision model answered without a JSON object"
        ));
    };
    if end <= start {
        return Err(format!(
            "{VISUAL_PROVIDER_FAILED}: the vision model answered with an unusable object"
        ));
    }
    let parsed: Value = serde_json::from_str(&content[start..=end]).map_err(|error| {
        format!("{VISUAL_PROVIDER_FAILED}: the vision model's answer is not JSON ({error})")
    })?;
    if !parsed
        .get("found")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let bbox = parsed.get("bbox").ok_or_else(|| {
        format!("{VISUAL_PROVIDER_FAILED}: the vision model found the control but gave no box")
    })?;
    let coordinate = |key: &str| -> Result<i32, String> {
        bbox.get(key)
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| {
                format!("{VISUAL_PROVIDER_FAILED}: the vision model's box has no readable {key:?}")
            })
    };
    let bounds = BBox::new(
        coordinate("x1")?,
        coordinate("y1")?,
        coordinate("x2")?,
        coordinate("y2")?,
    )
    .ok_or_else(|| {
        format!("{VISUAL_PROVIDER_FAILED}: the vision model's box has no area — nothing to press")
    })?;
    let label = parsed
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    Ok(Some(VisionHit { bounds, label }))
}

/// Ask the configured vision provider where `description` is in `png`.
async fn ask_vision(
    context: &ToolContext,
    png: &[u8],
    search: &Search,
) -> Result<Option<VisionHit>, String> {
    let Some(provider) = context.media_provider().cloned() else {
        return Err(format!(
            "{VISUAL_NO_PROVIDER}: the accessibility tree and OCR both matched nothing, and no \
             provider is configured to look at the picture for an icon that carries no text"
        ));
    };
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    let question = format!("Locate this control on the screenshot: {}", search.description);
    let request = CompletionRequest {
        messages: vec![
            Message::system(VISION_PROMPT),
            Message::user(question).with_image(encoded, "image/png"),
        ],
        // Same question, same answer: this decides where a finger lands.
        temperature: Some(0.0),
        max_tokens: Some(VISION_MAX_TOKENS),
        stream: false,
        requires_vision: true,
        ..Default::default()
    };
    let response = match tokio::time::timeout(VISION_TIMEOUT, provider.complete(request)).await {
        Err(_) => {
            return Err(format!(
                "{VISUAL_PROVIDER_FAILED}: the vision model did not answer within {} s",
                VISION_TIMEOUT.as_secs()
            ))
        }
        Ok(Err(error)) => {
            return Err(format!(
                "{VISUAL_PROVIDER_FAILED}: the vision model could not be asked ({error})"
            ))
        }
        Ok(Ok(response)) => response,
    };
    parse_vision_answer(&response.message.content)
}

// ── Screenshots: what one says, and what drawing on it proves ──────────────

/// A cheap fingerprint of what the screen looked like: a 12×24 brightness grid.
///
/// The same grid `FrameSignature` already uses to answer "did my tap change
/// anything", and for the same reason: a phone is never perfectly still. A
/// clock in the status bar, a blinking cursor and an animated ad all repaint
/// between two captures of a screen nobody touched, so an exact comparison
/// would report every search as "the screen moved" and refuse the answer even
/// when the control has not twitched.
///
/// What this catches is the case that matters: a list scrolled, a dialog
/// appeared, an app came to the front. Those move whole cells, not a hairline,
/// and [`crate::computer::platform::mobile::display::SIGNATURE_MIN_CELLS`] is
/// where "a hairline" stops counting as a change.
///
/// It is derived from the decoded pixels and is stable across encoders, so a
/// PNG re-encoded byte-for-byte differently still produces the same screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenPrint {
    /// Luma per cell, row-major, `columns * rows`.
    pub cells: Vec<u8>,
    pub columns: u32,
    pub rows: u32,
}

impl ScreenPrint {
    /// Whether these two describe materially different screens.
    pub fn differs(&self, other: &Self) -> bool {
        if self.columns != other.columns
            || self.rows != other.rows
            || self.cells.len() != other.cells.len()
        {
            // A different shape is not a comparison: say "different" rather
            // than pretending a grid of another size matched.
            return true;
        }
        let changed = self
            .cells
            .iter()
            .zip(&other.cells)
            .filter(|(before, after)| {
                before.abs_diff(**after) > super::display::SIGNATURE_CELL_DELTA
            })
            .count();
        changed >= super::display::SIGNATURE_MIN_CELLS
    }
}

/// The grid, plus how big the picture it came from is.
pub fn screen_print_blocking(png: &[u8]) -> Result<(ScreenPrint, (u32, u32)), String> {
    let pixmap = resvg::tiny_skia::Pixmap::decode_png(png).map_err(|error| {
        format!("{VISUAL_UNAVAILABLE}: the screenshot is not a PNG this build can read ({error})")
    })?;
    let (width, height) = (pixmap.width(), pixmap.height());
    // The grid the rest of the toolset computes from a frame. `Pixmap` stores
    // premultiplied RGBA in the same byte order `luma_grid_from_rgba` reads, and
    // a screenshot is fully opaque, so the two grids mean the same thing.
    let grid =
        super::android::luma_grid_from_rgba(pixmap.data(), width, height).ok_or_else(|| {
            format!(
                "{VISUAL_UNAVAILABLE}: a {width}x{height} screenshot is too small to compare \
                 against a later one"
            )
        })?;
    Ok((
        ScreenPrint {
            cells: grid.cells,
            columns: grid.columns,
            rows: grid.rows,
        },
        (width, height),
    ))
}

/// [`screen_print_blocking`], on the blocking pool.
pub async fn screen_print(png: &[u8]) -> Result<(ScreenPrint, (u32, u32)), String> {
    let png = png.to_vec();
    tokio::task::spawn_blocking(move || screen_print_blocking(&png))
        .await
        .map_err(|error| {
            format!("{VISUAL_UNAVAILABLE}: the screenshot could not be read ({error})")
        })?
}

/// Decode, fingerprint, and draw a red box around `bounds` in one pass.
///
/// Returns the fingerprint of the *unmarked* picture beside the annotated one:
/// the marker is drawn after the comparison is taken, so the picture the model
/// sees cannot differ from the screen the search matched on by the box itself.
pub fn annotate_blocking(png: &[u8], bounds: BBox) -> Result<(ScreenPrint, Vec<u8>), String> {
    let mut pixmap = resvg::tiny_skia::Pixmap::decode_png(png).map_err(|error| {
        format!("{VISUAL_UNAVAILABLE}: the screenshot is not a PNG this build can read ({error})")
    })?;
    let (width, height) = (pixmap.width(), pixmap.height());
    let grid =
        super::android::luma_grid_from_rgba(pixmap.data(), width, height).ok_or_else(|| {
            format!(
                "{VISUAL_UNAVAILABLE}: a {width}x{height} screenshot is too small to compare \
                 against a later one"
            )
        })?;
    if !bounds.inside(width, height) {
        return Err(format!(
            "{VISUAL_UNAVAILABLE}: the box ({}, {})-({}, {}) is not inside the {}x{} screenshot — \
             the rectangle and the picture describe different screens, so no location can be \
             confirmed from them",
            bounds.x1, bounds.y1, bounds.x2, bounds.y2, width, height
        ));
    }

    // A fixed fraction of the screen is too thin on a tablet and too thick on a
    // small phone; a fraction of the shorter side, bounded both ways, is
    // visible everywhere without covering what it points at.
    let thickness = (width.min(height) as i32 / 150).clamp(3, 12);
    let mut paint = resvg::tiny_skia::Paint::default();
    paint.set_color_rgba8(230, 30, 30, 235);
    paint.anti_alias = false;

    // Four edges rather than a stroke: `fill_rect` needs no path, and an
    // un-anti-aliased hairline is exactly what a marker box should be.
    let edges = [
        (bounds.x1, bounds.y1, bounds.x2, bounds.y1 + thickness),
        (bounds.x1, bounds.y2 - thickness, bounds.x2, bounds.y2),
        (bounds.x1, bounds.y1, bounds.x1 + thickness, bounds.y2),
        (bounds.x2 - thickness, bounds.y1, bounds.x2, bounds.y2),
    ];
    for (x1, y1, x2, y2) in edges {
        // A box thinner than the line is still drawn once across its extent
        // rather than skipping an edge: a partial box reads as a partial answer.
        let (x2, y2) = (x2.max(x1 + 1), y2.max(y1 + 1));
        let rect = resvg::tiny_skia::Rect::from_ltrb(x1 as f32, y1 as f32, x2 as f32, y2 as f32)
            .ok_or_else(|| format!("{VISUAL_UNAVAILABLE}: the marker box could not be drawn"))?;
        pixmap.fill_rect(rect, &paint, resvg::tiny_skia::Transform::identity(), None);
    }

    let annotated = pixmap.encode_png().map_err(|error| {
        format!("{VISUAL_UNAVAILABLE}: the marked screenshot could not be encoded ({error})")
    })?;
    let print = ScreenPrint {
        cells: grid.cells,
        columns: grid.columns,
        rows: grid.rows,
    };
    Ok((print, annotated))
}

/// [`annotate_blocking`], on the blocking pool.
pub async fn annotate(png: &[u8], bounds: BBox) -> Result<(ScreenPrint, Vec<u8>), String> {
    let png = png.to_vec();
    tokio::task::spawn_blocking(move || annotate_blocking(&png, bounds))
        .await
        .map_err(|error| {
            format!("{VISUAL_UNAVAILABLE}: the screenshot could not be marked ({error})")
        })?
}

// ── Handles ────────────────────────────────────────────────────────────────

/// A control this process located, and the screen it was located on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualTarget {
    /// The `@vN` the caller quotes back.
    pub handle: String,
    /// The device it was found on; a handle is not a name for a control on
    /// another phone.
    pub device: Option<String>,
    /// The display id (`0` for the user's own screen).
    pub display: i32,
    pub center: (i32, i32),
    pub bounds: BBox,
    pub label: String,
    pub source: Source,
    /// What the screen looked like when the control was confirmed on it.
    pub screen: ScreenPrint,
    /// When it was issued, for a message that says how long ago.
    pub at: Instant,
}

/// The handles issued by this process.
///
/// A struct rather than a bare global so the rules above it can be tested
/// without a static that every test shares.
#[derive(Debug)]
pub struct VisualRegistry {
    issued: u64,
    entries: std::collections::VecDeque<VisualTarget>,
}

impl Default for VisualRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl VisualRegistry {
    pub fn new() -> Self {
        Self {
            issued: 0,
            entries: std::collections::VecDeque::new(),
        }
    }

    /// Remember a located control and name it.
    pub fn register(&mut self, mut target: VisualTarget) -> String {
        self.issued += 1;
        let handle = format!("@v{}", self.issued);
        target.handle = handle.clone();
        self.entries.push_back(target);
        while self.entries.len() > REGISTRY_CAPACITY {
            self.entries.pop_front();
        }
        handle
    }

    /// The control a handle names, when it was found on this device and display.
    pub fn get(&self, handle: &str, device: Option<&str>, display: i32) -> Option<&VisualTarget> {
        self.entries.iter().rev().find(|target| {
            target.handle == handle
                && target.display == display
                && target.device.as_deref() == device
        })
    }

    /// How many handles are held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The process-wide registry.
///
/// A `std::sync::Mutex` that is never held across an `.await`: registration is
/// a push and a lookup is a scan, and both are finished before this returns.
fn registry() -> std::sync::MutexGuard<'static, VisualRegistry> {
    static REGISTRY: OnceLock<Mutex<VisualRegistry>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| Mutex::new(VisualRegistry::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Remember a located control, under a fresh handle.
fn remember(target: VisualTarget) -> String {
    registry().register(target)
}

/// Re-check a handle against the live screen and return where it points.
///
/// Called by `android_input` while it already holds the target lock, so it must
/// not take one itself.
///
/// The check is the screen print the handle carries: take the picture again and
/// ask whether this is materially the same screen. A screen that has moved on
/// means the handle names a place, not a control, and a tap there is exactly
/// the silent wrong-press this whole tool exists to prevent — so it is refused
/// and the caller searches again, which is one call.
///
/// The live screenshot is handed back with the answer: it is the frame the
/// press is about to start from, and decoding the same bytes twice would only
/// buy the same grid twice.
pub async fn resolve(
    handle: &str,
    device: &Option<String>,
    target: Target,
) -> Result<(VisualTarget, Vec<u8>), Refused> {
    let handle = handle.trim();
    let known = {
        let registry = registry();
        registry
            .get(handle, device.as_deref(), target.display_id())
            .cloned()
    };
    let Some(known) = known else {
        return Err(Refused {
            code: VISUAL_UNKNOWN,
            message: format!(
                "{VISUAL_UNKNOWN}: {handle} is not a handle this process issued for this device — \
                 it may have expired (only the last {REGISTRY_CAPACITY} are kept) or come from \
                 another screen. Locate the control again with android_find_visually."
            ),
        });
    };
    confirm(&known, device, target).await
}

/// Take the picture again and check that `known` still names something on it.
///
/// The same question [`resolve`] asks, asked again at a second point: a tap
/// may be held at an approval question for up to a minute, and the screen a
/// person approved is not guaranteed to be the screen the tap is dispatched on.
/// Called with the target lock already held, so it must not take one itself.
///
/// The live screenshot is handed back with the answer too: the caller may want
/// to judge the press's effect against the frame it started from, and decoding
/// it twice would only buy the same grid twice.
pub async fn confirm(
    known: &VisualTarget,
    device: &Option<String>,
    target: Target,
) -> Result<(VisualTarget, Vec<u8>), Refused> {
    let png = capture_png(device, target).await.map_err(|error| Refused {
        code: VISUAL_UNAVAILABLE,
        message: format!(
            "{VISUAL_UNAVAILABLE}: {} was found, but the screen could not be photographed \
             to confirm the control is still there ({error}). Nothing was done.",
            known.handle
        ),
    })?;
    let now = screen_print(&png).await.map_err(|message| Refused {
        code: VISUAL_UNAVAILABLE,
        message,
    })?;
    if now.0.differs(&known.screen) {
        // How long ago it was located tells the caller whether the phone moved
        // under a slow model or the handle is simply old.
        let age = known.at.elapsed().as_secs();
        return Err(Refused {
            code: VISUAL_STALE,
            message: format!(
                "{VISUAL_STALE}: the screen has changed since {} was located {} ago, so \
                 tapping that place would not necessarily hit {}. Locate it again with \
                 android_find_visually.",
                known.handle,
                if age < 1 {
                    "less than a second".to_string()
                } else {
                    format!("{age} s")
                },
                if known.label.is_empty() {
                    "the control"
                } else {
                    &known.label
                }
            ),
        });
    }
    Ok((known.clone(), png))
}

// ── Wording ────────────────────────────────────────────────────────────────

/// A candidate as the model reads it: the label and the box.
fn describe_candidate(candidate: &Candidate) -> String {
    format!(
        "\"{}\" box=({},{})-({},{})",
        candidate.label,
        candidate.bounds.x1,
        candidate.bounds.y1,
        candidate.bounds.x2,
        candidate.bounds.y2
    )
}

/// The list of equally good matches, so the caller can narrow the search.
///
/// `where_` names the pass that found them: the same refusal means different
/// things coming from the tree (the screen says it twice) and from OCR (the
/// picture reads the same word twice), and the caller's next move differs.
fn describe_ambiguous(where_: &str, candidates: &[Candidate], search: &Search) -> String {
    let mut text = format!(
        "{VISUAL_AMBIGUOUS}: {} control(s) in {where_} match {:?} equally well, so none of them \
         is the answer. Add `text` with the exact label one of them shows, or read the screen \
         with android_ui_tree and use an element index.\n",
        candidates.len(),
        search.needle
    );
    for candidate in candidates {
        text.push_str(&format!("{}\n", describe_candidate(candidate)));
    }
    text.trim_end().to_string()
}

/// A search that ended without a location, and the code that says which way.
///
/// One shape for every way out of the matching passes, so the tool's body has a
/// single refusal path instead of one `return` per branch — and so the code
/// beside the message cannot drift from the branch that produced it.
#[derive(Debug)]
pub struct Refused {
    pub code: &'static str,
    pub message: String,
}

impl Refused {
    /// Several things matched equally well, in `where_`.
    fn ambiguous(where_: &str, candidates: &[Candidate], search: &Search) -> Self {
        Self {
            code: VISUAL_AMBIGUOUS,
            message: describe_ambiguous(where_, candidates, search),
        }
    }

    /// Nothing matched.
    fn not_found(message: String) -> Self {
        Self {
            code: VISUAL_NOT_FOUND,
            message,
        }
    }
}

/// What the search ran, for the message that says it found nothing.
#[derive(Debug, Default)]
struct Attempts {
    elements: usize,
    ocr_lines: Option<usize>,
    ocr_failed: Option<String>,
    vision: Option<String>,
}

impl Attempts {
    fn describe(&self, search: &Search) -> String {
        let mut parts = vec![format!(
            "the accessibility tree names {} element(s) and none matches {:?}",
            self.elements, search.needle
        )];
        match (&self.ocr_lines, &self.ocr_failed) {
            (Some(count), _) => parts.push(format!(
                "OCR read {count} text line(s) from the screenshot and none matches"
            )),
            (None, Some(failure)) => {
                parts.push(format!("OCR could not read the screenshot ({failure})"))
            }
            (None, None) => {}
        }
        match &self.vision {
            Some(note) => parts.push(note.clone()),
            // Only reached when OCR failed: every path where the picture could
            // be read asks the vision model, which reports its own outcome.
            None => parts.push(
                "the vision model was not asked, because the screenshot it would have been \
                 shown could not be read first"
                    .to_string(),
            ),
        }
        format!("{VISUAL_NOT_FOUND}: not found — {}.", parts.join("; "))
    }
}

// ── The tool ───────────────────────────────────────────────────────────────

/// Find a control on screen by what it says or what it looks like.
#[derive(Debug)]
pub struct AdbFindVisuallyTool {
    device: Option<String>,
}

impl Default for AdbFindVisuallyTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbFindVisuallyTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbFindVisuallyTool {
    fn name(&self) -> &str {
        "android_find_visually"
    }

    fn description(&self) -> &str {
        "Find one control on screen by its text, and return the box it occupies with a marked \
         screenshot to check it against, then tap it with android_input visual=\"@vN\". Use it \
         when android_ui_tree does not name what you are looking for — a canvas, a game, a \
         custom-drawn toolbar — or when the text is drawn rather than exposed as a node. Three \
         passes, cheapest first: the accessibility tree (exact label, then substring), then OCR \
         of the screenshot, then, only when both miss, the vision provider for an icon that \
         carries no text at all. It answers with where the control is, not by pressing it: the \
         tap goes through android_input so the approval and repetition checks still apply. Says \
         plainly when a control matches several things, when the screen moved while it was being \
         read, and when no vision provider is configured for the icon pass."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Find a control by text or appearance",
            serde_json::json!({
                "target": {
                    "type": "string",
                    "description": "What you are looking for, in words: a label such as \"Sign in\", or a description such as \"the settings gear icon\". This is a *description*, not an element index (android_input's `target` is the index)."
                },
                "text": {
                    "type": "string",
                    "description": "Optional: the exact text the control shows, when you know it. It is what the tree and OCR are matched against; `target` is then only the description. Without it, `target` is used for both."
                },
                "display": { "type": "integer", "description": super::android::DISPLAY_PARAM },
                "device": { "type": "string", "description": super::android::DEVICE_PARAM }
            }),
            vec!["target"],
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
        let search = match Search::from_args(&args) {
            Ok(search) => search,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };

        // One reader at a time, for the whole search: the two screenshots and
        // the tree read have to describe the same moment, and another tool's
        // tap landing between them is exactly what the confirmation would then
        // report as a moved screen.
        //
        // The lease lasts through the vision call, which is the one part of
        // this tool that can take seconds — and a search that names an icon is
        // precisely the one a vision call is in the middle of when another
        // writer would otherwise press it.
        let _target = super::android::lock_target(&device, target).await;

        // The first capture brackets the search with the second one below.
        let mut before = match capture_png(&device, target).await {
            Ok(png) => png,
            Err(error) => return Ok(ToolExecutionResult::error(error.to_string())),
        };
        let mut size = png_dimensions(&before).and_then(|(width, height)| {
            // PNG dimensions are unsigned; the coordinate space the rest of the
            // toolset uses is signed. A screenshot past `i32::MAX` pixels is not
            // a phone, and is reported as an unknown size rather than wrapped.
            Some((i32::try_from(width).ok()?, i32::try_from(height).ok()?))
        });

        let mut dump = dump_ui(&device, target).await.ok();

        // The app lease is the same one `android_input` takes. A display lock
        // alone does not stop a write to the *same app* on another display, and
        // the two captures below would then bracket somebody else's edit and
        // report the screen as moved without saying why.
        let held = super::android::app_completed(
            &device,
            dump.as_ref().and_then(|dump| dump.package.as_deref()),
        );
        let lease = super::android::lease_app(
            &device,
            dump.as_ref().and_then(|dump| dump.package.as_deref()),
            held,
        )
        .await;
        if lease.stale {
            // Somebody wrote to this app while this call was waiting. The tree
            // and the picture in hand both describe a moment before that write,
            // so take both again — and stop rather than search a different app
            // than the one that was named.
            let package = dump.as_ref().and_then(|dump| dump.package.clone());
            match dump_ui(&device, target).await {
                Ok(fresh) if fresh.package == package => match capture_png(&device, target).await {
                    Ok(png) => {
                        size = png_dimensions(&png).and_then(|(width, height)| {
                            Some((i32::try_from(width).ok()?, i32::try_from(height).ok()?))
                        });
                        before = png;
                        dump = Some(fresh);
                    }
                    Err(error) => return Ok(ToolExecutionResult::error(error.to_string())),
                },
                Ok(fresh) => {
                    return Ok(ToolExecutionResult::error(format!(
                        "Refused: the screen changed app while this call waited for another \
                         writer — it was {:?}, it is now {:?}. Nothing was done. Read the screen \
                         again and retry.",
                        package.as_deref().unwrap_or("unknown"),
                        fresh.package.as_deref().unwrap_or("unknown"),
                    )))
                }
                Err(_) => {
                    return Ok(ToolExecutionResult::error(
                        "Refused: another writer changed this app while this call was waiting, \
                         and the screen could not be re-read afterwards. Nothing was done.",
                    ))
                }
            }
        }

        let elements = dump
            .as_ref()
            .map(|dump| dump.elements.as_slice())
            .unwrap_or(&[]);
        let package = dump.as_ref().and_then(|dump| dump.package.clone());

        let mut attempts = Attempts {
            elements: elements.len(),
            ..Attempts::default()
        };

        let located = match match_elements(elements, &search) {
            Finding::Unique(candidate) => Ok((Source::Tree, candidate)),
            Finding::Ambiguous(candidates) => {
                Err(Refused::ambiguous("the accessibility tree", &candidates, &search))
            }
            Finding::None => match super::android::ocr_png(&before).await {
                Ok(lines) => {
                    attempts.ocr_lines = Some(lines.len());
                    match match_lines(&lines, &search) {
                        Finding::Unique(candidate) => Ok((Source::Ocr, candidate)),
                        Finding::Ambiguous(candidates) => {
                            Err(Refused::ambiguous("the screenshot's text", &candidates, &search))
                        }
                        Finding::None => match ask_vision(context, &before, &search).await {
                            Ok(Some(hit)) => Ok((
                                Source::Vision,
                                Candidate {
                                    quality: Quality::Exact,
                                    label: hit.label,
                                    bounds: hit.bounds,
                                    // The model boxed a control, which is what
                                    // the box is for — unlike an OCR line,
                                    // which is only a picture of a word.
                                    clickable: true,
                                },
                            )),
                            Ok(None) => {
                                attempts.vision = Some(
                                    "the vision model was asked and found nothing".to_string(),
                                );
                                Err(Refused::not_found(attempts.describe(&search)))
                            }
                            Err(note) => {
                                let code = if note.starts_with(VISUAL_NO_PROVIDER) {
                                    attempts.vision = Some(note.clone());
                                    VISUAL_NO_PROVIDER
                                } else {
                                    attempts.vision = Some(note.clone());
                                    VISUAL_PROVIDER_FAILED
                                };
                                // A missing provider is a *degradation*, not a
                                // failure of the search: the answer says what
                                // was not looked at and what to do instead.
                                Err(Refused {
                                    code,
                                    message: format!("{}\n{}", attempts.describe(&search), note),
                                })
                            }
                        },
                    }
                }
                Err(failure) => {
                    attempts.ocr_failed = Some(failure);
                    Err(Refused::not_found(attempts.describe(&search)))
                }
            },
        };

        let (source, found) = match located {
            Ok(located) => located,
            Err(refused) => {
                return Ok(ToolExecutionResult::error(refused.message).with_data(json!({
                    "code": refused.code,
                    "device": resolved_serial(&device).await,
                })))
            }
        };

        // The confirmation. A second capture, and the same screen only if the
        // grid says so — which is what makes the box mean this moment rather
        // than the one the match was computed on.
        let after = match capture_png(&device, target).await {
            Ok(png) => png,
            Err(error) => return Ok(ToolExecutionResult::error(error.to_string())),
        };
        let (after_print, annotated) = match annotate(&after, found.bounds).await {
            Ok(pair) => pair,
            Err(refusal) => {
                return Ok(ToolExecutionResult::error(refusal).with_data(json!({
                    "code": VISUAL_UNAVAILABLE,
                    "device": resolved_serial(&device).await,
                })))
            }
        };
        let before_print = match screen_print(&before).await {
            Ok((print, _)) => print,
            Err(refusal) => {
                return Ok(ToolExecutionResult::error(refusal).with_data(json!({
                    "code": VISUAL_UNAVAILABLE,
                    "device": resolved_serial(&device).await,
                })))
            }
        };
        if before_print.differs(&after_print) {
            return Ok(ToolExecutionResult::error(format!(
                "{VISUAL_MOVED}: the screen changed while it was being read, so nothing was \
                 confirmed — the box would name whatever moved into that place. Search again when \
                 the screen has settled, or pass a more specific `text`."
            ))
            .with_data(json!({
                "code": VISUAL_MOVED,
                "device": resolved_serial(&device).await,
            })));
        }

        // The box is drawn but the picture reaches the model only as an
        // attachment: the marker line below is what the request-time
        // materializer turns into an image block, and a base64 payload in a
        // tool result is elided before it ever gets there.
        let mut output = format!(
            "Located {} from the {} at center=({},{}), box=({},{})-({},{}).",
            if found.label.is_empty() {
                format!("{:?}", search.description)
            } else {
                format!("{:?}", found.label)
            },
            match source {
                Source::Tree => "accessibility tree",
                Source::Ocr => "screenshot's text",
                Source::Vision => "vision model",
            },
            found.bounds.center().0,
            found.bounds.center().1,
            found.bounds.x1,
            found.bounds.y1,
            found.bounds.x2,
            found.bounds.y2,
        );
        if package.as_deref().is_some_and(super::takeover::is_own_app) {
            output.push_str(
                "\nThat is Syscity's own screen. It can be read but not operated: the app's \
                 settings and pairing pages are a person's controls, not the agent's.",
            );
        }
        output.push_str(
            "\nA second screenshot shows the same screen the match was read from, so the box \
             still names the same control.\n",
        );

        let mut screen_json = json!({ "columns": after_print.columns, "rows": after_print.rows });
        if let Some((width, height)) = size {
            screen_json["width"] = json!(width);
            screen_json["height"] = json!(height);
        }

        let handle = remember(VisualTarget {
            handle: String::new(),
            device: device.clone(),
            display: target.display_id(),
            center: found.bounds.center(),
            bounds: found.bounds,
            label: found.label.clone(),
            source,
            screen: after_print,
            at: Instant::now(),
        });

        let attached = match crate::attachments::store_bytes_async(annotated, "image/png").await {
            Ok(reference) => {
                output.push_str(
                    "The marked screenshot is attached; the box is drawn round the control it \
                     points at.\n",
                );
                output.push_str(&crate::attachments::render_ref_line(&reference));
                output.push('\n');
                Some(reference)
            }
            Err(error) => {
                // Fail-open, like every other screenshot path: the location is
                // still correct without the picture, and losing the marker
                // must not lose the answer.
                tracing::warn!(
                    "attachment store write failed ({error}); the marked screenshot is lost"
                );
                output.push_str(
                    "The marked screenshot could not be stored, so only the box above is left \
                     to check it against.\n",
                );
                None
            }
        };

        output.push_str(&format!(
            "Tap it with android_input `visual=\"{handle}\"`, which re-checks that this screen is \
             still the one the box was drawn on. Raw coordinates are x={} y={}.",
            found.bounds.center().0,
            found.bounds.center().1
        ));

        let point =
            super::coords::from_pixels(found.bounds.center().0, found.bounds.center().1, size);
        Ok(ToolExecutionResult::success(output).with_data(json!({
            "found": {
                "label": found.label,
                "source": source.as_str(),
                "bounds": {
                    "x1": found.bounds.x1,
                    "y1": found.bounds.y1,
                    "x2": found.bounds.x2,
                    "y2": found.bounds.y2,
                },
                "center": point.json(),
            },
            "visual": handle,
            "screen": screen_json,
            "screenshot_ref": attached.map(|reference| reference.to_json()),
            "package": package,
            "device": resolved_serial(&device).await,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::platform::mobile::android::{OcrLine, UiElement};

    fn element(label: &str, clickable: bool, bounds: (i32, i32, i32, i32)) -> UiElement {
        UiElement::new(
            1,
            Some(label.to_string()),
            None,
            None,
            "Button".to_string(),
            clickable,
            true,
            bounds,
        )
    }

    fn search(target: &str, text: Option<&str>) -> Search {
        let mut args = json!({ "target": target });
        if let Some(text) = text {
            args["text"] = json!(text);
        }
        Search::from_args(&args).expect("a search")
    }

    fn line(text: &str, bounds: (i32, i32, i32, i32)) -> OcrLine {
        OcrLine {
            text: text.to_string(),
            center: ((bounds.0 + bounds.2) / 2, (bounds.1 + bounds.3) / 2),
            bounds,
            confidence: 0.9,
        }
    }

    #[test]
    fn a_search_needs_a_target_and_uses_it_for_text_when_none_is_given() {
        assert!(Search::from_args(&json!({})).is_err());
        assert!(Search::from_args(&json!({ "target": "   " })).is_err());
        let plain = search("Sign in", None);
        assert_eq!(plain.needle, "Sign in");
        assert_eq!(plain.description, "Sign in");
        // With `text` the description and the literal are two different facts:
        // the icon is called one thing and carries another.
        let split = search("the settings gear", Some("Settings"));
        assert_eq!(split.needle, "Settings");
        assert_eq!(split.description, "the settings gear");
    }

    #[test]
    fn matching_folds_spacing_and_case_the_way_ocr_reads_them() {
        let search = search("Sign in", None);
        assert_eq!(search.quality("Sign in"), Some(Quality::Exact));
        assert_eq!(search.quality("  sign   IN "), Some(Quality::Exact));
        assert_eq!(search.quality("Sign in to continue"), Some(Quality::Partial));
        assert_eq!(search.quality("Cancel"), None);
        assert_eq!(search.quality(""), None);
    }

    #[test]
    fn an_exact_hit_beats_a_substring_one() {
        let elements = [
            element("Send later", true, (0, 0, 10, 10)),
            element("Send", true, (0, 20, 10, 30)),
        ];
        match match_elements(&elements, &search("Send", None)) {
            Finding::Unique(found) => {
                assert_eq!(found.label, "Send");
                assert_eq!(found.quality, Quality::Exact);
            }
            other => panic!("expected the exact hit, got {other:?}"),
        }
    }

    #[test]
    fn a_card_and_its_label_are_one_answer_when_only_one_is_pressable() {
        // A row that carries "Settings" and the text inside it carry the same
        // label; a person presses the row.
        let elements = [
            element("Settings", true, (0, 0, 100, 100)),
            element("Settings", false, (10, 10, 90, 40)),
        ];
        match match_elements(&elements, &search("Settings", None)) {
            Finding::Unique(found) => assert!(found.clickable, "{found:?}"),
            other => panic!("expected the pressable hit, got {other:?}"),
        }
    }

    #[test]
    fn two_equally_pressable_rows_are_refused_rather_than_guessed() {
        let elements = [
            element("Delete", true, (0, 0, 10, 10)),
            element("Delete", true, (0, 20, 10, 30)),
        ];
        match match_elements(&elements, &search("Delete", None)) {
            Finding::Ambiguous(candidates) => {
                assert_eq!(candidates.len(), 2);
                let said = describe_ambiguous(
                    "the accessibility tree",
                    &candidates,
                    &search("Delete", None),
                );
                assert!(said.contains(VISUAL_AMBIGUOUS), "{said}");
                assert!(said.contains("box=(0,0)-(10,10)"), "{said}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_tree_finds_nothing_rather_than_erroring() {
        assert_eq!(match_elements(&[], &search("anything", None)), Finding::None);
        assert_eq!(match_lines(&[], &search("anything", None)), Finding::None);
    }

    #[test]
    fn ocr_lines_match_like_tree_labels() {
        let lines = [
            line("Continue", (100, 200, 300, 240)),
            line("Continue with email", (100, 260, 400, 300)),
        ];
        match match_lines(&lines, &search("continue", None)) {
            Finding::Unique(found) => {
                assert_eq!(found.label, "Continue");
                assert_eq!(
                    found.bounds,
                    BBox {
                        x1: 100,
                        y1: 200,
                        x2: 300,
                        y2: 240
                    }
                );
                // A line of text is not a control: nothing says where its button
                // ends, so it never wins the "pressable" tie-break.
                assert!(!found.clickable);
            }
            other => panic!("expected the exact line, got {other:?}"),
        }
    }

    #[test]
    fn a_line_with_no_area_cannot_be_a_target() {
        let lines = [line("Fold", (10, 10, 10, 10))];
        assert_eq!(match_lines(&lines, &search("Fold", None)), Finding::None);
    }

    #[test]
    fn a_box_needs_an_area_and_knows_its_middle() {
        assert_eq!(BBox::new(10, 10, 10, 20), None);
        let bounds = BBox::new(100, 200, 300, 260).expect("a box");
        assert_eq!(bounds.center(), (200, 230));
        assert!(bounds.inside(1080, 2400));
        assert!(!bounds.inside(200, 2400));
        assert!(!BBox { x1: -1, y1: 0, x2: 10, y2: 10 }.inside(1080, 2400));
    }

    #[test]
    fn the_vision_answer_is_read_from_inside_whatever_the_model_wrapped_it_in() {
        let hit = parse_vision_answer(
            "Here it is:\n```json\n{\"found\": true, \"bbox\": {\"x1\": 10, \"y1\": 20, \"x2\": \
             110, \"y2\": 120}, \"label\": \"Settings\"}\n```",
        )
        .expect("a parseable answer")
        .expect("a hit");
        assert_eq!(
            hit.bounds,
            BBox {
                x1: 10,
                y1: 20,
                x2: 110,
                y2: 120
            }
        );
        assert_eq!(hit.label, "Settings");
        assert_eq!(hit.bounds.center(), (60, 70));
    }

    #[test]
    fn a_miss_says_which_passes_ran_and_which_did_not() {
        let search = search("the settings gear", None);
        // The tree and OCR both read, and the model was asked: every pass has
        // an outcome, so the answer is "it is not on this screen", not "the
        // tool does not know how to look".
        let all = Attempts {
            elements: 14,
            ocr_lines: Some(6),
            ocr_failed: None,
            vision: Some("the vision model was asked and found nothing".to_string()),
        };
        let said = all.describe(&search);
        assert!(said.starts_with(VISUAL_NOT_FOUND), "{said}");
        assert!(said.contains("14 element(s)"), "{said}");
        assert!(said.contains("OCR read 6 text line(s)"), "{said}");
        assert!(said.contains("the vision model was asked"), "{said}");

        // The picture could not be read, so the model never saw anything: the
        // answer must not claim it looked at the icon and missed.
        let blind = Attempts {
            elements: 14,
            ocr_lines: None,
            ocr_failed: Some("ML Kit unavailable".to_string()),
            vision: None,
        };
        let said = blind.describe(&search);
        assert!(said.contains("OCR could not read the screenshot"), "{said}");
        assert!(said.contains("was not asked"), "{said}");
        assert!(!said.contains("found nothing"), "{said}");
    }

    #[test]
    fn a_vision_answer_of_not_found_is_a_miss_not_a_failure() {
        assert_eq!(parse_vision_answer("{\"found\": false}"), Ok(None));
        assert_eq!(
            parse_vision_answer("{\"found\": true}"),
            Err(format!(
                "{VISUAL_PROVIDER_FAILED}: the vision model found the control but gave no box"
            ))
        );
    }

    #[test]
    fn a_vision_answer_that_cannot_be_trusted_is_refused() {
        for answer in [
            "I could not find it.",
            "{\"found\": true, \"bbox\": {\"x1\": 10, \"y1\": 20, \"x2\": 10, \"y2\": 120}}",
            "{\"found\": true, \"bbox\": {\"x1\": 10, \"y1\": 20}}",
            "{\"found\": true, \"bbox\": \"top right\"}",
        ] {
            let refused = parse_vision_answer(answer).expect_err(answer);
            assert!(refused.starts_with(VISUAL_PROVIDER_FAILED), "{refused}");
        }
    }

    /// A solid-colour PNG, so the drawing can be checked without a phone.
    fn solid_png(width: u32, height: u32, rgba: (u8, u8, u8, u8)) -> Vec<u8> {
        let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height).expect("a pixmap");
        let mut paint = resvg::tiny_skia::Paint::default();
        paint.set_color_rgba8(rgba.0, rgba.1, rgba.2, rgba.3);
        let rect = resvg::tiny_skia::Rect::from_ltrb(0.0, 0.0, width as f32, height as f32)
            .expect("the whole image");
        pixmap.fill_rect(rect, &paint, resvg::tiny_skia::Transform::identity(), None);
        pixmap.encode_png().expect("a PNG")
    }

    #[test]
    fn the_marker_box_is_drawn_on_the_edges_and_not_over_what_it_points_at() {
        let png = solid_png(200, 100, (255, 255, 255, 255));
        let bounds = BBox::new(50, 20, 150, 80).expect("a box");
        let (_, marked) = annotate_blocking(&png, bounds).expect("an annotated image");
        let pixmap = resvg::tiny_skia::Pixmap::decode_png(&marked).expect("the marked PNG");

        let pixel = |x: u32, y: u32| -> (u8, u8, u8) {
            let at = ((y as usize) * (pixmap.width() as usize) + x as usize) * 4;
            let data = pixmap.data();
            (data[at], data[at + 1], data[at + 2])
        };
        // On the edge: red.
        let (red, green, _) = pixel(50, 20);
        assert!(red > 150 && green < 120, "the edge is not marked: {:?}", pixel(50, 20));
        // Inside the box: untouched, so the control stays readable.
        assert_eq!(pixel(100, 50), (255, 255, 255));
        // Outside it: untouched too.
        assert_eq!(pixel(5, 5), (255, 255, 255));
    }

    #[test]
    fn a_box_outside_the_picture_is_refused_instead_of_clipped() {
        let png = solid_png(100, 100, (255, 255, 255, 255));
        let refused = annotate_blocking(
            &png,
            BBox {
                x1: 50,
                y1: 50,
                x2: 200,
                y2: 200,
            },
        )
        .expect_err("a box outside the image");
        assert!(refused.starts_with(VISUAL_UNAVAILABLE), "{refused}");
        assert!(refused.contains("different screens"), "{refused}");
    }

    #[test]
    fn the_screen_print_is_about_what_is_drawn_not_how_it_was_encoded() {
        let first = solid_png(40, 30, (10, 20, 30, 255));
        let second = solid_png(40, 30, (10, 20, 30, 255));
        // One luma step: a repaint the grid is deliberately blind to, like a
        // clock or a cursor. It must not be called a moved screen.
        let twitch = solid_png(40, 30, (10, 20, 31, 255));
        let (a, size) = screen_print_blocking(&first).expect("a print");
        let (b, _) = screen_print_blocking(&second).expect("a print");
        let (c, _) = screen_print_blocking(&twitch).expect("a print");
        assert_eq!(size, (40, 30));
        assert!(!a.differs(&b), "the same picture compared differently");
        assert!(!a.differs(&c), "a one-step repaint was called a moved screen");
    }

    #[test]
    fn a_screen_that_scrolled_is_a_moved_screen() {
        // Two horizontal bands of dark content, then the same picture with the
        // bands shifted down — a list that scrolled by a row.
        let band = |top: u32| -> Vec<u8> {
            let mut pixmap =
                resvg::tiny_skia::Pixmap::decode_png(&solid_png(120, 90, (255, 255, 255, 255)))
                    .expect("a pixmap");
            let data = pixmap.data_mut();
            for y in top..(top + 30) {
                for x in 0..120u32 {
                    let at = ((y as usize) * 120 + x as usize) * 4;
                    data[at] = 20;
                    data[at + 1] = 20;
                    data[at + 2] = 20;
                }
            }
            pixmap.encode_png().expect("a PNG")
        };
        let (first, _) = screen_print_blocking(&band(10)).expect("a print");
        let (same, _) = screen_print_blocking(&band(10)).expect("a print");
        let (moved, _) = screen_print_blocking(&band(50)).expect("a print");
        assert!(!first.differs(&same));
        assert!(first.differs(&moved), "a scrolled list was called the same screen");
        // A grid of another shape is not a comparison at all.
        let (other_size, _) =
            screen_print_blocking(&solid_png(60, 45, (255, 255, 255, 255))).expect("a print");
        assert!(first.differs(&other_size));
    }

    #[test]
    fn a_screenshot_this_build_cannot_read_says_so() {
        let refused = screen_print_blocking(b"not a png").expect_err("not a PNG");
        assert!(refused.starts_with(VISUAL_UNAVAILABLE), "{refused}");
        let refused = annotate_blocking(b"not a png", BBox::new(0, 0, 1, 1).expect("a box"))
            .expect_err("not a PNG");
        assert!(refused.starts_with(VISUAL_UNAVAILABLE), "{refused}");
    }

    fn target(device: Option<&str>, display: i32) -> VisualTarget {
        VisualTarget {
            handle: String::new(),
            device: device.map(str::to_string),
            display,
            center: (10, 20),
            bounds: BBox::new(0, 10, 20, 30).expect("a box"),
            label: "Settings".to_string(),
            source: Source::Tree,
            screen: ScreenPrint {
                cells: vec![0; 12 * 24],
                columns: 12,
                rows: 24,
            },
            at: Instant::now(),
        }
    }

    #[test]
    fn a_handle_names_one_control_on_one_device() {
        let mut registry = VisualRegistry::new();
        let handle = registry.register(target(Some("R58M111"), 0));
        assert_eq!(handle, "@v1");
        assert!(registry.get(&handle, Some("R58M111"), 0).is_some());
        // Another phone is not this phone, and the user's screen is not the
        // agent's display: neither can address a control found on the other.
        assert!(registry.get(&handle, Some("OTHER"), 0).is_none());
        assert!(registry.get(&handle, Some("R58M111"), 3).is_none());
        assert!(registry.get("@v99", Some("R58M111"), 0).is_none());
    }

    #[test]
    fn only_the_last_few_handles_are_kept() {
        let mut registry = VisualRegistry::new();
        let mut oldest = String::new();
        for _ in 0..REGISTRY_CAPACITY + 2 {
            let handle = registry.register(target(None, 0));
            if oldest.is_empty() {
                oldest = handle;
            }
        }
        assert_eq!(registry.len(), REGISTRY_CAPACITY);
        // The evicted handle is refused rather than resolved to the control
        // that has since taken its number.
        assert!(registry.get(&oldest, None, 0).is_none());
        let newest = format!("@v{}", REGISTRY_CAPACITY + 2);
        assert!(registry.get(&newest, None, 0).is_some());
    }
}
