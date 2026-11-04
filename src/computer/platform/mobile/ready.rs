//! Whether a write still means what the caller thinks it means.
//!
//! Three things can be wrong with a phone between one step and the next, and
//! none of them is "the last action failed":
//!
//! - **The display helper is gone.** A killed process, a reboot, a phone that
//!   dropped the loopback session. Every task that writes through a display
//!   gets the same refusal, and repeating the action will keep getting it.
//! - **The screen is locked.** A tap on the user's screen lands on the lock
//!   screen; an app started behind it did not change, so the action is reported
//!   as having done something it did not.
//! - **The display no longer holds what it held.** The user pressed home on it,
//!   or the task moved elsewhere. Every coordinate the next step uses was
//!   computed against the screen that used to be there.
//!
//! Left alone, each of these reads to the model as a failed action, and a model
//! that thinks its action failed tries it again. This decides, before the
//! write, that there is nothing to retry — and says which of the three it is,
//! because the three need three different next moves: restart the helper, wait
//! for the person to unlock, or read the screen again.
//!
//! Mika calls this `TaskGate`. The shape is the same: a state check that runs
//! *before* the action and blocks it, rather than a verdict on the action
//! afterwards.

use super::android::Target;
use super::display::{Readiness, DISPLAY_UNAVAILABLE};

/// Why the phone is not ready for this write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    /// One of the constants below.
    pub code: &'static str,
    pub message: String,
}

/// A person holds the phone: locked, or asleep.
pub const PHONE_LOCKED: &str = "PHONE_LOCKED";
/// What [`PHONE_LOCKED`] means, said once so the diagnosis tool and the write
/// guard cannot drift into two wordings of the same fact.
pub const PHONE_LOCKED_HINT: &str = "the phone is locked (or asleep), so a tap on the user's \
     screen lands on the lock screen rather than the app. Nothing was done. Wait for the person \
     to unlock it, or use a second display, which the lock screen does not cover.";
/// The display helper is not running.
pub const HELPER_DOWN: &str = "HELPER_DOWN";
/// The display no longer shows the app it was showing.
pub const DISPLAY_MOVED: &str = "DISPLAY_MOVED";

impl Blocked {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

/// Decide whether `target` may be written to now.
///
/// `expected_package` is the app the caller believes is on that display, from
/// the read its coordinates came from. Only used for a virtual display: the
/// user's own screen is *supposed* to change under the agent, and a drift there
/// is what the effect check is for.
///
/// A caller with no helper (`None`) is driving the user's screen over ADB;
/// that path needs no state check beyond the lock, which it also cannot see —
/// so it is not checked at all rather than guessed at. That is the honest
/// limit of what ADB alone can tell.
pub fn check(
    readiness: Option<&Readiness>,
    target: Target,
    expected_package: Option<&str>,
) -> Result<(), Blocked> {
    let Some(readiness) = readiness else {
        return Ok(());
    };

    let Target::Display(id) = target else {
        // The user's own screen. A locked phone is still worth saying, because
        // the tap lands on the lock screen and the agent will be told the
        // screen changed when nothing it meant to touch did.
        return if readiness.locked {
            Err(Blocked::new(PHONE_LOCKED, PHONE_LOCKED_HINT))
        } else {
            Ok(())
        };
    };

    if !readiness.helper {
        return Err(Blocked::new(
            HELPER_DOWN,
            format!(
                "the display helper is not running, so display {id} cannot be written to. \
                 Nothing was done. Start it with android_display action 'start' (or 'restart' if \
                 it was running before), then re-read the display — its contents did not survive."
            ),
        ));
    }

    match readiness.top_of(id) {
        // An empty display is a display the agent is about to launch into, or
        // one whose app it just closed; neither is "moved".
        None => Ok(()),
        Some(top) => match (expected_package, top.package.as_deref()) {
            // The caller named no app (a gesture on a blank screen, a key
            // press), so there is nothing to compare.
            (None, _) => Ok(()),
            (Some(expected), Some(now)) if expected == now => Ok(()),
            (Some(expected), now) => Err(Blocked::new(
                DISPLAY_MOVED,
                format!(
                    "display {id} no longer shows the app this step was computed against — it \
                     was {expected}, it is {}. Nothing was done. Read the display again \
                     (android_ui_tree with display {id}) and recompute the coordinates.",
                    now.unwrap_or("nothing")
                ),
            )),
        },
    }
}

/// A refusal from [`check`], worded for the model and cut down to what the tool
/// result carries.
pub fn describe(blocked: &Blocked) -> String {
    blocked.message.clone()
}

/// The code a caller can branch on, alongside the message.
pub fn code_of(blocked: &Blocked) -> &'static str {
    blocked.code
}

/// Readiness straight from the phone, or `None` when this host cannot ask.
///
/// `None` means one of two things and they are both fine for a caller that only
/// wants the check: there is no Syscity app on this host to ask (a desktop
/// driving a phone over ADB), or asking failed in a way that is not a display
/// problem. The distinct "helper is down" answer comes back as
/// `Some(readiness)` with `helper == false`, because a phone that answered but
/// has no helper is exactly the state [`HELPER_DOWN`] is about.
pub async fn probe() -> Option<Readiness> {
    let bridge = crate::device::bridge()?.clone();
    match super::display::VirtualDisplay::readiness(&bridge).await {
        Ok(readiness) => Some(readiness),
        // The bridge answered but the call failed: the helper is gone in a way
        // that is not the app's own "not running" answer. Treat it as down,
        // which is what the caller would do with it.
        Err(failed) if failed.code == DISPLAY_UNAVAILABLE => Some(Readiness::default()),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> Readiness {
        Readiness {
            helper: true,
            locked: false,
            main: Some(super::super::display::ScreenTop {
                display: 0,
                task: 7,
                package: Some("com.example.chat".to_string()),
            }),
            displays: vec![super::super::display::ScreenTop {
                display: 3,
                task: 11,
                package: Some("com.example.chat".to_string()),
            }],
            tasks: Some(vec![
                super::super::display::ScreenTask {
                    display: 0,
                    task: 7,
                    package: Some("com.example.chat".to_string()),
                },
                super::super::display::ScreenTask {
                    display: 3,
                    task: 11,
                    package: Some("com.example.chat".to_string()),
                },
            ]),
        }
    }

    #[test]
    fn a_healthy_phone_blocks_nothing() {
        assert_eq!(check(Some(&ready()), Target::Display(3), Some("com.example.chat")), Ok(()));
    }

    #[test]
    fn a_locked_phone_is_named_as_a_wait() {
        let mut state = ready();
        state.locked = true;
        let blocked = check(Some(&state), Target::Screen, None).expect_err("locked");
        assert_eq!(blocked.code, PHONE_LOCKED);
        assert!(blocked.message.contains("unlock"), "{}", blocked.message);
        // The retry advice must not be "try again": retrying is what the ladder
        // is trying to prevent.
        assert!(!blocked.message.contains("try again"), "{}", blocked.message);
    }

    #[test]
    fn a_missing_helper_is_a_start_not_a_retry() {
        let mut state = ready();
        state.helper = false;
        let blocked = check(Some(&state), Target::Display(3), Some("com.example.chat"))
            .expect_err("no helper");
        assert_eq!(blocked.code, HELPER_DOWN);
        assert!(blocked.message.contains("action 'start'"), "{}", blocked.message);
    }

    #[test]
    fn a_display_holding_a_different_app_is_moved() {
        let blocked = check(Some(&ready()), Target::Display(3), Some("com.example.mail"))
            .expect_err("a different app");
        assert_eq!(blocked.code, DISPLAY_MOVED);
        assert!(blocked.message.contains("com.example.chat"), "{}", blocked.message);
        assert!(blocked.message.contains("com.example.mail"), "{}", blocked.message);
    }

    #[test]
    fn an_empty_display_is_not_moved() {
        // A display about to be launched into, or one whose app was just
        // closed, is not a display that moved.
        assert_eq!(check(Some(&ready()), Target::Display(9), Some("anything")), Ok(()));
    }

    #[test]
    fn a_step_that_named_no_app_is_not_judged_against_one() {
        // A key press or a gesture on a blank screen has no package to have
        // been computed from.
        let mut state = ready();
        state.displays[0].package = Some("com.example.other".to_string());
        assert_eq!(check(Some(&state), Target::Display(3), None), Ok(()));
    }

    #[test]
    fn the_user_screen_changing_app_is_not_a_block() {
        // The person is allowed to use their own phone while the agent works.
        let mut state = ready();
        state.main.as_mut().unwrap().package = Some("com.example.video".to_string());
        assert_eq!(check(Some(&state), Target::Screen, Some("com.example.chat")), Ok(()));
    }

    #[test]
    fn no_readiness_means_no_opinion() {
        // A desktop driving a phone over ADB cannot see the lock or the tasks.
        // Guessing would block work over a state that was never observed.
        assert_eq!(check(None, Target::Screen, None), Ok(()));
        assert_eq!(check(None, Target::Display(3), Some("x")), Ok(()));
    }

    #[test]
    fn the_three_states_are_three_different_answers() {
        // The point of the codes: restart, wait, or re-read — not one "failed".
        let codes: Vec<&str> = [PHONE_LOCKED, HELPER_DOWN, DISPLAY_MOVED].to_vec();
        let unique: std::collections::HashSet<_> = codes.iter().collect();
        assert_eq!(unique.len(), codes.len());
    }
}
