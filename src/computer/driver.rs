//! Turning a `LoopState` into the next thing to do.
//!
//! [`ComputerUseLoop`](crate::computer::use_loop::ComputerUseLoop) knows how to
//! perceive, act and verify; it deliberately knows nothing about models. This
//! module supplies the missing middle: it renders a state as a decision prompt,
//! asks a provider, and parses the answer back into a [`LoopDecision`].
//!
//! Three properties are load-bearing and each one was a failure mode first:
//!
//! - **Text first.** The prompt carries the accessibility outline, not a
//!   screenshot. A model clicking by element id is far more reliable than one
//!   estimating a coordinate from pixels, and the outline is orders of
//!   magnitude cheaper. An image is attached only when
//!   [`plan_perception`](crate::computer::run_budget::plan_perception) says the
//!   text cannot carry the question.
//! - **A parse failure is `Unverifiable`, not `Done`.** The three-state
//!   verification exists because "nothing was established" is not "it worked".
//!   The same rule applies to reading a model's answer: text that is not a
//!   decision is a request to try again, never a completion.
//! - **`NeedHelp` is a real outcome.** A loop that answers every confusion
//!   with another action is the one that wanders a screen doing damage. The
//!   prompt makes "I cannot" a first-class reply, and the parser honours it.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::computer::run_budget::{
    plan_perception, ImageTrigger, PerceptionInputs, PerceptionPlan,
};
use crate::computer::tree_render::TreeOutline;
use crate::computer::types::{
    ClickTarget, DesktopAction, MouseButton, Point, ScrollDirection, UiElement,
};
use crate::computer::use_loop::{LoopDecision, LoopState, StepRecord, StepVerification};
use crate::computer::Screenshot;
use crate::providers::{CompletionRequest, Message, Provider};

/// How many of the most recent steps go into the prompt.
///
/// The whole history is kept on the state for the result; the prompt gets the
/// tail because a long history costs tokens and the last few steps are what
/// say whether the current approach is working.
pub const HISTORY_IN_PROMPT: usize = 6;

/// The instruction the model answers against.
///
/// Deliberately short. The detailed computer-use discipline already lives in
/// the agent's system prompt (`prompt_builder::add_computer_use_guidance`), and
/// repeating it here would make every step pay for it.
pub const DRIVER_SYSTEM_PROMPT: &str = r#"You drive a computer one step at a time. You are given a goal, the current
accessibility tree of the screen, and what happened on the last few steps.

Answer with ONE JSON object and nothing else:

  {"decision": "act",   "action": {...}}
  {"decision": "done",  "message": "what was accomplished"}
  {"decision": "help",  "reason": "what is blocking you"}

Actions are one of:
  {"type": "click", "element_id": "id from the tree"}
  {"type": "click", "x": 100, "y": 200}
  {"type": "double_click", "element_id": "..."}
  {"type": "type", "text": "...", "element_id": "field to type into"}
  {"type": "type", "text": "..."}
  {"type": "key_press", "keys": ["cmd", "s"]}
  {"type": "scroll", "element_id": "...", "direction": "down", "amount": 3}
  {"type": "launch_app", "name": "Calculator"}
  {"type": "activate_window", "title_pattern": "Untitled"}
  {"type": "read_ui_tree"}
  {"type": "screenshot"}

Rules:
- Prefer element_id over coordinates. An id addresses the exact control; a
  coordinate is a guess that a screen change invalidates. An id is bound to one
  observation — if the screen changed, re-read the tree rather than reusing it.
- One action per step. Do not chain.
- If the goal is already met, answer "done" — do not take another step to be
  sure. If it is not met and you do not know what to do next, answer "help"
  with the specific thing you would need. Guessing is worse than asking.
- Never type into a field marked [sensitive: ...] without the goal naming it."#;

/// A decision the driver could not use.
///
/// The variant matters to the caller: `Unparseable` means "ask again with the
/// same state" (a retry can help), while `Refused` means the provider declined
/// and the run should degrade rather than loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverError {
    /// The provider returned an error.
    Provider(String),
    /// The answer was not a decision. Carries the first part of what came back
    /// so the run log can show what the model said instead.
    Unparseable(String),
    /// The provider calls cannot be made (no provider, exhausted budget).
    Unavailable(String),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::Provider(e) => write!(f, "the decision model failed: {e}"),
            DriverError::Unparseable(e) => write!(f, "the answer was not a decision: {e}"),
            DriverError::Unavailable(e) => write!(f, "no decision model is available: {e}"),
        }
    }
}

impl std::error::Error for DriverError {}

/// What the driver needs to know about one step before prompting.
#[derive(Debug, Clone)]
pub struct StepContext {
    /// The accessibility tree as read for this step.
    pub tree: Vec<UiElement>,
    /// Whether that read was complete.
    pub tree_complete: bool,
    /// Why the read produced no tree at all, when it failed.
    ///
    /// Distinct from `tree_complete`: an incomplete tree *is* a tree the model
    /// can read part of, while this says there was nothing to read. The prompt
    /// words them differently — "an element you cannot see may still exist" is
    /// wrong advice for a screen nobody managed to read, and the honest line
    /// there is that the tree is unavailable and the picture is all there is.
    pub tree_read_failed: Option<String>,
    /// The screenshot, when one was taken. Always available because the loop
    /// captures before deciding; whether it is *sent* is the perception plan's
    /// call.
    pub screenshot: Option<Screenshot>,
    /// Whether the model routed to can accept an image.
    pub model_supports_vision: bool,
    /// Whether the *run* still has an image to spend.
    ///
    /// Separate from there being a screenshot: a run that has spent its image
    /// budget keeps capturing (the loop captures every step) but must stop
    /// attaching. Collapsing the two would make the plan promise an image the
    /// budget has already forbidden.
    pub image_budget_available: bool,
    /// Controls a visual scan found, when one was run for this step.
    ///
    /// Their ids are *not* resolvable by the adapter: the adapter's snapshot
    /// cache never held them, so the run rewrites a target naming one of them
    /// into a point before dispatch
    /// ([`retarget`](crate::computer::run_executor::retarget)). Carrying them
    /// here rather than merging them into `tree` is what keeps that rewrite
    /// decidable — a merged element would be indistinguishable from an
    /// accessibility one that genuinely resolves.
    pub vision: Option<crate::computer::vision::look::VisionElements>,
}

/// What one decision cost, alongside the decision itself.
#[derive(Debug, Clone)]
pub struct DecisionReport {
    pub decision: LoopDecision,
    /// Whether an image really went out.
    ///
    /// `false` when perception asked for one but the payload was empty or
    /// undecodable. Reported from what happened rather than from the plan, so
    /// the run's image count cannot drift from the provider's bill.
    pub image_attached: bool,
    /// Which condition asked for an image, when one was asked for.
    pub trigger: Option<ImageTrigger>,
    /// Content digest of the image that went out, when one did.
    ///
    /// This is what lets the run tell "another picture" from "the same picture
    /// again": two decisions against an unchanged screen carry the same digest,
    /// and charging each of them would count one screen twice.
    pub image_digest: Option<String>,
    /// Prompt tokens the provider reported, when it reported any.
    pub tokens_in: u64,
    /// Completion tokens the provider reported.
    pub tokens_out: u64,
}

/// One screenshot, ready to travel and named by its content.
#[derive(Debug, Clone)]
struct Attachment {
    /// Base64 payload, already shrunk to the driver's image limit.
    base64: String,
    mime: &'static str,
    /// Digest of the *source* bytes, so a repeated frame is recognisable
    /// before it is decoded and re-encoded.
    digest: String,
}

/// Decides the next action by asking a model provider.
///
/// Holds the provider rather than the router so a test can substitute a
/// scripted answer: the interesting behaviour here is prompt rendering and
/// answer parsing, not routing.
pub struct ModelDriver {
    provider: Arc<dyn Provider>,
    model: String,
    max_history: usize,
    attach_limit: crate::computer::image_budget::ImageLimit,
    /// The last frame this driver encoded, kept so an unchanged screen is not
    /// decoded and re-encoded for every step that looks at it.
    ///
    /// A `std::sync::Mutex` is right here: the critical section is a decode of
    /// an in-memory buffer (no `.await` inside), and the driver is shared by
    /// value across steps.
    last_attachment: std::sync::Mutex<Option<Attachment>>,
}

impl ModelDriver {
    pub fn new(provider: Arc<dyn Provider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
            max_history: HISTORY_IN_PROMPT,
            attach_limit: crate::computer::image_budget::ImageLimit::default(),
            last_attachment: std::sync::Mutex::new(None),
        }
    }

    pub fn with_max_history(mut self, n: usize) -> Self {
        self.max_history = n;
        self
    }

    /// Set the image ceiling used when a step does attach one.
    pub fn with_image_limit(mut self, limit: crate::computer::image_budget::ImageLimit) -> Self {
        self.attach_limit = limit;
        self
    }

    /// The prompt for one step, what perception decided, and the image that
    /// will actually travel with it.
    ///
    /// The plan and the attachment are produced together — the image is
    /// encoded *before* the prompt is written — so the text and the payload
    /// cannot disagree. A capture that decodes but will not encode (a
    /// truncated or corrupt frame) turns the plan back into "no image" here,
    /// rather than leaving a prompt that says "an image of the screen is
    /// attached" over a request carrying none. A model told to look at a
    /// picture it was not given acts on a screen it cannot see.
    fn compose(
        &self,
        state: &LoopState,
        ctx: &StepContext,
    ) -> (String, String, PerceptionPlan, Option<Attachment>) {
        // A visual scan answers the question an image would have been sent to
        // ask, and answers it in the same semantic form the tree uses. So when
        // one produced elements it *substitutes* for an attachment rather than
        // adding to it: sending both would pay twice for one answer. When the
        // scan produced nothing, `ctx.vision` is `None` and the image rung
        // stands — B′'s "nobody could look, so look yourself" rule.
        let vision_answered = ctx.vision.as_ref().is_some_and(|v| !v.is_empty());

        let mut perception = plan_perception(PerceptionInputs {
            tree_complete: ctx.tree_complete,
            tree_read_failed: ctx.tree_read_failed.is_some(),
            last_verified: state.last_verified,
            consecutive_failures: state.consecutive_failures,
            model_supports_vision: ctx.model_supports_vision,
            // Two conditions, not one: the run may still have images to spend
            // *and* this step may have a capture worth sending.
            image_budget_available: ctx.image_budget_available && can_attach(ctx),
        });
        if vision_answered && perception.attach_image {
            perception.attach_image = false;
            perception.trigger = None;
        }

        // Settle "will an image really go" before writing anything about it.
        let attachment = if perception.attach_image {
            match ctx.screenshot.as_ref().and_then(|shot| self.attach(shot)) {
                Some(image) => Some(image),
                None => {
                    debug!("a capture could not be encoded; the step goes out as text only");
                    perception.attach_image = false;
                    perception.trigger = None;
                    None
                }
            }
        } else {
            None
        };
        let mut wire = String::new();
        wire.push_str(&format!("Goal: {}\n", state.goal));
        wire.push_str(&format!("Step: {}\n", state.step));

        if state.consecutive_failures > 0 {
            wire.push_str(&format!("Consecutive failures: {}\n", state.consecutive_failures));
        }
        if let Some(err) = &state.last_error {
            wire.push_str(&format!("Last error: {}\n", err));
        }
        match state.last_verified {
            Some(true) => wire.push_str("Last step: verified — it did what it should have.\n"),
            Some(false) => wire.push_str("Last step: NOT verified — it did not take effect.\n"),
            None => wire.push_str("Last step: unchecked — nothing could confirm or deny it.\n"),
        }

        if !state.history.is_empty() {
            wire.push_str("\nRecent steps:\n");
            let start = state.history.len().saturating_sub(self.max_history);
            for record in &state.history[start..] {
                wire.push_str(&render_step(record));
            }
        }

        wire.push_str("\nCurrent screen:\n");
        let mut outline = TreeOutline::default();
        outline.push_tree(
            &ctx.tree,
            "accessibility",
            // The driver's own budget. Larger than the tool's default because
            // a run cannot ask the person to re-run with more lines, and the
            // pruning step has already taken the off-screen windows out.
            160,
            false,
            0,
            &std::collections::HashMap::new(),
        );
        wire.push_str(&outline.text);
        if let Some(reason) = state.capture_error.as_deref() {
            // Say exactly this, not "the screen is empty": the capture failed,
            // so nothing here is a report about the desktop. A model that
            // mistook a failed look for a blank screen would plan against it.
            wire.push_str(&format!(
                "(the screen could NOT be captured: {reason}. No screenshot from this step \
                 describes the desktop. Answer `help` if the goal needs the screen.)\n"
            ));
        } else if let Some(reason) = ctx.tree_read_failed.as_deref() {
            // Not "an element you cannot see may still exist" — that is advice
            // for a tree that was cut. Here nothing was read at all, and the
            // ways forward are a screenshot or screen coordinates.
            wire.push_str(&format!(
                "(the accessibility tree could not be read: {reason}. Act from the \
                 screenshot or by screen coordinates until it can be.)\n"
            ));
        } else if let Some(note) = outline.incomplete_note() {
            wire.push_str(note);
        }

        // The visual scan, as a second outline in the same format. It is kept
        // apart from the accessibility tree rather than merged into it so the
        // model can weigh two kinds of evidence separately: one is what the
        // platform reports, the other is what a detector guessed from pixels.
        if let Some(vision) = ctx.vision.as_ref().filter(|v| !v.is_empty()) {
            wire.push_str("\nDetected from the screenshot:\n");
            let mut seen = TreeOutline::default();
            seen.push_tree(
                &vision.snapshot.roots,
                "omniparser",
                // A scan is an addition to the tree, not a replacement for
                // it; half the tree's budget is enough to name what is on
                // screen without pushing the tree itself out.
                80,
                false,
                vision.len(),
                &std::collections::HashMap::new(),
            );
            wire.push_str(&seen.text);
            wire.push_str(&crate::computer::tree_render::trust_line("omniparser"));
            wire.push_str(&vision.note());
            wire.push('\n');
        }

        if perception.attach_image {
            let why = perception
                .trigger
                .map(ImageTrigger::describe)
                .unwrap_or("the tree is not enough");
            wire.push_str(&format!(
                "\nAn image of the screen is attached because {why}. Use it to find the \
                 control, then act by element id if the tree names one.\n"
            ));
        }

        wire.push_str("\nWhat is the next single step? Answer with one JSON object.\n");

        // Keyed off the failure count rather than off `perception.trigger`:
        // which reason asked for an image and whether the approach is working
        // are different facts, and a run that failed three times must be told
        // to change course even when an incomplete tree is what asked for the
        // picture.
        let system = if state.consecutive_failures >= 2 {
            format!(
                "{DRIVER_SYSTEM_PROMPT}\n\nNote: several steps in a row have failed. Your \
                 current approach is not working — consider a different route to the goal \
                 rather than repeating it more carefully."
            )
        } else {
            DRIVER_SYSTEM_PROMPT.to_string()
        };
        (system, wire, perception, attachment)
    }

    /// The prompt for one step.
    ///
    /// Public so tests — and anyone debugging a bad decision — can read
    /// exactly what the model was shown without calling a provider.
    pub fn render_prompt(&self, state: &LoopState, ctx: &StepContext) -> (String, String) {
        let (system, wire, _perception, _attachment) = self.compose(state, ctx);
        (system, wire)
    }

    /// Ask the model and parse its answer.
    ///
    /// Returns `LoopDecision` on success. `DriverError::Unparseable` is not a
    /// decision: the loop turns it into `Unverifiable` and tries again, which
    /// is why it is an error here rather than a `NeedHelp`.
    pub async fn decide(
        &self,
        state: &LoopState,
        ctx: &StepContext,
    ) -> Result<LoopDecision, DriverError> {
        self.decide_reporting(state, ctx)
            .await
            .map(|report| report.decision)
    }

    /// [`decide`](Self::decide), plus what the call actually cost.
    ///
    /// The run's budget is driven from this: counting images from the
    /// *decision* would count the ones perception wanted, not the ones the
    /// provider received.
    pub async fn decide_reporting(
        &self,
        state: &LoopState,
        ctx: &StepContext,
    ) -> Result<DecisionReport, DriverError> {
        let (system, user, perception, attachment) = self.compose(state, ctx);

        let mut message = Message::user(user);
        let mut image_attached = false;
        let mut image_digest = None;
        if perception.attach_image {
            if let Some(attachment) = attachment {
                message = message.with_image(attachment.base64, attachment.mime);
                image_attached = true;
                image_digest = Some(attachment.digest);
            }
        }

        let request = CompletionRequest {
            messages: vec![Message::system(system), message],
            // A decision is a judgement about a specific screen, but it must
            // be reproducible enough that a failing run can be read back.
            temperature: Some(0.0),
            max_tokens: Some(700),
            stream: false,
            model: Some(self.model.clone()),
            requires_vision: image_attached,
            ..Default::default()
        };

        let response = self
            .provider
            .complete(request)
            .await
            .map_err(|e| DriverError::Provider(e.to_string()))?;

        let decision = parse_decision(&response.message.content)?;
        let (tokens_in, tokens_out) = response
            .usage
            .as_ref()
            .map(|u| (u.prompt_tokens as u64, u.completion_tokens as u64))
            .unwrap_or((0, 0));
        Ok(DecisionReport {
            decision,
            image_attached,
            trigger: if image_attached {
                perception.trigger
            } else {
                None
            },
            image_digest,
            tokens_in,
            tokens_out,
        })
    }

    /// Base64 plus MIME for an attached screenshot, shrunk to the limit.
    ///
    /// `None` when the screenshot carries no bytes (a file-only capture, or a
    /// headless placeholder): attaching an empty image is worse than attaching
    /// none, because the model is told to look and finds nothing.
    ///
    /// A screenshot whose content digest matches the last one this driver
    /// encoded is served from the cache: the bytes are identical, so decoding
    /// and re-encoding them would produce the same payload at the same cost.
    /// The cache is keyed on the digest rather than on the `Screenshot` so two
    /// separately captured frames of an unchanged screen also hit it.
    fn attach(&self, shot: &Screenshot) -> Option<Attachment> {
        if shot.base64.is_empty() {
            return None;
        }
        let digest = shot
            .digest
            .clone()
            .or_else(|| crate::computer::types::digest_of_base64(&shot.base64))?;

        if let Ok(cache) = self.last_attachment.lock() {
            if let Some(last) = cache.as_ref().filter(|a| a.digest == digest) {
                return Some(last.clone());
            }
        }

        use base64::Engine as _;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&shot.base64)
            .ok()?;
        let encoded =
            crate::computer::image_budget::encode_or_passthrough(&raw, &self.attach_limit).ok()?;
        let attachment = Attachment {
            base64: encoded.base64,
            mime: encoded.mime,
            digest,
        };
        if let Ok(mut cache) = self.last_attachment.lock() {
            *cache = Some(attachment.clone());
        }
        Some(attachment)
    }
}

/// Whether this step has an image worth sending.
///
/// A screenshot with no bytes is a capture that produced nothing — a file-only
/// record or a headless placeholder. Counting it as "available" would let the
/// perception plan ask for an image the driver then cannot attach, and the
/// prompt would promise the model a picture that never arrives.
fn can_attach(ctx: &StepContext) -> bool {
    ctx.screenshot
        .as_ref()
        .is_some_and(|shot| !shot.base64.is_empty())
}

/// One line per completed step, for the prompt.
fn render_step(record: &StepRecord) -> String {
    let verdict = match record.verification {
        StepVerification::Verified => "verified",
        StepVerification::Failed => "FAILED",
        StepVerification::Unverifiable => "unchecked",
    };
    format!(
        "  {}. {} → {} ({})\n",
        record.step + 1,
        describe_action(&record.action),
        verdict,
        summary_of(&record.result.message)
    )
}

/// Trim a tool message to something that fits on one prompt line.
fn summary_of(message: &str) -> String {
    let flat = message.replace(['\n', '\r'], " ");
    let mut chars = flat.chars();
    let head: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// A short human-readable form of an action, for the history lines.
pub fn describe_action(action: &DesktopAction) -> String {
    match action {
        DesktopAction::Click { target, .. } => format!("click {}", describe_target(target)),
        DesktopAction::DoubleClick { target, .. } => {
            format!("double-click {}", describe_target(target))
        }
        DesktopAction::Type { text, target } => {
            let preview = summary_of(text);
            match target {
                Some(t) => format!("type {:?} into {}", preview, describe_target(t)),
                None => format!("type {:?}", preview),
            }
        }
        DesktopAction::KeyPress { keys } => format!("press {}", keys.join("+")),
        DesktopAction::Scroll { target, direction, amount } => format!(
            "scroll {} by {} at {}",
            direction_word(*direction),
            amount,
            describe_target(target)
        ),
        DesktopAction::Drag { from, to } => {
            format!("drag {} to {}", describe_target(from), describe_target(to))
        }
        DesktopAction::ReadUiTree { app } => match app {
            Some(app) => format!("read the UI tree of {app}"),
            None => "read the UI tree".to_string(),
        },
        DesktopAction::LaunchApp { name, .. } => format!("launch {name}"),
        DesktopAction::ActivateWindow { title_pattern } => {
            format!("activate the window {title_pattern:?}")
        }
        DesktopAction::CloseWindow { title_pattern } => {
            format!("close the window {title_pattern:?}")
        }
        DesktopAction::ListWindows => "list windows".to_string(),
        DesktopAction::GetWindowGeometry { title_pattern } => {
            format!("measure the window {title_pattern:?}")
        }
        DesktopAction::MoveWindow { title_pattern, .. } => {
            format!("move the window {title_pattern:?}")
        }
        DesktopAction::ResizeWindow { title_pattern, .. } => {
            format!("resize the window {title_pattern:?}")
        }
        DesktopAction::Screenshot { .. } => "take a screenshot".to_string(),
        other => format!("{other:?}"),
    }
}

fn describe_target(target: &ClickTarget) -> String {
    match target {
        ClickTarget::ElementId(id) => format!("[{id}]"),
        ClickTarget::ElementLabel(label) => format!("{label:?}"),
        ClickTarget::ElementRoleLabel { role, label } => format!("{role} {label:?}"),
        ClickTarget::Coordinate(p) => format!("({}, {})", p.x, p.y),
    }
}

fn direction_word(direction: ScrollDirection) -> &'static str {
    match direction {
        ScrollDirection::Up => "up",
        ScrollDirection::Down => "down",
        ScrollDirection::Left => "left",
        ScrollDirection::Right => "right",
    }
}

/// The wire format the model is asked to produce.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
enum WireDecision {
    Act { action: WireAction },
    Done { message: String },
    Help { reason: String },
}

/// One action, as the model writes it.
///
/// Deliberately a separate type from [`DesktopAction`]: the wire form carries
/// only the six things a model can usefully decide, and each is validated
/// here rather than by a deserializer that would accept a malformed action and
/// fail later inside the adapter — where the error is about the desktop, not
/// about the answer.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireAction {
    Click {
        #[serde(default)]
        element_id: Option<String>,
        #[serde(default)]
        x: Option<i32>,
        #[serde(default)]
        y: Option<i32>,
    },
    DoubleClick {
        #[serde(default)]
        element_id: Option<String>,
        #[serde(default)]
        x: Option<i32>,
        #[serde(default)]
        y: Option<i32>,
    },
    Type {
        text: String,
        #[serde(default)]
        element_id: Option<String>,
    },
    KeyPress {
        keys: Vec<String>,
    },
    Scroll {
        #[serde(default)]
        element_id: Option<String>,
        #[serde(default)]
        x: Option<i32>,
        #[serde(default)]
        y: Option<i32>,
        direction: String,
        #[serde(default = "default_scroll_amount")]
        amount: i32,
    },
    LaunchApp {
        name: String,
        #[serde(default)]
        args: Vec<String>,
    },
    ActivateWindow {
        title_pattern: String,
    },
    ReadUiTree {
        #[serde(default)]
        app: Option<String>,
    },
    Screenshot,
}

fn default_scroll_amount() -> i32 {
    3
}

impl WireAction {
    /// The canonical action, or a sentence saying what was wrong with it.
    fn into_action(self) -> Result<DesktopAction, String> {
        let to_target = |element_id: Option<String>, x: Option<i32>, y: Option<i32>| {
            match (element_id, x, y) {
                (Some(id), _, _) if !id.trim().is_empty() => {
                    Ok(ClickTarget::ElementId(id.trim().to_string()))
                }
                (_, Some(x), Some(y)) => Ok(ClickTarget::Coordinate(Point::new(x, y))),
                // Half a coordinate is not a point. Guessing the other half
                // would click somewhere the model never named.
                (_, Some(_), None) | (_, None, Some(_)) => {
                    Err("half a coordinate (both x and y, or neither)".to_string())
                }
                _ => Err("no element_id and no x/y".to_string()),
            }
        };

        match self {
            WireAction::Click { element_id, x, y } => Ok(DesktopAction::Click {
                target: to_target(element_id, x, y)?,
                button: MouseButton::Left,
            }),
            WireAction::DoubleClick { element_id, x, y } => Ok(DesktopAction::DoubleClick {
                target: to_target(element_id, x, y)?,
                button: MouseButton::Left,
            }),
            WireAction::Type { text, element_id } => Ok(DesktopAction::Type {
                text,
                target: element_id
                    .filter(|id| !id.trim().is_empty())
                    .map(|id| ClickTarget::ElementId(id.trim().to_string())),
            }),
            WireAction::KeyPress { keys } => {
                if keys.is_empty() {
                    return Err("a key press with no keys".to_string());
                }
                Ok(DesktopAction::KeyPress { keys })
            }
            WireAction::Scroll {
                element_id,
                x,
                y,
                direction,
                amount,
            } => Ok(DesktopAction::Scroll {
                target: to_target(element_id, x, y)?,
                direction: parse_direction(&direction)?,
                // A scroll must move something; zero is a no-op the model
                // would observe as a failure and then repeat.
                amount: if amount == 0 {
                    default_scroll_amount()
                } else {
                    amount
                },
            }),
            WireAction::LaunchApp { name, args } => Ok(DesktopAction::LaunchApp {
                name,
                args,
                wait_for_ready: true,
            }),
            WireAction::ActivateWindow { title_pattern } => {
                Ok(DesktopAction::ActivateWindow { title_pattern })
            }
            WireAction::ReadUiTree { app } => Ok(DesktopAction::ReadUiTree { app }),
            WireAction::Screenshot => Ok(DesktopAction::Screenshot { region: None }),
        }
    }
}

fn parse_direction(word: &str) -> Result<ScrollDirection, String> {
    match word.trim().to_ascii_lowercase().as_str() {
        "up" => Ok(ScrollDirection::Up),
        "down" => Ok(ScrollDirection::Down),
        "left" => Ok(ScrollDirection::Left),
        "right" => Ok(ScrollDirection::Right),
        other => Err(format!("unknown scroll direction {other:?}")),
    }
}

/// Turn a model's answer into a decision.
///
/// Accepts an optional ```json fence and an optional sentence around the
/// object — models do both, and failing on the wrapping would be failing on
/// presentation rather than on content. What is *not* accepted is an action
/// the loop cannot execute: that comes back as an error so the step is
/// recorded as unchecked and the model is asked again with the reason.
pub fn parse_decision(text: &str) -> Result<LoopDecision, DriverError> {
    let Some(object) = extract_json_object(text) else {
        let preview = summary_of(text.trim());
        return Err(DriverError::Unparseable(if preview.is_empty() {
            "the answer was empty".to_string()
        } else {
            preview
        }));
    };

    let wire: WireDecision = serde_json::from_str(&object)
        .map_err(|e| DriverError::Unparseable(format!("{e} in {object}")))?;

    match wire {
        WireDecision::Done { message } => Ok(LoopDecision::Done { message }),
        WireDecision::Help { reason } => Ok(LoopDecision::NeedHelp { reason }),
        WireDecision::Act { action } => action
            .into_action()
            .map(LoopDecision::Action)
            .map_err(DriverError::Unparseable),
    }
}

/// The first balanced `{...}` in `text`, tolerating a fence or prose around it.
///
/// Hand-rolled rather than a regex because the object contains nested braces
/// and strings that may contain braces; a depth counter that ignores quoted
/// regions is the only version that finds the right closing brace.
pub fn extract_json_object(text: &str) -> Option<String> {
    let bytes: Vec<char> = text.chars().collect();
    let start = bytes.iter().position(|c| *c == '{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *ch == '\\' {
                escaped = true;
            } else if *ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(bytes[start..=start + offset].iter().collect());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::ActionResult;
    use async_trait::async_trait;

    /// A one-pixel PNG, so a test screenshot carries real bytes.
    ///
    /// The point is not the picture: `can_attach` refuses an empty payload, so
    /// a placeholder string would make every "an image is attached" assertion
    /// pass while nothing was ever sent.
    const ONE_PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

    fn screenshot() -> Screenshot {
        Screenshot::new(ONE_PIXEL_PNG.to_string(), 1, 1)
    }

    fn state() -> LoopState {
        LoopState {
            step: 0,
            goal: "Open the calculator and compute 17 × 23".to_string(),
            screenshot: screenshot(),
            capture_error: None,
            history: vec![],
            last_verified: None,
            last_error: None,
            consecutive_failures: 0,
        }
    }

    fn ctx(tree_complete: bool) -> StepContext {
        StepContext {
            tree: vec![],
            tree_complete,
            tree_read_failed: None,
            screenshot: Some(screenshot()),
            model_supports_vision: true,
            image_budget_available: true,
            vision: None,
        }
    }

    fn driver() -> ModelDriver {
        ModelDriver::new(Arc::new(ScriptedProvider::default()), "test-model")
    }

    // ── prompt rendering ────────────────────────────────────────────────

    #[test]
    fn the_prompt_carries_the_goal_and_the_step() {
        let (system, user) = driver().render_prompt(&state(), &ctx(true));
        assert!(system.contains("JSON object"), "{system}");
        assert!(user.contains("Goal: Open the calculator"), "{user}");
        assert!(user.contains("Step: 0"), "{user}");
    }

    #[test]
    fn a_whole_tree_with_a_clean_history_asks_for_no_image() {
        let (_system, user) = driver().render_prompt(&state(), &ctx(true));
        assert!(!user.contains("image of the screen is attached"), "{user}");
    }

    /// One detected control ("Save" at 10,20 30x10), as a scan would deliver it.
    fn vision_elements() -> crate::computer::vision::look::test_support::Scan {
        crate::computer::vision::look::test_support::scan("Save", (10, 20, 30, 10))
    }

    /// A visual scan is rendered as its own outline, labelled as a guess.
    ///
    /// The label is the point: a detected control and an accessibility control
    /// look the same in an outline, and a model that cannot tell them apart
    /// will trust a box drawn around a visually-similar pixel as if the
    /// platform had reported it.
    #[test]
    fn a_visual_scan_is_shown_as_its_own_section() {
        let mut context = ctx(true);
        context.vision = Some(vision_elements());
        let (_system, user) = driver().render_prompt(&state(), &context);

        assert!(user.contains("Detected from the screenshot:"), "{user}");
        assert!(user.contains("\"Save\""), "{user}");
        assert!(
            user.contains("inferred from pixels"),
            "the scan must be labelled as a guess: {user}"
        );
    }

    /// When the scan answered the question, no image is sent: paying for both
    /// would buy one answer twice.
    #[test]
    fn a_visual_scan_replaces_the_image_attachment() {
        let mut context = ctx(false);
        context.vision = Some(vision_elements());
        let (_system, user) = driver().render_prompt(&state(), &context);
        assert!(
            !user.contains("image of the screen is attached"),
            "a scan that answered stands in for the image: {user}"
        );
        assert!(user.contains("Detected from the screenshot:"), "{user}");
    }

    /// A scan that found nothing is not an answer, so the image rung stands.
    #[test]
    fn an_empty_scan_does_not_cancel_the_image() {
        let mut context = ctx(false);
        let mut empty = vision_elements();
        empty.snapshot.roots.clear();
        context.vision = Some(empty);
        let (_system, user) = driver().render_prompt(&state(), &context);
        assert!(user.contains("image of the screen is attached"), "{user}");
        assert!(!user.contains("Detected from the screenshot:"), "{user}");
    }

    #[test]
    fn a_truncated_tree_says_an_image_is_attached() {
        let (_system, user) = driver().render_prompt(&state(), &ctx(false));
        assert!(user.contains("image of the screen is attached"), "{user}");
        assert!(user.contains("accessibility tree is incomplete"), "{user}");
    }

    /// A tree that could not be read is not a tree that was cut. The old
    /// wording — "an element you cannot see may still exist" — tells the model
    /// to re-read a screen nobody managed to read in the first place.
    #[test]
    fn an_unreadable_tree_is_told_apart_from_a_truncated_one() {
        let mut context = ctx(false);
        context.tree_read_failed = Some("pyatspi is not installed".to_string());
        let (_system, user) = driver().render_prompt(&state(), &context);
        assert!(user.contains("could not be read"), "{user}");
        assert!(user.contains("pyatspi is not installed"), "{user}");
        assert!(
            !user.contains("an element you cannot see may still exist"),
            "that advice is for a tree that was cut, not one that is absent: {user}"
        );
        // It is still a reason to attach the picture — nothing else describes
        // the screen.
        assert!(user.contains("could not be read"), "{user}");
    }

    /// A capture that failed is a fact about the look, not about the screen.
    ///
    /// Before this the loop answered a failed capture with an empty
    /// placeholder, and the prompt showed an empty tree — which reads as "the
    /// desktop has nothing on it". A model acting on that clicks into the
    /// dark. The reason is what lets it say "I cannot look".
    #[test]
    fn a_failed_capture_is_reported_as_one() {
        let mut s = state();
        s.capture_error =
            Some("The screen is blank: no desktop session is drawing on it".to_string());
        let (_system, user) = driver().render_prompt(&s, &ctx(true));
        assert!(user.contains("could NOT be captured"), "{user}");
        assert!(user.contains("no desktop session is drawing"), "{user}");
        assert!(
            user.contains("Answer `help`"),
            "an unseeable screen is a reason to hand back, not to guess: {user}"
        );
    }

    /// With a capture in hand the note must not appear — a stale warning would
    /// train the model to ignore it.
    #[test]
    fn a_successful_capture_says_nothing_about_failure() {
        let (_system, user) = driver().render_prompt(&state(), &ctx(true));
        assert!(!user.contains("could NOT be captured"), "{user}");
    }

    #[test]
    fn the_history_is_rendered_as_one_line_per_step() {
        let mut s = state();
        s.step = 2;
        s.history = vec![
            StepRecord {
                step: 0,
                action: DesktopAction::LaunchApp {
                    name: "Calculator".to_string(),
                    args: vec![],
                    wait_for_ready: true,
                },
                result: ActionResult::success("launched".to_string()),
                verification: StepVerification::Verified,
                screenshot_before: None,
                screenshot_after: None,
                snapshots_taken: 0,
                sensitive: false,
            },
            StepRecord {
                step: 1,
                action: DesktopAction::Click {
                    target: ClickTarget::ElementId("s1:0.4".to_string()),
                    button: MouseButton::Left,
                },
                result: ActionResult::success("clicked".to_string()),
                verification: StepVerification::Failed,
                screenshot_before: None,
                screenshot_after: None,
                snapshots_taken: 0,
                sensitive: false,
            },
        ];
        let (_system, user) = driver().render_prompt(&s, &ctx(true));
        assert!(user.contains("1. launch Calculator → verified"), "{user}");
        assert!(user.contains("2. click [s1:0.4] → FAILED"), "{user}");
    }

    /// The history is tail-limited: a long run must not grow every prompt.
    #[test]
    fn only_the_recent_steps_are_in_the_prompt() {
        let mut s = state();
        s.history = (0..20)
            .map(|i| StepRecord {
                step: i,
                action: DesktopAction::KeyPress { keys: vec![format!("k{i}")] },
                result: ActionResult::success("ok".to_string()),
                verification: StepVerification::Verified,
                screenshot_before: None,
                screenshot_after: None,
                snapshots_taken: 0,
                sensitive: false,
            })
            .collect();
        s.step = 20;
        let (_system, user) = driver().with_max_history(3).render_prompt(&s, &ctx(true));
        assert!(user.contains("press k19"), "{user}");
        assert!(!user.contains("press k16"), "the older steps must be dropped");
    }

    /// Repeated failures change the instruction, not just the data: telling
    /// the model "try again" after five failures is how a run digs deeper.
    #[test]
    fn repeated_failures_tell_the_model_to_change_approach() {
        let mut s = state();
        s.consecutive_failures = 3;
        s.last_verified = Some(false);
        let (system, user) = driver().render_prompt(&s, &ctx(true));
        assert!(system.contains("several steps in a row have failed"), "{system}");
        assert!(user.contains("Consecutive failures: 3"), "{user}");
    }

    /// An unverified step is not described as a failure.
    #[test]
    fn an_unchecked_step_is_worded_differently_from_a_failed_one() {
        let mut unchecked = state();
        unchecked.last_verified = None;
        let (_, user) = driver().render_prompt(&unchecked, &ctx(true));
        assert!(user.contains("unchecked"), "{user}");
        assert!(!user.contains("NOT verified"), "{user}");

        let mut failed = state();
        failed.last_verified = Some(false);
        let (_, user) = driver().render_prompt(&failed, &ctx(true));
        assert!(user.contains("NOT verified"), "{user}");
    }

    // ── parsing ─────────────────────────────────────────────────────────

    #[test]
    fn a_plain_done_is_a_done() {
        let decision =
            parse_decision(r#"{"decision": "done", "message": "Calculator shows 391"}"#).unwrap();
        assert_eq!(
            decision,
            LoopDecision::Done {
                message: "Calculator shows 391".to_string()
            }
        );
    }

    #[test]
    fn help_is_a_decision_not_a_failure() {
        let decision = parse_decision(
            r#"{"decision": "help", "reason": "the window is behind a modal I cannot dismiss"}"#,
        )
        .unwrap();
        assert!(matches!(decision, LoopDecision::NeedHelp { .. }));
    }

    #[test]
    fn a_click_by_element_id_becomes_an_id_target() {
        let decision = parse_decision(
            r#"{"decision": "act", "action": {"type": "click", "element_id": "s1:0.4"}}"#,
        )
        .unwrap();
        assert_eq!(
            decision,
            LoopDecision::Action(DesktopAction::Click {
                target: ClickTarget::ElementId("s1:0.4".to_string()),
                button: MouseButton::Left,
            })
        );
    }

    #[test]
    fn a_click_by_coordinate_becomes_a_point() {
        let decision = parse_decision(
            r#"{"decision": "act", "action": {"type": "click", "x": 120, "y": 340}}"#,
        )
        .unwrap();
        assert_eq!(
            decision,
            LoopDecision::Action(DesktopAction::Click {
                target: ClickTarget::Coordinate(Point::new(120, 340)),
                button: MouseButton::Left,
            })
        );
    }

    /// An id wins over coordinates when both are present: the id is exact.
    #[test]
    fn an_element_id_outranks_a_coordinate() {
        let decision = parse_decision(
            r#"{"decision": "act", "action": {"type": "click", "element_id": "s9:0.1", "x": 1, "y": 2}}"#,
        )
        .unwrap();
        match decision {
            LoopDecision::Action(DesktopAction::Click { target, .. }) => {
                assert_eq!(target, ClickTarget::ElementId("s9:0.1".to_string()));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_key_press_keeps_its_keys() {
        let decision = parse_decision(
            r#"{"decision": "act", "action": {"type": "key_press", "keys": ["cmd", "s"]}}"#,
        )
        .unwrap();
        assert_eq!(
            decision,
            LoopDecision::Action(DesktopAction::KeyPress {
                keys: vec!["cmd".to_string(), "s".to_string()]
            })
        );
    }

    #[test]
    fn a_scroll_keeps_its_direction_and_amount() {
        let decision = parse_decision(
            r#"{"decision": "act", "action": {"type": "scroll", "element_id": "s1:0.2", "direction": "down", "amount": 5}}"#,
        )
        .unwrap();
        match decision {
            LoopDecision::Action(DesktopAction::Scroll { direction, amount, .. }) => {
                assert_eq!(direction, ScrollDirection::Down);
                assert_eq!(amount, 5);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// A zero scroll is a no-op the model would see as a failure and repeat.
    #[test]
    fn a_zero_scroll_gets_a_usable_amount() {
        let decision = parse_decision(
            r#"{"decision": "act", "action": {"type": "scroll", "element_id": "s1:0.2", "direction": "down", "amount": 0}}"#,
        )
        .unwrap();
        match decision {
            LoopDecision::Action(DesktopAction::Scroll { amount, .. }) => {
                assert_eq!(amount, default_scroll_amount());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Models fence their JSON. Failing on the fence would be failing on
    /// presentation.
    #[test]
    fn a_fenced_answer_is_accepted() {
        let text = "Here you go:\n```json\n{\"decision\": \"done\", \"message\": \"ok\"}\n```";
        assert!(matches!(parse_decision(text), Ok(LoopDecision::Done { .. })));
    }

    #[test]
    fn prose_around_the_object_is_tolerated() {
        let text =
            "I think the goal is met. {\"decision\": \"done\", \"message\": \"done\"} Let me know.";
        assert!(matches!(parse_decision(text), Ok(LoopDecision::Done { .. })));
    }

    /// Braces inside strings must not confuse the extractor.
    #[test]
    fn braces_inside_a_string_do_not_end_the_object() {
        let text = r#"{"decision": "done", "message": "set to {x}"}"#;
        let object = extract_json_object(text).unwrap();
        assert_eq!(object, text);
        assert!(matches!(parse_decision(text), Ok(LoopDecision::Done { .. })));
    }

    #[test]
    fn an_empty_answer_is_unparseable_not_done() {
        let err = parse_decision("   ").unwrap_err();
        assert!(matches!(err, DriverError::Unparseable(_)), "{err:?}");
    }

    #[test]
    fn prose_without_an_object_is_unparseable() {
        let err = parse_decision("Sure, I'll click the button now.").unwrap_err();
        assert!(matches!(err, DriverError::Unparseable(_)), "{err:?}");
    }

    /// The core safety property: text that is not a decision is never read as
    /// one. A run that treated a malformed answer as success would stop early
    /// and report the goal met.
    #[test]
    fn a_malformed_answer_is_never_read_as_success() {
        for text in [
            "",
            "ok",
            "{}",
            r#"{"decision": "maybe"}"#,
            r#"{"decision": "done"}"#, // missing the message
        ] {
            let result = parse_decision(text);
            assert!(result.is_err(), "{text:?} was read as {result:?}");
        }
    }

    #[test]
    fn an_action_with_no_target_is_unparseable() {
        let err =
            parse_decision(r#"{"decision": "act", "action": {"type": "click"}}"#).unwrap_err();
        match err {
            DriverError::Unparseable(why) => assert!(why.contains("element_id"), "{why}"),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Half a coordinate would make the loop click wherever the missing
    /// coordinate defaulted to.
    #[test]
    fn half_a_coordinate_is_refused() {
        let err = parse_decision(r#"{"decision": "act", "action": {"type": "click", "x": 10}}"#)
            .unwrap_err();
        match err {
            DriverError::Unparseable(why) => assert!(why.contains("half a coordinate"), "{why}"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_unknown_scroll_direction_is_refused() {
        let err = parse_decision(
            r#"{"decision": "act", "action": {"type": "scroll", "element_id": "a", "direction": "diagonal"}}"#,
        )
        .unwrap_err();
        assert!(matches!(err, DriverError::Unparseable(_)));
    }

    #[test]
    fn an_empty_key_press_is_refused() {
        let err =
            parse_decision(r#"{"decision": "act", "action": {"type": "key_press", "keys": []}}"#)
                .unwrap_err();
        assert!(matches!(err, DriverError::Unparseable(_)));
    }

    #[test]
    fn a_type_without_a_field_is_allowed() {
        // Typing into whatever holds focus is legitimate — the model may have
        // clicked the field on the previous step.
        let decision =
            parse_decision(r#"{"decision": "act", "action": {"type": "type", "text": "hello"}}"#)
                .unwrap();
        assert_eq!(
            decision,
            LoopDecision::Action(DesktopAction::Type {
                text: "hello".to_string(),
                target: None
            })
        );
    }

    #[test]
    fn a_type_with_a_field_names_it() {
        let decision = parse_decision(
            r#"{"decision": "act", "action": {"type": "type", "text": "17*23", "element_id": "s2:0.9"}}"#,
        )
        .unwrap();
        match decision {
            LoopDecision::Action(DesktopAction::Type { target, .. }) => {
                assert_eq!(target, Some(ClickTarget::ElementId("s2:0.9".to_string())));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn launch_app_waits_for_the_window() {
        let decision = parse_decision(
            r#"{"decision": "act", "action": {"type": "launch_app", "name": "Calculator"}}"#,
        )
        .unwrap();
        assert_eq!(
            decision,
            LoopDecision::Action(DesktopAction::LaunchApp {
                name: "Calculator".to_string(),
                args: vec![],
                wait_for_ready: true
            })
        );
    }

    // ── the provider round trip ──────────────────────────────────────────

    #[derive(Default)]
    struct ScriptedProvider {
        answer: String,
        requests: std::sync::Mutex<Vec<CompletionRequest>>,
    }

    impl ScriptedProvider {
        fn answering(answer: &str) -> Self {
            Self {
                answer: answer.to_string(),
                requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<CompletionRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn default_model(&self) -> &str {
            "test-model"
        }

        fn supports_tools(&self) -> bool {
            false
        }

        fn max_context(&self) -> usize {
            8_192
        }

        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> crate::Result<crate::providers::CompletionResponse> {
            self.requests.lock().unwrap().push(request);
            Ok(crate::providers::CompletionResponse {
                message: Message::assistant(self.answer.clone()),
                usage: None,
                model: "test-model".to_string(),
                finish_reason: None,
            })
        }

        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> crate::Result<crate::providers::CompletionStream> {
            Ok(Box::pin(tokio_stream::iter(vec![])))
        }

        async fn health_check(&self) -> crate::Result<bool> {
            Ok(true)
        }

        async fn set_credential(
            &self,
            _credential: crate::model_router::Credential,
        ) -> crate::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn the_driver_sends_one_request_and_parses_the_answer() {
        let provider =
            Arc::new(ScriptedProvider::answering(r#"{"decision": "done", "message": "done"}"#));
        let driver = ModelDriver::new(provider.clone(), "test-model");
        let decision = driver.decide(&state(), &ctx(true)).await.unwrap();
        assert!(matches!(decision, LoopDecision::Done { .. }));

        let requests = provider.requests();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].requires_vision, "a whole tree needs no image");
        assert_eq!(requests[0].temperature, Some(0.0));
        assert_eq!(requests[0].model.as_deref(), Some("test-model"));
    }

    /// The expensive case must not happen by default: a healthy screen sends
    /// no image, so the request is text-only.
    #[tokio::test]
    async fn a_healthy_step_sends_no_image_to_the_provider() {
        let provider =
            Arc::new(ScriptedProvider::answering(r#"{"decision": "done", "message": "done"}"#));
        let driver = ModelDriver::new(provider.clone(), "test-model");
        driver.decide(&state(), &ctx(true)).await.unwrap();
        let request = &provider.requests()[0];
        assert!(!request.requires_vision);
        let user = request
            .messages
            .iter()
            .find(|m| m.role == crate::providers::Role::User)
            .expect("a user message");
        assert!(!user.has_images(), "no image should be attached to a text-only step");
    }

    /// A capture that cannot be encoded must not leave a prompt promising an
    /// image that never arrives.
    ///
    /// The failure this guards is narrow and real: `can_attach` checks only
    /// that the payload is non-empty, so a truncated or corrupt frame passes
    /// it and then fails to encode. The step has to go out as text — with the
    /// prompt saying so — rather than telling the model to look at a picture
    /// it was not given.
    #[tokio::test]
    async fn an_undecodable_capture_turns_the_step_back_into_text() {
        let provider =
            Arc::new(ScriptedProvider::answering(r#"{"decision": "done", "message": "done"}"#));
        let driver = ModelDriver::new(provider.clone(), "test-model");
        let mut context = ctx(false); // a truncated tree would ask for an image
        context.screenshot = Some(Screenshot::new(
            "aGVsbG8gd29ybGQ=".to_string(), // base64 of "hello world", not an image
            100,
            100,
        ));

        let report = driver.decide_reporting(&state(), &context).await.unwrap();
        assert!(!report.image_attached, "nothing was attached");
        assert_eq!(report.trigger, None, "no reason is claimed for an absent image");

        let request = &provider.requests()[0];
        assert!(!request.requires_vision);
        let user = request
            .messages
            .iter()
            .find(|m| m.role == crate::providers::Role::User)
            .expect("a user message");
        assert!(!user.has_images());
        let text = &user.content;
        assert!(
            !text.contains("image of the screen is attached"),
            "the prompt must not promise an image it does not carry: {text}"
        );
    }

    /// Two decisions against an unchanged screen report the same image digest,
    /// and the second one travels as the cached payload: the point of the
    /// digest on `Screenshot` is that the run can recognise the repeat.
    #[tokio::test]
    async fn an_unchanged_screen_reports_the_same_image_digest() {
        let provider = Arc::new(ScriptedProvider::answering(
            r#"{"decision": "help", "reason": "cannot see"}"#,
        ));
        let driver = ModelDriver::new(provider.clone(), "test-model");
        let context = ctx(false); // a truncated tree asks for an image

        let first = driver.decide_reporting(&state(), &context).await.unwrap();
        let second = driver.decide_reporting(&state(), &context).await.unwrap();

        let first_digest = first.image_digest.expect("an image went out");
        assert!(first.image_attached);
        assert_eq!(
            Some(first_digest),
            second.image_digest,
            "the same bytes must report the same digest, or the run cannot dedupe it"
        );

        // And it is the store's own key, so the summary and the CAS agree.
        let shot = context.screenshot.as_ref().expect("a capture");
        let expected = crate::computer::types::digest_of_base64(&shot.base64).expect("decodes");
        assert_eq!(second.image_digest.as_deref(), Some(expected.as_str()));
        assert!(expected.starts_with("sha256:"), "{expected}");
    }

    /// A model without vision must not be asked for one.
    #[tokio::test]
    async fn a_model_without_vision_is_never_sent_an_image() {
        let provider = Arc::new(ScriptedProvider::answering(
            r#"{"decision": "help", "reason": "cannot see"}"#,
        ));
        let driver = ModelDriver::new(provider.clone(), "text-only");
        let mut context = ctx(false);
        context.model_supports_vision = false;
        driver.decide(&state(), &context).await.unwrap();
        let request = &provider.requests()[0];
        assert!(!request.requires_vision, "{request:?}");
    }

    #[tokio::test]
    async fn a_provider_error_is_reported_as_a_provider_error() {
        struct Failing;
        #[async_trait]
        impl Provider for Failing {
            fn name(&self) -> &str {
                "failing"
            }
            fn default_model(&self) -> &str {
                "m"
            }
            fn supports_tools(&self) -> bool {
                false
            }
            fn max_context(&self) -> usize {
                8_192
            }
            async fn complete(
                &self,
                _request: CompletionRequest,
            ) -> crate::Result<crate::providers::CompletionResponse> {
                Err(crate::error::SyscityError::Internal("boom".to_string()))
            }
            async fn stream(
                &self,
                _request: CompletionRequest,
            ) -> crate::Result<crate::providers::CompletionStream> {
                Err(crate::error::SyscityError::Internal("no stream".to_string()))
            }
            async fn health_check(&self) -> crate::Result<bool> {
                Ok(false)
            }
            async fn set_credential(
                &self,
                _credential: crate::model_router::Credential,
            ) -> crate::Result<()> {
                Ok(())
            }
        }
        let driver = ModelDriver::new(Arc::new(Failing), "m");
        let err = driver.decide(&state(), &ctx(true)).await.unwrap_err();
        assert!(matches!(err, DriverError::Provider(_)), "{err:?}");
    }

    #[test]
    fn describe_action_covers_the_common_verbs() {
        assert_eq!(
            describe_action(&DesktopAction::KeyPress {
                keys: vec!["cmd".into(), "s".into()]
            }),
            "press cmd+s"
        );
        assert_eq!(
            describe_action(&DesktopAction::Screenshot { region: None }),
            "take a screenshot"
        );
        assert_eq!(
            describe_action(&DesktopAction::LaunchApp {
                name: "Notes".into(),
                args: vec![],
                wait_for_ready: true
            }),
            "launch Notes"
        );
    }
}
