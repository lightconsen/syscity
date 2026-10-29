//! The page rung's browser side.
//!
//! The computer session can name an element the way the accessibility tree
//! named it (a role and an accessible name) but it cannot turn that into a
//! DOM element — that needs the page. This actor does the translation: take a
//! fresh ARIA snapshot (which assigns the refs), find the line whose role and
//! name fit the target, and act on that ref through CDP.
//!
//! Matching is deliberately strict. The point of the page rung is to reach an
//! element the screen could not, not to pick a plausible nearby one: an
//! element that does not match by name is not clicked.

use std::sync::Arc;

use crate::browser::{ActKind, AriaSnapshot, BrowserPool};
use crate::computer::page::{PageAction, PageActor, PageTarget};
use crate::computer::{ActionEffect, ActionEvidence, ActionResult, ComputerError, Rung};

/// Longest snapshot the actor will read before matching. Element names are
/// unique enough that a truncated tail is very unlikely to hold the one that
/// matters, and an unbounded snapshot on a huge page costs more than the rung.
const SNAPSHOT_MAX_CHARS: usize = 100_000;

/// Acts on a live browser page, through the same pool the browser tool uses.
///
/// The pool is shared with the browser tool: a page opened by the tool has to
/// be the page that answers here, so a second pool would be a second browser
/// and no page at all.
pub struct BrowserPageActor {
    pool: Arc<BrowserPool>,
    profile: String,
}

impl BrowserPageActor {
    pub fn new(pool: Arc<BrowserPool>, profile: impl Into<String>) -> Self {
        Self { pool, profile: profile.into() }
    }
}

#[async_trait::async_trait]
impl PageActor for BrowserPageActor {
    async fn act(
        &self,
        owner: &str,
        target: &PageTarget,
        action: PageAction,
    ) -> crate::computer::Result<ActionResult> {
        let instance = self
            .pool
            .get_or_create(&self.profile)
            .await
            .map_err(|e| ComputerError::ToolFailed(format!("browser pool: {e}")))?;
        let Some(handle) = instance.most_recent_page_of(owner).await else {
            return Err(ComputerError::ElementNotFound(
                "this conversation has no open browser page — open one with the browser tool first"
                    .to_string(),
            ));
        };

        let snapshot = crate::browser::aria_snapshot(&handle.page, SNAPSHOT_MAX_CHARS)
            .await
            .map_err(|e| ComputerError::ToolFailed(format!("aria snapshot: {e}")))?;
        let Some(ref_id) = find_ref(&snapshot, target) else {
            return Err(ComputerError::ElementNotFound(describe_not_found(target)));
        };

        let kind = match action {
            PageAction::Click => ActKind::Click,
            PageAction::Type(text) => ActKind::Fill { text },
        };
        let message = crate::browser::act_by_ref(&handle.page, ref_id, kind)
            .await
            .map_err(|e| ComputerError::ToolFailed(format!("page action: {e}")))?;
        Ok(ActionResult::success(message)
            .with_evidence(ActionEvidence::new(Rung::Page, ActionEffect::Confirmed)))
    }
}

/// The ref of the element the target names, when the page has it.
fn find_ref(snapshot: &AriaSnapshot, target: &PageTarget) -> Option<usize> {
    let mut best: Option<(u8, usize)> = None;
    for line in &snapshot.lines {
        // Ref 0 is the snapshot's "not actionable" marker — a label or heading
        // that carries the right name but cannot be clicked or filled.
        if line.ref_id == 0 {
            continue;
        }
        let Some(score) =
            match_score(&target.role, target.label.as_deref(), &line.role, &line.name)
        else {
            continue;
        };
        if best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, line.ref_id));
        }
    }
    best.map(|(_, ref_id)| ref_id)
}

/// How well a snapshot line fits the target.
///
/// Higher is better, and `None` is "not this element". A name is what makes a
/// match specific — clicking "the button in this page" when several exist is
/// the guess the rung must not make — so a role-only match is the weakest and
/// a name that does not appear at all is no match.
fn match_score(
    wanted_role: &str,
    wanted_label: Option<&str>,
    line_role: &str,
    line_name: &str,
) -> Option<u8> {
    let label = wanted_label.unwrap_or("").trim();
    let name_matches = !label.is_empty()
        && (line_name.eq_ignore_ascii_case(label)
            || line_name.to_lowercase().contains(&label.to_lowercase()));

    if !label.is_empty() && !name_matches {
        return None;
    }
    if !role_matches(wanted_role, line_role) {
        return None;
    }

    Some(match (name_matches, line_name.eq_ignore_ascii_case(label)) {
        (false, _) => 1,
        (true, false) => 2,
        (true, true) => 3,
    })
}

/// Whether a page element with `line_role` is the kind of control the desktop
/// tree called `wanted_role`.
fn role_matches(wanted_role: &str, line_role: &str) -> bool {
    let wanted = normalize_role(wanted_role);
    let actual = normalize_role(line_role);
    if wanted.is_empty() {
        return true;
    }
    if wanted == actual {
        return true;
    }
    // The two vocabularies name the same controls differently.
    let aliases: &[&str] = match wanted.as_str() {
        "textfield" | "searchfield" | "combobox" | "edit" => &["textbox", "searchbox", "combobox"],
        "radiobutton" => &["radio", "radiogroup"],
        "checkbox" => &["checkbox"],
        "tabitem" | "tab" => &["tab"],
        "menuitem" | "menuitemcheckbox" | "menuitemradio" => &["menuitem"],
        "link" => &["link"],
        "button" | "toolbarbutton" => &["button"],
        "option" | "listitem" => &["option"],
        _ => &[],
    };
    aliases.contains(&actual.as_str())
}

fn normalize_role(role: &str) -> String {
    role.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn describe_not_found(target: &PageTarget) -> String {
    let label = target.label.as_deref().unwrap_or("").trim();
    match (target.role.trim(), label.is_empty()) {
        (role, false) if !role.is_empty() => {
            format!("the page has no {role} named \"{label}\" — take a fresh snapshot and check the name")
        }
        (_, false) => format!(
            "the page has no element named \"{label}\" — take a fresh snapshot and check the name"
        ),
        _ => "the page has no element matching this target".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::AriaNodeLine;

    fn line(ref_id: usize, role: &str, name: &str) -> AriaNodeLine {
        AriaNodeLine {
            ref_id,
            role: role.to_string(),
            name: name.to_string(),
            value: None,
            indent: 0,
        }
    }

    fn snapshot(lines: Vec<AriaNodeLine>) -> AriaSnapshot {
        AriaSnapshot {
            url: "https://example.test".to_string(),
            title: "Test".to_string(),
            lines,
            truncated: false,
            total_chars: 0,
        }
    }

    #[test]
    fn a_role_and_exact_name_pick_the_element() {
        let snap = snapshot(vec![
            line(1, "button", "Cancel"),
            line(2, "button", "Sign in"),
            line(3, "link", "Sign in"),
        ]);
        assert_eq!(find_ref(&snap, &PageTarget::new("button", Some("Sign in".into()))), Some(2));
    }

    #[test]
    fn the_desktop_and_page_vocabularies_are_reconciled() {
        let snap = snapshot(vec![line(4, "textbox", "Email")]);
        assert_eq!(find_ref(&snap, &PageTarget::new("text field", Some("Email".into()))), Some(4));

        let snap = snapshot(vec![line(5, "radio", "Express shipping")]);
        assert_eq!(
            find_ref(&snap, &PageTarget::new("radio button", Some("Express shipping".into()))),
            Some(5)
        );

        let snap = snapshot(vec![line(6, "checkbox", "Remember me")]);
        assert_eq!(
            find_ref(&snap, &PageTarget::new("check box", Some("Remember me".into()))),
            Some(6)
        );
    }

    #[test]
    fn a_name_that_does_not_appear_is_not_a_match() {
        // The wrong answer here is a plausible click on a different control.
        let snap = snapshot(vec![line(1, "button", "Cancel"), line(2, "button", "Delete")]);
        assert_eq!(
            find_ref(&snap, &PageTarget::new("button", Some("Delete account".into()))),
            None
        );
    }

    #[test]
    fn a_name_that_is_contained_in_the_element_name_still_matches() {
        let snap = snapshot(vec![line(9, "button", "Delete account permanently")]);
        assert_eq!(find_ref(&snap, &PageTarget::new("", Some("Delete account".into()))), Some(9));
    }

    #[test]
    fn a_label_only_target_matches_the_name_across_roles() {
        let snap = snapshot(vec![line(7, "link", "Settings")]);
        assert_eq!(find_ref(&snap, &PageTarget::new("", Some("Settings".into()))), Some(7));
    }

    #[test]
    fn a_label_without_a_ref_is_not_acted_on() {
        // The snapshot lists a label with the field's name but no ref: it can
        // be read, not clicked, and ref 0 must never reach the page.
        let snap = snapshot(vec![line(0, "textbox", "Email"), line(4, "textbox", "Email")]);
        assert_eq!(find_ref(&snap, &PageTarget::new("text field", Some("Email".into()))), Some(4));
    }

    #[test]
    fn a_role_only_target_matches_that_role() {
        let snap = snapshot(vec![line(3, "link", "Home"), line(8, "tab", "Inbox")]);
        assert_eq!(find_ref(&snap, &PageTarget::new("tab", None)), Some(8));
    }
}
