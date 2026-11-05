//! Judging what happened — verdicts every device set shares.
//!
//! A platform set has two jobs. The first is to *read* its device: an
//! accessibility tree, a screenshot, an ADB dump, a WebDriverAgent session. The
//! second — the one this module owns — is to *judge* what it read: did the
//! action change the screen, did the written text land, does this event mean
//! the screen is still moving.
//!
//! Those judgments are not Android's, even though that is where they were
//! first written, because that is where the failures were found. The questions
//! — "the screen changed, but not by enough to be a new page", "the field does
//! not hold what was typed even though the app clearly received it", "a content
//! event arrived, so the screen is still animating" — are the same for an iOS
//! set driving WebDriverAgent and for a future Windows-ARM set driving an
//! Android subsystem. Keeping the answers here means a second set inherits the
//! semantics instead of re-deriving them, and a fix to one is a fix to both.
//!
//! Nothing here touches a device. Every function takes observation data in and
//! returns a verdict, so a set with a completely different mechanism can call
//! them without adopting Android's types. What stays in a set is the part that
//! is genuinely platform-shaped: how to locate the field that was written to,
//! how to take a fingerprint, how to read the foreground app.
//!
//! [`ScreenSemantics`] is how a set says which of these judgments it actually
//! makes. Declaring it is not decoration: a caller that asks for a read-back on
//! a set which does not do them would otherwise be told "unknown" forever, with
//! no statement of whether that is a failure or a fact of the device.

use serde::{Deserialize, Serialize};

/// Which of the shared judgments a set actually makes.
///
/// A set that says `false` is not broken — the iOS bridge today cannot see a
/// field's contents, and a future Windows-ARM set driving an Android subsystem
/// inherits Android's answers wholesale. What matters is that the answer is
/// stated at the set, once, instead of being inferred from a result that says
/// `unknown` for two different reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScreenSemantics {
    /// Actions are followed by a re-read that says whether the screen changed —
    /// so `effect` is one of the five words rather than absent.
    pub judges_effect: bool,
    /// A written field can be read back, so "the text landed" is a fact and not
    /// an assumption from the keystrokes being accepted.
    pub reads_back_writes: bool,
    /// The device reports events, so a wait can be ended by the screen saying
    /// it moved instead of by polling.
    pub reports_events: bool,
}

impl ScreenSemantics {
    /// A set that makes none of these judgments yet.
    pub const NONE: Self = Self {
        judges_effect: false,
        reads_back_writes: false,
        reports_events: false,
    };

    /// All three: the full semantics, as the Android set has them.
    pub const ALL: Self = Self {
        judges_effect: true,
        reads_back_writes: true,
        reports_events: true,
    };

    /// Whether this set makes the given judgment.
    pub fn makes(self, judgment: Judgment) -> bool {
        match judgment {
            Judgment::Effect => self.judges_effect,
            Judgment::Readback => self.reads_back_writes,
            Judgment::Events => self.reports_events,
        }
    }

    /// One line naming what is missing, for a caller that relied on it.
    pub fn gap_note(self, judgment: Judgment) -> Option<String> {
        if self.makes(judgment) {
            return None;
        }
        Some(match judgment {
            Judgment::Effect => {
                "This device set does not judge the effect of an action: no re-read follows a \
                 tap, so \"did it work\" is not knowable here. Observe the screen instead."
                    .to_string()
            }
            Judgment::Readback => {
                "This device set cannot read a written field back, so a successful write means \
                 the keystrokes were accepted, not that the text is in the field."
                    .to_string()
            }
            Judgment::Events => {
                "This device set has no event stream, so waits are answered by polling the screen."
                    .to_string()
            }
        })
    }
}

/// One of the shared judgments a set may or may not make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Judgment {
    /// Did the action change the screen.
    Effect,
    /// Did the written text reach the field.
    Readback,
    /// Does the device report events.
    Events,
}

impl Judgment {
    /// Every judgment, for schemas and lists.
    pub const ALL: [Self; 3] = [Self::Effect, Self::Readback, Self::Events];
}

/// The screen was different after the action.
pub const EFFECT_CHANGED: &str = "changed";
/// The screen was the same after the action.
pub const EFFECT_UNCHANGED: &str = "unchanged";
/// The effect could not be judged — the tree could not be read.
pub const EFFECT_UNKNOWN: &str = "unknown";
/// Something moved, but the app stayed and the screen's structure did not
/// change — a tick, a highlight, a toast, a spinner.
///
/// Worth its own word: `unchanged` tells a caller the action missed, and
/// `changed` tells it the screen went somewhere. A tap that ticked a checkbox
/// is neither, and folding it into `changed` was hiding the degree.
pub const EFFECT_SMALL_CHANGE: &str = "small_change";
/// The action moved the task to a different app or a different place.
///
/// Only knowable when the foreground package was read before and after. A
/// screen that changed without the package changing is [`EFFECT_CHANGED`].
pub const EFFECT_PLACE_CHANGED: &str = "place_changed";

/// What a set may conclude about an action from what it observed.
///
/// The wire words are the constants above; this type is what the judgment
/// returns, so a caller that matches on it cannot silently fall through a
/// spelling mistake the way a string comparison can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effect {
    /// The screen moved to a different page.
    Changed,
    /// The screen is the same. The action may have missed.
    Unchanged,
    /// Something moved, but the screen did not go anywhere.
    SmallChange,
    /// The foreground app changed.
    PlaceChanged,
    /// The observation could not support a verdict.
    Unknown,
}

impl Effect {
    /// Every effect, most decisive first — for schemas and bare lists.
    pub const ALL: [Self; 5] = [
        Self::Changed,
        Self::PlaceChanged,
        Self::SmallChange,
        Self::Unchanged,
        Self::Unknown,
    ];

    /// The word used in a result, and accepted on a wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Changed => EFFECT_CHANGED,
            Self::Unchanged => EFFECT_UNCHANGED,
            Self::SmallChange => EFFECT_SMALL_CHANGE,
            Self::PlaceChanged => EFFECT_PLACE_CHANGED,
            Self::Unknown => EFFECT_UNKNOWN,
        }
    }

    /// Parse one effect word, exactly.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|effect| effect.as_str() == value.trim())
    }

    /// Whether this is a verdict at all, as opposed to "could not tell".
    pub fn is_decisive(self) -> bool {
        self != Self::Unknown
    }
}

impl std::fmt::Display for Effect {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How the element list changed between two reads.
///
/// Counts rather than a yes/no: "three rows appeared" and "the whole page was
/// replaced" are different answers, and only the second means the action left
/// this screen. Each set fills this in from its own notion of element identity;
/// the threshold below is applied to the totals, which is the part worth
/// sharing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TreeDelta {
    pub added: usize,
    pub removed: usize,
    /// Same element, different place.
    pub moved: usize,
    /// Same element, different label.
    pub edited: usize,
}

impl TreeDelta {
    /// Total number of elements that are not in the same place, or not the
    /// same, as before.
    pub fn changed(&self) -> usize {
        self.added + self.removed + self.moved + self.edited
    }

    /// One line for the result text, or `None` when nothing changed.
    pub fn describe(&self) -> Option<String> {
        if self.changed() == 0 {
            return None;
        }
        let mut parts = Vec::new();
        for (count, what) in [
            (self.added, "appeared"),
            (self.removed, "left"),
            (self.moved, "moved"),
            (self.edited, "changed text"),
        ] {
            if count > 0 {
                parts.push(format!("{count} {what}"));
            }
        }
        Some(format!("Compared with before: {}.", parts.join(", ")))
    }
}

/// How many changed elements make a screen "changed" rather than "small".
///
/// Proportional with a floor: three changed elements is a real change on a
/// short screen, and ten percent of a long one. Below either, the screen moved
/// but did not go anywhere. A screen that reports very few elements (a canvas,
/// a game surface with one node) is `changed` at three, which is deliberate:
/// with no structure to weigh, any movement is the most the tree can say.
pub fn changed_threshold(total: usize) -> usize {
    (total * 10).div_ceil(100).max(3)
}

/// Whether the action left whatever was in front.
///
/// Only decidable when both readings named an app. An unknown package on
/// either side is not "the same place" — it is "no answer", which is why this
/// returns `false` rather than assuming no change.
pub fn place_changed(before: Option<&str>, after: Option<&str>) -> bool {
    matches!((before, after), (Some(before), Some(after)) if before != after)
}

/// The effect verdict, from what was observed after the action.
///
/// Ordered so the most specific answer wins: a new app is a place change even
/// if the tree also changed; a canvas that moved under a motionless tree is a
/// small change; and a tree that could not be read twice is unknown, not
/// unchanged.
///
/// `pixels_changed` is the set's own answer about the picture when it compared
/// one; `None` means "not compared", which is not the same as "did not change".
/// `before_fingerprint`/`after_fingerprint` identify the tree reads — a
/// matching pair is what proves the structure did not move, and a missing
/// `before` is what makes the structural comparison unavailable.
pub fn classify(
    before_package: Option<&str>,
    after_package: Option<&str>,
    delta: Option<&TreeDelta>,
    shown: usize,
    pixels_changed: Option<bool>,
    before_fingerprint: Option<u64>,
    after_fingerprint: Option<u64>,
) -> Effect {
    if place_changed(before_package, after_package) {
        return Effect::PlaceChanged;
    }
    let Some(delta) = delta else {
        return Effect::Unknown;
    };
    let same_tree = before_fingerprint.is_some() && before_fingerprint == after_fingerprint;
    if same_tree {
        return if pixels_changed == Some(true) {
            Effect::SmallChange
        } else {
            Effect::Unchanged
        };
    }
    if before_fingerprint.is_none() {
        return Effect::Unknown;
    }
    if delta.changed() >= changed_threshold(shown) {
        Effect::Changed
    } else {
        Effect::SmallChange
    }
}

// ── Write readback ─────────────────────────────────────────────────────────

/// The text the screen actually holds matches what was typed.
pub const READBACK_MATCH: &str = "match";
/// It holds something else.
pub const READBACK_MISMATCH: &str = "mismatch";
/// The field could not be read back — no field was found, it is a password
/// box, or the tree could not be read at all.
pub const READBACK_UNKNOWN: &str = "unknown";

/// What a write's read-back says about the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadbackVerdict {
    /// The field holds the text that was typed.
    Match,
    /// It holds something else: the write did not take.
    Mismatch,
    /// The field cannot answer for the text — not found, a password box, an
    /// unreadable tree, or a field the app received the text into without
    /// showing it.
    Unknown,
}

impl ReadbackVerdict {
    /// The word used in a result.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Match => READBACK_MATCH,
            Self::Mismatch => READBACK_MISMATCH,
            Self::Unknown => READBACK_UNKNOWN,
        }
    }

    /// Parse one verdict word, exactly.
    pub fn parse(value: &str) -> Option<Self> {
        [Self::Match, Self::Mismatch, Self::Unknown]
            .into_iter()
            .find(|verdict| verdict.as_str() == value.trim())
    }

    /// Whether the caller knows the text is there. A mismatch is an answer;
    /// unknown is the absence of one, and the two lead to different retries.
    pub fn is_decided(self) -> bool {
        self != Self::Unknown
    }
}

/// Compare what was typed with what the field holds.
///
/// Whitespace is stripped on both sides: an auto-formatted phone number inserts
/// spaces as it goes, and that is the field formatting, not the text being
/// wrong. Case and punctuation are compared exactly — "Hello" and "hello" are
/// two different messages, and the field is where the difference matters.
pub fn readback_matches(written: &str, read: &str) -> bool {
    let norm = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    norm(written) == norm(read)
}

/// Decide the verdict from what was written, what the field reads now, and
/// what it read before the write.
///
/// A field that answers the same thing it did before is normally a mismatch —
/// the write did not take. It is `Unknown` instead when the screen meanwhile
/// showed text containing what was typed (an autocomplete suggestion, a
/// counter, the text echoed in a preview): the app clearly received the input,
/// and this field just cannot confirm the content. Claiming a mismatch there
/// would send a caller round a loop of retries that write the text twice.
pub fn readback_verdict(
    written: &str,
    actual: &str,
    baseline: Option<&str>,
    reacted: bool,
) -> ReadbackVerdict {
    if readback_matches(written, actual) {
        return ReadbackVerdict::Match;
    }
    if baseline.is_some_and(|base| base == actual) && reacted {
        return ReadbackVerdict::Unknown;
    }
    ReadbackVerdict::Mismatch
}

// ── Events ─────────────────────────────────────────────────────────────────

/// What said the screen moved first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Signal {
    /// The device's event stream announced a content change, then went quiet.
    Events,
    /// Only the polled reads could tell, by agreeing twice.
    Tree,
}

impl Signal {
    /// The word used in a result.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Events => "events",
            Self::Tree => "tree",
        }
    }
}

/// Whether a device event is evidence that the screen under an action moved.
///
/// A content change is the screen redrawing, so it counts — but only for the
/// app being judged, when the phone named an app at all. A window change is a
/// different place and is left to the caller's own package comparison, because
/// treating it as "the screen moved" would call every app switch a successful
/// action. A notification from another app arrives without the screen
/// changing, so it is not evidence either way.
///
/// The same rule applies to any platform that reports events; a set with no
/// stream never calls this, and a set whose stream reports different kinds
/// passes only the ones it trusts.
pub fn content_event_matters(event: &crate::device::DeviceEvent, package: Option<&str>) -> bool {
    match event.kind {
        crate::device::EventKind::Content => {
            package.is_none_or(|package| event.package.is_empty() || event.package == package)
        }
        crate::device::EventKind::Window | crate::device::EventKind::Notification => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{DeviceEvent, EventKind};

    fn delta(added: usize, removed: usize, moved: usize, edited: usize) -> TreeDelta {
        TreeDelta { added, removed, moved, edited }
    }

    #[test]
    fn every_effect_round_trips_through_its_word() {
        for effect in Effect::ALL {
            assert_eq!(Effect::parse(effect.as_str()), Some(effect), "{effect:?}");
            assert_eq!(effect.to_string(), effect.as_str());
        }
        assert_eq!(Effect::parse(" settled "), None);
    }

    #[test]
    fn unknown_is_the_only_indecisive_effect() {
        for effect in Effect::ALL {
            assert_eq!(effect.is_decisive(), effect != Effect::Unknown, "{effect:?}");
        }
    }

    #[test]
    fn a_new_app_is_a_place_change_whatever_the_tree_did() {
        let moved = delta(20, 20, 0, 0);
        assert_eq!(
            classify(Some("a"), Some("b"), Some(&moved), 40, None, Some(1), Some(2)),
            Effect::PlaceChanged
        );
    }

    #[test]
    fn an_unknown_package_is_not_a_place_change() {
        // One side could not name the app. That is no answer, not "same place"
        // — and it must not be handed to the structure comparison as if it were
        // a settled fact either.
        assert!(!place_changed(Some("a"), None));
        assert!(!place_changed(None, Some("b")));
        assert!(!place_changed(None, None));
        assert!(place_changed(Some("a"), Some("b")));
        assert!(!place_changed(Some("a"), Some("a")));
    }

    #[test]
    fn a_motionless_tree_that_moved_pixels_is_a_small_change() {
        let same = TreeDelta::default();
        assert_eq!(
            classify(None, None, Some(&same), 10, Some(true), Some(7), Some(7)),
            Effect::SmallChange
        );
        assert_eq!(
            classify(None, None, Some(&same), 10, Some(false), Some(7), Some(7)),
            Effect::Unchanged
        );
    }

    #[test]
    fn an_uncompared_picture_does_not_turn_a_motionless_tree_into_a_change() {
        // `None` is "not compared", and a screen that was not looked at is not
        // a screen that moved.
        let same = TreeDelta::default();
        assert_eq!(
            classify(None, None, Some(&same), 10, None, Some(7), Some(7)),
            Effect::Unchanged
        );
    }

    #[test]
    fn a_tree_with_no_baseline_cannot_be_judged() {
        assert_eq!(classify(None, None, None, 0, None, Some(1), Some(2)), Effect::Unknown);
        assert_eq!(
            classify(None, None, Some(&delta(1, 0, 0, 0)), 10, None, None, Some(2)),
            Effect::Unknown
        );
    }

    #[test]
    fn three_changed_elements_is_a_real_change_on_a_short_screen() {
        assert_eq!(
            classify(None, None, Some(&delta(3, 0, 0, 0)), 5, None, Some(1), Some(2)),
            Effect::Changed
        );
        assert_eq!(
            classify(None, None, Some(&delta(2, 0, 0, 0)), 5, None, Some(1), Some(2)),
            Effect::SmallChange
        );
    }

    #[test]
    fn a_long_screen_needs_ten_percent() {
        assert_eq!(changed_threshold(400), 40);
        assert_eq!(
            classify(None, None, Some(&delta(40, 0, 0, 0)), 400, None, Some(1), Some(2)),
            Effect::Changed
        );
        assert_eq!(
            classify(None, None, Some(&delta(39, 0, 0, 0)), 400, None, Some(1), Some(2)),
            Effect::SmallChange
        );
    }

    #[test]
    fn the_delta_describes_what_it_saw_and_says_nothing_when_nothing_moved() {
        assert_eq!(TreeDelta::default().describe(), None);
        let line = delta(2, 0, 1, 0).describe().expect("a line");
        assert!(line.contains("2 appeared"), "{line}");
        assert!(line.contains("1 moved"), "{line}");
        assert!(!line.contains("left"), "{line}");
        assert_eq!(delta(1, 2, 3, 4).changed(), 10);
    }

    #[test]
    fn every_readback_verdict_round_trips_through_its_word() {
        for verdict in [
            ReadbackVerdict::Match,
            ReadbackVerdict::Mismatch,
            ReadbackVerdict::Unknown,
        ] {
            assert_eq!(ReadbackVerdict::parse(verdict.as_str()), Some(verdict));
        }
        assert_eq!(ReadbackVerdict::parse("readable"), None);
        assert!(ReadbackVerdict::Match.is_decided());
        assert!(ReadbackVerdict::Mismatch.is_decided());
        assert!(!ReadbackVerdict::Unknown.is_decided());
    }

    #[test]
    fn a_field_holding_the_text_matches_and_anything_else_does_not() {
        assert_eq!(readback_verdict("hi", "hi", None, false), ReadbackVerdict::Match);
        assert_eq!(readback_verdict("hi", "ho", None, false), ReadbackVerdict::Mismatch);
        assert!(readback_matches("13800138000", "138 0013 8000"));
        assert!(!readback_matches("Hello", "hello"));
    }

    #[test]
    fn a_field_that_did_not_move_but_saw_the_text_is_unknown_not_a_mismatch() {
        assert_eq!(
            readback_verdict("hi", "Type a message", Some("Type a message"), true),
            ReadbackVerdict::Unknown
        );
        assert_eq!(
            readback_verdict("hi", "Type a message", Some("Type a message"), false),
            ReadbackVerdict::Mismatch
        );
    }

    fn event(kind: EventKind, package: &str) -> DeviceEvent {
        DeviceEvent {
            seq: 1,
            kind,
            package: package.to_string(),
            at_uptime_ms: 0,
            title: None,
            text: None,
            truncated: false,
        }
    }

    #[test]
    fn a_content_event_matters_only_for_the_app_being_judged() {
        assert!(content_event_matters(&event(EventKind::Content, "a"), None));
        assert!(content_event_matters(&event(EventKind::Content, "a"), Some("a")));
        assert!(!content_event_matters(&event(EventKind::Content, "b"), Some("a")));
        // A phone that withheld the package still tells us the screen moved.
        assert!(content_event_matters(&event(EventKind::Content, ""), Some("a")));
    }

    #[test]
    fn a_window_or_notification_is_not_evidence_the_screen_moved() {
        // A window change is a place change, judged by the packages; a
        // notification arrives without the screen changing at all.
        assert!(!content_event_matters(&event(EventKind::Window, "a"), Some("a")));
        assert!(!content_event_matters(&event(EventKind::Notification, "a"), Some("a")));
    }

    #[test]
    fn a_signal_names_which_witness_spoke_first() {
        assert_eq!(Signal::Events.as_str(), "events");
        assert_eq!(Signal::Tree.as_str(), "tree");
    }
}
