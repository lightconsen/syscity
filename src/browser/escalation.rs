//! The one path out of the page: what the browser tool cannot reach, and what
//! to do about it.
//!
//! This is not a fallback. Nothing here acts, and nothing detects the cases it
//! names — a detector would have to guess from inside the page, and the obvious
//! guess is wrong exactly where it matters (a styled upload button forwards its
//! click to a hidden input, so the element under the point is the button, not
//! the input). What a driver *can* do is make the escalation explicit: name what
//! it cannot reach, hand over the identity of the window that would have to be
//! operated, and say that doing so is a takeover of a desktop someone may be
//! using.
//!
//! The desktop is treated as shared, not owned. That is why the request carries
//! a consent step rather than an instruction to proceed.

/// Why the page is not enough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// A dialog owned by the operating system: a file picker, a print dialog, a
    /// save prompt.
    NativeDialog,
    /// The browser's own furniture: address bar, menus, extension UI, the
    /// downloads shelf.
    BrowserChrome,
    /// An HTTP authentication prompt, which the browser draws outside the page.
    HttpAuth,
    /// Something else the page cannot be asked to do.
    Other,
}

impl Reason {
    /// Parse the caller's word for why it is stuck.
    pub fn parse(name: &str) -> Result<Self, String> {
        match name.trim().to_lowercase().as_str() {
            "native_dialog" => Ok(Reason::NativeDialog),
            "browser_chrome" => Ok(Reason::BrowserChrome),
            "http_auth" => Ok(Reason::HttpAuth),
            "other" => Ok(Reason::Other),
            other => Err(format!(
                "unknown escalation reason `{other}` — use native_dialog, browser_chrome, \
                 http_auth or other"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Reason::NativeDialog => "native_dialog",
            Reason::BrowserChrome => "browser_chrome",
            Reason::HttpAuth => "http_auth",
            Reason::Other => "other",
        }
    }

    /// What this tool cannot do about it, in the caller's terms.
    pub fn cannot_reach(self) -> &'static str {
        match self {
            Reason::NativeDialog => {
                "native operating-system dialogs — a file picker, a print dialog, a save prompt. \
                 They belong to the desktop, not to the page, so nothing here can see or dismiss \
                 them."
            }
            Reason::BrowserChrome => {
                "the browser's own chrome — the address bar and its menus, extension UI, and the \
                 downloads shelf. They are outside the rendered page."
            }
            Reason::HttpAuth => {
                "an HTTP authentication prompt. The browser draws it outside the page, and this \
                 tool has no action that answers one."
            }
            Reason::Other => "whatever is in the way here. This tool can only act on the page.",
        }
    }

    /// The in-page route that should be tried first, when one exists.
    ///
    /// Escalating should not be a way around an answer that already exists: a
    /// file picker and a print dialog both have actions that avoid them
    /// entirely.
    pub fn instead_try(self) -> Option<&'static str> {
        match self {
            Reason::NativeDialog => Some(
                "If the dialog is a file picker, UploadFiles attaches files to a file input \
                 without opening one. If it is a print dialog, PrintToPdf prints without one. If \
                 it is a save prompt, ask for a download path with SetDownloadBehavior instead of \
                 answering the prompt.",
            ),
            Reason::BrowserChrome => Some(
                "For another tab, ListTabs and SwitchTab reach it without the chrome. Downloads \
                 that were merely hard to find are SetDownloadBehavior's business.",
            ),
            // Nothing in the page answers an auth prompt, and this tool is not
            // going to supply credentials.
            Reason::HttpAuth => None,
            Reason::Other => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reason_words_are_a_closed_set() {
        assert_eq!(Reason::parse("native_dialog").unwrap(), Reason::NativeDialog);
        assert_eq!(Reason::parse(" Native_Dialog ").unwrap(), Reason::NativeDialog);
        assert_eq!(Reason::parse("browser_chrome").unwrap(), Reason::BrowserChrome);
        assert_eq!(Reason::parse("http_auth").unwrap(), Reason::HttpAuth);
        assert_eq!(Reason::parse("other").unwrap(), Reason::Other);
    }

    #[test]
    fn a_reason_that_is_not_one_is_refused_with_the_list() {
        let refused = Reason::parse("dialog_blocked").unwrap_err();
        assert!(refused.contains("dialog_blocked"), "{refused}");
        assert!(refused.contains("native_dialog"), "{refused}");
    }

    #[test]
    fn every_reason_says_what_cannot_be_reached() {
        for reason in [
            Reason::NativeDialog,
            Reason::BrowserChrome,
            Reason::HttpAuth,
            Reason::Other,
        ] {
            let said = reason.cannot_reach();
            assert!(said.len() > 20, "{reason:?} says too little: {said}");
            // A refusal that does not name a cause is the thing this replaces.
            assert!(!said.contains("unknown"), "{reason:?}");
        }
    }

    #[test]
    fn the_reasons_with_an_in_page_answer_point_at_it() {
        // Escalation must not become a shortcut past an action that works.
        let file = Reason::NativeDialog.instead_try().unwrap();
        assert!(file.contains("UploadFiles"), "{file}");
        assert!(file.contains("PrintToPdf"), "{file}");

        let chrome = Reason::BrowserChrome.instead_try().unwrap();
        assert!(chrome.contains("SwitchTab"), "{chrome}");
    }

    #[test]
    fn an_auth_prompt_has_no_in_page_answer_to_offer() {
        // Saying "try X instead" here would send the caller after credentials
        // this tool should not be fetching.
        assert!(Reason::HttpAuth.instead_try().is_none());
        assert!(Reason::Other.instead_try().is_none());
    }
}
