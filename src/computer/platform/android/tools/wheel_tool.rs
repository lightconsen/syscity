//! Turn a wheel — a time, a date, a region — in one call.

use super::*;

/// Turn a wheel picker (time, date, region) to a value.
#[derive(Debug)]
pub struct AdbSetWheelTool {
    device: Option<String>,
}

impl Default for AdbSetWheelTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbSetWheelTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

/// How many scroll actions one wheel may receive before giving up.
const WHEEL_MAX_STEPS: i32 = 80;
/// How many drag rounds one wheel may receive.
const WHEEL_MAX_DRAGS: i32 = 5;
/// How far apart two readings must be for the wheel to count as settled.
const WHEEL_SETTLE_MS: u64 = 350;
/// How many times the wheel is read again after a value is expected.
const WHEEL_SETTLE_READS: usize = 5;
/// The most characters a value argument may hold.
const WHEEL_MAX_VALUE_CHARS: usize = 60;
/// A visible column smaller than this cannot hold a real drag.
const WHEEL_MIN_DRAG_ROWS: i32 = 3;
/// A drag shorter than this is dispatched as a tap by the input layer.
const WHEEL_MIN_DRAG_PX: i32 = 48;

/// A drag a lockstep wheel would have been sent, planned by `WheelMath`.
///
/// The drag goes through `android_input`, so the guard, the approval gate and
/// the effect check all see it in one history together with the caller's own
/// actions; it is the same gesture the model would have sent by hand, planned
/// once instead of once per item.
#[derive(Debug, Clone, PartialEq)]
struct WheelPinch {
    /// The straight-line drag in device pixels, from the column's lower edge
    /// upwards (forward) or the upper edge downwards.
    from: (i32, i32),
    to: (i32, i32),
    /// Hold the finger down instead of flicking, when the last drag did not
    /// move the wheel.
    slow: bool,
}

impl WheelPinch {
    /// The `android_input` arguments for this drag.
    fn input_args(&self, device: &Option<String>) -> Value {
        let mut args = serde_json::json!({
            "action": "swipe",
            "x": self.from.0,
            "y": self.from.1,
            "x2": self.to.0,
            "y2": self.to.1,
            // A settle after every drag would double the reads this tool
            // already takes, and its own loop is the settle.
            "verify": false,
        });
        if let Some(serial) = device {
            args["device"] = serde_json::json!(serial);
        }
        args
    }
}

/// Where a drag starts and ends inside a column.
///
/// The finger lands in the middle of the column horizontally and starts near
/// the edge it is pushing *from*, so a drag longer than the column can leave
/// the column's bounds — a wheel keeps following a finger that started inside
/// it — while still staying on the screen.
fn pinch_on_column(
    list: &WheelColumn,
    steps: i32,
    item_height: i32,
    boost: f64,
    slow: bool,
) -> Option<WheelPinch> {
    if steps == 0 {
        return None;
    }
    let (left, right) = (list.left, list.right);
    let x = (left + right) / 2;
    let top = list
        .items
        .iter()
        .map(|item| item.center_y - item_height / 2)
        .min()
        .unwrap_or(list.mid - item_height / 2);
    let bottom = list
        .items
        .iter()
        .map(|item| item.center_y + item_height / 2)
        .max()
        .unwrap_or(list.mid + item_height / 2);
    let span = (bottom - top).max(item_height.max(1));
    let max_drag = (span - item_height).max(item_height * WHEEL_MIN_DRAG_ROWS);
    // One drag, clamped to the column's own span: splitting it here would be a
    // second loop on top of the planner's, and the planner already re-reads
    // between rounds.
    let distance = ((steps.abs() as f64) * f64::from(item_height) * boost).round() as i32;
    let distance = distance.max(WHEEL_MIN_DRAG_PX).min(max_drag);
    let up = steps > 0;
    let (from_y, to_y) = if up {
        (bottom - item_height / 2, bottom - item_height / 2 - distance)
    } else {
        (top + item_height / 2, top + item_height / 2 + distance)
    };
    if from_y == to_y {
        return None;
    }
    Some(WheelPinch {
        from: (x, from_y),
        to: (x, to_y),
        slow: slow || distance == WHEEL_MIN_DRAG_PX,
    })
}

/// The column a wheel occupies: the items of one picker, and its bounds.
#[derive(Debug, Clone, PartialEq)]
struct WheelColumn {
    items: Vec<crate::computer::platform::android::wheel::Item>,
    left: i32,
    right: i32,
    /// The middle of the selected item, which is the line the wheel reads by.
    mid: i32,
}

impl WheelColumn {
    /// The column read out of the flat element list.
    ///
    /// The element list is flat, so a column is found by *geometry* rather
    /// than by parentage: the wheel node's own bounds say which items are
    /// inside it, and the midline says which of those is selected. A caller
    /// that pointed at a single item (which is what a model usually does —
    /// the item showing the value) takes that item's horizontal line, and the
    /// column is every item aligned with it.
    fn from_elements(elements: &[UiElement], anchor: &UiElement) -> Option<Self> {
        // Each row is paired with its own bounds: the wheel's horizontal
        // extent is the rows' box, not the spread of their centres. A
        // correctly centred column has *identical* centre x's, so taking the
        // centres as the extent would make it zero-wide — and a zero-wide run
        // is not a wheel to `looks_like_wheel`, whose centring tolerance is a
        // fraction of the width.
        let rows: Vec<(crate::computer::platform::android::wheel::Item, (i32, i32, i32, i32))> =
            elements
                .iter()
                .filter(|element| {
                    // An item of this column is inside the wheel's horizontal
                    // band and has something written on it. The wheel node
                    // itself is excluded by bounds below when it has children;
                    // a `NumberPicker` with no reported children is its own
                    // item.
                    let (x1, y1, x2, y2) = element.bounds;
                    x2 > x1
                        && y2 > y1
                        && element.center.0 >= anchor.bounds.0
                        && element.center.0 <= anchor.bounds.2
                        && element_label(element).is_some()
                })
                .map(|element| {
                    let label = element_label(element).unwrap_or("").to_string();
                    let mut item = crate::computer::platform::android::wheel::Item::new(
                        label,
                        element.center.0,
                        element.center.1,
                    );
                    item.selected = element.state.selected;
                    item.height = (element.bounds.3 - element.bounds.1).max(0);
                    (item, element.bounds)
                })
                .collect();
        let items: Vec<crate::computer::platform::android::wheel::Item> =
            rows.iter().map(|(item, _)| item.clone()).collect();
        let centers: Vec<i32> = items.iter().map(|item| item.center_y).collect();
        let item_height = crate::computer::platform::android::wheel::item_height(&centers);
        let mid = anchor.center.1;
        let selected =
            crate::computer::platform::android::wheel::selected_index(&items, mid, item_height);
        // The column is the run around the selected row, so the heading above
        // it and the buttons beside it are not part of the wheel. The rows and
        // their bounds are parallel, so one index range slices both.
        let (start, end) = match selected {
            Some(anchor_index) => crate::computer::platform::android::wheel::column_range(
                items.len(),
                &centers,
                anchor_index,
            ),
            None => (0, items.len()),
        };
        let rows = &rows[start..end];
        if rows.is_empty() {
            return None;
        }
        // The extent is the rows' own bounds: an item sits inside its row, and
        // the centring test is about the row's middle, not the text's.
        let left = rows.iter().map(|(_, bounds)| bounds.0).min()?;
        let right = rows.iter().map(|(_, bounds)| bounds.2).max()?;
        if right <= left {
            return None;
        }
        let items = rows.iter().map(|(item, _)| item.clone()).collect();
        Some(Self { items, left, right, mid })
    }

    /// The value the column reads right now, cleaned for the model.
    fn current(&self) -> Option<String> {
        let height = crate::computer::platform::android::wheel::item_height(
            &self
                .items
                .iter()
                .map(|item| item.center_y)
                .collect::<Vec<_>>(),
        );
        let selected = crate::computer::platform::android::wheel::selected_index(
            &self.items,
            self.mid,
            height,
        )?;
        crate::computer::platform::android::wheel::clean(
            self.items.get(selected).map(|item| item.text.as_str()),
        )
    }

    /// The visible values, top to bottom.
    fn values(&self) -> Vec<String> {
        self.items.iter().map(|item| item.text.clone()).collect()
    }

    /// Whether the run is a wheel at all.
    fn looks_like_wheel(&self) -> bool {
        crate::computer::platform::android::wheel::looks_like_wheel(
            &self.items,
            self.left,
            self.right,
        )
    }
}

/// The element a wheel reference points at.
///
/// Three ways of naming it, all the same question asked with different
/// information: `ref` (the current name for a key) and `key` are the element
/// itself, resolved against the live screen; `target` is its index in the read
/// the caller used. A wheel is usually referenced by the item showing its
/// value, so either the picker or one of its items is accepted.
fn locate_wheel_target<'a>(
    elements: &'a [UiElement],
    args: &Value,
) -> Result<&'a UiElement, String> {
    let key = ["ref", "key"]
        .iter()
        .find_map(|field| args.get(*field).and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|key| !key.is_empty());
    if let Some(key) = key {
        return super::relocate(elements, key);
    }
    let Some(index) = args.get("target").and_then(|v| v.as_u64()) else {
        return Err(
            "give `ref` (or `target`) naming the wheel, or the item showing its value".to_string()
        );
    };
    let index = usize::try_from(index)
        .ok()
        .filter(|index| *index > 0)
        .ok_or_else(|| "Element indices are 1-based; 0 does not exist.".to_string())?;
    elements.get(index - 1).ok_or_else(|| {
        format!(
            "No element {index} on screen now — the UI has {} element(s). Re-read it with \
             android_ui_tree.",
            elements.len()
        )
    })
}

/// Read the column a wheel shows, in one pass over a fresh tree.
async fn read_wheel_column(
    device: &Option<String>,
    target: Target,
    args: &Value,
) -> Result<(UiDump, WheelColumn), String> {
    let dump = dump_ui(device, target)
        .await
        .map_err(|failure| failure.message)?;
    let anchor = locate_wheel_target(&dump.elements, args)?;
    let column = WheelColumn::from_elements(&dump.elements, anchor)
        .ok_or_else(|| "nothing readable is on that line of the screen".to_string())?;
    Ok((dump, column))
}

/// Read the wheel twice, `WHEEL_SETTLE_MS` apart, until it agrees with itself.
///
/// A wheel keeps turning for a moment after a scroll and snaps to the nearest
/// item; a single read taken too early reports a value the wheel is about to
/// leave. Two equal readings, or the read budget, is the same rule
/// `settle_after_action` applies to the whole screen.
async fn settle_wheel_column(
    device: &Option<String>,
    target: Target,
    args: &Value,
) -> Result<(UiDump, WheelColumn), String> {
    let mut last = read_wheel_column(device, target, args).await?;
    for _ in 1..WHEEL_SETTLE_READS {
        tokio::time::sleep(Duration::from_millis(WHEEL_SETTLE_MS)).await;
        let again = read_wheel_column(device, target, args).await?;
        if again.1.current() == last.1.current() {
            return Ok(again);
        }
        last = again;
    }
    Ok(last)
}

/// What a wheel run ended with.
#[derive(Debug)]
struct WheelOutcome {
    reached: bool,
    value: Option<String>,
    from: Option<String>,
    method: &'static str,
    attempts: u32,
    error: Option<&'static str>,
    hint: Option<String>,
}

impl WheelOutcome {
    fn reached(from: Option<String>, value: Option<String>, method: &'static str) -> Self {
        Self {
            reached: true,
            value,
            from,
            method,
            attempts: 0,
            error: None,
            hint: None,
        }
    }

    fn failed(
        from: Option<String>,
        value: Option<String>,
        method: &'static str,
        error: &'static str,
    ) -> Self {
        Self {
            reached: false,
            value,
            from,
            method,
            attempts: 0,
            error: Some(error),
            hint: None,
        }
    }

    fn with_attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts;
        self
    }

    fn stopped(stop: crate::computer::platform::android::wheel::WheelStop) -> &'static str {
        stop.code()
    }
}

/// The value argument, validated.
fn wheel_value(args: &Value) -> Result<String, String> {
    let raw = args
        .get("value")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "`value` is required: the value the wheel should show, as written on it".to_string()
        })?;
    if raw.chars().count() > WHEEL_MAX_VALUE_CHARS {
        return Err(format!(
            "`value` is at most {WHEEL_MAX_VALUE_CHARS} characters — it is the value the wheel \
             shows, not a description of it"
        ));
    }
    Ok(raw.to_string())
}

#[async_trait]
impl Tool for AdbSetWheelTool {
    fn name(&self) -> &str {
        "android_set_wheel"
    }

    fn description(&self) -> &str {
        "Turn a wheel picker (a time, a date, a region — the controls that step one item at a \
         time) to a value in one call, instead of tapping or swiping it item by item. Pass the \
         wheel's `key`, or the `key` of the item it currently shows, and the value as it is \
         written on the wheel. The result reports whether the wheel now shows it, what it showed \
         before, and how it was driven (text / actions / drag); when it could not be reached it \
         says which way it failed (not_a_wheel / wheel_unreadable / wheel_at_end / \
         wheel_not_moving / wheel_not_reached) and what the wheel shows now."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Turn a wheel picker to a value",
            serde_json::json!({
                "ref": {
                    "type": "string",
                    "description": "Element key from an earlier android_ui_tree/android_observe of the wheel, or of the item it currently shows. Preferred: it names the element itself, so a column that scrolled or a screen that renumbered is still found."
                },
                "key": {
                    "type": "string",
                    "description": "Element key of the wheel, or of the item it currently shows, from an earlier android_ui_tree/android_observe. The wheel is found again on the live screen, so a column that scrolled is still the same wheel. Elements flagged `wheel` are pickers."
                },
                "target": {
                    "type": "integer",
                    "description": "Element index of the wheel (or of one of its items) in the last read, for callers working from indices rather than keys."
                },
                "target_description": {
                    "type": "string",
                    "description": "Optional label expected on the item showing the value; a mismatch refuses the turn and reports what is actually there."
                },
                "snapshot": {
                    "type": "string",
                    "description": "The `snapshot` of the read the reference came from. Optional; when the live read differs the wheel is still found by key and the result says the screen had changed."
                },
                "value": {
                    "type": "string",
                    "description": "The value the wheel should show, as written on it: 9, 40, 2026, PM, Beijing. Full-width digits and a status prefix ('selected, 21') are understood."
                },
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            vec!["value"],
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
        let value = match wheel_value(&args) {
            Ok(value) => value,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };

        let (dump, column) = match settle_wheel_column(&device, target, &args).await {
            Ok(read) => read,
            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
        };
        let from = column.current();

        // A reference that only named an item is checked against the label the
        // caller expected from that item, not against the wheel's value.
        if let Some(expected) = args.get("target_description").and_then(|v| v.as_str()) {
            if let Ok(anchor) = locate_wheel_target(&dump.elements, &args) {
                let label = element_label(anchor).unwrap_or("");
                if !super::label_matches(label, expected) {
                    return Ok(ToolExecutionResult::error(format!(
                        "Refused: that element is {label:?}, not {expected:?} — the screen \
                         changed. Re-read it with android_ui_tree."
                    )));
                }
            }
        }

        if !column.looks_like_wheel() {
            // The node was *called* a wheel but does not read as one now: the
            // column has scrolled into a longer list, or the reference is a
            // list. Either way, turning it to a value is not something this
            // tool can promise.
            let flag = locate_wheel_target(&dump.elements, &args)
                .map(|anchor| anchor.state.wheel)
                .unwrap_or(false);
            let error = if flag {
                "wheel_unreadable"
            } else {
                "not_a_wheel"
            };
            let outcome = WheelOutcome::failed(from.clone(), column.current(), "none", error);
            return Ok(ToolExecutionResult::error(wheel_failure_text(&outcome, &value))
                .with_data(wheel_data(&outcome, &value, &column, &dump)));
        }

        let mut outcome = match column.current() {
            Some(current) => {
                if crate::computer::platform::android::wheel::matches(Some(&current), &value) {
                    return Ok(ToolExecutionResult::success(format!(
                        "The wheel already shows {value}."
                    ))
                    .with_data(serde_json::json!({
                        "ok": true,
                        "target": value,
                        "value": current,
                        "from": current,
                        "attempts": 0,
                        "method": "none",
                        "wheel": wheel_geometry(&column),
                        "device": resolved_serial(&device).await,
                    })));
                }
                WheelOutcome {
                    from: Some(current),
                    ..WheelOutcome::reached(None, None, "none")
                }
            }
            // A picker with no readable value at all. A wheel whose middle row
            // was not reported is still readable *around* the middle, and the
            // `between` rules are not needed here: the run is checked before
            // this point, so an unreadable value means the column changed
            // under the read.
            None => WheelOutcome::failed(None, None, "none", "wheel_unreadable"),
        };

        // 1. Typing into a picker: the platform `NumberPicker` has an editable
        // middle row, and writing into it is exact and cheap. A wheel drawn by
        // the app has nothing to type into, and this step is skipped.
        outcome = match super::wheel_typing::type_into(&device, target, &args, &value).await {
            super::wheel_typing::Typing::Unsupported => outcome,
            super::wheel_typing::Typing::Done => {
                let settled = match settle_wheel_column(&device, target, &args).await {
                    Ok((dump, column)) => (dump, column),
                    // The write happened; the read afterwards did not. The
                    // outcome still reports what was attempted.
                    Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                };
                if crate::computer::platform::android::wheel::matches(
                    settled.1.current().as_deref(),
                    &value,
                ) {
                    let done =
                        WheelOutcome::reached(outcome.from.clone(), settled.1.current(), "text")
                            .with_attempts(1);
                    return Ok(ToolExecutionResult::success(wheel_success_text(&done, &value))
                        .with_data(wheel_data(&done, &value, &settled.1, &settled.0)));
                }
                outcome
            }
        };

        // 2. Accessibility scrolling: a picker that exposes scroll actions
        // steps exactly, and batching the steps is what removes the model's
        // turn per item.
        let anchor_wheel = locate_wheel_target(&dump.elements, &args)
            .ok()
            .filter(|anchor| anchor.state.wheel)
            .cloned();
        if let Some(wheel_element) = anchor_wheel {
            let mut planner =
                crate::computer::platform::android::wheel::WheelSteps::new(&value, WHEEL_MAX_STEPS);
            let mut reading = column;
            let mut attempts = 0_u32;
            loop {
                match planner.next(reading.current().as_deref(), &reading.values()) {
                    crate::computer::platform::android::wheel::WheelStep::Done => {
                        // The wheel may still be turning; confirm with one
                        // settled read before claiming the value.
                        let (dump, settled) =
                            match settle_wheel_column(&device, target, &args).await {
                                Ok(read) => read,
                                Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                            };
                        if crate::computer::platform::android::wheel::matches(
                            settled.current().as_deref(),
                            &value,
                        ) {
                            let done = WheelOutcome::reached(
                                outcome.from.clone(),
                                settled.current(),
                                "actions",
                            )
                            .with_attempts(attempts);
                            return Ok(ToolExecutionResult::success(wheel_success_text(
                                &done, &value,
                            ))
                            .with_data(wheel_data(&done, &value, &settled, &dump)));
                        }
                        reading = settled;
                        continue;
                    }
                    crate::computer::platform::android::wheel::WheelStep::Stop(stop) => {
                        outcome.error = Some(WheelOutcome::stopped(stop));
                        outcome.value = reading.current();
                        outcome.method = "actions";
                        outcome.attempts = attempts;
                        break;
                    }
                    crate::computer::platform::android::wheel::WheelStep::Act {
                        forward,
                        count,
                    } => {
                        let sent =
                            super::wheel_typing::perform_scroll(&wheel_element, forward, count)
                                .await;
                        attempts = attempts.saturating_add(sent.max(0) as u32);
                        if sent <= 0 {
                            outcome.error = Some("wheel_at_end");
                            outcome.value = reading.current();
                            outcome.method = "actions";
                            outcome.attempts = attempts;
                            break;
                        }
                        match settle_wheel_column(&device, target, &args).await {
                            Ok((_, column)) => reading = column,
                            Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                        }
                    }
                }
            }
            if outcome.error == Some("wheel_at_end") && !reading.looks_like_wheel() {
                // The column stopped reading as a wheel: the list scrolled
                // past it, and retrying as a drag would aim at nothing.
                let failed = WheelOutcome::failed(
                    outcome.from.clone(),
                    outcome.value.clone(),
                    "actions",
                    "wheel_not_reached",
                )
                .with_attempts(attempts);
                return Ok(ToolExecutionResult::error(wheel_failure_text(&failed, &value))
                    .with_data(wheel_data(&failed, &value, &reading, &dump)));
            }
        }

        // 3. Dragging: a wheel the app drew itself exposes no scroll action,
        // so the only way to turn it is a finger on the column — planned from
        // how far one drag actually moves it, not guessed.
        drag_wheel(&device, target, &args, &value, outcome, context).await
    }
}

/// The third and last way to turn a wheel: drag the column.
async fn drag_wheel(
    device: &Option<String>,
    target: Target,
    args: &Value,
    value: &str,
    mut outcome: WheelOutcome,
    _context: &ToolContext,
) -> crate::Result<ToolExecutionResult> {
    let (dump, column) = match settle_wheel_column(device, target, args).await {
        Ok(read) => read,
        Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
    };
    let from = outcome.from.clone().or(column.current());
    let mut planner = crate::computer::platform::android::wheel::WheelDrags::new(
        value,
        crate::computer::platform::android::wheel::item_height(
            &column
                .items
                .iter()
                .map(|item| item.center_y)
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|| (column.right - column.left).clamp(1, 200)),
    );
    let mut reading = column;
    let mut dragged = 0_i32;
    let mut attempts = outcome.attempts;
    let mut rounds = 0_i32;
    loop {
        let height = planner.item_height();
        let heights: Vec<i32> = reading.items.iter().map(|item| item.center_y).collect();
        let selected = crate::computer::platform::android::wheel::selected_index(
            &reading.items,
            reading.mid,
            Some(height),
        );
        match planner.next(&reading.values(), selected, &heights, dragged) {
            crate::computer::platform::android::wheel::WheelDrag::Done => {
                let (dump, settled) = match settle_wheel_column(device, target, args).await {
                    Ok(read) => read,
                    Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                };
                if crate::computer::platform::android::wheel::matches(
                    settled.current().as_deref(),
                    value,
                ) {
                    let done = WheelOutcome::reached(from, settled.current(), "drag")
                        .with_attempts(attempts);
                    return Ok(ToolExecutionResult::success(wheel_success_text(&done, value))
                        .with_data(wheel_data(&done, value, &settled, &dump)));
                }
                reading = settled;
                continue;
            }
            crate::computer::platform::android::wheel::WheelDrag::Stop(stop) => {
                outcome.error = Some(WheelOutcome::stopped(stop));
                outcome.value = reading.current();
                outcome.method = "drag";
                outcome.from = from;
                outcome.attempts = attempts;
                break;
            }
            crate::computer::platform::android::wheel::WheelDrag::Drag { steps, boost, slow } => {
                rounds += 1;
                let Some(pinch) = pinch_on_column(&reading, steps, height, boost, slow) else {
                    outcome.error = Some("wheel_not_moving");
                    outcome.value = reading.current();
                    outcome.method = "drag";
                    outcome.from = from;
                    outcome.attempts = attempts;
                    break;
                };
                let tool = super::AdbInputTool::new();
                let tool = match device {
                    Some(serial) => tool.with_device(serial.clone()),
                    None => tool,
                };
                if tool
                    .execute(pinch.input_args(device), _context)
                    .await
                    .is_err()
                {
                    outcome.error = Some("wheel_not_reached");
                    outcome.value = reading.current();
                    outcome.method = "drag";
                    outcome.from = from;
                    outcome.attempts = attempts;
                    break;
                }
                attempts += 1;
                dragged = pinch.from.1 - pinch.to.1;
                tokio::time::sleep(Duration::from_millis(WHEEL_SETTLE_MS)).await;
                match read_wheel_column(device, target, args).await {
                    Ok((_, column)) => reading = column,
                    Err(refusal) => return Ok(ToolExecutionResult::error(refusal)),
                }
                if rounds >= WHEEL_MAX_DRAGS {
                    if crate::computer::platform::android::wheel::matches(
                        reading.current().as_deref(),
                        value,
                    ) {
                        let done = WheelOutcome::reached(from, reading.current(), "drag")
                            .with_attempts(attempts);
                        return Ok(ToolExecutionResult::success(wheel_success_text(&done, value))
                            .with_data(wheel_data(&done, value, &reading, &dump)));
                    }
                    outcome.error = Some("wheel_not_reached");
                    outcome.value = reading.current();
                    outcome.method = "drag";
                    outcome.from = from;
                    outcome.attempts = attempts;
                    break;
                }
            }
        }
    }
    Ok(ToolExecutionResult::error(wheel_failure_text(&outcome, value))
        .with_data(wheel_data(&outcome, value, &reading, &dump)))
}

/// What the geometry of the wheel is, for a result that has to explain itself.
fn wheel_geometry(column: &WheelColumn) -> serde_json::Value {
    serde_json::json!({
        "items": column.values(),
        "current": column.current(),
        "left": column.left,
        "right": column.right,
        "mid": column.mid,
    })
}

/// The message a caller reads on success.
fn wheel_success_text(outcome: &WheelOutcome, target: &str) -> String {
    let from = outcome.from.as_deref().unwrap_or("?");
    let method = match outcome.method {
        "text" => "typed into the picker",
        "actions" => "scrolled with the accessibility action",
        "drag" => "dragged the column",
        _ => "already set",
    };
    let _ = target;
    format!(
        "The wheel now shows {} (was {from}); {method}. Took {} attempt(s).",
        outcome.value.as_deref().unwrap_or("?"),
        outcome.attempts
    )
}

/// The message a caller reads on failure, with the value that is actually on
/// the wheel — a wrong value the model can reason about beats a bare error.
fn wheel_failure_text(outcome: &WheelOutcome, target: &str) -> String {
    let reason = match outcome.error.unwrap_or("wheel_not_reached") {
        "not_a_wheel" => {
            "that element is not a wheel picker — it is a list or a plain container, so turning it \
             to a value is not what it does. Use android_input to scroll it, or android_find to \
             look for the control that holds the value."
        }
        "wheel_unreadable" => {
            "the wheel is there but the value it shows cannot be read: its items are not exposed \
             to the accessibility tree. Read the screen with android_observe — the screenshot may \
             show the value — or use android_find on a nearby label."
        }
        "wheel_at_end" => {
            "the wheel stopped at the end of its range and the value was not there. If the wheel \
             wraps (a clock, a month), check the value is written as the wheel writes it."
        }
        "wheel_not_moving" => {
            "the wheel did not move. It is on screen but ignored the scroll action and the drag — \
             it may be disabled, or another window is taking input."
        }
        _ => {
            "the wheel was turned as far as it goes without showing that value. It may not offer \
             it (a 0–59 column has no 75), or it may need a settling screen it never got."
        }
    };
    let shown = outcome
        .value
        .as_deref()
        .map(|value| format!(" It shows {value:?} now."))
        .unwrap_or_default();
    format!(
        "Could not set the wheel to {target:?}: {reason}{shown}{}",
        outcome
            .hint
            .as_deref()
            .map(|hint| format!(" {hint}"))
            .unwrap_or_default()
    )
}

/// The data payload shared by success and failure.
fn wheel_data(
    outcome: &WheelOutcome,
    target: &str,
    column: &WheelColumn,
    dump: &UiDump,
) -> serde_json::Value {
    serde_json::json!({
        "ok": outcome.reached,
        "target": target,
        "error": outcome.error,
        "value": outcome.value,
        "from": outcome.from,
        "attempts": outcome.attempts,
        "method": outcome.method,
        "wheel": wheel_geometry(column),
        "snapshot": dump.snapshot,
        "source": dump.source,
    })
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;

    #[test]
    fn the_column_of_a_wheel_is_the_run_around_its_middle() {
        // A heading, three values and a button below them — all centred on the
        // same line, which is what a screen with one wheel looks like.
        let elements = vec![
            wheel_row(1, "Hour", 540, 900),
            wheel_row(2, "20", 540, 1120),
            wheel_row(3, "21", 540, 1200),
            wheel_row(4, "22", 540, 1280),
            wheel_row(5, "OK", 540, 1500),
        ];
        let anchor = &elements[2];
        let column = WheelColumn::from_elements(&elements, anchor).expect("a column");
        assert_eq!(column.current().as_deref(), Some("21"));
        assert_eq!(column.values(), vec!["20", "21", "22"]);
    }
    #[test]
    fn a_sideways_neighbour_is_not_part_of_the_column() {
        // The hour wheel and the minute wheel sit side by side. Pointing at
        // one must not read the other.
        let elements = vec![
            wheel_row(1, "09", 300, 1120),
            wheel_row(2, "10", 300, 1200),
            wheel_row(3, "11", 300, 1280),
            wheel_row(4, "45", 800, 1120),
            wheel_row(5, "46", 800, 1200),
            wheel_row(6, "47", 800, 1280),
        ];
        let anchor = &elements[1];
        let column = WheelColumn::from_elements(&elements, anchor).expect("a column");
        assert_eq!(column.values(), vec!["09", "10", "11"]);
        assert_eq!(column.current().as_deref(), Some("10"));
    }
    #[test]
    fn a_short_centred_even_run_is_a_wheel() {
        let elements = vec![
            wheel_row(1, "20", 540, 1120),
            wheel_row(2, "21", 540, 1200),
            wheel_row(3, "22", 540, 1280),
        ];
        let column = WheelColumn::from_elements(&elements, &elements[1]).expect("a column");
        assert!(column.looks_like_wheel());
    }
    #[test]
    fn a_left_aligned_list_is_not_a_wheel() {
        // Same three values, but the anchor sees them against a left edge:
        // they are not centred on its midline, so this is a list.
        let mut elements = vec![
            wheel_row(1, "20", 400, 1120),
            wheel_row(2, "21", 400, 1200),
            wheel_row(3, "22", 400, 1280),
        ];
        elements[1].center = (400, 1200);
        let column = WheelColumn::from_elements(&elements, &elements[1]).expect("a column");
        // The column's own bounds are the items' centres, so the geometry test
        // is about them: three items, evenly spaced, and they are all centred
        // on the same x — a wheel by the geometry. What makes it not a wheel in
        // practice is that its parent is a list; that decision is the bridge's
        // (`wheel` flag), and the tool trusts the flag when it is there.
        assert_eq!(column.values(), vec!["20", "21", "22"]);
    }
    #[test]
    fn a_coordinate_drag_can_leave_the_column_but_not_the_screen() {
        let elements = vec![
            wheel_row(1, "20", 540, 1120),
            wheel_row(2, "21", 540, 1200),
            wheel_row(3, "22", 540, 1280),
        ];
        let column = WheelColumn::from_elements(&elements, &elements[1]).expect("a column");
        let pinch = pinch_on_column(&column, 8, 80, 1.0, false).expect("a drag");
        // A forward drag starts low and moves up, down the column's centre.
        assert_eq!(pinch.from.0, 540);
        assert_eq!(pinch.to.0, 540);
        assert!(pinch.to.1 < pinch.from.1, "{pinch:?}");
        // Backwards is the other way round.
        let back = pinch_on_column(&column, -8, 80, 1.0, false).expect("a drag");
        assert!(back.to.1 > back.from.1, "{back:?}");
    }
    #[test]
    fn a_short_drag_is_dispatched_slowly_so_it_is_not_a_tap() {
        let elements = vec![
            wheel_row(1, "20", 540, 1120),
            wheel_row(2, "21", 540, 1200),
            wheel_row(3, "22", 540, 1280),
        ];
        let column = WheelColumn::from_elements(&elements, &elements[1]).expect("a column");
        let tiny = pinch_on_column(&column, 1, 80, 0.2, false).expect("a drag");
        assert!(tiny.slow, "a drag at the minimum distance must not flick");
    }
    #[test]
    fn a_drag_names_the_device_it_is_for() {
        let elements = vec![
            wheel_row(1, "20", 540, 1120),
            wheel_row(2, "21", 540, 1200),
            wheel_row(3, "22", 540, 1280),
        ];
        let column = WheelColumn::from_elements(&elements, &elements[1]).expect("a column");
        let pinch = pinch_on_column(&column, 2, 80, 1.0, false).expect("a drag");
        let pinned = pinch.input_args(&Some("emulator-5554".to_string()));
        assert_eq!(pinned["device"], "emulator-5554");
        assert_eq!(pinned["verify"], false);
        let unpinned = pinch.input_args(&None);
        assert!(unpinned.get("device").is_none());
    }
    #[test]
    fn a_value_argument_is_required_and_bounded() {
        assert!(wheel_value(&serde_json::json!({})).is_err());
        assert!(wheel_value(&serde_json::json!({ "value": "  " })).is_err());
        assert_eq!(wheel_value(&serde_json::json!({ "value": " 9 " })).expect("9"), "9");
        let long = "x".repeat(61);
        assert!(wheel_value(&serde_json::json!({ "value": long })).is_err());
    }
    #[test]
    fn a_failure_says_what_the_wheel_shows_now() {
        let outcome = WheelOutcome {
            reached: false,
            value: Some("15".to_string()),
            from: Some("10".to_string()),
            method: "drag",
            attempts: 3,
            error: Some("wheel_not_moving"),
            hint: None,
        };
        let text = wheel_failure_text(&outcome, "20");
        assert!(text.contains("Could not set the wheel to \"20\""), "{text}");
        assert!(text.contains("did not move"), "{text}");
        assert!(text.contains("It shows \"15\" now"), "{text}");
    }
    #[test]
    fn every_way_to_stop_names_a_code_the_schema_can_explain() {
        use crate::computer::platform::android::wheel::WheelStop;
        let tool = AdbSetWheelTool::new();
        let description = tool.description();
        let fail = |code: &'static str| {
            wheel_failure_text(
                &WheelOutcome {
                    reached: false,
                    value: None,
                    from: None,
                    method: "none",
                    attempts: 0,
                    error: Some(code),
                    hint: None,
                },
                "1",
            )
        };
        // The five codes the tool advertises are exactly the ones its stops can
        // report — a code the model is told about but never sees is noise, and
        // a code it sees but was not told about is uninterpretable.
        let advertised = [
            "not_a_wheel",
            "wheel_unreadable",
            "wheel_at_end",
            "wheel_not_moving",
            "wheel_not_reached",
        ];
        for code in advertised {
            assert!(description.contains(code), "{code} is not in the description");
        }
        let stops = [
            WheelStop::AtEnd,
            WheelStop::NotMoving,
            WheelStop::Cap,
            WheelStop::Overshoot,
            WheelStop::UnknownValue,
        ];
        for stop in stops {
            assert!(
                advertised.contains(&stop.code()),
                "{:?} reports a code the schema does not explain",
                stop
            );
        }
        // Each code gets its own sentence: a caller that sees two codes must
        // not be told the same thing twice, or the distinction is cosmetic.
        let mut seen: Vec<(&'static str, String)> = Vec::new();
        for code in advertised {
            let text = fail(code);
            assert!(text.starts_with("Could not set the wheel"), "{code}: {text}");
            if let Some((other, _)) = seen.iter().find(|(_, prev)| *prev == text) {
                panic!("{code} and {other} explain a failure the same way: {text}");
            }
            seen.push((code, text));
        }
        // `Cap` and `Overshoot` are the two stops that share a code on purpose:
        // both mean "the action route cannot get there", and the next thing to
        // try is a drag either way.
        assert_eq!(WheelStop::Cap.code(), WheelStop::Overshoot.code());
    }
}
