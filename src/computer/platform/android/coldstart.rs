//! Whether an app may be ended before it is started again, and what the launch
//! actually turned into.
//!
//! Opening an app with a clean slate means killing its process first — a
//! chat that reopens on the last conversation is not the same start as one
//! that reopens on its home screen. But killing a process is not local: the
//! app may also have a task on the user's own screen, or on another display
//! the agent opened, or in the background where the person left it. Ending it
//! would discard state that has nothing to do with this launch.
//!
//! So "restart" is a decision about who else is holding the app, and there are
//! three answers:
//!
//! * **In use, on the user's screen.** The person is looking at it. Do not
//!   start it at all — a launch would either be a no-op or pull the window out
//!   from under them.
//! * **Warm.** It has a task somewhere other than the display this launch is
//!   for. Start it normally and say the cold start was skipped, because the
//!   alternative is destroying somebody else's session.
//! * **Restart.** Every task it has is on this display, so ending the process
//!   costs nothing anyone is using.
//!
//! The same three-way decision Mika makes in `ColdStart.decide`; the terms are
//! the ones the rest of this module reads screens in.
//!
//! The second half of the file is the other fact a launch has to establish
//! before it can be reported: what came to the top of the display. "The command
//! was dispatched" is not "the app is there", and a chooser — the system asking
//! which app to open — is neither.

use super::display::ScreenTop;

/// What a launch should do about the app's existing process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdStart {
    /// End the process, then start the app: nothing else is using it.
    Restart,
    /// Start it without ending anything: it has a task elsewhere.
    Warm,
    /// The person is using it on their own screen. Do not launch.
    InUse,
}

impl ColdStart {
    /// Decide from where the app's tasks are.
    ///
    /// `task_displays` is every display the app currently has a task on, and
    /// `agent_display` is the one this launch is for. A task on the agent's own
    /// display is the one the restart would replace, so it does not count as
    /// someone else's.
    pub fn decide(task_displays: &[i32], agent_display: i32, visible_on_main: bool) -> Self {
        if visible_on_main {
            return Self::InUse;
        }
        if task_displays
            .iter()
            .any(|display| *display != agent_display)
        {
            return Self::Warm;
        }
        Self::Restart
    }

    /// What to do when the task list could not be read at all.
    ///
    /// Ending a process on a guess is the one move that cannot be undone, so an
    /// unreadable list is a warm start: it costs the clean slate and keeps
    /// whatever somebody else was holding. The person's own screen is still
    /// visible from the readiness report, so "in use" survives even here.
    pub fn unaware(visible_on_main: bool) -> Self {
        if visible_on_main {
            Self::InUse
        } else {
            Self::Warm
        }
    }

    /// Whether the app may be started at all.
    pub fn allowed(self) -> bool {
        !matches!(self, Self::InUse)
    }

    /// Whether the process may be ended first.
    pub fn ends_process(self) -> bool {
        matches!(self, Self::Restart)
    }

    /// The word the result carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Restart => "clean",
            Self::Warm => "warm",
            Self::InUse => "in_use",
        }
    }

    /// The sentence the result carries.
    pub fn describe(self, package: &str) -> String {
        match self {
            Self::Restart => {
                format!(
                    "`{package}` had no tasks anywhere else, so it was ended and started fresh."
                )
            }
            Self::Warm => {
                format!(
                    "`{package}` was started without being ended: it still has a task on another \
                     display, and ending it would have closed that one too. Its screens are \
                     whatever it left off on, not its home screen."
                )
            }
            Self::InUse => {
                format!(
                    "`{package}` is open on the user's own screen, so it was not started — \
                     launching it would either do nothing or pull the window out from under \
                     them. Ask them to close it, or work on a second display."
                )
            }
        }
    }
}

/// What came to the top of the display after the launch was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// The app is on top of the display this launch was for.
    Here { task: i32 },
    /// A chooser is on top — the system is asking which app should open, and a
    /// person (or the model, with `android_dialog`) has to answer it.
    Chooser { package: String },
    /// Something else is on top. The launch did not land here.
    Elsewhere { package: Option<String> },
    /// The display reported nothing: empty, or the helper could not say.
    Silent,
}

impl Opened {
    /// Classify the task on top of `display`, from a readiness report.
    pub fn of(top: Option<&ScreenTop>, package: &str) -> Self {
        let Some(top) = top else {
            return Self::Silent;
        };
        match top.package.as_deref() {
            Some(now) if now == package => Self::Here { task: top.task },
            Some(now) if super::takeover::is_chooser(now) => {
                Self::Chooser { package: now.to_string() }
            }
            Some(now) => Self::Elsewhere { package: Some(now.to_string()) },
            // A task with no package is something on screen that cannot be
            // named; it is not the app that was asked for.
            None => Self::Elsewhere { package: None },
        }
    }

    /// Whether the app itself reached the display.
    pub fn arrived(&self) -> bool {
        matches!(self, Self::Here { .. })
    }

    /// The word the result carries.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Here { .. } => "here",
            Self::Chooser { .. } => "chooser",
            Self::Elsewhere { .. } => "elsewhere",
            Self::Silent => "silent",
        }
    }

    /// The sentence the result carries after the launch was sent, when the app
    /// itself did not end up on top.
    ///
    /// `None` for [`Here`](Self::Here): the caller words that one, because it
    /// is the ordinary success. Every other arm says what *is* there, since
    /// "the command was dispatched" is not the fact the caller needs.
    pub fn describe(&self, package: &str, display: i32) -> Option<String> {
        match self {
            Self::Here { .. } => None,
            Self::Chooser { package: chooser } => Some(format!(
                "`{package}` did not open directly: the system raised a chooser ({chooser}) on \
                 display {display}, so it is asking which app should handle it. The chooser is \
                 what the next read will see — answer it with android_dialog, or pick the app \
                 on it, and the launch is then finished."
            )),
            Self::Elsewhere { package: Some(other) } => Some(format!(
                "`{package}` did not come to the front of display {display}: {other} is on top of \
                 it. The app may still be starting, or the launch went elsewhere — read the \
                 display before acting on it."
            )),
            Self::Elsewhere { package: None } => Some(format!(
                "`{package}` did not come to the front of display {display}: something on it \
                 cannot be named. Read the display before acting on it."
            )),
            Self::Silent => Some(format!(
                "`{package}` did not appear on display {display}: the display is empty or the \
                 helper could not say what is on it. Read the display before acting on it."
            )),
        }
    }
}

/// Whether a package is the user's launcher rather than an app.
///
/// A launcher task is where the phone sits, not something the person opened, so
/// it never means "in use" and never holds a launch back.
pub fn is_home(package: &str) -> bool {
    let lowered = package.to_ascii_lowercase();
    lowered.contains("launcher")
        || lowered == "com.android.systemui"
        || lowered.ends_with(".recents")
}

/// How often [`await_opened`] looks again while a launch is settling.
const OPEN_POLL_MS: u64 = 150;

/// Wait for the launch to turn into one of the answers that ends the wait.
///
/// `am start -W` returns when the activity reports idle, which is not the same
/// moment the task is on top of the display — and for a chooser it returns
/// without the app ever arriving. This looks until the app *or* a chooser is on
/// top, so the caller reports what is there rather than what was asked for.
///
/// `None` means the phone could not be asked at all. That is deliberately not
/// an error: the launch itself already succeeded or failed on its own answer,
/// and a state the host cannot observe must not be reported as a failure.
pub async fn await_opened(
    bridge: &crate::device::DeviceBridgeRef,
    display: i32,
    package: &str,
    wait: std::time::Duration,
) -> Option<Opened> {
    let state = super::display::VirtualDisplay::readiness(bridge)
        .await
        .ok()?;
    let mut opened = Opened::of(state.top_of(display), package);
    let deadline = tokio::time::Instant::now() + wait;
    while !matches!(opened, Opened::Here { .. } | Opened::Chooser { .. }) {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(OPEN_POLL_MS)).await;
        // A read that fails mid-wait keeps the last observation: the wait had
        // an answer a moment ago, and that is more than "nothing".
        if let Ok(state) = super::display::VirtualDisplay::readiness(bridge).await {
            opened = Opened::of(state.top_of(display), package);
        }
    }
    Some(opened)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: i32 = 3;

    fn top(display: i32, task: i32, package: &str) -> ScreenTop {
        ScreenTop {
            display,
            task,
            package: Some(package.to_string()),
        }
    }

    #[test]
    fn an_app_the_person_is_using_is_not_started_at_all() {
        // The main screen wins over everything: a launch there would either be
        // a no-op or take the window away from the person.
        assert_eq!(ColdStart::decide(&[0], AGENT, true), ColdStart::InUse);
        assert_eq!(ColdStart::decide(&[], AGENT, true), ColdStart::InUse);
        assert!(!ColdStart::decide(&[0], AGENT, true).allowed());
    }

    #[test]
    fn an_app_with_a_task_elsewhere_is_started_warm() {
        // Ending it would close that task too — the state the person or another
        // display is holding.
        let warm = ColdStart::decide(&[0], AGENT, false);
        assert_eq!(warm, ColdStart::Warm);
        assert!(warm.allowed());
        assert!(!warm.ends_process());
        assert!(warm.describe("com.chat").contains("another display"));
        // A task on a third display is the same answer as one on the user's.
        assert_eq!(ColdStart::decide(&[AGENT, 5], AGENT, false), ColdStart::Warm);
    }

    #[test]
    fn an_app_only_on_this_display_may_be_ended() {
        let clean = ColdStart::decide(&[AGENT, AGENT], AGENT, false);
        assert_eq!(clean, ColdStart::Restart);
        assert!(clean.ends_process());
        assert_eq!(clean.as_str(), "clean");
        // No task at all is the ordinary cold start, and it is still a restart.
        assert_eq!(ColdStart::decide(&[], AGENT, false), ColdStart::Restart);
    }

    #[test]
    fn an_unreadable_task_list_never_ends_a_process() {
        // `Warm` is the honest degradation: it keeps somebody else's task.
        assert_eq!(ColdStart::unaware(false), ColdStart::Warm);
        assert!(!ColdStart::unaware(false).ends_process());
        // What the readiness report does say — the person's own screen — is
        // still honoured.
        assert_eq!(ColdStart::unaware(true), ColdStart::InUse);
    }

    #[test]
    fn the_launcher_is_not_an_app_in_use() {
        assert!(is_home("com.google.android.apps.nexuslauncher"));
        assert!(is_home("com.android.launcher3"));
        assert!(is_home("com.android.systemui"));
        assert!(!is_home("com.tencent.mm"));
    }

    #[test]
    fn the_app_on_top_of_the_display_is_what_opened() {
        let here = Opened::of(Some(&top(AGENT, 11, "com.chat")), "com.chat");
        assert_eq!(here, Opened::Here { task: 11 });
        assert!(here.arrived());
        assert_eq!(here.as_str(), "here");
        assert!(here.describe("com.chat", AGENT).is_none());
    }

    #[test]
    fn a_chooser_is_not_the_app_and_is_said_in_its_own_words() {
        let chooser = Opened::of(Some(&top(AGENT, 12, "com.android.intentresolver")), "com.chat");
        assert_eq!(
            chooser,
            Opened::Chooser {
                package: "com.android.intentresolver".to_string()
            }
        );
        assert!(!chooser.arrived());
        let text = chooser.describe("com.chat", AGENT).expect("a sentence");
        assert!(text.contains("chooser"), "{text}");
        assert!(text.contains("android_dialog"), "{text}");
    }

    #[test]
    fn a_different_app_on_top_is_reported_rather_than_called_a_launch() {
        let elsewhere = Opened::of(Some(&top(AGENT, 12, "com.mail")), "com.chat");
        assert_eq!(
            elsewhere,
            Opened::Elsewhere {
                package: Some("com.mail".to_string())
            }
        );
        assert!(!elsewhere.arrived());
        assert!(elsewhere
            .describe("com.chat", AGENT)
            .expect("a sentence")
            .contains("com.mail"));
    }

    #[test]
    fn an_empty_display_is_not_a_launch() {
        let silent = Opened::of(None, "com.chat");
        assert_eq!(silent, Opened::Silent);
        assert!(!silent.arrived());
        assert_eq!(silent.as_str(), "silent");
        // An unnamed task is something on screen, not the app that was asked
        // for.
        let unnamed = Opened::of(
            Some(&ScreenTop {
                display: AGENT,
                task: 4,
                package: None,
            }),
            "com.chat",
        );
        assert_eq!(unnamed, Opened::Elsewhere { package: None });
    }

    // ── The wait after the launch was sent ────────────────────────────────

    /// A bridge that answers with one prepared readiness report per call, and
    /// keeps returning the last one.
    struct Sequence {
        answers: std::sync::Mutex<std::collections::VecDeque<Option<ScreenTop>>>,
        last: Option<ScreenTop>,
    }

    impl Sequence {
        fn new(answers: Vec<Option<ScreenTop>>) -> Self {
            Self {
                last: answers.last().cloned().flatten(),
                answers: std::sync::Mutex::new(answers.into()),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::device::DeviceBridge for Sequence {
        async fn call(
            &self,
            _command: &str,
            _payload: serde_json::Value,
        ) -> crate::Result<serde_json::Value> {
            let top = self
                .answers
                .lock()
                .map(|mut queue| queue.pop_front().unwrap_or_else(|| self.last.clone()))
                .unwrap_or_else(|_| self.last.clone());
            // The wire form `Readiness::parse` reads. An empty display is a
            // task of `-1`, not a missing entry: the parser drops it either
            // way, but only the first is what the helper actually sends.
            let displays = top
                .map(|top| {
                    serde_json::json!([{ "id": top.display, "task": top.task, "package": top.package }])
                })
                .unwrap_or_else(|| serde_json::json!([]));
            Ok(serde_json::json!({
                "ok": true,
                "helper": true,
                "locked": false,
                "main": { "task": -1, "package": null },
                "displays": displays,
            }))
        }
    }

    #[tokio::test]
    async fn a_display_already_showing_the_app_ends_the_wait_on_the_first_read() {
        let bridge: crate::device::DeviceBridgeRef =
            std::sync::Arc::new(Sequence::new(vec![Some(top(AGENT, 11, "com.chat"))]));
        let opened =
            await_opened(&bridge, AGENT, "com.chat", std::time::Duration::from_secs(5)).await;
        assert_eq!(opened, Some(Opened::Here { task: 11 }));
    }

    #[tokio::test]
    async fn a_chooser_ends_the_wait_without_the_app_ever_arriving() {
        // The first read is an empty display; the second is the chooser the
        // system raised instead of opening the app.
        let bridge: crate::device::DeviceBridgeRef = std::sync::Arc::new(Sequence::new(vec![
            None,
            Some(top(AGENT, 12, "com.android.intentresolver")),
        ]));
        let opened =
            await_opened(&bridge, AGENT, "com.chat", std::time::Duration::from_secs(5)).await;
        assert_eq!(
            opened,
            Some(Opened::Chooser {
                package: "com.android.intentresolver".to_string()
            })
        );
    }

    #[tokio::test]
    async fn an_app_that_never_arrives_is_reported_as_what_is_there() {
        // Everything that will ever be on this display is the mail app, so the
        // wait runs to its deadline and says so — it does not call the launch
        // a success because a command was dispatched.
        let bridge: crate::device::DeviceBridgeRef =
            std::sync::Arc::new(Sequence::new(vec![Some(top(AGENT, 12, "com.mail"))]));
        let opened =
            await_opened(&bridge, AGENT, "com.chat", std::time::Duration::from_millis(250)).await;
        assert_eq!(
            opened,
            Some(Opened::Elsewhere {
                package: Some("com.mail".to_string())
            })
        );
    }

    #[tokio::test]
    async fn an_unaskable_phone_is_not_an_answer() {
        // "The host cannot see this" must not be reported as "it did not
        // open": the launch already had its own verdict.
        struct Down;
        #[async_trait::async_trait]
        impl crate::device::DeviceBridge for Down {
            async fn call(
                &self,
                _command: &str,
                _payload: serde_json::Value,
            ) -> crate::Result<serde_json::Value> {
                Err(crate::error::SyscityError::Internal("no socket".into()))
            }
        }
        let bridge: crate::device::DeviceBridgeRef = std::sync::Arc::new(Down);
        assert_eq!(
            await_opened(&bridge, AGENT, "com.chat", std::time::Duration::from_secs(5)).await,
            None
        );
    }
}
