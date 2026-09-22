//! Android tool tests (the original file's `#[cfg(test)] mod tests` body).

use super::app::AdbAppManagerTool;
use super::input::AdbInputTool;
use super::pairing::{AdbPairTool, AdbStatusTool};
use super::screenshot::AdbScreenshotTool;
use super::*;
use crate::computer::platform::mobile::run_cmd_bytes;

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
    assert_eq!(encode_input_text("a\r\nb"), "input text 'a'; input keyevent 66; input text 'b'");
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
    assert_eq!(els.len(), 4, "{classes:?}");
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
    assert!(summary.starts_with("4 actionable element(s):"), "{summary}");
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
    UiElement {
        index,
        text: text.map(str::to_string),
        content_desc: None,
        resource_id: None,
        class: class.to_string(),
        clickable: true,
        enabled: true,
        bounds: (0, 0, 10, 10),
        center: (5, 5),
    }
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
    assert_eq!(parse_tap_steps(Some(&args)).unwrap(), vec![(1, 2), (3, 4)]);
}

#[test]
fn malformed_steps_are_rejected_with_a_reason() {
    assert!(parse_tap_steps(None)
        .unwrap_err()
        .contains("requires a 'steps' array"));
    assert!(parse_tap_steps(Some(&serde_json::json!([])))
        .unwrap_err()
        .contains("at least one"));
    let missing_y = serde_json::json!([{ "x": 1 }]);
    assert!(parse_tap_steps(Some(&missing_y))
        .unwrap_err()
        .contains("step 0"));
    let wrong_type = serde_json::json!([{ "x": "1", "y": 2 }]);
    assert!(parse_tap_steps(Some(&wrong_type)).is_err());
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

    let secure = DumpFailure::classify("ERROR: null root node returned by UiTestAutomationBridge.");
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
fn test_android_toolset_exposes_the_combined_observe_tool() {
    let names: Vec<String> = AndroidToolset::new()
        .tools()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(names.contains(&"android_observe".to_string()), "{names:?}");
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
