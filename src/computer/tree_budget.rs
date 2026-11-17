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
    /// Roots moved ahead of others because they are the window under the
    /// pointer, for a caller that renders until its line budget runs out.
    pub promoted: usize,
}

impl PruneReport {
    /// Whether pruning changed anything.
    pub fn changed(&self) -> bool {
        self.folded > 0 || self.promoted > 0
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
///
/// The kept roots are then ordered *budget-first*: before anything else, the
/// window a person is actually working in — the focused one, else the one with
/// keyboard focus inside it — and then the rest by visible area, largest first.
/// Folding alone does not answer the other half of the question. Two windows
/// can both be on screen with one small and one large; the platform reports
/// siblings in whatever order it likes, so a small dock panel with a hundred
/// rows can still come first and eat the line budget the renderer stops at,
/// leaving the main window out of the outline. Ordering is what makes the
/// budget go to the window that matters, whatever the renderer's cap is.
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

    // Stable sort by priority, so windows of equal rank keep the platform's
    // own order — a stable order is what lets a person compare two outlines and
    // see the same windows in the same places.
    //
    // Folded roots carry the flag explicitly rather than being recognised from
    // their label: the label is a rendering, and a real window could legitimately
    // be titled "[off-screen]" — ordering by a string would let that decide the
    // budget.
    let folded_flags: Vec<bool> = roots
        .iter()
        .map(|root| is_offscreen(root.bounds, geometry))
        .collect();
    let mut indexed: Vec<(usize, bool, UiElement)> = kept
        .into_iter()
        .enumerate()
        .map(|(i, root)| (i, folded_flags[i], root))
        .collect();
    indexed.sort_by_key(|(_, folded, root)| {
        // Folded roots say one line and never need to be first; a window
        // nobody can see is not the window the person is typing into.
        (*folded, priority_rank(root))
    });
    // Count the roots whose position actually changed, not the ones whose rank
    // merely differs: a two-window swap is one promotion and one demotion, and
    // reporting two would overstate what the renderer did.
    report.promoted = indexed
        .iter()
        .enumerate()
        .filter(|(new, (old, _, _))| new != old)
        .count();

    (indexed.into_iter().map(|(_, _, root)| root).collect(), report)
}

/// How early a root should be rendered when the budget is finite: lower first.
///
/// Focus beats size. A focused 400×300 dialog is where the person is; the
/// 1920×1080 window behind it is scenery. Within "focused", an area rank keeps
/// the ordering useful when the platform reports focus for several roots (an
/// app's window frame and its focused child both do), and within "not focused"
/// the largest visible area wins, because that is the window with the most to
/// show.
fn priority_rank(root: &UiElement) -> u64 {
    let focused = root.focused.known() == Some(true) || subtree_has_focus(root);
    // Descending area: a bigger window sorts earlier. A window with no measured
    // geometry sorts *below* a measured one of any size — not knowing where a
    // window is is not evidence that it is the big one.
    let size = match root.bounds {
        Some(b) => {
            let area = u64::from(b.width).saturating_mul(u64::from(b.height));
            u64::from(u32::MAX) - area.min(u64::from(u32::MAX))
        }
        None => u64::from(u32::MAX),
    };
    if focused {
        size
    } else {
        // A whole range above every focused size, so focus outranks area for
        // every possible pair of windows.
        size + u64::from(u32::MAX) + 1
    }
}

/// Whether any descendant holds focus.
///
/// The focused control is inside a window, not usually the window itself, so a
/// root that only reports `focused: false` while its text field reports
/// `focused: true` still has to be recognised as the window in use.
fn subtree_has_focus(root: &UiElement) -> bool {
    root.children
        .iter()
        .any(|c| c.focused.known() == Some(true) || subtree_has_focus(c))
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

    // ── budget-first ordering ────────────────────────────────────────────
    //
    // Folding answers "a window nobody can see must not eat the budget". It
    // does not answer "two windows are on screen and the small one is first":
    // the platform reports siblings in its own order, and a renderer that
    // stops at `max_lines` then spends them on the dock panel. These are the
    // cases the plan's "budget allocated main-window-first" requires.

    /// The measured case, in the order that used to hurt: a small panel with
    /// far more rows than the budget comes first, and the main window second.
    #[test]
    fn the_focused_window_is_rendered_before_a_larger_but_unfocused_one() {
        let mut panel = window("Emojis", Some(Rect::new(1600, 0, 300, 500)), items(119));
        // Even a big window loses to the one being typed into.
        let mut editor = window("Document", Some(Rect::new(0, 0, 1500, 900)), vec![]);
        editor.focused = Tri::Known(true);
        panel.focused = Tri::Known(false);

        let (pruned, report) = prune(&[panel, editor], &display());
        assert_eq!(pruned[0].label.as_deref(), Some("Document"), "the focused window comes first");
        assert_eq!(report.promoted, 2, "both roots moved");
        // Nothing was folded: both are on screen, and the budget is a line
        // count rather than a reason to hide a visible window.
        assert_eq!(report.folded, 0);
    }

    /// Focus on a control *inside* a window makes that window the one in use.
    /// AT-SPI reports the frame and the focused entry separately, and keying
    /// only off the root's own state would miss every real case.
    #[test]
    fn focus_inside_a_window_marks_the_window() {
        let mut editor = window(
            "Document",
            Some(Rect::new(0, 0, 800, 600)),
            vec![{
                let mut field = window("Name", Some(Rect::new(10, 10, 200, 30)), vec![]);
                field.focused = Tri::Known(true);
                field
            }],
        );
        editor.focused = Tri::Known(false);
        let panel = window("Panel", Some(Rect::new(0, 0, 1500, 900)), items(40));

        let (pruned, _) = prune(&[panel, editor], &display());
        assert_eq!(pruned[0].label.as_deref(), Some("Document"));
    }

    /// With nobody focused, the largest visible window still goes first: it is
    /// the one with the most to show, and it is the least surprising order for
    /// a person comparing two outlines.
    #[test]
    fn without_focus_the_largest_window_comes_first() {
        let small = window("Toolbar", Some(Rect::new(0, 0, 200, 40)), items(30));
        let large = window("Main", Some(Rect::new(0, 100, 1400, 800)), items(4));

        let (pruned, _) = prune(&[small, large], &display());
        assert_eq!(pruned[0].label.as_deref(), Some("Main"));
    }

    /// A folded window never displaces a visible one, whatever its size says:
    /// the whole point of folding it is that nobody can see it.
    #[test]
    fn folded_windows_sort_behind_everything_visible() {
        let big_off = window("Off-screen giant", Some(Rect::new(-3000, 0, 2000, 2000)), items(5));
        let small_on = window("On screen", Some(Rect::new(10, 10, 100, 100)), vec![]);

        let (pruned, report) = prune(&[big_off, small_on], &display());
        assert_eq!(report.folded, 1);
        assert_eq!(pruned[0].label.as_deref(), Some("On screen"));
        assert!(pruned[1].label.as_deref().unwrap().contains("off-screen"));
    }

    /// A window with no measured geometry has no claim to the front: not
    /// knowing where it is is not evidence that it is the important one.
    #[test]
    fn an_unmeasured_window_sorts_behind_a_measured_one() {
        let unmeasured = window("Unknown", None, items(50));
        let measured = window("Known", Some(Rect::new(0, 0, 400, 300)), vec![]);

        let (pruned, _) = prune(&[unmeasured, measured], &display());
        assert_eq!(pruned[0].label.as_deref(), Some("Known"));
    }

    /// Ordering is stable: two equally-sized unfocused windows keep the order
    /// the platform reported, so two outlines of the same screen read the same.
    #[test]
    fn equal_windows_keep_the_platform_order() {
        let a = window("Alpha", Some(Rect::new(0, 0, 400, 300)), vec![]);
        let b = window("Beta", Some(Rect::new(400, 0, 400, 300)), vec![]);
        let (pruned, report) = prune(&[a, b], &display());
        assert_eq!(pruned[0].label.as_deref(), Some("Alpha"));
        assert_eq!(pruned[1].label.as_deref(), Some("Beta"));
        assert_eq!(report.promoted, 0, "nothing was reordered");
    }

    /// The OpenShot regression, with the renderer's cap in play: with a
    /// hundred-line budget the main window must still be inside the outline.
    #[test]
    fn the_line_budget_reaches_the_main_window() {
        use crate::computer::tree_render::TreeOutline;

        let main = window(
            "Untitled Project",
            Some(Rect::new(0, 0, 1440, 900)),
            vec![window(
                "Timeline",
                Some(Rect::new(0, 600, 1440, 300)),
                vec![],
            )],
        );
        let emojis = window("Emojis", Some(Rect::new(1500, 0, 400, 900)), items(119));

        let (pruned, _) = prune(&[emojis, main], &display());
        let mut outline = TreeOutline::default();
        outline.push_tree(
            &pruned,
            "accessibility",
            10,
            false,
            0,
            &std::collections::HashMap::new(),
        );
        assert!(
            outline.text.contains("Untitled Project"),
            "the main window must survive a tight budget: {}",
            outline.text
        );
    }

    /// Folding is not the whole answer. Here *both* windows are on screen, so
    /// nothing is folded — and the small panel is still reported first, so if
    /// ordering did not promote the main window the panel's rows would consume
    /// the renderer's whole cap and the window being edited would be missing
    /// from the outline. This is the half of the OpenShot bug that pruning
    /// alone does not fix, and the reason `prune` orders as well as folds.
    #[test]
    fn a_tight_budget_reaches_the_main_window_when_both_windows_are_on_screen() {
        use crate::computer::tree_render::TreeOutline;

        let main = window(
            "Untitled Project",
            Some(Rect::new(0, 0, 1440, 900)),
            vec![window(
                "Timeline",
                Some(Rect::new(0, 600, 1440, 300)),
                vec![],
            )],
        );
        // Parked against the right edge of the display, not outside it.
        let panel = window("Emojis", Some(Rect::new(1500, 0, 400, 900)), items(119));

        let (pruned, report) = prune(&[panel, main], &display());
        assert_eq!(report.folded, 0, "both windows are visible; nothing to fold");
        assert_eq!(report.promoted, 2, "the two roots swapped places");

        let mut outline = TreeOutline::default();
        outline.push_tree(
            &pruned,
            "accessibility",
            10,
            false,
            0,
            &std::collections::HashMap::new(),
        );
        assert!(
            outline.text.contains("Untitled Project"),
            "ordering alone must carry the main window into a tight budget: {}",
            outline.text
        );
    }
}
