//! Several known steps in one call, each one still judged.

use super::*;

/// The most steps one `android_run` may take.
///
/// A step is a full turn's worth of work — find the target, act, read the
/// screen back — and past about thirty of them one tool call is no longer
/// something a reader can follow when it goes wrong. The same ceiling
/// `android_input action=sequence` puts on a chain of taps.
pub const RUN_MAX_STEPS: usize = super::MAX_SEQUENCE_STEPS;

/// The most steps a run may take when every read costs a `uiautomator` dump.
///
/// The in-process accessibility snapshot is one bridge call and answers in
/// milliseconds. `uiautomator dump` is a subprocess plus an `/sdcard` round
/// trip, and a run reads at least twice per step — once to find the target,
/// once to see what happened. Ten steps on that path already cost about what
/// thirty do in-process, so the ceiling follows the read path instead of
/// pretending the two are the same. The result says which path it was on.
pub const RUN_MAX_STEPS_FALLBACK: usize = 10;

/// The most characters one `clear` will backspace.
///
/// A field holding a book is not what "clear" is for, and every key is a real
/// key event on the phone: after a point, selecting everything and deleting is
/// the honest answer, and the model can ask for that itself.
const RUN_CLEAR_MAX_KEYS: usize = 200;

/// The most a step's own `wait` may take.
const RUN_INLINE_WAIT_MS: u64 = 10_000;

/// Why a run stopped before its last command.
///
/// Every one of these is a fact about the phone, not about the step: the step
/// may well have worked, and the next one is what would be wrong.
pub const STOP_APP_SWITCHED: &str = "app_switched";
pub const STOP_OPENED_ELSEWHERE: &str = "opened_elsewhere";
pub const STOP_SCREEN_NEEDS_REVIEW: &str = "screen_needs_review";
pub const STOP_AMBIGUOUS_TARGET: &str = "ambiguous_target";
pub const STOP_TARGET_NOT_FOUND: &str = "target_not_found";
pub const STOP_FAILED: &str = "failed";
pub const STOP_CONDITION_UNMET: &str = "condition_unmet";
pub const STOP_FINAL_READ_FAILED: &str = "final_read_failed";
/// A person declined the run's plan, so none of its steps ran.
pub const STOP_PLAN_REFUSED: &str = "plan_refused";
/// The run asked for more steps than the slow read path can afford.
pub const STOP_STEP_BUDGET: &str = "step_budget_exceeded";

/// A step that did not do what it was asked, with the reason in the run's own
/// vocabulary so the caller can act on the code rather than parse a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StepError {
    code: &'static str,
    detail: String,
}

impl StepError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self { code, detail: detail.into() }
    }
}

/// One command in a run, as it turned out.
#[derive(Debug, Clone, PartialEq)]
pub struct RunStep {
    /// 1-based, so it matches how the model numbers things.
    pub n: usize,
    pub op: String,
    /// Ran and had its intended effect.
    pub ok: bool,
    /// Ran at all: `false` when the run stopped before reaching it.
    pub executed: bool,
    /// What the screen did afterwards, when the step changed it.
    pub effect: Option<String>,
    /// The machine code for a failure, one of the `STOP_*` constants.
    pub code: Option<String>,
    /// Why it did not do what it was asked, in words.
    pub error: Option<String>,
}

impl RunStep {
    fn new(n: usize, op: &str) -> Self {
        Self {
            n,
            op: op.to_string(),
            ok: false,
            executed: false,
            effect: None,
            code: None,
            error: None,
        }
    }

    fn executed(mut self, effect: Option<&str>) -> Self {
        self.executed = true;
        self.ok = true;
        self.effect = effect.map(str::to_string);
        self
    }

    fn failed(mut self, failure: &StepError) -> Self {
        self.executed = true;
        self.code = Some(failure.code.to_string());
        self.error = Some(failure.detail.clone());
        self
    }

    /// A step that was never reached, and why.
    fn not_run(mut self, reason: &str) -> Self {
        self.executed = false;
        self.ok = false;
        self.error = Some(reason.to_string());
        self
    }

    /// Mark a step that ran but whose aftermath says the run must not go on.
    ///
    /// The step itself may have worked — the reason is about the phone — so the
    /// code and sentence are attached without touching `executed`.
    fn fail_in_place(&mut self, failure: &StepError) {
        self.ok = false;
        self.code = Some(failure.code.to_string());
        self.error = Some(failure.detail.clone());
    }

    fn describe(&self) -> String {
        let state = if !self.executed {
            "not run"
        } else if self.ok {
            "ok"
        } else {
            "failed"
        };
        let mut line = format!("{}. {} — {state}", self.n, self.op);
        if let Some(effect) = &self.effect {
            line.push_str(&format!(", effect={effect}"));
        }
        if let Some(error) = &self.error {
            line.push_str(&format!(": {error}"));
        }
        line
    }
}

/// What a step may name as its target.
///
/// The same three criteria `android_query` takes, because that is how a step
/// finds its target again on the screen it is actually on — not by the index
/// it had in a read three steps ago.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StepTarget {
    text: Option<String>,
    resource_id: Option<String>,
    class: Option<String>,
}

impl StepTarget {
    fn from_command(command: &Value) -> Option<Self> {
        let field = |key: &str| {
            command
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let target = Self {
            text: field("targetText"),
            resource_id: field("resourceId"),
            class: field("class"),
        };
        (target.text.is_some() || target.resource_id.is_some() || target.class.is_some())
            .then_some(target)
    }

    fn query(&self) -> Option<super::query::Query> {
        super::query::Query::new(
            self.text.as_deref(),
            self.resource_id.as_deref(),
            self.class.as_deref(),
        )
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(text) = &self.text {
            parts.push(format!("text {text:?}"));
        }
        if let Some(id) = &self.resource_id {
            parts.push(format!("resourceId {id:?}"));
        }
        if let Some(class) = &self.class {
            parts.push(format!("class {class:?}"));
        }
        parts.join(" + ")
    }
}

/// Locate a step's target on the screen as it is now.
///
/// The distinction that matters is between "nothing matched" and "several
/// matched": tapping the first of two identical rows is the silent
/// wrong-target failure, so ambiguity stops the run with the candidates named
/// rather than picking one.
fn locate_step_target(
    dump: &UiDump,
    target: &StepTarget,
    editable: bool,
) -> Result<UiElement, StepError> {
    let Some(query) = target.query() else {
        return Err(StepError::new(
            STOP_TARGET_NOT_FOUND,
            "the step names no targetText, resourceId or class",
        ));
    };
    let mut candidates: Vec<&UiElement> = dump
        .elements
        .iter()
        .filter(|element| query.matches(element))
        .filter(|element| element.enabled)
        .collect();
    // A write needs a field and a tap needs something to press. When the query
    // already names exactly one node, that node is the answer even if the
    // class was not one this code knows — the model can see more than the
    // class table does.
    if editable {
        let fields: Vec<&UiElement> = candidates
            .iter()
            .copied()
            .filter(|e| e.state.editable || e.state.password)
            .collect();
        if !fields.is_empty() {
            candidates = fields;
        }
    } else {
        let clickable: Vec<&UiElement> =
            candidates.iter().copied().filter(|e| e.clickable).collect();
        if !clickable.is_empty() {
            candidates = clickable;
        }
    }
    // An exact label wins over a superset: "发送" should not be ambiguous
    // because "发送图片" is also on the screen.
    if let Some(exact) = target.text.as_deref() {
        let exact_hits: Vec<&UiElement> = candidates
            .iter()
            .copied()
            .filter(|element| {
                element_label(element).is_some_and(|label| label.eq_ignore_ascii_case(exact))
            })
            .collect();
        if !exact_hits.is_empty() {
            candidates = exact_hits;
        }
    }
    match candidates.as_slice() {
        [] => Err(StepError::new(
            STOP_TARGET_NOT_FOUND,
            format!(
                "nothing on the screen matches {} ({} element(s) were read)",
                target.describe(),
                dump.elements.len()
            ),
        )),
        [one] => Ok((*one).clone()),
        many => {
            let names: Vec<String> = many
                .iter()
                .take(6)
                .map(|element| {
                    format!(
                        "[{}] {} {:?}",
                        element.index,
                        element.class,
                        element_label(element).unwrap_or("-")
                    )
                })
                .collect();
            Err(StepError::new(
                STOP_AMBIGUOUS_TARGET,
                format!(
                    "{} elements match {}{} — narrow it with resourceId or class, or take one \
                     of: {}",
                    many.len(),
                    target.describe(),
                    if dump.truncated {
                        " (and the tree was truncated, so this is not the whole screen)"
                    } else {
                        ""
                    },
                    names.join("; ")
                ),
            ))
        }
    }
}

/// An `android_input` call pinned to this run's device and plan.
fn input_tool(device: &Option<String>, plan: Option<&super::gate::Plan>) -> AdbInputTool {
    let tool = match device {
        Some(serial) => AdbInputTool::new().with_device(serial.clone()),
        None => AdbInputTool::new(),
    };
    match plan {
        Some(plan) => tool.under_plan(plan.clone()),
        None => tool,
    }
}

/// An `android_app_manager` call pinned to this run's device.
fn app_manager_tool(device: &Option<String>) -> AdbAppManagerTool {
    match device {
        Some(serial) => AdbAppManagerTool::new().with_device(serial.clone()),
        None => AdbAppManagerTool::new(),
    }
}

/// An `android_wait` call pinned to this run's device.
fn wait_tool(device: &Option<String>) -> AdbWaitTool {
    match device {
        Some(serial) => AdbWaitTool::new().with_device(serial.clone()),
        None => AdbWaitTool::new(),
    }
}

/// The `data` object the run's arguments carry, with the device and display
/// naming the target.
fn step_args(device: &Option<String>, display: Option<i64>) -> serde_json::Map<String, Value> {
    let mut args = serde_json::Map::new();
    if let Some(serial) = device {
        args.insert("device".to_string(), Value::String(serial.clone()));
    }
    if let Some(display) = display {
        args.insert("display".to_string(), Value::Number(display.into()));
    }
    args
}

/// One point a run step names, in pixels.
///
/// A step written in the 0–999 space is a fraction of the screen the step acts
/// on, and that screen has to be read to learn its size. The caller does that
/// read once per step (see [`step_coordinate_space`]) and passes it in, so a
/// swipe's two ends share one read rather than paying for one each.
fn adopted_point(
    command: &Value,
    keys: super::coords::Keys,
    size: Option<(i32, i32)>,
) -> Result<(i32, i32), StepError> {
    let names_point = [keys.x, keys.y, keys.x_pct, keys.y_pct]
        .iter()
        .any(|key| command.get(*key).is_some());
    if !names_point {
        // The step named no point at all. `coords::resolve` would answer the
        // origin, but a swipe written without one is a mistake, not a zero.
        return Err(StepError::new(
            STOP_FAILED,
            format!(
                "swipe needs `{}` / `{}` (or `{}` / `{}`)",
                keys.x, keys.y, keys.x_pct, keys.y_pct
            ),
        ));
    }
    super::coords::resolve(command, keys, size)
        .map(|point| point.pixels())
        .map_err(|refused| StepError::new(STOP_FAILED, refused))
}

/// The screen a step's 0–999 points are a fraction of, when it names any.
///
/// `None` for a step that stays in pixels: it costs no read, which is what
/// keeps every existing `android_run` step exactly as cheap as it was. A step
/// that does name a percent pays for one read — the size is not obtainable any
/// other way for a virtual display, and `wm size` describes the wrong screen.
async fn step_coordinate_space(
    command: &Value,
    target: Target,
    device: &Option<String>,
) -> Result<Option<(i32, i32)>, StepError> {
    if !super::coords::uses_percent(command) {
        return Ok(None);
    }
    let dump = dump_ui(device, target).await.map_err(|failure| {
        StepError::new(
            STOP_FAILED,
            format!(
                "could not read the screen to turn the 0–999 point into pixels: {}",
                failure.message
            ),
        )
    })?;
    Ok(super::coordinate_space(device, target, dump.screen).await)
}

/// Run one delegated tool and turn a refusal into a step error.
async fn call_tool(
    result: crate::Result<ToolExecutionResult>,
    op: &str,
) -> Result<ToolExecutionResult, StepError> {
    match result {
        Ok(result) if result.success => Ok(result),
        Ok(result) => Err(StepError::new(STOP_FAILED, result.error.unwrap_or(result.output))),
        Err(error) => Err(StepError::new(STOP_FAILED, format!("{op} could not run: {error}"))),
    }
}

/// Read `effect` out of a delegated tool's result.
fn effect_of(result: &ToolExecutionResult) -> Option<String> {
    result
        .data
        .as_ref()
        .and_then(|data| data.get("effect"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The key a display action names, as the caller wrote it.
pub(crate) fn described_keycode(args: &Value) -> &str {
    args.get("keycode")
        .and_then(|v| v.as_str())
        .unwrap_or("BACK")
}

/// The `Target` a step addresses.
fn step_target(display: Option<i64>) -> Target {
    match display.and_then(|id| i32::try_from(id).ok()) {
        Some(id) => Target::Display(id),
        None => Target::Screen,
    }
}

/// Execute one command against a screen and say what happened.
///
/// Every op goes through the tool that already implements it, so a step cannot
/// do anything a single call could not: the repetition ladder, the approval
/// gate, the app lease and the safety net all still apply, one step at a time.
/// What a run saves is the model's turns, not the phone's checks.
async fn run_step(
    context: &ToolContext,
    device: &Option<String>,
    display: Option<i64>,
    plan: Option<&super::gate::Plan>,
    command: &Value,
) -> Result<Option<String>, StepError> {
    let op = command.get("op").and_then(Value::as_str).unwrap_or("");
    let target = step_target(display);

    match op {
        "launch" => {
            let package = command
                .get("package")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| StepError::new(STOP_FAILED, "launch needs `package`"))?;
            let mut args = step_args(device, None);
            args.insert("action".to_string(), Value::String("launch".to_string()));
            args.insert("package".to_string(), Value::String(package.to_string()));
            if let Some(activity) = command.get("activity").and_then(Value::as_str) {
                args.insert("activity".to_string(), Value::String(activity.to_string()));
            }
            let result = call_tool(
                app_manager_tool(device)
                    .execute(Value::Object(args), context)
                    .await,
                "launch",
            )
            .await?;
            // `am start -W` already waited for the activity to report idle, so
            // the screen this reports is the app's first frame rather than
            // whatever was there before it.
            Ok(effect_of(&result).or_else(|| Some("launched".to_string())))
        }
        "tap" | "type" | "clear" => {
            // Each of these needs a fresh read to find its target, and that
            // read is also the picture the effect is judged against — so the
            // step reads once here and hands the input tool a point. The input
            // tool's own safety net works on indices from *its* read; a point
            // is what a run legitimately has after locating by label.
            let Some(target_spec) = StepTarget::from_command(command) else {
                return Err(StepError::new(
                    STOP_TARGET_NOT_FOUND,
                    format!("{op} needs targetText, resourceId or class"),
                ));
            };
            let dump = dump_ui(device, target).await.map_err(|failure| {
                StepError::new(
                    STOP_FAILED,
                    format!("could not read the screen: {}", failure.message),
                )
            })?;
            let editable = op != "tap";
            let element = locate_step_target(&dump, &target_spec, editable)?;
            let (x, y) = element.center;

            let mut tap_args = step_args(device, display);
            tap_args.insert("action".to_string(), Value::String("tap".to_string()));
            tap_args.insert("x".to_string(), Value::Number(x.into()));
            tap_args.insert("y".to_string(), Value::Number(y.into()));
            if op != "tap" {
                // For a write the tap only moves focus; the text step below is
                // what confirms the screen, so the two extra reads a tap's own
                // effect check would take are pure cost.
                tap_args.insert("verify".to_string(), Value::Bool(false));
            }
            let tapped = call_tool(
                input_tool(device, plan)
                    .execute(Value::Object(tap_args), context)
                    .await,
                op,
            )
            .await?;
            if op == "tap" {
                return Ok(effect_of(&tapped));
            }

            let mut last_effect = effect_of(&tapped);
            if op == "clear" {
                let length = element
                    .text
                    .as_deref()
                    .map(str::chars)
                    .map(Iterator::count)
                    .unwrap_or(0)
                    .min(RUN_CLEAR_MAX_KEYS);
                if length > 0 {
                    // `input keyevent` takes several codes at once, so the whole
                    // clear is one dispatch rather than one per character —
                    // which also stops the repetition ladder from reading it as
                    // sixty identical actions.
                    let mut clear_args = step_args(device, display);
                    clear_args.insert("action".to_string(), Value::String("key".to_string()));
                    clear_args.insert(
                        "keycode".to_string(),
                        Value::String(vec!["KEYCODE_DEL"; length].join(" ")),
                    );
                    let cleared = call_tool(
                        input_tool(device, plan)
                            .execute(Value::Object(clear_args), context)
                            .await,
                        "clear",
                    )
                    .await?;
                    last_effect = effect_of(&cleared).or(last_effect);
                }
                return Ok(last_effect);
            }

            let text = command
                .get("text")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| StepError::new(STOP_FAILED, "type needs `text`"))?;
            let mut type_args = step_args(device, display);
            type_args.insert("action".to_string(), Value::String("text".to_string()));
            type_args.insert("text".to_string(), Value::String(text.to_string()));
            let typed = call_tool(
                input_tool(device, plan)
                    .execute(Value::Object(type_args), context)
                    .await,
                "type",
            )
            .await?;
            Ok(effect_of(&typed).or(last_effect))
        }
        "back" | "key" => {
            let keycode = if op == "back" {
                "KEYCODE_BACK".to_string()
            } else {
                command
                    .get("keycode")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| StepError::new(STOP_FAILED, "key needs `keycode`, e.g. ENTER"))?
                    .to_string()
            };
            let mut args = step_args(device, display);
            args.insert("action".to_string(), Value::String("key".to_string()));
            args.insert("keycode".to_string(), Value::String(keycode));
            let result = call_tool(
                input_tool(device, plan)
                    .execute(Value::Object(args), context)
                    .await,
                op,
            )
            .await?;
            Ok(effect_of(&result))
        }
        "swipe" => {
            // The step's own points may be a percent of the screen the step is
            // acting on; the size is read once and shared by both ends.
            let space = step_coordinate_space(command, target, device).await?;
            let from = adopted_point(command, super::coords::TAP, space)?;
            let to = adopted_point(command, super::coords::END, space)?;
            let mut args = step_args(device, display);
            args.insert("action".to_string(), Value::String("swipe".to_string()));
            args.insert("x".to_string(), Value::Number(from.0.into()));
            args.insert("y".to_string(), Value::Number(from.1.into()));
            args.insert("x2".to_string(), Value::Number(to.0.into()));
            args.insert("y2".to_string(), Value::Number(to.1.into()));
            let result = call_tool(
                input_tool(device, plan)
                    .execute(Value::Object(args), context)
                    .await,
                op,
            )
            .await?;
            Ok(effect_of(&result))
        }
        "scroll" => {
            let direction = Direction::parse(command.get("direction").and_then(Value::as_str))
                .map_err(|message| StepError::new(STOP_FAILED, message))?;
            let amount = command
                .get("amount")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, 20);
            let mut last = None;
            for _ in 0..amount {
                let dump = dump_ui(device, target).await.map_err(|failure| {
                    StepError::new(
                        STOP_FAILED,
                        format!("could not read the screen: {}", failure.message),
                    )
                })?;
                let size = screen_size(device, dump.screen).await.ok_or_else(|| {
                    StepError::new(STOP_FAILED, "the screen size is unknown, so it cannot scroll")
                })?;
                let container = scroll_container(&dump.elements, size, direction);
                let (from, to) = scroll_points(size, direction, container);
                let mut args = step_args(device, display);
                args.insert("action".to_string(), Value::String("swipe".to_string()));
                args.insert("x".to_string(), Value::Number(from.0.into()));
                args.insert("y".to_string(), Value::Number(from.1.into()));
                args.insert("x2".to_string(), Value::Number(to.0.into()));
                args.insert("y2".to_string(), Value::Number(to.1.into()));
                let result = call_tool(
                    input_tool(device, plan)
                        .execute(Value::Object(args), context)
                        .await,
                    "scroll",
                )
                .await?;
                let effect = effect_of(&result);
                // The end of the list is not a failure, but scrolling further
                // is wasted: report where the run got to and move on.
                if effect.as_deref() == Some(EFFECT_UNCHANGED) {
                    return Ok(effect);
                }
                last = effect;
            }
            Ok(last)
        }
        "wait" => {
            let Some(query) = StepTarget::from_command(command) else {
                return Err(StepError::new(
                    STOP_CONDITION_UNMET,
                    "wait needs targetText, resourceId or class",
                ));
            };
            let present = command
                .get("present")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let timeout = command
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .unwrap_or(5_000)
                .min(RUN_INLINE_WAIT_MS);
            let mut args = step_args(device, display);
            args.insert(
                "condition".to_string(),
                Value::String(if present { "appear" } else { "disappear" }.to_string()),
            );
            if let Some(text) = &query.text {
                args.insert("text".to_string(), Value::String(text.clone()));
            }
            if let Some(id) = &query.resource_id {
                args.insert("resource_id".to_string(), Value::String(id.clone()));
            }
            if let Some(class) = &query.class {
                args.insert("class".to_string(), Value::String(class.clone()));
            }
            args.insert("timeout_ms".to_string(), Value::Number(timeout.into()));
            let result = call_tool(
                wait_tool(device)
                    .execute(Value::Object(args), context)
                    .await,
                "wait",
            )
            .await?;
            let met = result
                .data
                .as_ref()
                .and_then(|data| data.get("met"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if met {
                Ok(None)
            } else {
                Err(StepError::new(
                    STOP_CONDITION_UNMET,
                    format!(
                        "{} did not happen within {timeout} ms",
                        if present { "appearing" } else { "disappearing" }
                    ),
                ))
            }
        }
        "assert" => {
            let Some(query) = StepTarget::from_command(command) else {
                return Err(StepError::new(
                    STOP_CONDITION_UNMET,
                    "assert needs targetText, resourceId or class",
                ));
            };
            let present = command
                .get("present")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let dump = dump_ui(device, target).await.map_err(|failure| {
                StepError::new(
                    STOP_FAILED,
                    format!("could not read the screen: {}", failure.message),
                )
            })?;
            let found = query
                .query()
                .is_some_and(|query| query.count(&dump.elements) > 0);
            if found == present {
                Ok(None)
            } else {
                Err(StepError::new(
                    STOP_CONDITION_UNMET,
                    format!(
                        "expected {} to be on the screen, and it is {}",
                        query.describe(),
                        if found { "" } else { "not" }
                    ),
                ))
            }
        }
        "observe" => Ok(None),
        other => Err(StepError::new(STOP_FAILED, format!("unknown op {other:?}"))),
    }
}

/// Why the screen is not the app the step asked for, when that is the cause.
///
/// Two interruptions are not "the wrong app": a chooser means the step wanted a
/// choice nobody made, and a platform prompt means a person has to answer. They
/// are reported with their own codes because each sends the caller to a
/// different move — answer the sheet, or hand the phone to its owner — while a
/// plain app switch just means the remaining steps were written for a screen
/// that is no longer there.
fn launch_interrupted(package: &str, step: usize) -> Option<StepError> {
    if super::takeover::is_chooser(package) {
        return Some(StepError::new(
            STOP_OPENED_ELSEWHERE,
            format!(
                "step {step} opened a chooser ({package}), so the steps after it would act on \
                 the sheet — answer it yourself (android_dialog), then start a new run"
            ),
        ));
    }
    if super::takeover::is_system_overlay(package) {
        return Some(StepError::new(
            STOP_SCREEN_NEEDS_REVIEW,
            format!(
                "the system put up a prompt ({package}) — a person has to answer it \
                 (android_hand_to_user), then the run can continue"
            ),
        ));
    }
    None
}

/// Every op a command list accepts.
const RUN_OPS: [&str; 11] = [
    "launch", "tap", "type", "clear", "scroll", "swipe", "key", "back", "wait", "assert", "observe",
];

/// How long a step waits for the phone to report a window change.
///
/// A transition's event is pushed while the step's own input is still settling,
/// so it is normally already queued when the step returns; the grace only
/// covers the lag. Waiting longer would tax every step on a screen that does
/// not move — exactly the case the per-step tree read was slow for.
const RUN_EVENT_GRACE_MS: u64 = 200;

/// What a step's aftermath looked like to the event stream.
pub(crate) enum WatchVerdict {
    /// The phone moved to this app, which is not the one the run is in.
    Moved(String),
    /// Still in the expected app (or no window change at all).
    Still,
    /// The stream cannot answer — the run must fall back to reading the tree.
    Unavailable,
}

/// The phone's own account of what changed, watched across a run.
///
/// With a push channel, a run no longer re-reads the tree after every step just
/// to learn whether the app changed: the window event says so, and it says so
/// without a bridge call. On a host without one (a desktop driving a phone over
/// ADB, or the service off) the watch is inert and the run keeps the per-step
/// read — a missing stream is never a reason to stop checking.
///
/// Events describe the user's own display, which is why a run against a virtual
/// display does not use this even when the channel exists.
pub(crate) struct EventWatch {
    receiver: Option<tokio::sync::broadcast::Receiver<crate::device::DeviceEvent>>,
    /// The stream fell behind or closed: what it said can no longer be
    /// trusted to be complete, so the run reads the tree instead.
    degraded: bool,
}

impl EventWatch {
    /// Attach to the event stream, if this device really has one.
    ///
    /// A channel existing is not enough to trust its silence: the phone must
    /// have the service on, and the running binding must deliver window
    /// changes, or "no event" would mean "nothing was ever sent" and the run
    /// would fail to notice an app switch. One status read per run pays for
    /// that, and only when a channel exists at all.
    async fn attach() -> Self {
        let Some(bridge) = crate::device::bridge().cloned() else {
            return Self::with_channel(None);
        };
        let Some(receiver) = bridge.events() else {
            return Self::with_channel(None);
        };
        if let Some(status) = read_a11y_status().await {
            if status.enabled == Some(false) {
                return Self::with_channel(None);
            }
            if let Some(mask) = status.subscribed_event_types {
                if mask & crate::device::EventKind::Window.subscription_bit() == 0 {
                    return Self::with_channel(None);
                }
            }
        }
        Self::with_channel(Some(receiver))
    }

    pub(super) fn with_channel(
        mut receiver: Option<tokio::sync::broadcast::Receiver<crate::device::DeviceEvent>>,
    ) -> Self {
        // Anything already queued happened before the run started and is not
        // this run's business.
        if let Some(channel) = receiver.as_mut() {
            while channel.try_recv().is_ok() {}
        }
        Self { receiver, degraded: false }
    }

    /// Whether this watch can answer at all.
    pub(super) fn available(&self) -> bool {
        self.receiver.is_some() && !self.degraded
    }

    /// The next window event from an app other than `expected`, within `grace`.
    ///
    /// "Another app" is the whole test: a new activity of the *same* package is
    /// still the run's screen, and the next step's own read decides whether its
    /// targets survived. A chooser and a system prompt come through as their
    /// own packages, so the caller can tell them apart by name.
    pub(super) async fn moved(
        &mut self,
        expected: Option<&str>,
        grace: std::time::Duration,
    ) -> WatchVerdict {
        use tokio::sync::broadcast::error::RecvError;

        let Some(receiver) = self.receiver.as_mut() else {
            return WatchVerdict::Unavailable;
        };
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            tokio::select! {
                incoming = receiver.recv() => match incoming {
                    Ok(event) => {
                        if event.kind != crate::device::EventKind::Window
                            || event.package.is_empty()
                            || expected.is_some_and(|package| package == event.package)
                        {
                            continue;
                        }
                        return WatchVerdict::Moved(event.package);
                    }
                    // The subscriber fell behind: an event that would have
                    // answered is gone, so the stream stops being evidence.
                    Err(RecvError::Lagged(_)) => {
                        self.degraded = true;
                        return WatchVerdict::Unavailable;
                    }
                    Err(RecvError::Closed) => {
                        self.receiver = None;
                        return WatchVerdict::Unavailable;
                    }
                },
                _ = tokio::time::sleep_until(deadline) => return WatchVerdict::Still,
            }
        }
    }
}

/// Run several commands in one call, re-locating each target first.
#[derive(Debug)]
pub struct AdbRunTool {
    device: Option<String>,
}

impl Default for AdbRunTool {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbRunTool {
    pub fn new() -> Self {
        Self { device: None }
    }

    pub fn with_device(mut self, device: String) -> Self {
        self.device = Some(device);
        self
    }
}

#[async_trait]
impl Tool for AdbRunTool {
    fn name(&self) -> &str {
        "android_run"
    }

    fn description(&self) -> &str {
        "Run up to 30 phone commands in one call, finding each target on the live screen before \
         acting: one conversation turn instead of one per tap. Each step is a full read → find → \
         act → check cycle, so the result says what the screen did after every step. It stops \
         early when the app changes, an action fails, a target is missing or ambiguous, or the \
         step's condition is not met — and says which step and why. Every step still goes through \
         the same risk checks, approval gate and repetition guard that a single android_input \
         call does. Prefer this over a chain of single calls when the steps are known in advance; \
         use single calls when the next step depends on what you see."
    }

    fn parameters_schema(&self) -> Value {
        create_schema(
            "Run several phone commands in one call",
            serde_json::json!({
                "commands": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": RUN_MAX_STEPS,
                    "description": "Ordered steps. Each target is found again on the current screen before acting.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": {
                                "type": "string",
                                "enum": ["launch", "tap", "type", "clear", "scroll", "swipe", "key", "back", "wait", "assert", "observe"],
                                "description": "What to do."
                            },
                            "targetText": {
                                "type": "string",
                                "description": "Substring of the target's text or content description."
                            },
                            "resourceId": {
                                "type": "string",
                                "description": "Substring of the target's resource id."
                            },
                            "class": {
                                "type": "string",
                                "description": "Substring of the target's class."
                            },
                            "text": { "type": "string", "description": "For 'type': the text to enter." },
                            "package": { "type": "string", "description": "For 'launch': the app to start." },
                            "activity": { "type": "string", "description": "For 'launch': an explicit activity class." },
                            "direction": {
                                "type": "string",
                                "enum": ["up", "down"],
                                "description": "For 'scroll'."
                            },
                            "amount": {
                                "type": "integer",
                                "description": "For 'scroll': how many screenfuls (default 1, max 20)."
                            },
                            "x": { "type": "integer", "description": "For 'swipe': start x, in pixels." },
                            "y": { "type": "integer", "description": "For 'swipe': start y, in pixels." },
                            "x2": { "type": "integer", "description": "For 'swipe': end x, in pixels." },
                            "y2": { "type": "integer", "description": "For 'swipe': end y, in pixels." },
                            "x_pct": {
                                "type": "integer",
                                "minimum": 0,
                                "maximum": 999,
                                "description": "For 'swipe': start x as 0–999 across the screen, when its size is not known."
                            },
                            "y_pct": {
                                "type": "integer",
                                "minimum": 0,
                                "maximum": 999,
                                "description": "For 'swipe': start y as 0–999 down the screen."
                            },
                            "x2_pct": {
                                "type": "integer",
                                "minimum": 0,
                                "maximum": 999,
                                "description": "For 'swipe': end x as 0–999."
                            },
                            "y2_pct": {
                                "type": "integer",
                                "minimum": 0,
                                "maximum": 999,
                                "description": "For 'swipe': end y as 0–999."
                            },
                            "keycode": {
                                "type": "string",
                                "description": "For 'key': the Android keycode name, e.g. ENTER."
                            },
                            "timeoutMs": {
                                "type": "integer",
                                "description": "For 'wait': how long to wait (default 5000, max 10000)."
                            },
                            "present": {
                                "type": "boolean",
                                "description": "For 'wait' and 'assert': whether the target should be there (default true)."
                            }
                        },
                        "required": ["op"]
                    }
                },
                "display": { "type": "integer", "description": DISPLAY_PARAM },
                "device": { "type": "string", "description": DEVICE_PARAM }
            }),
            vec!["commands"],
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
        let display = match target {
            Target::Display(id) => Some(i64::from(id)),
            Target::Screen => None,
        };
        let Some(commands) = args.get("commands").and_then(Value::as_array) else {
            return Ok(ToolExecutionResult::error("`commands` must be an array of steps"));
        };
        if commands.is_empty() {
            return Ok(ToolExecutionResult::error("`commands` needs at least one step"));
        }
        if commands.len() > RUN_MAX_STEPS {
            return Ok(ToolExecutionResult::error(format!(
                "`commands` takes at most {RUN_MAX_STEPS} steps, got {} — split the work into \
                 smaller runs and check the screen between them",
                commands.len()
            )));
        }
        if let Some(bad) = commands.iter().find(|command| {
            command
                .get("op")
                .and_then(Value::as_str)
                .is_none_or(|op| !RUN_OPS.contains(&op))
        }) {
            return Ok(ToolExecutionResult::error(format!(
                "every step needs an `op`, one of {}; got {bad}",
                RUN_OPS.join(", ")
            )));
        }

        // The first read is the baseline: the app the run started in is what
        // "the app changed" is measured against, and the path it was read
        // through decides how many steps this run can afford.
        let opening = match dump_ui(&device, target).await {
            Ok(dump) => dump,
            Err(failure) => {
                return Ok(ToolExecutionResult::error(format!(
                    "could not read the screen to start the run: {}",
                    failure.message
                ))
                .with_data(serde_json::json!({ "code": failure.code })))
            }
        };
        let source = opening.source;
        if source == "uiautomator" && commands.len() > RUN_MAX_STEPS_FALLBACK {
            return Ok(ToolExecutionResult::error(format!(
                "This phone is being read through `uiautomator`, which costs a subprocess and an \
                 SD-card round trip per read, so a run is limited to {RUN_MAX_STEPS_FALLBACK} \
                 steps (got {}). Turn Syscity's accessibility service on for the cheap path, or \
                 split this into runs of at most {RUN_MAX_STEPS_FALLBACK}.",
                commands.len()
            ))
            .with_data(serde_json::json!({
                "code": STOP_STEP_BUDGET,
                "read_source": source,
                "max_steps": RUN_MAX_STEPS_FALLBACK,
            })));
        }

        let mut expected_package = opening.package.clone();
        // The run's irreversible steps, named before any of them runs.
        //
        // A run cannot ask about each step — thirty questions, each with less
        // context than the plan — so its plan is put to a person once, here,
        // and every step that was named then runs on that yes. A step the plan
        // did not name still meets the ordinary gate below.
        let planned = super::gate::plan_steps(commands);
        let plan =
            match super::gate::approve_plan(planned, context, opening.package.as_deref()).await {
                Ok(plan) => plan,
                Err(refusal) => {
                    return Ok(ToolExecutionResult::error(refusal.reason).with_data(
                        serde_json::json!({
                            "code": STOP_PLAN_REFUSED,
                            "steps_requested": commands.len(),
                        }),
                    ))
                }
            };
        let mut steps: Vec<RunStep> = Vec::with_capacity(commands.len());
        let mut stop: Option<(String, usize)> = None;
        let mut unreadable = 0_usize;
        // The event stream answers "did the app change" for a run on the user's
        // display; everywhere else it is inert and the per-step read does.
        let mut watch = match target {
            Target::Screen => EventWatch::attach().await,
            Target::Display(_) => EventWatch::with_channel(None),
        };
        let mut checked_by_events = 0_usize;

        for (index, command) in commands.iter().enumerate() {
            let n = index + 1;
            let op = command
                .get("op")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            let mut step = match run_step(context, &device, display, Some(&plan), command).await {
                Ok(effect) => RunStep::new(n, &op).executed(effect.as_deref()),
                Err(failure) => {
                    let code = failure.code.to_string();
                    steps.push(RunStep::new(n, &op).failed(&failure));
                    stop = Some((code, n));
                    break;
                }
            };

            if op == "observe" {
                steps.push(step);
                continue;
            }

            // A `launch` is *supposed* to change the app, so it is the one step
            // whose aftermath cannot be judged against the app it started in.
            // The new app is checked for the two ways a launch is not what was
            // asked for — a chooser it cannot answer, a prompt only a person
            // can — and otherwise becomes the baseline for the steps after it.
            if op == "launch" {
                let launched = match watch.available() {
                    true => match watch
                        .moved(
                            expected_package.as_deref(),
                            Duration::from_millis(RUN_EVENT_GRACE_MS),
                        )
                        .await
                    {
                        WatchVerdict::Moved(package) => Some(package),
                        _ => None,
                    },
                    false => None,
                };
                let launch_read = match launched {
                    Some(package) => Some(package),
                    None => dump_ui(&device, target)
                        .await
                        .ok()
                        .and_then(|after| after.package),
                };
                match launch_read {
                    Some(now) => {
                        if let Some(failure) = launch_interrupted(&now, n) {
                            step.fail_in_place(&failure);
                            stop = Some((failure.code.to_string(), n));
                            steps.push(step);
                            break;
                        }
                        expected_package = Some(now);
                        steps.push(step);
                    }
                    None => {
                        // A launch that cannot be read back changed nothing for
                        // this run's purposes; the next step's own read will say
                        // where it got to. Three in a row is a dead phone.
                        unreadable += 1;
                        if unreadable >= 3 {
                            let failure = StepError::new(
                                STOP_SCREEN_NEEDS_REVIEW,
                                "the screen stopped being readable for three steps in a row",
                            );
                            step.fail_in_place(&failure);
                            stop = Some((STOP_SCREEN_NEEDS_REVIEW.to_string(), n));
                            steps.push(step);
                            break;
                        }
                        steps.push(step);
                    }
                }
                continue;
            }

            // After an acting step, ask the phone whether the run can go on.
            // Doing this per step is what makes the next step's read safe: the
            // whole premise of a run is that each target is found again, and a
            // different app is a different set of targets.
            //
            // The event stream answers that question — "the app changed" —
            // without a bridge call. When it answered, this loop takes no read
            // at all: content changes are already in the step's own result, and
            // "the tree cannot describe this screen" is answered where it
            // matters, by the step whose target cannot be found
            // ([`locate_step_target`]).
            let moved_to = match watch
                .moved(expected_package.as_deref(), Duration::from_millis(RUN_EVENT_GRACE_MS))
                .await
            {
                WatchVerdict::Moved(package) => Some(package),
                WatchVerdict::Still | WatchVerdict::Unavailable => None,
            };
            let from_events = watch.available();
            if from_events {
                checked_by_events += 1;
            }
            let after: Option<UiDump> = if from_events {
                None
            } else {
                match dump_ui(&device, target).await {
                    Ok(dump) => {
                        unreadable = 0;
                        Some(dump)
                    }
                    Err(_) => {
                        unreadable += 1;
                        // One unreadable screen is a transition; three in a row
                        // is a phone that needs a person.
                        if unreadable >= 3 {
                            let failure = StepError::new(
                                STOP_SCREEN_NEEDS_REVIEW,
                                "the screen stopped being readable for three steps in a row",
                            );
                            step.fail_in_place(&failure);
                            stop = Some((STOP_SCREEN_NEEDS_REVIEW.to_string(), n));
                        }
                        steps.push(step);
                        if stop.is_some() {
                            break;
                        }
                        continue;
                    }
                }
            };

            // Every check below ends the run; the step it is attached to is
            // the one whose aftermath was caught. The app change itself comes
            // from the event stream when it could answer, and from this read
            // otherwise.
            let changed_app = moved_to.or_else(|| {
                match (
                    expected_package.as_deref(),
                    after.as_ref().and_then(|dump| dump.package.as_deref()),
                ) {
                    (Some(before), Some(now)) if before != now => Some(now.to_string()),
                    _ => None,
                }
            });
            let halted = changed_app.map(|now| match launch_interrupted(&now, n) {
                Some(opened) => opened,
                None => StepError::new(
                    STOP_APP_SWITCHED,
                    format!(
                        "the screen was in {} and is now in {now}, so the remaining steps no \
                         longer target the screen they were written for",
                        expected_package.as_deref().unwrap_or("another app")
                    ),
                ),
            });
            // A screen the tree cannot describe is a screen the next step's
            // locate cannot work on. Saying so now, with the app named, is more
            // useful than thirty identical failures.
            let halted = halted.or_else(|| {
                after.as_ref().and_then(|dump| {
                    (assess_tree(&dump.elements) == TreeQuality::Unusable).then(|| {
                        StepError::new(
                            STOP_SCREEN_NEEDS_REVIEW,
                            "the accessibility tree describes none of this screen — read the \
                             screenshot (android_observe) before continuing",
                        )
                    })
                })
            });
            // An action on a display can start something that lands on the
            // user's own screen: theirs stops being private, and this run stops
            // being about the display it was given.
            let halted = match halted {
                Some(halted) => Some(halted),
                None => match target {
                    Target::Display(id) => match display_bridge() {
                        Ok(bridge) => {
                            match super::takeover::check_spill(&bridge, id, None, false).await {
                                Some(spill) => Some(StepError::new(
                                    STOP_OPENED_ELSEWHERE,
                                    format!(
                                        "{} ended up on the user's screen ({}) rather than \
                                         display {id}",
                                        spill.package, spill.reason
                                    ),
                                )),
                                None => None,
                            }
                        }
                        Err(_) => None,
                    },
                    Target::Screen => None,
                },
            };

            match halted {
                Some(failure) => {
                    step.fail_in_place(&failure);
                    stop = Some((failure.code.to_string(), n));
                    steps.push(step);
                    break;
                }
                None => steps.push(step),
            }
        }

        // The commands the run never reached are still part of its shape: a
        // caller reading a stop needs to see what was left undone, not infer it
        // from a short list.
        if let Some((code, at)) = &stop {
            let why = format!("not run: the run stopped at step {at} ({code})");
            for (index, command) in commands.iter().enumerate().skip(*at) {
                let op = command.get("op").and_then(Value::as_str).unwrap_or("");
                steps.push(RunStep::new(index + 1, op).not_run(&why));
            }
        }

        // The last read is what the caller acts on next, so it is taken even
        // when the run reached its last command cleanly.
        let final_read = dump_ui(&device, target).await;
        let serial = resolved_serial(&device).await;
        let total = commands.len();
        let stopped = stop.as_ref().map(|(code, at)| (code.as_str(), *at));

        let (success, mut text) = match (&final_read, &stop) {
            (Ok(dump), None) => {
                let mut text = format!(
                    "Ran all {total} step(s) through the {source} read path:\n{}",
                    steps
                        .iter()
                        .map(RunStep::describe)
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                if source == "uiautomator" {
                    text.push_str(
                        "\n\nThis run read the screen through `uiautomator`, which is the slow \
                         path — turning Syscity's accessibility service on makes runs cheaper and \
                         longer.",
                    );
                }
                text.push_str(&app_change_note(checked_by_events));
                text.push_str("\n\nThe screen is now:\n\n");
                text.push_str(&format_ui_summary(&dump.elements));
                if dump.truncated {
                    text.push_str(
                        "\n\nThe tree was truncated (more nodes than one read carries), so \
                         elements below are missing.",
                    );
                }
                (true, text)
            }
            (_, Some((code, at))) => {
                let mut text = format!("Stopped at step {at} of {total}: {}.", stop_sentence(code));
                text.push_str(&format!(
                    "\n\n{}",
                    steps
                        .iter()
                        .map(RunStep::describe)
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
                match &final_read {
                    Ok(dump) => {
                        text.push_str("\n\nThe screen is now:\n\n");
                        text.push_str(&format_ui_summary(&dump.elements));
                    }
                    Err(failure) => text.push_str(&format!(
                        "\n\nThe screen could not be read afterwards ({}).",
                        failure.message
                    )),
                }
                (false, text)
            }
            (Err(failure), None) => (
                false,
                format!(
                    "Ran all {total} step(s), but the screen could not be read afterwards ({}).",
                    failure.message
                ),
            ),
        };

        let plan_note = plan.note();
        if let (true, Some(note)) = (success, plan_note.as_deref()) {
            text.push('\n');
            text.push('\n');
            text.push_str(note);
        }
        let result = if success {
            ToolExecutionResult::success(text)
        } else {
            ToolExecutionResult::error(text)
        };
        Ok(result.with_data(run_data(RunReport {
            steps: &steps,
            total,
            stopped,
            plan: &plan,
            final_read: final_read.as_ref().ok(),
            source,
            device: serial,
            checked_by_events,
        })))
    }
}

/// What a run did, as JSON, beside its text answer.
///
/// Grouped rather than passed one by one: a run reports a lot of small facts,
/// and a signature with eleven arguments is one nobody can read at the call
/// site.
struct RunReport<'a> {
    steps: &'a [RunStep],
    total: usize,
    stopped: Option<(&'a str, usize)>,
    plan: &'a super::gate::Plan,
    final_read: Option<&'a UiDump>,
    source: &'static str,
    device: Option<String>,
    checked_by_events: usize,
}

/// The JSON a run reports, beside its text answer.
fn run_data(report: RunReport<'_>) -> Value {
    let RunReport {
        steps,
        total,
        stopped,
        plan,
        final_read,
        source,
        device,
        checked_by_events,
    } = report;
    serde_json::json!({
        "ok": stopped.is_none(),
        "stopped": stopped.map(|(reason, step)| {
            serde_json::json!({ "reason": reason, "step": step })
        }),
        "completed": steps.iter().filter(|step| step.ok).count(),
        "total": total,
        "steps": steps.iter().map(|step| serde_json::json!({
            "n": step.n,
            "op": step.op,
            "ok": step.ok,
            "executed": step.executed,
            "effect": step.effect,
            "code": step.code,
            "error": step.error,
        })).collect::<Vec<_>>(),
        // Which reader the run worked through. `accessibility` is the phone's
        // own service and costs a bridge call per read; `uiautomator` is ADB's
        // dump and costs a subprocess.
        "read_source": source,
        // How many steps learned "the app changed" from the event stream
        // instead of a tree read. Zero means the per-step read was the only
        // evidence available on this phone.
        "app_change_source": if checked_by_events > 0 { "events" } else { "tree" },
        "checked_by_events": checked_by_events,
        "max_steps": if source == "uiautomator" { RUN_MAX_STEPS_FALLBACK } else { RUN_MAX_STEPS },
        "truncated": final_read.is_some_and(|dump| dump.truncated),
        "elements": final_read.map(|dump| dump.elements.clone()),
        // The irreversible steps the run named before it started, and whether
        // a person allowed them. Absent — no key at all — for a run with
        // nothing irreversible in it, which is the ordinary case.
        "plan": plan.note().map(|note| serde_json::json!({
            "note": note,
            "approved": plan.grant.is_some(),
            "steps": plan.planned.iter().map(|step| serde_json::json!({
                "what": step.what,
                "risk": step.risk.label(),
            })).collect::<Vec<_>>(),
        })),
        "device": device,
    })
}

/// What a run says about how it noticed app changes.
fn app_change_note(checked_by_events: usize) -> String {
    if checked_by_events == 0 {
        return String::new();
    }
    format!(
        "\n\nApp changes were watched through the phone's event stream for {checked_by_events} \
         step(s), so no tree was re-read just to notice one."
    )
}

/// What a stop reason means, in a sentence.
fn stop_sentence(code: &str) -> &'static str {
    match code {
        STOP_APP_SWITCHED => {
            "the screen changed app, so the rest of the steps no longer target the screen they \
             were written for"
        }
        STOP_OPENED_ELSEWHERE => "an app opened on the user's screen rather than this display",
        STOP_SCREEN_NEEDS_REVIEW => {
            "the accessibility tree cannot describe this screen — read it (the screenshot, or OCR) \
             before continuing"
        }
        STOP_AMBIGUOUS_TARGET => {
            "more than one element matched, so acting on the first would be a guess — narrow the \
             target with text, resourceId and class together"
        }
        STOP_TARGET_NOT_FOUND => "the target was not on the screen this step read",
        STOP_CONDITION_UNMET => "the screen was not in the state the step needed",
        STOP_FINAL_READ_FAILED => "the screen could not be read after the last step",
        STOP_STEP_BUDGET => "the run is longer than the slow read path can afford",
        _ => "a step failed",
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;

    #[test]
    fn a_step_target_reads_the_three_query_fields() {
        let target = StepTarget::from_command(&serde_json::json!({
            "op": "tap",
            "targetText": " 发送 ",
            "resourceId": "id/send",
            "class": "Button",
        }))
        .expect("a target");
        assert_eq!(target.text.as_deref(), Some("发送"));
        assert_eq!(target.resource_id.as_deref(), Some("id/send"));
        assert_eq!(target.class.as_deref(), Some("Button"));
        assert!(target.query().is_some());
    }
    #[test]
    fn a_step_without_a_target_has_none() {
        // Blank strings are not a target: `text: ""` means "any", which would
        // match the whole screen and tap the first thing on it.
        assert!(StepTarget::from_command(&serde_json::json!({
            "op": "tap",
            "targetText": "   ",
        }))
        .is_none());
        assert!(StepTarget::from_command(&serde_json::json!({ "op": "tap" })).is_none());
    }
    #[test]
    fn exactly_one_match_is_the_target() {
        let screen = dump(
            vec![
                element(1, "发送", "android.widget.Button", true, (900, 2100, 1050, 2200)),
                element(2, "取消", "android.widget.Button", true, (100, 2100, 250, 2200)),
            ],
            false,
        );
        let target = StepTarget::from_command(&serde_json::json!({ "targetText": "发送" }))
            .expect("a target");
        let found = locate_step_target(&screen, &target, false).expect("one match");
        assert_eq!(found.center, (975, 2150));
    }
    #[test]
    fn two_identical_matches_are_refused_not_guessed() {
        // Two rows that say the same thing: tapping the first is the silent
        // wrong-target failure, so the run stops and names both.
        let screen = dump(
            vec![
                element(1, "删除", "android.widget.Button", true, (0, 0, 100, 100)),
                element(2, "删除", "android.widget.Button", true, (0, 200, 100, 300)),
            ],
            false,
        );
        let target = StepTarget::from_command(&serde_json::json!({ "targetText": "删除" }))
            .expect("a target");
        let error = locate_step_target(&screen, &target, false).expect_err("ambiguous");
        assert_eq!(error.code, STOP_AMBIGUOUS_TARGET);
        assert!(error.detail.contains("2 elements"), "{}", error.detail);
    }
    #[test]
    fn an_exact_label_beats_a_longer_one_on_the_same_screen() {
        // "发送" and "发送图片" both contain the needle; the exact label is the
        // button, and the superset is decoration.
        let screen = dump(
            vec![
                element(1, "发送图片", "android.widget.Button", true, (0, 0, 100, 100)),
                element(2, "发送", "android.widget.Button", true, (800, 2100, 1000, 2200)),
            ],
            false,
        );
        let target = StepTarget::from_command(&serde_json::json!({ "targetText": "发送" }))
            .expect("a target");
        let found = locate_step_target(&screen, &target, false).expect("exact");
        assert_eq!(found.text.as_deref(), Some("发送"));
    }
    #[test]
    fn a_write_prefers_an_editable_field_over_a_label_with_the_same_text() {
        let mut field = element(2, "搜索", "android.widget.EditText", false, (0, 100, 1080, 220));
        field.state.editable = true;
        let label = element(1, "搜索", "android.widget.TextView", false, (0, 0, 200, 60));
        let screen = dump(vec![label, field], false);
        let target = StepTarget::from_command(&serde_json::json!({ "targetText": "搜索" }))
            .expect("a target");
        let found = locate_step_target(&screen, &target, true).expect("the field");
        assert_eq!(found.class, "android.widget.EditText");
    }
    #[test]
    fn a_target_that_is_not_on_the_screen_says_so() {
        let screen = dump(vec![element(1, "首页", "TextView", false, (0, 0, 100, 40))], false);
        let target = StepTarget::from_command(&serde_json::json!({ "targetText": "设置" }))
            .expect("a target");
        let error = locate_step_target(&screen, &target, false).expect_err("missing");
        assert_eq!(error.code, STOP_TARGET_NOT_FOUND);
        assert!(error.detail.contains("设置"), "{}", error.detail);
    }
    #[test]
    fn a_truncated_tree_that_named_one_match_is_still_the_match() {
        // Truncation only makes *ambiguity* unknowable further down; a single
        // hit is still a single hit, and refusing it would make the fallback
        // path unable to run at all.
        let screen = dump(
            vec![element(
                1,
                "发送",
                "android.widget.Button",
                true,
                (0, 0, 100, 100),
            )],
            true,
        );
        let target = StepTarget::from_command(&serde_json::json!({ "targetText": "发送" }))
            .expect("a target");
        assert!(locate_step_target(&screen, &target, false).is_ok());
    }
    #[test]
    fn a_run_is_shorter_when_every_read_is_a_uiautomator_dump() {
        // The fallback path costs a subprocess per read, so its ceiling is
        // lower — and lower by enough that the difference is worth reporting.
        assert!(RUN_MAX_STEPS_FALLBACK < RUN_MAX_STEPS);
        assert_eq!(RUN_MAX_STEPS, super::super::MAX_SEQUENCE_STEPS);
    }
    #[test]
    fn a_step_the_run_never_reached_says_so() {
        let step =
            RunStep::new(4, "tap").not_run("not run: the run stopped at step 3 (app_switched)");
        assert!(!step.executed);
        assert!(!step.ok);
        let line = step.describe();
        assert!(line.starts_with("4. tap — not run"), "{line}");
        assert!(line.contains("stopped at step 3"), "{line}");
    }
    #[test]
    fn a_step_that_ran_but_halted_the_run_keeps_its_effect() {
        // The step worked; what stopped the run was the phone afterwards. The
        // effect is still evidence, so it survives the failure.
        let mut step = RunStep::new(2, "tap").executed(Some("changed"));
        step.fail_in_place(&StepError::new(STOP_APP_SWITCHED, "it moved to another app"));
        assert!(step.executed);
        assert!(!step.ok);
        assert_eq!(step.effect.as_deref(), Some("changed"));
        assert_eq!(step.code.as_deref(), Some(STOP_APP_SWITCHED));
    }
    #[test]
    fn every_op_a_run_claims_to_accept_is_listed_once() {
        assert_eq!(RUN_OPS.len(), 11);
        assert!(RUN_OPS.contains(&"launch"));
        assert!(RUN_OPS.contains(&"observe"));
        // The schema's enum and the parser's list are the same contract; a
        // value in one and not the other is a step the model can write and the
        // run cannot execute.
        let schema = AdbRunTool::new().parameters_schema();
        let ops = schema
            .pointer("/properties/commands/items/properties/op/enum")
            .and_then(Value::as_array)
            .expect("an op enum");
        let declared: Vec<&str> = ops.iter().filter_map(Value::as_str).collect();
        assert_eq!(declared, RUN_OPS.to_vec());
    }
    #[test]
    fn a_scroll_aims_inside_the_container_when_the_tree_names_one() {
        let list = scrollable(3, (0, 300, 1080, 2000));
        let (from, to) = scroll_points((1080, 2340), Direction::Down, Some(&list));
        // Down means the finger moves up: content moves towards the top.
        assert!(from.1 > to.1, "{from:?} {to:?}");
        assert!(from.1 <= 2000 && to.1 >= 300, "the drag stays inside the list");
        assert_eq!(from.0, list.center.0);
    }
    #[test]
    fn a_scroll_without_a_container_uses_the_middle_of_the_screen() {
        let (from, to) = scroll_points((1080, 2340), Direction::Up, None);
        assert_eq!(from, (540, 585));
        assert_eq!(to, (540, 1755));
    }

    #[test]
    fn a_run_reports_whether_events_or_the_tree_watched_for_switches() {
        let from_events = run_data_with(3);
        assert_eq!(from_events["app_change_source"], "events");
        assert_eq!(from_events["checked_by_events"], 3);
        let from_tree = run_data_with(0);
        assert_eq!(from_tree["app_change_source"], "tree");
    }

    fn run_data_with(checked_by_events: usize) -> Value {
        run_data(RunReport {
            steps: &[],
            total: 0,
            stopped: None,
            plan: &super::super::gate::Plan {
                grant: None,
                planned: Vec::new(),
            },
            final_read: None,
            source: "accessibility",
            device: None,
            checked_by_events,
        })
    }

    #[test]
    fn a_chooser_stops_a_run_before_it_acts_on_the_sheet() {
        // `launch` can open a chooser — two apps claim the link — and the steps
        // after it would target the sheet's rows, not the app's.
        let chooser = launch_interrupted("com.android.intentresolver", 3).expect("a stop");
        assert_eq!(chooser.code, STOP_OPENED_ELSEWHERE);
        assert!(chooser.detail.contains("step 3"), "{}", chooser.detail);
        // A permission prompt is the phone asking a person, not the agent
        // landing somewhere wrong.
        let prompt =
            launch_interrupted("com.google.android.permissioncontroller", 1).expect("a stop");
        assert_eq!(prompt.code, STOP_SCREEN_NEEDS_REVIEW);
        assert!(prompt.detail.contains("hand_to_user"), "{}", prompt.detail);
        // An ordinary app is not an interruption at all.
        assert!(launch_interrupted("com.example.chat", 1).is_none());
    }

    // ── Run steps in the 0–999 space ───────────────────────────────────────

    #[tokio::test]
    async fn a_run_swipe_without_a_point_is_refused_before_any_read() {
        // No `adb` here, and none is needed: the refusal happens before the
        // step is allowed to reach the screen at all.
        let error = adopted_point(&serde_json::json!({}), super::super::coords::TAP, None)
            .expect_err("no point");
        assert_eq!(error.code, STOP_FAILED);
        assert!(error.detail.contains("x_pct"), "{}", error.detail);
    }

    #[tokio::test]
    async fn a_run_swipe_in_pixels_is_taken_as_given() {
        // The pixel path never asks for a size, so a run step keeps working —
        // and keeps costing what it did — on a phone with no read to spare.
        assert!(step_coordinate_space(
            &serde_json::json!({ "x": 1, "y": 2 }),
            Target::Screen,
            &None
        )
        .await
        .expect("no percent, no read")
        .is_none());
        let point = adopted_point(
            &serde_json::json!({ "x2": 300, "y2": 40 }),
            super::super::coords::END,
            None,
        )
        .expect("pixels");
        assert_eq!(point, (300, 40));
    }

    #[tokio::test]
    async fn a_run_swipe_percent_needs_a_screen_and_a_failed_read_says_so() {
        // A percent step pays for one read, and when there is no phone to ask
        // the step says that rather than converting against a guessed size.
        let error =
            step_coordinate_space(&serde_json::json!({ "x_pct": 500 }), Target::Screen, &None)
                .await
                .expect_err("no screen to read");
        assert_eq!(error.code, STOP_FAILED);
        assert!(error.detail.contains("0–999"), "{}", error.detail);
        // With nothing to convert against, the point is refused, not zeroed.
        let error = adopted_point(
            &serde_json::json!({ "x_pct": 500, "y_pct": 500 }),
            super::super::coords::TAP,
            None,
        )
        .expect_err("no size");
        assert!(error.detail.contains("pixels"), "{}", error.detail);
    }

    #[test]
    fn tool_names_are_stable() {
        assert_eq!(AdbCollectTool::new().name(), "android_collect");
        assert_eq!(AdbFindTool::new().name(), "android_find");
        assert_eq!(AdbDialogTool::new().name(), "android_dialog");
        assert_eq!(AdbSystemActionTool::new().name(), "android_system_action");
        assert_eq!(AdbOcrTool::new().name(), "android_ocr");
        assert_eq!(AdbDisplayTool::new().name(), "android_display");
        assert_eq!(AdbQueryTool::new().name(), "android_query");
        assert_eq!(AdbWaitTool::new().name(), "android_wait");
        assert_eq!(AdbRunTool::new().name(), "android_run");
        assert_eq!(AdbExtractTool::new().name(), "android_extract");
        assert_eq!(AdbEventTool::new().name(), "android_event");
    }
}
