//! The calculator run: a goal on a real screen, and a capture a person can read.
//!
//! The plan's Stage B verification on a real display: ask for a goal the way a
//! person would, let the engine drive it, and require two things — the run
//! reaches `done`, and what it captured at the end is a readable picture rather
//! than a well-formed empty rectangle.
//!
//! Both halves matter. A run that concludes while the display shows nothing is
//! precisely the failure a status payload cannot reveal; a readable picture of
//! an unfinished task is not the task done. Either alone would let a broken
//! loop look healthy.
//!
//! The decision model is scripted rather than an LLM. The subject here is the
//! engine — observe, act on what it read, verify, conclude — and a model that
//! answers differently on Tuesday would make this test measure the model. The
//! script reads element ids out of the *prompt the driver actually rendered*,
//! so it still exercises "decide from what is on the screen": nothing is
//! addressed by a coordinate someone wrote down in advance.
//!
//! Two gates, both loud (CLAUDE.md: a silently-green test that never ran is
//! worse than a red one): Screen Recording for the capture, Accessibility for
//! driving the calculator. Either missing is a skip, because the run would fail
//! on a permission rather than on a defect.
//!
//! Everything is undone at the end — the calculator is quit — because this test
//! clicks into the session it runs in.

// The permission probe only exists on macOS (`platform::macos` is
// `cfg(target_os = "macos")` in the lib), so importing it unconditionally
// makes this whole test binary fail to compile everywhere else — which is what
// it did on the Linux CI runner. The script logic below needs no display and
// runs on every platform; only the journey does.
#[cfg(target_os = "macos")]
use syscity::computer::platform::macos::permissions::has_accessibility_permission;

use std::sync::Mutex;

use syscity::computer::types::UiElement;

use super::*;

/// What to compute, and what the screen must show afterwards.
const EXPRESSION: &str = "17×23";
const RESULT: &str = "391";

/// The keys to press, in order, with the labels a calculator may publish for
/// each. Two spellings per key because accessibility trees disagree: macOS
/// Calculator publishes the glyph, while other builds spell it out.
const KEYS: &[&[&str]] = &[
    &["1"],
    &["7"],
    &["×", "Multiply", "*"],
    &["2"],
    &["3"],
    &["=", "Equals"],
];

/// Whether this runner may drive another app through its accessibility tree.
///
/// On macOS that is TCC's Accessibility grant. On any other platform the
/// journey cannot run at all — `skip_if_no_screen_capture` has already
/// returned by then, and the message it prints says why — so `false` here is
/// not a silent pass but the second half of a skip the caller has announced.
fn may_drive_accessibility() -> bool {
    #[cfg(target_os = "macos")]
    {
        has_accessibility_permission()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[tokio::test]
#[serial]
async fn a_goal_on_a_real_screen_ends_done_with_a_readable_capture() {
    if !skip_if_no_screen_capture() {
        return;
    }
    if !may_drive_accessibility() {
        eprintln!(
            "Skipping computer.run E2E: Accessibility permission is not granted to the \
             terminal/runner, so the calculator cannot be driven. Grant it in System Settings \
             → Privacy & Security → Accessibility."
        );
        return;
    }

    let port = free_port();
    let mut config = test_config(port, true);
    config.computer.enabled = true;
    // Three steps of opening plus six keystrokes, with room for a re-read.
    config.computer.max_steps = 14;
    config.computer.settle_delay_ms = 300;
    // No local vision model, and none is needed: the tree names the keys. The
    // run must not consult an API either — that is the privacy contract.
    config.computer.vision.mode = "off".to_string();
    // The run may drive Calculator. An empty list means no boundary, so naming
    // it is what proves the boundary accepts a real app and does not only
    // refuse one. Note this also rules out bare key presses: they name no app
    // and are refused, so the script must click the keypad.
    config.computer.allow_apps = vec!["Calculator".to_string()];

    let gateway = new_test_gateway(config).await;

    // The live desktop adapter, taken before the gateway is handed to the
    // server: it is the same handle the run drives, so reading through it
    // afterwards answers "what is on screen now".
    let adapter = gateway.state().tools.computer_adapter.read().await.clone();
    let adapter = adapter.expect("the gateway builds a desktop adapter when computer use is on");

    // One decision per provider call. `MockProvider`'s callback is `Fn`, and a
    // counter behind a mutex is the smallest way to keep "step N" honest.
    let step = Arc::new(Mutex::new(0usize));
    let decided = {
        let step = step.clone();
        MockProvider::new().with_callback(move |messages| {
            let is_driver = messages
                .iter()
                .any(|m| m.role == Role::System && m.content.contains("You drive a computer"));
            if !is_driver {
                return ProviderMessage::assistant("NOCACHE");
            }
            let mut n = step.lock().unwrap_or_else(|e| e.into_inner());
            let answer = next_decision(*n, messages);
            *n += 1;
            ProviderMessage::assistant(answer)
        })
    };
    register_mock_provider_with_model(&gateway.model_router(), decided, "mock-model").await;

    start_gateway_and_wait(port, gateway).await;

    let mut client = FrontendSimulator::connect(port).await;
    let started = client
        .request(
            "computer.run",
            json!({ "goal": format!("打开计算器，算 {EXPRESSION}，把结果留在屏幕上") }),
        )
        .await;
    assert_eq!(started["ok"], true, "{started}");
    let run_id = started["payload"]["run_id"]
        .as_str()
        .expect("a run id")
        .to_string();

    // The run is spawned; poll rather than sleep a guessed amount.
    let mut view = serde_json::Value::Null;
    for _ in 0..300 {
        let status = client
            .request("computer.status", json!({ "run_id": &run_id }))
            .await;
        assert_eq!(status["ok"], true, "{status}");
        view = status["payload"].clone();
        if view["finished"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // 1. It finished, and it finished by saying the goal was met.
    assert_eq!(view["finished"], true, "the run never finished: {view}");
    assert_eq!(view["phase"], "done", "a goal that was carried out must end `done`: {view}");
    assert_eq!(view["summary"]["stop_reason"], "done", "{view}");
    assert!(
        view["summary"]["steps"].as_u64().unwrap_or(0) >= KEYS.len() as u64,
        "the run must have taken at least one step per keystroke: {view}"
    );

    // 2. The screen really shows the answer. This is the assertion a `done`
    //    cannot make for itself: a loop can conclude from its own plan while
    //    the display shows nothing.
    let shown = read_display(&adapter).await;
    assert!(
        shown.contains(RESULT),
        "the calculator must show {RESULT} after {EXPRESSION}; the screen read as {shown:?}"
    );

    // 3. The capture is a readable picture. Dimensions alone are not enough —
    //    `screencapture` happily writes a black rectangle — so this also
    //    requires the frame to carry content.
    let shot = capture_screen().expect("a capture of the final screen");
    assert_readable(&shot);

    quit_calculator();
}

/// The next scripted decision, as the driver's JSON.
///
/// Step 0 opens the app; then one click per key; then `done`. Clicks name an
/// element id taken from the outline in the prompt, so each step acts on what
/// the previous read showed rather than on a remembered id.
fn next_decision(step: usize, messages: &[syscity::providers::Message]) -> String {
    if step == 0 {
        return r#"{"decision": "act", "action": {"type": "launch_app", "name": "Calculator"}}"#
            .to_string();
    }
    let index = step - 1;
    if index >= KEYS.len() {
        return format!(
            r#"{{"decision": "done", "message": "{EXPRESSION} = {RESULT}, shown on the calculator"}}"#
        );
    }

    let outline = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| m.content.as_str())
        .unwrap_or_default();

    match find_key(outline, KEYS[index]) {
        Some(id) => {
            format!(r#"{{"decision": "act", "action": {{"type": "click", "element_id": "{id}"}}}}"#)
        }
        // Not found is a fact worth reporting rather than a guess: the run then
        // stops saying which key was missing, and the failure message carries
        // the screen it was looking at.
        None => format!(
            r#"{{"decision": "help", "reason": "the calculator keypad has no {} button; \
               the tree offered: {}"}}"#,
            KEYS[index].join("/"),
            compact(outline)
        ),
    }
}

/// Find the element id for one key in the rendered tree outline.
///
/// The outline is one line per element (`role "label" [id=...]`), which is the
/// documented form the model reads. Coupling to it here is deliberate: it makes
/// the script behave like a reader of the prompt, and if the rendering changes
/// the test fails with the prompt in the message instead of silently passing.
fn find_key(outline: &str, labels: &[&str]) -> Option<String> {
    for line in outline.lines() {
        let Some(label) = line.split('"').nth(1) else {
            continue;
        };
        if !labels
            .iter()
            .any(|wanted| label.eq_ignore_ascii_case(wanted))
        {
            continue;
        }
        let rest = &line[line.find("[id=")? + 4..];
        let id = rest.split(']').next()?.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

/// The outline with its newlines folded, for an error message that fits.
fn compact(outline: &str) -> String {
    let flat = outline.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(600).collect()
}

/// Read the calculator's display from the live desktop adapter.
///
/// The adapter is the one the run drove, so this answers "what is on screen
/// now" rather than repeating the run's own recollection of it.
async fn read_display(adapter: &Arc<dyn syscity::computer::ComputerAdapter>) -> String {
    let tree = adapter
        .read_ui_tree(None)
        .await
        .expect("reading the calculator's tree");
    render_text(&tree)
}

/// Every label and value in the tree, flattened — what the screen says.
fn render_text(tree: &[UiElement]) -> String {
    fn walk(el: &UiElement, out: &mut String) {
        if let Some(label) = el.label.as_deref().filter(|l| !l.is_empty()) {
            out.push_str(label);
            out.push(' ');
        }
        if let Some(value) = el.value.as_deref().filter(|v| !v.is_empty()) {
            out.push_str(value);
            out.push(' ');
        }
        for child in &el.children {
            walk(child, out);
        }
    }
    let mut out = String::new();
    for root in tree {
        walk(root, &mut out);
    }
    out
}

/// Capture the whole screen and decode it, failing loudly when it cannot.
fn capture_screen() -> Option<image::DynamicImage> {
    let path = std::env::temp_dir().join(format!(
        "syscity_calculator_{}_{}.png",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let status = std::process::Command::new("/usr/sbin/screencapture")
        .arg("-x")
        .arg(&path)
        .status()
        .ok()?;
    if !status.success() {
        eprintln!("screencapture failed; there is no capture to read");
        return None;
    }
    let image = image::open(&path).ok();
    let _ = std::fs::remove_file(&path);
    image
}

/// Assert the capture is a picture with something in it.
///
/// A successful `screencapture` on a sleeping display still writes a valid PNG,
/// and "the file decodes" would accept that as readable. Sampling distinct
/// colours is what separates a screen with a calculator on it from a uniform
/// rectangle — the actual question behind "截图可读".
fn assert_readable(image: &image::DynamicImage) {
    let rgb = image.to_rgb8();
    let (w, h) = rgb.dimensions();
    assert!(w >= 640 && h >= 480, "a screen capture this small is not the desktop: {w}x{h}");

    let mut seen = std::collections::HashSet::new();
    let step_x = (w / 60).max(1) as usize;
    let step_y = (h / 60).max(1) as usize;
    for y in (0..h).step_by(step_y) {
        for x in (0..w).step_by(step_x) {
            let p = rgb.get_pixel(x, y);
            seen.insert((p[0], p[1], p[2]));
        }
    }
    assert!(
        seen.len() > 8,
        "the capture has only {} distinct colours — an empty screen, not a readable answer",
        seen.len()
    );
}

/// Leave the machine as it was found.
fn quit_calculator() {
    let status = std::process::Command::new("osascript")
        .args(["-e", r#"tell application "Calculator" to quit"#])
        .status();
    if let Err(e) = status {
        eprintln!("could not quit Calculator after the run: {e}");
    }
}

// ── The script itself, verified without a screen ────────────────────────────
//
// The journey above needs a real display, so on a host without one it skips and
// proves nothing. These drive the same scripted decisions against a calculator
// outline in the exact form the driver renders, which is where a bug in
// "read the prompt, name the key" would actually live.

/// A calculator as the driver renders it — `role "label" ... at (x,y) [id=...]`,
/// nested, with the display publishing the value the way macOS does.
const CALCULATOR_OUTLINE: &str = r#"UI tree:
window "Calculator" at (0,0) 300x500 [id=calc-win]
  text_field "Display" = "0" at (10,10) 280x60 [id=calc-display]
  button "1" at (10,80) 60x60 [id=key-1]
  button "7" at (10,220) 60x60 [id=key-7]
  button "2" at (80,150) 60x60 [id=key-2]
  button "3" at (150,150) 60x60 [id=key-3]
  button "×" at (10,350) 60x60 [id=key-mul]
  button "=" at (80,420) 60x60 [id=key-equals]
"#;

fn user_prompt(content: &str) -> Vec<syscity::providers::Message> {
    vec![
        syscity::providers::Message::system("You drive a computer one step at a time."),
        syscity::providers::Message::user(content),
    ]
}

/// The whole six-step script, resolved against a rendered tree: open, then one
/// click per key in the arithmetic, then done — each naming the id the outline
/// published for that label.
#[test]
fn the_script_clicks_the_keypad_it_was_shown_and_then_concludes() {
    let prompt = user_prompt(CALCULATOR_OUTLINE);

    let opening = next_decision(0, &prompt);
    assert!(opening.contains(r#""launch_app""#), "step 0 opens the app: {opening}");
    assert!(opening.contains(r#""Calculator""#), "{opening}");

    let expected = ["key-1", "key-7", "key-mul", "key-2", "key-3", "key-equals"];
    for (step, want) in expected.iter().enumerate() {
        let answer = next_decision(step + 1, &prompt);
        assert!(
            answer.contains(&format!(r#""element_id": "{want}""#)),
            "step {} should click {want}, answered {answer}",
            step + 1
        );
    }

    let closing = next_decision(1 + KEYS.len(), &prompt);
    assert!(closing.contains(r#""decision": "done""#), "{closing}");
    assert!(closing.contains(RESULT), "the closing message names the answer: {closing}");
}

/// A keypad that does not publish one of the keys must produce `help` naming
/// it — not a click on the wrong button, and not a silent `done`.
#[test]
fn a_missing_key_asks_for_a_person_instead_of_guessing() {
    let outline = CALCULATOR_OUTLINE.replace(r#"  button "7" at (10,220) 60x60 [id=key-7]"#, "");
    let prompt = user_prompt(&outline);

    // Step 1 is the first "7".
    let answer = next_decision(2, &prompt);
    assert!(answer.contains(r#""decision": "help""#), "{answer}");
    assert!(answer.contains("7"), "the reason names the missing key: {answer}");
}

/// Labels are matched the way the platform spells them, and a tree that
/// publishes `Multiply` rather than the glyph still resolves.
#[test]
fn a_key_is_found_under_either_spelling() {
    let spelled = CALCULATOR_OUTLINE.replace(r#"button "×""#, r#"button "Multiply""#);
    let prompt = user_prompt(&spelled);
    let answer = next_decision(3, &prompt);
    assert!(answer.contains(r#""element_id": "key-mul""#), "{answer}");
}

/// A scripted decision never reuses an id from a previous screen: the ids come
/// from the prompt it was given, so an outline that renamed them is followed.
#[test]
fn the_script_follows_the_ids_of_the_screen_it_was_shown() {
    let renamed = CALCULATOR_OUTLINE.replace("[id=key-1]", "[id=snapshot-42-button-1]");
    let prompt = user_prompt(&renamed);
    let answer = next_decision(1, &prompt);
    assert!(
        answer.contains(r#""element_id": "snapshot-42-button-1""#),
        "the click must name the id this read published: {answer}"
    );
    assert!(
        !answer.contains("key-1"),
        "an id from a previous name must not be reused: {answer}"
    );
}

/// The screen read must find the answer in the display field, not only in the
/// model's closing message — otherwise assertion 2 would be satisfied by the
/// run's own opinion of itself.
#[test]
fn the_display_value_is_what_the_screen_read_returns() {
    use syscity::computer::Tri;
    let tree = vec![UiElement {
        id: "calc-win".into(),
        role: "window".into(),
        label: Some("Calculator".into()),
        value: None,
        bounds: None,
        enabled: Tri::Unknown,
        focused: Tri::Unknown,
        actions: None,
        children_count: None,
        truncated: false,
        children: vec![UiElement {
            id: "calc-display".into(),
            role: "text_field".into(),
            label: Some("Display".into()),
            value: Some(RESULT.into()),
            bounds: None,
            enabled: Tri::Unknown,
            focused: Tri::Unknown,
            actions: None,
            children: Vec::new(),
            children_count: None,
            truncated: false,
        }],
    }];

    let shown = render_text(&tree);
    assert!(shown.contains(RESULT), "the value on screen is read back: {shown}");
}
