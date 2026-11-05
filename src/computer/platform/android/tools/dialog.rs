//! Read the dialog in the way, and press one of its buttons.

use super::*;

/// Read the dialog that is blocking the screen, optionally pressing one button.
#[derive(Debug)]
pub struct AdbDialogTool {
    device: Option<String>,
}

impl Default for AdbDialogTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbDialogTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// Classes that mean "this node is (part of) a dialog".
pub(crate) fn is_dialog_class(class: &str) -> bool {
    let lowered = class.to_ascii_lowercase();
    lowered.contains("dialog") || lowered.contains("popupwindow") || lowered.contains("alert")
}

#[async_trait]
impl Tool for AdbDialogTool {
    fn name(&self) -> &str {
        "android_dialog"
    }

    fn description(&self) -> &str {
        "Read the dialog on screen — its text and its buttons — and optionally press one. Pass \
         `button` only when you know which one: it is pressed only if exactly one button matches, \
         which is what stops a dialog from being dismissed with the wrong answer. Without \
         `button` this only reads."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Read (and optionally answer) a dialog",
            serde_json::json!({
                "button": {
                    "type": "string",
                    "description": "Button text to press. Refused unless exactly one button on the dialog matches."
                },
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        args: Value,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        // A dialog on a display is a write to that display's own focus, not to
        // the user's screen; the same split as `android_input` applies here.
        let _target = super::lock_target(&device, target).await;
        let dump = match dump_ui(&device, target).await {
            Ok(dump) => dump,
            Err(failure) => {
                return Ok(ToolExecutionResult::error(failure.message)
                    .with_data(serde_json::json!({ "code": failure.code })))
            }
        };

        // A dialog is reported by its own node classes; the buttons are the
        // clickable, labelled elements sitting inside those nodes' rectangle.
        let roots: Vec<&UiElement> = dump
            .elements
            .iter()
            .filter(|element| is_dialog_class(&element.class))
            .collect();
        if roots.is_empty() {
            return Ok(ToolExecutionResult::success(format!(
                "No dialog detected — the screen holds no dialog or popup window. Here is what is \
                 on it:\n\n{}",
                format_ui_summary(&dump.elements)
            ))
            .with_data(serde_json::json!({
                "dialog": Value::Null,
                "device": resolved_serial(&device).await,
            })));
        }

        let region = roots
            .iter()
            .fold((i32::MAX, i32::MAX, i32::MIN, i32::MIN), |acc, element| {
                let (x1, y1, x2, y2) = element.bounds;
                (acc.0.min(x1), acc.1.min(y1), acc.2.max(x2), acc.3.max(y2))
            });
        let inside = |element: &UiElement| {
            let (x1, y1, x2, y2) = element.bounds;
            x1 >= region.0 && y1 >= region.1 && x2 <= region.2 && y2 <= region.3
        };

        let members: Vec<&UiElement> = dump
            .elements
            .iter()
            .filter(|element| inside(element))
            .collect();
        let buttons: Vec<&UiElement> = members
            .iter()
            .copied()
            .filter(|element| element.clickable && element_label(element).is_some())
            .collect();
        let message = members
            .iter()
            .filter(|element| !element.clickable && element_label(element).is_some())
            .filter_map(|element| element_label(element))
            .collect::<Vec<_>>()
            .join(" ");

        let summary = format!(
            "Dialog: \"{}\"\nButtons: {}",
            if message.is_empty() {
                "(no text)"
            } else {
                message.as_str()
            },
            if buttons.is_empty() {
                "(none found)".to_string()
            } else {
                buttons
                    .iter()
                    .filter_map(|element| element_label(element))
                    .collect::<Vec<_>>()
                    .join(" | ")
            }
        );

        let wanted = args
            .get("button")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let Some(wanted) = wanted else {
            return Ok(ToolExecutionResult::success(summary).with_data(serde_json::json!({
                "message": message,
                "buttons": buttons.iter().filter_map(|e| element_label(e)).collect::<Vec<_>>(),
                "device": resolved_serial(&device).await,
            })));
        };

        let needle = wanted.to_lowercase();
        let matching: Vec<&&UiElement> = buttons
            .iter()
            .filter(|element| {
                element_label(element).is_some_and(|label| label.to_lowercase().contains(&needle))
            })
            .collect();
        let chosen = match matching.as_slice() {
            [only] => *only,
            [] => {
                return Ok(ToolExecutionResult::success(format!(
                    "{summary}\n\nNo button matches {wanted:?}, so nothing was pressed."
                ))
                .with_data(serde_json::json!({
                    "pressed": false,
                    "reason": "no matching button",
                    "device": resolved_serial(&device).await,
                })))
            }
            many => {
                let candidates: Vec<&str> = many
                    .iter()
                    .filter_map(|element| element_label(element))
                    .collect();
                return Ok(ToolExecutionResult::success(format!(
                    "{summary}\n\n{wanted:?} matches {} buttons ({}), so nothing was pressed — \
                     say which one exactly.",
                    candidates.len(),
                    candidates.join(" | ")
                ))
                .with_data(serde_json::json!({
                    "pressed": false,
                    "reason": "ambiguous button",
                    "candidates": candidates,
                    "device": resolved_serial(&device).await,
                })));
            }
        };

        // A person may have locked the phone, and the task may have left the
        // display while the model was thinking. Either way the button being
        // pressed is not the button that was read.
        let readiness = super::ready::probe().await;
        if let Err(blocked) =
            super::ready::check(readiness.as_ref(), target, dump.package.as_deref())
        {
            return Ok(ToolExecutionResult::error(super::ready::describe(&blocked)));
        }

        // The dialog's own button is a fresh decision point: a confirmation
        // prompt reading "Send 3 photos?" ends in a send whatever opened it.
        let offer = chosen
            .text
            .as_deref()
            .or(chosen.content_desc.as_deref())
            .and_then(super::risk::classify_named);
        let subject = format!("press \"{wanted}\" on a dialog");
        let clearance =
            match super::gate::check(offer, context, &subject, dump.package.as_deref()).await {
                Ok(clearance) => clearance,
                Err(refusal) => return Ok(ToolExecutionResult::error(refusal.reason)),
            };

        // Pressing the same dialog button again and again is the loop the
        // ladder exists for, and it runs after the gate so that "the person
        // said yes" is not silently spent on a refusal.
        let guard_key = super::guard::Guard::bucket_key(
            &context.conversation_id,
            &format!("{}:{}", device.as_deref().unwrap_or("-"), target.display_id()),
        );
        let signature = format!("dialog:{wanted}");
        let ladder = super::guard::Guard::global().check(&guard_key, &signature);
        if let Some(reason) = ladder.refusal() {
            return Ok(ToolExecutionResult::error(if clearance.asked {
                format!("{reason} (An approval was given but unused; nothing was pressed.)")
            } else {
                reason.to_string()
            }));
        }

        // The question was open for as long as the person took to read it, and
        // a dialog is exactly the kind of screen that changes underneath one:
        // a countdown closes it, a message arrives behind it. What was approved
        // was the button that was described; before pressing, check that it is
        // still that button in that app. The re-read only happens when somebody
        // was actually asked — otherwise there is nothing to check.
        let mut fresh_dump: Option<UiDump> = None;
        if clearance.asked {
            let fresh = match super::dump_ui(&device, target).await {
                Ok(fresh) => fresh,
                Err(failed) => {
                    return Ok(ToolExecutionResult::error(format!(
                        "Refused: \"{subject}\" was approved, but the screen could not be read \
                         again to confirm it is still there ({}). Nothing was pressed.",
                        failed.message
                    )))
                }
            };
            let still_there = fresh.elements.iter().any(|element| {
                element.clickable
                    && super::element_label(element)
                        .is_some_and(|label| label.trim() == wanted.trim())
            });
            if !still_there {
                return Ok(ToolExecutionResult::error(format!(
                    "Refused: \"{subject}\" was approved, and by then the dialog no longer had \
                     that button. Nothing was pressed. Read the screen again."
                )));
            }
            let now = super::risk::classify_named(wanted);
            if let Err(withheld) =
                super::gate::verify(&clearance, now.as_ref(), fresh.package.as_deref())
            {
                return Ok(ToolExecutionResult::error(withheld.reason));
            }
            fresh_dump = Some(fresh);
        }
        let before = tree_fingerprint(&fresh_dump.as_ref().unwrap_or(&dump).elements);
        // The button's own position may have moved with its container; press
        // where it is *now*, not where it was before the question.
        let at = fresh_dump
            .as_ref()
            .and_then(|fresh| {
                fresh
                    .elements
                    .iter()
                    .find(|element| {
                        element.clickable
                            && super::element_label(element)
                                .is_some_and(|label| label.trim() == wanted.trim())
                    })
                    .map(|element| element.center)
            })
            .unwrap_or(chosen.center);
        let pressed = match target {
            Target::Screen => {
                let command = format!("input tap {} {}", at.0, at.1);
                let argv = adb_args(&device, &["shell", &command]);
                run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                    .await
                    .map(|(status, _, stderr)| (status.success(), stderr))
                    .map_err(|e| e.to_string())
            }
            Target::Display(id) => match display_bridge() {
                Ok(bridge) => super::display::VirtualDisplay::attach(bridge, id)
                    .tap(at.0, at.1)
                    .await
                    .map(|()| (true, String::new()))
                    .map_err(|e| e.to_string()),
                Err(message) => Err(message),
            },
        };
        match pressed {
            Ok((true, _)) => {}
            Ok((false, stderr)) => {
                return Ok(ToolExecutionResult::error(format!(
                    "pressing {wanted:?} failed: {}",
                    stderr.trim()
                )))
            }
            Err(message) => {
                return Ok(ToolExecutionResult::error(format!(
                    "pressing {wanted:?} failed: {message}"
                )))
            }
        }
        let settle = settle_after_action(&device, target, Some(before), 1200).await;

        // The ladder learns what the press did, judged by the screen rather
        // than the dispatch: a dialog that reappears is the same screen, and
        // that is the stuck state this counts. The screen it left is the
        // frame, so a dialog that keeps coming back is a cycle and not just a
        // repeated press.
        super::guard::Guard::global().record_with(
            &guard_key,
            &signature,
            super::guard_outcome(settle.effect),
            Some(tree_fingerprint(&settle.elements)),
        );

        // A dialog button that reaches outward counts toward the pace exactly
        // as an `android_input` tap would — otherwise the two paths would be
        // two different speeds through the same app.
        let pace_note = if clearance.reaches_outward() && context.mobile_control().pace_outward {
            let package = dump.package.as_deref();
            Some(super::note_outward(package).await)
        } else {
            None
        };
        let mut text = format!("Pressed {wanted:?}. {}", settle.verdict());
        if let Some(note) = &pace_note {
            text.push_str(&format!(" {note}"));
        }
        if let Some(note) = ladder.note() {
            text.push(' ');
            text.push_str(note);
        }

        Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
            "pressed": true,
            "effect": settle.effect,
            "elements": settle.elements,
            "device": resolved_serial(&device).await,
            "risk": clearance.note(),
            "risk_approved": clearance.asked,
            "paced": pace_note,
            "guard": ladder.note(),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_classes_are_recognised() {
        assert!(is_dialog_class("android.app.AlertDialog"));
        assert!(is_dialog_class("com.x.DialogTitle"));
        assert!(!is_dialog_class("android.widget.Button"));
    }
}
