//! The per-app boundary for desktop actions (`[computer] allow_apps`).
//!
//! A desktop automation grant is a grant on *the whole screen*, and the screen
//! holds the user's bank as well as their browser. A permission rule can only
//! look at what a call names, so `[permissions].deny = ["computer:Safari"]`
//! stops a call that says "Safari" and nothing else; the boundary that has to
//! hold against a model that never says the app's name is this one.
//!
//! The rule is deliberately one sentence long: **when the list is non-empty,
//! only the named apps may be acted on.** Two consequences follow:
//!
//! - An action that names a *different* app is refused before anything is
//!   dispatched — not after, so nothing was delivered and nothing has to be
//!   undone.
//! - An action that names *no* app — a coordinate click, a bare key press,
//!   "type into whatever holds focus" — is the case the boundary is really
//!   for: the event reaches whatever is in front, which is not necessarily
//!   anything the caller aimed at. Such an action is allowed only when the
//!   platform can say *which app would receive it* and that app is on the
//!   list; when the receiving app cannot be established, the answer is still
//!   no. That is [`refusal_reaching`](AppAllowlist::refusal_reaching).
//!
//! This is enforced in our own code, before the adapter is asked to act. It is
//! not a sandbox: the operating system still lets this process move the pointer
//! anywhere. What it does cover now is an action that names an allowed app but
//! would land in someone else's window: which app an action *names* is judged
//! before dispatch, and which app would *receive* a point or a keystroke is
//! judged per rung, before that rung delivers anything — a mismatch is refused
//! with the receiving app named. What it still does not cover is a *window* of
//! an allowed app hosting content from another app (a web view, an embedded
//! browser) — that is app-level granularity, not window-level, and is noted in
//! the plan's §6.6.4.
//!
//! The receiving answer is also a reading of a live screen, taken moments
//! before the event: focus or stacking can change in between. It narrows the
//! window in which an unintended receiver can be hit; it does not close it,
//! which is what "not a sandbox" means here.

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

    /// Whether `app` may be acted on — the same question as [`refusal`], in
    /// the shape a caller asks when it already knows the answer is a name.
    pub fn allows(&self, app: &str) -> bool {
        self.refusal(Some(app)).is_none()
    }

    /// Why an action that names no app may not run, given the app that would
    /// receive it.
    ///
    /// This is the second half of the boundary. [`refusal`](Self::refusal)
    /// judges a *claimed* target — what the caller said it meant. This judges
    /// the *receiving* app — who would actually get the event — and it is what
    /// turns "a coordinate click reaches whatever is in front, so it cannot be
    /// shown to stay inside the list" from a constant refusal into a check:
    /// when the screen says Safari is in front, a click at a point is an
    /// action on Safari, and a list naming Safari may allow it.
    ///
    /// The asymmetry with [`refusal`](Self::refusal) is deliberate. There,
    /// `None` (no app named) is a refusal because nothing establishes where
    /// the action goes. Here the caller has already established where it goes;
    /// passing `None` means the platform could not answer that question, and
    /// the honest verdict for that is the old one — refuse, and say why.
    pub fn refusal_reaching(&self, receiving: Option<&str>) -> Option<String> {
        let entries = self
            .entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if entries.is_empty() {
            return None;
        }
        let listed = entries.join(", ");
        match receiving {
            Some(app) if entries.iter().any(|entry| matches(entry, app)) => None,
            Some(app) => Some(format!(
                "this action names no application, and the one that would receive it — {app:?} — \
                 is not on the [computer] allow_apps list ({listed}); the action was not sent"
            )),
            None => Some(format!(
                "this action names no application, and the platform could not say which one would \
                 receive it, so it cannot be shown to stay inside [computer] allow_apps \
                 ({listed}). Target an element in an allowed app instead; the action was not sent"
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
    fn a_receiving_app_on_the_list_passes() {
        let allow = AppAllowlist::new(vec!["Safari".to_string()]);
        assert_eq!(allow.refusal_reaching(Some("Safari")), None);
        assert_eq!(allow.refusal_reaching(Some("safari")), None);
        assert!(allow.allows("Safari"));
    }

    #[test]
    fn a_receiving_app_off_the_list_is_named_in_the_refusal() {
        let allow = AppAllowlist::new(vec!["Safari".to_string()]);
        let refusal = allow
            .refusal_reaching(Some("1Password"))
            .expect("a refusal");
        assert!(refusal.contains("1Password"), "the receiving app is named: {refusal}");
        assert!(refusal.contains("Safari"), "the list is named: {refusal}");
        assert!(refusal.contains("was not sent"), "{refusal}");
        assert!(!allow.allows("1Password"));
    }

    /// The two questions differ in exactly one answer: the one the platform
    /// could not give. A claim that names no app is refused; a receiver the
    /// screen cannot establish is also refused, but says *that* is why.
    #[test]
    fn an_unknown_receiver_is_refused_for_its_own_reason() {
        let allow = AppAllowlist::new(vec!["Safari".to_string()]);
        let claim = allow.refusal(None).expect("a refusal");
        let receiver = allow.refusal_reaching(None).expect("a refusal");
        assert!(receiver.contains("could not say"), "{receiver}");
        assert!(receiver.contains("Safari"), "the list is named: {receiver}");
        assert_ne!(claim, receiver, "the two refusals explain different things");
    }

    #[test]
    fn no_list_never_refuses_a_receiver() {
        let allow = AppAllowlist::default();
        assert_eq!(allow.refusal_reaching(Some("1Password")), None);
        assert_eq!(allow.refusal_reaching(None), None);
        assert!(allow.allows("anything"));
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
