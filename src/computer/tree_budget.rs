//! Deciding what a tree outline is allowed to spend its lines on.
//!
//! `max_tree_lines` is a *global* count, and the accessibility tree arrives in
//! whatever order the platform exposes siblings. Those two facts together mean
//! an app can lose its own window: OpenShot keeps an `Emojis` tool window
//! parked off-screen with 119 `listitem`s under it, and at the default cap of
//! 100 lines the emoji list consumed the whole budget before the renderer
//! reached the timeline. The model was then shown a truncated tree that did
//! not contain the thing it was asked to edit, and scored 0.75 on an `add_title`
//! task it should have solved.
//!
//! The fix is to decide *before* rendering: a window that is entirely outside
//! every display is folded to one line that says how many children were
//! omitted, and the remaining budget goes to what a person could actually see.
//! That is not a wording change — it changes which world model the model is
//! given, which is why it sits here rather than inside the renderer.
//!
//! Two rules matter and both are conservative:
//!
//! - An element with **no geometry** is never folded. Not knowing where
//!   something is is not evidence that it is off-screen, and dropping it would
//!   hide elements the platform simply did not describe.
//! - An element that **overlaps** a display is kept whole, even if most of it
//!   is off-screen. A window hanging off the right edge is still the window the
//!   person is typing into.
//!
//! When the display layout is unknown (headless, a probe that failed) nothing
//! is folded: with no geometry there is no visibility question to answer, and
//! guessing would silently hide windows on a machine that does have a screen.

use crate::computer::display::DisplayGeometry;
use crate::computer::types::{Rect, UiElement};

/// How a pruned tree came out, for the run summary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct PruneReport {
    /// Windows folded away because they sit entirely outside every display.
    pub folded: usize,
    /// Lines those windows would have cost, as far as their reported child
    /// counts go. A lower bound: unfetched children are not counted.
    pub lines_saved: usize,
    /// The layout was unknown, so nothing was folded. Reported so a caller can
    /// tell "everything is visible" from "nobody could tell".
    pub geometry_unknown: bool,
}

impl PruneReport {
    /// Whether pruning changed anything.
    pub fn changed(&self) -> bool {
        self.folded > 0
    }
}

/// Whether two rectangles share a non-empty area.
///
/// A zero-width or zero-height element never overlaps anything: it occupies no
/// space on screen, so it cannot be what the person is looking at.
fn overlaps(a: Rect, b: Rect) -> bool {
    if a.width == 0 || a.height == 0 || b.width == 0 || b.height == 0 {
        return false;
    }
    let ax2 = a.x.saturating_add(a.width as i32);
    let ay2 = a.y.saturating_add(a.height as i32);
    let bx2 = b.x.saturating_add(b.width as i32);
    let by2 = b.y.saturating_add(b.height as i32);
    a.x < bx2 && b.x < ax2 && a.y < by2 && b.y < ay2
}

/// Whether an element is entirely outside every display.
///
/// `true` only when the geometry was read *and* it excludes every display.
/// An element with no bounds answers `false`: not knowing where it is must not
/// be read as "it is nowhere".
pub fn is_offscreen(bounds: Option<Rect>, geometry: &DisplayGeometry) -> bool {
    let Some(bounds) = bounds else {
        return false;
    };
    if geometry.is_empty() {
        return false;
    }
    !geometry
        .displays
        .iter()
        .any(|display| overlaps(bounds, display.rect()))
}

/// Count an element's descendants, for the "N children omitted" line.
fn subtree_size(elements: &[UiElement]) -> usize {
    elements.iter().map(|e| 1 + subtree_size(&e.children)).sum()
}

/// Return the tree the renderer should draw, plus what was taken out.
///
/// Roots that are off-screen are replaced by a single folded line placed where
/// the root was, so the outline still says the window exists and where — the
/// model can ask for it explicitly if the task really is about that window.
/// Only *top-level* off-screen roots are folded. Descendants are reached
/// through their window, and a control inside a visible window is visible
/// whether or not its own box happens to fall outside the display.
pub fn prune(roots: &[UiElement], geometry: &DisplayGeometry) -> (Vec<UiElement>, PruneReport) {
    let mut report = PruneReport::default();
    if geometry.is_empty() {
        report.geometry_unknown = true;
        return (roots.to_vec(), report);
    }

    let mut kept = Vec::with_capacity(roots.len());
    for root in roots {
        if is_offscreen(root.bounds, geometry) {
            report.folded += 1;
            report.lines_saved += 1 + subtree_size(&root.children);
            kept.push(fold(root));
        } else {
            kept.push(root.clone());
        }
    }
    (kept, report)
}

/// The one-line stand-in for an off-screen window.
///
/// It keeps role, label, geometry and id — everything needed to say *what* was
/// folded and to address it deliberately — and drops the children, replacing
/// them with a count. `truncated` stays `false`: this is not a read that
/// stopped early, and marking it would make `tree_is_complete` claim the tree
/// is incomplete when the renderer is the one that chose to fold.
fn fold(root: &UiElement) -> UiElement {
    let omitted = subtree_size(&root.children);
    let mut folded = root.clone();
    folded.children = Vec::new();
    folded.children_count = None;
    folded.value = None;
    folded.label = Some(match (root.label.as_deref(), omitted) {
        (Some(label), n) if n > 0 => format!("{label} [{n} children omitted, off-screen]"),
        (Some(label), _) => format!("{label} [off-screen]"),
        (None, n) if n > 0 => format!("[{n} children omitted, off-screen]"),
        (None, _) => "[off-screen]".to_string(),
    });
    folded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::display::{DisplayGeometry, DisplayInfo};
    use crate::computer::types::Tri;

    fn display() -> DisplayGeometry {
        DisplayGeometry::new(vec![DisplayInfo::new(0, 0, 1920, 1080).primary()])
    }

    fn window(label: &str, bounds: Option<Rect>, children: Vec<UiElement>) -> UiElement {
        UiElement {
            id: String::new(),
            role: "window".to_string(),
            label: Some(label.to_string()),
            value: None,
            bounds,
            enabled: Tri::Known(true),
            focused: Tri::Unknown,
            actions: None,
            children,
            children_count: None,
            truncated: false,
        }
    }

    fn items(n: usize) -> Vec<UiElement> {
        (0..n)
            .map(|i| window(&format!("emoji {i}"), Some(Rect::new(-720, -600, 20, 20)), vec![]))
            .collect()
    }

    /// The OpenShot shape: a visible main window and an off-screen tool window
    /// holding far more children than the line budget allows.
    #[test]
    fn an_offscreen_window_is_folded_to_one_line() {
        let main = window(
            "Untitled Project",
            Some(Rect::new(0, 0, 1440, 900)),
            vec![window(
                "Timeline",
                Some(Rect::new(0, 600, 1440, 300)),
                vec![],
            )],
        );
        let emojis = window("Emojis", Some(Rect::new(-725, -600, 230, 156)), items(119));
        let roots = vec![main, emojis];

        let (pruned, report) = prune(&roots, &display());
        assert_eq!(pruned.len(), 2, "the window itself must not disappear");
        assert_eq!(report.folded, 1);
        assert!(report.lines_saved >= 119, "saved {}", report.lines_saved);

        // The main window is untouched, so the timeline is still reachable.
        assert_eq!(pruned[0].label.as_deref(), Some("Untitled Project"));
        assert_eq!(pruned[0].children.len(), 1);

        // The folded one is one line saying what is gone.
        let folded = &pruned[1];
        assert!(folded.children.is_empty());
        let label = folded.label.as_deref().unwrap();
        assert!(label.contains("Emojis"), "{label}");
        assert!(label.contains("119 children omitted"), "{label}");
        assert!(label.contains("off-screen"), "{label}");
        // Folding is not a truncation: the layout was read in full.
        assert!(!folded.truncated);
    }

    /// A window hanging off the edge is still the window being typed into.
    #[test]
    fn a_partially_visible_window_is_kept_whole() {
        let edge = window("Editor", Some(Rect::new(1800, 100, 600, 400)), items(3));
        let (pruned, report) = prune(&[edge], &display());
        assert_eq!(report.folded, 0);
        assert_eq!(pruned[0].children.len(), 3);
    }

    /// An element whose geometry was never read is not evidence of absence.
    #[test]
    fn an_element_without_geometry_is_never_folded() {
        let unknown = window("Unknown", None, items(5));
        let (pruned, report) = prune(&[unknown], &display());
        assert_eq!(report.folded, 0);
        assert_eq!(pruned[0].children.len(), 5);
    }

    /// With no display layout there is no visibility question — and a guess
    /// here would hide windows on a machine that does have a screen.
    #[test]
    fn an_unknown_layout_folds_nothing() {
        let roots = vec![window(
            "Far",
            Some(Rect::new(-5000, -5000, 100, 100)),
            items(2),
        )];
        let (pruned, report) = prune(&roots, &DisplayGeometry::default());
        assert_eq!(report.folded, 0);
        assert!(report.geometry_unknown);
        assert_eq!(pruned[0].children.len(), 2);
    }

    /// Two displays side by side: a window on the second one is visible even
    /// though it is far outside the primary's frame.
    #[test]
    fn a_window_on_a_second_display_is_visible() {
        let two = DisplayGeometry::new(vec![
            DisplayInfo::new(0, 0, 1920, 1080).primary(),
            DisplayInfo::new(1920, 0, 1920, 1080),
        ]);
        let right = window("On the right", Some(Rect::new(2000, 100, 800, 600)), items(4));
        let (pruned, report) = prune(&[right], &two);
        assert_eq!(report.folded, 0);
        assert_eq!(pruned[0].children.len(), 4);
    }

    /// A zero-size element occupies no screen space, so it overlaps nothing.
    #[test]
    fn a_zero_size_element_is_not_visible() {
        assert!(is_offscreen(Some(Rect::new(100, 100, 0, 50)), &display()));
        assert!(is_offscreen(Some(Rect::new(100, 100, 50, 0)), &display()));
        assert!(!is_offscreen(Some(Rect::new(100, 100, 50, 50)), &display()));
    }

    /// The fold keeps the id, so the model can still ask about that window.
    #[test]
    fn the_fold_keeps_the_address() {
        let mut off = window("Panel", Some(Rect::new(-800, -600, 200, 200)), items(2));
        off.id = "s1:0.7".to_string();
        let (pruned, _) = prune(&[off], &display());
        assert_eq!(pruned[0].id, "s1:0.7");
        assert_eq!(pruned[0].role, "window");
    }

    /// A window with no label still says why it is a single line.
    #[test]
    fn an_unlabelled_offscreen_window_still_explains_itself() {
        let mut off = window("x", Some(Rect::new(-800, -600, 200, 200)), items(3));
        off.label = None;
        let (pruned, _) = prune(&[off], &display());
        let label = pruned[0].label.as_deref().unwrap();
        assert!(label.contains("3 children omitted"), "{label}");
    }
}
