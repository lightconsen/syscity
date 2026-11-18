//! What a replay checks before it moves anything.
//!
//! A recorded workflow aims by *name*, not by id: [`crate::planner::workflow::stable_target`]
//! rewrites an element id into the element's label (or role + label) so the
//! replay survives a restart that invalidates every id. The cost is that a name
//! is not an identity — two "Save" buttons are two different buttons — and the
//! screen a workflow runs against is not the screen it was recorded on. A
//! replay that binds a step to the wrong one of them clicks the wrong thing and
//! reports success, which is the one failure mode a replay must not have.
//!
//! So before the first step, the targets are resolved against the current tree
//! and the ambiguous ones are reported *before* anything moves.
//!
//! # Why ambiguity and not "exists"
//!
//! The plan said "verify every target exists and is unique; if not unique, stop
//! and ask". The second half is right and the first half is a trap: most
//! workflows open their window in step 1 (`launch Finder`, then click inside
//! it), so at *replay start* the window is not on screen yet. A preflight that
//! demanded existence would refuse every workflow that opens its own app — that
//! is, most of them.
//!
//! Ambiguity is different. It is only knowable for an element that *is* on
//! screen, and its meaning does not depend on when it is asked: two matches now
//! are still two matches at the moment the step runs, so a step that will have
//! to guess later can be named now. Nothing is reported for a target that is
//! merely absent — the step will report that itself, in its own words, when it
//! gets there.
//!
//! # Why this is not the ladder's refusal
//!
//! `ComputerSession` already refuses an ambiguous target: `resolve()` returns
//! `Ambiguous`, `ladder_for` cannot offer the accessibility rung, and the pixel
//! rung's own resolution fails, so nothing is dispatched. That is the right end
//! state and too late to be useful — it surfaces as step N failing for a reason
//! the person has to reconstruct from a step log, after N-1 steps have already
//! touched the screen. This module answers the same question at step 0, and
//! answers it for every step at once.

use crate::computer::element_index::{ResolveError, UiSnapshot};
use crate::computer::types::{ClickTarget, DesktopAction};

/// One step that cannot be replayed unambiguously.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousStep {
    /// 1-based position in the workflow, which is how a person counts steps.
    pub step: usize,
    /// The step's description, or the action's own rendering when it has none.
    pub description: String,
    /// What was asked for, e.g. `label~Save`.
    pub query: String,
    /// How many elements matched.
    pub count: usize,
    /// The first few matches, so the caller can choose one.
    pub candidates: Vec<CandidateSummary>,
}

/// An element that matched, as a person reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateSummary {
    /// The element id to aim at instead.
    pub id: String,
    pub role: String,
    pub label: Option<String>,
}

/// The targets one action names, in the order they would be acted on.
///
/// Coordinates name no element and are skipped: a point is not ambiguous, it is
/// exactly itself, however many windows sit under it.
pub fn targets_of(action: &DesktopAction) -> Vec<&ClickTarget> {
    match action {
        DesktopAction::Click { target, .. }
        | DesktopAction::DoubleClick { target, .. }
        | DesktopAction::Hover { target }
        | DesktopAction::Scroll { target, .. } => vec![target],
        DesktopAction::Drag { from, to } => vec![from, to],
        DesktopAction::Type { target: Some(target), .. } => vec![target],
        _ => Vec::new(),
    }
}

/// Why a target cannot be replayed unambiguously, if it cannot.
///
/// `None` covers both "one match" and "no match": only a *choice* is a reason
/// to stop before starting, and the second case belongs to the step that runs.
pub fn ambiguity_in(
    tree: &UiSnapshot,
    target: &ClickTarget,
) -> Option<(String, usize, Vec<CandidateSummary>)> {
    // An id names one element or none — a path id is a slot in one tree, and a
    // platform id is looked up, not matched. Only names are matched.
    if matches!(target, ClickTarget::Coordinate(_) | ClickTarget::ElementId(_)) {
        return None;
    }
    match tree.resolve(target) {
        Err(ResolveError::Ambiguous { query, count, candidates }) => Some((
            query,
            count,
            candidates
                .into_iter()
                .map(|c| CandidateSummary {
                    id: c.id,
                    role: c.role,
                    label: c.label,
                })
                .collect(),
        )),
        _ => None,
    }
}

/// Every step that would have to guess, against `tree`.
///
/// A step may name two targets (a drag); each is checked, and a step with two
/// ambiguous ends is reported once per end because they have to be fixed
/// separately.
pub fn find_ambiguous(steps: &[(String, DesktopAction)], tree: &UiSnapshot) -> Vec<AmbiguousStep> {
    let mut out = Vec::new();
    for (index, (description, action)) in steps.iter().enumerate() {
        for target in targets_of(action) {
            let Some((query, count, candidates)) = ambiguity_in(tree, target) else {
                continue;
            };
            out.push(AmbiguousStep {
                step: index + 1,
                description: description.clone(),
                query,
                count,
                candidates,
            });
        }
    }
    out
}

/// A message for the person, naming what to do about it.
///
/// Deliberately not JSON: this is a refusal, and a refusal a person cannot read
/// is a refusal they work around by forcing the replay.
pub fn refuse(workflow: &str, found: &[AmbiguousStep]) -> String {
    let mut message = format!(
        "Workflow {workflow:?} would have to guess: {} target(s) match more than one element on \
         screen. Nothing was replayed.",
        found.len()
    );
    for step in found {
        message.push_str(&format!(
            "\n  step {} ({}): {} matched {} elements",
            step.step, step.description, step.query, step.count
        ));
        for candidate in &step.candidates {
            let label = candidate.label.as_deref().unwrap_or("<unlabelled>");
            message.push_str(&format!("\n    {} {:?} id={}", candidate.role, label, candidate.id));
        }
    }
    message.push_str(
        "\nRe-record the step aimed at an element id, or make the targets distinguishable \
         (different labels, or one window in front).",
    );
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::element_index::assign_ids;
    use crate::computer::types::{Point, Rect, Tri};
    use crate::computer::UiElement;

    fn element(role: &str, label: Option<&str>, x: i32) -> UiElement {
        UiElement {
            id: String::new(),
            role: role.to_string(),
            label: label.map(String::from),
            value: None,
            bounds: Some(Rect::new(x, 10, 20, 10)),
            enabled: Tri::Known(true),
            focused: Tri::Known(false),
            actions: None,
            children: vec![],
            children_count: None,
            truncated: false,
        }
    }

    fn tree(roots: Vec<UiElement>) -> UiSnapshot {
        let mut roots = roots;
        assign_ids(&mut roots, 7);
        UiSnapshot { id: 7, app: None, roots }
    }

    fn click(target: ClickTarget) -> DesktopAction {
        DesktopAction::Click {
            target,
            button: crate::computer::MouseButton::Left,
        }
    }

    #[test]
    fn two_same_labelled_buttons_are_reported_before_the_replay() {
        let tree = tree(vec![
            element("button", Some("Save"), 0),
            element("button", Some("Save"), 40),
        ]);
        let steps =
            vec![("click Save".to_string(), click(ClickTarget::ElementLabel("Save".into())))];
        let found = find_ambiguous(&steps, &tree);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].step, 1);
        assert_eq!(found[0].count, 2);
        assert_eq!(found[0].candidates.len(), 2);
        // The ids travel, so the person can aim a re-recording at one of them.
        assert!(found[0].candidates.iter().all(|c| !c.id.is_empty()));
    }

    #[test]
    fn a_unique_target_is_not_a_reason_to_refuse() {
        let tree = tree(vec![element("button", Some("Save"), 0)]);
        let steps =
            vec![("click Save".to_string(), click(ClickTarget::ElementLabel("Save".into())))];
        assert!(find_ambiguous(&steps, &tree).is_empty());
    }

    #[test]
    fn an_absent_target_is_left_to_the_step_that_finds_it_missing() {
        // The reason this is not an existence check: step 1 of most workflows
        // opens the very window its later steps click in.
        let tree = tree(vec![element("button", Some("Open"), 0)]);
        let steps =
            vec![("click Save".to_string(), click(ClickTarget::ElementLabel("Save".into())))];
        assert!(
            find_ambiguous(&steps, &tree).is_empty(),
            "an absent target must not block a workflow that creates it"
        );
    }

    #[test]
    fn coordinates_are_never_ambiguous() {
        let tree = tree(vec![
            element("button", Some("Save"), 0),
            element("button", Some("Save"), 40),
        ]);
        let steps =
            vec![("click a point".to_string(), click(ClickTarget::Coordinate(Point::new(5, 5))))];
        assert!(find_ambiguous(&steps, &tree).is_empty());
    }

    #[test]
    fn a_drag_reports_each_ambiguous_end() {
        let tree = tree(vec![
            element("cell", Some("Invoices"), 0),
            element("cell", Some("Invoices"), 40),
        ]);
        let steps = vec![(
            "drag".to_string(),
            DesktopAction::Drag {
                from: ClickTarget::ElementLabel("Invoices".into()),
                to: ClickTarget::ElementLabel("Invoices".into()),
            },
        )];
        let found = find_ambiguous(&steps, &tree);
        assert_eq!(found.len(), 2, "both ends need fixing: {found:?}");
        assert!(found.iter().all(|f| f.step == 1));
    }

    #[test]
    fn role_qualified_targets_are_checked_too() {
        let tree = tree(vec![
            element("button", Some("Save"), 0),
            element("button", Some("Save"), 40),
        ]);
        let steps = vec![(
            "click Save".to_string(),
            click(ClickTarget::ElementRoleLabel {
                role: "button".into(),
                label: "Save".into(),
            }),
        )];
        assert_eq!(find_ambiguous(&steps, &tree).len(), 1);
    }

    #[test]
    fn the_refusal_names_the_step_the_matches_and_the_way_out() {
        let tree = tree(vec![
            element("button", Some("Save"), 0),
            element("button", Some("Save"), 40),
        ]);
        let steps =
            vec![("click Save".to_string(), click(ClickTarget::ElementLabel("Save".into())))];
        let message = refuse("invoices", &find_ambiguous(&steps, &tree));
        assert!(message.contains("invoices"), "{message}");
        assert!(message.contains("step 1"), "{message}");
        assert!(message.contains("Nothing was replayed"), "{message}");
        assert!(message.contains("Re-record"), "the way out is named: {message}");
    }

    #[test]
    fn step_numbers_are_one_based_like_the_step_lines() {
        let tree = tree(vec![
            element("button", Some("Save"), 0),
            element("button", Some("Save"), 40),
        ]);
        let steps = vec![
            ("open".to_string(), click(ClickTarget::Coordinate(Point::new(1, 1)))),
            ("save".to_string(), click(ClickTarget::ElementLabel("Save".into()))),
        ];
        let found = find_ambiguous(&steps, &tree);
        assert_eq!(found[0].step, 2, "the second step, counted from one");
    }
}
