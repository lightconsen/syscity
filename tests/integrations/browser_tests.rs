//! Browser integration tests
//!
//! These drive a real Chrome through the browser tool: a page is loaded from a
//! `data:` URL, and the assertion is about what the page did rather than about
//! what the tool returned.
//!
//! Three things worth knowing before adding one:
//!
//! - **A `#` in a `data:` URL starts the fragment.** Everything after it — markup
//!   and inline handlers alike — never reaches the document, and the failure
//!   looks like a handler that never fired. Style by colour name, or encode it.
//! - **An assertion about input has to be about an effect the browser produced,
//!   not about a handler running.** `onkeydown` fires for a script-dispatched
//!   `KeyboardEvent` too, which is how a `Press` that delivered nothing real
//!   passed this suite for as long as it did. `test_browser_press_enter_submits`
//!   and `..._tab_moves_focus` assert the default action instead, and
//!   `..._drag_reaches_pointer_listeners` asserts the held-button mask.
//! - **Without Chrome every test here returns early and reports success.** A
//!   green run is not evidence that any of them ran; they need a lane with a
//!   browser installed to mean anything.

#![cfg(feature = "browser")]

use serde_json::json;
use serial_test::serial;
use syscity::browser::{
    assert_navigation_allowed, ActKind, BrowserPool, BrowserPoolConfig, BrowserProfile,
    NavigationPolicy,
};
use syscity::tools::browser::BrowserTool;
use syscity::tools::{Tool, ToolContext};

/// Check if Chrome/Chromium is available on the system
fn chrome_available() -> bool {
    for cmd in [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
    ] {
        if std::process::Command::new("which")
            .arg(cmd)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return true;
        }
    }
    std::path::Path::new("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome").exists()
        || std::path::Path::new("/Applications/Chromium.app/Contents/MacOS/Chromium").exists()
}

/// Detect whether Chrome version is compatible with chromiumoxide 0.9.
/// Very new Chrome versions (200+) may send CDP messages that future
/// chromiumoxide versions cannot deserialize.
fn chrome_compatible() -> bool {
    if !chrome_available() {
        return false;
    }
    let commands: Vec<String> = [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain(std::iter::once(
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".to_string(),
    ))
    .chain(std::iter::once(
        "/Applications/Chromium.app/Contents/MacOS/Chromium".to_string(),
    ))
    .collect();

    for cmd in &commands {
        if let Ok(output) = std::process::Command::new(cmd).arg("--version").output() {
            if let Ok(version_str) = String::from_utf8(output.stdout) {
                let parts: Vec<&str> = version_str.split_whitespace().collect();
                for part in &parts {
                    if let Some(dot_idx) = part.find('.') {
                        if let Ok(major) = part[..dot_idx].parse::<u32>() {
                            if major > 200 {
                                eprintln!(
                                    "Skipping: Chrome {} may be too new for chromiumoxide 0.9. \
                                     Consider upgrading chromiumoxide if tests fail.",
                                    major
                                );
                                return false;
                            }
                            return true;
                        }
                    }
                }
            }
        }
    }
    true
}

fn skip_if_incompatible() {
    if !chrome_available() {
        eprintln!("Skipping: Chrome/Chromium not found.");
    } else if !chrome_compatible() {
        // chrome_compatible() already prints the reason
    }
}

#[tokio::test]
async fn test_browser_navigate_blocks_private_ip() {
    let policy = NavigationPolicy::restrictive();

    assert!(assert_navigation_allowed("http://127.0.0.1/", &policy)
        .await
        .is_err());
    assert!(assert_navigation_allowed("http://10.0.0.1/", &policy)
        .await
        .is_err());
    assert!(assert_navigation_allowed("http://192.168.1.1/", &policy)
        .await
        .is_err());
    assert!(assert_navigation_allowed("http://172.16.0.1/", &policy)
        .await
        .is_err());
    assert!(assert_navigation_allowed("http://[::1]/", &policy)
        .await
        .is_err());
    assert!(assert_navigation_allowed("http://localhost/", &policy)
        .await
        .is_err());
    assert!(assert_navigation_allowed("https://example.com/", &policy)
        .await
        .is_ok());
}

#[test]
fn test_browser_profile_serde_roundtrip() {
    let profile = BrowserProfile::new("test")
        .with_viewport(1920, 1080)
        .with_headless(false)
        .with_user_agent("TestAgent/1.0");

    let json = serde_json::to_string(&profile).unwrap();
    let de: BrowserProfile = serde_json::from_str(&json).unwrap();

    assert_eq!(de.name, "test");
    assert_eq!(de.viewport_width, 1920);
    assert_eq!(de.viewport_height, 1080);
    assert!(!de.headless);
    assert_eq!(de.user_agent, Some("TestAgent/1.0".to_string()));
}

#[test]
fn test_browser_pool_lifecycle() {
    let config = BrowserPoolConfig::default();
    let pool = BrowserPool::new(config);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let profiles = pool.status().await;
        assert!(profiles.is_empty());
    });
}

#[test]
fn test_browser_pool_register_and_status() {
    let config = BrowserPoolConfig::default();
    let pool = BrowserPool::new(config);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let profile = BrowserProfile::headed("test-headed");
        pool.register_profile(profile).await;

        let status = pool.status().await;
        assert!(status.is_empty());
    });
}

#[test]
fn test_browser_pool_with_profiles() {
    let profiles = vec![
        BrowserProfile::new("default"),
        BrowserProfile::headed("headed"),
    ];
    let config = BrowserPoolConfig::default();
    let pool = BrowserPool::with_profiles(config, profiles);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let status = pool.status().await;
        assert!(status.is_empty());
    });
}

#[test]
fn test_act_kind_serde() {
    let click = ActKind::Click;
    let json = serde_json::to_string(&click).unwrap();
    assert!(json.contains("click"));

    let type_action = ActKind::Type { text: "hello".to_string() };
    let json = serde_json::to_string(&type_action).unwrap();
    assert!(json.contains("type"));
    assert!(json.contains("hello"));

    let fill = ActKind::Fill { text: "world".to_string() };
    let json = serde_json::to_string(&fill).unwrap();
    assert!(json.contains("fill"));

    let hover = ActKind::Hover;
    let json = serde_json::to_string(&hover).unwrap();
    assert!(json.contains("hover"));
}

#[tokio::test]
async fn test_navigation_guard_allowlist() {
    let policy = NavigationPolicy {
        allow_private: false,
        allowed_hostnames: vec!["example.com".to_string()],
        blocked_hostnames: Vec::new(),
    };

    assert!(assert_navigation_allowed("https://example.com/", &policy)
        .await
        .is_ok());
    assert!(assert_navigation_allowed("https://google.com/", &policy)
        .await
        .is_err());
}

#[tokio::test]
async fn test_navigation_guard_schemes() {
    let policy = NavigationPolicy::restrictive();

    assert!(assert_navigation_allowed("http://example.com/", &policy)
        .await
        .is_ok());
    assert!(assert_navigation_allowed("https://example.com/", &policy)
        .await
        .is_ok());
    assert!(assert_navigation_allowed("file:///etc/passwd", &policy)
        .await
        .is_err());
    assert!(assert_navigation_allowed("ftp://example.com/", &policy)
        .await
        .is_err());
}

// ── Direct Browser Tool Integration Tests (require compatible Chrome)
// ───────────

#[tokio::test]
#[serial]
async fn test_browser_click() {
    skip_if_incompatible();
    if !chrome_compatible() {
        return;
    }

    let tool = BrowserTool::new();
    let ctx = ToolContext::default();
    let args = json!({
        "actions": [
            { "navigate": { "url": "data:text/html,<html><body><button id='btn' onclick=\"document.body.innerText='clicked'\">Click</button></body></html>" } },
            { "click": { "selector": "#btn" } },
            { "get_text": {} }
        ]
    });

    let result = tool.execute(args, &ctx).await.unwrap();
    assert!(result.success, "browser click failed: {:?}", result.error);
    let data = result.data.expect("expected data");
    let results = data
        .get("results")
        .expect("expected results")
        .as_array()
        .expect("expected array");
    assert_eq!(results.len(), 3);
    let ok_val = results[2].get("Ok").expect("expected Ok");
    let text = ok_val.get("text").and_then(|v| v.as_str()).unwrap_or("");
    assert!(text.contains("clicked"), "expected 'clicked' in page text, got: {}", text);
}

#[tokio::test]
#[serial]
async fn test_browser_type() {
    skip_if_incompatible();
    if !chrome_compatible() {
        return;
    }

    let tool = BrowserTool::new();
    let ctx = ToolContext::default();
    let args = json!({
        "actions": [
            { "navigate": { "url": "data:text/html,<html><body><input id='input' type='text'><div id='result'></div><script>document.getElementById('input').addEventListener('input', function(e) { document.getElementById('result').innerText = e.target.value; });</script></body></html>" } },
            { "type": { "selector": "#input", "text": "hello", "clear": true } },
            { "get_text": { "selector": "#result" } }
        ]
    });

    let result = tool.execute(args, &ctx).await.unwrap();
    assert!(result.success, "browser type failed: {:?}", result.error);
    let data = result.data.expect("expected data");
    let results = data
        .get("results")
        .expect("expected results")
        .as_array()
        .expect("expected array");
    assert_eq!(results.len(), 3);
    let ok_val = results[2].get("Ok").expect("expected Ok");
    let text = ok_val.get("text").and_then(|v| v.as_str()).unwrap_or("");
    assert!(text.contains("hello"), "expected 'hello' in result, got: {}", text);
}

#[tokio::test]
#[serial]
async fn test_browser_scroll() {
    skip_if_incompatible();
    if !chrome_compatible() {
        return;
    }

    let tool = BrowserTool::new();
    let ctx = ToolContext::default();
    let args = json!({
        "actions": [
            { "navigate": { "url": "data:text/html,<html><body><div style='height:3000px'></div><div id='bottom'>Bottom</div></body></html>" } },
            { "scroll": { "direction": "down", "amount": 1000 } },
            { "execute_script": { "script": "return window.scrollY;" } }
        ]
    });

    let result = tool.execute(args, &ctx).await.unwrap();
    assert!(result.success, "browser scroll failed: {:?}", result.error);
    let data = result.data.expect("expected data");
    let results = data
        .get("results")
        .expect("expected results")
        .as_array()
        .expect("expected array");
    assert_eq!(results.len(), 3);
    let ok_val = results[2].get("Ok").expect("expected Ok");
    let scroll_y = ok_val.get("result").and_then(|v| v.as_f64()).unwrap_or(0.0);
    assert!(scroll_y > 0.0, "expected scrollY > 0, got: {}", scroll_y);
}

#[tokio::test]
#[serial]
async fn test_browser_press() {
    skip_if_incompatible();
    if !chrome_compatible() {
        return;
    }

    let tool = BrowserTool::new();
    let ctx = ToolContext::default();
    let args = json!({
        "actions": [
            { "navigate": { "url": "data:text/html,<html><body><input id='input' type='text' onkeydown=\"document.getElementById('result').innerText='pressed:'+event.key\"><div id='result'></div></body></html>" } },
            { "click": { "selector": "#input" } },
            { "press": { "key": "a" } },
            { "get_text": { "selector": "#result" } }
        ]
    });

    let result = tool.execute(args, &ctx).await.unwrap();
    assert!(result.success, "browser press failed: {:?}", result.error);
    let data = result.data.expect("expected data");
    let results = data
        .get("results")
        .expect("expected results")
        .as_array()
        .expect("expected array");
    assert_eq!(results.len(), 4);
    let ok_val = results[3].get("Ok").expect("expected Ok");
    let text = ok_val.get("text").and_then(|v| v.as_str()).unwrap_or("");
    assert!(text.contains("pressed:a"), "expected 'pressed:a' in result, got: {}", text);
}

/// Pressing Enter has to submit the form.
///
/// This is the assertion the rest of this file cannot make. A test that reads
/// what an `onkeydown` handler wrote into the DOM passes just as happily when
/// the key was a synthesized `KeyboardEvent` — those fire the handlers too. Only
/// a key the browser itself acted on submits a form, moves focus, or closes an
/// overlay, which is why `Press` sends CDP key events rather than dispatching
/// events from a script.
#[tokio::test]
#[serial]
async fn test_browser_press_enter_submits_the_form() {
    skip_if_incompatible();
    if !chrome_compatible() {
        return;
    }

    let tool = BrowserTool::new();
    let ctx = ToolContext::default();
    let args = json!({
        "actions": [
            { "navigate": { "url": "data:text/html,<html><body><form onsubmit=\"event.preventDefault();document.body.innerText='submitted'\"><input id='input' type='text'></form></body></html>" } },
            { "click": { "selector": "#input" } },
            { "press": { "key": "Enter" } },
            { "get_text": {} }
        ]
    });

    let result = tool.execute(args, &ctx).await.unwrap();
    assert!(result.success, "browser press failed: {:?}", result.error);
    let data = result.data.expect("expected data");
    let results = data
        .get("results")
        .expect("results")
        .as_array()
        .expect("array");
    assert_eq!(results.len(), 4);
    let text = results[3]
        .get("Ok")
        .and_then(|v| v.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        text.contains("submitted"),
        "Enter did not submit the form — the page text was {text:?}"
    );
}

/// Pressing Tab has to move the focus.
///
/// Same reason as the test above, and the same blind spot in the others: focus
/// moves because the browser handled the key, not because a handler ran.
#[tokio::test]
#[serial]
async fn test_browser_press_tab_moves_focus() {
    skip_if_incompatible();
    if !chrome_compatible() {
        return;
    }

    let tool = BrowserTool::new();
    let ctx = ToolContext::default();
    let args = json!({
        "actions": [
            { "navigate": { "url": "data:text/html,<html><body><input id='first' type='text'><input id='second' type='text'></body></html>" } },
            { "click": { "selector": "#first" } },
            { "press": { "key": "Tab" } },
            { "execute_script": { "script": "return (document.activeElement || {}).id || '(none)'" } }
        ]
    });

    let result = tool.execute(args, &ctx).await.unwrap();
    assert!(result.success, "browser press failed: {:?}", result.error);
    let data = result.data.expect("expected data");
    let results = data
        .get("results")
        .expect("results")
        .as_array()
        .expect("array");
    assert_eq!(results.len(), 4);
    let focused = results[3]
        .get("Ok")
        .and_then(|v| v.get("result"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(focused, "second", "Tab did not move the focus — activeElement is {focused:?}");
}

/// A coordinate click lands where it was aimed.
#[tokio::test]
#[serial]
async fn test_browser_click_at_coordinates() {
    skip_if_incompatible();
    if !chrome_compatible() {
        return;
    }

    let tool = BrowserTool::new();
    let ctx = ToolContext::default();
    let args = json!({
        "actions": [
            { "navigate": { "url": "data:text/html,<html><body><button id='b' style='position:absolute;left:100px;top:50px;width:200px;height:40px' onclick=\"document.body.innerText='clicked'\">Go</button></body></html>" } },
            { "click_at": { "x": 200, "y": 70 } },
            { "get_text": {} }
        ]
    });

    let result = tool.execute(args, &ctx).await.unwrap();
    assert!(result.success, "click_at failed: {:?}", result.error);
    let data = result.data.expect("expected data");
    let results = data
        .get("results")
        .expect("results")
        .as_array()
        .expect("array");
    let text = results[2]
        .get("Ok")
        .and_then(|v| v.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(text.contains("clicked"), "coordinate click missed: {text:?}");
}

/// A double click is a real one.
///
/// Two press/release pairs carrying click counts 1 and 2 is what a browser needs
/// in order to raise `dblclick`; one pair carrying `clickCount: 2` is not.
#[tokio::test]
#[serial]
async fn test_browser_click_at_double_click() {
    skip_if_incompatible();
    if !chrome_compatible() {
        return;
    }

    let tool = BrowserTool::new();
    let ctx = ToolContext::default();
    let args = json!({
        "actions": [
            { "navigate": { "url": "data:text/html,<html><body><button id='b' style='position:absolute;left:100px;top:50px;width:200px;height:40px' ondblclick=\"document.body.innerText='double'\">Go</button></body></html>" } },
            { "click_at": { "x": 200, "y": 70, "click_count": 2 } },
            { "get_text": {} }
        ]
    });

    let result = tool.execute(args, &ctx).await.unwrap();
    assert!(result.success, "double click failed: {:?}", result.error);
    let data = result.data.expect("expected data");
    let results = data
        .get("results")
        .expect("results")
        .as_array()
        .expect("array");
    let text = results[2]
        .get("Ok")
        .and_then(|v| v.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(text.contains("double"), "no dblclick was raised: {text:?}");
}

/// The right button raises the page's own context menu event.
#[tokio::test]
#[serial]
async fn test_browser_click_at_right_button() {
    skip_if_incompatible();
    if !chrome_compatible() {
        return;
    }

    let tool = BrowserTool::new();
    let ctx = ToolContext::default();
    let args = json!({
        "actions": [
            { "navigate": { "url": "data:text/html,<html><body><div style='position:absolute;left:100px;top:50px;width:200px;height:40px' oncontextmenu=\"document.body.innerText='menu';return false\">right</div></body></html>" } },
            { "click_at": { "x": 200, "y": 70, "button": "right" } },
            { "get_text": {} }
        ]
    });

    let result = tool.execute(args, &ctx).await.unwrap();
    assert!(result.success, "right click failed: {:?}", result.error);
    let data = result.data.expect("expected data");
    let results = data
        .get("results")
        .expect("results")
        .as_array()
        .expect("array");
    let text = results[2]
        .get("Ok")
        .and_then(|v| v.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(text.contains("menu"), "no contextmenu event: {text:?}");
}

/// A drag is pointer events with the button held.
///
/// Both halves of that sentence were wrong before: the old implementation
/// dispatched JavaScript `MouseEvent`s, so a page listening for `pointermove`
/// heard nothing at all — and it sent no movement between press and release, so
/// there was nothing to hear anyway. This asserts the listener fires *and* that
/// it sees `buttons === 1`, the field that separates a drag from a hover.
#[tokio::test]
#[serial]
async fn test_browser_drag_reaches_pointer_listeners() {
    skip_if_incompatible();
    if !chrome_compatible() {
        return;
    }

    let tool = BrowserTool::new();
    let ctx = ToolContext::default();
    // The listener is on the body and records the highest `buttons` it ever
    // sees, so the answer does not depend on the pointer staying inside the
    // element it started on — a drag is expected to leave it.
    //
    // No `#` anywhere in this page: in a data URL it starts the fragment, so
    // everything after it — handlers included — never reaches the document. The
    // div is styled by colour name for that reason.
    let page = "data:text/html,<html><body id='body' \
                onpointermove=\"window.__b=Math.max(window.__b||0,event.buttons);document.getElementById('log').textContent='buttons '+window.__b\">\
                <div id='log'>none</div>\
                <div id='src' style='position:absolute;left:100px;top:50px;width:120px;height:60px;background:gray'>drag</div>\
                </body></html>";
    let args = json!({
        "actions": [
            { "navigate": { "url": page } },
            { "drag_at": { "from_x": 150, "from_y": 75, "to_x": 400, "to_y": 300 } },
            { "get_text": { "selector": "#log" } }
        ]
    });

    let result = tool.execute(args, &ctx).await.unwrap();
    assert!(result.success, "drag failed: {:?}", result.error);
    let data = result.data.expect("expected data");
    let results = data
        .get("results")
        .expect("results")
        .as_array()
        .expect("array");
    let text = results[2]
        .get("Ok")
        .and_then(|v| v.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        text.contains("buttons 1"),
        "no pointermove carried the held button: {text:?} | all results: {results:?}"
    );
}

/// A hotkey is a combination, not two keystrokes in a row.
///
/// Control+A with the modifier held selects the field's contents; pressing
/// Control and then A does not. That difference lives entirely in the
/// `modifiers` mask each event carries, so this is the assertion that tells the
/// two implementations apart.
#[tokio::test]
#[serial]
async fn test_browser_hotkey_selects_all() {
    skip_if_incompatible();
    if !chrome_compatible() {
        return;
    }

    // Select-all is Cmd+A on macOS and Ctrl+A elsewhere. The mechanism under
    // test is the same one; the combination has to be the one the platform
    // actually binds, or the test would be asserting that a shortcut macOS does
    // not have fails to work.
    let modifier = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };

    let tool = BrowserTool::new();
    let ctx = ToolContext::default();
    let args = json!({
        "actions": [
            { "navigate": { "url": "data:text/html,<html><body><input id='field' type='text' value='selectme'><div id='log'>none</div></body></html>" } },
            { "click": { "selector": "#field" } },
            { "hotkey": { "keys": [modifier, "a"] } },
            { "execute_script": { "script": "const el = document.activeElement; return String(el.selectionStart) + '-' + String(el.selectionEnd)" } }
        ]
    });

    let result = tool.execute(args, &ctx).await.unwrap();
    assert!(result.success, "hotkey failed: {:?}", result.error);
    let data = result.data.expect("expected data");
    let results = data
        .get("results")
        .expect("results")
        .as_array()
        .expect("array");
    let selection = results[3]
        .get("Ok")
        .and_then(|v| v.get("result"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(
        selection, "0-8",
        "{modifier}+A did not select the field — selection was {selection:?}"
    );
}
