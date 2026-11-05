//! What kind of screen this is, when the obvious answer ("read the tree")
//! did not work.
//!
//! A model that taps and sees nothing concluded that its action failed and
//! tried again — twelve times, on a locked phone. The truth was one of five
//! things, and each has a different next move:
//!
//! * **Normal** — the tree describes the screen. Work from it.
//! * **Blank** — there is no window, or the window's contents are not exposed.
//!   Read the screenshot, or start the app.
//! * **Loading** — the screen keeps changing. Wait; do not act yet.
//! * **Protected** — the window is a secure one and the frame is black. No
//!   screenshot will ever help; the person has to do this part.
//! * **Needs a person** — a captcha, a payment, a password, an SMS code. The
//!   agent cannot finish this, and saying so is cheaper than failing at it.
//!
//! The vocabulary is Mika's `ScreenDiagnosis`, and so are the precedence rules:
//! a screen that is still changing is *loading* even if it also shows captcha
//! text (that text may be a stale frame), and a secure window is *protected*
//! even when its text names a payment (the text is what the person should act
//! on, which is what `takeoverReason` is for).
//!
//! What is new here is that the same judgement also covers the states
//! [`super::ready`] decides before a write. A locked phone is not a sixth
//! screen state — it is "a person is needed", with its own cause, and the hint
//! for it is the readiness refusal verbatim rather than a second wording of
//! the same fact.

use super::display::{FrameSignature, SIGNATURE_CELL_DELTA};

/// The five answers, as the wire calls them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenState {
    Normal,
    Blank,
    Loading,
    Protected,
    NeedsHuman,
}

impl ScreenState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Blank => "blank",
            Self::Loading => "loading",
            Self::Protected => "protected",
            Self::NeedsHuman => "needs_human",
        }
    }

    /// Whether the tree can be worked from as it is.
    pub fn usable(self) -> bool {
        matches!(self, Self::Normal)
    }
}

/// The facts a diagnosis is made from.
///
/// They are gathered by the caller because some of them are expensive (a
/// black frame on the user's screen is a full `screencap`); keeping the
/// judgement a pure function is what makes every branch testable without a
/// phone.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    /// The app the platform said owns the screen, when it said.
    pub current_app: Option<String>,
    /// A window was read and had at least one element.
    pub has_window: bool,
    /// How many elements the read produced.
    pub readable_elements: usize,
    /// The screen is still changing between two reads.
    pub changing: bool,
    /// Whether each observed frame was entirely black, in order.
    pub black_frames: Vec<bool>,
    /// The text on the screen, from the tree and/or OCR.
    pub texts: Vec<String>,
    /// A password field is on screen.
    pub password_field: bool,
    /// The foreground app was named even though its window could not be read.
    pub foreground_app: Option<String>,
    /// A person has to act first: the phone is locked or asleep.
    pub locked: bool,
}

/// A diagnosis, in the words a caller acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    pub state: ScreenState,
    pub current_app: Option<String>,
    pub needs_human: bool,
    /// The `android_hand_to_user` reason to use, when somebody must act.
    pub takeover_reason: Option<String>,
    /// The fact the verdict rests on, short.
    pub cause: Option<String>,
    /// What to do next.
    pub hint: String,
}

/// Captcha and "are you human" wording.
const CAPTCHA: &[&str] = &[
    "captcha",
    "人机验证",
    "安全验证",
    "滑块验证",
    "拖动滑块",
    "向右滑动完成",
    "请完成验证",
    "verify you are human",
    "not a robot",
];

/// Payment wording — the screen where a wrong tap moves money.
const PAYMENT: &[&str] = &[
    "支付密码",
    "确认支付",
    "确认付款",
    "立即支付",
    "输入密码支付",
    "payment password",
    "confirm payment",
    "pay now",
];

/// SMS-code wording: the other half of a login nobody but the person can do.
const SMS_CODE: &[&str] = &[
    "获取验证码",
    "发送验证码",
    "短信验证码",
    "get code",
    "send code",
    "verification code",
];

/// The first payment phrase on the screen, if any.
///
/// Exposed because a caller may want to warn about the payment even when the
/// screen's own verdict came from something else (`protected`).
pub fn payment_text(texts: &[String]) -> Option<&'static str> {
    let joined = texts.join("\n").to_lowercase();
    PAYMENT.iter().copied().find(|word| joined.contains(word))
}

fn first_hit(words: &[&'static str], text: &str) -> Option<&'static str> {
    words.iter().copied().find(|word| text.contains(word))
}

/// Whether a frame is one a person would see as black.
///
/// Not "every cell is zero": a secure window still lets the status bar show,
/// and an OLED screen's black is 0 while an LCD's is 2–3. The bar is what
/// separates "nothing was drawn" from "something dark was drawn", and it is
/// the only thing a 12×24 grid can honestly say.
pub fn is_black(frame: &FrameSignature) -> bool {
    frame.cells.iter().all(|cell| *cell <= BLACK_CELL_MAX)
}

/// The brightest a cell may be for the frame to be called black.
const BLACK_CELL_MAX: u8 = 12;

/// Whether two frames differ enough to mean the screen is still drawing.
fn frames_differ(a: &FrameSignature, b: &FrameSignature) -> bool {
    if a.columns != b.columns || a.rows != b.rows || a.cells.len() != b.cells.len() {
        // A different geometry is a different screen, not a still one.
        return true;
    }
    a.cells
        .iter()
        .zip(&b.cells)
        .filter(|(x, y)| x.abs_diff(**y) > SIGNATURE_CELL_DELTA)
        .count()
        >= 1
}

/// Decide what kind of screen `facts` describe.
pub fn classify(facts: &Facts) -> Diagnosis {
    let text = facts.texts.join("\n").to_lowercase();
    let captcha = first_hit(CAPTCHA, &text);
    let payment = first_hit(PAYMENT, &text);
    let code = first_hit(SMS_CODE, &text);
    // What a person would be asked to do, if the screen needs one.
    let human: Option<(String, String)> = if facts.locked {
        Some(("unlock".to_string(), "the phone is locked or asleep".to_string()))
    } else if let Some(word) = captcha {
        Some(("captcha".to_string(), format!("captcha text: {word}")))
    } else if let Some(word) = payment {
        Some(("payment".to_string(), format!("payment text: {word}")))
    } else if facts.password_field {
        Some(("login".to_string(), "a password field is on screen".into()))
    } else {
        code.map(|word| ("login".to_string(), format!("verification-code text: {word}")))
    };

    // A locked phone outranks everything: nothing on the screen was produced by
    // the app, so nothing read from it can be trusted.
    if facts.locked {
        return Diagnosis {
            state: ScreenState::NeedsHuman,
            current_app: facts.current_app.clone(),
            needs_human: true,
            takeover_reason: Some("unlock".to_string()),
            cause: Some("the phone is locked or asleep".to_string()),
            hint: crate::computer::platform::android::ready::PHONE_LOCKED_HINT.to_string(),
        };
    }

    // A black frame while the screen is still changing is a page transition
    // caught mid-paint, not a protected window and not an empty one.
    if facts.changing && facts.black_frames.iter().any(|black| *black) {
        return Diagnosis {
            state: ScreenState::Loading,
            current_app: facts.current_app.clone(),
            needs_human: false,
            takeover_reason: None,
            cause: Some("a black frame while the screen keeps changing".to_string()),
            hint: "The screen is still changing (a page transition, a splash, a list painting \
                   in). Wait for it with android_wait, then read it — acting now lands on \
                   whatever is half drawn."
                .to_string(),
        };
    }

    // Two quiet black frames: the window is secure (a banking app, a payment
    // sheet, a DRM player). One black frame may just be a frame that was not
    // drawn yet.
    if facts.has_window
        && facts.black_frames.len() >= 2
        && facts.black_frames.iter().all(|black| *black)
    {
        let reason = human
            .as_ref()
            .map(|(reason, _)| reason.clone())
            .unwrap_or_else(|| "other".to_string());
        let cause = match &human {
            Some((_, cause)) => format!("secure window: the frame stays black; {cause}"),
            None => "secure window: the frame stays black".to_string(),
        };
        return Diagnosis {
            state: ScreenState::Protected,
            current_app: facts.current_app.clone(),
            needs_human: true,
            takeover_reason: Some(reason),
            cause: Some(cause),
            hint: "This window is protected: the system will not let the agent photograph it, and \
                   the accessibility tree describes almost none of it. A person has to do this \
                   part — hand the screen to them with android_hand_to_user."
                .to_string(),
        };
    }

    if let Some((reason, cause)) = human {
        return Diagnosis {
            state: ScreenState::NeedsHuman,
            current_app: facts.current_app.clone(),
            needs_human: true,
            takeover_reason: Some(reason.clone()),
            cause: Some(cause),
            hint: format!(
                "This screen needs the person: {}. Do not try to get through it — hand it over \
                 with android_hand_to_user (reason '{}') and continue when they answer.",
                reason, reason
            ),
        };
    }

    if facts.changing {
        return Diagnosis {
            state: ScreenState::Loading,
            current_app: facts.current_app.clone(),
            needs_human: false,
            takeover_reason: None,
            cause: Some("the screen keeps changing".to_string()),
            hint: "The screen is still changing. Wait for it with android_wait, then read it."
                .to_string(),
        };
    }

    if !facts.has_window || facts.readable_elements == 0 {
        let (cause, hint) = if facts.has_window {
            (
                "the window is there but has no readable elements".to_string(),
                "There is a window but the accessibility tree describes none of it. Read the \
                 screenshot (android_screenshot) or the text on it (android_ocr) and tap by \
                 coordinates."
                    .to_string(),
            )
        } else if let Some(app) = facts.foreground_app.as_ref().or(facts.current_app.as_ref()) {
            // The app is in front; its window simply could not be read. That is
            // not "it did not open", so starting it again would be wrong.
            (
                format!("the window of {app} could not be read"),
                format!(
                    "{app} is in front but its window could not be read. Do not start it again \
                     — it is already open. Read the screenshot instead."
                ),
            )
        } else {
            (
                "no window on the screen".to_string(),
                "Nothing is on this display. Start the app you want with android_app_manager \
                 (or android_display for the agent's own screen)."
                    .to_string(),
            )
        };
        return Diagnosis {
            state: ScreenState::Blank,
            current_app: facts
                .current_app
                .clone()
                .or_else(|| facts.foreground_app.clone()),
            needs_human: false,
            takeover_reason: None,
            cause: Some(cause),
            hint,
        };
    }

    Diagnosis {
        state: ScreenState::Normal,
        current_app: facts.current_app.clone(),
        needs_human: false,
        takeover_reason: None,
        cause: None,
        hint: "The accessibility tree describes this screen; work from it.".to_string(),
    }
}

/// The one-line answer for a tool result.
pub fn describe(diagnosis: &Diagnosis) -> String {
    let app = diagnosis
        .current_app
        .as_deref()
        .map(|package| format!(" ({package})"))
        .unwrap_or_default();
    let cause = diagnosis
        .cause
        .as_deref()
        .map(|cause| format!(": {cause}"))
        .unwrap_or_default();
    format!("{}{app}{cause}. {}", diagnosis.state.as_str(), diagnosis.hint)
}

/// Whether two frames say the screen is still drawing.
pub fn still_changing(before: &FrameSignature, after: &FrameSignature) -> bool {
    frames_differ(before, after)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normal() -> Facts {
        Facts {
            current_app: Some("com.example.chat".to_string()),
            has_window: true,
            readable_elements: 14,
            ..Facts::default()
        }
    }

    fn black() -> FrameSignature {
        FrameSignature {
            columns: 2,
            rows: 2,
            cells: vec![0, 1, 2, 0],
        }
    }

    fn drawn() -> FrameSignature {
        FrameSignature {
            columns: 2,
            rows: 2,
            cells: vec![10, 220, 180, 30],
        }
    }

    #[test]
    fn a_described_screen_is_normal() {
        let verdict = classify(&normal());
        assert_eq!(verdict.state, ScreenState::Normal);
        assert!(verdict.state.usable());
        assert!(!verdict.needs_human);
        assert!(verdict.takeover_reason.is_none());
        assert!(describe(&verdict).starts_with("normal (com.example.chat)"));
    }

    #[test]
    fn a_captcha_needs_a_person_and_says_which_word() {
        let mut facts = normal();
        facts.texts = vec!["安全验证".to_string(), "向右滑动完成".to_string()];
        let verdict = classify(&facts);
        assert_eq!(verdict.state, ScreenState::NeedsHuman);
        assert!(verdict.needs_human);
        assert_eq!(verdict.takeover_reason.as_deref(), Some("captcha"));
        assert!(verdict.cause.as_deref().unwrap().contains("安全验证"));
        // The advice must be "hand it over", not "try again".
        assert!(verdict.hint.contains("android_hand_to_user"));
        assert!(!verdict.state.usable());
    }

    #[test]
    fn a_payment_screen_is_a_person_not_a_retry() {
        let mut facts = normal();
        facts.texts = vec!["确认支付".to_string()];
        let verdict = classify(&facts);
        assert_eq!(verdict.state, ScreenState::NeedsHuman);
        assert_eq!(verdict.takeover_reason.as_deref(), Some("payment"));
        assert_eq!(payment_text(&facts.texts), Some("确认支付"));
    }

    #[test]
    fn a_password_field_is_a_login() {
        let mut facts = normal();
        facts.password_field = true;
        let verdict = classify(&facts);
        assert_eq!(verdict.state, ScreenState::NeedsHuman);
        assert_eq!(verdict.takeover_reason.as_deref(), Some("login"));
    }

    #[test]
    fn an_sms_code_screen_is_a_login_too() {
        let mut facts = normal();
        facts.texts = vec!["获取验证码".to_string()];
        let verdict = classify(&facts);
        assert_eq!(verdict.takeover_reason.as_deref(), Some("login"));
        assert!(verdict.cause.as_deref().unwrap().contains("获取验证码"));
    }

    #[test]
    fn a_secure_window_is_protected_and_keeps_the_human_reason() {
        let mut facts = normal();
        facts.black_frames = vec![true, true];
        facts.texts = vec!["确认支付".to_string()];
        let verdict = classify(&facts);
        assert_eq!(verdict.state, ScreenState::Protected);
        assert!(verdict.needs_human);
        // The screen needs a person for a payment *inside* a protected window;
        // the reason names the payment, so the hand-off says what to ask for.
        assert_eq!(verdict.takeover_reason.as_deref(), Some("payment"));
        assert!(verdict.cause.as_deref().unwrap().contains("secure window"));
    }

    #[test]
    fn one_black_frame_is_not_yet_a_protected_window() {
        // A frame that had not been drawn is common while a page paints.
        let mut facts = normal();
        facts.black_frames = vec![true];
        let verdict = classify(&facts);
        assert_eq!(verdict.state, ScreenState::Normal);
    }

    #[test]
    fn a_black_frame_while_changing_is_loading_not_protected() {
        let mut facts = normal();
        facts.changing = true;
        facts.black_frames = vec![true, true];
        let verdict = classify(&facts);
        assert_eq!(verdict.state, ScreenState::Loading);
        assert!(!verdict.needs_human);
        assert!(verdict.hint.contains("android_wait"));
    }

    #[test]
    fn a_changing_screen_without_a_frame_is_loading() {
        let mut facts = normal();
        facts.changing = true;
        assert_eq!(classify(&facts).state, ScreenState::Loading);
    }

    #[test]
    fn an_empty_screen_with_no_window_is_blank_and_says_to_start_something() {
        let facts = Facts::default();
        let verdict = classify(&facts);
        assert_eq!(verdict.state, ScreenState::Blank);
        assert!(!verdict.needs_human);
        assert!(verdict.hint.contains("android_app_manager"));
    }

    #[test]
    fn an_unreadable_window_is_not_an_app_that_failed_to_open() {
        // The app is in front; its window simply cannot be described. Telling
        // the model to start it again would relaunch an app that is already
        // there, and lose whatever state it had.
        let facts = Facts {
            current_app: None,
            foreground_app: Some("com.example.game".to_string()),
            ..Facts::default()
        };
        let verdict = classify(&facts);
        assert_eq!(verdict.state, ScreenState::Blank);
        assert_eq!(verdict.current_app.as_deref(), Some("com.example.game"));
        assert!(verdict.hint.contains("Do not start it again"));
    }

    #[test]
    fn a_window_with_nothing_readable_is_blank_but_keeps_the_app() {
        let facts = Facts {
            current_app: Some("com.example.app".to_string()),
            has_window: true,
            readable_elements: 0,
            ..Facts::default()
        };
        let verdict = classify(&facts);
        assert_eq!(verdict.state, ScreenState::Blank);
        assert_eq!(verdict.current_app.as_deref(), Some("com.example.app"));
        assert!(verdict.hint.contains("android_screenshot"));
    }

    #[test]
    fn a_locked_phone_outranks_whatever_is_painted() {
        let mut facts = normal();
        facts.locked = true;
        facts.black_frames = vec![true, true];
        let verdict = classify(&facts);
        assert_eq!(verdict.state, ScreenState::NeedsHuman);
        assert_eq!(verdict.takeover_reason.as_deref(), Some("unlock"));
        // The hint is the readiness refusal, not a second wording of it.
        assert_eq!(verdict.hint, crate::computer::platform::android::ready::PHONE_LOCKED_HINT);
    }

    #[test]
    fn a_payment_text_on_a_normal_screen_is_still_found_by_itself() {
        // `payment_text` is for callers that want the warning even when the
        // verdict came from somewhere else (a protected window).
        assert_eq!(payment_text(&["合计 30 元".to_string()]), None);
        assert_eq!(
            payment_text(&["Input your Password".to_string()]),
            None,
            "only the payment wording, not every password prompt"
        );
    }

    #[test]
    fn a_dark_frame_is_black_and_a_drawn_one_is_not() {
        assert!(is_black(&black()));
        assert!(!is_black(&drawn()));
        // A status bar alone does not make a frame "drawn".
        let bar = FrameSignature {
            columns: 2,
            rows: 2,
            cells: vec![0, 0, 0, 9],
        };
        assert!(is_black(&bar));
    }

    #[test]
    fn frames_of_different_shapes_are_still_changing() {
        let a = FrameSignature {
            columns: 2,
            rows: 2,
            cells: vec![0, 0, 0, 0],
        };
        let b = FrameSignature {
            columns: 4,
            rows: 4,
            cells: vec![0; 16],
        };
        assert!(still_changing(&a, &b));
        assert!(!still_changing(&a, &a));
    }
}
