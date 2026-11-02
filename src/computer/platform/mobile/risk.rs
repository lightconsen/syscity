//! What a tap is about to do, before it does it.
//!
//! A phone agent's worst failure is not a wrong screen: it is the right screen
//! with the wrong button under the finger — send, submit, pay, delete. The
//! accessibility tree already names every element, so the cheapest way to know
//! is to read the label of the element being tapped and decide whether the
//! action can be taken back.
//!
//! # What this is not
//!
//! It is a word list, not an understanding of the screen. It can be wrong in
//! both directions, and it is tuned to be wrong toward asking rather than
//! toward acting:
//!
//! - a hit is reported with the word that caused it, so a wrong refusal is
//!   traceable instead of mysterious;
//! - `submit` is the weakest kind: it is the only one a deployment may choose
//!   not to ask about, so "提交订单" is classified as an *order* by a compound
//!   rule rather than falling through to it;
//! - words that describe a *status* rather than an action are excluded
//!   ("待付款", "付款成功", "订单列表"), which is the difference between a
//!   confirmation prompt that fires on the payment button and one that fires
//!   every time an order page loads.
//!
//! The labels are matched case-insensitively against an element's text and
//! content description. Coordinates alone carry no label, so a raw-coordinate
//! tap is classified from the element whose bounds contain it — the same thing
//! the user sees under their finger.

use super::android::UiElement;

/// What kind of action is about to happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Reaches another person: sending a message, posting, replying.
    Send,
    /// Commits a form that was being filled in.
    Submit,
    /// Moves money.
    Pay,
    /// Removes something.
    Delete,
    /// Places an order or buys.
    Order,
}

impl Risk {
    /// The word a person would use for it, for the approval prompt.
    pub fn label(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Submit => "submit",
            Self::Pay => "pay",
            Self::Delete => "delete",
            Self::Order => "order",
        }
    }

    /// Whether a human should be asked before this runs.
    ///
    /// Every kind reaches past the phone: money leaves an account, a message
    /// reaches someone who cannot be unsent to, a delete is gone. The one that
    /// is merely hard to undo on the phone itself — a submitted form — is the
    /// one a deployment may choose to wave through.
    pub fn asks_by_default(self) -> bool {
        !matches!(self, Self::Submit)
    }
}

/// A classification, with the word that caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub risk: Risk,
    /// The label as it appeared on screen. Quoted back in the prompt so a
    /// refusal can be argued with.
    pub matched: String,
    /// Where it matched: the element's label, plus `At (x, y)` for a tap on raw
    /// coordinates.
    pub element: Option<String>,
}

impl Offer {
    /// One line for the approval request.
    pub fn describe(&self) -> String {
        let place = self
            .element
            .as_deref()
            .map(|label| format!(" on \"{label}\""))
            .unwrap_or_default();
        format!(
            "this looks like a {} action (matched \"{}\"{place})",
            self.risk.label(),
            self.matched
        )
    }
}

/// Words that name the action on an element that is a button.
///
/// Kept short on purpose. Every entry here is a word that appears on a button
/// whose press cannot be taken back; a word that is also a menu entry or a
/// counter ("评论 23") is handled by the rules in [`classify_label`], not by
/// leaving it out.
const WORDS: &[(&str, Risk)] = &[
    // Paying. 支付/付款 alone miss 立即付款 and 去支付, so both the verb and
    // the compound are listed.
    ("支付", Risk::Pay),
    ("付款", Risk::Pay),
    ("转账", Risk::Pay),
    ("打赏", Risk::Pay),
    ("缴费", Risk::Pay),
    ("充值", Risk::Pay),
    ("提现", Risk::Pay),
    ("pay", Risk::Pay),
    ("buy now", Risk::Order),
    ("购买", Risk::Order),
    ("下单", Risk::Order),
    ("结算", Risk::Order),
    ("checkout", Risk::Order),
    ("place order", Risk::Order),
    // Removing.
    ("删除", Risk::Delete),
    ("撤回", Risk::Delete),
    ("注销", Risk::Delete),
    ("退出群聊", Risk::Delete),
    ("delete", Risk::Delete),
    ("remove", Risk::Delete),
    // Reaching someone.
    ("发送", Risk::Send),
    ("发布", Risk::Send),
    ("发表", Risk::Send),
    ("转发", Risk::Send),
    ("send", Risk::Send),
    ("post", Risk::Send),
    ("publish", Risk::Send),
    ("reply", Risk::Send),
    // Committing a form. Exact labels only — see [`classify_label`].
    ("提交", Risk::Submit),
    ("submit", Risk::Submit),
];

/// Whole labels that would otherwise be read as something weaker.
///
/// "提交订单" ends in 订单, which [`is_status`] reads as a place — but it is the
/// button that places the order, and classifying it as a plain `submit` would
/// wave it through wherever submits are configured to. Checked before the
/// status rule for exactly that reason.
const COMPOUNDS: &[(&str, Risk)] = &[
    ("提交订单", Risk::Order),
    ("确认订单", Risk::Order),
    ("确认下单", Risk::Order),
    ("confirm order", Risk::Order),
    ("confirm payment", Risk::Pay),
];

/// Endings that turn an action word into a status or a place.
///
/// "付款成功" is the receipt, "待付款" is a to-do badge, "付款方式" is a picker.
/// None of them is a button that takes money, and all three are far more common
/// on a screen than the button itself.
const STATUS_SUFFIXES: &[&str] = &[
    "成功", "失败", "记录", "明细", "方式", "列表", "中心", "入口", "状态", "详情", "设置", "订单",
    "历史", "信息", "管理",
];

/// Prefixes that make a status: "待付款", "已发送".
const STATUS_PREFIXES: &[char] = &['待', '已'];

/// Words that name the action but also name a place, so they count only when
/// the label is the word itself — plus, at most, a count: "评论" and
/// "评论(23)" are the reply button, "评论列表" is the place. The count form is
/// the reason these need their own rule instead of a plain `contains`.
const ENTRY_WORDS: &[(&str, Risk)] = &[("评论", Risk::Send), ("comment", Risk::Send)];

/// Classify one element's label.
///
/// Returns the first rule that matches, strongest first: a compound that would
/// otherwise read as a status, then the status exclusion, then the ambiguous
/// words in [`ENTRY_WORDS`], then the verbs in [`WORDS`].
pub fn classify_label(label: &str) -> Option<(Risk, String)> {
    let text = label.trim();
    if text.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();

    // Labels that are actions despite looking like a status.
    for (compound, risk) in COMPOUNDS {
        if lower.contains(compound) {
            return Some((*risk, text.to_string()));
        }
    }

    // A status is not an action. Checked before the verbs so "付款成功" does not
    // read as a pay button.
    if is_status(text) {
        return None;
    }

    // "评论(23)" is the reply button on a feed, not a tab — the count does not
    // change what a press does. "评论列表" does, and is not classified.
    for (word, risk) in ENTRY_WORDS {
        if let Some(rest) = lower.strip_prefix(word) {
            let rest = rest.trim();
            let only_count = rest.is_empty()
                || rest
                    .trim_start_matches(|c: char| c == '(' || c == '（' || c.is_whitespace())
                    .trim_end_matches(|c: char| c == ')' || c == '）' || c.is_whitespace())
                    .chars()
                    .all(|c| c.is_ascii_digit());
            if only_count {
                return Some((*risk, text.to_string()));
            }
            return None;
        }
    }

    for (word, risk) in WORDS {
        if lower.contains(word) {
            return Some((*risk, word.to_string()));
        }
    }
    None
}

/// Whether a label describes a state instead of an action.
fn is_status(text: &str) -> bool {
    let first = text.chars().next();
    if first.is_some_and(|c| STATUS_PREFIXES.contains(&c)) {
        return true;
    }
    STATUS_SUFFIXES.iter().any(|suffix| text.ends_with(suffix))
}

/// Classify a tap by the element that will receive it.
///
/// `index` is the 1-based element index the caller passed, when it passed one.
/// With coordinates only, the element containing the point stands in for it: a
/// tap lands on whatever is under it, and that is the thing to classify.
pub fn classify_tap(elements: &[UiElement], index: Option<usize>) -> Option<Offer> {
    if let Some(index) = index {
        let element = elements.get(index.checked_sub(1)?)?;
        return classify_element(element);
    }
    None
}

/// Classify a tap at raw coordinates against the tree read just before it.
pub fn classify_point(elements: &[UiElement], x: i32, y: i32) -> Option<Offer> {
    // The smallest element containing the point: a button inside a card inside
    // a scroll view shares the point, and the button is what is pressed.
    let target = elements
        .iter()
        .filter(|element| {
            let (x1, y1, x2, y2) = element.bounds;
            x >= x1 && y >= y1 && x <= x2 && y <= y2
        })
        .min_by_key(|element| {
            let (x1, y1, x2, y2) = element.bounds;
            (x2 - x1) as i64 * (y2 - y1) as i64
        })?;
    classify_element(target)
}

/// Classify a label that arrived without an element (a dialog button, say).
pub fn classify_named(label: &str) -> Option<Offer> {
    let (risk, matched) = classify_label(label)?;
    Some(Offer {
        risk,
        matched,
        element: Some(label.trim().to_string()),
    })
}

fn classify_element(element: &UiElement) -> Option<Offer> {
    let label = element
        .text
        .as_deref()
        .or(element.content_desc.as_deref())
        .unwrap_or("")
        .trim();
    let (risk, matched) = classify_label(label)?;
    Some(Offer {
        risk,
        matched,
        element: Some(label.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(text: &str, bounds: (i32, i32, i32, i32)) -> UiElement {
        UiElement::new(
            1,
            Some(text.to_string()),
            None,
            None,
            "Button".to_string(),
            true,
            true,
            bounds,
        )
    }

    fn kind(label: &str) -> Option<Risk> {
        classify_label(label).map(|(risk, _)| risk)
    }

    #[test]
    fn a_send_button_is_a_send() {
        assert_eq!(kind("发送"), Some(Risk::Send));
        assert_eq!(kind("Send"), Some(Risk::Send));
    }

    #[test]
    fn what_was_matched_is_reported() {
        // The point of quoting it back: a wrong refusal can be argued with.
        let (risk, matched) = classify_label("立即发送").expect("hit");
        assert_eq!(risk, Risk::Send);
        assert_eq!(matched, "发送");
    }

    #[test]
    fn a_payment_button_is_a_payment() {
        assert_eq!(kind("立即付款"), Some(Risk::Pay));
        assert_eq!(kind("去支付"), Some(Risk::Pay));
        assert_eq!(kind("Pay now"), Some(Risk::Pay));
    }

    #[test]
    fn a_status_is_not_a_button() {
        // These are the labels that appear on an order the user is only
        // looking at. Treating them as buttons would ask for permission to
        // read a page.
        assert_eq!(kind("待付款"), None);
        assert_eq!(kind("已付款"), None);
        assert_eq!(kind("付款成功"), None);
        assert_eq!(kind("订单明细"), None);
        assert_eq!(kind("付款方式"), None);
    }

    #[test]
    fn a_count_does_not_change_what_a_press_does() {
        // "评论(23)" is the reply button on a feed; the count is decoration.
        // "评论列表" navigates instead, and is left alone.
        assert_eq!(kind("评论(23)"), Some(Risk::Send));
        assert_eq!(kind("评论（12）"), Some(Risk::Send));
        assert_eq!(kind("评论"), Some(Risk::Send));
        assert_eq!(kind("评论列表"), None);
        assert_eq!(kind("Comment (5)"), Some(Risk::Send));
    }

    #[test]
    fn an_order_compound_is_an_order_not_a_submit() {
        // 提交订单 ends in a status suffix, so without the compound rule it
        // would fall through to the plain 提交 verb and be classified as the
        // kind a deployment is allowed to wave through.
        assert_eq!(kind("提交订单"), Some(Risk::Order));
        assert_eq!(kind("Confirm payment"), Some(Risk::Pay));
    }

    #[test]
    fn a_plain_submit_is_a_submit() {
        assert_eq!(kind("提交"), Some(Risk::Submit));
        assert_eq!(kind("Submit"), Some(Risk::Submit));
        assert_eq!(kind("提交反馈"), Some(Risk::Submit));
    }

    #[test]
    fn a_button_nested_in_a_card_is_what_is_pressed() {
        let card = element("Card", (0, 0, 1080, 800));
        let button = element("删除", (800, 700, 1050, 790));
        let hit = classify_point(&[card, button], 900, 750).expect("hit");
        assert_eq!(hit.risk, Risk::Delete);
        assert_eq!(hit.element.as_deref(), Some("删除"));
    }

    #[test]
    fn a_point_on_nothing_is_no_risk_rather_than_a_guess() {
        let card = element("Card", (0, 0, 100, 100));
        assert_eq!(classify_point(&[card], 900, 750), None);
    }

    #[test]
    fn an_index_points_at_that_element() {
        let elements = vec![
            element("Home", (0, 0, 100, 100)),
            element("Ignore", (0, 100, 100, 200)),
        ];
        assert_eq!(classify_tap(&elements, Some(1)), None);
        assert_eq!(classify_tap(&elements, Some(0)), None, "indices are 1-based");
        assert_eq!(classify_tap(&elements, Some(9)), None);
    }

    #[test]
    fn the_description_reads_like_a_sentence() {
        let (risk, matched) = classify_label("立即付款").expect("hit");
        let offer = Offer {
            risk,
            matched,
            element: Some("立即付款".to_string()),
        };
        assert!(offer.describe().contains("pay"));
        assert!(offer.describe().contains("立即付款"));
    }

    #[test]
    fn money_and_orders_ask_deleting_and_sending_ask_submitting_may_not() {
        for risk in [Risk::Pay, Risk::Order, Risk::Delete, Risk::Send] {
            assert!(risk.asks_by_default(), "{risk:?} should ask");
        }
        assert!(!Risk::Submit.asks_by_default());
    }
}
