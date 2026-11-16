//! The per-app boundary for desktop actions (`[computer] allow_apps`).
//!
//! A desktop automation grant is a grant on *the whole screen*, and the screen
//! holds the user's bank as well as their browser. A permission rule can only
//! look at what a call names, so `[permissions].deny = ["computer:Safari"]`
//! stops a call that says "Safari" and nothing else; the boundary that has to
//! hold against a model that never says the app's name is this one.
//!
//! The rule is deliberately one sentence long: **when the list is non-empty,
//! only the named apps may be acted on.** Two consequences follow, and both are
//! the point rather than a rough edge:
//!
//! - An action that names a *different* app is refused before anything is
//!   dispatched — not after, so nothing was delivered and nothing has to be
//!   undone.
//! - An action that names *no* app is refused too. A coordinate click, a bare
//!   key press and "type into whatever holds focus" reach whatever happens to
//!   be in front, so they are exactly the actions an allowlist exists to stop.
//!   "Unknown" is not a member of the list.
//!
//! This is enforced in our own code, before the adapter is asked to act. It is
//! not a sandbox: the operating system still lets this process move the pointer
//! anywhere, and an action that both names an allowed app and lands on top of
//! another window is not caught here. Isolation is a different (and, for this
//! product, rejected) design; see the plan's §6.6.4.

use std::sync::{Arc, RwLock};

use crate::hooks::matcher::glob_match;

/// The apps a session may act on, shared with whoever can change the answer.
///
/// Shared rather than copied because the tool's session outlives a `config.set`:
/// a value captured when the session was built would keep the *old* list for as
/// long as the conversation lasts, which is the one lifetime an operator
/// tightening the allowlist most wants covered.
#[derive(Debug, Clone, Default)]
pub struct AppAllowlist {
    entries: Arc<RwLock<Vec<String>>>,
}

impl AppAllowlist {
    /// An allowlist holding `entries`. An empty list allows every app — the
    /// default, and the behaviour of every install that has never set one.
    pub fn new(entries: Vec<String>) -> Self {
        Self {
            entries: Arc::new(RwLock::new(clean(entries))),
        }
    }

    /// Replace the entries (a config change). Blank entries are dropped, and so
    /// is the whole list's meaning if it comes back empty: no list, no
    /// boundary.
    pub fn replace(&self, entries: Vec<String>) {
        *self.entries.write().unwrap_or_else(|e| e.into_inner()) = clean(entries);
    }

    /// The entries, as configured.
    pub fn entries(&self) -> Vec<String> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether the list is empty, which means "no boundary".
    pub fn is_empty(&self) -> bool {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Why acting on `app` is not allowed, when it is not.
    ///
    /// `app` is `None` when the action names no application. A pattern may use
    /// `*` (`"Google Chrome*"`), and matching is case-insensitive because the
    /// spelling of an app name is the platform's, not the operator's: macOS
    /// says `Safari` and Windows says `safari`, and a list that cared about the
    /// difference would refuse the app it names.
    pub fn refusal(&self, app: Option<&str>) -> Option<String> {
        let entries = self
            .entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if entries.is_empty() {
            return None;
        }
        let listed = entries.join(", ");
        match app {
            Some(app) if entries.iter().any(|entry| matches(entry, app)) => None,
            Some(app) => Some(format!(
                "{app:?} is not on the [computer] allow_apps list ({listed}); the action was not \
                 sent"
            )),
            None => Some(format!(
                "this action names no application, and [computer] allow_apps is set ({listed}) — a \
                 coordinate click, a bare key press and typing into whatever holds focus reach \
                 whatever is in front, so none of them can be shown to stay inside the list. \
                 Target an element in an allowed app instead; the action was not sent"
            )),
        }
    }
}

fn clean(entries: Vec<String>) -> Vec<String> {
    entries
        .into_iter()
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect()
}

fn matches(entry: &str, app: &str) -> bool {
    glob_match(&entry.to_lowercase(), &app.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_list_has_no_opinion() {
        let allow = AppAllowlist::default();
        assert!(allow.is_empty());
        assert_eq!(allow.refusal(Some("1Password")), None);
        // Even an action that names nothing is fine: there is no boundary.
        assert_eq!(allow.refusal(None), None);
    }

    #[test]
    fn a_listed_app_passes_and_another_is_refused() {
        let allow = AppAllowlist::new(vec!["Safari".to_string()]);
        assert_eq!(allow.refusal(Some("Safari")), None);
        let refusal = allow.refusal(Some("1Password")).expect("a refusal");
        assert!(refusal.contains("1Password"), "{refusal}");
        assert!(refusal.contains("Safari"), "the refusal names the list: {refusal}");
    }

    #[test]
    fn an_action_with_no_app_is_refused_rather_than_assumed() {
        let allow = AppAllowlist::new(vec!["Safari".to_string()]);
        let refusal = allow.refusal(None).expect("a refusal");
        assert!(refusal.contains("names no application"), "{refusal}");
        assert!(
            refusal.contains("was not sent"),
            "the refusal says nothing was delivered: {refusal}"
        );
    }

    #[test]
    fn spelling_is_the_platforms_and_matching_is_ours() {
        let allow = AppAllowlist::new(vec!["safari".to_string(), "Google Chrome*".to_string()]);
        // macOS spells it with a capital; the list must not care.
        assert_eq!(allow.refusal(Some("Safari")), None);
        assert_eq!(allow.refusal(Some("Google Chrome Canary")), None);
        assert!(allow.refusal(Some("Chromium")).is_some());
    }

    #[test]
    fn a_blank_entry_does_not_become_a_boundary() {
        let allow = AppAllowlist::new(vec!["  ".to_string(), String::new()]);
        assert!(allow.is_empty());
        assert_eq!(allow.refusal(Some("anything")), None);
    }

    #[test]
    fn replacing_the_list_takes_effect_for_everyone_holding_it() {
        let allow = AppAllowlist::new(vec!["Safari".to_string()]);
        let shared = allow.clone();
        assert_eq!(shared.refusal(Some("Safari")), None);
        allow.replace(vec!["Finder".to_string()]);
        assert!(shared.refusal(Some("Safari")).is_some(), "the clone sees the new list");
        assert_eq!(shared.refusal(Some("Finder")), None);
    }
}
