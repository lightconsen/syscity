//! Android device control via ADB.
//!
//! Provides screenshot, tap, swipe, text input, key events, app installation,
//! launch, force-stop, and a structured UI element list.
//!
//! Observation returns an indexed element list parsed from the platform's
//! `uiautomator dump`, not the raw XML: the dump is an order of magnitude
//! larger, and the model would otherwise have to parse `bounds` to act on it.
//! Text input escapes for the *device* shell (a plain `input text '<text>'`
//! breaks on an embedded quote) and routes non-ASCII through ADBKeyboard or,
//! failing that, through a clipboard paste by the phone's own helper — `input
//! text` types through the key character map and cannot carry it.
//!
//! Two things make the loop shorter rather than just correct. Every action is
//! followed by a re-read that answers "did the screen change" ([`EFFECT_CHANGED`]
//! / [`EFFECT_UNCHANGED`] / [`EFFECT_UNKNOWN`]), so the model does not need a
//! separate observation to learn what its tap did. And every tree read is judged
//! ([`assess_tree`]): a screen the accessibility API describes nothing useful on
//! falls back to OCR instead of handing back an empty list that reads like a
//! blank screen.

use serde_json::Value;

use super::common::{run_cmd, run_cmd_bytes};
use crate::computer::platform::{OsControlScope, PlatformConstraints, PlatformToolSet};
use crate::tools::target_lock::TargetLease;
use crate::tools::{create_schema, Tool, ToolContext, ToolExecutionResult};

pub mod a11y;
pub mod adb;
pub mod coldstart;
pub mod coords;
pub mod diagnose;
pub mod diff;
pub mod display;
pub mod gate;
pub mod guard;
pub mod knowledge;
pub mod pacing;
pub mod profiles;
pub mod query;
pub mod ready;
pub mod risk;
pub mod takeover;
pub mod tools;
pub mod visual;
pub mod web;
pub mod wheel;
pub mod wheel_typing;

// ── Android Capability Set ─────────────────────────────────────────────────

/// Android device platform tool set (requires `adb` on PATH).
pub struct AndroidToolset;

impl Default for AndroidToolset {
    fn default() -> Self {
        Self::new()
    }
}

impl AndroidToolset {
    pub fn new() -> Self {
        Self
    }
}

impl PlatformToolSet for AndroidToolset {
    fn id(&self) -> &str {
        "android"
    }

    fn name(&self) -> &str {
        "Android Device Bridge"
    }

    fn description(&self) -> &str {
        "Control Android devices via ADB: screenshot, tap, type, install, launch apps"
    }

    fn constraints(&self) -> &PlatformConstraints {
        // Availability is determined by `has_adb()` at runtime.
        static CONSTRAINTS: std::sync::OnceLock<PlatformConstraints> = std::sync::OnceLock::new();
        CONSTRAINTS.get_or_init(|| PlatformConstraints {
            target_os: Vec::<String>::new(), // any OS
            requires_gui: false,
            requires_services: Vec::<String>::new(),
        })
    }

    fn scope(&self) -> OsControlScope {
        OsControlScope::UserSpace
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(AdbObserveTool::new()),
            Box::new(AdbScreenshotTool::new()),
            Box::new(AdbInputTool::new()),
            Box::new(AdbAppManagerTool::new()),
            Box::new(AdbUiTreeTool::new()),
            Box::new(AdbVerifyTool::new()),
            // Higher-level tools: one call where the model would otherwise
            // spend several turns reading, scrolling and tapping.
            Box::new(self::tools::AdbCollectTool::new()),
            Box::new(self::tools::AdbFindTool::new()),
            // The two cheap reads: find one control without reading the whole
            // tree, and wait for the screen instead of observing it again.
            Box::new(self::tools::AdbQueryTool::new()),
            Box::new(self::tools::AdbWaitTool::new()),
            // One call for several known steps. Each step still goes through
            // `android_input`/`android_app_manager`, so nothing a single call
            // checks is skipped — what it saves is the model's turns.
            Box::new(self::tools::AdbRunTool::new()),
            // The same screen, read as data rather than as tap targets.
            Box::new(self::tools::AdbExtractTool::new()),
            // What the phone says has happened, without reading the screen:
            // windows, content changes and notifications, as facts.
            Box::new(self::tools::AdbEventTool::new()),
            // One call to turn a wheel (a time, a date, a region): the control
            // a model otherwise drives one tap at a time.
            Box::new(self::tools::AdbSetWheelTool::new()),
            Box::new(self::tools::AdbDialogTool::new()),
            Box::new(self::tools::AdbSystemActionTool::new()),
            Box::new(self::tools::AdbOcrTool::new()),
            // Why this screen looks the way it does: a blank, a spinner, a
            // protected window, or a person the agent must hand it to.
            Box::new(self::tools::AdbDiagnoseTool::new()),
            // The phone's own browser: a page that draws itself in JavaScript
            // has no nodes for the accessibility tree, so it is read and
            // pressed through the DOM instead.
            Box::new(self::web::AdbWebTool::new()),
            // Finding a control the tree cannot name: the tree first, then OCR
            // of the screenshot, then the configured vision provider for an
            // icon with no text — and a second screenshot to prove the box
            // still points where the match was read.
            Box::new(self::visual::AdbFindVisuallyTool::new()),
            // The second screen: the user keeps the phone while a task runs,
            // and a step they have to do themselves goes back to them.
            Box::new(self::tools::AdbDisplayTool::new()),
            Box::new(self::tools::AdbHandToUserTool::new()),
            // Loopback self-pairing (§4.5): pair the phone with its own
            // wireless-debugging adbd and report pairing state.
            Box::new(AdbPairTool::new()),
            Box::new(AdbStatusTool::new()),
        ]
    }

    fn is_available(&self) -> bool {
        has_adb()
    }

    /// The six tools a turn needs before it has decided anything.
    ///
    /// Reading the screen, pressing at a point, typing, opening an app, asking
    /// the screen what changed, and handing the phone back to its person. Every
    /// other tool in the set is a specialisation of one of these — a different
    /// way to read, a longer way to press — and is opened when the turn says
    /// what it is trying to do.
    fn core_tools(&self) -> &'static [&'static str] {
        &[
            "android_observe",
            "android_input",
            "android_app_manager",
            "android_verify",
            "android_wait",
            "android_hand_to_user",
        ]
    }

    fn tool_groups(&self) -> &'static [crate::computer::platform::ToolGroupSpec] {
        TIER_GROUPS
    }

    /// Android makes all three of the shared judgments.
    ///
    /// Every action is followed by a settle that classifies the effect, a write
    /// is read back from the field, and the accessibility service reports
    /// events a wait can be ended by. The words and the rules are the common
    /// ones in [`crate::computer::platform::screen`]; what this declaration
    /// adds is the statement that they are answers here, not fallbacks.
    fn screen_semantics(&self) -> crate::computer::platform::screen::ScreenSemantics {
        crate::computer::platform::screen::ScreenSemantics::ALL
    }
}

/// The optional groups, by what a turn is trying to do.
///
/// Grouped by task rather than by module: a model that has decided to find a
/// control needs `android_find`, `android_query` and `android_ui_tree`
/// regardless of which file they live in.
static TIER_GROUPS: &[crate::computer::platform::ToolGroupSpec] = &[
    crate::computer::platform::ToolGroupSpec {
        id: "android.read",
        about: "Read the screen in a form other than the default: the raw tree, \
                an extract of a list, screenshots, or a diff.",
        tools: &[
            "android_ui_tree",
            "android_screenshot",
            "android_extract",
            "android_collect",
        ],
    },
    crate::computer::platform::ToolGroupSpec {
        id: "android.locate",
        about: "Find one control by name, by OCR, or by the model — for a screen whose \
                tree does not name what the task calls it.",
        tools: &[
            "android_find",
            "android_query",
            "android_ocr",
            "android_find_visually",
        ],
    },
    crate::computer::platform::ToolGroupSpec {
        id: "android.sequence",
        about: "Run a known sequence of steps in one call, for a task long enough that \
                reading between every step would cost more than doing it.",
        tools: &["android_run"],
    },
    crate::computer::platform::ToolGroupSpec {
        id: "android.system",
        about: "The phone's own controls: dialogs, system actions, a wheel, and the \
                accessibility events that say when something changed.",
        tools: &[
            "android_dialog",
            "android_system_action",
            "android_set_wheel",
            "android_event",
        ],
    },
    crate::computer::platform::ToolGroupSpec {
        id: "android.web",
        about: "The phone's browser, driven through the page instead of the tree.",
        tools: &["android_web"],
    },
    crate::computer::platform::ToolGroupSpec {
        id: "android.diagnose",
        about: "Why a screen looks the way it does — a blank, a spinner, a protected \
                window — and the second display for a task the person keeps.",
        tools: &["android_diagnose", "android_display"],
    },
    crate::computer::platform::ToolGroupSpec {
        id: "android.connect",
        about: "Pairing the phone with its own loopback adbd, and reporting the state \
                of that connection.",
        tools: &["device_adb_pair", "device_adb_status"],
    },
];

// ── Device targeting ───────────────────────────────────────────────────────

/// Schema text for the `device` argument the targeting tools share.
/// Shared description for the `display` argument.
pub(super) const DISPLAY_PARAM: &str =
    "Optional: id from android_display start. Omit to act on the phone's own screen.";

pub(super) const DEVICE_PARAM: &str =
    "Device serial to act on. Omit it and the call uses the only attached device — the result \
     reports which serial was actually used, since that is whatever is attached at the time.";

pub(super) const SINCE_PARAM: &str =
    "Optional: the `snapshot` of an earlier read, to get only what changed since then (what \
     appeared, left, moved) instead of the whole screen. Falls back to the whole screen when the \
     named read is not one of the last few, was of a different app, or when more than half the \
     screen is different — the result says which.";

/// `adb` argv prefix for the target device — no `-s` when none is pinned, in
/// which case adb resolves the device itself.
pub(super) fn adb_args(device: &Option<String>, base: &[&str]) -> Vec<String> {
    let mut args = Vec::with_capacity(base.len() + 2);
    if let Some(serial) = device {
        args.push("-s".to_string());
        args.push(serial.clone());
    }
    args.extend(base.iter().map(|s| s.to_string()));
    args
}

/// The device a call acts on: the caller's argument when given, else the serial
/// the toolset was configured with.
///
/// Omitting `device` does not mean "any device" — adb resolves it to whichever
/// single device is attached at that moment, so a call made after a swap lands
/// on the new one without saying so. Passing it pins this call's target.
pub(super) fn resolve_device(
    args: &Value,
    configured: Option<&str>,
) -> Result<Option<String>, String> {
    match args.get("device") {
        // Absent, or explicitly null: fall back to the configured serial.
        None | Some(Value::Null) => Ok(configured.map(str::to_string)),
        Some(Value::String(serial)) if !serial.trim().is_empty() => {
            Ok(Some(serial.trim().to_string()))
        }
        // An empty or non-string serial must not quietly become "no device":
        // that sends the command to a target the caller did not ask for, which
        // is the whole failure this argument exists to prevent.
        Some(Value::String(_)) => {
            Err("`device` was given but is empty — omit it to use the only attached device"
                .to_string())
        }
        Some(other) => Err(format!("`device` must be a serial string, got {other}")),
    }
}

/// The serial this call actually addressed, for the result to report.
///
/// A pinned serial is known for free; otherwise ask adb. Naming the device is
/// what turns a silent swap into a visible line in the transcript. Best-effort
/// by design — nothing about labelling the target is worth failing a call over —
/// so a failure here surfaces as a null in the result rather than as silence.
pub(super) async fn resolved_serial(device: &Option<String>) -> Option<String> {
    if let Some(serial) = device {
        return Some(serial.clone());
    }
    let (status, stdout, _) = run_cmd("adb", &["get-serialno"]).await.ok()?;
    if !status.success() {
        return None;
    }
    let serial = stdout.trim();
    if serial.is_empty() || serial == "unknown" {
        None
    } else {
        Some(serial.to_string())
    }
}

/// The key to hold while acting on `serial`.
///
/// A pinned serial is a phone of its own, and two phones on one host are not
/// each other's problem. Without a serial the call addresses whatever adb has
/// attached, which is not a device this process can name — so it takes the
/// kind-wide lock, which is exactly what the old coarse key was, and for the
/// right reason.
pub(crate) fn device_lock_key(device: &Option<String>) -> String {
    match device {
        Some(serial) => crate::tools::target_lock::android_device(serial),
        None => crate::tools::target_lock::ANDROID.to_string(),
    }
}

/// Wait for exclusive use of the device this call is about to address.
///
/// `&str` rather than the key itself: the guard does not borrow it, so the key
/// may be built in the match arm that resolves the device.
///
/// Taken across processes: two `adb` drivers on one host — the gateway and the
/// CLI — are two writers to one phone. See [`TargetLease`].
pub(super) async fn lock_device(device: &Option<String>) -> TargetLease {
    let key = device_lock_key(device);
    crate::tools::target_lock::TargetLocks::global()
        .lock_across_processes(&key)
        .await
}

/// Wait for exclusive use of the thing this call is about to address.
///
/// The user's screen has one pointer and one focus, so a write to it takes the
/// device. A virtual display has a focus of its own: two of them are two
/// places, and making them wait for each other would be cost with no
/// correctness behind it. The app they share is guarded separately, by
/// [`lease_app`].
pub(super) async fn lock_target(device: &Option<String>, target: Target) -> TargetLease {
    match target {
        Target::Screen => lock_device(device).await,
        Target::Display(id) => {
            let key = match device {
                Some(serial) => crate::tools::target_lock::android_display(serial, id),
                None => format!("android-display:any:{id}"),
            };
            crate::tools::target_lock::TargetLocks::global()
                .lock_across_processes(&key)
                .await
        }
    }
}

/// Wait for exclusive use of one app on one phone, for the length of a write.
///
/// The device lock serialises input on a phone, and on the user's screen that
/// is all there is to it: one focus, one screen. A virtual display is not the
/// user's screen though — it has its own focus — and the helper lets two of
/// them run at once. Two tasks can then drive the *same app* on *two displays*
/// of one phone, which the display-side checks cannot see: the app is one
/// process, one account, one set of unsaved edits, and two writers interleaving
/// in it produce a state neither of them would have reached alone.
///
/// So a write also takes this, keyed by `(serial, package)`. It is taken after
/// the pre-action read (the package is what that read names) and released when
/// the action finishes. Order is always the display-or-device lock first and
/// this second, so the two cannot deadlock.
///
/// [`stale`](AppLease::stale) covers the gap the ordering leaves: the read that
/// named the package happened *before* the lock, so another write to the same
/// app may have landed in between. When it says so, the caller re-reads.
pub(super) type AppLease = crate::tools::target_lock::AppLease;

/// Take the app lease for the package the pre-action read named.
///
/// An unreadable tree names no package, and there is no key to take: the write
/// proceeds under the display lock only, as it did before. That is a real gap
/// — a blind action is exactly one you cannot serialise — but inventing a key
/// would serialise every app on the phone against every other, which is the
/// coarse lock this whole module is trying not to be.
pub(super) async fn lease_app(
    device: &Option<String>,
    package: Option<&str>,
    seen_completed: u64,
) -> AppLease {
    let Some(package) = package.map(str::trim).filter(|p| !p.is_empty()) else {
        return AppLease::none();
    };
    let key = crate::tools::target_lock::android_app(device.as_deref().unwrap_or("-"), package);
    crate::tools::target_lock::AppGates::global()
        .lease(&key, seen_completed)
        .await
}

/// How many writes to `package` have finished, for [`lease_app`]'s staleness
/// check. Zero when no package is known, which never reports stale.
pub(super) fn app_completed(device: &Option<String>, package: Option<&str>) -> u64 {
    let Some(package) = package.map(str::trim).filter(|p| !p.is_empty()) else {
        return 0;
    };
    crate::tools::target_lock::AppGates::global().completed(
        &crate::tools::target_lock::android_app(device.as_deref().unwrap_or("-"), package),
    )
}

// The tools themselves live in files of their own (`observe`, `input`,
// `apps`, `tree`, `dump`, `feedback`, `ocr`, `checks`, `verify`, `pairing`).
// Their items are re-exported here so a sibling names one without a path,
// exactly as it did when this was a single file.
mod apps;
mod checks;
mod dump;
mod feedback;
mod input;
mod observe;
mod ocr;
mod pairing;
mod tree;
mod verify;

pub use adb::*;
pub use apps::*;
use checks::*;
pub use dump::*;
pub use feedback::*;
pub use input::*;
pub use observe::*;
pub use ocr::*;
pub use pairing::*;
pub use tree::*;
pub use verify::*;

#[cfg(test)]
mod tests {
    use super::*;

    // ── What an input is, before it happens ─────────────────────────────
    //
    // The gate now covers text and key events as well as taps, and the
    // judgement has to be the same both times it is made — before dispatch,
    // and again after an approval. These cover the shape of that judgement
    // without a phone: a `classify_input` that disagrees with itself would
    // refuse every approved action as "no longer there".

    fn text_field(label: &str, bounds: (i32, i32, i32, i32), focused: bool) -> UiElement {
        UiElement::with_state(
            1,
            Some(label.to_string()),
            None,
            None,
            "EditText".to_string(),
            true,
            true,
            NodeState {
                editable: true,
                focused,
                ..NodeState::default()
            },
            bounds,
        )
    }

    #[test]
    fn a_sequence_is_judged_by_the_strongest_step_under_it() {
        let elements = vec![UiElement::new(
            1,
            Some("立即付款".to_string()),
            None,
            None,
            "Button".to_string(),
            true,
            true,
            (0, 0, 400, 100),
        )];
        let op = InputOp::Taps(vec![(100, 50), (900, 900)]);
        let offer = classify_input(&op, &elements, &Value::Null, None).expect("a payment");
        assert_eq!(offer.risk, super::risk::Risk::Pay);
    }

    #[test]
    fn a_typed_line_break_is_a_send_and_plain_typing_is_not() {
        let box_ = text_field("", (0, 0, 800, 120), true);
        let args = Value::Null;
        assert_eq!(
            classify_input(&InputOp::Text("你好".to_string()), &[box_.clone()], &args, None),
            None
        );
        let offer = classify_input(&InputOp::Text("你好\n".to_string()), &[box_], &args, None)
            .expect("a send");
        assert_eq!(offer.risk, super::risk::Risk::Send);
        assert_eq!(offer.matched, "换行");
    }

    #[test]
    fn an_activating_key_is_judged_and_a_back_key_is_not() {
        let box_ = text_field("", (0, 0, 800, 120), true);
        let args = Value::Null;
        assert_eq!(
            classify_input(&InputOp::Key("KEYCODE_BACK".to_string()), &[box_.clone()], &args, None),
            None
        );
        assert!(classify_input(&InputOp::Key("KEYCODE_ENTER".to_string()), &[box_], &args, None)
            .is_some());
    }

    #[test]
    fn a_swipe_moves_the_screen_without_pressing_anything() {
        let op = InputOp::Swipe { from: (0, 0), to: (10, 10) };
        assert_eq!(classify_input(&op, &[], &Value::Null, None), None);
    }

    #[test]
    fn the_question_names_the_field_the_text_lands_in() {
        // A person reading "type text and press enter" has no idea which
        // conversation it is; the field's own label is what makes it a
        // question worth answering.
        let offer = super::risk::Offer {
            risk: super::risk::Risk::Send,
            matched: "换行".to_string(),
            element: Some("和小明的对话".to_string()),
        };
        let subject = subject_for(
            &InputOp::Text("好的\n".to_string()),
            Some(&offer),
            None,
            Some("com.example.chat"),
        );
        assert!(subject.contains("和小明的对话"), "{subject}");
        assert!(subject.contains("com.example.chat"), "{subject}");
    }

    // ── Launch entry resolution ─────────────────────────────────────────
    //
    // `launch` used to guess `pkg/.MainActivity`. These cover the two parsers
    // that replaced the guess: what the package manager answers, and what
    // `am start -W` says about whether it worked. Both are pure so they run
    // without a device — which is the only way they get run at all.

    #[test]
    fn resolve_activity_reads_the_brief_form() {
        // API 26+ `--brief` output, verbatim.
        assert_eq!(
            parse_resolve_activity("com.tencent.mm/.ui.LauncherUI\n"),
            Some("com.tencent.mm/.ui.LauncherUI".to_string())
        );
    }

    #[test]
    fn resolve_activity_skips_the_preamble() {
        let out = "priority=0\npackageName=com.example\ncom.example/.MainActivity\n";
        assert_eq!(parse_resolve_activity(out), Some("com.example/.MainActivity".to_string()));
    }

    #[test]
    fn resolve_activity_returns_none_for_an_error() {
        // What the package manager prints when nothing matches.
        assert_eq!(parse_resolve_activity("Error: Activity not found\n"), None);
        assert_eq!(parse_resolve_activity(""), None);
    }

    #[test]
    fn resolve_activity_ignores_lines_that_are_not_components() {
        let out = "priority=0\nsome warning\ncom.example/.Main\n";
        assert_eq!(parse_resolve_activity(out), Some("com.example/.Main".to_string()));
    }

    #[test]
    fn am_start_ok_is_started() {
        let out = "Starting: Intent { cmp=com.example/.Main }\nStatus: ok\nActivity: com.example/.Main\nThisTime: 412\n";
        assert_eq!(parse_am_start_output(out, ""), LaunchOutcome::Started);
    }

    #[test]
    fn already_in_front_counts_as_started() {
        // Android prints this instead of `Status:` when the task is already top.
        // Treating it as failure would report every second launch of a warm app
        // as broken.
        let warning =
            "Warning: Activity not started, its current task has been brought to the front";
        assert_eq!(parse_am_start_output("", warning), LaunchOutcome::AlreadyInFront);
    }

    #[test]
    fn error_wins_over_a_later_status() {
        let out = "Error: Activity class {com.example/.Main} does not exist.\nStatus: ok\n";
        assert_eq!(parse_am_start_output(out, ""), LaunchOutcome::Failed);
    }

    #[test]
    fn status_other_than_ok_is_failed() {
        assert_eq!(parse_am_start_output("Status: timeout\n", ""), LaunchOutcome::Failed);
    }

    #[test]
    fn silent_success_is_unknown_not_ok() {
        // Exit zero with no verdict means the command ran and told us nothing.
        // Assuming success here is exactly the "dispatched, not started" bug.
        assert_eq!(parse_am_start_output("", ""), LaunchOutcome::Unknown);
        assert_eq!(
            parse_am_start_output("Starting: Intent { cmp=a/b }\n", ""),
            LaunchOutcome::Unknown
        );
    }

    // ── Stable references ───────────────────────────────────────────────
    //
    // An index is a position in one reading of the screen; a key is the
    // element itself. The tests below are about the property that matters: the
    // same element found again after the list moved, and a refusal rather than
    // a guess when it cannot be.

    /// A labelled button at `bounds`, numbered as the parser would number it.
    fn labelled_at(index: usize, text: &str, bounds: (i32, i32, i32, i32)) -> UiElement {
        UiElement::new(
            index,
            Some(text.to_string()),
            None,
            Some(format!("com.x:id/{}", text.replace(' ', "_"))),
            "Button".to_string(),
            true,
            true,
            bounds,
        )
    }

    #[test]
    fn a_key_does_not_depend_on_where_the_element_is() {
        let top = labelled_at(1, "Alice", (0, 0, 100, 100));
        let scrolled = labelled_at(1, "Alice", (0, 400, 100, 500));
        assert_eq!(top.key, scrolled.key);
        assert_ne!(top.center, scrolled.center, "the test needs them to have moved");
    }

    #[test]
    fn different_elements_have_different_keys() {
        assert_ne!(
            labelled_at(1, "Alice", (0, 0, 100, 100)).key,
            labelled_at(1, "Bob", (0, 0, 100, 100)).key
        );
    }

    #[test]
    fn a_key_is_found_again_after_the_list_scrolled() {
        let read_before = vec![
            labelled_at(1, "Alice", (0, 0, 100, 100)),
            labelled_at(2, "Bob", (0, 100, 100, 200)),
        ];
        let key = read_before[1].key.clone();
        // A new row arrived above, so Bob is element 3 now and is elsewhere.
        let read_after = vec![
            labelled_at(1, "Zed", (0, 0, 100, 100)),
            labelled_at(2, "Alice", (0, 100, 100, 200)),
            labelled_at(3, "Bob", (0, 200, 100, 300)),
        ];
        let found = relocate(&read_after, &key).expect("relocated");
        assert_eq!(element_label(found), Some("Bob"));
        assert_eq!(found.index, 3);
    }

    #[test]
    fn a_key_that_is_gone_is_a_refusal_not_a_guess() {
        let elements = vec![labelled_at(1, "Alice", (0, 0, 100, 100))];
        let err = relocate(&elements, "deadbeef").expect_err("refused");
        assert!(err.starts_with("STALE_REF"), "{err}");
        assert!(err.contains("re-read"), "{err}");
    }

    #[test]
    fn two_indistinguishable_elements_are_refused_and_listed() {
        // Two rows with the same label, class and resource id: picking the
        // first would be the silent wrong-target failure the key exists for.
        let elements = vec![
            labelled_at(1, "Row", (0, 0, 100, 100)),
            labelled_at(2, "Row", (0, 200, 100, 300)),
        ];
        let err = relocate(&elements, &elements[0].key).expect_err("refused");
        assert!(err.contains("2 elements match"), "{err}");
        assert!(err.contains("[1]"), "{err}");
        assert!(err.contains("[2]"), "{err}");
    }

    #[test]
    fn an_empty_key_never_matches_anything() {
        let mut element = labelled_at(1, "Alice", (0, 0, 100, 100));
        element.key.clear();
        assert!(relocate(&[element], "").is_err());
    }

    // ── Text input encoding ─────────────────────────────────────────────
    //
    // `adb shell <cmd>` hands the string to the *device* shell, so the text has
    // to survive two layers: the shell's word parsing, then `input`'s own `%s`
    // convention. Getting either wrong used to send a malformed command.

    #[test]
    fn plain_text_is_a_single_quoted_word() {
        assert_eq!(encode_input_text("hello"), "input text 'hello'");
    }

    #[test]
    fn spaces_use_inputs_percent_s_convention() {
        assert_eq!(encode_input_text("hello world"), "input text 'hello%sworld'");
    }

    #[test]
    fn a_literal_percent_is_doubled() {
        assert_eq!(encode_input_text("100%"), "input text '100%%'");
        // Doubling happens before spaces are converted, so the `%` that
        // introduces `%s` is not itself doubled.
        assert_eq!(encode_input_text("50% off"), "input text '50%%%soff'");
    }

    #[test]
    fn an_embedded_quote_cannot_end_the_command() {
        // The old encoding produced `input text 'it's'`, which the device shell
        // reads as the command `input text 'it'` followed by `s'` — the text
        // after the quote was interpreted as shell, not typed.
        assert_eq!(encode_input_text("it's"), r#"input text 'it'\''s'"#);
    }

    #[test]
    fn shell_metacharacters_are_inert() {
        let cmd = encode_input_text("a; rm -rf / && echo $(whoami) `id` | tee > /tmp/x");
        // Exactly one command, wrapped as one quoted word: the metacharacters are
        // data, not syntax.
        assert_eq!(cmd.matches("input text").count(), 1);
        assert!(cmd.starts_with("input text '"), "got {cmd}");
        assert!(cmd.ends_with('\''), "got {cmd}");
        assert!(cmd.contains("$(whoami)"), "got {cmd}");
    }

    #[test]
    fn a_keycode_name_works_on_both_dispatch_paths() {
        // `input keyevent` takes both spellings, the display helper only takes
        // the field name — so the bare name is normalised where the op is built.
        assert_eq!(normalize_keycode("ENTER"), "KEYCODE_ENTER");
        assert_eq!(normalize_keycode(" enter "), "KEYCODE_ENTER");
        assert_eq!(normalize_keycode("KEYCODE_ENTER"), "KEYCODE_ENTER");
        // A number is accepted by both dispatchers and must not be prefixed.
        assert_eq!(normalize_keycode("66"), "66");
        assert_eq!(normalize_keycode(""), "");
    }

    #[test]
    fn newlines_become_enter_keyevents() {
        assert_eq!(
            encode_input_text("line1\nline2"),
            "input text 'line1'; input keyevent 66; input text 'line2'"
        );
    }

    #[test]
    fn blank_lines_survive_as_paragraph_breaks() {
        assert_eq!(
            encode_input_text("a\n\nb"),
            "input text 'a'; input keyevent 66; input keyevent 66; input text 'b'"
        );
    }

    #[test]
    fn carriage_returns_from_crlf_are_stripped() {
        assert_eq!(
            encode_input_text("a\r\nb"),
            "input text 'a'; input keyevent 66; input text 'b'"
        );
    }

    #[test]
    fn non_ascii_is_not_silently_mangled_here() {
        // The helper passes non-ASCII through unchanged; deciding what to *do*
        // with it (ADBKeyboard vs an explicit error) happens in `execute`, which
        // needs a device. This pins that the helper itself does not corrupt it.
        let cmd = encode_input_text("登录");
        assert_eq!(cmd, "input text '登录'");
    }

    // ── UI tree parsing ─────────────────────────────────────────────────

    /// A dump in the shape `uiautomator dump` actually produces: nested
    /// containers, a text field, a labelled button, a node whose label lives in
    /// `content-desc`, an escaped entity, and bare layout containers that carry
    /// nothing actionable.
    const DUMP: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?>
<hierarchy rotation="0">
  <node index="0" class="android.widget.FrameLayout" text="" resource-id="" bounds="[0,0][1080,2340]">
    <node index="1" class="android.widget.TextView" text="登录 &amp; 注册" resource-id="com.app:id/title" bounds="[100,200][980,300]" enabled="true" clickable="false" />
    <node index="2" class="android.widget.Button" text="登录" resource-id="com.app:id/login" bounds="[120,840][360,920]" enabled="true" clickable="true" />
    <node index="3" class="android.widget.ImageButton" text="" content-desc="返回" resource-id="" bounds="[0,100][100,200]" enabled="true" clickable="true" />
    <node index="4" class="android.widget.EditText" text="" resource-id="com.app:id/phone" bounds="[120,500][960,580]" enabled="false" clickable="true" />
    <node index="5" class="android.view.View" text="" resource-id="" bounds="[0,300][1080,400]" enabled="true" clickable="false" scrollable="false" />
    <node index="6" class="android.widget.EditText" text="" resource-id="com.app:id/code" bounds="[120,600][960,680]" enabled="true" clickable="false" password="true" focused="true" />
    <node index="7" class="androidx.recyclerview.widget.RecyclerView" text="" resource-id="" bounds="[0,1000][1080,2000]" enabled="true" clickable="false" scrollable="true">
      <node index="0" class="android.widget.CheckBox" text="记住我" resource-id="" bounds="[120,1020][400,1100]" enabled="true" clickable="true" checkable="true" checked="true" />
    </node>
  </node>
</hierarchy>"#;

    #[test]
    fn parses_bounds_attribute() {
        assert_eq!(parse_bounds("[0,0][1080,2340]"), Some((0, 0, 1080, 2340)));
        assert_eq!(parse_bounds("[120,840][360,920]"), Some((120, 840, 360, 920)));
        assert_eq!(parse_bounds("garbage"), None);
        assert_eq!(parse_bounds("[1,2]"), None);
    }

    #[test]
    fn keeps_only_actionable_or_labelled_nodes() {
        let els = parse_uiautomator_xml(DUMP).expect("parses");
        // The bare FrameLayout and the unlabelled non-clickable View are dropped.
        let classes: Vec<&str> = els.iter().map(|e| e.class.as_str()).collect();
        assert!(!classes.contains(&"FrameLayout"), "container kept: {classes:?}");
        // The unnamed code field and the scroll list are kept despite having no
        // name: one is where a write goes, the other is where a scroll goes.
        assert!(classes.contains(&"EditText"), "{classes:?}");
        assert!(classes.contains(&"RecyclerView"), "{classes:?}");
        assert_eq!(els.len(), 7, "{classes:?}");
    }

    #[test]
    fn xml_semantics_reach_the_element() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        let code = els
            .iter()
            .find(|e| e.resource_id.as_deref() == Some("com.app:id/code"))
            .unwrap();
        assert!(code.state.password);
        assert!(code.state.focused);
        assert!(code.state.editable);
        let list = els.iter().find(|e| e.class == "RecyclerView").unwrap();
        assert!(list.state.scrollable);
        assert!(list.state.scroll_forward && list.state.scroll_backward);
        let check = els.iter().find(|e| e.class == "CheckBox").unwrap();
        assert!(check.state.checkable && check.state.checked);
    }

    #[test]
    fn depth_is_counted_from_the_root_node() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        let title = els.iter().find(|e| e.class == "TextView").unwrap();
        let list = els.iter().find(|e| e.class == "RecyclerView").unwrap();
        let check = els.iter().find(|e| e.class == "CheckBox").unwrap();
        assert_eq!(title.state.depth, 1);
        assert_eq!(list.state.depth, 1);
        // The checkbox sits under the list, so a scroll aimed at the midpoint
        // would pick the list and not the row.
        assert_eq!(check.state.depth, 2);
    }

    #[test]
    fn a_password_field_is_not_read_out() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        let summary = format_ui_summary(&els);
        assert!(summary.contains("[password]"), "{summary}");
        assert!(!summary.contains("com.app:id/code"), "{summary}");
        assert!(summary.contains("editable"), "{summary}");
        assert!(summary.contains("checked"), "{summary}");
        assert!(summary.contains("scrollable"), "{summary}");
    }

    #[test]
    fn indices_are_one_based_and_contiguous() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        for (i, e) in els.iter().enumerate() {
            assert_eq!(e.index, i + 1);
        }
    }

    #[test]
    fn class_names_are_shortened() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        assert_eq!(els[0].class, "TextView");
        assert_eq!(els[1].class, "Button");
    }

    #[test]
    fn xml_entities_are_decoded() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        assert_eq!(els[0].text.as_deref(), Some("登录 & 注册"));
    }

    #[test]
    fn empty_attributes_become_none() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        // The image button has text="" but a content-desc.
        let back = els.iter().find(|e| e.class == "ImageButton").unwrap();
        assert_eq!(back.text, None);
        assert_eq!(back.content_desc.as_deref(), Some("返回"));
    }

    #[test]
    fn center_is_derived_from_bounds() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        let login = els
            .iter()
            .find(|e| e.text.as_deref() == Some("登录"))
            .unwrap();
        assert_eq!(login.bounds, (120, 840, 360, 920));
        assert_eq!(login.center, (240, 880));
    }

    #[test]
    fn disabled_flag_is_carried() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        let phone = els.iter().find(|e| e.class == "EditText").unwrap();
        assert!(!phone.enabled);
        assert!(els[1].enabled);
    }

    #[test]
    fn summary_lists_elements_with_their_index_and_center() {
        let els = parse_uiautomator_xml(DUMP).unwrap();
        let summary = format_ui_summary(&els);
        assert!(summary.starts_with("7 actionable element(s):"), "{summary}");
        assert!(summary.contains(r#"[2] Button "登录" clickable center=(240,880)"#), "{summary}");
        assert!(summary.contains("disabled"), "{summary}");
    }

    #[test]
    fn malformed_xml_is_an_error_not_a_panic() {
        assert!(parse_uiautomator_xml("<hierarchy><node").is_err());
        assert!(parse_uiautomator_xml("").is_err());
    }

    #[test]
    fn empty_hierarchy_summarises_clearly() {
        let els = parse_uiautomator_xml("<hierarchy rotation=\"0\"/>").unwrap();
        assert!(els.is_empty());
        let msg = format_ui_summary(&els);
        assert!(msg.contains("no actionable elements"), "{msg}");
        // An empty tree is not a dead end: the screenshot still describes the
        // screen, so the summary has to say so.
        assert!(msg.contains("screenshot"), "{msg}");
    }

    // ── Tap safety net ──────────────────────────────────────────────────
    //
    // An indexed tap re-reads the tree and validates before dispatching. All of
    // that decision-making is `validate_target`, which needs no device.

    fn element(index: usize, text: Option<&str>, class: &str) -> UiElement {
        let element = UiElement::new(
            index,
            text.map(str::to_string),
            None,
            None,
            class.to_string(),
            true,
            true,
            (0, 0, 10, 10),
        );
        element
    }

    #[test]
    fn valid_index_resolves_to_its_center() {
        let els = vec![element(1, Some("登录"), "Button")];
        let target = validate_target(&els, 1, None).expect("valid");
        assert_eq!(target.center, (5, 5));
    }

    #[test]
    fn index_zero_is_rejected() {
        let els = vec![element(1, Some("登录"), "Button")];
        let err = validate_target(&els, 0, None).unwrap_err();
        assert!(err.contains("1-based"), "{err}");
    }

    #[test]
    fn an_index_past_the_end_says_how_many_there_are() {
        let els = vec![element(1, Some("a"), "Button")];
        let err = validate_target(&els, 7, None).unwrap_err();
        assert!(err.contains("No element 7"), "{err}");
        assert!(err.contains("1 element(s)"), "{err}");
    }

    #[test]
    fn a_description_mismatch_refuses_and_names_both() {
        // This is the case the safety net exists for: the model is aiming at
        // something that is no longer at that index.
        let els = vec![element(1, Some("注册"), "Button")];
        let err = validate_target(&els, 1, Some("登录")).unwrap_err();
        assert!(err.contains("Refused"), "{err}");
        assert!(err.contains("\"注册\""), "{err}");
        assert!(err.contains("\"登录\""), "{err}");
    }

    #[test]
    fn a_matching_description_passes_loosely() {
        let els = vec![element(1, Some("Login with phone"), "Button")];
        assert!(validate_target(&els, 1, Some("login")).is_ok(), "case-insensitive substring");
        assert!(validate_target(&els, 1, Some("Login with phone")).is_ok(), "exact");
    }

    #[test]
    fn an_unlabelled_element_cannot_be_checked_but_passes() {
        // An icon button identified from the screenshot has no text to match;
        // refusing there would block a legitimate tap.
        let els = vec![element(1, None, "ImageButton")];
        assert!(validate_target(&els, 1, Some("back arrow")).is_ok());
    }

    #[test]
    fn non_clickable_and_disabled_targets_are_refused() {
        let mut no_click = element(1, Some("标题"), "TextView");
        no_click.clickable = false;
        let err = validate_target(&[no_click], 1, None).unwrap_err();
        assert!(err.contains("not clickable"), "{err}");

        let mut disabled = element(1, Some("提交"), "Button");
        disabled.enabled = false;
        let err = validate_target(&[disabled], 1, None).unwrap_err();
        assert!(err.contains("disabled"), "{err}");
    }

    // ── Delivery ladder ─────────────────────────────────────────────────
    //
    // The rungs themselves need a device; what decides between them is a pure
    // function and is tested here.

    #[test]
    fn a_press_gesture_is_a_still_finger_on_the_element() {
        let element = element(1, Some("Play"), "Button");
        let paths = press_paths(&element);
        assert_eq!(paths.len(), 1);
        // Three points on the same coordinate: a contact the platform sees for
        // more than one frame, which is what a tap is.
        assert_eq!(paths[0], vec![(5, 5), (5, 5), (5, 5)]);
    }

    #[test]
    fn only_a_control_press_is_news_in_the_result() {
        // A shell press is what every result was before the ladder existed.
        assert!(Via::Shell.note().is_none());
        assert!(Via::None.note().is_none());
        assert!(Via::Semantic.note().unwrap().contains("control"));
        assert!(Via::Gesture.note().unwrap().contains("gesture"));
        assert_eq!(Via::Semantic.as_str(), "semantic");
        assert_eq!(Via::Gesture.as_str(), "gesture");
        assert_eq!(Via::Shell.as_str(), "shell");
    }

    // ── Tap sequences ───────────────────────────────────────────────────

    #[test]
    fn a_sequence_becomes_one_shell_command() {
        // One dispatch, not N: every extra round trip is a model turn during
        // which the transient control has already faded.
        assert_eq!(encode_tap_sequence(&[(10, 20), (30, 40)]), "input tap 10 20; input tap 30 40");
        assert_eq!(encode_tap_sequence(&[(5, 6)]), "input tap 5 6");
    }

    #[test]
    fn steps_parse_from_the_argument() {
        let args = serde_json::json!([{ "x": 1, "y": 2 }, { "x": 3, "y": 4 }]);
        let steps = parse_tap_steps(Some(&args), None).unwrap();
        assert_eq!(steps.iter().map(|p| p.pixels()).collect::<Vec<_>>(), vec![(1, 2), (3, 4)]);
    }

    #[test]
    fn steps_may_name_percents() {
        // A sequence written for a screen the caller has not measured: each
        // step is a fraction of whatever is there.
        let args = serde_json::json!([{ "x_pct": 500, "y_pct": 0 }, { "x_pct": 0, "y_pct": 999 }]);
        let steps = parse_tap_steps(Some(&args), Some((1080, 2400))).unwrap();
        assert_eq!(steps.iter().map(|p| p.pixels()).collect::<Vec<_>>(), vec![(540, 0), (0, 2399)]);
        // The same steps on an unsized screen are refused, not guessed at.
        let refused = parse_tap_steps(Some(&args), None).unwrap_err();
        assert!(refused.contains("step 0"), "{refused}");
    }

    #[test]
    fn malformed_steps_are_rejected_with_a_reason() {
        assert!(parse_tap_steps(None, None)
            .unwrap_err()
            .contains("requires a 'steps' array"));
        assert!(parse_tap_steps(Some(&serde_json::json!([])), None)
            .unwrap_err()
            .contains("at least one"));
        let missing_y = serde_json::json!([{ "x": 1 }]);
        let error = parse_tap_steps(Some(&missing_y), None).unwrap_err();
        assert!(error.contains("step 0"), "{error}");
        // A half-named percent is caught by the same rule as a half-named pixel
        // pair rather than silently starting at zero.
        let only_y = serde_json::json!([{ "y_pct": 5 }]);
        let error = parse_tap_steps(Some(&only_y), Some((1080, 2400))).unwrap_err();
        assert!(error.contains("step 0"), "{error}");
    }

    /// A binary payload must survive as bytes.
    ///
    /// `screencap -p` returns a PNG, and the screenshot path used to base64 the
    /// result of `run_cmd`, whose stdout is `String::from_utf8_lossy` — every
    /// non-UTF-8 byte became U+FFFD, so the "screenshot" was not a decodable
    /// image. This uses a local `printf` rather than adb, so the fix is verified
    /// here rather than only on a device.
    #[tokio::test]
    async fn binary_stdout_survives_as_bytes() {
        let (status, bytes, _) = run_cmd_bytes("printf", &["\\377\\376\\101"]).await.unwrap();
        assert!(status.success());
        assert_eq!(bytes, vec![0xFF, 0xFE, 0x41]);
        // The lossy path would have replaced the invalid byte sequences.
        assert_ne!(String::from_utf8_lossy(&bytes).as_bytes(), bytes.as_slice());
    }

    // ── Failure classification (B4) ─────────────────────────────────────
    //
    // `uiautomator`'s stderr looks the same across causes that need different
    // responses, and the fallback ("use the screenshot") applies to all of them.

    #[test]
    fn dump_failures_are_told_apart() {
        // The code is the half a caller branches on, so it has to be right as
        // well as the sentence.
        let offline = DumpFailure::classify("error: device 'emulator-5554' not found");
        assert_eq!(offline.code, DEVICE_GONE);
        assert!(offline.message.contains("not connected"), "{offline}");

        let busy = DumpFailure::classify("ERROR: could not get idle state.");
        assert_eq!(busy.code, SCREEN_NEVER_IDLE);
        assert!(busy.message.contains("never settled"), "{busy}");

        let secure =
            DumpFailure::classify("ERROR: null root node returned by UiTestAutomationBridge.");
        assert_eq!(secure.code, SECURE_WINDOW);
        assert!(secure.message.contains("secure window"), "{secure}");

        let disabled = DumpFailure::classify("java.lang.SecurityException: Permission Denial");
        assert_eq!(disabled.code, ACCESSIBILITY_DISABLED);
        assert!(disabled.message.contains("not enabled"), "{disabled}");

        // Anything unrecognised still gets a code, so a caller never has to
        // handle "no code at all".
        assert_eq!(DumpFailure::classify("something nobody has seen before").code, DUMP_FAILED);
    }

    #[test]
    fn every_dump_failure_points_at_the_screenshot() {
        for stderr in [
            "device offline",
            "ERROR: could not get idle state.",
            "something nobody has seen before",
            "",
        ] {
            let failure = DumpFailure::classify(stderr);
            let msg = &failure.message;
            assert!(msg.contains("screenshot"), "{msg}");
            assert!(msg.contains("android_observe"), "{msg}");
            assert!(
                [
                    DEVICE_GONE,
                    SCREEN_NEVER_IDLE,
                    SECURE_WINDOW,
                    ACCESSIBILITY_DISABLED,
                    DUMP_FAILED
                ]
                .contains(&failure.code),
                "unknown code {}",
                failure.code
            );
        }
    }

    #[test]
    fn raw_output_is_kept_when_there_is_any() {
        assert!(DumpFailure::classify("weird failure")
            .message
            .contains("weird failure"));
        assert!(!DumpFailure::classify("   ").message.contains("Raw output"));
    }

    #[test]
    fn png_dimensions_reads_the_ihdr_chunk() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&13u32.to_be_bytes()); // IHDR length (unused)
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1080u32.to_be_bytes());
        png.extend_from_slice(&2340u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), Some((1080, 2340)));

        assert_eq!(png_dimensions(b"not a png"), None);
        assert_eq!(png_dimensions(&[0x89, b'P']), None);
    }

    #[test]
    fn coordinate_space_check_catches_a_rotation_or_scale() {
        // Matching, including the axes swapped by a rotation.
        assert!(coordinate_space_consistent(Some((1080, 2340)), Some((1080, 2340))));
        assert!(coordinate_space_consistent(Some((1080, 2340)), Some((2340, 1080))));
        // A scaled capture, or a stale tree from before a rotation.
        assert!(!coordinate_space_consistent(Some((1080, 2340)), Some((540, 1170))));
        assert!(!coordinate_space_consistent(Some((1080, 2340)), Some((1080, 1080))));
        // Nothing to compare is not a mismatch.
        assert!(coordinate_space_consistent(None, Some((1080, 2340))));
        assert!(coordinate_space_consistent(Some((1080, 2340)), None));
    }

    #[test]
    fn screen_bounds_come_from_the_root_node() {
        assert_eq!(parse_screen_bounds(DUMP), Some((1080, 2340)));
        assert_eq!(parse_screen_bounds("<hierarchy rotation=\"0\"/>"), None);
        assert_eq!(parse_screen_bounds("garbage"), None);
    }

    #[test]
    fn test_adb_screenshot_tool_name() {
        let tool = AdbScreenshotTool::new();
        assert_eq!(tool.name(), "android_screenshot");
    }

    #[test]
    fn test_adb_input_tool_schema() {
        let tool = AdbInputTool::new();
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }

    #[test]
    fn test_adb_app_manager_tool_name() {
        let tool = AdbAppManagerTool::new();
        assert_eq!(tool.name(), "android_app_manager");
    }

    // ── Tree quality ────────────────────────────────────────────────────
    //
    // `uiautomator dump` succeeds on a game, a canvas and a normal screen
    // alike; what differs is whether the tree describes any of it.

    fn node(class: &str, text: Option<&str>, bounds: (i32, i32, i32, i32)) -> UiElement {
        UiElement::new(
            1,
            text.map(str::to_string),
            None,
            None,
            class.to_string(),
            true,
            true,
            bounds,
        )
    }

    // ── Incremental reads ──────────────────────────────────────────────

    fn row(index: usize, key: &str, y: i32) -> UiElement {
        let mut element = node("android.widget.TextView", Some(key), (0, y, 200, y + 80));
        element.index = index;
        element.key = key.to_string();
        element
    }

    fn ui_dump(elements: Vec<UiElement>, package: &str) -> UiDump {
        let fingerprint = tree_fingerprint(&elements);
        UiDump {
            elements,
            screen: Some((1080, 2340)),
            source: "accessibility",
            package: Some(package.to_string()),
            snapshot: snapshot_name(fingerprint),
            truncated: false,
        }
    }

    #[test]
    fn a_snapshot_is_the_same_name_for_the_same_screen() {
        // Diffing against a name only works if the name is the screen, not the
        // enumeration: two identical reads must be the same baseline.
        let first = ui_dump(
            vec![node("android.widget.TextView", Some("a"), (0, 0, 100, 50))],
            "com.example",
        );
        let second = ui_dump(
            vec![node("android.widget.TextView", Some("a"), (0, 0, 100, 50))],
            "com.example",
        );
        assert_eq!(first.snapshot, second.snapshot);
        // And different screens must not share one.
        let other = ui_dump(
            vec![node("android.widget.TextView", Some("b"), (0, 0, 100, 50))],
            "com.example",
        );
        assert_ne!(first.snapshot, other.snapshot);
    }

    #[test]
    fn no_since_means_a_full_read() {
        let dump = ui_dump(vec![node("android.widget.TextView", Some("a"), (0, 0, 100, 50))], "x");
        assert!(since_answer(None, Target::Screen, None, &dump).is_none());
    }

    #[test]
    fn a_named_previous_read_yields_only_what_changed() {
        let before = vec![row(1, "a", 0), row(2, "b", 80)];
        crate::computer::platform::android::diff::remember(
            None,
            0,
            tree_fingerprint(&before),
            Some("com.example"),
            &before,
        );
        let after = vec![row(1, "a", 0), row(2, "b", 80), row(3, "c", 160)];
        let dump = ui_dump(after, "com.example");
        // The baseline name is the fingerprint of the *earlier* read.
        let since = format!("{:x}", tree_fingerprint(&before));
        let answer = since_answer(None, Target::Screen, Some(&since), &dump).expect("an answer");
        match answer {
            SinceAnswer::Delta { text, data } => {
                assert!(text.contains("1 appeared"), "{text}");
                let added = data["diff"]["added"].as_u64().expect("a count");
                assert_eq!(added, 1);
                // Only the new row is addressable now, so only it is listed.
                let listed = data["elements"].as_array().expect("rows");
                assert_eq!(listed.len(), 1);
                assert_eq!(listed[0]["key"], "c");
                assert_eq!(data["baseline"], true);
            }
            SinceAnswer::Full(reason) => panic!("expected a delta, got {reason:?}"),
        }
    }

    #[test]
    fn an_unknown_baseline_falls_back_and_says_why() {
        let dump = ui_dump(vec![node("android.widget.TextView", Some("a"), (0, 0, 100, 50))], "x");
        let answer =
            since_answer(None, Target::Screen, Some("deadbeef"), &dump).expect("an answer");
        match answer {
            SinceAnswer::Full(reason) => {
                assert_eq!(
                    reason,
                    crate::computer::platform::android::diff::FullReason::NoBaseline
                );
                assert!(reason.describe().contains("whole screen"));
            }
            SinceAnswer::Delta { .. } => panic!("expected a fallback"),
        }
    }

    #[test]
    fn a_different_app_is_not_compared() {
        let before = vec![row(1, "a", 0)];
        crate::computer::platform::android::diff::remember(
            None,
            0,
            0x1234,
            Some("com.one"),
            &before,
        );
        let dump = ui_dump(vec![row(1, "a", 0)], "com.two");
        let answer = since_answer(None, Target::Screen, Some("1234"), &dump).expect("an answer");
        assert!(matches!(
            answer,
            SinceAnswer::Full(crate::computer::platform::android::diff::FullReason::AppChanged)
        ));
    }

    #[test]
    fn a_fallback_names_its_reason_in_the_data() {
        let data =
            since_fallback(crate::computer::platform::android::diff::FullReason::MostlyChanged);
        assert_eq!(data["baseline"], false);
        assert_eq!(data["fallback"], "mostly_changed");
    }

    #[test]
    fn an_empty_tree_is_unusable() {
        assert_eq!(assess_tree(&[]), TreeQuality::Unusable);
    }

    #[test]
    fn a_tree_of_anonymous_boxes_is_unusable() {
        let elements = vec![
            node("android.widget.FrameLayout", None, (0, 0, 100, 100)),
            node("android.widget.FrameLayout", None, (0, 0, 100, 50)),
        ];
        assert_eq!(assess_tree(&elements), TreeQuality::Unusable);
    }

    #[test]
    fn a_mostly_nameless_tree_is_partial() {
        let elements = vec![
            node("android.widget.Button", Some("OK"), (0, 0, 10, 10)),
            node("android.view.View", None, (0, 0, 10, 10)),
            node("android.view.View", None, (0, 0, 10, 10)),
            node("android.view.View", None, (0, 0, 10, 10)),
            node("android.view.View", None, (0, 0, 10, 10)),
        ];
        assert_eq!(assess_tree(&elements), TreeQuality::Partial);
    }

    #[test]
    fn a_named_screen_is_usable_and_says_nothing() {
        let elements = (0..5)
            .map(|i| node("android.widget.TextView", Some("row"), (0, i * 10, 100, i * 10 + 9)))
            .collect::<Vec<_>>();
        assert_eq!(assess_tree(&elements), TreeQuality::Usable);
        assert_eq!(TreeQuality::Usable.advice(), None);
        assert!(TreeQuality::Unusable.advice().is_some());
    }

    #[test]
    fn structural_nodes_keep_a_nameless_list_screen_usable() {
        // A message list whose rows are custom-drawn: the only thing the tree
        // says is "there is a list here", and that is exactly what a read needs.
        let elements = (0..4)
            .map(|i| {
                let mut element = node("android.view.View", None, (0, i * 10, 100, i * 10 + 9));
                element.state.scrollable = i == 0;
                element
            })
            .collect::<Vec<_>>();
        assert_eq!(assess_tree(&elements), TreeQuality::Partial);
        let mut list_only = elements[..1].to_vec();
        list_only[0].state.scrollable = true;
        assert_eq!(assess_tree(&list_only), TreeQuality::Partial);
        // Enough structure and the screen is judged readable even unnamed.
        let structured = (0..3)
            .map(|i| {
                let mut element =
                    node("android.widget.EditText", None, (0, i * 10, 100, i * 10 + 9));
                element.state.editable = true;
                element
            })
            .collect::<Vec<_>>();
        assert_eq!(assess_tree(&structured), TreeQuality::Usable);
    }

    #[test]
    fn a_tree_of_unstructured_unnamed_boxes_is_still_unusable() {
        let mut clickable = node("android.view.View", None, (0, 0, 100, 100));
        clickable.clickable = true;
        let elements = vec![
            clickable,
            node("android.view.View", None, (0, 0, 100, 50)),
            node("android.view.View", None, (0, 50, 100, 100)),
        ];
        assert_eq!(assess_tree(&elements), TreeQuality::Unusable);
    }

    #[test]
    fn content_description_counts_as_a_name() {
        let mut described = node("android.widget.ImageButton", None, (0, 0, 10, 10));
        described.content_desc = Some("Back".to_string());
        assert_eq!(element_label(&described), Some("Back"));
    }

    #[test]
    fn blank_labels_do_not_count() {
        let blank = node("android.widget.TextView", Some("   "), (0, 0, 10, 10));
        assert_eq!(element_label(&blank), None);
    }

    // ── Write readback ─────────────────────────────────────────────────────

    fn field(text: Option<&str>, rid: Option<&str>, bounds: (i32, i32, i32, i32)) -> UiElement {
        let mut element = UiElement::new(
            1,
            text.map(str::to_string),
            None,
            rid.map(str::to_string),
            "EditText".to_string(),
            true,
            true,
            bounds,
        );
        element.state.editable = true;
        element
    }

    #[test]
    fn readback_ignores_whitespace_the_field_inserted() {
        assert!(readback_matches("13800138000", "138 0013 8000"));
        assert!(readback_matches("hello world", "helloworld"));
        // Case and punctuation are the message, not formatting.
        assert!(!readback_matches("Hello", "hello"));
        assert!(!readback_matches("done.", "done"));
    }

    // The verdict *rules* are tested with the shared layer in
    // `platform::screen`; these cover the field being read back on Android.

    #[test]
    fn a_readback_carries_the_shared_word_not_a_private_one() {
        let judged = readback_verdict("hi", "hi", None, false);
        assert_eq!(judged, ReadbackVerdict::Match);
        assert_eq!(
            Readback {
                verdict: judged.as_str(),
                actual: Some("hi".to_string()),
                note: None,
            }
            .verdict,
            READBACK_MATCH
        );
    }

    #[test]
    fn the_write_point_is_the_focused_field() {
        let mut first = field(None, Some("id/a"), (0, 0, 400, 100));
        let focused = field(None, Some("id/b"), (0, 200, 400, 300));
        first.state.focused = false;
        let mut focused = focused;
        focused.state.focused = true;
        let elements = vec![first, focused];
        assert_eq!(write_point(&elements), (200, 250));
    }

    #[test]
    fn the_field_is_located_by_place_not_by_key_which_the_write_changed() {
        let before = vec![
            field(Some("old"), Some("id/a"), (0, 0, 400, 100)),
            field(None, Some("id/b"), (0, 200, 400, 300)),
        ];
        let found = locate_for_readback(&before, (200, 250)).expect("a field");
        assert_eq!(found.resource_id.as_deref(), Some("id/b"));
        // And after the write the same field has a different key — which is
        // exactly why the key cannot be what finds it.
        let mut after = field(Some("new"), Some("id/b"), (0, 200, 400, 300));
        after.key = "different".to_string();
        assert_ne!(found.key, after.key);
    }

    #[test]
    fn a_focused_field_wins_over_one_the_point_merely_lands_in() {
        let mut focused = field(None, Some("id/a"), (0, 0, 400, 100));
        focused.state.focused = true;
        let other = field(None, Some("id/b"), (0, 0, 400, 100));
        let elements = vec![other, focused];
        assert_eq!(
            locate_for_readback(&elements, (200, 50))
                .expect("a field")
                .resource_id
                .as_deref(),
            Some("id/a")
        );
    }

    #[test]
    fn a_password_field_is_still_found_so_the_refusal_can_name_it() {
        let mut secret = field(None, Some("id/pw"), (0, 0, 400, 100));
        secret.state.password = true;
        secret.state.editable = false;
        let elements = vec![secret];
        let found = locate_for_readback(&elements, (200, 50)).expect("a field");
        assert!(found.state.password);
    }

    #[test]
    fn a_screen_without_a_field_has_nothing_to_read_back() {
        assert!(locate_for_readback(&[], (0, 0)).is_none());
        let label = node("android.widget.TextView", Some("hi"), (0, 0, 100, 100));
        assert!(locate_for_readback(&[label], (10, 10)).is_none());
    }

    #[test]
    fn an_echo_is_a_label_the_screen_did_not_have_before() {
        let before = vec![node(
            "android.widget.TextView",
            Some("Message"),
            (0, 0, 100, 40),
        )];
        let after = vec![
            node("android.widget.TextView", Some("Message"), (0, 0, 100, 40)),
            node("android.widget.TextView", Some("say hello now"), (0, 40, 100, 80)),
        ];
        assert!(screen_echoed(&before, &after, "hello"));
        // The same label was already there: not evidence of this write.
        assert!(!screen_echoed(&before, &after, "Message"));
        assert!(!screen_echoed(&before, &before, "hello"));
    }

    // ── Targets ─────────────────────────────────────────────────────────

    #[test]
    fn no_display_argument_means_the_users_screen() {
        let args = serde_json::json!({ "action": "tap", "x": 1, "y": 2 });
        assert_eq!(Target::from_args(&args), Ok(Target::Screen));
        let explicit_null = serde_json::json!({ "display": null });
        assert_eq!(Target::from_args(&explicit_null), Ok(Target::Screen));
    }

    #[test]
    fn a_display_id_targets_that_display() {
        let args = serde_json::json!({ "display": 3 });
        assert_eq!(Target::from_args(&args), Ok(Target::Display(3)));
    }

    #[test]
    fn a_non_integer_display_is_refused_with_a_reason() {
        let args = serde_json::json!({ "display": "three" });
        let message = Target::from_args(&args).expect_err("refused");
        assert!(message.contains("integer"), "{message}");
    }

    // ── Action effect ───────────────────────────────────────────────────

    #[test]
    fn fingerprint_tracks_labels_and_positions() {
        let base = vec![node("android.widget.TextView", Some("a"), (0, 0, 10, 10))];
        let same = base.clone();
        let moved = vec![node("android.widget.TextView", Some("a"), (0, 50, 10, 60))];
        let renamed = vec![node("android.widget.TextView", Some("b"), (0, 0, 10, 10))];

        assert_eq!(tree_fingerprint(&base), tree_fingerprint(&same));
        assert_ne!(tree_fingerprint(&base), tree_fingerprint(&moved));
        assert_ne!(tree_fingerprint(&base), tree_fingerprint(&renamed));
    }

    #[test]
    fn an_unknown_effect_says_so_instead_of_claiming_success() {
        let settle = Settle {
            effect: EFFECT_UNKNOWN,
            elapsed_ms: 300,
            elements: Vec::new(),
            note: Some("SCREEN_NEVER_IDLE".to_string()),
            pixel_changed: None,
            diff: None,
            package: None,
            first_signal: None,
        };
        let verdict = settle.verdict();
        assert!(verdict.starts_with("Effect unknown"), "{verdict}");
        assert!(verdict.contains("SCREEN_NEVER_IDLE"), "{verdict}");
    }

    #[test]
    fn only_a_content_event_from_this_screen_ends_a_settle_early() {
        let event = |kind, package: &str| crate::device::DeviceEvent {
            seq: 1,
            kind,
            package: package.to_string(),
            at_uptime_ms: 1,
            title: None,
            text: None,
            truncated: false,
        };
        use crate::device::EventKind;
        // The screen redrawing is the signal; a window change is a different
        // place (the caller's package check owns that) and a notification from
        // another app appears without the screen moving.
        assert!(settle_event_matters(&event(EventKind::Content, "com.a"), Some("com.a")));
        assert!(!settle_event_matters(&event(EventKind::Window, "com.b"), Some("com.a")));
        assert!(!settle_event_matters(&event(EventKind::Notification, "com.a"), Some("com.a")));
        // A content event from a different app is that app redrawing, not this
        // screen moving — and an app that *came to the top* raises a window
        // event, which the caller's own package check handles.
        assert!(!settle_event_matters(&event(EventKind::Content, "com.b"), Some("com.a")));
        // Before the first read the package is unknown, so any content event
        // counts — dropping it would miss the action's own first frame.
        assert!(settle_event_matters(&event(EventKind::Content, "com.b"), None));
    }

    #[test]
    fn an_unchanged_screen_points_at_the_screenshot() {
        let settle = Settle {
            effect: EFFECT_UNCHANGED,
            elapsed_ms: 1200,
            elements: Vec::new(),
            note: None,
            pixel_changed: None,
            diff: None,
            package: None,
            first_signal: None,
        };
        let verdict = settle.verdict();
        assert!(verdict.contains("unchanged"), "{verdict}");
        assert!(verdict.contains("screenshot"), "{verdict}");
    }

    #[test]
    fn a_changed_screen_reports_how_long_it_took() {
        let settle = Settle {
            effect: EFFECT_CHANGED,
            elapsed_ms: 420,
            elements: vec![node("android.widget.TextView", Some("a"), (0, 0, 10, 10))],
            note: None,
            pixel_changed: None,
            diff: None,
            package: None,
            first_signal: None,
        };
        let verdict = settle.verdict();
        assert!(verdict.contains("changed"), "{verdict}");
        assert!(verdict.contains("420 ms"), "{verdict}");
        assert!(verdict.contains("1 elements"), "{verdict}");
    }

    // ── Gesture paths ───────────────────────────────────────────────────

    #[test]
    fn a_two_finger_pinch_parses_into_two_paths() {
        let args = serde_json::json!({
            "paths": [
                [[540, 1200], [540, 900]],
                [[540, 1200], [540, 1500]],
            ]
        });
        let paths = parse_gesture_paths(args.get("paths")).expect("paths");
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], vec![(540, 1200), (540, 900)]);
        assert_eq!(paths[1], vec![(540, 1200), (540, 1500)]);
    }

    #[test]
    fn a_gesture_without_paths_is_refused() {
        let args = serde_json::json!({});
        let message = parse_gesture_paths(args.get("paths")).expect_err("refused");
        assert!(message.contains("paths"), "{message}");
    }

    #[test]
    fn a_third_finger_is_refused() {
        let args = serde_json::json!({
            "paths": [
                [[0, 0], [1, 1]],
                [[0, 0], [1, 1]],
                [[0, 0], [1, 1]],
            ]
        });
        let message = parse_gesture_paths(args.get("paths")).expect_err("refused");
        assert!(message.contains("one or two"), "{message}");
    }

    #[test]
    fn a_single_point_is_not_a_gesture() {
        let args = serde_json::json!({ "paths": [[[10, 10]]] });
        let message = parse_gesture_paths(args.get("paths")).expect_err("refused");
        assert!(message.contains("2"), "{message}");
    }

    #[test]
    fn a_point_that_is_not_a_pair_is_named() {
        let args = serde_json::json!({ "paths": [[[10, 10], [20]]] });
        let message = parse_gesture_paths(args.get("paths")).expect_err("refused");
        assert!(message.contains("paths[0][1]"), "{message}");
    }

    #[test]
    fn a_second_display_is_display_zero_only_when_it_is_the_screen() {
        assert_eq!(Target::Screen.display_id(), 0);
        assert_eq!(Target::Display(7).display_id(), 7);
    }

    #[test]
    fn a_pixel_change_overturns_an_unchanged_tree() {
        // The tree said nothing changed; the picture says otherwise. A canvas
        // draws without touching the tree, and that is the case this exists for.
        let before = super::display::FrameSignature {
            columns: 2,
            rows: 2,
            cells: vec![0, 0, 0, 0],
        };
        let after = super::display::FrameSignature {
            columns: 2,
            rows: 2,
            cells: vec![255, 255, 255, 255],
        };
        assert!(before.differs(&after));
    }

    #[test]
    fn a_settle_without_a_pixel_signal_leaves_it_unknown() {
        let settle = Settle {
            effect: EFFECT_UNCHANGED,
            elapsed_ms: 1200,
            elements: Vec::new(),
            note: None,
            pixel_changed: None,
            diff: None,
            package: None,
            first_signal: None,
        };
        assert_eq!(settle.pixel_changed, None);
        assert_eq!(settle.effect, EFFECT_UNCHANGED);
    }

    // ── Effect classification ──────────────────────────────────────────────

    #[test]
    fn a_row_appearing_is_a_small_change_not_the_screen_changing() {
        let before: Vec<_> = (0..20)
            .map(|i| node("android.widget.TextView", Some("row"), (0, i * 10, 100, i * 10 + 9)))
            .collect();
        let mut after = before.clone();
        // One label changed: the key hashes the text, so it reads as one gone
        // and one new — beneath max(3, 10% of 20) and therefore small.
        after[0] = node("android.widget.TextView", Some("row2"), (0, 0, 100, 9));
        let diff = tree_diff(&before, &after);
        assert_eq!(diff.removed, 1);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.changed(), 2);
        assert_eq!(
            classify_effect(None, None, Some(&diff), after.len(), None, Some(1), Some(2)),
            EFFECT_SMALL_CHANGE
        );
    }

    #[test]
    fn three_changed_elements_are_a_real_change_on_a_short_screen() {
        let before = vec![
            node("android.widget.Button", Some("a"), (0, 0, 100, 50)),
            node("android.widget.Button", Some("b"), (0, 50, 100, 100)),
            node("android.widget.Button", Some("c"), (0, 100, 100, 150)),
        ];
        let after = vec![
            node("android.widget.Button", Some("x"), (0, 0, 100, 50)),
            node("android.widget.Button", Some("y"), (0, 50, 100, 100)),
            node("android.widget.Button", Some("z"), (0, 100, 100, 150)),
        ];
        let diff = tree_diff(&before, &after);
        // Three labels replaced read as three gone and three new: six changed
        // against a threshold of three.
        assert_eq!(diff.changed(), 6);
        assert_eq!(
            classify_effect(None, None, Some(&diff), after.len(), None, Some(1), Some(2)),
            EFFECT_CHANGED
        );
    }

    #[test]
    fn a_moved_element_is_counted_once() {
        let before = vec![node("android.widget.TextView", Some("a"), (0, 0, 100, 50))];
        let after = vec![node("android.widget.TextView", Some("a"), (0, 40, 100, 90))];
        let diff = tree_diff(&before, &after);
        assert_eq!(
            diff,
            TreeDiff {
                added: 0,
                removed: 0,
                moved: 1,
                edited: 0
            }
        );
    }

    #[test]
    fn duplicate_rows_are_counted_as_a_multiset() {
        // Two identical rows, one of them gone: "one left", not "nothing
        // happened because the other one still matches".
        let before = vec![
            node("android.widget.TextView", Some("same"), (0, 0, 100, 50)),
            node("android.widget.TextView", Some("same"), (0, 50, 100, 100)),
        ];
        let after = vec![node(
            "android.widget.TextView",
            Some("same"),
            (0, 0, 100, 50),
        )];
        let diff = tree_diff(&before, &after);
        assert_eq!(diff.removed, 1);
        assert_eq!(diff.added, 0);
    }

    #[test]
    fn text_that_changed_under_the_same_key_is_edited() {
        // A counter going 3 → 4 keeps its resource id and class; only the text
        // moved, and the key is built from it, so this shows up as a pair.
        let before = vec![node("android.widget.TextView", Some("3"), (0, 0, 100, 50))];
        let after = vec![node("android.widget.TextView", Some("4"), (0, 0, 100, 50))];
        let diff = tree_diff(&before, &after);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
    }

    #[test]
    fn a_canvas_that_moved_under_a_motionless_tree_is_a_small_change() {
        assert_eq!(
            classify_effect(
                None,
                None,
                Some(&TreeDiff::default()),
                0,
                Some(true),
                Some(7),
                Some(7)
            ),
            EFFECT_SMALL_CHANGE
        );
        assert_eq!(
            classify_effect(
                None,
                None,
                Some(&TreeDiff::default()),
                0,
                Some(false),
                Some(7),
                Some(7)
            ),
            EFFECT_UNCHANGED
        );
    }

    #[test]
    fn a_new_app_is_a_place_change_even_when_the_tree_changed() {
        assert_eq!(
            classify_effect(
                Some("com.a"),
                Some("com.b"),
                Some(&TreeDiff {
                    added: 9,
                    ..TreeDiff::default()
                }),
                9,
                None,
                Some(1),
                Some(2)
            ),
            EFFECT_PLACE_CHANGED
        );
    }

    #[test]
    fn a_tree_that_was_never_read_before_is_unknown() {
        assert_eq!(classify_effect(None, None, None, 0, None, None, Some(3)), EFFECT_UNKNOWN);
    }

    // ── Raw screencap ──────────────────────────────────────────────────────

    fn raw_frame(width: u32, height: u32, rgba: [u8; 4], header: usize) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes()); // RGBA_8888
        out.extend(std::iter::repeat_n(0u8, header - 12));
        out.extend(std::iter::repeat_n(rgba, (width * height) as usize).flatten());
        out
    }

    #[test]
    fn a_raw_frame_becomes_a_grid() {
        // 120×240 gives 10×10 pixel cells, so every cell is the same colour.
        for header in [12, 16] {
            let frame = raw_frame(120, 240, [255, 0, 0, 255], header);
            let signature = raw_screencap_signature(&frame).expect("a grid");
            assert_eq!((signature.columns, signature.rows), (12, 24));
            assert_eq!(signature.cells.len(), 12 * 24);
            // (3*255 + 0 + 0) / 10 = 76
            assert!(signature.cells.iter().all(|&cell| cell == 76), "{header}");
        }
    }

    #[test]
    fn a_frame_in_another_format_is_refused_rather_than_misread() {
        let mut frame = raw_frame(120, 240, [0, 255, 0, 255], 16);
        // Not RGBA_8888: the byte count matches but the layout does not, which
        // is exactly the case that would silently produce a grid of noise.
        frame[8..12].copy_from_slice(&0x11u32.to_le_bytes());
        assert!(raw_screencap_signature(&frame).is_none());
    }

    #[test]
    fn a_truncated_frame_is_refused() {
        let mut frame = raw_frame(120, 240, [0, 0, 0, 255], 16);
        frame.truncate(frame.len() - 400);
        assert!(raw_screencap_signature(&frame).is_none());
    }

    #[test]
    fn a_frame_too_small_for_the_grid_is_refused() {
        // Fewer columns than grid cells: there is no honest average to take.
        let frame = raw_frame(8, 8, [0, 0, 0, 255], 16);
        assert!(raw_screencap_signature(&frame).is_none());
    }

    #[test]
    fn the_summary_ceiling_truncates_instead_of_dumping_everything() {
        let elements = (0..70)
            .map(|i| node("android.widget.TextView", Some("row"), (0, i * 10, 100, i * 10 + 9)))
            .collect::<Vec<_>>();
        let text = format_ui_summary_limited(&elements, 60);
        assert!(text.contains("10 more element(s)"), "{text}");
        assert!(format_ui_summary_limited(&elements[..3], 60).contains("3 actionable element(s)"));
    }

    #[test]
    fn ocr_lines_carry_tappable_centres() {
        let lines = [OcrLine {
            text: "Settings".to_string(),
            center: (50, 25),
            bounds: (0, 0, 100, 50),
            confidence: 0.9,
        }];
        let text = format_ocr_lines(&lines);
        assert!(text.contains("Settings"), "{text}");
        assert!(text.contains("center=(50,25)"), "{text}");
        assert!(format_ocr_lines(&[]).contains("no text"));
    }

    #[test]
    fn a_bridge_ocr_answer_becomes_tappable_lines() {
        let answer = serde_json::json!({
            "ok": true,
            "width": 1080,
            "height": 2400,
            "lines": [
                { "text": "设置", "bounds": [10, 20, 110, 60], "confidence": 0.93 },
                { "text": "  确定  ", "bounds": [900, 2200, 1060, 2320], "confidence": 0.81 },
            ],
        });
        let lines = parse_ocr_answer(&answer).expect("lines");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "设置");
        assert_eq!(lines[0].center, (60, 40));
        assert_eq!(lines[1].text, "确定");
        // The tap target has to be inside the line's own box.
        let (cx, cy) = lines[1].center;
        assert!(cx > 900 && cx < 1060 && cy > 2200 && cy < 2320);
        assert!((lines[0].confidence - 0.93).abs() < 1e-6);
    }

    #[test]
    fn an_unloadable_engine_is_never_an_empty_screen() {
        // The whole point of the sentinel: an empty list would tell the model
        // the screen is blank, and it would stop looking.
        let answer = serde_json::json!({
            "ok": false,
            "code": "ocr_unavailable",
            "error": "the on-device OCR engine could not be loaded",
        });
        let refused = parse_ocr_answer(&answer).expect_err("no engine");
        assert!(refused.starts_with("ocr_unavailable"), "{refused}");
        assert!(refused.contains("could not be loaded"), "{refused}");
    }

    #[test]
    fn a_success_answer_without_lines_is_malformed_not_blank() {
        let refused =
            parse_ocr_answer(&serde_json::json!({ "ok": true })).expect_err("no line list");
        assert!(refused.starts_with("ocr_unavailable"), "{refused}");
    }

    #[test]
    fn unusable_ocr_lines_are_skipped_but_the_rest_survive() {
        let answer = serde_json::json!({
            "ok": true,
            "lines": [
                { "text": "kept", "bounds": [0, 0, 100, 40] },
                { "text": "   ", "bounds": [0, 0, 100, 40] },
                { "text": "no bounds" },
                { "text": "short bounds", "bounds": [0, 0, 100] },
                { "text": "not numbers", "bounds": ["a", 0, 1, 1] },
                { "text": "flat", "bounds": [10, 10, 10, 40] },
                { "text": "no confidence", "bounds": [0, 50, 100, 90] },
            ],
        });
        let lines = parse_ocr_answer(&answer).expect("lines");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].text, "kept");
        assert_eq!(lines[1].text, "no confidence");
        // Absent confidence is reported as zero, not as a negative sentinel.
        assert_eq!(lines[1].confidence, 0.0);
    }

    #[test]
    fn the_input_tool_documents_the_effect_report() {
        // The model only learns that `verify` exists from the description; a
        // silent removal would make it spend a turn on android_observe again.
        let described = AdbInputTool::new().description().to_string();
        assert!(described.contains("effect"), "{described}");
        assert!(described.contains("verify=false"), "{described}");
    }

    #[test]
    fn test_android_toolset_id() {
        let set = AndroidToolset::new();
        assert_eq!(set.id(), "android");
    }

    #[test]
    fn test_adb_pair_tool_name_and_schema() {
        let tool = AdbPairTool::new();
        assert_eq!(tool.name(), "device_adb_pair");
        let schema = tool.parameters_schema();
        let props = schema.get("properties").unwrap();
        assert!(props.get("port").is_some());
        assert!(props.get("code").is_some());
        assert!(props.get("connect_port").is_some());
    }

    #[test]
    fn test_adb_status_tool_name() {
        let tool = AdbStatusTool::new();
        assert_eq!(tool.name(), "device_adb_status");
    }

    #[test]
    fn test_android_toolset_includes_pairing_tools() {
        let names: Vec<String> = AndroidToolset::new()
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&"device_adb_pair".to_string()));
        assert!(names.contains(&"device_adb_status".to_string()));
    }

    #[test]
    fn every_android_tool_is_core_or_grouped() {
        // The registry leaves a tool in neither list visible, so an unclassified
        // tool is not lost — but it is also never opened by a group, which
        // means no tier and no line in the catalog. Adding a tool has to say
        // which of the two it is, and this is where that is enforced.
        let set = AndroidToolset::new();
        let core: std::collections::HashSet<&str> = set.core_tools().iter().copied().collect();
        let grouped: std::collections::HashSet<&str> = set
            .tool_groups()
            .iter()
            .flat_map(|group| group.tools.iter().copied())
            .collect();

        for tool in set.tools() {
            let name = tool.name();
            assert!(
                core.contains(name) || grouped.contains(name),
                "{name} is offered by AndroidToolset but named in neither core_tools nor a group"
            );
        }
    }

    #[test]
    fn the_core_is_the_small_set_a_turn_needs() {
        // Six, because that is the claim the tiering makes: reading, pressing,
        // opening, verifying, waiting and handing back. A seventh belongs in a
        // group, and this test is where that gets decided.
        let set = AndroidToolset::new();
        assert_eq!(set.core_tools().len(), 6, "{:?}", set.core_tools());
    }

    #[test]
    fn no_group_names_a_core_tool() {
        // A group that names a core tool says nothing the core list has not
        // already said, and makes the group's tool count a lie.
        let set = AndroidToolset::new();
        let core: std::collections::HashSet<&str> = set.core_tools().iter().copied().collect();
        for group in set.tool_groups() {
            for name in group.tools {
                assert!(!core.contains(name), "group {} names core tool {name}", group.id);
            }
        }
    }

    #[test]
    fn no_tool_belongs_to_two_groups() {
        // Two groups could both claim a tool; opening one would make it visible
        // while the other still counted it as hidden. One home per tool.
        let set = AndroidToolset::new();
        let mut seen = std::collections::HashMap::new();
        for group in set.tool_groups() {
            for name in group.tools {
                if let Some(previous) = seen.insert(*name, group.id) {
                    panic!("{name} is in both {previous} and {}", group.id);
                }
            }
        }
    }

    #[test]
    fn test_android_toolset_exposes_the_combined_observe_tool() {
        let names: Vec<String> = AndroidToolset::new()
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&"android_observe".to_string()), "{names:?}");
    }

    #[test]
    fn test_android_toolset_exposes_the_web_tool() {
        // The phone's browser is one more member of the set, so it is offered
        // by the same profile switch as the rest of the screen tools.
        let names: Vec<String> = AndroidToolset::new()
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&"android_web".to_string()), "{names:?}");
    }

    #[test]
    fn test_android_toolset_exposes_the_visual_search_tool() {
        // Where the tree and OCR both come up empty, the search that can still
        // name an icon is offered by the same profile switch as the rest.
        let names: Vec<String> = AndroidToolset::new()
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&"android_find_visually".to_string()), "{names:?}");
    }

    // ── Device targeting ────────────────────────────────────────────────
    //
    // A call with no `device` is not "any device": adb resolves it to whichever
    // single device is attached at that moment, so a swap between two calls
    // retargets silently. The argument therefore has to be honoured when given,
    // refused when it cannot be a serial, and the result has to name what was
    // actually addressed.

    #[test]
    fn the_callers_device_argument_beats_the_configured_serial() {
        let args = serde_json::json!({ "device": "emulator-5554" });
        assert_eq!(
            resolve_device(&args, Some("R58M111")).unwrap(),
            Some("emulator-5554".to_string())
        );
    }

    #[test]
    fn omitting_device_falls_back_to_the_configured_serial() {
        assert_eq!(
            resolve_device(&serde_json::json!({}), Some("R58M111")).unwrap(),
            Some("R58M111".to_string())
        );
        assert_eq!(resolve_device(&serde_json::json!({}), None).unwrap(), None);
        // JSON null is an omission, not a serial: that is what a serializer
        // emits for a `None`, so it must fall back like an absent key.
        assert_eq!(
            resolve_device(&serde_json::json!({ "device": null }), Some("R58M111")).unwrap(),
            Some("R58M111".to_string())
        );
    }

    #[test]
    fn a_device_that_is_not_a_serial_is_refused_not_ignored() {
        // Falling back to the configured serial here would act on a device the
        // caller did not name — the exact silent retarget this argument exists
        // to prevent, so it has to fail instead of quietly doing something.
        for args in [
            serde_json::json!({ "device": "" }),
            serde_json::json!({ "device": "   " }),
            serde_json::json!({ "device": 7 }),
            serde_json::json!({ "device": ["emulator-5554"] }),
        ] {
            assert!(resolve_device(&args, Some("R58M111")).is_err(), "{args}");
        }
    }

    #[test]
    fn a_serial_is_trimmed_before_use() {
        let args = serde_json::json!({ "device": " emulator-5554 " });
        assert_eq!(resolve_device(&args, None).unwrap(), Some("emulator-5554".to_string()));
    }

    #[tokio::test]
    async fn a_pinned_serial_is_reported_without_asking_adb() {
        // There is no adb in this test, and the pinned case must not need one:
        // the serial is already known, so it is reported without a round trip.
        assert_eq!(
            resolved_serial(&Some("emulator-5554".to_string())).await,
            Some("emulator-5554".to_string())
        );
    }

    // ── Verification verdicts ────────────────────────────────────────────
    //
    // The tree is the evidence, and there are three answers rather than two:
    // "could not check" is neither a yes nor a no, and folding it into either
    // one invents a fact.

    #[test]
    fn a_verification_answers_with_evidence_from_the_tree() {
        let elements = vec![element(1, Some("Inbox"), "android.widget.TextView")];
        assert_eq!(
            evaluate(&Expectation::TextContains("inbox".into()), &elements),
            VERDICT_SATISFIED,
            "the match is case-insensitive, like the tap path's"
        );
        assert_eq!(
            evaluate(&Expectation::TextContains("Sent".into()), &elements),
            VERDICT_UNSATISFIED
        );
        // An empty screen is not a pass.
        assert_eq!(
            evaluate(&Expectation::TextContains("anything".into()), &[]),
            VERDICT_UNSATISFIED
        );
    }

    #[test]
    fn an_element_that_is_not_there_is_unsatisfied_not_unknown() {
        // The tree was read and the element is not in it: that is an answer.
        let elements = vec![element(1, Some("Inbox"), "android.widget.TextView")];
        assert_eq!(
            evaluate(&Expectation::ElementAt { index: 1, description: None }, &elements),
            VERDICT_SATISFIED
        );
        assert_eq!(
            evaluate(&Expectation::ElementAt { index: 7, description: None }, &elements),
            VERDICT_UNSATISFIED
        );
        // Indices are 1-based, so 0 does not exist.
        assert_eq!(
            evaluate(&Expectation::ElementAt { index: 0, description: None }, &elements),
            VERDICT_UNSATISFIED
        );
    }

    #[test]
    fn a_relabelled_element_fails_the_label_check() {
        let elements = vec![element(1, Some("Inbox"), "android.widget.TextView")];
        assert_eq!(
            evaluate(
                &Expectation::ElementAt {
                    index: 1,
                    description: Some("Sent".into())
                },
                &elements
            ),
            VERDICT_UNSATISFIED
        );
        assert_eq!(
            evaluate(
                &Expectation::ElementAt {
                    index: 1,
                    description: Some("inbo".into())
                },
                &elements
            ),
            VERDICT_SATISFIED
        );
    }

    #[test]
    fn a_verification_must_name_something_to_check() {
        assert!(Expectation::parse(&serde_json::json!({})).is_err());
        assert!(Expectation::parse(&serde_json::json!({ "text": "   " })).is_err());
        assert!(matches!(
            Expectation::parse(&serde_json::json!({ "text": "Inbox" })).unwrap(),
            Expectation::TextContains(_)
        ));
        assert!(matches!(
            Expectation::parse(&serde_json::json!({ "target": 3 })).unwrap(),
            Expectation::ElementAt { index: 3, .. }
        ));
    }
}
