//! Browser action type definitions and normalization.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single form field for `FillForm`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormField {
    /// CSS selector of the input
    pub selector: String,
    /// Value to type into it
    pub value: String,
    /// Clear existing content first (default: true)
    pub clear: Option<bool>,
}

/// Browser action types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserAction {
    /// Navigate to a URL
    Navigate { url: String },
    /// Click on an element
    Click {
        selector: String,
        /// left (default), middle or right
        button: Option<String>,
        /// 1 (default), 2 for a double click, 3 for a triple
        click_count: Option<u32>,
    },
    /// Type text into an input field
    Type {
        selector: String,
        text: String,
        clear: Option<bool>,
    },
    /// Fill multiple form fields in one action
    FillForm {
        /// Fields to fill: each has a CSS selector and a value
        fields: Vec<FormField>,
    },
    /// Hover over an element
    Hover { selector: String },
    /// Click at viewport coordinates
    ClickAt {
        x: f64,
        y: f64,
        /// left (default), middle or right
        button: Option<String>,
        /// 1 (default), 2 for a double click, 3 for a triple
        click_count: Option<u32>,
    },
    /// Get the current page HTML
    GetHtml,
    /// Get text content of the page or specific element
    GetText { selector: Option<String> },
    /// Take a screenshot
    Screenshot {
        full_page: Option<bool>,
        selector: Option<String>,
    },
    /// Wait for an element to appear
    WaitFor {
        selector: String,
        timeout_ms: Option<u64>,
    },
    /// Scroll the page
    Scroll { direction: String, amount: u32 },
    /// Execute JavaScript
    ExecuteScript { script: String },
    /// Go back in history
    Back,
    /// Go forward in history
    Forward,
    /// Reload the page
    Reload,
    /// Get all cookies for the current page
    GetCookies,
    /// Set a cookie
    SetCookie {
        name: String,
        value: String,
        domain: Option<String>,
        path: Option<String>,
    },
    /// Clear all cookies
    ClearCookies,
    /// Print page to PDF
    PrintToPdf {
        landscape: Option<bool>,
        display_header_footer: Option<bool>,
        print_background: Option<bool>,
        scale: Option<f64>,
        paper_width: Option<f64>,
        paper_height: Option<f64>,
        margin_top: Option<f64>,
        margin_bottom: Option<f64>,
        margin_left: Option<f64>,
        margin_right: Option<f64>,
        page_ranges: Option<String>,
    },
    /// Get performance metrics
    GetPerformanceMetrics,
    /// Get network log (fetch/XHR) with optional filtering and bodies
    GetNetworkLog {
        /// Substring filter on the request URL
        url: Option<String>,
        /// HTTP method filter (GET, POST, ...)
        method: Option<String>,
        /// Resource type filter (document, xhr, fetch, script, img, stylesheet, ...)
        resource_type: Option<String>,
        /// Minimum HTTP status code (inclusive)
        min_status: Option<u16>,
        /// Maximum HTTP status code (inclusive)
        max_status: Option<u16>,
        /// Include (truncated) response bodies, default true
        include_body: Option<bool>,
        /// Max entries to return (default 50)
        limit: Option<usize>,
        /// Entries to skip (pagination)
        offset: Option<usize>,
    },
    /// Get captured console messages and uncaught exceptions
    GetConsoleMessages {
        /// Level filter: log, info, warn, error, debug, exception
        level: Option<String>,
        /// Max entries to return (default 100)
        limit: Option<usize>,
    },
    /// Clear captured network and console buffers
    ClearCaptures,
    /// Start a screencast, saving JPEG frames to an artifacts directory
    ScreencastStart {
        /// JPEG quality 0-100 (default 80)
        quality: Option<u32>,
        /// Save every Nth frame (default 1)
        every_nth_frame: Option<u32>,
    },
    /// Stop the active screencast and return the saved frames location
    ScreencastStop,
    /// Set mobile device emulation
    EmulateMobile { device_name: String },
    /// Emulate network conditions (throttling)
    EmulateNetwork {
        /// Extra latency in ms
        latency_ms: Option<f64>,
        /// Download throughput in bytes/sec (-1 or None = no limit)
        download_bps: Option<f64>,
        /// Upload throughput in bytes/sec
        upload_bps: Option<f64>,
        /// Simulate offline mode
        offline: Option<bool>,
    },
    /// Throttle CPU by a slowdown factor (1 = no throttle, 4 = 4x slower)
    EmulateCpu { rate: f64 },
    /// Set viewport size dynamically
    SetViewport {
        width: u32,
        height: u32,
        device_scale_factor: Option<f64>,
        mobile: Option<bool>,
    },
    /// Take an ARIA snapshot of the current page
    Snapshot { max_chars: Option<usize> },
    /// Act on an element by ref_id from a previous snapshot
    #[cfg(feature = "browser")]
    Act {
        ref_id: usize,
        action: crate::browser::ActKind,
    },
    /// Press a key on the page
    Press { key: String },
    /// Drag an element to another element, or by an offset
    Drag {
        selector: String,
        target_selector: Option<String>,
        delta_x: Option<i32>,
        delta_y: Option<i32>,
        /// Intermediate moves between press and release (default 12)
        steps: Option<u32>,
    },
    /// Drag between viewport coordinates
    DragAt {
        from_x: f64,
        from_y: f64,
        to_x: f64,
        to_y: f64,
        /// Intermediate moves between press and release (default 12)
        steps: Option<u32>,
        /// left (default), middle or right
        button: Option<String>,
    },
    /// Select text in an input or textarea
    Select {
        selector: String,
        text: Option<String>,
        start: Option<usize>,
        end: Option<usize>,
    },
    /// Upload files to a file input element
    UploadFiles {
        selector: String,
        files: Vec<String>,
    },
    /// Handle a JavaScript dialog (alert/confirm/prompt)
    HandleDialog {
        action: String,
        text: Option<String>,
    },
    /// Set download behavior
    SetDownloadBehavior {
        behavior: String,
        download_path: Option<String>,
    },
    /// List browser tabs/pages
    ListTabs,
    /// Switch to a specific tab by index or title
    SwitchTab {
        index: Option<usize>,
        title: Option<String>,
    },
    /// Close a specific tab by index or title
    CloseTab {
        index: Option<usize>,
        title: Option<String>,
    },
}

/// `ClickAt` -> `click_at`, which is the name serde accepts for the variant.
fn snake_case_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            for lower in c.to_lowercase() {
                out.push(lower);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse one action, accepting the shapes callers actually send.
///
/// serde takes the canonical externally-tagged snake_case form,
/// `{"click_at": {"x": 1}}`. Two other shapes are tolerated because they reach
/// us anyway: the PascalCase variant names the JSON schema advertises
/// (`{"ClickAt": {"x": 1}}`) and an `action` key naming the variant
/// (`{"action": "ClickAt", "x": 1}`). The previous helper rewrote that `action`
/// value and left serde to fail on the object, so neither tolerated shape
/// actually worked — a model following the schema got "unknown variant
/// `ClickAt`" back.
pub(super) fn parse_action(value: &Value) -> Result<BrowserAction, serde_json::Error> {
    // A unit variant arrives as a bare string.
    if let Some(name) = value.as_str() {
        return serde_json::from_value(Value::String(snake_case_name(name)));
    }

    let Some(object) = value.as_object() else {
        return serde_json::from_value(value.clone());
    };

    // `{"action": "ClickAt", ...rest}` -> `{"click_at": {...rest}}`.
    if let Some(name) = object.get("action").and_then(Value::as_str) {
        let mut rest = object.clone();
        rest.remove("action");
        let mut tagged = serde_json::Map::new();
        tagged.insert(snake_case_name(name), Value::Object(rest));
        return serde_json::from_value(Value::Object(tagged));
    }

    // `{"ClickAt": {...}}` -> `{"click_at": {...}}`.
    if object.len() == 1 {
        if let Some((name, body)) = object.iter().next() {
            let canonical = snake_case_name(name);
            if canonical != *name {
                let mut tagged = serde_json::Map::new();
                tagged.insert(canonical, body.clone());
                return serde_json::from_value(Value::Object(tagged));
            }
        }
    }

    serde_json::from_value(value.clone())
}

#[cfg(test)]
mod parse_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_canonical_snake_case_shape_parses() {
        let parsed = parse_action(&json!({ "click_at": { "x": 1.0, "y": 2.0 } })).unwrap();
        assert!(matches!(parsed, BrowserAction::ClickAt { x: 1.0, y: 2.0, .. }));
    }

    #[test]
    fn the_pascal_case_shape_the_schema_advertises_parses() {
        // This is the shape the tool's JSON schema shows the model, and it used
        // to come back as "unknown variant `ClickAt`".
        let parsed = parse_action(&json!({ "ClickAt": { "x": 1.0, "y": 2.0 } })).unwrap();
        assert!(matches!(parsed, BrowserAction::ClickAt { x: 1.0, y: 2.0, .. }));
    }

    #[test]
    fn an_action_key_naming_the_variant_parses() {
        let parsed = parse_action(&json!({
            "action": "Navigate",
            "url": "https://example.com"
        }))
        .unwrap();
        assert!(
            matches!(parsed, BrowserAction::Navigate { ref url } if url == "https://example.com")
        );
    }

    #[test]
    fn unit_variants_parse_from_a_bare_string_in_either_case() {
        assert!(matches!(parse_action(&json!("list_tabs")).unwrap(), BrowserAction::ListTabs));
        assert!(matches!(parse_action(&json!("ListTabs")).unwrap(), BrowserAction::ListTabs));
    }

    #[test]
    fn a_name_that_is_not_a_variant_is_still_an_error() {
        // Tolerating other shapes must not turn a typo into a silent no-op.
        assert!(parse_action(&json!({ "lick_at": { "x": 1.0 } })).is_err());
        assert!(parse_action(&json!("NotAnAction")).is_err());
        assert!(parse_action(&json!(42)).is_err());
    }

    #[test]
    fn a_canonical_name_is_left_alone() {
        // Round-tripping a serialized action must not depend on the rewrite.
        let original = BrowserAction::Screenshot {
            full_page: Some(true),
            selector: None,
        };
        let value = serde_json::to_value(&original).unwrap();
        assert!(matches!(
            parse_action(&value).unwrap(),
            BrowserAction::Screenshot { full_page: Some(true), .. }
        ));
    }
}
