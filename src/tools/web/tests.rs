//! Error-code taxonomy tests for the web tool module.

// Tests assert against fallible bind/send/read results; unwrapping keeps
// the failure-path assertions readable (same allowance as
// `sandbox_interceptor`).
#![allow(clippy::unwrap_used)]

use super::*;

// ── Error-code taxonomy ─────────────────────────────────────────────────

#[test]
fn test_error_code_wire_values() {
    assert_eq!(WebErrorCode::ConfiguredMissing.as_str(), "CONFIGURED_MISSING");
    assert_eq!(WebErrorCode::NotConfigured.as_str(), "NOT_CONFIGURED");
    assert_eq!(WebErrorCode::Unavailable.as_str(), "UNAVAILABLE");
    assert_eq!(WebErrorCode::Ambiguous.as_str(), "AMBIGUOUS");
}

#[test]
fn test_error_result_carries_code_merged_with_extra() {
    let result = WebErrorCode::Unavailable.error_result("boom", serde_json::json!({ "url": "u" }));
    assert!(!result.success);
    assert_eq!(result.data.as_ref().unwrap()["code"], "UNAVAILABLE");
    assert_eq!(result.data.as_ref().unwrap()["url"], "u");
}

fn attempt(provider: &'static str, kind: AttemptKind) -> ProviderAttempt {
    match kind {
        AttemptKind::MissingValue { field } => ProviderAttempt::missing_value(provider, field),
        AttemptKind::Unavailable { detail } => ProviderAttempt::unavailable(provider, detail),
    }
}

#[test]
fn test_classify_attempts_single_causes() {
    assert_eq!(
        classify_attempts(&[attempt(
            "brave",
            AttemptKind::MissingValue { field: "api_key" }
        )]),
        WebErrorCode::ConfiguredMissing
    );
    assert_eq!(
        classify_attempts(&[attempt(
            "duckduckgo",
            AttemptKind::Unavailable { detail: "timeout".into() }
        )]),
        WebErrorCode::Unavailable
    );
}

#[test]
fn test_classify_attempts_shared_class_keeps_code() {
    let attempts = vec![
        attempt("tavily", AttemptKind::MissingValue { field: "api_key" }),
        attempt("serper", AttemptKind::MissingValue { field: "api_key" }),
    ];
    assert_eq!(classify_attempts(&attempts), WebErrorCode::ConfiguredMissing);

    let attempts = vec![
        attempt("duckduckgo", AttemptKind::Unavailable { detail: "dns".into() }),
        attempt("exa", AttemptKind::Unavailable { detail: "5xx".into() }),
    ];
    assert_eq!(classify_attempts(&attempts), WebErrorCode::Unavailable);
}

#[test]
fn test_classify_attempts_mixed_classes_are_ambiguous() {
    let attempts = vec![
        attempt("brave", AttemptKind::MissingValue { field: "api_key" }),
        attempt("duckduckgo", AttemptKind::Unavailable { detail: "refused".into() }),
    ];
    assert_eq!(classify_attempts(&attempts), WebErrorCode::Ambiguous);
}

#[test]
fn test_classify_attempts_empty_is_not_configured() {
    assert_eq!(classify_attempts(&[]), WebErrorCode::NotConfigured);
}

#[test]
fn test_missing_credential_detection() {
    assert_eq!(missing_credential(&SearchProvider::DuckDuckGo), None);
    assert_eq!(
        missing_credential(&SearchProvider::Brave { api_key: String::new() }),
        Some("api_key")
    );
    assert_eq!(
        missing_credential(&SearchProvider::Tavily { api_key: "  ".to_string() }),
        Some("api_key")
    );
    assert_eq!(missing_credential(&SearchProvider::Serper { api_key: "key".to_string() }), None);
    assert_eq!(
        missing_credential(&SearchProvider::Custom {
            url: "  ".to_string(),
            api_key: None,
            headers: None,
            result_parser: None
        }),
        Some("url")
    );
}
