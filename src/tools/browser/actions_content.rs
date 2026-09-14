//! Content extraction and script/input actions.

use super::{BrowserAction, BrowserScreenshot};
use serde_json::{json, Value};

use crate::browser::escalation;
use crate::browser::keys;

pub(super) async fn execute_content_actions(
    action: BrowserAction,
    page: &chromiumoxide::Page,
    _browser: Option<&chromiumoxide::Browser>,
    _screenshot_data: &mut Option<BrowserScreenshot>,
) -> Result<serde_json::Value, String> {
    match action {
        BrowserAction::GetHtml => match page.content().await {
            Ok(html) => Ok(json!({
                "success": true,
                "html": html,
                "length": html.len()
            })),
            Err(e) => Err(format!("Failed to get HTML: {}", e)),
        },

        BrowserAction::GetText { selector } => match selector {
            Some(sel) => match page.find_element(&sel).await {
                Ok(elem) => match elem.inner_text().await {
                    Ok(Some(text)) => Ok(json!({
                        "success": true,
                        "text": text,
                        "selector": sel
                    })),
                    Ok(None) => Ok(json!({
                        "success": true,
                        "text": "",
                        "selector": sel
                    })),
                    Err(e) => Err(format!("Failed to get text: {}", e)),
                },
                Err(e) => Err(format!("Element not found: {}", e)),
            },
            None => {
                let script = r#"() => document.body.innerText"#;
                match page.evaluate(script).await {
                    Ok(result) => {
                        let text = result.into_value::<String>().unwrap_or_default();
                        Ok(json!({
                            "success": true,
                            "text": text
                        }))
                    }
                    Err(e) => Err(format!("Failed to get page text: {}", e)),
                }
            }
        },

        BrowserAction::Snapshot { max_chars } => {
            let max = max_chars.unwrap_or(8000);
            match crate::browser::aria_snapshot(page, max).await {
                Ok(snapshot) => {
                    let text = snapshot.to_text();
                    Ok(json!({
                        "success": true,
                        "snapshot": text,
                        "url": snapshot.url,
                        "title": snapshot.title,
                        "interactive_count": snapshot.interactive_count(),
                        "truncated": snapshot.truncated
                    }))
                }
                Err(e) => Err(format!("Failed to take ARIA snapshot: {}", e)),
            }
        }

        BrowserAction::GetPerformanceMetrics => {
            let script = r#"() => {
                    const nav = performance.getEntriesByType('navigation')[0] || {};
                    return {
                        navigation: {
                            dns_lookup: nav.domainLookupEnd - nav.domainLookupStart,
                            connection_time: nav.connectEnd - nav.connectStart,
                            response_time: nav.responseEnd - nav.responseStart,
                            dom_interactive: nav.domInteractive,
                            dom_complete: nav.domComplete,
                            load_event: nav.loadEventEnd - nav.loadEventStart,
                            transfer_size: nav.transferSize,
                            decoded_body_size: nav.decodedBodySize
                        },
                        memory: performance.memory ? {
                            used_js_heap_size: performance.memory.usedJSHeapSize,
                            total_js_heap_size: performance.memory.totalJSHeapSize,
                            js_heap_size_limit: performance.memory.jsHeapSizeLimit
                        } : null
                    };
                }"#;
            match page.evaluate(script).await {
                Ok(result) => {
                    let metrics = result.value().cloned().unwrap_or(json!(null));
                    Ok(json!({ "success": true, "metrics": metrics }))
                }
                Err(e) => Err(format!("Failed to get performance metrics: {}", e)),
            }
        }

        BrowserAction::GetConsoleMessages { level, limit } => {
            crate::browser::instrument::ensure_instrumented(page).await?;
            let script = r#"() => (window.__syscity_console || [])"#;
            match page.evaluate(script).await {
                Ok(result) => {
                    let entries = result.into_value::<Vec<Value>>().unwrap_or_default();
                    let filtered: Vec<Value> = entries
                        .into_iter()
                        .filter(|e| {
                            level.as_ref().is_none_or(|l| {
                                e.get("level")
                                    .and_then(|v| v.as_str())
                                    .is_some_and(|v| v.eq_ignore_ascii_case(l))
                            })
                        })
                        .collect();
                    let total = filtered.len();
                    let mut messages: Vec<Value> =
                        filtered.into_iter().take(limit.unwrap_or(100)).collect();
                    // Best-effort: resolve error/warn stacks via source maps.
                    crate::browser::sourcemap::sourcemap_messages(&mut messages).await;
                    Ok(json!({
                        "success": true,
                        "messages": messages,
                        "count": messages.len(),
                        "total": total
                    }))
                }
                Err(e) => Err(format!("Failed to get console messages: {}", e)),
            }
        }

        BrowserAction::ExecuteScript { script } => {
            match page
                .evaluate(format!("() => {{ {} }}", script).as_str())
                .await
            {
                Ok(result) => {
                    let value = result.value().cloned().unwrap_or(json!(null));
                    Ok(json!({
                        "success": true,
                        "result": value
                    }))
                }
                Err(e) => Err(format!("Script execution failed: {}", e)),
            }
        }

        BrowserAction::Press { key } => {
            let steps = match keys::press_steps(&key) {
                Ok(steps) => steps,
                Err(refusal) => return Err(refusal),
            };
            dispatch_key_steps(page, &steps).await?;
            crate::browser::instrument::auto_wait(page).await;

            let spec = &steps[0].spec;
            Ok(json!({
                "success": true,
                "delivered": "cdp_key_event",
                "key": spec.key,
                "code": spec.code,
                "typed": spec.text,
            }))
        }

        BrowserAction::Hotkey { keys: combination } => {
            let steps = match keys::hotkey_steps(&combination) {
                Ok(steps) => steps,
                Err(refusal) => return Err(refusal),
            };
            dispatch_key_steps(page, &steps).await?;
            crate::browser::instrument::auto_wait(page).await;

            // The last press is the key being modified; everything before it is
            // a modifier held down.
            let main = steps
                .iter()
                .find(|step| step.kind != keys::KeyStepKind::Up)
                .map(|step| step.spec.key.clone())
                .unwrap_or_default();
            let modifiers: Vec<&str> = steps
                .iter()
                .filter(|step| step.kind == keys::KeyStepKind::DownRaw)
                .filter(|step| step.spec.key != main)
                .map(|step| step.spec.key.as_str())
                .collect();

            Ok(json!({
                "success": true,
                "delivered": "cdp_key_event",
                "key": main,
                "modifiers": modifiers,
                "typed": serde_json::Value::Null,
            }))
        }

        BrowserAction::Act { ref_id, action } => {
            match crate::browser::act_by_ref(page, ref_id, action).await {
                Ok(msg) => {
                    crate::browser::instrument::auto_wait(page).await;
                    Ok(json!({ "success": true, "message": msg }))
                }
                Err(e) => Err(format!("Failed to act on ref {}: {}", ref_id, e)),
            }
        }

        BrowserAction::Escalate { reason, detail } => {
            escalate(page, &reason, detail.as_deref()).await
        }

        _ => Err("browser: action not handled by this group".to_string()),
    }
}

/// Report that the page cannot be finished from inside the page.
///
/// Deliberately does nothing else. The caller gets what it needs to escalate —
/// which window would have to be operated, what this tool cannot reach, the
/// in-page route worth trying first, and the fact that consent comes first
/// because the desktop is shared.

/// Send a key sequence, one CDP event per step.
///
/// The steps are built by `browser::keys`; this is only the mapping onto CDP,
/// so that a single press and a combination differ in how their steps are
/// computed rather than in how they are sent.
async fn dispatch_key_steps(
    page: &chromiumoxide::Page,
    steps: &[keys::KeyStep],
) -> Result<(), String> {
    use chromiumoxide::cdp::browser_protocol::input::{
        DispatchKeyEventParams, DispatchKeyEventType,
    };

    for step in steps {
        let kind = match step.kind {
            keys::KeyStepKind::Down => DispatchKeyEventType::KeyDown,
            keys::KeyStepKind::DownRaw => DispatchKeyEventType::RawKeyDown,
            keys::KeyStepKind::Up => DispatchKeyEventType::KeyUp,
        };
        let mut params = DispatchKeyEventParams::new(kind);
        params.key = Some(step.spec.key.clone());
        params.code = Some(step.spec.code.clone());
        params.windows_virtual_key_code = Some(step.spec.vk);
        // Which modifiers are held during this event — the field that makes
        // Control+A a combination rather than two keystrokes in a row.
        params.modifiers = Some(step.modifiers);
        // macOS runs its editing shortcuts in the browser process rather than
        // from the DOM event, so the command has to travel with the key: without
        // it Cmd+A arrives, matches nothing, and selects nothing. Not sent on a
        // key-up, where the command has already run.
        if cfg!(target_os = "macos") && step.kind != keys::KeyStepKind::Up {
            if let Some(command) = keys::editing_command(step.modifiers, &step.spec.code) {
                params.commands = Some(vec![command.to_string()]);
            }
        }
        if step.kind == keys::KeyStepKind::Down {
            params.text = step.spec.text.clone();
            params.unmodified_text = step.spec.text.clone();
        }
        page.execute(params)
            .await
            .map_err(|e| format!("Failed to send {}: {}", step.spec.key, e))?;
    }
    Ok(())
}

async fn escalate(
    page: &chromiumoxide::Page,
    reason: &str,
    detail: Option<&str>,
) -> Result<serde_json::Value, String> {
    let reason = escalation::Reason::parse(reason)?;

    // The identity comes from the driver rather than from the caller's
    // recollection: it is the one fact here that has to be right, because it is
    // what the desktop side would match a window against.
    let title = page.get_title().await.ok().flatten().unwrap_or_default();
    let url = page.url().await.ok().flatten().unwrap_or_default();

    let subject = if title.is_empty() {
        "this browser window".to_string()
    } else {
        format!("the window titled \"{title}\"")
    };

    let mut escalation = serde_json::Map::new();
    escalation.insert("code".to_string(), json!("needs_os_injection"));
    escalation.insert("reason".to_string(), json!(reason.name()));
    escalation.insert("cannot_reach".to_string(), json!(reason.cannot_reach()));
    escalation.insert("browser_window".to_string(), json!({ "title": title, "url": url }));
    if let Some(detail) = detail {
        escalation.insert("detail".to_string(), json!(detail));
    }
    if let Some(instead) = reason.instead_try() {
        escalation.insert("instead_try_first".to_string(), json!(instead));
    }
    escalation.insert(
        "consent_required".to_string(),
        json!(format!(
            "Reaching {subject} means input at the operating-system level: the pointer moves and \
             keystrokes go to whatever holds focus on the desktop, which someone may be using. Put \
             that to the user and wait for an answer before going further."
        )),
    );
    escalation.insert(
        "if_the_user_agrees".to_string(),
        json!([
            "Use the `computer` tool with `activate_window` and a distinctive substring of that \
             title — window titles are matched as a pattern, so a plain substring is the safest \
             choice.",
            "Then screenshot, click, key or type against that window. The pointer and the focus \
             move: this is not a background action.",
            "Come back to this tool afterwards. The page can verify what happened (Snapshot, \
             GetText), which a desktop action on its own cannot."
        ]),
    );

    Ok(json!({ "success": true, "escalation": escalation }))
}
