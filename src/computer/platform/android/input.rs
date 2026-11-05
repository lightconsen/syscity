//! Tap, swipe, type and press — the one path every input takes.
use async_trait::async_trait;
use serde_json::Value;

// The set, the device targeting, and the siblings' items: this file used
// to be part of `android`, and names them exactly as it did there.
use super::*;

/// Tap, swipe, type, and press keys on the device.
#[derive(Debug)]
pub struct AdbInputTool {
    device: Option<String>,
    /// The run this call is a step of, when it is one.
    ///
    /// A run puts its whole plan to a person once instead of asking about each
    /// of thirty steps; the plan travels with the step so that the step's own
    /// gate can check it is still the step that was described. Set only by
    /// `android_run`.
    plan: Option<super::gate::Plan>,
}

impl Default for AdbInputTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbInputTool {
    pub fn new() -> Self {
        Self { device: None, plan: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }

    /// Run this call as one step of an already-approved sequence.
    pub fn under_plan(mut self, plan: super::gate::Plan) -> Self {
        self.plan = Some(plan);
        self
    }

    /// Whether the device's active input method is ADBKeyboard.
    ///
    /// ADBKeyboard accepts text over a broadcast, which is the only way to type
    /// non-ASCII without a UI: `input text` goes through the device's key
    /// character map and drops or mangles anything outside ASCII (CJK in
    /// particular). Costs one extra `adb` round trip, so it is only consulted
    /// for text that actually needs it.
    async fn adbkeyboard_is_active(&self, device: &Option<String>) -> bool {
        let args =
            adb_args(device, &["shell", "settings", "get", "secure", "default_input_method"]);
        match run_cmd("adb", &args.iter().map(|s| s.as_str()).collect::<Vec<_>>()).await {
            Ok((status, stdout, _)) if status.success() => {
                stdout.to_lowercase().contains("adbkeyboard")
            }
            _ => false,
        }
    }
}

/// The one spelling of a keycode every dispatcher understands.
///
/// `input keyevent` accepts either a name (`KEYCODE_ENTER`, `ENTER`) or a
/// number, and so did this tool by passing the string straight through. The
/// display helper does not: it reflects the named field on `android.view.
/// KeyEvent` (`DevicePlugin.kt`, `op: "key"`), so a bare `ENTER` is an
/// `NoSuchFieldException` there. Normalising at the one place the key op is
/// built means a name works on both paths, and a number is left alone because
/// both accept it.
pub(super) fn normalize_keycode(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().all(|c| c.is_ascii_digit()) {
        return trimmed.to_string();
    }
    let upper = trimmed.to_ascii_uppercase();
    if upper.starts_with("KEYCODE_") {
        upper
    } else {
        format!("KEYCODE_{upper}")
    }
}

/// Quote a string as a single shell word for the *device* shell.
///
/// A single-quoted word may not contain a quote, so an embedded `'` has to close
/// the word, emit an escaped quote and reopen it (`'\''`) — the only escape a
/// single-quoted shell word supports. Without this a quote in the text ends the
/// command early and everything after it is parsed as further commands.
pub(super) fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Apply `input text`'s own escapes: `%s` means space and `%%` a literal `%`.
///
/// A literal `%` must be doubled *before* spaces become `%s`, otherwise the `%`
/// introduced here would itself be doubled.
pub(super) fn input_encode(s: &str) -> String {
    s.replace('%', "%%").replace(' ', "%s")
}

/// Build a tap sequence: several taps dispatched in a single shell command.
///
/// For a control that is about to auto-fade (a transient toolbar, a toast with a
/// button), tapping it, waiting for a model turn and tapping the next thing is
/// usually too slow — the control is gone. Chaining the taps into one dispatch
/// closes that window.
///
/// Deliberately coordinate-based: the later taps usually target a control that
/// only appears *because* an earlier one revealed it, so it cannot be validated
/// against the current tree. Each `input` is its own process on the device,
/// which spaces the taps by a few tens of milliseconds without needing `sleep`.
pub(super) fn encode_tap_sequence(steps: &[(i32, i32)]) -> String {
    steps
        .iter()
        .map(|(x, y)| format!("input tap {} {}", x, y))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Read a `steps` argument as a list of taps.
///
/// Each step is `{ x, y }` in pixels or `{ x_pct, y_pct }` in the 0–999 space
/// (a pixel pair wins when both are given). `size` is the screen those
/// percentages are a percentage of, and is only consulted for a step that
/// actually names one.
pub(super) fn parse_tap_steps(
    value: Option<&Value>,
    size: Option<(i32, i32)>,
) -> Result<Vec<coords::Point>, String> {
    let Some(Value::Array(items)) = value else {
        return Err("action 'sequence' requires a 'steps' array of { x, y } taps".to_string());
    };
    if items.is_empty() {
        return Err("action 'sequence' needs at least one step".to_string());
    }
    // A sequence is one dispatch, so the ladder, the gate and the pace all see
    // it as one action — but it lands as many taps. Unbounded, it is a way to
    // do fifty taps in the shape of one before any of them has an effect to
    // read. The cap is the per-call budget a person could watch.
    if items.len() > MAX_SEQUENCE_STEPS {
        return Err(format!(
            "action 'sequence' takes at most {MAX_SEQUENCE_STEPS} steps, got {} — split it, \
             checking the screen between the halves",
            items.len()
        ));
    }
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let has = |key: &str| item.get(key).and_then(Value::as_i64).is_some();
            // A step is one point, and both of its axes have to come from the
            // same space. Half a pixel pair, or a pair split across the two
            // spaces, would otherwise be read as "the missing axis is zero" —
            // a tap at the left edge the caller never asked for.
            let pixels = has("x") && has("y");
            let percents = has("x_pct") && has("y_pct");
            let partial =
                (has("x") || has("y")) != pixels || (has("x_pct") || has("y_pct")) != percents;
            if partial {
                return Err(format!(
                    "step {i} needs both of 'x' and 'y' (or both of 'x_pct' and 'y_pct')"
                ));
            }
            if !pixels && !percents {
                return Err(format!("step {i} needs integer 'x' and 'y'"));
            }
            coords::resolve(item, coords::TAP, size)
                .map_err(|refused| format!("step {i}: {refused}"))
        })
        .collect()
}

/// The screen a 0–999 coordinate is a fraction of.
///
/// A read that just happened already knows it, and that is the screen the
/// coordinates were going to be measured against — so the dump is preferred to
/// asking again, which for the user's screen would be a second round trip. Only
/// when the read failed does this fall back to `wm size`, which describes the
/// default display: a virtual display has a size of its own that `wm` does not
/// know about, so there the honest answer is "unknown" and a percent is refused
/// rather than converted against the wrong screen.
pub(super) async fn coordinate_space(
    device: &Option<String>,
    target: Target,
    from_dump: Option<(i32, i32)>,
) -> Option<(i32, i32)> {
    if let Some(size) = from_dump.filter(|(width, height)| *width > 0 && *height > 0) {
        return Some(size);
    }
    match target {
        Target::Display(_) => None,
        Target::Screen => super::tools::screen_size(device, None).await,
    }
}

/// The most taps one `sequence` may dispatch.
///
/// The same ceiling `android_run` puts on a command list: beyond it a single
/// call is no longer something the screen's own feedback can keep up with.
pub(super) const MAX_SEQUENCE_STEPS: usize = 30;

/// Build the device-shell command(s) that type `text`.
///
/// `input text` cannot produce a newline, so every line break becomes an
/// `input keyevent 66` (Enter) *between* the lines, which also makes blank lines
/// come out as paragraph breaks. A single-line text is one command.
pub(super) fn encode_input_text(text: &str) -> String {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();

    let mut commands: Vec<String> = Vec::with_capacity(lines.len() * 2);
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            commands.push("input keyevent 66".to_string());
        }
        if !line.is_empty() {
            commands.push(format!("input text {}", sh_quote(&input_encode(line))));
        }
    }
    commands.join("; ")
}

#[async_trait]
impl Tool for AdbInputTool {
    fn name(&self) -> &str {
        "android_input"
    }

    fn description(&self) -> &str {
        "Send tap, swipe, text, or key events to an Android device via ADB. For taps prefer \
         `target` (an element index from android_ui_tree): it is re-read from the live screen \
         before dispatch, so a screen that changed under you is reported instead of silently \
         tapping the wrong thing. Use action 'sequence' with several coordinate steps when you \
         need to hit something that will disappear before your next turn. The result reports \
         whether the screen actually changed afterwards (effect: changed / unchanged / unknown) \
         and what is on it now, so a separate android_observe is usually unnecessary. Set \
         verify=false when the action is not meant to change the screen and you want to skip \
         the extra reads. On a second display, 'gesture' sends one or two fingers (a pinch, a \
         curved drag), and non-ASCII text is typed by pasting it — both are things `input` \
         cannot do. A tap named by `target` or `key` is pressed as a control first (an \
         accessibility action on the element itself), then as a service gesture, and only then \
         by shell injection; the result's `via` says which was used. Every coordinate \
         argument also takes a 0–999 form (`x_pct` / `y_pct`, `x2_pct` / `y2_pct`): \
         pixels still win when both are given, and the result's `coordinates` reports \
         both spaces, so a point measured on one screen can be reused on another."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Send input to Android device",
            serde_json::json!({
                "action": {
                    "type": "string",
                    "description": "tap | sequence | swipe | text | key | gesture",
                    "enum": ["tap", "sequence", "swipe", "text", "key", "gesture"]
                },
                "paths": {
                    "type": "array",
                    "description": "For 'gesture': one or two fingers, each a list of 2–32 [x, y] points in the display's pixels. Two fingers moving apart is a pinch; one finger following several points is a curved drag. Only on a second display.",
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "array",
                            "items": { "type": "integer" }
                        }
                    }
                },
                "duration_ms": {
                    "type": "integer",
                    "description": "For 'gesture': how long the whole gesture takes (default 500, max 5000)."
                },
                "steps": {
                    "type": "array",
                    "description": "For 'sequence': taps to dispatch back to back in one call, for a control that will auto-fade before your next turn. Each is { \"x\": int, \"y\": int } in pixels, or { \"x_pct\": int, \"y_pct\": int } in the 0–999 space.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer" },
                            "y": { "type": "integer" },
                            "x_pct": { "type": "integer", "minimum": 0, "maximum": 999 },
                            "y_pct": { "type": "integer", "minimum": 0, "maximum": 999 }
                        }
                    }
                },
                "target": {
                    "type": "integer",
                    "description": "Element index from android_ui_tree. Preferred over x/y for taps: the element is re-read from the live screen and the tap is refused if it moved, changed, or is not clickable."
                },
                "target_description": {
                    "type": "string",
                    "description": "Optional label you expect at `target`. A mismatch refuses the tap and reports what is actually there."
                },
                "key": {
                    "type": "string",
                    "description": "Element key from the `key` field of an earlier android_ui_tree/android_observe. Preferred over `target`: the key is what the element *is*, so it is looked up again in the live screen even after the list scrolled or a row appeared. Refused with STALE_REF if it is gone, or if several elements are now indistinguishable — the refusal lists them."
                },
                "snapshot": {
                    "type": "string",
                    "description": "The `snapshot` of the read the reference came from. Optional; when the live read has a different snapshot the reference is still resolved by key, and the result says the screen had changed."
                },
                "visual": {
                    "type": "string",
                    "description": "A `@vN` handle from android_find_visually. The tap goes to the located control and is refused (VISUAL_STALE) when the screen no longer matches the one the box was drawn on. Use it for controls the accessibility tree cannot name, such as icons and canvas-drawn buttons."
                },
                "x": { "type": "integer", "description": "X coordinate in pixels (tap/swipe). No safety net — prefer `target`. A pixel always wins over `x_pct`." },
                "y": { "type": "integer", "description": "Y coordinate in pixels (tap/swipe). No safety net — prefer `target`." },
                "x2": { "type": "integer", "description": "End X in pixels (swipe)" },
                "y2": { "type": "integer", "description": "End Y in pixels (swipe)" },
                "x_pct": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 999,
                    "description": "X as 0–999 across the screen (tap/swipe/sequence): 0 is the left edge, 999 the right. Use it when you know where on the screen something is but not the screen's size — including the far end of a swipe when sent as `x2_pct`."
                },
                "y_pct": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 999,
                    "description": "Y as 0–999 down the screen (tap/swipe/sequence): 0 is the top edge, 999 the bottom."
                },
                "x2_pct": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 999,
                    "description": "End X as 0–999 (swipe)."
                },
                "y2_pct": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 999,
                    "description": "End Y as 0–999 (swipe)."
                },
                "text": { "type": "string", "description": "Text to type" },
                "keycode": { "type": "string", "description": "Android keycode name or number" },
                "verify": {
                    "type": "boolean",
                    "description": "Default true: re-read the screen after acting and report whether it changed. Costs two to three extra tree reads; set false to skip them."
                },
                "settle_ms": {
                    "type": "integer",
                    "description": "How long to wait for the screen to settle when verifying (default 1200, max 4000). Raise it for a slow screen transition."
                },
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            vec!["action"],
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

        // Which lock is the right one depends on where the action lands.
        //
        // The user's own screen has one pointer and one focus, so everything
        // addressed to it waits on the device. A virtual display has a focus of
        // its own, so two of them are two places to type and do not need to
        // wait for each other — but the *app* they are driving is shared, and
        // that is what the lease below is for.
        let _target = lock_target(&device, target).await;

        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("tap");

        let verify = args.get("verify").and_then(|v| v.as_bool()).unwrap_or(true);
        let settle_budget = args
            .get("settle_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(SETTLE_DEFAULT_MS);

        // One read serves both purposes: it validates an indexed tap (the
        // safety net) and it is the picture the effect is judged against. A
        // failed read only refuses when the caller pointed at an element —
        // every other action still dispatches, and reports its effect as
        // unknown rather than pretending it worked.
        // The dump's package is only used by the outward pacing, but it has to
        // be read before the action: afterwards the screen belongs to whatever
        // the action opened.
        let mut before_package: Option<String> = None;
        let mut before_snapshot: Option<String> = None;
        // The screen the read saw, kept for the 0–999 coordinate space: a
        // percent argument is a percent *of this*, and it is already in hand.
        let mut before_screen: Option<(i32, i32)> = None;
        let pointed_at_an_element = args.get("target").is_some() || args.get("key").is_some();
        let mut before = match dump_ui(&device, target).await {
            Ok(dump) => {
                before_package = dump.package;
                before_snapshot = Some(dump.snapshot);
                before_screen = dump.screen;
                Some(dump.elements)
            }
            Err(failed) => {
                if action == "tap" && pointed_at_an_element {
                    return Ok(ToolExecutionResult::error(failed.message));
                }
                None
            }
        };

        // The read named the app; now take exclusive use of it. This is the
        // half the display lock cannot give: two displays, one app. It is taken
        // after the read because the read is what names the app, so the read
        // itself may predate the lease — `stale` is that, and the answer is to
        // read again rather than act on a screen that has moved.
        let held = app_completed(&device, before_package.as_deref());
        let lease = lease_app(&device, before_package.as_deref(), held).await;
        if lease.stale {
            match dump_ui(&device, target).await {
                // Somebody wrote to this app while this call was waiting. The
                // tree it read a moment ago is no longer the screen; use this
                // one, and if the app itself changed, stop rather than take a
                // second lease on a different app while holding this one.
                Ok(dump) if dump.package == before_package => {
                    before_package = dump.package;
                    before_snapshot = Some(dump.snapshot);
                    before_screen = dump.screen;
                    before = Some(dump.elements);
                }
                Ok(dump) => {
                    return Ok(ToolExecutionResult::error(format!(
                        "Refused: the screen changed app while this call waited for another \
                         writer — it was {:?}, it is now {:?}. Nothing was done. Read the screen \
                         again and retry.",
                        before_package.as_deref().unwrap_or("unknown"),
                        dump.package.as_deref().unwrap_or("unknown"),
                    )));
                }
                Err(_) => {
                    // Nothing to act on: the screen cannot be read now, and the
                    // read in hand describes a moment before somebody else's
                    // write.
                    return Ok(ToolExecutionResult::error(
                        "Refused: another writer changed this app while this call was waiting, \
                         and the screen could not be re-read afterwards. Nothing was done. Read \
                         the screen again and retry.",
                    ));
                }
            }
        }

        // The app's own screens are the controls that govern the agent:
        // settings, pairing, the display list. Driving them is the agent
        // reaching for its own levers, and it is never what the task meant.
        // Refused here rather than in the gate because this is not a question
        // of judgement — there is no answer a person could give that makes it
        // right. Reads stay allowed: the agent may look at its own screen to
        // report on it.
        if let Some(package) = before_package.as_deref() {
            if super::takeover::is_own_app(package) {
                return Ok(ToolExecutionResult::error(format!(
                    "Refused: this would write to Syscity's own screen ({package}). The app's \
                     settings, pairing and display pages are the controls that govern the agent, \
                     so the agent does not operate them — a person does. Nothing was done."
                )));
            }
        }

        // Whether the phone is still in the state this action was computed
        // for. This runs before the ladder, because the ladder counts an action
        // that did not work, and an action that was never taken is not evidence
        // of anything — least of all that it should be tried again.
        let readiness = super::ready::probe().await;
        if let Err(blocked) =
            super::ready::check(readiness.as_ref(), target, before_package.as_deref())
        {
            return Ok(ToolExecutionResult::error(super::ready::describe(&blocked)));
        }

        // A reference from an earlier read is resolved against this one before
        // anything is dispatched, and the two ways of naming an element are the
        // same question asked with different information: an index is a
        // position in *that* read, a key is the element itself.
        let snapshot_note = match args
            .get("snapshot")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            Some(was) if before_snapshot.as_deref() != Some(was) => Some(
                "The screen had changed since that read; the element was found again by its key, \
                 not by where it used to be.",
            ),
            _ => None,
        };

        // The action is resolved once, into something that both dispatchers
        // understand. Where it goes — the device's screen or the agent's — is
        // the only thing that differs afterwards.
        //
        // A tap named by element may already have been delivered by a rung
        // above the shell (the control itself, or a service gesture), which is
        // what `pressed` records: the resolved op is then only the fallback the
        // safety checks below still reason about. `press_target` is the element
        // it would be pressed on, remembered rather than pressed — the gate and
        // the approval re-check run before anything leaves this process.
        let mut pressed: Option<(Via, (i32, i32))> = None;
        let mut press_target: Option<UiElement> = None;
        // A tap by `visual` handle. It is deliberately *not* a `press_target`:
        // the tree has no node for a control the accessibility API cannot
        // describe, and an accessibility action addressed to the wrong node
        // would press something else. It goes by shell injection at the point
        // the handle names, which is what a person's finger would do.
        let mut visual_target: Option<super::visual::VisualTarget> = None;
        // The screen a 0–999 coordinate is a fraction of. The read above already
        // knows it on every path that produced one, so a percent costs nothing
        // extra; only a caller that actually used a percent on a screen whose
        // read failed pays for the `wm size` fallback. Asked only for the three
        // actions that take a point: a `_pct` key beside a `text` action is not
        // a coordinate and must not buy a round trip.
        let screen =
            if matches!(action, "tap" | "swipe" | "sequence") && coords::uses_percent(&args) {
                coordinate_space(&device, target, before_screen).await
            } else {
                before_screen
            };
        let op = match action {
            "tap" => {
                // A handle from `android_find_visually` is checked before
                // anything else that would name a point. It is a stronger
                // reference than an index or a key: the element it names may be
                // the one thing on screen the tree cannot describe, so there is
                // nothing to relocate it *by* — the screen it was located on
                // is the reference, and a screenshot that no longer matches it
                // is a refusal rather than a tap at a place the control left.
                let visual = args
                    .get("visual")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|handle| !handle.is_empty());
                if let Some(handle) = visual {
                    match super::visual::resolve(handle, &device, target).await {
                        // The screenshot comes back with the handle but is not
                        // kept: the effect check below takes its own "before"
                        // picture, and that one has to be of the moment the
                        // press starts from, not of the moment it was resolved.
                        Ok((located, _png)) => {
                            visual_target = Some(located.clone());
                            InputOp::Taps(vec![located.center])
                        }
                        Err(refused) => {
                            return Ok(ToolExecutionResult::error(refused.message)
                                .with_data(serde_json::json!({ "code": refused.code })))
                        }
                    }
                } else if let Some(key) = args
                    .get("key")
                    .and_then(|v| v.as_str())
                    .filter(|k| !k.trim().is_empty())
                {
                    let elements = before.as_deref().unwrap_or(&[]);
                    match relocate(elements, key.trim()) {
                        Ok(element) => {
                            if let Some(description) =
                                args.get("target_description").and_then(|v| v.as_str())
                            {
                                let label = element_label(element).unwrap_or("");
                                if !label_matches(label, description) {
                                    return Ok(ToolExecutionResult::error(format!(
                                        "Refused: that key is now {label:?}, not {description:?} — \
                                         the screen changed. Re-read it with android_ui_tree."
                                    )));
                                }
                            }
                            match check_tappable(element, element.index) {
                                Ok(element) => {
                                    // Remembered, not pressed yet: the gate and
                                    // the approval re-check below run before
                                    // anything leaves this process.
                                    press_target = Some(element.clone());
                                    InputOp::Taps(vec![element.center])
                                }
                                Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                            }
                        }
                        Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                    }
                } else {
                    match args.get("target").and_then(|v| v.as_u64()) {
                        // An element index, checked against the read taken
                        // immediately before dispatch.
                        Some(index) => {
                            let description =
                                args.get("target_description").and_then(|v| v.as_str());
                            let elements = before.as_deref().unwrap_or(&[]);
                            match validate_target(
                                elements,
                                usize::try_from(index).unwrap_or(0),
                                description,
                            ) {
                                Ok(element) => {
                                    press_target = Some(element.clone());
                                    InputOp::Taps(vec![element.center])
                                }
                                Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                            }
                        }
                        // Raw coordinates: no safety net, the caller is on its own.
                        None => {
                            let point = match coords::resolve(&args, coords::TAP, screen) {
                                Ok(point) => point,
                                Err(refused) => return Ok(ToolExecutionResult::error(refused)),
                            };
                            InputOp::Taps(vec![point.pixels()])
                        }
                    }
                }
            }
            "swipe" => {
                match (
                    coords::resolve(&args, coords::TAP, screen),
                    coords::resolve(&args, coords::END, screen),
                ) {
                    (Ok(from), Ok(to)) => InputOp::Swipe {
                        from: from.pixels(),
                        to: to.pixels(),
                    },
                    (Err(refused), _) | (_, Err(refused)) => {
                        return Ok(ToolExecutionResult::error(refused))
                    }
                }
            }
            "text" => {
                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if text.is_empty() {
                    return Ok(ToolExecutionResult::error(
                        "action 'text' requires a non-empty 'text' argument",
                    ));
                }
                InputOp::Text(text.to_string())
            }
            "sequence" => match parse_tap_steps(args.get("steps"), screen) {
                Ok(steps) => InputOp::Taps(steps.iter().map(|point| point.pixels()).collect()),
                Err(msg) => return Ok(ToolExecutionResult::error(msg)),
            },
            "key" => InputOp::Key(normalize_keycode(
                args.get("keycode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("HOME"),
            )),
            "gesture" => match parse_gesture_paths(args.get("paths")) {
                Ok(paths) => InputOp::Gesture {
                    paths,
                    duration_ms: u32::try_from(
                        args.get("duration_ms")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(500),
                    )
                    .unwrap_or(500)
                    .clamp(50, 5000),
                },
                Err(message) => return Ok(ToolExecutionResult::error(message)),
            },
            _ => return Ok(ToolExecutionResult::error(format!("Unknown action: {}", action))),
        };

        // What is about to be pressed, before it is pressed.
        //
        // Only a tap can reach past the phone, and only a tap has a label to
        // judge: a swipe and a key event do not. The classification happens
        // here, after the tree read, because the tree is what names the
        // element — and it happens before the dispatch, because a send that is
        // refused must not leave the app.
        //
        // The repetition guard runs *before* the gate. Asking a person to
        // approve the twelfth identical send would be asking them to endorse a
        // loop the agent should have noticed itself; the ladder refuses that
        // without troubling anyone.
        // A tap on a located control is classified by the label the search
        // read, not by whatever the tree has at that point: the whole reason
        // the handle exists is that the tree does not describe this control,
        // and an unclassified press on an icon that turns out to say "删除"
        // is exactly the send this check is for.
        let signature = match visual_target.as_ref() {
            Some(located) if !located.label.trim().is_empty() => {
                format!("tap:{}", located.label.trim())
            }
            _ => action_signature(&op, before.as_deref()),
        };
        let guard_key = super::guard::Guard::bucket_key(
            &context.conversation_id,
            &format!("{}:{}", device.as_deref().unwrap_or("-"), target.display_id()),
        );
        let ladder = super::guard::Guard::global().check(&guard_key, &signature);
        if let Some(reason) = ladder.refusal() {
            return Ok(ToolExecutionResult::error(reason.to_string()));
        }

        let classified =
            classify_input(&op, before.as_deref().unwrap_or(&[]), &args, visual_target.as_ref());
        let clearance = match &self.plan {
            // A step of an approved run. The plan's yes covers the steps it
            // named, and those steps do not ask again; a step it did *not*
            // name is an action nobody has been asked about yet, and it goes
            // through the ordinary gate below rather than being refused for
            // the plan's omission — a run whose target text did not match
            // what the screen calls the button should not become un-runnable.
            Some(plan)
                if classified
                    .as_ref()
                    .is_some_and(|offer| plan.accounts_for(offer)) =>
            {
                plan.clearance(classified.as_ref(), before_package.as_deref())
                    .unwrap_or(super::gate::Clearance {
                        offer: classified.clone(),
                        asked: false,
                        granted: None,
                    })
            }
            // An ordinary call, or a step the plan never named: classified and
            // put to a human on its own.
            _ => match classified {
                Some(offer) => {
                    let subject = subject_for(
                        &op,
                        Some(&offer),
                        visual_target.as_ref(),
                        before_package.as_deref(),
                    );
                    match super::gate::check(
                        Some(offer),
                        context,
                        &subject,
                        before_package.as_deref(),
                    )
                    .await
                    {
                        Ok(clearance) => clearance,
                        Err(refusal) => return Ok(ToolExecutionResult::error(refusal.reason)),
                    }
                }
                None => super::gate::Clearance {
                    offer: None,
                    asked: false,
                    granted: None,
                },
            },
        };
        // Whether this call is a step of a run rather than a question of its
        // own. The re-read below exists because a *question* left the screen
        // unattended for as long as a person took to read it; a step was
        // approved as part of a plan read immediately before the run started,
        // and re-reading after every step would cost the run its whole point.
        let plan_approved = self.plan.as_ref().is_some_and(|plan| plan.grant.is_some());

        // The question was open for as long as the person took to read it, and
        // a phone screen moves under a question: a message arrives and the row
        // under the finger becomes a different row. What they approved was the
        // button described in the question. Before dispatching, check that the
        // same thing is still under the finger — one re-read, and only when
        // somebody actually said yes to *this* call.
        if clearance.granted.is_some() {
            let fresh = match dump_ui(&device, target).await {
                Ok(fresh) => fresh,
                Err(failed) => {
                    return Ok(ToolExecutionResult::error(format!(
                        "\"{}\" was approved, but the screen could not be read again to confirm \
                         it is still there ({}). Nothing was done. Read the screen with \
                         android_observe and retry if it is still wanted.",
                        clearance
                            .granted
                            .as_ref()
                            .map(|grant| grant.subject.clone())
                            .unwrap_or_default(),
                        failed.message
                    )))
                }
            };
            // The same judgement, made again against the screen as it is now.
            // Everything that turns an action into the approved thing — the
            // element under a tap, the field an enter lands in, the strongest
            // step of a sequence — is asked again rather than remembered: the
            // question sat open for as long as the person took to read it.
            let now = classify_input(&op, &fresh.elements, &args, visual_target.as_ref());
            if let Err(withheld) =
                super::gate::verify(&clearance, now.as_ref(), fresh.package.as_deref())
            {
                return Ok(ToolExecutionResult::error(withheld.reason));
            }
            // A located control has no node to re-read, so the tree above cannot
            // say whether it is still there: the label is the same word it was
            // when the person approved it, and the thing wearing that label may
            // have moved a row down. Ask the screen the question the handle was
            // issued with, exactly as the tap would have before the question was
            // put to them.
            //
            // Skipped when the call runs under a sequence's approval: there is
            // nothing a person approved *for this step*, and a handle resolved
            // a moment ago belongs to the plan's own read, not to a question
            // that sat open while somebody read it.
            if !plan_approved {
                if let Some(located) = visual_target.as_ref() {
                    if let Err(refused) = super::visual::confirm(located, &device, target).await {
                        return Ok(ToolExecutionResult::error(refused.message)
                            .with_data(serde_json::json!({ "code": refused.code })));
                    }
                }
            }
        }

        // Two things have to be read *before* the action, not after: the
        // picture (the tree cannot see a canvas change) and the user's screen
        // (an app that lands there has to be told apart from one the user
        // opened). Both are only worth taking on a display, where the action
        // is not supposed to touch the user's screen at all.
        //
        // The user's own screen is different: the only way to see its pixels is
        // a raw `screencap`, a second ADB round trip carrying ~10 MB, and an
        // action on it has no spill to watch for. So its picture is taken only
        // when the tree has already said "unchanged" — after the action, once
        // — and the tree is read again from that same moment. The cost is one
        // extra round trip and one extra read, and only on the path where
        // without them the answer would have been a bare "unchanged".
        let watched = match target {
            Target::Display(id) if verify => {
                let pixels = display_pixels(id).await;
                let watch = match display_bridge() {
                    Ok(bridge) => {
                        let display = super::display::VirtualDisplay::attach(bridge, id);
                        match display.watch_main_screen(SPILL_WINDOW_MS as u32).await {
                            Ok(watch) => Some(watch),
                            Err(failed) => {
                                // Without the window there is no spill check at
                                // all, so a failure here has to be visible.
                                tracing::warn!(
                                    display = id,
                                    error = %failed,
                                    "could not start watching the user's screen; \
                                     a spill during this action will go unreported",
                                );
                                None
                            }
                        }
                    }
                    Err(_) => None,
                };
                Some((pixels, watch))
            }
            _ => None,
        };

        // The event subscription is taken here, before the action, so an event
        // the action causes is still in the channel when the settle starts.
        // Taking it afterwards would miss exactly the event wanted. It is
        // detached from what the channel already holds by the settle itself.
        let mut settle_stream = if verify { settle_events(target) } else { None };
        // Whatever the channel held before the action is history: a content
        // event from the previous screen would otherwise make the settle
        // return at its first quiet moment and never see this action's.
        if let Some(receiver) = settle_stream.as_mut() {
            while receiver.try_recv().is_ok() {}
        }

        // The press itself, now that it is allowed to happen and the picture
        // the effect is judged against has been taken: the ladder is tried in
        // order and `pressed` records the rung that delivered it. A shell rung
        // sends nothing here — the dispatcher below is the only one that can
        // reach a virtual display.
        if let Some(element) = press_target.as_ref() {
            let (via, refusal) =
                press_element(element, false, matches!(target, Target::Screen)).await;
            if let Some(refusal) = refusal {
                return Ok(ToolExecutionResult::error(refusal));
            }
            pressed = Some((via, element.center));
        }

        // Which rung pressed it — only a press has an answer. A swipe or a
        // typed line is a shell injection, and saying "shell" there would read
        // as if the ladder had been tried and failed.
        let via = (action == "tap" || pressed.is_some())
            .then(|| pressed.as_ref().map(|(via, _)| *via).unwrap_or(Via::Shell));

        // Whether the text went in by clipboard rather than by keystrokes —
        // the result says which, because the two fail in different ways.
        let pasted = match target {
            Target::Screen => {
                if let InputOp::Gesture { .. } = &op {
                    // `input` has no multi-finger form at all, and the
                    // accessibility service is the only thing that could
                    // dispatch one — but only to a display it owns. Saying so
                    // beats sending one finger of a pinch.
                    return Ok(ToolExecutionResult::error(
                        "Multi-finger gestures are only dispatched on a second display \
                         (android_display start). On the user's screen use 'swipe', or run the \
                         task on a display where the gesture can be sent properly.",
                    ));
                }
                // Set when the text went in by clipboard paste rather than by
                // `input text`: the result line says so, and the readback below
                // is the only confirmation a paste gets.
                let mut pasted = false;
                let shell_cmd = match &op {
                    // Rejected above: no shell form exists for two fingers.
                    InputOp::Gesture { .. } => {
                        return Ok(ToolExecutionResult::error(
                            "Multi-finger gestures are only dispatched on a second display \
                             (android_display start).",
                        ))
                    }
                    InputOp::Taps(steps) => encode_tap_sequence(steps),
                    InputOp::Swipe { from, to } => {
                        format!("input swipe {} {} {} {}", from.0, from.1, to.0, to.1)
                    }
                    InputOp::Key(keycode) => format!("input keyevent {keycode}"),
                    InputOp::Text(text) if text.is_ascii() => encode_input_text(text),
                    InputOp::Text(text) => {
                        if self.adbkeyboard_is_active(&device).await {
                            // ADBKeyboard takes the text base64-encoded over a
                            // broadcast, which carries any character the device
                            // can render.
                            let payload = base64::Engine::encode(
                                &base64::engine::general_purpose::STANDARD,
                                text.as_bytes(),
                            );
                            format!("am broadcast -a ADB_INPUT_B64 --es msg '{}'", payload)
                        } else {
                            // Two ways without ADBKeyboard, tried in that order:
                            // the app's own helper (uid 2000, which can inject
                            // Ctrl+V on the user's screen and can put the
                            // person's clipboard back afterwards), and nothing.
                            //
                            // `input text` types through the device's key
                            // character map, so non-ASCII (CJK in particular)
                            // comes out dropped or mangled. Failing loudly is
                            // still better than silently typing the wrong thing
                            // — but only once the paste has been ruled out.
                            match paste_on_user_screen(text).await {
                                Ok(()) => {
                                    pasted = true;
                                    String::new()
                                }
                                Err(_) => {
                                    return Ok(ToolExecutionResult::error(
                                        "Cannot type non-ASCII text: `input text` only types \
                                         ASCII reliably, ADBKeyboard is not the active input \
                                         method, and the display helper is not available on this \
                                         device to paste it. Install/enable ADBKeyboard \
                                         (com.android.adbkeyboard) and select it as the input \
                                         method, or keep the text ASCII.",
                                    ))
                                }
                            }
                        }
                    }
                };
                if !pasted {
                    let argv = adb_args(&device, &["shell", &shell_cmd]);
                    // A press already delivered by a rung above the shell is
                    // not sent again: what the safety net still owes the
                    // caller is the *effect*, not a second tap.
                    if pressed.is_none() {
                        let (status, _stdout, stderr) =
                            run_cmd("adb", &argv.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                                .await
                                .map_err(|e| crate::error::SyscityError::ExternalService {
                                    source: "adb input failed".to_string(),
                                    cause: Some(Box::new(e)),
                                })?;
                        if !status.success() {
                            let note = note_failure(&guard_key, &signature, true);
                            return Ok(ToolExecutionResult::error(with_note(
                                format!("adb input failed: {stderr}"),
                                note,
                            )));
                        }
                    }
                }
                pasted
            }
            Target::Display(id) => {
                let mut pasted = false;
                if let InputOp::Text(text) = &op {
                    if !text.is_ascii() {
                        // `input -d text` goes through the key character map and
                        // mangles anything outside ASCII. The helper's clipboard
                        // paste carries whatever the app can render, and puts
                        // the user's own clipboard back afterwards.
                        if let Err(message) = paste_on_display(id, text).await {
                            let note = note_failure(&guard_key, &signature, true);
                            return Ok(ToolExecutionResult::error(with_note(message, note)));
                        }
                        pasted = true;
                    }
                }
                // A press already delivered by a rung above the shell is not
                // sent again, and a pasted text is already in.
                if !pasted && pressed.is_none() {
                    if let Err(message) = dispatch_display(id, &op).await {
                        let note = note_failure(&guard_key, &signature, true);
                        return Ok(ToolExecutionResult::error(with_note(message, note)));
                    }
                }
                pasted
            }
        };

        // The action is now out. Record how far it reached, so the next one in
        // the same app is spaced like a person's would be. The package is the
        // one read *before* the action: after it, the screen may belong to
        // whatever the action opened.
        let pace_note = if clearance.reaches_outward() && context.mobile_control().pace_outward {
            Some(note_outward(before_package.as_deref()).await)
        } else {
            None
        };

        // A write is the one action whose *content* can be checked, and "the
        // shell accepted the keystrokes" is not "the field holds the text" — a
        // message that never lands is the failure mode that matters. Read it
        // back here, before the settle re-read: if the text opened a suggestion
        // list, the field is still there now and may not be after settling.
        let readback = match (&op, verify) {
            (InputOp::Text(the_text), true) => {
                Some(read_back_write(&device, target, the_text, before.as_deref()).await)
            }
            _ => None,
        };
        let text_len = match &op {
            InputOp::Text(the_text) => the_text.chars().count(),
            _ => 0,
        };

        let serial = resolved_serial(&device).await;
        // Where the finger actually went, in both spaces. A caller that sent a
        // percent gets its own step back; one that sent pixels — or pointed at
        // an element — gets the percent that names that pixel, which is what
        // makes a coordinate reusable on a screen of another size. A sequence
        // is several taps and is not described here: its own steps were already
        // coordinates, and reporting only the first would name one of N.
        let coordinates = match &op {
            InputOp::Taps(points) if points.len() == 1 => {
                coords::from_pixels(points[0].0, points[0].1, screen).json()
            }
            InputOp::Swipe { from, to } => serde_json::json!({
                "from": coords::from_pixels(from.0, from.1, screen).json(),
                "to": coords::from_pixels(to.0, to.1, screen).json(),
            }),
            _ => Value::Null,
        };
        if !verify {
            // No re-read was asked for, so the screen did not move as far as
            // this call knows — recorded as unknown, which the ladder does not
            // count as "no effect".
            super::guard::Guard::global().record(
                &guard_key,
                &signature,
                super::guard::Outcome::Unknown,
            );
            let mut text = format!("Input '{action}' sent (effect not checked).");
            if let Some(note) = via.and_then(Via::note) {
                text.push_str(&format!(" {note}"));
            }
            if pasted {
                text.push_str(&format!(
                    " The text went in by clipboard paste ({} character(s)) — `input text` \
                     cannot carry non-ASCII text. The person's own clipboard was put back.",
                    text_len
                ));
            }
            if let Some(note) = snapshot_note {
                text.push_str(&format!(" {note}"));
            }
            if let Some(note) = ladder_note(&ladder) {
                text.push_str(&format!(" {note}"));
            }
            if let Some(note) = clearance.note() {
                text.push_str(&format!(" Judged {note}."));
            }
            return Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
                "device": serial,
                "effect": "unverified",
                "via": via.map(Via::as_str),
                "coordinates": coordinates,
                "risk": clearance.note(),
                "risk_approved": clearance.asked,
                "paced": pace_note,
                "reference_moved": snapshot_note.is_some(),
            })));
        }

        // Judge the effect here rather than leaving it to the model: a dispatch
        // that was accepted says nothing about whether the tap landed.
        let before_fingerprint = before.as_ref().map(|elements| tree_fingerprint(elements));
        let before_copy = before
            .as_ref()
            .map(|elements| (before_fingerprint.unwrap_or_default(), elements.clone()));
        let before_pixels = watched.as_ref().and_then(|(pixels, _)| pixels.as_ref());
        let mut settle = settle_after_action_with_pixels_and_events_from(
            &device,
            target,
            before_copy,
            before_pixels,
            settle_stream,
            settle_budget,
        )
        .await;

        // The tree said nothing changed, and on the user's own screen there was
        // no before-frame to compare against. Some screens — a game, a map, a
        // camera preview — change without changing what the tree exposes, so
        // "unchanged" there is not yet an answer. Take the picture once, and
        // only now: this is a raw `screencap`, a second ADB round trip of ~10 MB,
        // and paying it for every tap would make every tap twice as slow for an
        // answer only canvases need. Two reads a moment apart are enough to
        // tell a picture that moved from one that blinked (a cursor, a clock).
        if matches!(target, Target::Screen)
            && settle.effect == EFFECT_UNCHANGED
            && before_package.is_some()
        {
            if let Some(first) = user_screen_signature(&device).await {
                tokio::time::sleep(std::time::Duration::from_millis(SETTLE_POLL_MS)).await;
                if let Some(second) = user_screen_signature(&device).await {
                    settle.pixel_changed = Some(first.differs(&second));
                    if settle.pixel_changed == Some(true) {
                        settle.effect = EFFECT_SMALL_CHANGE;
                    }
                }
            }
        }

        // Now that both reads are in hand, the place-change question can be
        // answered — it needs the package from *before* the action, which only
        // this call has.
        if settle.effect != EFFECT_UNKNOWN {
            if let (Some(before), Some(after)) =
                (before_package.as_deref(), settle.package.as_deref())
            {
                if before != after {
                    settle.effect = EFFECT_PLACE_CHANGED;
                }
            }
        }

        // The ladder learns what this did. It counts the screen's answer, not
        // the action's: three different taps that all leave the same screen are
        // one stuck screen, and the streak is what says so. The screen it left
        // is the frame — without one, a model wandering through the same two
        // screens is invisible. An unread tree is not a frame, so nothing is
        // claimed about where the task is.
        let outcome = guard_outcome(settle.effect);
        let frame = (!settle.elements.is_empty()).then(|| tree_fingerprint(&settle.elements));
        super::guard::Guard::global().record_with(&guard_key, &signature, outcome, frame);
        // The action reached the phone. Whether it did what was asked is what
        // the settle decided; the failure ladder counts calls that never got
        // that far.
        note_success(&guard_key);

        let mut text = format!("Input '{action}' sent. {}", settle.verdict());
        if let Some(note) = via.and_then(Via::note) {
            text.push_str(&format!(" {note}"));
        }
        if pasted {
            text.push_str(&format!(
                " The text went in by clipboard paste ({} character(s)) — `input text` cannot \
                 carry non-ASCII text.",
                text_len
            ));
        }
        if let Some(note) = readback_note(readback.as_ref()) {
            text.push_str(&format!(" {note}"));
        }
        if let Some(note) = snapshot_note {
            text.push_str(&format!(" {note}"));
        }
        if let Some(note) = ladder_note(&ladder) {
            text.push_str(&format!(" {note}"));
        }
        if let Some(note) = clearance.note() {
            text.push_str(&format!(" Judged {note}."));
        }
        if let Some(note) = &pace_note {
            text.push_str(&format!(" {note}"));
        }
        if settle.pixel_changed == Some(true) && settle.elements.is_empty() {
            text.push_str(
                " The picture changed even though the tree names nothing here — this screen draws \
                 itself.",
            );
        }

        // An action can start something that lands on the user's screen — a
        // share sheet, a chooser, an app that ignores `--display`. The agent's
        // display then just stops changing, which reads as "nothing happened".
        //
        // The window taken before the action is what tells a spill apart from
        // an app the user opened themselves. Without it there is only "what is
        // on top now", which cannot know who put it there.
        let mut spills = Vec::new();
        if let Target::Display(id) = target {
            if let Some((_, Some((top, since)))) = watched.as_ref() {
                if let Ok(bridge) = display_bridge() {
                    spills = super::takeover::watch_and_check_spill(
                        &bridge,
                        id,
                        *top,
                        *since,
                        SPILL_WINDOW_MS,
                        &[],
                        true,
                    )
                    .await;
                }
            }
        }
        for spill in &spills {
            text.push_str(&format!(
                "\n\nWarning: {} ended up on the user's screen ({}) rather than this display — \
                 {}. The screen above is this display; the app above is not.",
                spill.package, spill.task_id, spill.reason
            ));
        }
        if !settle.elements.is_empty() {
            text.push_str("\n\n");
            text.push_str(&format_ui_summary_limited(&settle.elements, 60));
        }

        Ok(ToolExecutionResult::success(text).with_data(serde_json::json!({
            "device": serial,
            "effect": settle.effect,
            "via": via.map(Via::as_str),
            "pixels_changed": settle.pixel_changed,
            // What answered first: `events` (a content event, then quiet) or
            // `tree` (the poll). The model does not need it to act, but it is
            // how "the stream is working on this phone" is visible at all.
            "first_signal": settle.first_signal.map(|signal| signal.as_str()),
            "spill": spills.first().map(|s| serde_json::json!({
                "task": s.task_id,
                "package": s.package,
                "recovered": s.recovered,
                "reason": s.reason,
            })),
            "spills": spills.iter().map(|s| serde_json::json!({
                "task": s.task_id,
                "package": s.package,
                "recovered": s.recovered,
                "reason": s.reason,
            })).collect::<Vec<_>>(),
            "settled_ms": settle.elapsed_ms,
            "elements": settle.elements,
            "readback": readback.as_ref().map(|read| serde_json::json!({
                "verdict": read.verdict,
                "actual": read.actual,
                "note": read.note,
            })),
            "risk": clearance.note(),
            "risk_approved": clearance.asked,
            "coordinates": coordinates,
            "paced": pace_note,
            "reference_moved": snapshot_note.is_some(),
            "guard": ladder.note(),
            // Which `@vN` this tap went to, when it went by handle rather than
            // by index or key — so the transcript says a visual tap was a
            // visual tap, and a re-search can be matched to the press it led to.
            "visual": visual_target.as_ref().map(|located| located.handle.clone()),
        })))
    }
}

/// How long an action's spill window stays open.
///
/// Long enough for a chooser to appear after the tap that opened it, short
/// enough that a task the user starts themselves a minute later is not blamed
/// on the agent.
pub(super) const SPILL_WINDOW_MS: u64 = 3000;

/// Record an outward action for the pace and describe the pause it took.
///
/// `package` is where the action happened. When the read that preceded it named
/// no app (an empty tree, a failed accessibility fetch), the history is keyed
/// on `"?"` rather than skipped: a pace that does not apply to the app the tree
/// failed on would be no pace at all for the very screens that are hardest to
/// read.
pub(super) async fn note_outward(package: Option<&str>) -> String {
    let package = package.unwrap_or("?");
    let waited = super::pacing::Pacing::global().pace(package).await;
    if waited.is_zero() {
        "Next outward action here can go straight away.".to_string()
    } else {
        format!(
            "Waited {} ms so the next outward action in this app is not back-to-back.",
            waited.as_millis()
        )
    }
}
