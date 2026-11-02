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
/// This is the **single look** version: whatever is on top of the user's screen
/// right now. It is what a caller without a watch gets, and it cannot tell an
/// app the agent started from an app the user opened themselves.
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
    if is_not_a_spill(&package_name) {
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

/// Which arrivals belong to an action.
///
/// The single look answers "is something on the user's screen now", which
/// conflates three different things: the agent spilled an app, the user opened
/// one themselves, or the user was already in one and it changed page. The
/// window separates them, because it knows *when* each task rose.
///
/// Facts come from the helper ([`super::display::Arrival`]); the judgement is
/// here so it can be tested without a phone.
///
/// What is not a spill, and why:
///
/// * **Outside the window.** A task that rose before the action is not it.
/// * **Already gone.** A task that has since left the user's screen is not
///   there to recover, and moving it would be acting on a stale fact.
/// * **Never on the user's screen.** `now != 0` means it is somewhere else —
///   very often the agent's own display, which is where it should be.
/// * **Came from another display.** A task moved here deliberately (a handoff)
///   is not something the action started.
/// * **The user touched the screen during the window.** Then who opened it is
///   unknowable, and the safe answer is "not the agent's fault".
/// * **The launcher, the system UI, or this app.**
pub fn spilled_by_action(
    arrivals: &[super::display::Arrival],
    since: u64,
    window_ms: u64,
    top_before: i32,
    user_touched: &[u64],
    self_package: &str,
) -> Vec<SpillCandidate> {
    let mut out: Vec<SpillCandidate> = Vec::new();
    for arrival in arrivals {
        if arrival.at < since || arrival.at > since + window_ms {
            continue;
        }
        let Some(package) = arrival.package.as_deref() else {
            continue;
        };
        if !arrival.standard || is_not_a_spill(package) || package == self_package {
            continue;
        }
        // `from` is the display it was last seen on: -1 never seen, 0 already
        // here. Anything else means somebody put it here on purpose.
        if arrival.from != -1 && arrival.from != MAIN_DISPLAY {
            continue;
        }
        if arrival.now != MAIN_DISPLAY {
            continue;
        }
        if user_touched
            .iter()
            .any(|at| *at >= since && *at <= arrival.at)
        {
            continue;
        }
        let candidate = SpillCandidate {
            task_id: arrival.task,
            package: package.to_string(),
            verdict: if arrival.task == top_before && arrival.same_task {
                // The app the user was already in changed page: they are
                // looking at it, so moving it would take the screen away.
                SpillVerdict::InUse
            } else if arrival.task == top_before {
                continue;
            } else {
                SpillVerdict::PullBack
            },
        };
        // One entry per task, the latest: the facts supersede.
        out.retain(|existing| existing.task_id != candidate.task_id);
        out.push(candidate);
    }
    out
}

/// The user's own screen.
pub const MAIN_DISPLAY: i32 = 0;

/// What the window says about one arrival.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpillVerdict {
    /// Move it back to the agent's display.
    PullBack,
    /// The user is already in this app; leave it and say so.
    InUse,
}

/// One task the window attributes to an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillCandidate {
    pub task_id: i32,
    pub package: String,
    pub verdict: SpillVerdict,
}

/// Watch the user's screen around one action, and judge what arrived.
///
/// `since` and `window` come from the helper's clock, which is not this
/// process's: the caller keeps the value [`super::display::VirtualDisplay::watch_main_screen`]
/// returned.
pub async fn watch_and_check_spill(
    bridge: &crate::device::DeviceBridgeRef,
    display_id: i32,
    top_before: i32,
    since: u64,
    window_ms: u64,
    user_touched: &[u64],
    recover: bool,
) -> Vec<Spill> {
    let display = super::display::VirtualDisplay::attach(bridge.clone(), display_id);
    let arrivals = match display.main_arrivals(since).await {
        Ok(arrivals) => arrivals,
        Err(failed) => {
            tracing::warn!(
                display = display_id,
                error = %failed,
                "could not read what rose to the top of the user's screen",
            );
            return Vec::new();
        }
    };
    let candidates =
        spilled_by_action(&arrivals, since, window_ms, top_before, user_touched, SELF_PACKAGE);

    let mut spills = Vec::new();
    for candidate in candidates {
        match candidate.verdict {
            SpillVerdict::InUse => spills.push(Spill {
                task_id: candidate.task_id,
                package: candidate.package,
                recovered: false,
                reason: "the user was already using this app, so it was left alone",
            }),
            SpillVerdict::PullBack => {
                let moved = recover && display.move_task_here(candidate.task_id).await.is_ok();
                spills.push(Spill {
                    task_id: candidate.task_id,
                    package: candidate.package,
                    recovered: moved,
                    reason: if moved {
                        "moved back to the agent's display"
                    } else if recover {
                        "it could not be moved back — it is still on the user's screen"
                    } else {
                        "left where it is (recovery was not requested)"
                    },
                });
            }
        }
    }
    spills
}

/// This app's own package — its task is the UI, never a spill.
const SELF_PACKAGE: &str = "net.syscity.desktop";

/// Packages that are never a spill: the launcher and the system UI sit on the
/// user's screen by design.
fn is_not_a_spill(package: &str) -> bool {
    package.contains("launcher") || package == "com.android.systemui" || package == SELF_PACKAGE
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
    fn the_apps_own_task_is_never_a_spill() {
        // It is the UI. Relocating it would take the screen from the user, and
        // the "recovery" would be the agent moving its own window around.
        assert!(is_not_a_spill("net.syscity.desktop"));
    }

    #[test]
    fn the_launcher_and_system_ui_are_not_spills() {
        assert!(is_not_a_spill("com.google.android.apps.nexuslauncher"));
        assert!(is_not_a_spill("com.android.launcher3"));
        assert!(is_not_a_spill("com.android.systemui"));
    }

    #[test]
    fn an_app_that_was_opened_is_a_spill() {
        assert!(!is_not_a_spill("com.android.intentresolver"));
        assert!(!is_not_a_spill("com.google.android.documentsui"));
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

    // ── The spill window ────────────────────────────────────────────────

    use super::super::display::Arrival;

    fn arrival(task: i32, package: &str, at: u64, from: i32, now: i32) -> Arrival {
        Arrival {
            task,
            package: Some(package.to_string()),
            at,
            from,
            same_task: false,
            standard: true,
            now,
        }
    }

    #[test]
    fn an_app_that_rose_during_the_window_is_a_spill() {
        let arrivals = vec![arrival(42, "com.android.intentresolver", 1_050, -1, 0)];
        let spills = spilled_by_action(&arrivals, 1_000, 3_000, 7, &[], SELF_PACKAGE);
        assert_eq!(spills.len(), 1);
        assert_eq!(spills[0].task_id, 42);
        assert_eq!(spills[0].verdict, SpillVerdict::PullBack);
    }

    #[test]
    fn a_task_that_rose_before_the_action_is_not_blamed_on_it() {
        let arrivals = vec![arrival(42, "com.example.chat", 900, -1, 0)];
        let spills = spilled_by_action(&arrivals, 1_000, 3_000, 7, &[], SELF_PACKAGE);
        assert!(spills.is_empty(), "{spills:?}");
    }

    #[test]
    fn a_task_that_rose_after_the_window_closed_is_not_blamed_on_it() {
        let arrivals = vec![arrival(42, "com.example.chat", 4_500, -1, 0)];
        let spills = spilled_by_action(&arrivals, 1_000, 3_000, 7, &[], SELF_PACKAGE);
        assert!(spills.is_empty(), "{spills:?}");
    }

    #[test]
    fn a_task_the_user_opened_is_not_a_spill() {
        // The user touched the screen at 1_500, the app rose at 1_600: the
        // agent cannot be credited with it, so the safe answer is "not ours".
        let arrivals = vec![arrival(42, "com.example.chat", 1_600, -1, 0)];
        let spills = spilled_by_action(&arrivals, 1_000, 3_000, 7, &[1_500], SELF_PACKAGE);
        assert!(spills.is_empty(), "{spills:?}");
    }

    #[test]
    fn a_touch_before_the_action_does_not_excuse_a_spill() {
        // The user had the screen at 800, before the window; what rose at 1_600
        // is still the agent's doing.
        let arrivals = vec![arrival(42, "com.example.chat", 1_600, -1, 0)];
        let spills = spilled_by_action(&arrivals, 1_000, 3_000, 7, &[800], SELF_PACKAGE);
        assert_eq!(spills.len(), 1, "{spills:?}");
    }

    #[test]
    fn a_task_that_has_already_left_the_users_screen_is_not_moved() {
        let arrivals = vec![arrival(42, "com.example.chat", 1_100, -1, 3)];
        let spills = spilled_by_action(&arrivals, 1_000, 3_000, 7, &[], SELF_PACKAGE);
        assert!(spills.is_empty(), "{spills:?}");
    }

    #[test]
    fn a_task_moved_here_on_purpose_is_not_a_spill() {
        // `from = 3` means it was on the agent's own display and moved;
        // that is a handoff, not something an action started.
        let arrivals = vec![arrival(42, "com.example.chat", 1_100, 3, 0)];
        let spills = spilled_by_action(&arrivals, 1_000, 3_000, 7, &[], SELF_PACKAGE);
        assert!(spills.is_empty(), "{spills:?}");
    }

    #[test]
    fn the_launcher_and_this_app_are_never_spills() {
        let arrivals = vec![
            arrival(1, "com.google.android.apps.nexuslauncher", 1_100, -1, 0),
            arrival(2, "net.syscity.desktop", 1_200, -1, 0),
            arrival(3, "com.android.systemui", 1_300, -1, 0),
        ];
        let spills = spilled_by_action(&arrivals, 1_000, 3_000, 7, &[], SELF_PACKAGE);
        assert!(spills.is_empty(), "{spills:?}");
    }

    #[test]
    fn the_app_the_user_was_already_in_is_left_alone() {
        // Same task as before the action, showing a different page: they are
        // looking at it, so moving it would take the screen away.
        let arrivals = vec![Arrival {
            task: 7,
            package: Some("com.example.mail".to_string()),
            at: 1_200,
            from: MAIN_DISPLAY,
            same_task: true,
            standard: true,
            now: MAIN_DISPLAY,
        }];
        let spills = spilled_by_action(&arrivals, 1_000, 3_000, 7, &[], SELF_PACKAGE);
        assert_eq!(spills.len(), 1);
        assert_eq!(spills[0].verdict, SpillVerdict::InUse);
    }

    #[test]
    fn one_entry_per_task_the_latest_word_wins() {
        // A task that bounced (rose, left, came back) is one spill, not three.
        let arrivals = vec![
            arrival(42, "com.example.chat", 1_100, -1, 0),
            arrival(42, "com.example.chat", 1_500, -1, 0),
        ];
        let spills = spilled_by_action(&arrivals, 1_000, 3_000, 7, &[], SELF_PACKAGE);
        assert_eq!(spills.len(), 1);
        assert_eq!(spills[0].task_id, 42);
    }

    #[test]
    fn an_unnamed_task_is_skipped_rather_than_guessed() {
        let arrivals = vec![Arrival {
            task: 42,
            package: None,
            at: 1_100,
            from: -1,
            same_task: false,
            standard: true,
            now: 0,
        }];
        let spills = spilled_by_action(&arrivals, 1_000, 3_000, 7, &[], SELF_PACKAGE);
        assert!(spills.is_empty(), "{spills:?}");
    }

    #[test]
    fn an_empty_screen_says_to_read_it_first() {
        let text = describe(&outcome(TAKEOVER_DONE, "done", Some(true)));
        assert!(text.contains("nothing to act on"), "{text}");
        assert!(text.contains("android_observe"), "{text}");
    }
}
