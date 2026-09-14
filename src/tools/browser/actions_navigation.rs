//! Navigation, traversal and tab actions (single-page mode).

use std::time::Duration;

use super::{BrowserAction, BrowserScreenshot};
use serde_json::{json, Value};

use tracing::{info, warn};

use chromiumoxide::cdp::browser_protocol::input::{
    DispatchMouseEventParams, DispatchMouseEventType,
};

use crate::browser::coordinate_space::{self, PageMetrics};
use crate::browser::pointer::{self, PointerButton, PointerStep};

/// Dispatch a pointer sequence, one CDP event per step.
///
/// The sequence is built by `browser::pointer`; this is only the mapping onto
/// CDP, so that clicks and drags differ in how the steps are computed rather
/// than in how they are sent.
pub(super) async fn dispatch_pointer(
    page: &chromiumoxide::Page,
    steps: &[PointerStep],
    button: PointerButton,
) -> Result<(), String> {
    for step in steps {
        let kind = match step.kind {
            pointer::StepKind::Move => DispatchMouseEventType::MouseMoved,
            pointer::StepKind::Press => DispatchMouseEventType::MousePressed,
            pointer::StepKind::Release => DispatchMouseEventType::MouseReleased,
        };
        let mut params = DispatchMouseEventParams::new(kind, step.x, step.y);
        // `button` names the button that changed; a move changes none. The held
        // set travels separately in `buttons`.
        params.button = match step.kind {
            pointer::StepKind::Move => None,
            _ => Some(button.cdp()),
        };
        params.buttons = Some(step.buttons);
        params.click_count = Some(step.click_count);
        page.execute(params).await.map_err(|e| {
            format!("Failed to send a pointer event at ({}, {}): {}", step.x, step.y, e)
        })?;
    }
    Ok(())
}

/// Click a resolved point, after checking it lands in the live viewport.
///
/// Returns the metrics of the moment, so the caller can report the space the
/// click was made in.
pub(super) async fn click_point(
    page: &chromiumoxide::Page,
    point: (f64, f64),
    button: PointerButton,
    count: u32,
) -> Result<PageMetrics, String> {
    let metrics = match coordinate_space::read(page).await {
        Some(metrics) => metrics,
        None => {
            return Err(format!(
                "cannot click at ({}, {}): the page's viewport could not be read, so the point's \
                 coordinate space cannot be checked. Take a fresh screenshot and retry.",
                point.0, point.1
            ))
        }
    };
    coordinate_space::check_click(point, &metrics)?;
    dispatch_pointer(page, &pointer::click_sequence(point, button, count), button).await?;
    crate::browser::instrument::auto_wait(page).await;
    Ok(metrics)
}

/// The centre of an element's border box, in viewport CSS pixels.
pub(super) async fn element_center(
    page: &chromiumoxide::Page,
    selector: &str,
) -> Result<(f64, f64), String> {
    let script = format!(
        r#"() => {{
                const el = document.querySelector('{}');
                if (!el) return {{ error: 'Element not found: {}' }};
                const r = el.getBoundingClientRect();
                if (r.width === 0 && r.height === 0) {{
                    return {{ error: 'Element has no visible box: {}' }};
                }}
                return {{ x: r.left + r.width / 2, y: r.top + r.height / 2 }};
            }}"#,
        selector, selector, selector
    );
    let value = page
        .evaluate(script.as_str())
        .await
        .map_err(|e| format!("Failed to measure {}: {}", selector, e))?
        .value()
        .cloned()
        .unwrap_or(json!(null));

    if let Some(message) = value.get("error").and_then(Value::as_str) {
        return Err(message.to_string());
    }
    match (value.get("x").and_then(Value::as_f64), value.get("y").and_then(Value::as_f64)) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(format!("Could not measure {}: the page returned {}", selector, value)),
    }
}

pub(super) async fn execute_navigation_actions(
    action: BrowserAction,
    page: &chromiumoxide::Page,
    _browser: Option<&chromiumoxide::Browser>,
    _screenshot_data: &mut Option<BrowserScreenshot>,
) -> Result<serde_json::Value, String> {
    match action {
        BrowserAction::Navigate { url } => {
            info!("Navigating to: {}", url);
            match page.goto(&url).await {
                Ok(_) => {
                    if let Err(e) = page.wait_for_navigation().await {
                        warn!("Navigation wait failed after page load: {}", e);
                    }
                    Ok(json!({
                        "success": true,
                        "url": url,
                        "title": page.get_title().await.ok().flatten().unwrap_or_default()
                    }))
                }
                Err(e) => Err(format!("Failed to navigate: {}", e)),
            }
        }

        BrowserAction::Click { selector, button, click_count } => {
            let button = PointerButton::parse(button.as_deref())?;
            let count = click_count.unwrap_or(1);

            // A plain left click stays with chromiumoxide, which resolves the
            // element itself (scrolling it into view, honouring the CDP click
            // path). The other buttons and counts are not offered there, so the
            // element is measured and clicked as a point — which also means the
            // same viewport check applies to it.
            if button == PointerButton::Left && count == 1 {
                match page.find_element(&selector).await {
                    Ok(elem) => match elem.click().await {
                        Ok(_) => {
                            crate::browser::instrument::auto_wait(page).await;
                            Ok(json!({ "success": true, "selector": selector }))
                        }
                        Err(e) => Err(format!("Failed to click element: {}", e)),
                    },
                    Err(e) => Err(format!("Element not found: {}", e)),
                }
            } else {
                let point = element_center(page, &selector).await?;
                let metrics = click_point(page, point, button, count).await?;
                Ok(json!({
                    "success": true,
                    "selector": selector,
                    "x": point.0,
                    "y": point.1,
                    "button": button.name(),
                    "click_count": count,
                    "viewport": { "width": metrics.width, "height": metrics.height },
                }))
            }
        }

        BrowserAction::Hover { selector } => match page.find_element(&selector).await {
            Ok(elem) => match elem.hover().await {
                Ok(_) => Ok(json!({ "success": true, "selector": selector })),
                Err(e) => Err(format!("Failed to hover element: {}", e)),
            },
            Err(e) => Err(format!("Element not found: {}", e)),
        },

        BrowserAction::ClickAt { x, y, button, click_count } => {
            let button = PointerButton::parse(button.as_deref())?;
            let count = click_count.unwrap_or(1);
            let metrics = click_point(page, (x, y), button, count).await?;
            Ok(json!({
                "success": true,
                "x": x,
                "y": y,
                "button": button.name(),
                "click_count": count,
                "viewport": { "width": metrics.width, "height": metrics.height },
            }))
        }

        BrowserAction::Back => {
            let script = r#"() => { history.back(); return location.href; }"#;
            match page.evaluate(script).await {
                Ok(result) => {
                    let url = result.into_value::<String>().unwrap_or_default();
                    Ok(json!({ "success": true, "action": "back", "url": url }))
                }
                Err(e) => Err(format!("Failed to go back: {}", e)),
            }
        }

        BrowserAction::Forward => {
            let script = r#"() => { history.forward(); return location.href; }"#;
            match page.evaluate(script).await {
                Ok(result) => {
                    let url = result.into_value::<String>().unwrap_or_default();
                    Ok(json!({ "success": true, "action": "forward", "url": url }))
                }
                Err(e) => Err(format!("Failed to go forward: {}", e)),
            }
        }

        BrowserAction::Reload => match page.reload().await {
            Ok(_) => Ok(json!({ "success": true, "action": "reload" })),
            Err(e) => Err(format!("Failed to reload: {}", e)),
        },

        BrowserAction::WaitFor { selector, timeout_ms } => {
            let timeout = Duration::from_millis(timeout_ms.unwrap_or(5000));
            let start = std::time::Instant::now();

            loop {
                if start.elapsed() > timeout {
                    break Err(format!("Timeout waiting for element: {}", selector));
                }

                match page.find_element(&selector).await {
                    Ok(_) => {
                        break Ok(json!({
                            "success": true,
                            "selector": selector
                        }))
                    }
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }

        BrowserAction::Scroll { direction, amount } => {
            let (dx, dy) = if direction == "up" {
                (0, -(amount as i32))
            } else if direction == "down" {
                (0, amount as i32)
            } else if direction == "left" {
                (-(amount as i32), 0)
            } else {
                (amount as i32, 0)
            };
            let script =
                format!(r#"() => {{ window.scrollBy({}, {}); return window.scrollY; }}"#, dx, dy);

            match page.evaluate(script.as_str()).await {
                Ok(result) => {
                    let scroll_y = result.into_value::<f64>().unwrap_or(0.0);
                    Ok(json!({
                        "success": true,
                        "direction": direction,
                        "amount": amount,
                        "scroll_y": scroll_y
                    }))
                }
                Err(e) => Err(format!("Failed to scroll: {}", e)),
            }
        }

        BrowserAction::ListTabs => Err("Tab management requires pool-based mode".to_string()),

        BrowserAction::SwitchTab { .. } => {
            Err("Tab management requires pool-based mode".to_string())
        }

        BrowserAction::CloseTab { .. } => {
            Err("Tab management requires pool-based mode".to_string())
        }
        _ => Err("browser: action not handled by this group".to_string()),
    }
}
