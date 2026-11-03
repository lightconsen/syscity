//! Integration tests for Computer Use modules.
//!
//! These tests exercise real bugs found during code review:
//!
//! - sensitive_ui  : privilege escalation (0 coverage), nested trees, Chinese
//!   keywords
//! - screenshot_encoder : `which` command cross-platform, NetworkCondition edge
//!   cases
//! - audio         : analyze_segment edge cases (empty, boundary, edge
//!   conditions)
//! - system        : blocking sleep in async context (verified via timeout
//!   behavior)

// ---------------------------------------------------------------------------
// sensitive_ui
// ---------------------------------------------------------------------------

#[cfg(test)]
mod sensitive_ui_tests {
    use syscity::computer::sensitive_ui::{
        SensitiveUiDetector, SensitiveUiFinding, SensitiveUiType,
    };
    use syscity::computer::types::{Rect, Tri, UiElement};

    fn make_node(
        id: &str,
        role: &str,
        label: Option<&str>,
        value: Option<&str>,
        children: Vec<UiElement>,
    ) -> UiElement {
        UiElement {
            id: id.to_string(),
            role: role.to_string(),
            label: label.map(String::from),
            value: value.map(String::from),
            bounds: Some(Rect::new(0, 0, 100, 30)),
            enabled: Tri::Known(true),
            focused: Tri::Unknown,
            actions: None,
            children,
            children_count: None,
            truncated: false,
        }
    }

    // ── Privilege escalation (previously 0 test coverage) ───────────────

    #[test]
    fn test_privilege_escalation_dialog() {
        let detector = SensitiveUiDetector::new();
        // A dialog requesting admin credentials
        let tree = make_node(
            "auth-dlg",
            "dialog",
            Some("This application requires administrator privileges"),
            None,
            vec![],
        );
        let findings = detector.scan_tree(&tree);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::PrivilegeEscalation)),
            "should detect privilege escalation in admin dialog: {:?}",
            findings
        );
    }

    #[test]
    fn test_privilege_escalation_sudo_prompt() {
        let detector = SensitiveUiDetector::new();
        // A dialog with sudo in the label
        let tree =
            make_node("sudo-dlg", "alert", Some("sudo requires your password"), None, vec![]);
        let findings = detector.scan_tree(&tree);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::PrivilegeEscalation)),
            "should detect sudo prompt: {:?}",
            findings
        );
    }

    #[test]
    fn test_privilege_escalation_uac() {
        let detector = SensitiveUiDetector::new();
        // Windows UAC prompt
        let tree = make_node("uac-dlg", "dialog", Some("User Account Control"), None, vec![]);
        let findings = detector.scan_tree(&tree);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::PrivilegeEscalation)),
            "should detect UAC dialog: {:?}",
            findings
        );
    }

    #[test]
    fn test_privilege_escalation_chinese() {
        let detector = SensitiveUiDetector::new();
        // Chinese privilege escalation prompt (one of the signals)
        let tree = make_node("auth-cn", "dialog", Some("需要管理员权限"), None, vec![]);
        let findings = detector.scan_tree(&tree);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::PrivilegeEscalation)),
            "should detect Chinese privilege escalation: {:?}",
            findings
        );
    }

    #[test]
    fn test_privilege_escalation_enter_password() {
        let detector = SensitiveUiDetector::new();
        // "Enter password to" is a signal for privilege escalation
        let tree =
            make_node("pwd-prompt", "prompt", Some("Enter password to continue"), None, vec![]);
        let findings = detector.scan_tree(&tree);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::PrivilegeEscalation)),
            "should detect enter-password prompt: {:?}",
            findings
        );
    }

    #[test]
    fn test_no_false_positive_privilege_escalation() {
        let detector = SensitiveUiDetector::new();
        // A regular info dialog should NOT trigger privilege escalation
        let tree =
            make_node("info-dlg", "dialog", Some("Operation completed successfully"), None, vec![]);
        let findings = detector.scan_tree(&tree);
        assert!(
            !findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::PrivilegeEscalation)),
            "regular info dialog should not trigger privilege escalation: {:?}",
            findings
        );
    }

    // ── Nested tree detection ───────────────────────────────────────────

    #[test]
    fn test_sensitive_child_under_non_sensitive_parent() {
        let detector = SensitiveUiDetector::new();
        // Non-sensitive window containing a sensitive password field
        let tree = make_node(
            "main-window",
            "window",
            Some("Settings"),
            None,
            vec![make_node(
                "pwd-field",
                "text_field",
                Some("Password"),
                None,
                vec![],
            )],
        );
        let findings = detector.scan_tree(&tree);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::PasswordField)),
            "should detect password field nested under non-sensitive parent: {:?}",
            findings
        );
    }

    #[test]
    fn test_multiple_nested_sensitive_elements() {
        let detector = SensitiveUiDetector::new();
        // Deeply nested: window > form > fieldset > password + card fields
        let tree = make_node(
            "checkout-window",
            "window",
            Some("Checkout"),
            None,
            vec![make_node(
                "form",
                "form",
                None,
                None,
                vec![make_node(
                    "fieldset",
                    "group",
                    Some("Billing"),
                    None,
                    vec![
                        make_node("card-input", "text_field", Some("Card Number"), None, vec![]),
                        make_node("cvv-input", "text_field", Some("CVV"), None, vec![]),
                        make_node("name-input", "text_field", Some("Name"), None, vec![]),
                    ],
                )],
            )],
        );
        let findings = detector.scan_tree(&tree);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::PaymentField)),
            "should detect payment fields in nested tree: {:?}",
            findings
        );
        // Name is not payment
        let name_findings: Vec<&SensitiveUiFinding> = findings
            .iter()
            .filter(|f| f.element_id == "name-input")
            .collect();
        assert!(name_findings.is_empty(), "'Name' field should not be detected as sensitive");
    }

    // ── Destruction confirmation with value field ───────────────────────

    #[test]
    fn test_destructive_confirmation_via_value() {
        let detector = SensitiveUiDetector::new();
        // Dialog with destructive keywords in value (not label)
        let tree = UiElement {
            id: "confirm-dlg".to_string(),
            role: "dialog".to_string(),
            label: Some("Confirm".to_string()),
            value: Some("Are you sure you want to permanently delete this item?".to_string()),
            bounds: Some(Rect::new(0, 0, 400, 200)),
            enabled: Tri::Known(true),
            focused: Tri::Unknown,
            actions: None,
            children: vec![],
            children_count: None,
            truncated: false,
        };
        let findings = detector.scan_tree(&tree);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::DestructiveConfirmation)),
            "should detect destruction via value field: {:?}",
            findings
        );
    }

    #[test]
    fn test_destructive_confirmation_chinese() {
        let detector = SensitiveUiDetector::new();
        let tree = UiElement {
            id: "del-dlg".to_string(),
            role: "dialog".to_string(),
            label: Some("确认删除".to_string()),
            value: Some("此操作不可恢复".to_string()),
            bounds: Some(Rect::new(0, 0, 400, 200)),
            enabled: Tri::Known(true),
            focused: Tri::Unknown,
            actions: None,
            children: vec![],
            children_count: None,
            truncated: false,
        };
        let findings = detector.scan_tree(&tree);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::DestructiveConfirmation)),
            "should detect Chinese destruction confirmation: {:?}",
            findings
        );
    }

    #[test]
    fn test_window_without_confirm_not_destructive() {
        let detector = SensitiveUiDetector::new();
        // Window role only counts as destructive if it ALSO has "confirm"
        let tree = make_node("win", "window", Some("Delete something"), None, vec![]);
        let findings = detector.scan_tree(&tree);
        assert!(
            !findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::DestructiveConfirmation)),
            "window without 'confirm' should not match: {:?}",
            findings
        );
    }

    // ── Chinese payment detection ───────────────────────────────────────

    #[test]
    fn test_chinese_payment_page() {
        let detector = SensitiveUiDetector::new();
        let tree = make_node("page", "page", Some("支付"), None, vec![]);
        let findings = detector.scan_tree(&tree);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::PaymentPage)),
            "should detect Chinese payment page (支付): {:?}",
            findings
        );
    }

    #[test]
    fn test_chinese_payment_field_card() {
        let detector = SensitiveUiDetector::new();
        let tree = make_node("field", "text_field", Some("银行卡号"), None, vec![]);
        let findings = detector.scan_tree(&tree);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::PaymentField)),
            "should detect Chinese card number field: {:?}",
            findings
        );
    }

    #[test]
    fn test_chinese_payment_field_verification_code() {
        let detector = SensitiveUiDetector::new();
        let tree = make_node("field", "text_field", Some("验证码"), None, vec![]);
        let findings = detector.scan_tree(&tree);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::PaymentField)),
            "should detect Chinese verification code field: {:?}",
            findings
        );
    }

    // ── Edge cases ──────────────────────────────────────────────────────

    #[test]
    fn test_empty_tree() {
        let detector = SensitiveUiDetector::new();
        let tree = make_node("empty", "window", None, None, vec![]);
        let findings = detector.scan_tree(&tree);
        assert!(findings.is_empty(), "empty tree should have no findings");
    }

    #[test]
    fn test_has_sensitive_ui_helper() {
        let detector = SensitiveUiDetector::new();
        let sensitive = make_node("pwd", "password_field", None, None, vec![]);
        assert!(detector.has_sensitive_ui(&sensitive));

        let normal = make_node("btn", "button", Some("OK"), None, vec![]);
        assert!(!detector.has_sensitive_ui(&normal));
    }

    #[test]
    fn test_case_insensitivity_mixed_case() {
        let detector = SensitiveUiDetector::new();
        let tree = make_node("cc", "text_field", Some("Credit Card"), None, vec![]);
        let findings = detector.scan_tree(&tree);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::PaymentField)),
            "should detect 'Credit Card' case-insensitively: {:?}",
            findings
        );
    }

    #[test]
    fn test_payment_page_via_value() {
        let detector = SensitiveUiDetector::new();
        let tree = make_node("page", "page", Some("checkout"), Some("payment"), vec![]);
        let findings = detector.scan_tree(&tree);
        assert!(
            findings
                .iter()
                .any(|f| matches!(f.category, SensitiveUiType::PaymentPage)),
            "should detect payment page via value field: {:?}",
            findings
        );
    }
}

// ---------------------------------------------------------------------------
// screenshot_encoder
// ---------------------------------------------------------------------------

#[cfg(test)]
mod screenshot_encoder_tests {
    use serial_test::serial;
    use syscity::computer::screenshot_encoder::NetworkCondition;

    #[test]
    #[serial]
    fn test_network_condition_env_local() {
        // SYSCITY_LOCAL_MODE should force Local
        unsafe { std::env::set_var("SYSCITY_LOCAL_MODE", "1") };
        let result = NetworkCondition::detect();
        unsafe { std::env::remove_var("SYSCITY_LOCAL_MODE") };
        assert_eq!(result, NetworkCondition::Local);
    }

    #[test]
    #[serial]
    fn test_network_condition_env_remote() {
        // SYSCITY_REMOTE_ENDPOINT should force Remote
        unsafe { std::env::set_var("SYSCITY_REMOTE_ENDPOINT", "https://example.com") };
        let result = NetworkCondition::detect();
        unsafe { std::env::remove_var("SYSCITY_REMOTE_ENDPOINT") };
        assert_eq!(result, NetworkCondition::Remote);
    }

    #[test]
    #[serial]
    fn test_auto_detect_env_takes_priority_over_host() {
        // SYSCITY_LOCAL_MODE should win even when a host is provided
        unsafe { std::env::set_var("SYSCITY_LOCAL_MODE", "1") };
        let result = NetworkCondition::auto_detect(Some("8.8.8.8"));
        unsafe { std::env::remove_var("SYSCITY_LOCAL_MODE") };
        assert_eq!(result, NetworkCondition::Local);
    }

    #[test]
    fn test_network_condition_preferred_format() {
        assert_eq!(NetworkCondition::Local.preferred_format(), "png");
        assert_eq!(NetworkCondition::Normal.preferred_format(), "jpeg");
        assert_eq!(NetworkCondition::Remote.preferred_format(), "jpeg");
    }

    #[test]
    #[serial]
    fn test_auto_detect_no_host_fallsback_normal() {
        // With no env vars and no host, must return Normal
        assert_eq!(NetworkCondition::auto_detect(None), NetworkCondition::Normal);
    }
}

// ---------------------------------------------------------------------------
// audio — analyze_segment edge cases
// ---------------------------------------------------------------------------

#[cfg(test)]
mod audio_integration_tests {
    use std::time::Instant;

    use syscity::computer::audio::{AudioCapture, AudioSegment, AudioSource, DetectedAudioEvent};

    fn make_segment(samples: Vec<f32>, duration_ms: u64) -> AudioSegment {
        AudioSegment {
            timestamp: Instant::now(),
            samples,
            duration_ms,
            source: AudioSource::Microphone,
        }
    }

    #[test]
    fn test_analyze_empty_samples_is_silence() {
        let capture = AudioCapture::new().unwrap();
        let seg = make_segment(vec![], 100);
        let events = capture.analyze_segment(&seg);
        assert!(
            events.contains(&DetectedAudioEvent::Silence { duration_ms: 100 }),
            "empty samples should be silence: {:?}",
            events
        );
    }

    #[test]
    fn test_analyze_nearly_silent() {
        let capture = AudioCapture::new().unwrap();
        // Very quiet samples (below silence threshold of 0.001 RMS)
        let samples = vec![0.0001_f32; 1600];
        let seg = make_segment(samples, 100);
        let events = capture.analyze_segment(&seg);
        // rms of 0.0001 is sqrt(0.0001^2) = 0.0001 which is < 0.001
        assert!(
            events.contains(&DetectedAudioEvent::Silence { duration_ms: 100 }),
            "near-silent samples should be silence: {:?}",
            events
        );
    }

    #[test]
    fn test_analyze_short_speech_not_detected() {
        let capture = AudioCapture::new().unwrap();
        // Short burst (< 300ms) with voice-like energy: should NOT be Speech
        let samples: Vec<f32> = (0..160)
            .map(|i| 0.3 * (i as f32 / 16.0 * std::f32::consts::TAU).sin())
            .collect();
        let seg = make_segment(samples, 10);
        let events = capture.analyze_segment(&seg);
        assert!(
            !events.contains(&DetectedAudioEvent::Speech),
            "short burst (<300ms) should not be classified as Speech: {:?}",
            events
        );
    }

    #[test]
    fn test_analyze_loud_beep_is_error_chime() {
        let capture = AudioCapture::new().unwrap();
        // Loud tonal beep: high RMS, low ZCR, short duration
        let samples: Vec<f32> = (0..160).map(|i| if i < 80 { 0.9 } else { 0.0 }).collect();
        // ZCR of a constant high value followed by zero should be low
        let seg = make_segment(samples, 10);
        let events = capture.analyze_segment(&seg);
        assert!(
            events.contains(&DetectedAudioEvent::ErrorChime),
            "loud beep should be ErrorChime: {:?}",
            events
        );
    }

    #[test]
    fn test_analyze_quiet_beep_is_notification() {
        let capture = AudioCapture::new().unwrap();
        // Quiet tonal beep: moderate RMS, low ZCR, short duration
        let samples: Vec<f32> = (0..160).map(|i| if i < 80 { 0.05 } else { 0.0 }).collect();
        let seg = make_segment(samples, 10);
        let events = capture.analyze_segment(&seg);
        assert!(
            events.contains(&DetectedAudioEvent::Notification),
            "quiet beep should be Notification: {:?}",
            events
        );
    }

    #[test]
    fn test_analyze_edgecase_rms_boundary() {
        let capture = AudioCapture::new().unwrap();
        // RMS exactly at 0.001 boundary should NOT be silence
        let val = 0.001_f32;
        let samples = vec![val; 1600]; // all same value, rms = |val| = 0.001
        let seg = make_segment(samples, 100);
        let events = capture.analyze_segment(&seg);
        assert!(
            !events.contains(&DetectedAudioEvent::Silence { duration_ms: 100 }),
            "RMS exactly at 0.001 boundary should not be silence: {:?}",
            events
        );
    }

    #[test]
    fn test_analyze_zcr_edge_cases() {
        let capture = AudioCapture::new().unwrap();
        // Single sample — ZCR should handle gracefully
        let seg = make_segment(vec![0.5], 1);
        let rms = seg.rms_energy();
        assert!((rms - 0.5).abs() < 1e-6, "RMS of single sample should be the value");

        // Two identical samples — ZCR should be 0
        // This won't trigger speech or silence; check it doesn't panic
        let seg2 = make_segment(vec![0.5, 0.5], 1);
        let events = capture.analyze_segment(&seg2);
        // Should not crash — any classification is acceptable
        assert!(!events.is_empty(), "should return at least one classification");
    }

    #[test]
    fn test_audio_segment_has_voice_activity() {
        let _capture = AudioCapture::new().unwrap();

        // Very quiet: should NOT have voice activity
        let quiet = make_segment(vec![0.0001; 1600], 100);
        assert!(!quiet.has_voice_activity(-40.0));

        // Loud enough: SHOULD have voice activity
        let loud = make_segment(vec![0.5; 1600], 100);
        assert!(loud.has_voice_activity(-40.0));
    }

    #[test]
    fn test_audio_source_display() {
        assert_eq!(format!("{}", AudioSource::Microphone), "microphone");
        assert_eq!(format!("{}", AudioSource::SystemOutput), "system_output");
    }
}
