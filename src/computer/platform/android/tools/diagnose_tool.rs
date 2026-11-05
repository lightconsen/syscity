//! Why the screen looks the way it does.

use super::*;

/// Answer "why is there nothing here" before the model starts guessing.
///
/// Every branch of [`super::diagnose::classify`] already exists somewhere in
/// this module, but only as a side effect of an action: the readiness check
/// fires before a write, the tree verdict fires on a read, the black frame only
/// appears in a pixel comparison. A model that tapped and saw nothing had no
/// way to ask what kind of screen it was looking at, so it read again, or
/// tapped again.
#[derive(Debug)]
pub struct AdbDiagnoseTool {
    device: Option<String>,
}

impl Default for AdbDiagnoseTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbDiagnoseTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// How long to wait between two reads when asking whether the screen is still
/// drawing.
const DIAGNOSE_CHANGING_MS: u64 = 450;

/// How many element labels are scanned for the needs-a-person vocabulary.
///
/// A screen with more text than this has long since been describable, so the
/// vocabulary has nothing left to find in it.
const DIAGNOSE_TEXT_LIMIT: usize = 400;

#[async_trait]
impl Tool for AdbDiagnoseTool {
    fn name(&self) -> &str {
        "android_diagnose"
    }

    fn description(&self) -> &str {
        "Say what kind of screen this is, when reading it did not explain it. Answers one of \
         `normal`, `blank`, `loading`, `protected` or `needs_human`, with `usable`, `needsHuman`, \
         the `cause` it rested on and a `hint` for the next move. `needs_human` covers a captcha, \
         a payment sheet, a password or SMS-code screen and a locked phone — each comes with the \
         `takeoverReason` to pass to android_hand_to_user. `protected` means the window is secure \
         and its frame stays black, so no screenshot will help. Call it after an action that \
         seemed to do nothing, before repeating that action."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Diagnose the current screen",
            serde_json::json!({
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            Vec::<String>::new(),
        )
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let _target = super::lock_device(&device).await;

        let readiness = super::ready::probe().await;
        let locked = readiness.as_ref().is_some_and(|state| state.locked);

        let mut facts = super::diagnose::Facts { locked, ..Default::default() };
        let mut dump_note: Option<String> = None;
        let mut quality: Option<TreeQuality> = None;
        match dump_ui(&device, target).await {
            Ok(dump) => {
                let quality_now = assess_tree(&dump.elements);
                quality = Some(quality_now);
                facts.current_app = dump.package.clone();
                facts.has_window = !dump.elements.is_empty();
                facts.readable_elements = dump
                    .elements
                    .iter()
                    .filter(|element| element_label(element).is_some())
                    .count();
                facts.password_field = dump.elements.iter().any(|e| e.state.password);
                facts.texts = dump
                    .elements
                    .iter()
                    .filter_map(element_label)
                    .take(DIAGNOSE_TEXT_LIMIT)
                    .map(str::to_string)
                    .collect();
                // A screen the tree describes nothing on is the one case where a
                // second read is worth paying for: "still changing" and "nothing
                // there" have different next moves.
                if quality_now == TreeQuality::Unusable {
                    let before = tree_fingerprint(&dump.elements);
                    tokio::time::sleep(Duration::from_millis(DIAGNOSE_CHANGING_MS)).await;
                    if let Ok(again) = dump_ui(&device, target).await {
                        facts.changing = tree_fingerprint(&again.elements) != before
                            || again.package != dump.package;
                        facts.current_app = again.package.or(facts.current_app);
                        if facts.readable_elements == 0 && !again.elements.is_empty() {
                            facts.has_window = true;
                            facts.readable_elements = again
                                .elements
                                .iter()
                                .filter(|element| element_label(element).is_some())
                                .count();
                            facts.password_field = again.elements.iter().any(|e| e.state.password);
                            facts.texts = again
                                .elements
                                .iter()
                                .filter_map(element_label)
                                .take(DIAGNOSE_TEXT_LIMIT)
                                .map(str::to_string)
                                .collect();
                        }
                    }
                }
            }
            Err(failure) => {
                // No tree is itself a fact: it is a blank or unreadable window,
                // not an unknown screen.
                dump_note = Some(failure.message.clone());
            }
        }

        // OCR is the second opinion, and only when the tree could not give one
        // — the same rule `android_collect` and `android_observe` follow.
        let mut ocr_note: Option<String> = None;
        if facts.texts.is_empty() || quality == Some(TreeQuality::Unusable) {
            match ocr_screen(&device, target).await {
                Ok(lines) if !lines.is_empty() => {
                    facts.texts.extend(
                        lines
                            .iter()
                            .take(DIAGNOSE_TEXT_LIMIT)
                            .map(|line| line.text.clone()),
                    );
                }
                Ok(_) => {}
                Err(message) => ocr_note = Some(message),
            }
        }

        // A frame, and a second one only if the first was black: one black
        // frame is a page that has not painted yet, two quiet ones are a secure
        // window. Paying for a second screen-size frame on every healthy
        // diagnosis would make this tool too expensive to call.
        if let Some(frame) = super::display_frame(target).await {
            let first_black = super::diagnose::is_black(&frame);
            facts.black_frames = vec![first_black];
            if first_black {
                tokio::time::sleep(Duration::from_millis(DIAGNOSE_CHANGING_MS)).await;
                if let Some(again) = super::display_frame(target).await {
                    facts.black_frames.push(super::diagnose::is_black(&again));
                }
            }
        }

        let verdict = super::diagnose::classify(&facts);
        let mut text = super::diagnose::describe(&verdict);
        if let Some(note) = &dump_note {
            text.push_str(&format!("\nThe tree could not be read: {note}"));
        } else if let Some(quality) = quality {
            if let Some(advice) = quality.advice() {
                text.push_str(&format!("\n{}", advice));
            }
        }
        if let Some(note) = &ocr_note {
            text.push_str(&format!("\nOCR did not answer: {note}"));
        }

        Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
            "state": verdict.state.as_str(),
            "usable": verdict.state.usable(),
            "needs_human": verdict.needs_human,
            "needsHuman": verdict.needs_human,
            "current_app": verdict.current_app,
            "takeover_reason": verdict.takeover_reason,
            "takeoverReason": verdict.takeover_reason,
            "cause": verdict.cause,
            "hint": verdict.hint,
            "tree": quality.map(TreeQuality::as_str),
            "elements": facts.readable_elements,
            "changing": facts.changing,
            "black_frames": facts.black_frames,
            "device": resolved_serial(&device).await,
        })))
    }
}
