//! Reading one screen against the last one, instead of from scratch.
//!
//! A model driving a phone re-reads the screen after almost every action. Each
//! read is a full enumeration — every row of a long list, every time — and the
//! repeated part is the part that carries no information. What changed is the
//! news: a message arrived, a field was filled, a dialog appeared.
//!
//! So a read can name the read it follows (`since`), and then report the delta:
//! what appeared, what left, what moved, and how much stayed the same.
//!
//! Three things make that unsafe to do blindly, and each falls back to the full
//! list rather than guessing:
//!
//! * **No baseline.** The name is a fingerprint of the elements, and only the
//!   last few reads are kept. A name that has been evicted, or that came from
//!   another device, is not a baseline.
//! * **A different app.** Rows that look alike across two apps are a
//!   coincidence; the screens are unrelated.
//! * **More than half the screen changed.** A diff of most of the screen is a
//!   worse description than the screen: the model would be reconstructing the
//!   present from an absent past. Past that share, the full list is short by
//!   comparison to the reconstruction.
//!
//! Element identity is [`super::android::element_key`], which is what survives
//! a screen changing — not the index, which is only a position in one read.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use super::android::UiElement;

/// How many past reads are kept for diffing.
///
/// Six is a conversation's depth, not a screen's: the caller diffs against the
/// read it just acted on, and a handful of turns of slack covers a model that
/// batches its actions.
const HISTORY: usize = 6;

/// A read that a later one can be compared with.
#[derive(Debug, Clone)]
pub struct Baseline {
    pub package: Option<String>,
    pub elements: Vec<UiElement>,
}

/// How a read relates to the one it followed.
#[derive(Debug, Clone)]
pub enum ScreenDiff {
    /// The comparison could be made.
    Diff(UiDelta),
    /// It could not, and the full list is being returned instead.
    Full(FullReason),
}

/// Why a diff was not possible, in the model's terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullReason {
    /// Nothing is stored under that name — it expired, or it came from another
    /// device.
    NoBaseline,
    /// The named read was of a different app.
    AppChanged,
    /// Too much of the screen is different for a delta to be a useful answer.
    MostlyChanged,
}

impl FullReason {
    /// The sentence the result carries when it falls back.
    pub fn describe(self) -> &'static str {
        match self {
            Self::NoBaseline => {
                "There is no read with that name to compare against — only the last few reads \
                 are kept, so it may have expired, or it may be from another device. This is the \
                 whole screen."
            }
            Self::AppChanged => {
                "The app changed since that read, so the two screens are unrelated and are not \
                 compared. This is the whole screen."
            }
            Self::MostlyChanged => {
                "Most of the screen is different from that read — a delta would describe the \
                 change worse than the screen itself does. This is the whole screen."
            }
        }
    }
}

/// What a comparison found.
#[derive(Debug, Default, Clone)]
pub struct UiDelta {
    /// In the new read, not in the old one.
    pub added: Vec<UiElement>,
    /// In the old read, not in the new one. Indices refer to the *old* read.
    pub removed: Vec<UiElement>,
    /// Same identity, different place.
    pub moved: Vec<UiElement>,
    /// Same identity, same place.
    pub unchanged: usize,
    pub before_len: usize,
    pub after_len: usize,
}

impl UiDelta {
    /// Elements that are not the same element in the same place.
    pub fn changed(&self) -> usize {
        self.added.len() + self.removed.len() + self.moved.len()
    }

    /// Whether anything at all moved.
    pub fn is_empty(&self) -> bool {
        self.changed() == 0
    }
}

/// Compare two reads by element identity.
///
/// The same two passes [`super::android::tree_diff`] uses, and for the same
/// reason: pair identical rows in place first, then let the leftovers pair as
/// movement, and only then call the rest gone. One inserted row at the top of a
/// list must read as one arrival, not as every row moving.
pub fn diff_elements(before: &[UiElement], after: &[UiElement]) -> UiDelta {
    let mut delta = UiDelta {
        before_len: before.len(),
        after_len: after.len(),
        ..UiDelta::default()
    };
    let mut unclaimed: HashMap<&str, Vec<&UiElement>> = HashMap::new();
    for element in before.iter().filter(|element| !element.key.is_empty()) {
        unclaimed
            .entry(element.key.as_str())
            .or_default()
            .push(element);
    }

    // Pass one: same identity, same place.
    let mut leftover: Vec<&UiElement> = Vec::new();
    for element in after {
        let previous = unclaimed.get_mut(element.key.as_str()).and_then(|list| {
            let position = list
                .iter()
                .position(|other| other.bounds == element.bounds)?;
            Some(list.swap_remove(position))
        });
        match previous {
            Some(_) => delta.unchanged += 1,
            None => leftover.push(element),
        }
    }

    // Pass two: same identity, somewhere else.
    for element in leftover {
        match unclaimed
            .get_mut(element.key.as_str())
            .filter(|list| !list.is_empty())
            .map(|list| list.swap_remove(0))
        {
            Some(_) => delta.moved.push(element.clone()),
            None => delta.added.push(element.clone()),
        }
    }

    delta.removed = unclaimed.into_values().flatten().cloned().collect();
    delta
}

/// Store a read so a later one can name it.
pub fn remember(
    device: Option<&str>,
    display: i32,
    fingerprint: u64,
    package: Option<&str>,
    elements: &[UiElement],
) {
    let mut history = history();
    history.push_back((
        key(device, display, fingerprint),
        Baseline {
            package: package.map(str::to_string),
            elements: elements.to_vec(),
        },
    ));
    while history.len() > HISTORY {
        history.pop_front();
    }
}

/// Compare the read about to be returned with the one `since` names.
///
/// `since` is the `snapshot` of an earlier read: the bare fingerprint, in hex,
/// with or without a `0x` prefix. The comparison is deliberately conservative —
/// see [`FullReason`] for each case where the whole list is the better answer.
pub fn compare(
    device: Option<&str>,
    display: i32,
    since: &str,
    package: Option<&str>,
    after: &[UiElement],
) -> ScreenDiff {
    let Some(fingerprint) = parse_since(since) else {
        return ScreenDiff::Full(FullReason::NoBaseline);
    };
    let baseline = {
        let history = history();
        history
            .iter()
            .rev()
            .find(|(stored, _)| *stored == key(device, display, fingerprint))
            .map(|(_, baseline)| baseline.clone())
    };
    let Some(baseline) = baseline else {
        return ScreenDiff::Full(FullReason::NoBaseline);
    };
    if baseline.package.as_deref() != package {
        return ScreenDiff::Full(FullReason::AppChanged);
    }
    let delta = diff_elements(&baseline.elements, after);
    // Twice the changed count against the larger of the two reads: at 50% the
    // reconstruction has stopped being shorter than the screen.
    let largest = delta.before_len.max(delta.after_len);
    if largest > 0 && delta.changed() * 2 > largest {
        return ScreenDiff::Full(FullReason::MostlyChanged);
    }
    ScreenDiff::Diff(delta)
}

/// Read a `since` value as the fingerprint it names.
fn parse_since(since: &str) -> Option<u64> {
    let text = since.trim();
    let text = text.strip_prefix("0x").unwrap_or(text);
    u64::from_str_radix(text, 16).ok()
}

/// The process-wide history.
///
/// Process-wide because a baseline outlives one tool call: the point is to
/// compare this read with the one an earlier call returned. The storage key
/// includes the device and the display, so one phone's screen is never diffed
/// against another's, and the agent's virtual display never against the user's.
fn history() -> std::sync::MutexGuard<'static, VecDeque<(String, Baseline)>> {
    static HISTORY: std::sync::OnceLock<Mutex<VecDeque<(String, Baseline)>>> =
        std::sync::OnceLock::new();
    HISTORY
        .get_or_init(|| Mutex::new(VecDeque::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn key(device: Option<&str>, display: i32, fingerprint: u64) -> String {
    format!("{}|{display}|{fingerprint:x}", device.unwrap_or("local"))
}

/// The `since` argument of the read the caller named, if it named one.
pub fn since_arg(args: &serde_json::Value) -> Option<&str> {
    args.get("since")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|since| !since.is_empty())
}

/// The delta as the model reads it: a sentence, the rows that arrived, and the
/// rows that left.
///
/// Added and removed rows are listed with their index *in their own read*,
/// which is the only index that can address them: an added row's index is valid
/// now, and a removed row's index was valid then. Either way the line is the
/// label a person would have read, which is what the model acts on.
pub fn describe(delta: &UiDelta) -> String {
    if delta.is_empty() {
        return format!(
            "Nothing changed since that read: the same {} element(s), in the same places.",
            delta.after_len
        );
    }
    let mut text = format!(
        "Changed since that read: {} of {} element(s) — {} appeared, {} left, {} moved, {} \
         unchanged.",
        delta.changed(),
        delta.after_len,
        delta.added.len(),
        delta.removed.len(),
        delta.moved.len(),
        delta.unchanged
    );
    if !delta.added.is_empty() {
        text.push_str("\nAppeared:");
        for element in &delta.added {
            text.push('\n');
            text.push_str(&super::android::format_ui_element(element));
        }
    }
    if !delta.removed.is_empty() {
        text.push_str("\nLeft:");
        for element in &delta.removed {
            text.push('\n');
            text.push_str(&super::android::format_ui_element(element));
        }
    }
    if !delta.moved.is_empty() {
        text.push_str("\nMoved:");
        for element in &delta.moved {
            text.push('\n');
            text.push_str(&super::android::format_ui_element(element));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::platform::mobile::android::NodeState;

    fn row(index: usize, text: &str, y: i32) -> UiElement {
        let mut element = UiElement::with_state(
            index,
            Some(text.to_string()),
            None,
            None,
            "TextView".to_string(),
            false,
            true,
            NodeState::default(),
            (0, y, 200, y + 80),
        );
        element.key = format!("k-{text}-{index}");
        element
    }

    fn keyed(index: usize, key: &str, y: i32) -> UiElement {
        let mut element = row(index, key, y);
        element.key = key.to_string();
        element
    }

    #[test]
    fn an_identical_screen_changes_nothing() {
        let rows: Vec<_> = (0..5)
            .map(|i| keyed(i + 1, &format!("k{i}"), i as i32 * 80))
            .collect();
        let delta = diff_elements(&rows, &rows);
        assert!(delta.is_empty());
        assert_eq!(delta.unchanged, 5);
        assert!(describe(&delta).starts_with("Nothing changed"));
    }

    #[test]
    fn an_appended_row_is_one_arrival_and_not_every_row_moving() {
        // The common case: a message list grows at the bottom and every
        // existing row stays exactly where it was. Pairing by list order would
        // call that one row, not the answer the caller needs.
        let before: Vec<_> = (1..=5)
            .map(|i| keyed(i, &format!("k{i}"), (i - 1) as i32 * 80))
            .collect();
        let mut after = before.clone();
        after.push(keyed(6, "new", 5 * 80));
        let delta = diff_elements(&before, &after);
        assert_eq!(delta.added.len(), 1);
        assert_eq!(delta.added[0].key, "new");
        assert_eq!(delta.moved.len(), 0);
        assert_eq!(delta.removed.len(), 0);
        assert_eq!(delta.unchanged, 5);
    }

    #[test]
    fn a_row_pushing_the_others_down_is_a_movement_not_an_arrival() {
        // Insertion at the top really does move every row, and saying so is
        // the truth — the alternative is claiming five new rows.
        let before: Vec<_> = (1..=5)
            .map(|i| keyed(i, &format!("k{i}"), i as i32 * 80))
            .collect();
        let mut after = vec![keyed(1, "new", 0)];
        after.extend((1..=5).map(|i| keyed(i + 1, &format!("k{i}"), (i + 1) as i32 * 80)));
        let delta = diff_elements(&before, &after);
        assert_eq!(delta.added.len(), 1);
        assert_eq!(delta.added[0].key, "new");
        assert_eq!(delta.moved.len(), 5);
        assert_eq!(delta.removed.len(), 0);
        assert_eq!(delta.unchanged, 0);
    }

    #[test]
    fn a_row_that_moved_is_reported_as_moved() {
        let before = vec![keyed(1, "a", 0), keyed(2, "b", 80)];
        let after = vec![keyed(1, "a", 80), keyed(2, "b", 0)];
        let delta = diff_elements(&before, &after);
        assert_eq!(delta.moved.len(), 2);
        assert_eq!(delta.added.len(), 0);
        assert_eq!(delta.removed.len(), 0);
    }

    #[test]
    fn a_substituted_row_is_one_out_and_one_in() {
        let before = vec![keyed(1, "a", 0)];
        let after = vec![keyed(1, "b", 0)];
        let delta = diff_elements(&before, &after);
        assert_eq!(delta.added.len(), 1);
        assert_eq!(delta.removed.len(), 1);
        assert_eq!(delta.unchanged, 0);
    }

    #[test]
    fn duplicate_rows_are_compared_as_a_multiset() {
        let before = vec![keyed(1, "same", 0), keyed(2, "same", 80)];
        let after = vec![keyed(1, "same", 0)];
        let delta = diff_elements(&before, &after);
        assert_eq!(delta.removed.len(), 1);
        assert_eq!(delta.added.len(), 0);
        assert_eq!(delta.unchanged, 1);
    }

    #[test]
    fn the_delta_lists_what_arrived_with_the_screen_line() {
        let before = vec![keyed(1, "a", 0)];
        let after = vec![keyed(1, "a", 0), keyed(2, "b", 80)];
        let delta = diff_elements(&before, &after);
        let text = describe(&delta);
        assert!(text.contains("1 appeared"), "{text}");
        assert!(text.contains("[2] TextView \"b\""), "{text}");
    }

    #[test]
    fn a_since_value_is_hex_with_or_without_a_prefix() {
        assert_eq!(parse_since("ff"), Some(255));
        assert_eq!(parse_since(" 0xff "), Some(255));
        assert_eq!(parse_since("0x"), None);
        assert_eq!(parse_since("not-a-number"), None);
    }

    #[test]
    fn a_baseline_is_found_by_device_display_and_fingerprint() {
        let rows = vec![keyed(1, "a", 0)];
        remember(Some("emulator-5554"), 0, 0xabc, Some("com.example"), &rows);
        let found = compare(Some("emulator-5554"), 0, "abc", Some("com.example"), &rows);
        assert!(matches!(found, ScreenDiff::Diff(delta) if delta.is_empty()));
        // The same name on another device names nothing.
        let other = compare(Some("other"), 0, "abc", Some("com.example"), &rows);
        assert!(matches!(other, ScreenDiff::Full(FullReason::NoBaseline)));
        // So does the same name on another display.
        let display = compare(Some("emulator-5554"), 3, "abc", Some("com.example"), &rows);
        assert!(matches!(display, ScreenDiff::Full(FullReason::NoBaseline)));
    }

    #[test]
    fn a_baseline_from_another_app_is_not_compared() {
        let rows = vec![keyed(1, "a", 0)];
        remember(None, 0, 0x11, Some("com.example"), &rows);
        let found = compare(None, 0, "11", Some("com.other"), &rows);
        assert!(matches!(found, ScreenDiff::Full(FullReason::AppChanged)));
    }

    #[test]
    fn a_mostly_new_screen_falls_back_to_the_whole_list() {
        let before: Vec<_> = (1..=4)
            .map(|i| keyed(i, &format!("old{i}"), i as i32 * 80))
            .collect();
        remember(None, 0, 0x22, Some("com.example"), &before);
        let after: Vec<_> = (1..=4)
            .map(|i| keyed(i, &format!("new{i}"), i as i32 * 80))
            .collect();
        // All four rows left and four arrived: eight of four.
        let found = compare(None, 0, "22", Some("com.example"), &after);
        assert!(matches!(found, ScreenDiff::Full(FullReason::MostlyChanged)));
    }

    #[test]
    fn exactly_half_changed_is_still_a_diff() {
        let before: Vec<_> = (1..=4)
            .map(|i| keyed(i, &format!("old{i}"), i as i32 * 80))
            .collect();
        remember(None, 0, 0x33, Some("com.example"), &before);
        let mut after: Vec<_> = (1..=4)
            .map(|i| keyed(i, &format!("old{i}"), i as i32 * 80))
            .collect();
        after[0] = keyed(1, "fresh", 0);
        // One out, one in: two of four.
        let found = compare(None, 0, "33", Some("com.example"), &after);
        assert!(matches!(found, ScreenDiff::Diff(_)));
    }

    #[test]
    fn the_oldest_reads_fall_out_of_the_history() {
        let rows = vec![keyed(1, "a", 0)];
        for i in 0..(HISTORY as u64 + 2) {
            remember(Some("h"), 9, i, Some("com.example"), &rows);
        }
        // The first two are gone, the last one is there.
        assert!(matches!(
            compare(Some("h"), 9, "0", Some("com.example"), &rows),
            ScreenDiff::Full(FullReason::NoBaseline)
        ));
        let newest = format!("{:x}", HISTORY as u64 + 1);
        assert!(matches!(
            compare(Some("h"), 9, &newest, Some("com.example"), &rows),
            ScreenDiff::Diff(_)
        ));
    }
}
