//! Finding a control on the current screen, and waiting for one to appear.
//!
//! Two questions the model asks constantly, and answered today only by reading
//! the whole tree and counting rows itself (`android_query`) or by calling
//! `android_observe` again a second later (`android_wait`). Both are cheap
//! reads of the same tree the rest of the set uses; what they add is that the
//! *tool* does the looking, so the answer is one line instead of several
//! thousand characters of context.
//!
//! This module is the pure part — which element matches, and whether a reading
//! satisfies a condition — so the polling loop in the tools can be driven
//! without a device and the rules can be tested directly.

use super::android::{element_label, UiElement};

/// What to look for in the tree.
///
/// Every field is a case-insensitive substring, and at least one must be set.
/// Substrings rather than exact matches because a real label carries decoration
/// ("发送 (2)", "评论 23") that the model should not have to reproduce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    text: Option<String>,
    resource_id: Option<String>,
    class: Option<String>,
}

impl Query {
    /// Build a query, or `None` when no criterion was given.
    ///
    /// An empty string counts as "not given": a model that passes `text: ""`
    /// means "any", and matching every element on the screen would be a much
    /// worse answer than saying the argument is missing.
    pub fn new(text: Option<&str>, resource_id: Option<&str>, class: Option<&str>) -> Option<Self> {
        let clean = |value: Option<&str>| {
            value
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_lowercase)
        };
        let query = Self {
            text: clean(text),
            resource_id: clean(resource_id),
            class: clean(class),
        };
        if query.text.is_none() && query.resource_id.is_none() && query.class.is_none() {
            return None;
        }
        Some(query)
    }

    /// Whether this element matches every criterion that was given.
    pub fn matches(&self, element: &UiElement) -> bool {
        let text_hit = self.text.as_deref().is_none_or(|needle| {
            element_label(element).is_some_and(|label| label.to_lowercase().contains(needle))
        });
        let id_hit = self.resource_id.as_deref().is_none_or(|needle| {
            element
                .resource_id
                .as_deref()
                .is_some_and(|id| id.to_lowercase().contains(needle))
        });
        let class_hit = self
            .class
            .as_deref()
            .is_none_or(|needle| element.class.to_lowercase().contains(needle));
        text_hit && id_hit && class_hit
    }

    /// How many elements match.
    pub fn count(&self, elements: &[UiElement]) -> usize {
        elements
            .iter()
            .filter(|element| self.matches(element))
            .count()
    }

    /// The first `limit` matches, in tree order.
    pub fn find<'a>(&self, elements: &'a [UiElement], limit: usize) -> Vec<&'a UiElement> {
        elements
            .iter()
            .filter(|element| self.matches(element))
            .take(limit)
            .collect()
    }
}

/// What to wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// Something matching the query is on screen.
    Appear,
    /// Nothing matching the query is on screen any more.
    Disappear,
    /// Two readings in a row describe the same screen: whatever was happening
    /// has stopped.
    Stable,
}

impl Condition {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "appear" | "appears" => Ok(Self::Appear),
            "disappear" | "disappears" | "gone" => Ok(Self::Disappear),
            "stable" | "settle" | "settled" => Ok(Self::Stable),
            other => {
                Err(format!("condition must be 'appear', 'disappear' or 'stable', not {other:?}"))
            }
        }
    }

    /// Whether waiting on this condition needs something to look for.
    pub fn needs_query(self) -> bool {
        !matches!(self, Self::Stable)
    }

    /// Whether one reading satisfies the condition.
    ///
    /// `stable` needs the caller to have seen the same fingerprint twice, which
    /// is a fact about a *sequence* of readings; the poll loop passes it in
    /// rather than this function having to hold state.
    pub fn met(self, matches: usize, seen_twice: bool) -> bool {
        match self {
            Self::Appear => matches > 0,
            Self::Disappear => matches == 0,
            Self::Stable => seen_twice,
        }
    }

    /// The word used in the result.
    pub fn label(self) -> &'static str {
        match self {
            Self::Appear => "appear",
            Self::Disappear => "disappear",
            Self::Stable => "stable",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(text: Option<&str>, id: Option<&str>, class: &str) -> UiElement {
        UiElement::new(
            1,
            text.map(str::to_string),
            None,
            id.map(str::to_string),
            class.to_string(),
            true,
            true,
            (0, 0, 10, 10),
        )
    }

    fn query(text: Option<&str>, id: Option<&str>, class: Option<&str>) -> Query {
        Query::new(text, id, class).expect("a criterion")
    }

    #[test]
    fn text_is_a_case_insensitive_substring() {
        let q = query(Some("Send"), None, None);
        assert!(q.matches(&element(Some("Send now"), None, "Button")));
        assert!(q.matches(&element(Some("send"), None, "Button")));
        assert!(!q.matches(&element(Some("Cancel"), None, "Button")));
        assert!(!q.matches(&element(None, None, "Button")));
    }

    #[test]
    fn a_content_description_matches_like_text() {
        let mut button = element(None, None, "Button");
        button.content_desc = Some("发送消息".to_string());
        assert!(query(Some("发送"), None, None).matches(&button));
    }

    #[test]
    fn criteria_are_combined_so_a_class_narrows_a_text_hit() {
        let elements = [
            element(Some("Alice"), Some("com.x:id/avatar"), "ImageView"),
            element(Some("Alice"), Some("com.x:id/name"), "TextView"),
        ];
        assert_eq!(query(Some("alice"), None, None).count(&elements), 2);
        assert_eq!(query(Some("alice"), None, Some("textview")).count(&elements), 1);
        assert_eq!(query(None, Some("name"), None).count(&elements), 1);
    }

    #[test]
    fn an_empty_criterion_is_not_a_criterion() {
        assert_eq!(Query::new(Some(""), None, Some("  ")), None);
        assert_eq!(Query::new(None, None, None), None);
    }

    #[test]
    fn find_stops_at_the_limit_and_keeps_tree_order() {
        let elements: Vec<UiElement> = (0..5)
            .map(|i| element(Some(&format!("row {i}")), None, "TextView"))
            .collect();
        let found = query(Some("row"), None, None).find(&elements, 3);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].text.as_deref(), Some("row 0"));
        assert_eq!(found[2].text.as_deref(), Some("row 2"));
    }

    #[test]
    fn conditions_read_their_own_word() {
        assert_eq!(Condition::parse("appear"), Ok(Condition::Appear));
        assert_eq!(Condition::parse(" Gone "), Ok(Condition::Disappear));
        assert_eq!(Condition::parse("settled"), Ok(Condition::Stable));
        assert!(Condition::parse("blink").is_err());
    }

    #[test]
    fn appear_and_disappear_are_opposites_and_stable_is_its_own_fact() {
        assert!(Condition::Appear.met(1, false));
        assert!(!Condition::Appear.met(0, true));
        assert!(Condition::Disappear.met(0, false));
        assert!(!Condition::Disappear.met(1, true));
        assert!(Condition::Stable.met(0, true));
        assert!(!Condition::Stable.met(3, false));
    }

    #[test]
    fn only_stable_waits_without_something_to_look_for() {
        assert!(Condition::Appear.needs_query());
        assert!(Condition::Disappear.needs_query());
        assert!(!Condition::Stable.needs_query());
    }
}
