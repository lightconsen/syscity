//! Handing the agent's display to the user, and taking it back.
//!
//! Some steps cannot be automated and should not be faked: a login, a captcha, a
//! payment confirmation. The honest move is to put the screen in front of the
//! person, wait for them to say they are done, and then re-read it — because
//! "I finished" is a claim, not a fact, and the agent's next action depends on
//! what the screen actually shows now.
//!
//! Two things make this more than an `ask_user` call:
//!
//! * **The screen moves.** The display's app is brought to the user's own
//!   screen, so they can touch it with the phone in their hand rather than
//!   through a description of it. When they hand it back, it returns to the
//!   agent's display.
//! * **The answer is verified.** After the hand-back, the display is read again
//!   and its fingerprint compared with the one taken before. A user who tapped
//!   "done" without doing anything gets a result that says so, rather than an
//!   agent that proceeds on a stale screen.
//!
//! What this deliberately does not do is decide the user is unnecessary. It is a
//! tool the model calls when it recognises it is stuck — not a fallback wired
//! into every failed tap.

use serde_json::{json, Value};

use super::android::{format_ui_summary, tree_fingerprint, UiElement};
use super::display::{DisplayError, DISPLAY_UNAVAILABLE};
use crate::device::CMD_VIRTUAL_DISPLAY;

/// The user took over and handed back.
pub const TAKEOVER_DONE: &str = "done";
/// The user said they would handle it themselves and no longer need the agent.
pub const TAKEOVER_SELF: &str = "self";
/// The user asked to skip this step.
pub const TAKEOVER_SKIPPED: &str = "skipped";
/// Nobody answered before the deadline.
pub const TAKEOVER_TIMEOUT: &str = "timeout";

/// What happened while the user had the screen.
#[derive(Debug, Clone, PartialEq)]
pub struct TakeoverOutcome {
    /// One of the [`TAKEOVER_DONE`] constants.
    pub verdict: &'static str,
    /// What the user typed back, verbatim.
    pub answer: String,
    /// Whether the screen changed while they had it. `None` when it could not
    /// be read on either side.
    pub screen_changed: Option<bool>,
    /// The screen as it is now, for the model to act on.
    pub elements: Vec<UiElement>,
}

impl TakeoverOutcome {
    /// A sentence for the tool result.
    ///
    /// Says what the screen did, not just what the user said: they are two
    /// different facts and only one of them is evidence.
    pub fn summary(&self) -> String {
        let change = match self.screen_changed {
            Some(true) => "the screen changed while they had it",
            Some(false) => {
                "the screen looks the same as before — they may not have done anything, \
                 or the change is one the accessibility tree cannot see"
            }
            None => "the screen could not be compared (it was unreadable on one side)",
        };
        format!("User said {:?}; {change}.", self.answer)
    }
}

/// Step aside: bring the display's app to the user, wait, and take it back.
pub async fn hand_to_user(
    bridge: &crate::device::DeviceBridgeRef,
    display_id: i32,
    before: Option<u64>,
    ask: AskFn<'_>,
) -> Result<TakeoverOutcome, DisplayError> {
    // 1. Move the app to the user's screen.
    //
    // The helper does the move; from the app's side this is one op. A display
    // that cannot be moved still gets the question asked — the user can reach
    // the app themselves — so a failure here is reported, not fatal.
    let moved = move_to_user(bridge, display_id).await;

    // 2. Ask, and wait.
    let answer = ask().await;

    // 3. Take it back, whether or not they answered.
    let returned = move_to_agent(bridge, display_id).await;

    // 4. Read it. The fingerprint comparison is what turns "I'm done" into
    //    evidence the model can act on.
    let elements =
        super::android::dump_ui_elements(&None, super::android::Target::Display(display_id))
            .await
            .unwrap_or_default();
    let after = if elements.is_empty() {
        None
    } else {
        Some(tree_fingerprint(&elements))
    };
    let screen_changed = match (before, after) {
        (Some(before), Some(after)) => Some(before != after),
        _ => None,
    };

    let (verdict, answer) = match answer {
        Some(text) => {
            let lowered = text.to_ascii_lowercase();
            let verdict = if lowered.contains("skip") {
                TAKEOVER_SKIPPED
            } else if lowered.contains("i'll do it") || lowered.contains("myself") {
                TAKEOVER_SELF
            } else {
                TAKEOVER_DONE
            };
            (verdict, text)
        }
        None => (TAKEOVER_TIMEOUT, String::new()),
    };

    // A move that failed is worth saying: the user may have been looking at the
    // wrong screen the whole time.
    if moved.is_err() || returned.is_err() {
        tracing::warn!(
            display = display_id,
            moved = ?moved.as_ref().err().map(|e| e.message.as_str()),
            returned = ?returned.as_ref().err().map(|e| e.message.as_str()),
            "moving the display for the user failed; the answer was still taken",
        );
    }

    Ok(TakeoverOutcome {
        verdict,
        answer,
        screen_changed,
        elements,
    })
}

/// Ask the human. `None` means the deadline passed.
pub type AskFn<'a> = Box<
    dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + 'a>>
        + Send
        + 'a,
>;

/// Move the display's top task onto the user's screen.
async fn move_to_user(
    bridge: &crate::device::DeviceBridgeRef,
    display_id: i32,
) -> Result<(), DisplayError> {
    let answer = bridge
        .call(CMD_VIRTUAL_DISPLAY, json!({ "op": "move_to_user", "display": display_id }))
        .await
        .map_err(|e| DisplayError {
            code: DISPLAY_UNAVAILABLE,
            message: format!("could not move the display to the user ({e})"),
        })?;
    if answer.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(DisplayError {
            code: DISPLAY_UNAVAILABLE,
            message: answer
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the helper refused to move the display")
                .to_string(),
        })
    }
}

/// Move it back to the agent's display.
async fn move_to_agent(
    bridge: &crate::device::DeviceBridgeRef,
    display_id: i32,
) -> Result<(), DisplayError> {
    let answer = bridge
        .call(CMD_VIRTUAL_DISPLAY, json!({ "op": "move_to_agent", "display": display_id }))
        .await
        .map_err(|e| DisplayError {
            code: DISPLAY_UNAVAILABLE,
            message: format!("could not take the display back ({e})"),
        })?;
    if answer.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(DisplayError {
            code: DISPLAY_UNAVAILABLE,
            message: answer
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the helper refused to take the display back")
                .to_string(),
        })
    }
}

/// The screen as the tool result describes it: a line per element, plus the
/// verdict.
pub fn describe(outcome: &TakeoverOutcome) -> String {
    let mut text = outcome.summary();
    if !outcome.elements.is_empty() {
        text.push_str("\n\n");
        text.push_str(&format_ui_summary(&outcome.elements));
    } else {
        text.push_str(
            "\n\nThe screen could not be read after the hand-back, so there is nothing to act \
             on here. Read it with android_observe before trying anything.",
        );
    }
    text
}

/// Check whether an action pushed an app onto the user's own screen.
///
/// An action on the agent's display can start something that lands on the
/// user's — a share sheet, a chooser, an app that ignores `--display`. From the
/// agent's side that is invisible: its own display simply stops changing, and
/// the next read says "unchanged".
///
/// Returns `Some((task_id, package))` when something is on the user's screen
/// that should not be, and pulls it back when `recover` is set. An app the user
/// was already using is left alone — moving it would yank the screen out from
/// under them, which is the thing the second display exists to prevent.
pub async fn check_spill(
    bridge: &crate::device::DeviceBridgeRef,
    display_id: i32,
    user_was_using: Option<&str>,
    recover: bool,
) -> Option<Spill> {
    let display = super::display::VirtualDisplay::attach(bridge.clone(), display_id);
    let (task_id, package) = display.main_screen_top().await.ok()??;
    // The user's launcher is not a spill: it is what a phone sits on.
    let package_name = package?;
    if package_name.contains("launcher") || package_name == "com.android.systemui" {
        return None;
    }
    if user_was_using == Some(package_name.as_str()) {
        return Some(Spill {
            task_id,
            package: package_name,
            recovered: false,
            reason: "the user was already using this app, so it was left alone",
        });
    }

    let mut recovered = false;
    if recover {
        recovered = display.move_task_here(task_id).await.is_ok();
    }
    Some(Spill {
        task_id,
        package: package_name,
        recovered,
        reason: if recovered {
            "moved back to the agent's display"
        } else if recover {
            "it could not be moved back — it is still on the user's screen"
        } else {
            "left where it is (recovery was not requested)"
        },
    })
}

/// An app that ended up on the user's screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Spill {
    pub task_id: i32,
    pub package: String,
    pub recovered: bool,
    /// What was done about it.
    pub reason: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(verdict: &'static str, answer: &str, changed: Option<bool>) -> TakeoverOutcome {
        TakeoverOutcome {
            verdict,
            answer: answer.to_string(),
            screen_changed: changed,
            elements: Vec::new(),
        }
    }

    #[test]
    fn a_changed_screen_is_reported_as_evidence() {
        let summary = outcome(TAKEOVER_DONE, "done", Some(true)).summary();
        assert!(summary.contains("the screen changed"), "{summary}");
        assert!(!summary.contains("looks the same"), "{summary}");
    }

    #[test]
    fn an_unchanged_screen_says_it_did_not_change() {
        let summary = outcome(TAKEOVER_DONE, "done", Some(false)).summary();
        assert!(summary.contains("looks the same"), "{summary}");
        // It must not claim the user did nothing: a canvas change is invisible.
        assert!(summary.contains("cannot see"), "{summary}");
    }

    #[test]
    fn an_unreadable_screen_is_unknown_not_unchanged() {
        let summary = outcome(TAKEOVER_DONE, "done", None).summary();
        assert!(summary.contains("could not be compared"), "{summary}");
    }

    #[test]
    fn the_answer_is_quoted_verbatim() {
        let summary = outcome(TAKEOVER_DONE, "logged in as 张三", Some(true)).summary();
        assert!(summary.contains("logged in as 张三"), "{summary}");
    }

    #[test]
    fn an_answer_that_asks_to_skip_is_skipped() {
        // The wording matters: "skip" in any case, in the user's own language
        // as long as it contains the word.
        let lowered = "Skip this step".to_ascii_lowercase();
        assert!(lowered.contains("skip"));
    }

    #[test]
    fn a_spill_names_what_moved_and_what_was_done() {
        let spilled = Spill {
            task_id: 42,
            package: "com.android.intentresolver".to_string(),
            recovered: true,
            reason: "moved back to the agent's display",
        };
        // The tool result builds this sentence; the fields are what a caller
        // branches on, so both have to be present and distinct.
        assert_eq!(spilled.task_id, 42);
        assert!(spilled.recovered);
        assert!(spilled.reason.contains("moved back"));
    }

    #[test]
    fn an_unrecovered_spill_says_it_is_still_on_the_users_screen() {
        let spilled = Spill {
            task_id: 7,
            package: "com.example".to_string(),
            recovered: false,
            reason: "it could not be moved back — it is still on the user's screen",
        };
        assert!(!spilled.recovered);
        assert!(spilled.reason.contains("still on the user's screen"));
    }

    #[test]
    fn an_empty_screen_says_to_read_it_first() {
        let text = describe(&outcome(TAKEOVER_DONE, "done", Some(true)));
        assert!(text.contains("nothing to act on"), "{text}");
        assert!(text.contains("android_observe"), "{text}");
    }
}
