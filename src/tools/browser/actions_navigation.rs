//! Navigation, traversal and tab actions (single-page mode).

use std::time::Duration;

use super::{BrowserAction, BrowserScreenshot};
use serde_json::json;

use tracing::{info, warn};

use crate::browser::coordinate_space;

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

        BrowserAction::Click { selector } => match page.find_element(&selector).await {
            Ok(elem) => match elem.click().await {
                Ok(_) => {
                    crate::browser::instrument::auto_wait(page).await;
                    Ok(json!({
                        "success": true,
                        "selector": selector
                    }))
                }
                Err(e) => Err(format!("Failed to click element: {}", e)),
            },
            Err(e) => Err(format!("Element not found: {}", e)),
        },

        BrowserAction::Hover { selector } => match page.find_element(&selector).await {
            Ok(elem) => match elem.hover().await {
                Ok(_) => Ok(json!({ "success": true, "selector": selector })),
                Err(e) => Err(format!("Failed to hover element: {}", e)),
            },
            Err(e) => Err(format!("Element not found: {}", e)),
        },

        BrowserAction::ClickAt { x, y } => {
            use chromiumoxide::cdp::browser_protocol::input::{
                DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
            };

            // The point is in the live viewport's units, so it is checked
            // against the live viewport. This costs one read, which is the
            // point: a coordinate lifted from a full-page screenshot would
            // otherwise be dispatched at an off-screen position and reported as
            // a successful click.
            let metrics = match coordinate_space::read(page).await {
                Some(metrics) => metrics,
                None => {
                    return Err(format!(
                        "cannot click at ({x}, {y}): the page's viewport could not be read, so the \
                         point's coordinate space cannot be checked. Take a fresh screenshot and \
                         retry."
                    ))
                }
            };
            coordinate_space::check_click((x, y), &metrics)?;

            let mut press =
                DispatchMouseEventParams::new(DispatchMouseEventType::MousePressed, x, y);
            press.button = Some(MouseButton::Left);
            press.click_count = Some(1);
            let mut release =
                DispatchMouseEventParams::new(DispatchMouseEventType::MouseReleased, x, y);
            release.button = Some(MouseButton::Left);
            release.click_count = Some(1);
            if let Err(e) = page.execute(press).await {
                return Err(format!("Failed to click at ({}, {}): {}", x, y, e));
            }
            if let Err(e) = page.execute(release).await {
                return Err(format!("Failed to click at ({}, {}): {}", x, y, e));
            }
            crate::browser::instrument::auto_wait(page).await;
            Ok(json!({
                "success": true,
                "x": x,
                "y": y,
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
