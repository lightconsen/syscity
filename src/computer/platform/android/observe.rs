//! Capturing the screen, and reading it once into both a tree and a picture.
use async_trait::async_trait;
use serde_json::Value;

// The set, the device targeting, and the siblings' items: this file used
// to be part of `android`, and names them exactly as it did there.
use super::*;

/// Capture device screen via `adb exec-out screencap -p`.
#[derive(Debug)]
pub struct AdbScreenshotTool {
    device: Option<String>,
}

impl Default for AdbScreenshotTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbScreenshotTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbScreenshotTool {
    fn name(&self) -> &str {
        "android_screenshot"
    }

    fn description(&self) -> &str {
        "Capture a screenshot of the connected Android device and return it as base64 PNG."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Capture Android device screenshot",
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
        // Resolved before the lock, because the lock is the device: a call that
        // names a serial does not wait for one that names another.
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let _target = lock_device(&device).await;
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        let png = match capture_png(&device, target).await {
            Ok(png) => png,
            Err(e) => return Ok(ToolExecutionResult::error(e.to_string())),
        };
        let base64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);

        Ok(
            ToolExecutionResult::success("Screenshot captured").with_data(serde_json::json!({
                "base64": base64,
                "format": "png",
                "device": resolved_serial(&device).await,
            })),
        )
    }
}

/// What an action *is*, for the repetition guard.
///
/// Not the arguments: a tap by index and the same tap by key are the same
/// action, and a model that alternates between the two would otherwise reset
/// the count while doing exactly the same thing. The signature names the
/// element where one is known, so scrolling the same list and tapping the same
/// row is still "the same action", and separately names the operation where it
/// is not.
pub(super) fn action_signature(op: &InputOp, before: Option<&[UiElement]>) -> String {
    match op {
        InputOp::Taps(points) if points.len() == 1 => {
            let (x, y) = points[0];
            let label = before
                .and_then(|elements| {
                    elements
                        .iter()
                        .filter(|element| {
                            let (x1, y1, x2, y2) = element.bounds;
                            x >= x1 && y >= y1 && x <= x2 && y <= y2
                        })
                        .min_by_key(|element| {
                            let (x1, y1, x2, y2) = element.bounds;
                            (x2 - x1) as i64 * (y2 - y1) as i64
                        })
                })
                .and_then(element_label)
                .unwrap_or("");
            if label.is_empty() {
                format!("tap@{x},{y}")
            } else {
                format!("tap:{label}")
            }
        }
        // A sequence is several taps in one call; what it *is* is the gesture
        // as a whole, so two identical sequences are the same repeat.
        InputOp::Taps(points) => format!("sequence@{}", points.len()),
        // Both ends: a short nudge and a long drag are different actions, and
        // calling them the same repeat would refuse a gesture that had not yet
        // been tried.
        InputOp::Swipe { from, to } => {
            format!("swipe@{}:{}>{}:{}", from.0, from.1, to.0, to.1)
        }
        InputOp::Text(text) => format!("text:{}", text.chars().take(40).collect::<String>()),
        InputOp::Key(keycap) => format!("key:{keycap}"),
        InputOp::Gesture { paths, .. } => format!("gesture@{}", paths.len()),
    }
}

/// What an input is about to do, before it happens.
///
/// One place, because every kind of input asks the same question and the
/// answer has to be the same before dispatch and after an approval. A swipe
/// and a multi-finger gesture move the screen without pressing anything, and
/// have no answer.
///
/// - a single tap is judged by the element under it, or by the label a located
///   control carried when the tree could not name it;
/// - a tap *sequence* is judged by its strongest step — a chain whose first
///   tap pays is a payment however harmless the rest looks;
/// - typed text is judged only by the line break in it, because that is the
///   enter `input text` emits, and in a chat field it is the send;
/// - a key press is judged by the control it activates, if it activates one.
pub(super) fn classify_input(
    op: &InputOp,
    elements: &[UiElement],
    args: &Value,
    visual: Option<&super::visual::VisualTarget>,
) -> Option<super::risk::Offer> {
    match op {
        InputOp::Taps(points) if points.len() == 1 => match visual {
            // The label the location carried: a name read off the screen,
            // whether by the tree, by OCR or by the model.
            Some(located) => super::risk::classify_named(&located.label),
            None => match args.get("target").and_then(|v| v.as_u64()) {
                Some(index) => super::risk::classify_tap(elements, usize::try_from(index).ok()),
                None => super::risk::classify_point(elements, points[0].0, points[0].1),
            },
        },
        // Raw coordinates with no element to name each one by. Every step that
        // lands on something the tree knows is classified, and the strongest
        // hit answers for the chain.
        InputOp::Taps(points) => super::risk::classify_sequence(elements, points),
        InputOp::Text(text) => super::risk::classify_pressed_text(elements, text),
        InputOp::Key(keycode) => super::risk::classify_key(elements, keycode),
        InputOp::Swipe { .. } | InputOp::Gesture { .. } => None,
    }
}

/// How the approval question names the action.
///
/// The subject has to say what a person is approving in terms they recognise:
/// which control, or which field the enter lands in, and in which app.
pub(super) fn subject_for(
    op: &InputOp,
    offer: Option<&super::risk::Offer>,
    visual: Option<&super::visual::VisualTarget>,
    package: Option<&str>,
) -> String {
    let point = match op {
        InputOp::Taps(points) if points.len() == 1 => Some(points[0]),
        _ => None,
    };
    let subject = match op {
        InputOp::Taps(_) => offer
            .and_then(|o| o.element.clone())
            .or_else(|| {
                // A located control with no readable label — an icon — is
                // still something the question has to name, or a person is
                // being asked to approve "tap (540, 1200)".
                visual.map(|located| {
                    format!("tap the control at ({}, {})", located.center.0, located.center.1)
                })
            })
            .map(|label| format!("tap \"{label}\""))
            .unwrap_or_else(|| match point {
                Some((x, y)) => format!("tap ({x}, {y})"),
                None => "tap a sequence of points".to_string(),
            }),
        InputOp::Text(_) => match offer.and_then(|o| o.element.as_deref()) {
            Some(field) if !field.is_empty() => {
                format!("type text into \"{field}\" and press enter")
            }
            _ => "type text and press enter".to_string(),
        },
        InputOp::Key(keycode) => format!("press {keycode}"),
        InputOp::Swipe { .. } => "swipe".to_string(),
        InputOp::Gesture { .. } => "send a multi-finger gesture".to_string(),
    };
    match package {
        Some(package) => format!("{subject} in {package}"),
        None => subject,
    }
}

// ── Touch tracking ─────────────────────────────────────────────────────────

/// One change to the screen, as the post-action check saw it.
///
/// The effect vocabulary is the shared one in
/// [`crate::computer::platform::screen`]; this maps it onto the ladder's
/// coarser three. A word the shared layer does not know is not a verdict, so it
/// counts as [`crate::computer::platform::screen::Effect::Unknown`] rather than
/// as progress.
pub(super) fn guard_outcome(effect: &str) -> super::guard::Outcome {
    match crate::computer::platform::screen::Effect::parse(effect) {
        Some(crate::computer::platform::screen::Effect::Changed) => super::guard::Outcome::Changed,
        Some(crate::computer::platform::screen::Effect::Unchanged) => {
            super::guard::Outcome::Unchanged
        }
        _ => super::guard::Outcome::Unknown,
    }
}

/// What the model is told when it is on a lower rung of the ladder.
pub(super) fn ladder_note(verdict: &super::guard::Verdict) -> Option<String> {
    verdict.note().map(str::to_string)
}

/// The guard bucket a read tool is counted in.
///
/// The same key shape the write tools use, so a read and a tap on one phone
/// are counted together: reading a screen the task is stuck on is part of the
/// same loop, and a separate bucket would let it run forever.
pub(super) fn guard_key_for(context: &ToolContext, device: Option<&str>, target: Target) -> String {
    super::guard::Guard::bucket_key(
        &context.conversation_id,
        &format!("{}:{}", device.unwrap_or("-"), target.display_id()),
    )
}

/// Record a read of the screen and return the note to attach to its result.
///
/// `content` is the screen's fingerprint: the same one twice means the screen
/// has not moved under the reader, which is the signal that reading it again
/// cannot answer a new question. An empty read has no fingerprint — one that
/// saw nothing is not a read of an unchanged screen.
pub(super) fn read_note(key: &str, tool: &str, elements: &[UiElement]) -> Option<String> {
    let content = (!elements.is_empty()).then(|| tree_fingerprint(elements));
    super::guard::Guard::global().record_read(key, tool, content)
}

/// [`read_note`] for a read that came back as OCR lines rather than a tree.
pub(super) fn read_note_ocr(key: &str, tool: &str, lines: &[OcrLine]) -> Option<String> {
    let content = (!lines.is_empty()).then(|| ocr_fingerprint(lines));
    super::guard::Guard::global().record_read(key, tool, content)
}

/// A fingerprint of what OCR read, so "the same screen" can be recognised
/// without the tree.
///
/// Only the text is hashed, not the coordinates: the same words in the same
/// order are the same screen to a reader, and a one-pixel shift in where a line
/// was boxed is not a change worth spending a turn on.
pub(super) fn ocr_fingerprint(lines: &[OcrLine]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for line in lines {
        std::hash::Hash::hash(&line.text.trim(), &mut hasher);
    }
    std::hash::Hasher::finish(&hasher)
}

/// A result's text with the ladder's note appended, when there is one.
pub(super) fn with_note(text: String, note: Option<String>) -> String {
    match note {
        Some(note) => format!("{text} {note}"),
        None => text,
    }
}

/// Record a failed action and return the note to append to its error.
///
/// `recoverable` is whether retrying the same call could work: a dispatch the
/// shell refused or a temporary ADB fault can, a missing capability cannot. The
/// ladder turns a run of these into a note, then a refusal on the *next* call —
/// a model that keeps re-issuing a call that keeps failing is the one loop with
/// no screen to see it in.
///
/// The note is attached to the error rather than only to the next result: the
/// model is looking at this failure now, which is the moment the ladder has
/// something to say. The refusal is left to `check`, where it can stop the call
/// before it runs.
pub(super) fn note_failure(key: &str, signature: &str, recoverable: bool) -> Option<String> {
    let ladder = super::guard::Guard::global();
    ladder.record_failure(key, recoverable);
    ladder.check(key, signature).note().map(str::to_string)
}

/// Record that an action worked, ending any failure streak on this target.
pub(super) fn note_success(key: &str) {
    super::guard::Guard::global().record_success(key);
}

// ── ADB Observe Tool ───────────────────────────────────────────────────────

/// Screenshot **and** UI tree from one call, with the coordinate space made
/// explicit.
///
/// Two separate adb reads cannot be simultaneous, but doing them back to back in
/// one call shrinks the window — and, more usefully, the result states what was
/// actually captured. If the screenshot's real pixel size and the rectangle the
/// element tree describes disagree (a rotation, a scaled capture), then every
/// coordinate in the tree is wrong for the image the model is looking at, and it
/// is told so instead of discovering it by tapping the wrong place.
#[derive(Debug)]
pub struct AdbObserveTool {
    device: Option<String>,
}

impl Default for AdbObserveTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbObserveTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbObserveTool {
    fn name(&self) -> &str {
        "android_observe"
    }

    fn description(&self) -> &str {
        "Look at the Android device: returns the numbered actionable elements and a screenshot \
         together, so the elements and the image describe the same moment. Preferred over \
         calling android_ui_tree and android_screenshot separately. Says so when the two \
         disagree about the screen size, which would make the element coordinates unusable. When \
         the UI tree cannot be read at all it still returns the screenshot and says why: in that \
         case the elements are absent from the result rather than empty, because an empty list \
         means the tree was read and held nothing actionable. Pass `since` with the snapshot of \
         an earlier read to get only what changed instead of the whole list; the screenshot is \
         still returned, because a canvas can change while the tree stays silent."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Observe the Android device",
            serde_json::json!({
                "since": { "type": "string", "description": SINCE_PARAM },
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
        // Resolved before the lock, because the lock is the device: a call that
        // names a serial does not wait for one that names another.
        let device = match resolve_device(&args, self.device.as_deref()) {
            Ok(device) => device,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let _target = lock_device(&device).await;
        let target = match Target::from_args(&args) {
            Ok(target) => target,
            Err(message) => return Ok(ToolExecutionResult::error(message)),
        };
        // Refused before the capture: at this point the answer is one the model
        // already has, and a screenshot is the most expensive read there is.
        let guard_key = guard_key_for(context, device.as_deref(), target);
        if let Some(reason) = super::guard::Guard::global()
            .check_read(&guard_key, "android_observe")
            .refusal()
        {
            return Ok(ToolExecutionResult::error(reason.to_string()));
        }
        let since = super::diff::since_arg(&args).map(str::to_string);
        let png = match capture_png(&device, target).await {
            Ok(png) => png,
            Err(e) => return Ok(ToolExecutionResult::error(e.to_string())),
        };
        let as_size = |dims: Option<(u32, u32)>| {
            dims.map(|(w, h)| serde_json::json!({ "width": w, "height": h }))
        };

        let dump = match dump_ui(&device, target).await {
            Ok(dump) => dump,
            // A tree that cannot be read does not fail this call. The screenshot
            // is already in hand and is most of what the tool is for, and every
            // message about a failed dump tells the model to work from the
            // screenshot that android_observe returns — which an error result
            // would throw away, since its payload does not reach the model. So
            // the observation comes back with the image, no elements, and the
            // failure's code for a caller to branch on.
            Err(failure) => {
                let shot = png_dimensions(&png);
                let size = shot
                    .map(|(w, h)| format!("{w}x{h}"))
                    .unwrap_or_else(|| "unknown size".to_string());

                let mut data = serde_json::Map::new();
                data.insert("tree_available".to_string(), serde_json::json!(false));
                data.insert("code".to_string(), serde_json::json!(failure.code));
                data.insert(
                    "screenshot".to_string(),
                    as_size(shot).unwrap_or(serde_json::Value::Null),
                );
                data.insert(
                    "screenshot_base64".to_string(),
                    serde_json::json!(base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        &png,
                    )),
                );
                data.insert(
                    "device".to_string(),
                    serde_json::json!(resolved_serial(&device).await),
                );

                return Ok(ToolExecutionResult::success(format!(
                    "Screenshot captured ({size}), but the UI tree could not be read, so there are \
                     no elements in this result — absent, not empty. {}",
                    failure.message
                ))
                .with_data(serde_json::Value::Object(data)));
            }
        };

        // A delta replaces the element list, not the screenshot: the picture is
        // how a caller learns that a canvas changed while the tree stayed
        // silent, so a delta answer carries the image too. A *fallback* is left
        // to the normal path below, which runs the quality check and OCR — the
        // stricter read, and the one that would otherwise be skipped.
        let mut fallback = None;
        let since_delta = match since_answer(device.as_deref(), target, since.as_deref(), &dump) {
            Some(SinceAnswer::Delta { text, data }) => Some((text, data)),
            Some(SinceAnswer::Full(reason)) => {
                fallback = Some(reason);
                None
            }
            None => None,
        };
        let shot = png_dimensions(&png);
        if let Some((text, mut data)) = since_delta {
            let quality = assess_tree(&dump.elements);
            data["quality"] = serde_json::json!(quality.as_str());
            data["screenshot_base64"] = serde_json::json!(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &png,
            ));
            data["screenshot"] = as_size(shot).unwrap_or(serde_json::Value::Null);
            data["screen"] = serde_json::json!(dump
                .screen
                .map(|(w, h)| serde_json::json!({ "width": w, "height": h })));
            data["coordinates_consistent"] =
                serde_json::json!(coordinate_space_consistent(dump.screen, shot));
            data["source"] = serde_json::json!(dump.source);
            data["device"] = serde_json::json!(resolved_serial(&device).await);
            let note = read_note(&guard_key, "android_observe", &dump.elements);
            return Ok(ToolExecutionResult::success(with_note(text, note)).with_data(data));
        }
        let consistent = coordinate_space_consistent(dump.screen, shot);
        let note = read_note(&guard_key, "android_observe", &dump.elements);

        let quality = assess_tree(&dump.elements);
        let mut summary = format_ui_summary(&dump.elements);
        let mut ocr_text = None;
        // The screenshot is already in hand, so reading it costs no extra
        // capture — only the OCR pass. Do it when the tree describes little or
        // nothing, and hand back the text as tappable lines.
        if quality != TreeQuality::Usable {
            match ocr_png(&png).await {
                Ok(lines) => {
                    summary.push_str("\n\n");
                    summary.push_str(&format_ocr_lines(&lines));
                    ocr_text = Some(lines);
                }
                Err(why) => {
                    if let Some(advice) = quality.advice() {
                        summary.push_str(&format!("\n\n{advice} (OCR unavailable: {why})"));
                    }
                }
            }
        }
        if !consistent {
            let (sw, sh) = dump.screen.unwrap_or((0, 0));
            let (iw, ih) = shot.unwrap_or((0, 0));
            summary.push_str(&format!(
                "\n\nWARNING: the screenshot ({iw}x{ih}) and the UI tree ({sw}x{sh}) do not describe \
                 the same screen — it most likely rotated or was scaled between the two reads. \
                 The element coordinates above may not match the image: observe again before \
                 tapping."
            ));
        }

        let mut data = serde_json::json!({
            "count": dump.elements.len(),
            "quality": quality.as_str(),
            "advice": quality.advice(),
            "ocr": ocr_text,
            "elements": dump.elements,
            "screenshot_base64": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &png,
            ),
            "source": dump.source,
            "snapshot": dump.snapshot,
            "screenshot": as_size(shot),
            "screen": dump.screen.map(|(w, h)| serde_json::json!({ "width": w, "height": h })),
            "coordinates_consistent": consistent,
            "device": resolved_serial(&device).await,
        });
        if let Some(reason) = fallback {
            let note = since_fallback(reason);
            data["baseline"] = note["baseline"].clone();
            data["fallback"] = note["fallback"].clone();
            summary.push_str(&format!("\n\n{}", reason.describe()));
        }

        // What this phone already knows about the app being read, when the
        // read supports it. Said after the elements, so the screen the model
        // is looking at stays the answer and the remembered facts read as a
        // prior. A read whose package is unknown gets nothing: a hint about
        // the wrong app is worse than no hint.
        if let Some(package) = dump.package.as_deref() {
            let knowledge = super::knowledge::global()
                .for_screen(package, &dump.elements)
                .await;
            if let Some(note) = super::knowledge::describe(&knowledge) {
                summary.push_str("\n\n");
                summary.push_str(&note);
                super::knowledge::global().mark_shown(&knowledge).await;
                data["knowledge"] = serde_json::json!(knowledge
                    .iter()
                    .map(|fact| serde_json::json!({
                        "kind": format!("{:?}", fact.kind),
                        "text": fact.text,
                    }))
                    .collect::<Vec<_>>());
            }
        }

        Ok(ToolExecutionResult::success(with_note(summary, note)).with_data(data))
    }
}

// ── Incremental reads ──────────────────────────────────────────────────────

/// What a read asked to be compared with an earlier one found.
pub(super) enum SinceAnswer {
    /// The comparison held: report the delta instead of the whole screen.
    Delta {
        text: String,
        data: serde_json::Value,
    },
    /// It did not: report the whole screen, and say why it was not compared.
    Full(super::diff::FullReason),
}

/// Answer a `since` argument against the read that has just been taken.
///
/// `None` means the caller did not ask for a delta at all. Otherwise this is
/// either the delta — what appeared, what left, what moved — or the reason the
/// whole list is the better answer ([`super::diff::FullReason`]).
pub(super) fn since_answer(
    device: Option<&str>,
    target: Target,
    since: Option<&str>,
    dump: &UiDump,
) -> Option<SinceAnswer> {
    let since = since?;
    match super::diff::compare(
        device,
        target.display_id(),
        since,
        dump.package.as_deref(),
        &dump.elements,
    ) {
        super::diff::ScreenDiff::Diff(delta) => {
            // Only what is on the screen now is addressable: an added row's
            // index works, and a removed row's does not.
            let mut present: Vec<&UiElement> = delta.added.iter().collect();
            present.extend(delta.moved.iter());
            let data = serde_json::json!({
                "since": since,
                "baseline": true,
                "diff": {
                    "added": delta.added.len(),
                    "removed": delta.removed.len(),
                    "moved": delta.moved.len(),
                    "unchanged": delta.unchanged,
                    "before": delta.before_len,
                    "after": delta.after_len,
                },
                "elements": present,
                "count": present.len(),
                "snapshot": dump.snapshot,
            });
            Some(SinceAnswer::Delta {
                text: super::diff::describe(&delta),
                data,
            })
        }
        super::diff::ScreenDiff::Full(reason) => Some(SinceAnswer::Full(reason)),
    }
}

/// The `data` field naming why a read was not compared.
pub(super) fn since_fallback(reason: super::diff::FullReason) -> serde_json::Value {
    serde_json::json!({
        "baseline": false,
        "fallback": match reason {
            crate::computer::platform::android::diff::FullReason::NoBaseline => "no_baseline",
            crate::computer::platform::android::diff::FullReason::AppChanged => "app_changed",
            crate::computer::platform::android::diff::FullReason::MostlyChanged => "mostly_changed",
        },
    })
}
