//! After an action: what changed, and whether a write landed, settled.
use serde_json::Value;

// The set, the device targeting, and the siblings' items: this file used
// to be part of `android`, and names them exactly as it did there.
use super::*;

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
pub(super) enum InputOp {
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
pub(super) async fn dispatch_display(id: i32, op: &InputOp) -> Result<(), String> {
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
pub(super) async fn read_back_write(
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
        verdict: verdict.as_str(),
        actual: Some(actual),
        note: (verdict == ReadbackVerdict::Unknown).then(|| {
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
pub(super) fn write_point(before: &[UiElement]) -> (i32, i32) {
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
pub(super) fn intersects(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
}

/// The sentence a write's result ends with, when it has one.
pub(super) fn readback_note(readback: Option<&Readback>) -> Option<String> {
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
pub(super) async fn paste_on_display(display_id: i32, text: &str) -> Result<(), String> {
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
pub(super) async fn paste_on_user_screen(text: &str) -> Result<(), String> {
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
pub(super) async fn display_pixels(display_id: i32) -> Option<super::display::FrameSignature> {
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
pub(super) fn parse_gesture_paths(value: Option<&Value>) -> Result<Vec<Vec<(i32, i32)>>, String> {
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
//
// The verdicts and the rules that produce them are the shared ones in
// [`crate::computer::platform::screen`]: an iOS set driving WebDriverAgent has
// the same write-and-verify problem and should reach the same answer. What is
// Android's is the *locating* — finding the field that was written to in an
// accessibility tree — and that stays below.

pub use crate::computer::platform::screen::{
    readback_matches, readback_verdict, ReadbackVerdict, READBACK_MATCH, READBACK_MISMATCH,
    READBACK_UNKNOWN,
};

/// The field read back after `this` much silence, or `None` to skip the check.
///
/// A short pause is enough on a local accessibility read and is a wasted second
/// on the ADB path, so it is only spent when the caller asked for verification
/// and expects to read back.
pub(super) const READBACK_DELAY_MS: u64 = 250;

/// One field's answer after a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readback {
    /// One of the `READBACK_*` words.
    pub verdict: &'static str,
    /// What the field holds now, when it could be read.
    pub actual: Option<String>,
    /// Why the verdict is unknown, when it is.
    pub note: Option<String>,
}

impl Readback {
    pub(super) fn unknown(note: impl Into<String>) -> Self {
        Self {
            verdict: READBACK_UNKNOWN,
            actual: None,
            note: Some(note.into()),
        }
    }
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

/// Whether the screen after the write shows text derived from what was typed.
///
/// The evidence that an unreadable field nevertheless received the input: the
/// app echoed it somewhere — a suggestion strip, a preview, a character count.
/// Only labels that *contain* the typed text count, and only ones that were not
/// on the screen before, so a static label that happens to match is not
/// mistaken for a response.
pub(super) fn screen_echoed(before: &[UiElement], after: &[UiElement], written: &str) -> bool {
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
//
// The five effect words and the rules that produce them live in
// [`crate::computer::platform::screen`], because none of them are Android's:
// an iOS set driving WebDriverAgent asks the same questions and should get
// the same answers. What is Android's is the *evidence* — the tree fingerprint,
// the element identity, the screenshots — and that stays here.

pub use crate::computer::platform::screen::{
    EFFECT_CHANGED, EFFECT_PLACE_CHANGED, EFFECT_SMALL_CHANGE, EFFECT_UNCHANGED, EFFECT_UNKNOWN,
};

/// How the element list changed between two reads.
///
/// The counting rules and the wording are the shared ones in
/// [`crate::computer::platform::screen`]; what is Android's is the identity
/// used to pair rows — see [`tree_diff`].
pub use crate::computer::platform::screen::TreeDelta as TreeDiff;

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

/// The effect verdict, from what was observed after the action.
///
/// The judgment itself is the shared one — see
/// [`crate::computer::platform::screen::classify`] — and this wrapper exists
/// only so the Android code keeps its `&'static str` result, which is what the
/// result payload has always carried. Anything new should use
/// [`crate::computer::platform::screen::Effect`] directly.
pub fn classify_effect(
    before_package: Option<&str>,
    after_package: Option<&str>,
    diff: Option<&TreeDiff>,
    shown: usize,
    pixels_changed: Option<bool>,
    before_fingerprint: Option<u64>,
    after_fingerprint: Option<u64>,
) -> &'static str {
    crate::computer::platform::screen::classify(
        before_package,
        after_package,
        diff,
        shown,
        pixels_changed,
        before_fingerprint,
        after_fingerprint,
    )
    .as_str()
}

/// How often the screen is polled while waiting for it to settle.
pub(super) const SETTLE_POLL_MS: u64 = 150;
/// A screen that keeps this value for one poll is considered settled, rather
/// than merely "nothing has happened yet".
pub(super) const SETTLE_MIN_MS: u64 = 250;
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
    /// [`Signal::Events`](crate::computer::platform::screen::Signal::Events)
    /// (a content event, then silence) or
    /// [`Signal::Tree`](crate::computer::platform::screen::Signal::Tree) (the
    /// poll). `None` when no stream was available and nothing was claimed
    /// about it.
    pub first_signal: Option<crate::computer::platform::screen::Signal>,
}

impl Settle {
    /// A one-line verdict for the tool result.
    pub(super) fn verdict(&self) -> String {
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
    let mut first_signal: Option<crate::computer::platform::screen::Signal> = None;
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
                    first_signal = Some(crate::computer::platform::screen::Signal::Events);
                    break;
                }
                if stable && started.elapsed().as_millis() as u64 >= SETTLE_MIN_MS {
                    first_signal = Some(crate::computer::platform::screen::Signal::Tree);
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
pub(super) const SETTLE_CONTENT_QUIET_MS: u64 = 400;

pub(super) fn settle_event_matters(
    event: &crate::device::DeviceEvent,
    package: Option<&str>,
) -> bool {
    // The rule is shared — see `platform::screen::content_event_matters` — so
    // an iOS set with an event stream reaches the same answer.
    crate::computer::platform::screen::content_event_matters(event, package)
}

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
pub(super) async fn display_frame(target: Target) -> Option<super::display::FrameSignature> {
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
pub(super) fn format_ui_summary_limited(elements: &[UiElement], limit: usize) -> String {
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
