//! ScreenState — a unified snapshot of everything visible on screen.
//!
//! Combines the three perception sources into one struct:
//! - **UI tree** (accessibility API): structured hierarchy of elements
//! - **Screenshot** (pixels): ground truth of what is rendered
//! - **OCR text** (vision): text the accessibility tree cannot see
//!
//! Two snapshots can be compared with [`ScreenState::diff`] to produce a
//! [`ScreenDiff`] — the foundation of the verification loop: capture before
//! an action, capture after, diff, and hand the summary back to the LLM so
//! it knows whether the action had the intended effect.

use serde::{Deserialize, Serialize};

use super::TextBlock;
use crate::computer::types::{Rect, UiElement};
use crate::computer::{ComputerAdapter, DesktopAction, Result, Screenshot};

/// Whether the action's whole intended effect *is* a bounds change.
///
/// For these, a diff whose only content is `moved:` entries is the success
/// case. For every other action the same diff is the desktop reflowing, and
/// counting it as a working action is how a click on nothing verified.
pub fn action_changes_geometry(action: &DesktopAction) -> bool {
    matches!(
        action,
        DesktopAction::MoveWindow { .. }
            | DesktopAction::ResizeWindow { .. }
            | DesktopAction::MinimizeWindow { .. }
            | DesktopAction::MaximizeWindow { .. }
    )
}

/// Whether an action is expected to visibly change the screen — only these
/// get pre/post verification snapshots.
pub fn is_screen_mutating_action(action: &DesktopAction) -> bool {
    matches!(
        action,
        DesktopAction::Click { .. }
            | DesktopAction::DoubleClick { .. }
            | DesktopAction::Type { .. }
            | DesktopAction::KeyPress { .. }
            | DesktopAction::Scroll { .. }
            | DesktopAction::Drag { .. }
            | DesktopAction::LaunchApp { .. }
            | DesktopAction::ActivateWindow { .. }
            | DesktopAction::CloseWindow { .. }
            | DesktopAction::MoveWindow { .. }
            | DesktopAction::ResizeWindow { .. }
            | DesktopAction::MinimizeWindow { .. }
            | DesktopAction::MaximizeWindow { .. }
    )
}

/// Complete state of the screen at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenState {
    /// Top-level UI tree roots (windows), each with nested children.
    pub ui_tree: Vec<UiElement>,
    /// Screenshot (base64 PNG + dimensions).
    pub screenshot: Screenshot,
    /// All text visible on screen (joined OCR blocks). Empty when OCR was
    /// not run for this snapshot.
    #[serde(default)]
    pub ocr_text: String,
    /// Positioned OCR text regions. Empty when OCR was not run.
    #[serde(default)]
    pub ocr_regions: Vec<TextBlockSer>,
}

/// Serializable mirror of [`TextBlock`] (TextBlock itself does not derive
/// serde traits; it lives in the parent module for the vision pipeline).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextBlockSer {
    pub text: String,
    pub confidence: f32,
    pub bounds: Rect,
}

impl From<&TextBlock> for TextBlockSer {
    fn from(b: &TextBlock) -> Self {
        Self {
            text: b.text.clone(),
            confidence: b.confidence,
            bounds: b.bounds,
        }
    }
}

impl ScreenState {
    /// Build a snapshot from already-captured parts (no OCR).
    pub fn from_parts(ui_tree: Vec<UiElement>, screenshot: Screenshot) -> Self {
        Self {
            ui_tree,
            screenshot,
            ocr_text: String::new(),
            ocr_regions: Vec::new(),
        }
    }

    /// Fast capture: screenshot + UI tree, no OCR.
    ///
    /// This is the snapshot used by the transparent verification loop — it
    /// must stay cheap (~300ms on macOS).
    pub async fn capture_light(adapter: &dyn ComputerAdapter) -> Result<Self> {
        let screenshot = adapter.screenshot(None).await?;
        let ui_tree = adapter.read_ui_tree(None).await.unwrap_or_default();
        Ok(Self {
            ui_tree,
            screenshot,
            ocr_text: String::new(),
            ocr_regions: Vec::new(),
        })
    }

    /// Full capture: screenshot + UI tree + OCR text.
    #[cfg(feature = "vision")]
    pub async fn capture(
        adapter: &dyn ComputerAdapter,
        ocr: &mut super::ocr_rapid::RapidOcr,
    ) -> Result<Self> {
        let mut state = Self::capture_light(adapter).await?;
        let blocks = ocr.detect_text(&state.screenshot).await.unwrap_or_default();
        state.ocr_text = blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        state.ocr_regions = blocks.iter().map(TextBlockSer::from).collect();
        Ok(state)
    }

    /// Compare this snapshot (before) with another (after).
    pub fn diff(&self, after: &ScreenState) -> ScreenDiff {
        ScreenDiff {
            pixel: pixel_diff(&self.screenshot, &after.screenshot),
            tree: tree_diff(&self.ui_tree, &after.ui_tree),
        }
    }
}

/// Result of comparing two screen states.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenDiff {
    pub pixel: PixelDiffResult,
    pub tree: TreeDiffResult,
}

impl ScreenDiff {
    /// Whether the diff shows any user-visible change at all.
    pub fn has_meaningful_changes(&self) -> bool {
        self.pixel.change_percentage >= 0.5 || self.tree.has_changes()
    }

    /// Whether the diff is evidence that *an action* did something, as
    /// opposed to the desktop's own chrome tidying itself up.
    ///
    /// `has_meaningful_changes` answers "is the screen different"; this
    /// answers "did the thing I asked for happen". They are not the same
    /// question, and conflating them is how a step that changed nothing on
    /// the target reported `effect=confirmed`: xfce4-panel re-lays itself out
    /// between any two captures, so `tree.has_changes()` was true on every
    /// step regardless of what was clicked.
    ///
    /// A window or panel that only *moved* is not evidence. Pixel change is,
    /// a state transition (`value:`/`focus:`/`enabled:`) is, and elements
    /// appearing or disappearing are — but the last two only after the
    /// phantom-add bug in [`diff_elements`] is gone, which is why the
    /// matcher pairs equal-keyed siblings one-for-one.
    ///
    /// `geometry_counts` is for the actions whose whole point is geometry —
    /// `move_window`, `resize_window`, `minimize_window`, `maximize_window`.
    /// For those a bounds change is the expected effect, not churn.
    pub fn evidences_action(&self, geometry_counts: bool) -> bool {
        if self.pixel.change_percentage >= 0.5 {
            return true;
        }
        if !self.tree.added.is_empty() || !self.tree.removed.is_empty() {
            return true;
        }
        if geometry_counts {
            return !self.tree.changed.is_empty();
        }
        self.tree
            .changed
            .iter()
            .any(|entry| !entry.contains(" moved: "))
    }

    /// Compact one-paragraph summary for LLM consumption.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();

        if self.pixel.change_percentage >= 0.5 {
            if self.pixel.changed_regions.is_empty() {
                parts.push(format!("pixels: {:.1}% changed", self.pixel.change_percentage));
            } else {
                let regions = self
                    .pixel
                    .changed_regions
                    .iter()
                    .take(3)
                    .map(|r| format!("({},{}) {}x{}", r.x, r.y, r.width, r.height))
                    .collect::<Vec<_>>()
                    .join(", ");
                parts.push(format!(
                    "pixels: {:.1}% changed in {} region(s): {}",
                    self.pixel.change_percentage,
                    self.pixel.changed_regions.len(),
                    regions
                ));
            }
        }

        if !self.tree.added.is_empty() {
            parts.push(format!("added: {}", self.tree.added.join(", ")));
        }
        if !self.tree.removed.is_empty() {
            parts.push(format!("removed: {}", self.tree.removed.join(", ")));
        }
        if !self.tree.changed.is_empty() {
            parts.push(format!("changed: {}", self.tree.changed.join(", ")));
        }

        if parts.is_empty() {
            "no visible change detected".to_string()
        } else {
            parts.join("; ")
        }
    }

    /// The rendering of the facts, followed by what they do *not* establish.
    ///
    /// [`summary`](Self::summary) lists what moved; a page of `moved:` lines
    /// and no `value:`/`focus:` line says the window manager tidied itself
    /// up. The model reads the same text either way unless it is told which
    /// one it is looking at.
    pub fn summary_for_action(&self, geometry_counts: bool) -> String {
        let summary = self.summary();
        if summary == "no visible change detected" || self.evidences_action(geometry_counts) {
            return summary;
        }
        format!(
            "{summary}\nOnly geometry changed: no element's value, focus or enabled state did, \
             and too few pixels moved to count. That is the desktop reflowing, not the action \
             taking effect."
        )
    }
}

/// Pixel-level comparison of two screenshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PixelDiffResult {
    /// Percentage of 32x32 blocks whose average color changed (0.0–100.0).
    pub change_percentage: f32,
    /// Bounding boxes of connected changed regions, largest first (max 8).
    pub changed_regions: Vec<Rect>,
}

/// UI-tree comparison of two snapshots.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TreeDiffResult {
    /// Elements present only in the after-snapshot: `path role "label"`.
    pub added: Vec<String>,
    /// Elements present only in the before-snapshot.
    pub removed: Vec<String>,
    /// Elements present in both but with state changes (enabled/value/focus/bounds).
    pub changed: Vec<String>,
}

impl TreeDiffResult {
    pub fn has_changes(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty() || !self.changed.is_empty()
    }
}

/// Maximum diff entries per category before truncation.
const MAX_DIFF_ENTRIES: usize = 20;

// ── Pixel diff ─────────────────────────────────────────────────────────────

/// Compare two screenshots. With the `vision` feature this decodes the PNGs
/// and does a block-based comparison; otherwise it falls back to a sampled
/// byte comparison (no region information).
pub fn pixel_diff(before: &Screenshot, after: &Screenshot) -> PixelDiffResult {
    if before.base64 == after.base64 {
        return PixelDiffResult {
            change_percentage: 0.0,
            changed_regions: Vec::new(),
        };
    }
    if before.width != after.width || before.height != after.height {
        return PixelDiffResult {
            change_percentage: 100.0,
            changed_regions: vec![Rect::new(0, 0, after.width, after.height)],
        };
    }
    pixel_diff_impl(&before.base64, &after.base64)
}

#[cfg(feature = "vision")]
fn pixel_diff_impl(before_b64: &str, after_b64: &str) -> PixelDiffResult {
    use base64::Engine as _;
    let before_bytes = base64::engine::general_purpose::STANDARD.decode(before_b64);
    let after_bytes = base64::engine::general_purpose::STANDARD.decode(after_b64);
    match (before_bytes, after_bytes) {
        (Ok(b), Ok(a)) => block_diff_bytes(&b, &a),
        _ => byte_sampled_diff(before_b64, after_b64),
    }
}

#[cfg(not(feature = "vision"))]
fn pixel_diff_impl(before_b64: &str, after_b64: &str) -> PixelDiffResult {
    byte_sampled_diff(before_b64, after_b64)
}

/// Fallback: sampled byte comparison. Reports a change percentage but no
/// regions (we cannot map byte offsets to pixels without decoding).
fn byte_sampled_diff(before_b64: &str, after_b64: &str) -> PixelDiffResult {
    let (b, a) = (before_b64.as_bytes(), after_b64.as_bytes());
    let len = b.len().min(a.len());
    if len == 0 {
        return PixelDiffResult {
            change_percentage: 100.0,
            changed_regions: Vec::new(),
        };
    }
    let step = 64;
    let samples = len / step + 1;
    let mismatches = (0..len).step_by(step).filter(|&i| b[i] != a[i]).count();
    PixelDiffResult {
        change_percentage: (mismatches as f32 / samples as f32) * 100.0,
        changed_regions: Vec::new(),
    }
}

/// Decode PNGs and compare 32x32 blocks by average RGB distance.
#[cfg(feature = "vision")]
fn block_diff_bytes(before: &[u8], after: &[u8]) -> PixelDiffResult {
    let b = image::load_from_memory(before);
    let a = image::load_from_memory(after);
    match (b, a) {
        (Ok(b), Ok(a)) => block_diff_images(&b, &a),
        _ => PixelDiffResult {
            change_percentage: 100.0,
            changed_regions: Vec::new(),
        },
    }
}

#[cfg(feature = "vision")]
fn block_diff_images(before: &image::DynamicImage, after: &image::DynamicImage) -> PixelDiffResult {
    use image::GenericImageView;

    const BLOCK: u32 = 32;
    /// Average per-channel distance (0–255) above which a block counts as changed.
    const THRESHOLD: f32 = 30.0;

    let (w, h) = before.dimensions();
    if (w, h) != after.dimensions() {
        return PixelDiffResult {
            change_percentage: 100.0,
            changed_regions: vec![Rect::new(0, 0, w, h)],
        };
    }

    let b = before.to_rgb8();
    let a = after.to_rgb8();
    let cols = w.div_ceil(BLOCK);
    let rows = h.div_ceil(BLOCK);
    let mut changed = vec![false; (cols * rows) as usize];
    let mut changed_count = 0u32;

    for by in 0..rows {
        for bx in 0..cols {
            let (mut sb, mut sa) = ([0u64; 3], [0u64; 3]);
            let mut n = 0u64;
            for y in (by * BLOCK)..((by + 1) * BLOCK).min(h) {
                for x in (bx * BLOCK)..((bx + 1) * BLOCK).min(w) {
                    let pb = b.get_pixel(x, y).0;
                    let pa = a.get_pixel(x, y).0;
                    for c in 0..3 {
                        sb[c] += pb[c] as u64;
                        sa[c] += pa[c] as u64;
                    }
                    n += 1;
                }
            }
            if n == 0 {
                continue;
            }
            let dist: f32 = (0..3)
                .map(|c| ((sb[c] as f32 / n as f32) - (sa[c] as f32 / n as f32)).abs())
                .sum::<f32>()
                / 3.0;
            if dist > THRESHOLD {
                changed[(by * cols + bx) as usize] = true;
                changed_count += 1;
            }
        }
    }

    let total = cols * rows;
    let change_percentage = if total == 0 {
        0.0
    } else {
        (changed_count as f32 / total as f32) * 100.0
    };

    PixelDiffResult {
        change_percentage,
        changed_regions: merge_changed_blocks(&changed, cols, rows, BLOCK),
    }
}

/// Flood-fill connected changed blocks and return bounding boxes,
/// largest area first, capped at 8 regions.
#[cfg(feature = "vision")]
fn merge_changed_blocks(changed: &[bool], cols: u32, rows: u32, block: u32) -> Vec<Rect> {
    let mut visited = vec![false; changed.len()];
    let mut regions: Vec<Rect> = Vec::new();

    for start in 0..changed.len() {
        if !changed[start] || visited[start] {
            continue;
        }
        // BFS flood fill.
        let mut stack = vec![start];
        let (mut min_x, mut min_y) = (u32::MAX, u32::MAX);
        let (mut max_x, mut max_y) = (0u32, 0u32);
        while let Some(idx) = stack.pop() {
            if visited[idx] || !changed[idx] {
                continue;
            }
            visited[idx] = true;
            let (bx, by) = (idx as u32 % cols, idx as u32 / cols);
            min_x = min_x.min(bx);
            min_y = min_y.min(by);
            max_x = max_x.max(bx);
            max_y = max_y.max(by);
            // 4-connectivity neighbours.
            if bx > 0 {
                stack.push(idx - 1);
            }
            if bx + 1 < cols {
                stack.push(idx + 1);
            }
            if by > 0 {
                stack.push(idx - cols as usize);
            }
            if by + 1 < rows {
                stack.push(idx + cols as usize);
            }
        }
        regions.push(Rect::new(
            (min_x * block) as i32,
            (min_y * block) as i32,
            (max_x - min_x + 1) * block,
            (max_y - min_y + 1) * block,
        ));
    }

    regions.sort_by_key(|r| std::cmp::Reverse(r.width as u64 * r.height as u64));
    regions.truncate(8);
    regions
}

// ── Tree diff ──────────────────────────────────────────────────────────────

/// Recursively compare two UI trees, matching elements by (role, label).
pub fn tree_diff(before: &[UiElement], after: &[UiElement]) -> TreeDiffResult {
    let mut out = TreeDiffResult::default();
    diff_elements(before, after, "", &mut out);
    truncate_diff(&mut out);
    out
}

fn element_key(el: &UiElement) -> (&str, Option<&str>) {
    (el.role.as_str(), el.label.as_deref())
}

/// Whether an element of this role can hold keyboard focus.
///
/// Window managers and web engines mark the *window* or *container* that has
/// the pointer's attention as focused too, and that flag flips when a click
/// lands outside it. A focus transition on such an element is not evidence
/// that the click did anything to the content; a transition on a control that
/// accepts typing is.
fn takes_keyboard_focus(role: &str) -> bool {
    // Roles arrive in two spellings: the session's vocabulary (`spin_button`)
    // and a platform's raw name (`spin button`). Both mean the same control.
    let key: String = role
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    matches!(
        key.as_str(),
        "textfield"
            | "textfieldpassword"
            | "button"
            | "checkbox"
            | "radiobutton"
            | "combobox"
            | "menuitem"
            | "tab"
            | "slider"
            | "spinbutton"
            | "searchfield"
            | "link"
            | "entry"
            | "edit"
    ) || key.contains("textfield")
        || key.contains("edit")
}

fn describe(el: &UiElement) -> String {
    match &el.label {
        Some(l) if !l.is_empty() => format!("{} \"{}\"", el.role, l),
        _ => el.role.clone(),
    }
}

fn diff_elements(before: &[UiElement], after: &[UiElement], path: &str, out: &mut TreeDiffResult) {
    // Pair equal-keyed siblings **one for one**, not "does one exist at all".
    // A panel that re-enumerates leaves several siblings sharing a
    // `(role, label)` key; the old `after.iter().find(..)` handed every one of
    // them the same counterpart, and the `added` pass then compared against
    // the *whole* before-list, so a key that was already there could still be
    // reported as newly added. Eighteen identical `button "Applications"`
    // entries in one diff are what that looked like — and they consumed the
    // diff quota that the element actually clicked needed.
    let mut matched = vec![false; after.len()];
    for b in before {
        let key = element_key(b);
        let child_path = format!("{}/{}", path, describe(b));
        let counterpart = after
            .iter()
            .enumerate()
            .position(|(i, a)| !matched[i] && element_key(a) == key);
        match counterpart {
            Some(idx) => {
                matched[idx] = true;
                let a = &after[idx];
                if b.enabled != a.enabled {
                    out.changed
                        .push(format!("{} enabled: {} → {}", child_path, b.enabled, a.enabled));
                }
                if b.value != a.value {
                    out.changed
                        .push(format!("{} value: {:?} → {:?}", child_path, b.value, a.value));
                }
                // A *read* focus change is a change: clicking a field moves
                // focus to it, and that is often the entire visible effect of
                // a correct click on a segmented control (a date input's
                // month/day/year sub-fields highlight a few pixels of
                // themselves, far below the pixel diff's threshold). Without
                // this the click read as a no-op, the ladder escalated, and
                // the keyboard rung typed into a control that was already
                // focused — corrupting a value that was on its way to being
                // right. Only two *answers* are compared: an unread focus on
                // either side is Unknown, not a transition to "false".
                //
                // Only a control that can take keyboard input counts. A window
                // or container losing focus is what a click *outside* it looks
                // like; treating that as a working action is the opposite of
                // what the evidence means.
                if takes_keyboard_focus(&b.role) {
                    match (b.focused.known(), a.focused.known()) {
                        (Some(before), Some(after)) if before != after => {
                            out.changed
                                .push(format!("{} focus: {} → {}", child_path, before, after));
                        }
                        _ => {}
                    }
                }
                if b.bounds != a.bounds {
                    // Geometry the read did not publish is not "moved to";
                    // say which side went missing instead of inventing 0×0.
                    let describe_bounds = |r: Option<Rect>| match r {
                        Some(r) => format!("({},{}) {}x{}", r.x, r.y, r.width, r.height),
                        None => "unknown".to_string(),
                    };
                    out.changed.push(format!(
                        "{} moved: {} → {}",
                        child_path,
                        describe_bounds(a.bounds),
                        describe_bounds(b.bounds)
                    ));
                }
                diff_elements(&b.children, &a.children, &child_path, out);
            }
            None => out.removed.push(child_path),
        }
    }
    for (idx, a) in after.iter().enumerate() {
        if !matched[idx] {
            out.added.push(format!("{}/{}", path, describe(a)));
        }
    }
}

/// How much a `changed:` entry is worth keeping when the list has to be cut.
///
/// The quota is small on purpose, and it is spent in walk order unless
/// something reorders it. On a desktop with a taskbar, the panel's layout
/// churn produces twenty `moved:` lines before the window the action went to
/// is reached at all, so a field's `value:` change — the only line that says
/// the action worked — was always the part that got omitted.
fn change_priority(entry: &str) -> u8 {
    if entry.contains(" value: ") || entry.contains(" focus: ") || entry.contains(" enabled: ") {
        0
    } else if entry.contains(" moved: ") {
        2
    } else {
        1
    }
}

fn truncate_diff(out: &mut TreeDiffResult) {
    // Collapse repeats first. Repeats share a path, so they are the same
    // element read twice, not two elements — and each copy of
    // `button "Applications"` was costing a line of the quota.
    for list in [&mut out.added, &mut out.removed, &mut out.changed] {
        collapse_repeats(list);
    }
    // Then keep the entries that answer "did it work" and drop the ones that
    // only say "the desktop moved".
    out.changed.sort_by_key(|entry| change_priority(entry));
    for list in [&mut out.added, &mut out.removed, &mut out.changed] {
        if list.len() > MAX_DIFF_ENTRIES {
            let omitted = list.len() - MAX_DIFF_ENTRIES;
            list.truncate(MAX_DIFF_ENTRIES);
            list.push(format!("… {} more omitted", omitted));
        }
    }
}

/// Replace a run of identical entries with one entry and a count.
fn collapse_repeats(list: &mut Vec<String>) {
    let mut collapsed: Vec<String> = Vec::with_capacity(list.len());
    let mut i = 0;
    while i < list.len() {
        let mut run = 1;
        while i + run < list.len() && list[i + run] == list[i] {
            run += 1;
        }
        if run > 1 {
            collapsed.push(format!("{} (×{})", list[i], run));
        } else {
            collapsed.push(list[i].clone());
        }
        i += run;
    }
    *list = collapsed;
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::types::Rect;

    fn el(role: &str, label: Option<&str>, children: Vec<UiElement>) -> UiElement {
        UiElement {
            id: String::new(),
            role: role.to_string(),
            label: label.map(String::from),
            value: None,
            bounds: Some(Rect::new(0, 0, 100, 30)),
            enabled: crate::computer::Tri::Known(true),
            focused: crate::computer::Tri::Unknown,
            actions: None,
            children,
            children_count: None,
            truncated: false,
        }
    }

    #[test]
    fn tree_diff_identical_trees_have_no_changes() {
        let tree = vec![el(
            "window",
            Some("App"),
            vec![el("button", Some("OK"), vec![])],
        )];
        let diff = tree_diff(&tree, &tree);
        assert!(!diff.has_changes());
    }

    #[test]
    fn tree_diff_detects_added_element() {
        let before = vec![el("window", Some("App"), vec![])];
        let after = vec![el(
            "window",
            Some("App"),
            vec![el("button", Some("OK"), vec![])],
        )];
        let diff = tree_diff(&before, &after);
        assert_eq!(diff.added.len(), 1);
        assert!(diff.added[0].contains("button"));
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn tree_diff_detects_removed_element() {
        let before = vec![el(
            "window",
            Some("App"),
            vec![el("button", Some("OK"), vec![])],
        )];
        let after = vec![el("window", Some("App"), vec![])];
        let diff = tree_diff(&before, &after);
        assert_eq!(diff.removed.len(), 1);
        assert!(diff.added.is_empty());
    }

    #[test]
    fn tree_diff_detects_enabled_change() {
        let mut before_btn = el("button", Some("OK"), vec![]);
        before_btn.enabled = crate::computer::Tri::Known(false);
        let before = vec![el("window", Some("App"), vec![before_btn])];
        let after = vec![el(
            "window",
            Some("App"),
            vec![el("button", Some("OK"), vec![])],
        )];
        let diff = tree_diff(&before, &after);
        assert_eq!(diff.changed.len(), 1);
        assert!(diff.changed[0].contains("enabled: false → true"));
    }

    #[test]
    fn tree_diff_detects_focus_change() {
        // The whole point: a click that only moves focus is a real effect.
        // A segmented date control highlights a few dozen pixels — below the
        // pixel diff's threshold — so this verdict is the only thing that
        // stops the ladder from escalating and typing into a focused field.
        let mut before_seg = el("spin button", Some("month"), vec![]);
        before_seg.focused = crate::computer::Tri::Known(false);
        let mut after_seg = el("spin button", Some("month"), vec![]);
        after_seg.focused = crate::computer::Tri::Known(true);
        let before = vec![el("window", Some("App"), vec![before_seg])];
        let after = vec![el("window", Some("App"), vec![after_seg])];
        let diff = tree_diff(&before, &after);
        assert_eq!(diff.changed.len(), 1);
        assert!(diff.changed[0].contains("focus: false → true"), "{:?}", diff.changed);
        assert!(diff.has_changes());
    }

    #[test]
    fn tree_diff_ignores_unread_focus() {
        // `Unknown` is "nobody asked", not "false": two unread sides are not a
        // transition, and a one-sided read is not evidence of a move.
        let before = vec![el("button", Some("OK"), vec![])];
        let mut after = vec![el("button", Some("OK"), vec![])];
        after[0].focused = crate::computer::Tri::Known(true);
        assert!(!tree_diff(&before, &after).has_changes());
        assert!(!tree_diff(&after, &before).has_changes());
        let mut absent = vec![el("button", Some("OK"), vec![])];
        absent[0].focused = crate::computer::Tri::Absent;
        assert!(!tree_diff(&before, &absent).has_changes());
    }

    /// A window losing its activation is what a click landing *outside* it
    /// looks like. Counting that as an effect verified the wrong action: the
    /// screen shows the click went somewhere else, so the ladder has to keep
    /// escalating rather than report success.
    #[test]
    fn a_window_losing_activation_is_not_a_working_action() {
        let mut before_win = el("window", Some("App"), vec![]);
        before_win.focused = crate::computer::Tri::Known(true);
        let mut after_win = el("window", Some("App"), vec![]);
        after_win.focused = crate::computer::Tri::Known(false);
        let diff = tree_diff(&[before_win], &[after_win]);
        assert!(!diff.has_changes(), "{:?}", diff.changed);

        // A container is the same story: it is not a control the model clicks.
        let mut before_doc = el("document web", Some("Page"), vec![]);
        before_doc.focused = crate::computer::Tri::Known(true);
        let mut after_doc = el("document web", Some("Page"), vec![]);
        after_doc.focused = crate::computer::Tri::Known(false);
        assert!(!tree_diff(&[before_doc], &[after_doc]).has_changes());
    }

    /// The counterpart: the same transition on a control that takes typing is
    /// a real effect, so the guard is a filter and not a blanket disable.
    #[test]
    fn a_field_losing_focus_is_still_a_reported_change() {
        let mut before_field = el("text_field", Some("Name"), vec![]);
        before_field.focused = crate::computer::Tri::Known(true);
        let mut after_field = el("text_field", Some("Name"), vec![]);
        after_field.focused = crate::computer::Tri::Known(false);
        assert!(tree_diff(&[before_field], &[after_field]).has_changes());
    }

    /// A panel that re-enumerates presents several siblings with the same
    /// `(role, label)` key. Matching "does one exist at all" gave every one of
    /// them the same counterpart and then reported the *whole* key as newly
    /// added — 18 identical `button "Applications"` lines, and the tree diff
    /// said the screen had changed even when the action left it alone.
    #[test]
    fn repeated_sibling_keys_are_not_reported_as_added() {
        let panel = |count: usize| {
            let buttons = (0..count)
                .map(|_| el("button", Some("Applications"), vec![]))
                .collect();
            vec![el("panel", None, buttons)]
        };
        let diff = tree_diff(&panel(3), &panel(3));
        assert!(diff.added.is_empty(), "{:?}", diff.added);
        assert!(diff.removed.is_empty(), "{:?}", diff.removed);
        assert!(!diff.has_changes());

        // One extra copy really is one addition, not three.
        let diff = tree_diff(&panel(2), &panel(3));
        assert_eq!(diff.added.len(), 1, "{:?}", diff.added);
        assert!(diff.removed.is_empty(), "{:?}", diff.removed);
    }

    /// The quota is 20 entries per category. A taskbar's layout churn fills
    /// `changed` on its own, before the window the action went to is reached,
    /// so the `value:` line — the only one that says the action worked — was
    /// the part that got omitted.
    #[test]
    fn a_value_change_survives_a_flood_of_geometry_churn() {
        let window = |with_value: bool| {
            let panel = (0..40)
                .map(|i| {
                    let mut b = el("button", Some("Applications"), vec![]);
                    b.bounds = Some(Rect::new(i * 10, 0, 10, 26));
                    b
                })
                .collect::<Vec<_>>();
            let mut month = el("spin button", Some("month"), vec![]);
            month.value = Some(if with_value { "1.0" } else { "0.0" }.to_string());
            vec![
                el("panel", None, panel),
                el("window", Some("Date Picker Task"), vec![month]),
            ]
        };
        let diff = tree_diff(&window(false), &window(true));
        assert!(diff.has_changes());
        let joined = diff.changed.join(" | ");
        assert!(
            joined.contains("value: Some(\"0.0\") → Some(\"1.0\")"),
            "the value change was crowded out: {joined}"
        );
    }

    /// Repeats are the same element read twice; each copy was costing a line
    /// of the quota.
    #[test]
    fn repeated_diff_entries_are_collapsed() {
        let mut out = TreeDiffResult {
            added: vec!["/p/button \"Applications\"".to_string(); 6],
            ..Default::default()
        };
        truncate_diff(&mut out);
        assert_eq!(out.added.len(), 1, "{:?}", out.added);
        assert!(out.added[0].ends_with("(×6)"), "{:?}", out.added);
    }

    /// The point of `evidences_action`: a desktop that only reflows is not
    /// evidence that a click landed.
    #[test]
    fn geometry_only_churn_is_not_evidence_an_action_worked() {
        let diff = ScreenDiff {
            pixel: PixelDiffResult {
                change_percentage: 0.0,
                changed_regions: vec![],
            },
            tree: TreeDiffResult {
                changed: vec![
                    "/application \"xfce4-panel\"/window moved: (0,0) 96x26 → (96,0) 408x26"
                        .to_string(),
                ],
                ..Default::default()
            },
        };
        assert!(diff.has_meaningful_changes(), "the screen did change");
        assert!(!diff.evidences_action(false), "but not because of the action");
        assert!(diff.evidences_action(true), "unless the action was to move it");
        assert!(diff
            .summary_for_action(false)
            .contains("Only geometry changed"));

        // A value or focus transition is evidence for any action.
        let diff = ScreenDiff {
            pixel: PixelDiffResult {
                change_percentage: 0.0,
                changed_regions: vec![],
            },
            tree: TreeDiffResult {
                changed: vec![
                    "/window/spin button \"month\" value: Some(\"0.0\") → Some(\"1.0\")"
                        .to_string(),
                ],
                ..Default::default()
            },
        };
        assert!(diff.evidences_action(false));
    }

    #[test]
    fn pixel_diff_identical_base64_is_zero() {
        let ss = Screenshot::new("aGVsbG8=".to_string(), 100, 100);
        let diff = pixel_diff(&ss, &ss);
        assert_eq!(diff.change_percentage, 0.0);
        assert!(diff.changed_regions.is_empty());
    }

    #[test]
    fn pixel_diff_dimension_mismatch_is_full_change() {
        let a = Screenshot::new("AAAA".to_string(), 100, 100);
        let b = Screenshot::new("BBBB".to_string(), 200, 100);
        let diff = pixel_diff(&a, &b);
        assert_eq!(diff.change_percentage, 100.0);
    }

    #[test]
    fn byte_sampled_diff_reports_percentage() {
        // step = 64, so mismatches at indices 0 and 64 are sampled.
        let before = "A".repeat(256);
        let mut after = before.clone();
        after.replace_range(0..1, "B");
        after.replace_range(64..65, "B");
        let diff = byte_sampled_diff(&before, &after);
        // 2 of 5 sampled indices (0, 64, 128, 192, 256→clamped) differ.
        assert!(diff.change_percentage > 0.0);
        assert!(diff.change_percentage < 100.0);
    }

    #[test]
    fn summary_reports_no_change() {
        let diff = ScreenDiff {
            pixel: PixelDiffResult {
                change_percentage: 0.0,
                changed_regions: vec![],
            },
            tree: TreeDiffResult::default(),
        };
        assert_eq!(diff.summary(), "no visible change detected");
        assert!(!diff.has_meaningful_changes());
    }

    #[test]
    fn summary_includes_pixel_and_tree() {
        let diff = ScreenDiff {
            pixel: PixelDiffResult {
                change_percentage: 12.5,
                changed_regions: vec![Rect::new(100, 140, 200, 160)],
            },
            tree: TreeDiffResult {
                added: vec!["/window/button \"OK\"".to_string()],
                removed: vec![],
                changed: vec![],
            },
        };
        let s = diff.summary();
        assert!(s.contains("12.5%"));
        assert!(s.contains("added:"));
        assert!(diff.has_meaningful_changes());
    }

    /// Encode a solid-color image (with an optional white block) as a
    /// base64 PNG screenshot for pixel-diff tests.
    #[cfg(feature = "vision")]
    fn png_screenshot(width: u32, height: u32, white_block: Option<Rect>) -> Screenshot {
        use base64::Engine as _;
        let mut img = image::RgbImage::new(width, height);
        if let Some(r) = white_block {
            for y in r.y as u32..(r.y as u32 + r.height).min(height) {
                for x in r.x as u32..(r.x as u32 + r.width).min(width) {
                    img.put_pixel(x, y, image::Rgb([255, 255, 255]));
                }
            }
        }
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        Screenshot::new(
            base64::engine::general_purpose::STANDARD.encode(buf.into_inner()),
            width,
            height,
        )
    }

    #[cfg(feature = "vision")]
    #[test]
    fn pixel_diff_identical_pngs_is_zero() {
        let a = png_screenshot(64, 64, None);
        let b = png_screenshot(64, 64, None);
        let diff = pixel_diff(&a, &b);
        assert_eq!(diff.change_percentage, 0.0);
        assert!(diff.changed_regions.is_empty());
    }

    #[cfg(feature = "vision")]
    #[test]
    fn pixel_diff_detects_changed_block() {
        let a = png_screenshot(64, 64, None);
        // One 32x32 block out of four changes → 25% of blocks.
        let b = png_screenshot(64, 64, Some(Rect::new(32, 0, 32, 32)));
        let diff = pixel_diff(&a, &b);
        assert!((diff.change_percentage - 25.0).abs() < 0.1);
        assert_eq!(diff.changed_regions.len(), 1);
        assert_eq!(diff.changed_regions[0].x, 32);
        assert_eq!(diff.changed_regions[0].y, 0);
    }
}
