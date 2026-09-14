//! Browser automation tool for Syscity
//!
//! Provides web browser automation capabilities using headless Chrome/Chromium.
//! Supports navigation, clicking, form input, screenshots, and content
//! extraction.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::tools::sdk::ToolCapabilities;

mod action;
#[cfg(feature = "browser")]
mod actions_content;
#[cfg(feature = "browser")]
mod actions_cookies;
#[cfg(feature = "browser")]
mod actions_form;
#[cfg(feature = "browser")]
mod actions_navigation;
#[cfg(feature = "browser")]
mod actions_network;
#[cfg(feature = "browser")]
mod actions_screenshot;
#[cfg(feature = "browser")]
mod execution;
#[cfg(feature = "browser")]
mod screencast;

use action::normalize_browser_actions;
pub use action::{BrowserAction, FormField};

/// How a captured browser screenshot is handed from the screenshot action to
/// result assembly: normally a content-addressed attachment reference; inline
/// base64 only as the fail-open fallback when the store is unavailable.
#[cfg(feature = "browser")]
pub(super) enum BrowserScreenshot {
    /// Bytes stored in the attachment CAS; only the reference travels.
    Ref(crate::attachments::AttachmentRef),
    /// Fail-open fallback: raw base64, the pre-CAS behavior.
    Inline(String),
}

#[cfg(feature = "browser")]
use actions_content::execute_content_actions;
#[cfg(feature = "browser")]
use actions_cookies::execute_cookies_actions;
#[cfg(feature = "browser")]
use actions_form::execute_form_actions;
#[cfg(feature = "browser")]
use actions_navigation::execute_navigation_actions;
#[cfg(feature = "browser")]
use actions_network::execute_network_actions;
#[cfg(feature = "browser")]
use actions_screenshot::execute_screenshot_actions;
pub struct BrowserTool {
    /// Chrome/Chromium executable path (None = auto-detect)
    chrome_path: Option<String>,
    /// Default viewport width
    viewport_width: u32,
    /// Default viewport height
    viewport_height: u32,
    /// Whether to run headless (default: true)
    headless: bool,
    /// Default timeout for browser operations (feature-gated: only used when
    /// `browser` feature is enabled, hence `#[allow(dead_code)]` to suppress
    /// warnings when feature is off).
    #[allow(dead_code)]
    default_timeout: Duration,
    /// Optional browser pool for persistent sessions
    #[cfg(feature = "browser")]
    pool: Option<std::sync::Arc<crate::browser::BrowserPool>>,
    /// Profile name for pool-based sessions
    profile: String,
}

impl Default for BrowserTool {
    fn default() -> Self {
        Self {
            chrome_path: None,
            viewport_width: 1280,
            viewport_height: 720,
            headless: true,
            default_timeout: Duration::from_secs(30),
            #[cfg(feature = "browser")]
            pool: None,
            profile: "default".to_string(),
        }
    }
}

impl BrowserTool {
    /// Create a new browser tool
    pub fn new() -> Self {
        Self::default()
    }

    /// Set Chrome/Chromium executable path
    pub fn with_chrome_path(mut self, path: impl Into<String>) -> Self {
        self.chrome_path = Some(path.into());
        self
    }

    /// Set viewport size
    pub fn with_viewport(mut self, width: u32, height: u32) -> Self {
        self.viewport_width = width;
        self.viewport_height = height;
        self
    }

    /// Set headless mode
    pub fn with_headless(mut self, headless: bool) -> Self {
        self.headless = headless;
        self
    }

    /// Set browser pool for persistent sessions (browser feature only)
    #[cfg(feature = "browser")]
    pub fn with_pool(mut self, pool: std::sync::Arc<crate::browser::BrowserPool>) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Set profile name for pool-based sessions
    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = profile.into();
        self
    }

    /// Execute a single browser action against a page.
    #[cfg(feature = "browser")]
    async fn execute_single_action(
        action: BrowserAction,
        page: &chromiumoxide::Page,
        browser: Option<&chromiumoxide::Browser>,
        screenshot_data: &mut Option<BrowserScreenshot>,
    ) -> Result<serde_json::Value, String> {
        match action {
            BrowserAction::Navigate { .. }
            | BrowserAction::Click { .. }
            | BrowserAction::Hover { .. }
            | BrowserAction::ClickAt { .. }
            | BrowserAction::Back
            | BrowserAction::Forward
            | BrowserAction::Reload
            | BrowserAction::WaitFor { .. }
            | BrowserAction::Scroll { .. }
            | BrowserAction::ListTabs
            | BrowserAction::SwitchTab { .. }
            | BrowserAction::CloseTab { .. } => {
                execute_navigation_actions(action, page, browser, screenshot_data).await
            }
            BrowserAction::GetHtml
            | BrowserAction::GetText { .. }
            | BrowserAction::Snapshot { .. }
            | BrowserAction::GetPerformanceMetrics
            | BrowserAction::GetConsoleMessages { .. }
            | BrowserAction::ExecuteScript { .. }
            | BrowserAction::Press { .. }
            | BrowserAction::Act { .. } => {
                execute_content_actions(action, page, browser, screenshot_data).await
            }
            BrowserAction::Type { .. }
            | BrowserAction::FillForm { .. }
            | BrowserAction::Select { .. }
            | BrowserAction::Drag { .. }
            | BrowserAction::UploadFiles { .. }
            | BrowserAction::HandleDialog { .. }
            | BrowserAction::SetDownloadBehavior { .. } => {
                execute_form_actions(action, page, browser, screenshot_data).await
            }
            BrowserAction::GetCookies
            | BrowserAction::SetCookie { .. }
            | BrowserAction::ClearCookies => {
                execute_cookies_actions(action, page, browser, screenshot_data).await
            }
            BrowserAction::GetNetworkLog { .. }
            | BrowserAction::EmulateNetwork { .. }
            | BrowserAction::EmulateCpu { .. }
            | BrowserAction::ClearCaptures
            | BrowserAction::EmulateMobile { .. }
            | BrowserAction::SetViewport { .. } => {
                execute_network_actions(action, page, browser, screenshot_data).await
            }
            BrowserAction::Screenshot { .. }
            | BrowserAction::PrintToPdf { .. }
            | BrowserAction::ScreencastStart { .. }
            | BrowserAction::ScreencastStop => {
                execute_screenshot_actions(action, page, browser, screenshot_data).await
            }
        }
    }

    /// Fallback implementation when browser feature is not enabled
    #[cfg(not(feature = "browser"))]
    async fn execute_actions(
        &self,
        _actions: Vec<BrowserAction>,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        Ok(ToolExecutionResult::error(
            "Browser automation not available. Build with --features browser to enable.",
        ))
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        "Automate web browser interactions. Navigate to URLs, click elements, fill forms, take \
         screenshots, extract content, and execute JavaScript. Clicking/typing automatically waits \
         for the page to settle (navigation + network idle). Also captures network traffic \
         (fetch/XHR with response bodies via GetNetworkLog), console messages and uncaught \
         exceptions (GetConsoleMessages), and can record screencasts as JPEG frame sequences \
         (ScreencastStart/ScreencastStop). Use this tool when the user asks to open a webpage, \
         browse the web, take a website screenshot, debug a page's network/console activity, or \
         automate browser actions (打开网页/浏览/网页截图/网页调试). Coordinates are viewport CSS \
         pixels: every screenshot reports its coordinate space, image size, viewport size, device \
         pixel ratio and scroll offset, so image pixels and click coordinates can be reconciled \
         instead of confused — and a click outside the live viewport is refused rather than \
         dispatched somewhere unnamed. Requires Chrome/Chromium to be installed."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "actions": {
                    "type": "array",
                    "description": "List of browser actions to execute in sequence",
                    "items": {
                        "type": "object",
                        "oneOf": [
                            {
                                "type": "object",
                                "properties": {
                                    "Navigate": {
                                        "type": "object",
                                        "properties": {
                                            "url": { "type": "string", "description": "URL to navigate to" }
                                        },
                                        "required": ["url"]
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "Click": {
                                        "type": "object",
                                        "properties": {
                                            "selector": { "type": "string", "description": "CSS selector for element to click" }
                                        },
                                        "required": ["selector"]
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "Type": {
                                        "type": "object",
                                        "properties": {
                                            "selector": { "type": "string", "description": "CSS selector for input field" },
                                            "text": { "type": "string", "description": "Text to type" },
                                            "clear": { "type": "boolean", "description": "Clear field before typing (default: true)" }
                                        },
                                        "required": ["selector", "text"]
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "GetHtml": {
                                        "type": "object",
                                        "properties": {}
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "GetText": {
                                        "type": "object",
                                        "properties": {
                                            "selector": { "type": "string", "description": "Optional CSS selector (omit for full page)" }
                                        }
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "Screenshot": {
                                        "type": "object",
                                        "properties": {
                                            "full_page": { "type": "boolean", "description": "Capture full page (default: false). A full-page image is document-sized, not viewport-sized — its points need converting before they can be clicked, and the result says so." },
                                            "selector": { "type": "string", "description": "Optional CSS selector for specific element" }
                                        }
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "WaitFor": {
                                        "type": "object",
                                        "properties": {
                                            "selector": { "type": "string", "description": "CSS selector to wait for" },
                                            "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds (default: 5000)" }
                                        },
                                        "required": ["selector"]
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "Scroll": {
                                        "type": "object",
                                        "properties": {
                                            "direction": { "type": "string", "enum": ["up", "down"], "description": "Scroll direction" },
                                            "amount": { "type": "integer", "description": "Pixels to scroll" }
                                        },
                                        "required": ["direction", "amount"]
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "ExecuteScript": {
                                        "type": "object",
                                        "properties": {
                                            "script": { "type": "string", "description": "JavaScript code to execute" }
                                        },
                                        "required": ["script"]
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "Back": { "type": "object", "properties": {} }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "Forward": { "type": "object", "properties": {} }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "Reload": { "type": "object", "properties": {} }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "GetCookies": { "type": "object", "properties": {} }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "SetCookie": {
                                        "type": "object",
                                        "properties": {
                                            "name": { "type": "string", "description": "Cookie name" },
                                            "value": { "type": "string", "description": "Cookie value" },
                                            "domain": { "type": "string", "description": "Optional cookie domain" },
                                            "path": { "type": "string", "description": "Optional cookie path" }
                                        },
                                        "required": ["name", "value"]
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "ClearCookies": { "type": "object", "properties": {} }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "PrintToPdf": {
                                        "type": "object",
                                        "properties": {
                                            "landscape": { "type": "boolean", "description": "Landscape orientation" },
                                            "display_header_footer": { "type": "boolean", "description": "Show header/footer" },
                                            "print_background": { "type": "boolean", "description": "Print background graphics" },
                                            "scale": { "type": "number", "description": "Scale factor (0.1-2.0)" },
                                            "paper_width": { "type": "number", "description": "Paper width in inches" },
                                            "paper_height": { "type": "number", "description": "Paper height in inches" },
                                            "margin_top": { "type": "number", "description": "Top margin in inches" },
                                            "margin_bottom": { "type": "number", "description": "Bottom margin in inches" },
                                            "margin_left": { "type": "number", "description": "Left margin in inches" },
                                            "margin_right": { "type": "number", "description": "Right margin in inches" },
                                            "page_ranges": { "type": "string", "description": "Page ranges to print (e.g., '1-5, 8, 11-13')" }
                                        }
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "GetPerformanceMetrics": { "type": "object", "properties": {} }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "GetNetworkLog": {
                                        "type": "object",
                                        "properties": {
                                            "url": { "type": "string", "description": "Substring filter on request URL" },
                                            "method": { "type": "string", "description": "HTTP method filter (GET, POST, ...)" },
                                            "resource_type": { "type": "string", "description": "Resource type filter (document, xhr, fetch, script, img, stylesheet, ...)" },
                                            "min_status": { "type": "integer", "description": "Minimum HTTP status (inclusive)" },
                                            "max_status": { "type": "integer", "description": "Maximum HTTP status (inclusive)" },
                                            "include_body": { "type": "boolean", "description": "Include truncated response bodies (default: true)" },
                                            "limit": { "type": "integer", "description": "Max entries to return (default: 50)" },
                                            "offset": { "type": "integer", "description": "Entries to skip for pagination (default: 0)" }
                                        }
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "FillForm": {
                                        "type": "object",
                                        "properties": {
                                            "fields": {
                                                "type": "array",
                                                "items": {
                                                    "type": "object",
                                                    "properties": {
                                                        "selector": { "type": "string", "description": "CSS selector of the input" },
                                                        "value": { "type": "string", "description": "Value to type" },
                                                        "clear": { "type": "boolean", "description": "Clear first (default: true)" }
                                                    },
                                                    "required": ["selector", "value"]
                                                },
                                                "description": "Fields to fill"
                                            }
                                        },
                                        "required": ["fields"]
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "Hover": {
                                        "type": "object",
                                        "properties": {
                                            "selector": { "type": "string", "description": "CSS selector to hover" }
                                        },
                                        "required": ["selector"]
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "ClickAt": {
                                        "type": "object",
                                        "properties": {
                                            "x": { "type": "number", "description": "Viewport x coordinate in CSS pixels. If it came from a screenshot, divide image pixels by the device_pixel_ratio the screenshot reported (and subtract its scroll offset for a full_page capture). A point outside the live viewport is refused." },
                                            "y": { "type": "number", "description": "Viewport y coordinate in CSS pixels, same units as x." }
                                        },
                                        "required": ["x", "y"]
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "EmulateNetwork": {
                                        "type": "object",
                                        "properties": {
                                            "latency_ms": { "type": "number", "description": "Extra latency in ms (default: 0)" },
                                            "download_bps": { "type": "number", "description": "Download throughput bytes/sec (default: unlimited)" },
                                            "upload_bps": { "type": "number", "description": "Upload throughput bytes/sec (default: unlimited)" },
                                            "offline": { "type": "boolean", "description": "Simulate offline (default: false)" }
                                        }
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "EmulateCpu": {
                                        "type": "object",
                                        "properties": {
                                            "rate": { "type": "number", "description": "CPU slowdown factor (1 = none, 4 = 4x slower)" }
                                        },
                                        "required": ["rate"]
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "GetConsoleMessages": {
                                        "type": "object",
                                        "properties": {
                                            "level": { "type": "string", "enum": ["log", "info", "warn", "error", "debug", "exception"], "description": "Level filter" },
                                            "limit": { "type": "integer", "description": "Max messages to return (default: 100)" }
                                        }
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "ClearCaptures": { "type": "object", "properties": {} }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "ScreencastStart": {
                                        "type": "object",
                                        "properties": {
                                            "quality": { "type": "integer", "description": "JPEG quality 0-100 (default: 80)" },
                                            "every_nth_frame": { "type": "integer", "description": "Save every Nth frame (default: 1)" }
                                        }
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "ScreencastStop": { "type": "object", "properties": {} }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "EmulateMobile": {
                                        "type": "object",
                                        "properties": {
                                            "device_name": { "type": "string", "enum": ["iphone_x", "iphone_12", "pixel_5", "ipad"], "description": "Device to emulate" }
                                        },
                                        "required": ["device_name"]
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "SetViewport": {
                                        "type": "object",
                                        "properties": {
                                            "width": { "type": "integer", "description": "Viewport width in pixels" },
                                            "height": { "type": "integer", "description": "Viewport height in pixels" },
                                            "device_scale_factor": { "type": "number", "description": "Device scale factor (DPR)" },
                                            "mobile": { "type": "boolean", "description": "Enable mobile emulation" }
                                        },
                                        "required": ["width", "height"]
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "Snapshot": {
                                        "type": "object",
                                        "properties": {
                                            "max_chars": { "type": "integer", "description": "Maximum characters in snapshot (default: 8000)" }
                                        }
                                    }
                                }
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "Act": {
                                        "type": "object",
                                        "properties": {
                                            "ref_id": { "type": "integer", "description": "Reference ID from a previous snapshot" },
                                            "action": {
                                                "type": "object",
                                                "oneOf": [
                                                    { "type": "object", "properties": { "click": { "type": "object", "properties": {} } } },
                                                    { "type": "object", "properties": { "type": { "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] } } },
                                                    { "type": "object", "properties": { "hover": { "type": "object", "properties": {} } } },
                                                    { "type": "object", "properties": { "fill": { "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] } } }
                                                ]
                                            }
                                        },
                                        "required": ["ref_id", "action"]
                                    }
                                }
                            }
                        ]
                    }
                }
            },
            "required": ["actions"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: true,
            risk_level: crate::tools::approval::RiskLevel::High,
            categories: vec!["network".to_string(), "browser".to_string()],
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        mut args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        // Normalize action names to handle LLM-generated PascalCase variant names
        if let Some(actions_val) = args.get_mut("actions") {
            normalize_browser_actions(actions_val);
        }

        let actions: Vec<BrowserAction> = serde_json::from_value(
            args.get("actions").cloned().unwrap_or(json!([])),
        )
        .map_err(|e| {
            crate::error::SyscityError::Validation(format!("Invalid browser actions: {}", e))
        })?;

        if actions.is_empty() {
            return Ok(ToolExecutionResult::error("No browser actions specified"));
        }

        self.execute_actions(actions, context).await
    }

    fn timeout(&self, _context: &ToolContext) -> Duration {
        Duration::from_secs(60) // Browser operations can take longer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_browser_tool_name() {
        let tool = BrowserTool::new();
        assert_eq!(tool.name(), "browser");
    }

    #[test]
    fn test_browser_tool_schema() {
        let tool = BrowserTool::new();
        let schema = tool.parameters_schema();
        assert!(schema.get("properties").is_some());
    }

    #[test]
    fn test_browser_tool_default() {
        let tool = BrowserTool::default();
        assert_eq!(tool.viewport_width, 1280);
        assert_eq!(tool.viewport_height, 720);
        assert!(tool.headless);
        assert_eq!(tool.default_timeout, Duration::from_secs(30));
        assert!(tool.chrome_path.is_none());
        assert_eq!(tool.profile, "default");
    }

    #[test]
    fn test_browser_tool_with_chrome_path() {
        let tool = BrowserTool::new().with_chrome_path("/usr/bin/chrome");
        assert_eq!(tool.chrome_path, Some("/usr/bin/chrome".to_string()));
    }

    #[test]
    fn test_browser_tool_with_viewport() {
        let tool = BrowserTool::new().with_viewport(1920, 1080);
        assert_eq!(tool.viewport_width, 1920);
        assert_eq!(tool.viewport_height, 1080);
    }

    #[test]
    fn test_browser_tool_with_headless() {
        let tool = BrowserTool::new().with_headless(false);
        assert!(!tool.headless);
    }

    #[test]
    fn test_browser_tool_with_profile() {
        let tool = BrowserTool::new().with_profile("headed");
        assert_eq!(tool.profile, "headed");
    }

    #[test]
    fn test_browser_tool_timeout() {
        let tool = BrowserTool::new();
        let ctx = ToolContext::default();
        assert_eq!(tool.timeout(&ctx), Duration::from_secs(60));
    }

    #[test]
    fn test_browser_action_serialization() {
        let nav = BrowserAction::Navigate {
            url: "https://example.com".to_string(),
        };
        let json = serde_json::to_string(&nav).unwrap();
        assert!(json.contains("navigate"));
        assert!(json.contains("example.com"));

        let click = BrowserAction::Click { selector: "#btn".to_string() };
        let json = serde_json::to_string(&click).unwrap();
        assert!(json.contains("click"));

        let back = BrowserAction::Back;
        let json = serde_json::to_string(&back).unwrap();
        assert!(json.contains("back"));
    }

    #[tokio::test]
    async fn test_browser_tool_execute_empty_actions() {
        let tool = BrowserTool::new();
        let ctx = ToolContext::default();
        let result = tool.execute(json!({"actions": []}), &ctx).await.unwrap();
        assert!(!result.success);
    }

    #[test]
    #[cfg(feature = "browser")]
    fn test_browser_action_new_variants_serialization() {
        let get_cookies = BrowserAction::GetCookies;
        let json = serde_json::to_string(&get_cookies).unwrap();
        assert!(json.contains("get_cookies"));

        let set_cookie = BrowserAction::SetCookie {
            name: "session".to_string(),
            value: "abc123".to_string(),
            domain: Some(".example.com".to_string()),
            path: Some("/".to_string()),
        };
        let json = serde_json::to_string(&set_cookie).unwrap();
        assert!(json.contains("set_cookie"));
        assert!(json.contains("session"));
        assert!(json.contains("abc123"));

        let clear_cookies = BrowserAction::ClearCookies;
        let json = serde_json::to_string(&clear_cookies).unwrap();
        assert!(json.contains("clear_cookies"));

        let pdf = BrowserAction::PrintToPdf {
            landscape: Some(true),
            display_header_footer: Some(false),
            print_background: Some(true),
            scale: Some(1.5),
            paper_width: Some(8.5),
            paper_height: Some(11.0),
            margin_top: Some(0.5),
            margin_bottom: Some(0.5),
            margin_left: Some(0.5),
            margin_right: Some(0.5),
            page_ranges: Some("1-3".to_string()),
        };
        let json = serde_json::to_string(&pdf).unwrap();
        assert!(json.contains("print_to_pdf"));
        assert!(json.contains("1.5"));

        let perf = BrowserAction::GetPerformanceMetrics;
        let json = serde_json::to_string(&perf).unwrap();
        assert!(json.contains("get_performance_metrics"));

        let net = BrowserAction::GetNetworkLog {
            url: None,
            method: None,
            resource_type: None,
            min_status: None,
            max_status: None,
            include_body: None,
            limit: None,
            offset: None,
        };
        let json = serde_json::to_string(&net).unwrap();
        assert!(json.contains("get_network_log"));

        let net_filtered = BrowserAction::GetNetworkLog {
            url: Some("/api/".to_string()),
            method: Some("POST".to_string()),
            resource_type: Some("xhr".to_string()),
            min_status: Some(200),
            max_status: Some(299),
            include_body: Some(false),
            limit: Some(10),
            offset: Some(5),
        };
        let json = serde_json::to_string(&net_filtered).unwrap();
        assert!(json.contains("/api/"));
        let roundtrip: BrowserAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            roundtrip,
            BrowserAction::GetNetworkLog {
                min_status: Some(200),
                limit: Some(10),
                ..
            }
        ));

        let console = BrowserAction::GetConsoleMessages {
            level: Some("error".to_string()),
            limit: Some(20),
        };
        let json = serde_json::to_string(&console).unwrap();
        assert!(json.contains("get_console_messages"));
        assert!(json.contains("error"));

        let clear = BrowserAction::ClearCaptures;
        let json = serde_json::to_string(&clear).unwrap();
        assert!(json.contains("clear_captures"));

        let cast_start = BrowserAction::ScreencastStart {
            quality: Some(70),
            every_nth_frame: Some(2),
        };
        let json = serde_json::to_string(&cast_start).unwrap();
        assert!(json.contains("screencast_start"));
        assert!(json.contains("70"));

        let cast_stop = BrowserAction::ScreencastStop;
        let json = serde_json::to_string(&cast_stop).unwrap();
        assert!(json.contains("screencast_stop"));

        let form = BrowserAction::FillForm {
            fields: vec![
                FormField {
                    selector: "#user".to_string(),
                    value: "alice".to_string(),
                    clear: None,
                },
                FormField {
                    selector: "#pass".to_string(),
                    value: "secret".to_string(),
                    clear: Some(false),
                },
            ],
        };
        let json = serde_json::to_string(&form).unwrap();
        assert!(json.contains("fill_form"));
        assert!(json.contains("#user"));
        let roundtrip: BrowserAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            roundtrip,
            BrowserAction::FillForm { ref fields } if fields.len() == 2
        ));

        let hover = BrowserAction::Hover { selector: "#menu".to_string() };
        let json = serde_json::to_string(&hover).unwrap();
        assert!(json.contains("hover"));

        let click_at = BrowserAction::ClickAt { x: 100.5, y: 200.0 };
        let json = serde_json::to_string(&click_at).unwrap();
        assert!(json.contains("click_at"));

        let emu_net = BrowserAction::EmulateNetwork {
            latency_ms: Some(200.0),
            download_bps: Some(1_000_000.0),
            upload_bps: None,
            offline: Some(false),
        };
        let json = serde_json::to_string(&emu_net).unwrap();
        assert!(json.contains("emulate_network"));
        assert!(json.contains("200"));

        let emu_cpu = BrowserAction::EmulateCpu { rate: 4.0 };
        let json = serde_json::to_string(&emu_cpu).unwrap();
        assert!(json.contains("emulate_cpu"));

        let mobile = BrowserAction::EmulateMobile {
            device_name: "iphone_x".to_string(),
        };
        let json = serde_json::to_string(&mobile).unwrap();
        assert!(json.contains("emulate_mobile"));
        assert!(json.contains("iphone_x"));

        let viewport = BrowserAction::SetViewport {
            width: 1920,
            height: 1080,
            device_scale_factor: Some(2.0),
            mobile: Some(false),
        };
        let json = serde_json::to_string(&viewport).unwrap();
        assert!(json.contains("set_viewport"));
        assert!(json.contains("1920"));

        let snapshot = BrowserAction::Snapshot { max_chars: Some(4000) };
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains("snapshot"));
        assert!(json.contains("4000"));

        let act = BrowserAction::Act {
            ref_id: 3,
            action: crate::browser::ActKind::Click,
        };
        let json = serde_json::to_string(&act).unwrap();
        assert!(json.contains("act"));
        assert!(json.contains("3"));
        assert!(json.contains("click"));

        let press = BrowserAction::Press { key: "Enter".to_string() };
        let json = serde_json::to_string(&press).unwrap();
        assert!(json.contains("press"));
        assert!(json.contains("Enter"));

        let drag = BrowserAction::Drag {
            selector: "#item".to_string(),
            target_selector: Some("#dropzone".to_string()),
            delta_x: Some(100),
            delta_y: Some(0),
        };
        let json = serde_json::to_string(&drag).unwrap();
        assert!(json.contains("drag"));
        assert!(json.contains("#item"));

        let select = BrowserAction::Select {
            selector: "#input".to_string(),
            text: Some("hello".to_string()),
            start: None,
            end: None,
        };
        let json = serde_json::to_string(&select).unwrap();
        assert!(json.contains("select"));
        assert!(json.contains("hello"));

        let upload = BrowserAction::UploadFiles {
            selector: "#file".to_string(),
            files: vec!["/tmp/test.txt".to_string()],
        };
        let json = serde_json::to_string(&upload).unwrap();
        assert!(json.contains("upload_files"));
        assert!(json.contains("/tmp/test.txt"));

        let dialog = BrowserAction::HandleDialog {
            action: "accept".to_string(),
            text: Some("ok".to_string()),
        };
        let json = serde_json::to_string(&dialog).unwrap();
        assert!(json.contains("handle_dialog"));
        assert!(json.contains("accept"));

        let download = BrowserAction::SetDownloadBehavior {
            behavior: "allow".to_string(),
            download_path: Some("/tmp".to_string()),
        };
        let json = serde_json::to_string(&download).unwrap();
        assert!(json.contains("set_download_behavior"));
        assert!(json.contains("allow"));

        let list_tabs = BrowserAction::ListTabs;
        let json = serde_json::to_string(&list_tabs).unwrap();
        assert!(json.contains("list_tabs"));

        let switch_tab = BrowserAction::SwitchTab {
            index: Some(1),
            title: Some("Example".to_string()),
        };
        let json = serde_json::to_string(&switch_tab).unwrap();
        assert!(json.contains("switch_tab"));
        assert!(json.contains("Example"));

        let close_tab = BrowserAction::CloseTab { index: Some(0), title: None };
        let json = serde_json::to_string(&close_tab).unwrap();
        assert!(json.contains("close_tab"));
    }

    #[test]
    #[cfg(feature = "browser")]
    fn test_browser_action_deserialization_new_variants() {
        // Unit variants serialize as {"variant": null}
        let get_cookies: BrowserAction =
            serde_json::from_value(json!({"get_cookies": null})).unwrap();
        assert!(matches!(get_cookies, BrowserAction::GetCookies));

        let set_cookie: BrowserAction = serde_json::from_value(json!({
            "set_cookie": { "name": "foo", "value": "bar", "domain": "example.com" }
        }))
        .unwrap();
        assert!(
            matches!(set_cookie, BrowserAction::SetCookie { ref name, ref value, .. } if name == "foo" && value == "bar")
        );

        let pdf: BrowserAction = serde_json::from_value(json!({
            "print_to_pdf": { "landscape": true, "scale": 1.2 }
        }))
        .unwrap();
        assert!(matches!(
            pdf,
            BrowserAction::PrintToPdf {
                landscape: Some(true),
                scale: Some(1.2),
                ..
            }
        ));

        let mobile: BrowserAction = serde_json::from_value(json!({
            "emulate_mobile": { "device_name": "pixel_5" }
        }))
        .unwrap();
        assert!(
            matches!(mobile, BrowserAction::EmulateMobile { ref device_name } if device_name == "pixel_5")
        );

        let viewport: BrowserAction = serde_json::from_value(json!({
            "set_viewport": { "width": 800, "height": 600, "mobile": true }
        }))
        .unwrap();
        assert!(matches!(
            viewport,
            BrowserAction::SetViewport {
                width: 800,
                height: 600,
                mobile: Some(true),
                ..
            }
        ));

        let snapshot: BrowserAction = serde_json::from_value(json!({
            "snapshot": { "max_chars": 5000 }
        }))
        .unwrap();
        assert!(matches!(snapshot, BrowserAction::Snapshot { max_chars: Some(5000) }));

        let act: BrowserAction = serde_json::from_value(json!({
            "act": { "ref_id": 7, "action": { "click": null } }
        }))
        .unwrap();
        assert!(matches!(
            act,
            BrowserAction::Act {
                ref_id: 7,
                action: crate::browser::ActKind::Click
            }
        ));

        let act_type: BrowserAction = serde_json::from_value(json!({
            "act": { "ref_id": 2, "action": { "type": { "text": "hello" } } }
        }))
        .unwrap();
        assert!(matches!(
            act_type,
            BrowserAction::Act { ref_id: 2, action: crate::browser::ActKind::Type { ref text } } if text == "hello"
        ));

        let press: BrowserAction = serde_json::from_value(json!({
            "press": { "key": "Enter" }
        }))
        .unwrap();
        assert!(matches!(press, BrowserAction::Press { ref key } if key == "Enter"));

        let drag: BrowserAction = serde_json::from_value(json!({
            "drag": { "selector": "#item", "target_selector": "#dropzone", "delta_x": 100, "delta_y": 0 }
        })).unwrap();
        assert!(matches!(drag, BrowserAction::Drag { ref selector, .. } if selector == "#item"));

        let select: BrowserAction = serde_json::from_value(json!({
            "select": { "selector": "#input", "text": "hello", "start": 0, "end": 5 }
        }))
        .unwrap();
        assert!(
            matches!(select, BrowserAction::Select { ref selector, .. } if selector == "#input")
        );

        let upload: BrowserAction = serde_json::from_value(json!({
            "upload_files": { "selector": "#file", "files": ["/tmp/test.txt"] }
        }))
        .unwrap();
        assert!(
            matches!(upload, BrowserAction::UploadFiles { ref selector, .. } if selector == "#file")
        );

        let dialog: BrowserAction = serde_json::from_value(json!({
            "handle_dialog": { "action": "accept", "text": "ok" }
        }))
        .unwrap();
        assert!(
            matches!(dialog, BrowserAction::HandleDialog { ref action, .. } if action == "accept")
        );

        let download: BrowserAction = serde_json::from_value(json!({
            "set_download_behavior": { "behavior": "allow", "download_path": "/tmp" }
        }))
        .unwrap();
        assert!(
            matches!(download, BrowserAction::SetDownloadBehavior { ref behavior, .. } if behavior == "allow")
        );

        let list_tabs: BrowserAction = serde_json::from_value(json!({"list_tabs": null})).unwrap();
        assert!(matches!(list_tabs, BrowserAction::ListTabs));

        let switch_tab: BrowserAction = serde_json::from_value(json!({
            "switch_tab": { "index": 1, "title": "Example" }
        }))
        .unwrap();
        assert!(matches!(switch_tab, BrowserAction::SwitchTab { index: Some(1), .. }));

        let close_tab: BrowserAction = serde_json::from_value(json!({
            "close_tab": { "index": 0 }
        }))
        .unwrap();
        assert!(matches!(close_tab, BrowserAction::CloseTab { index: Some(0), .. }));
    }
}
