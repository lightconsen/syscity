//! Form filling, text input, drag/select and download behavior.

use super::{BrowserAction, BrowserScreenshot};
use serde_json::{json, Value};

use tracing::warn;

use crate::browser::coordinate_space;
use crate::browser::pointer::{self, PointerButton};

/// Turn a script's own result into the action's result, treating the `error`
/// field these scripts use as a failure.
///
/// They report a missing element *inside* the value they return, so the action
/// used to answer `success: true` with `{"result": {"error": ...}}` — a failure
/// dressed as a success, which a caller reading `success` has no way to notice.
fn js_result(value: Value) -> Result<Value, String> {
    match value.get("error").and_then(Value::as_str) {
        Some(message) => Err(message.to_string()),
        None => Ok(json!({ "success": true, "result": value })),
    }
}

/// Drag between two viewport points, as a real pointer sequence.
///
/// This used to dispatch three JS-synthesized `MouseEvent`s at two points. Those
/// are `isTrusted: false`, they are not pointer events at all — so anything
/// listening for `pointerdown`/`pointermove` (most modern drag code) heard
/// nothing — and with no movement between press and release the gesture was a
/// click with extra steps. This sends CDP mouse events, carrying the held-button
/// mask that separates a drag from a hover.
async fn drag_between(
    page: &chromiumoxide::Page,
    from: (f64, f64),
    to: (f64, f64),
    steps: Option<u32>,
    button: PointerButton,
) -> Result<Value, String> {
    let steps = pointer::validated_steps(steps)?;

    let metrics =
        match coordinate_space::read(page).await {
            Some(metrics) => metrics,
            None => return Err(
                "cannot drag: the page's viewport could not be read, so the points' coordinate \
                 space cannot be checked. Take a fresh screenshot and retry."
                    .to_string(),
            ),
        };
    coordinate_space::check_click(from, &metrics)
        .map_err(|refusal| format!("drag start {refusal}"))?;
    coordinate_space::check_click(to, &metrics).map_err(|refusal| format!("drag end {refusal}"))?;

    let sequence = pointer::drag_sequence(from, to, steps, button);
    super::actions_navigation::dispatch_pointer(page, &sequence, button).await?;
    crate::browser::instrument::auto_wait(page).await;

    let mut result = json!({
        "success": true,
        "from": { "x": from.0, "y": from.1 },
        "to": { "x": to.0, "y": to.1 },
        "button": button.name(),
        "steps": steps,
        "viewport": { "width": metrics.width, "height": metrics.height },
    });

    // An HTML5 draggable element does not move on a press-and-move sequence: the
    // browser raises a drag from `dragstart`, which a synthetic pointer cannot
    // produce. Fixing that needs Input.dispatchDragEvent, so it is reported
    // rather than silently attempted — but reported, not refused, because some
    // pages set `draggable` and handle the pointer events themselves, and
    // refusing would block the drags that do work.
    if let Some(element) = html5_draggable_at(page, from).await? {
        if let Some(object) = result.as_object_mut() {
            object.insert(
                "drag_and_drop_warning".to_string(),
                json!(format!(
                    "the start point is on an HTML5 draggable element ({element}); the browser \
                     starts a native drag from dragstart, which a synthetic press-and-move does \
                     not raise. If the page relies on native drag-and-drop this will not move it — \
                     verify the effect rather than assuming it worked."
                )),
            );
        }
    }

    Ok(result)
}

/// The HTML5-draggable element under a point, described for a message.
async fn html5_draggable_at(
    page: &chromiumoxide::Page,
    point: (f64, f64),
) -> Result<Option<String>, String> {
    let script = format!(
        r#"() => {{
                const el = document.elementFromPoint({}, {});
                if (!el) return {{ draggable: null }};
                const d = el.closest('[draggable="true"]');
                if (!d) return {{ draggable: null }};
                const id = d.id ? '#' + d.id : '';
                return {{ draggable: d.tagName.toLowerCase() + id }};
            }}"#,
        point.0, point.1
    );
    let value = page
        .evaluate(script.as_str())
        .await
        .map_err(|e| format!("Failed to inspect the drag source: {e}"))?
        .value()
        .cloned()
        .unwrap_or(json!(null));
    Ok(value
        .get("draggable")
        .and_then(Value::as_str)
        .map(str::to_string))
}

pub(super) async fn execute_form_actions(
    action: BrowserAction,
    page: &chromiumoxide::Page,
    browser: Option<&chromiumoxide::Browser>,
    _screenshot_data: &mut Option<BrowserScreenshot>,
) -> Result<serde_json::Value, String> {
    match action {
        BrowserAction::Type { selector, text, clear } => match page.find_element(&selector).await {
            Ok(elem) => {
                if clear.unwrap_or(true) {
                    if elem.click().await.is_err() {
                        warn!("Failed to click browser element before typing");
                    }
                    if elem.click().await.is_err() {
                        warn!("Failed to click browser element before typing");
                    }
                }
                match elem.type_str(&text).await {
                    Ok(_) => {
                        crate::browser::instrument::auto_wait(page).await;
                        Ok(json!({
                            "success": true,
                            "selector": selector,
                            "text_length": text.len()
                        }))
                    }
                    Err(e) => Err(format!("Failed to type: {}", e)),
                }
            }
            Err(e) => Err(format!("Element not found: {}", e)),
        },

        BrowserAction::FillForm { fields } => {
            let mut filled = 0usize;
            let mut errors = Vec::new();
            for field in &fields {
                match page.find_element(&field.selector).await {
                    Ok(elem) => {
                        if field.clear.unwrap_or(true) {
                            let _ = elem.click().await;
                        }
                        match elem.type_str(&field.value).await {
                            Ok(_) => filled += 1,
                            Err(e) => errors.push(format!("{}: {}", field.selector, e)),
                        }
                    }
                    Err(e) => errors.push(format!("{}: {}", field.selector, e)),
                }
            }
            crate::browser::instrument::auto_wait(page).await;
            if errors.is_empty() {
                Ok(json!({ "success": true, "filled": filled }))
            } else {
                Err(format!(
                    "Filled {}/{} fields; errors: {}",
                    filled,
                    fields.len(),
                    errors.join("; ")
                ))
            }
        }

        BrowserAction::Select { selector, text, start, end } => {
            let script = if let Some(text) = text {
                format!(
                    r#"() => {{
                            const el = document.querySelector('{}');
                            if (!el) return {{ error: 'Element not found' }};
                            if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
                                el.focus();
                                const idx = el.value.indexOf('{}');
                                if (idx >= 0) {{
                                    el.setSelectionRange(idx, idx + {});
                                    return {{ success: true, selected: '{}' }};
                                }}
                                return {{ error: 'Text not found in element' }};
                            }}
                            const range = document.createRange();
                            const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
                            let node;
                            while ((node = walker.nextNode())) {{
                                const idx = node.textContent.indexOf('{}');
                                if (idx >= 0) {{
                                    range.setStart(node, idx);
                                    range.setEnd(node, idx + {});
                                    const sel = window.getSelection();
                                    sel.removeAllRanges();
                                    sel.addRange(range);
                                    return {{ success: true, selected: '{}' }};
                                }}
                            }}
                            return {{ error: 'Text not found' }};
                        }}"#,
                    selector,
                    text,
                    text.len(),
                    text,
                    text,
                    text.len(),
                    text
                )
            } else {
                let s = start.unwrap_or(0);
                let e = end.unwrap_or(usize::MAX);
                format!(
                    r#"() => {{
                            const el = document.querySelector('{}');
                            if (!el) return {{ error: 'Element not found' }};
                            if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
                                el.focus();
                                const len = el.value.length;
                                const start = Math.min({}, len);
                                const end = Math.min({}, len);
                                el.setSelectionRange(start, end);
                                return {{ success: true, start, end, selected: el.value.substring(start, end) }};
                            }}
                            return {{ error: 'Selection by index only supported for input/textarea' }};
                        }}"#,
                    selector, s, e
                )
            };
            match page.evaluate(script.as_str()).await {
                Ok(result) => {
                    let value = result.value().cloned().unwrap_or(json!(null));
                    js_result(value).map_err(|e| format!("Failed to select: {e}"))
                }
                Err(e) => Err(format!("Failed to select: {}", e)),
            }
        }

        BrowserAction::Drag {
            selector,
            target_selector,
            delta_x,
            delta_y,
            steps,
        } => {
            let from = super::actions_navigation::element_center(page, &selector).await?;
            let to = match target_selector.as_deref() {
                Some(target) => super::actions_navigation::element_center(page, target).await?,
                None => (
                    from.0 + f64::from(delta_x.unwrap_or(100)),
                    from.1 + f64::from(delta_y.unwrap_or(0)),
                ),
            };
            drag_between(page, from, to, steps, PointerButton::Left).await
        }

        BrowserAction::DragAt {
            from_x,
            from_y,
            to_x,
            to_y,
            steps,
            button,
        } => {
            let button = PointerButton::parse(button.as_deref())?;
            drag_between(page, (from_x, from_y), (to_x, to_y), steps, button).await
        }

        BrowserAction::UploadFiles { selector, files } => {
            use chromiumoxide::cdp::browser_protocol::dom::{
                GetDocumentParams, QuerySelectorParams, SetFileInputFilesParams,
            };
            let doc = page
                .execute(GetDocumentParams::default())
                .await
                .map_err(|e| format!("Failed to get document: {}", e))?;
            let root_id = doc.result.root.node_id;
            let query = page
                .execute(QuerySelectorParams::new(root_id, &selector))
                .await
                .map_err(|e| format!("Failed to query selector: {}", e))?;
            let node_id = query.result.node_id;
            let mut params = SetFileInputFilesParams::new(files);
            params.node_id = Some(node_id);
            match page.execute(params).await {
                Ok(_) => Ok(json!({
                    "success": true,
                    "selector": selector,
                })),
                Err(e) => Err(format!("Failed to set file input files: {}", e)),
            }
        }

        BrowserAction::HandleDialog { action, text } => {
            use chromiumoxide::cdp::browser_protocol::page::HandleJavaScriptDialogParams;
            let accept = action == "accept";
            let mut params = HandleJavaScriptDialogParams::new(accept);
            params.prompt_text = text;
            match page.execute(params).await {
                Ok(_) => Ok(json!({ "success": true, "action": action })),
                Err(e) => Err(format!("Failed to handle dialog (no dialog may be open): {}", e)),
            }
        }

        BrowserAction::SetDownloadBehavior { behavior, download_path } => {
            let browser_ref =
                browser.ok_or("Download behavior requires a browser session".to_string())?;
            use chromiumoxide::cdp::browser_protocol::browser::{
                SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
            };
            let behavior_enum = match behavior.as_str() {
                "allow" => SetDownloadBehaviorBehavior::Allow,
                "deny" => SetDownloadBehaviorBehavior::Deny,
                "allowAndName" => SetDownloadBehaviorBehavior::AllowAndName,
                _ => SetDownloadBehaviorBehavior::Default,
            };
            let mut params = SetDownloadBehaviorParams::new(behavior_enum);
            params.download_path = download_path;
            match browser_ref.execute(params).await {
                Ok(_) => Ok(json!({ "success": true, "behavior": behavior })),
                Err(e) => Err(format!("Failed to set download behavior: {}", e)),
            }
        }
        _ => Err("browser: action not handled by this group".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_error_is_a_failure_not_a_success() {
        // The scripts report a missing element *inside* the value they return.
        // Answering `success: true` with that nested is a lie the caller cannot
        // see, which is what this used to do.
        let failed = js_result(json!({ "error": "Element not found" })).unwrap_err();
        assert_eq!(failed, "Element not found");
    }

    #[test]
    fn an_ordinary_result_keeps_its_shape() {
        let ok = js_result(json!({ "start": 0, "selected": "abc" })).unwrap();
        assert_eq!(ok["success"], json!(true));
        assert_eq!(ok["result"]["selected"], json!("abc"));
    }

    #[test]
    fn only_a_top_level_error_string_counts_as_a_failure() {
        // A form fill returns `errors: []` as data; treating that as a failure
        // would invert its meaning. And a non-string `error` field is not the
        // convention these scripts use.
        assert!(js_result(json!({ "errors": [] })).is_ok());
        assert!(js_result(json!({ "error": 7 })).is_ok());
    }
}
