//! The page rung: an action the desktop could not deliver, performed on the
//! document instead.
//!
//! A click on a web page goes through the same ladder as any other click —
//! accessibility action, pixel, keyboard. When all three leave the page
//! unchanged, the remaining honest option is not a fourth blind click: it is
//! to ask the page itself, through the browser's own protocol, which element
//! the caller meant and to act on that element directly.
//!
//! This module defines only the seam. The session knows *what* it wanted
//! (a role and a name, the way the accessibility tree spelled them); the
//! browser side knows how to find that element in the DOM and act on it. The
//! two meet here so the computer module does not depend on the browser.

use crate::computer::ActionResult;

/// What the ladder asks the page to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageAction {
    /// Click the element once.
    Click,
    /// Replace the element's value with this text.
    Type(String),
}

/// The element, named the way a page names it.
///
/// `role` is the accessibility role the desktop tree reported ("button",
/// "text field"); `label` is its accessible name. An empty role means "match
/// on the name alone".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageTarget {
    pub role: String,
    pub label: Option<String>,
}

impl PageTarget {
    pub fn new(role: impl Into<String>, label: Option<String>) -> Self {
        Self { role: role.into(), label }
    }

    /// Whether anything was named at all — a target with neither a role nor a
    /// name cannot be looked up, and asking the page would only guess.
    pub fn is_addressable(&self) -> bool {
        !self.role.trim().is_empty() || self.label.as_deref().is_some_and(|l| !l.trim().is_empty())
    }
}

/// Act on a document through the browser.
///
/// Implementations are expected to refuse rather than guess: returning an
/// error means "the page has no such element, or no page is attached", and a
/// guess could click a different control than the caller described.
#[async_trait::async_trait]
pub trait PageActor: Send + Sync {
    /// Perform `action` on the element `target` names, in the page belonging
    /// to `owner` (a conversation id, the way the browser tool scopes tabs).
    async fn act(
        &self,
        owner: &str,
        target: &PageTarget,
        action: PageAction,
    ) -> crate::computer::Result<ActionResult>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_or_a_name_makes_a_target_addressable() {
        assert!(PageTarget::new("button", None).is_addressable());
        assert!(PageTarget::new("", Some("Sign in".to_string())).is_addressable());
        assert!(!PageTarget::new("", None).is_addressable());
        assert!(!PageTarget::new("  ", Some("  ".to_string())).is_addressable());
    }
}
