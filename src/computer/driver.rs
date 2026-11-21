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
  {"decision": "act",   "actions": [{...}, {...}]}
  {"decision": "done",  "message": "what was accomplished"}
  {"decision": "help",  "reason": "what is blocking you"}

A step may carry more than one action: `"actions": [...]` runs them in order
without looking at the screen between them. Use it when the next actions follow
from this picture alone — clicking a field, typing, pressing Tab, typing the
next field. It costs one look instead of four. Do not chain actions whose
success decides the next one: if the second needs to know whether the first
worked, give it its own step and look.

Actions are one of:
  {"type": "click", "element_id": "id from the tree"}
  {"type": "click", "x": 100, "y": 200}
  {"type": "double_click", "element_id": "..."}
  {"type": "type", "text": "...", "element_id": "field to type into"}
  {"type": "type", "text": "..."}
  {"type": "key_press", "keys": ["cmd", "s"]}
  {"type": "scroll", "element_id": "...", "direction": "down", "amount": 3}
  {"type": "drag", "element_id": "source", "to_element_id": "target"}
  {"type": "drag", "x": 100, "y": 200, "to_x": 400, "to_y": 300}
  {"type": "hover", "element_id": "control to test"}
  {"type": "hover", "x": 100, "y": 200}
  {"type": "pointer_move", "x": 900, "y": 600}
  {"type": "wait", "milliseconds": 2000}
  {"type": "clipboard_set", "text": "..."}
  {"type": "clipboard_get"}
  {"type": "launch_app", "name": "Calculator"}
  {"type": "activate_window", "title_pattern": "Untitled"}
  {"type": "close_window", "title_pattern": "Untitled"}
  {"type": "list_windows"}
  {"type": "get_window_geometry", "title_pattern": "Untitled"}
  {"type": "move_window", "title_pattern": "Untitled", "x": 0, "y": 0}
  {"type": "resize_window", "title_pattern": "Untitled", "width": 800, "height": 600}
  {"type": "minimize_window", "title_pattern": "Untitled"}
  {"type": "maximize_window", "title_pattern": "Untitled"}
  {"type": "read_ui_tree"}
  {"type": "screenshot"}
  {"type": "screenshot", "region": {"x": 0, "y": 0, "width": 300, "height": 200}}

Rules:
- Prefer element_id over coordinates. An id addresses the exact control; a
  coordinate is a guess that a screen change invalidates. An id is bound to one
  observation — if the screen changed, re-read the tree rather than reusing it.
- One look, one decision. The actions in a step all run against this picture,
  so only batch what this picture already answers. A field you clicked is not
  focused until the action runs, so follow a click with a type, not with a
  second click whose target you expected the first to reveal.
- If the goal is already met, answer "done" — do not take another step to be
  sure. If it is not met and you do not know what to do next, answer "help"
  with the specific thing you would need. Guessing is worse than asking.
- Never type into a field marked [sensitive: ...] without the goal naming it.
- Dragging is how sliders, scrollbars, text selection and drag-and-drop move.
  Nothing else moves them: a click sets a scrollbar's position to that spot,
  which is not the same gesture and often does not do what was meant.
- A drag is one gesture you cannot watch. You choose where it ends before you
  start, and the thumb lands where you chose — so a long drag to an exact
  value overshoots and you only find out afterwards. Drag to roughly the right
  place, look at where the thumb actually is, then drag again from there by the
  small distance that is left. Two short drags beat one long one aimed blind.
- A scrollbar or slider's *position* is how you tell where you are. The thumb
  touching the far end of its track is "there is nothing more this way" — that
  is a fact you read off the picture, and it saves the scroll-and-look rounds
  a person would otherwise spend. A thumb in the middle is not the end, however
  empty the rest of the pane looks: the content may still be loading.
- To replace text already in a field, select it first: key_press
  ["cmd", "a"] (use "ctrl" on Linux and Windows), then type. Typing alone
  appends.
- When you cannot tell from the picture whether a control is live, hover it
  and look at the next step's screen. A control that reacts to the pointer is
  one a click will probably reach. This is a measurement, not proof: a disabled
  control may still be styled to react, and a live one may not react at all.
  The tooltip a probe opens is cleared for you before the next action, so you
  do not need to move the pointer away first.
- Every window action names a window by a *substring of its title*, and
  `title_pattern` is the only way to say which one. The titles come from
  `list_windows` — the step's own line says how many it found, and the titles
  are listed underneath it. Call `list_windows` first, then copy a title from
  that list rather than guessing at one. A window that is minimised is marked
  `[minimised]`: it is visible in neither the tree nor the screenshot, so it
  can only be reached by naming it.
- If something is still loading, wait rather than acting again. A second click
  on a control that has not drawn yet lands on whatever is under it."#;

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
    /// The area this step's capture covers, in screen points.
    ///
    /// `None` is the whole display layout. `Some` means the model asked for a
    /// close-up on the previous step: the prompt says which rectangle the
    /// picture shows, and the scan's boxes were mapped from that rectangle's
    /// corner rather than the screen's.
    pub capture_region: Option<crate::computer::types::Rect>,
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
    /// The size of the payload that travels, in image pixels.
    ///
    /// Not the capture's size: the encoder may have shrunk it to fit
    /// [`ImageLimit`]. The prompt says so, because a model that reads a
    /// position off a 1024-wide picture of a 2560-wide screen and emits it as
    /// a screen coordinate aims at something less than half as far right as
    /// it meant — and asks for a close-up centred there.
    width: u32,
    height: u32,
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
        // adding to it: sending both would pay twice for one answer.
        //
        // That reasoning only holds where the text index is *authoritative*: a
        // complete tree, every control named and addressable. There the scan's
        // boxes are a second reading of a screen the model can already
        // navigate, and the picture is redundant.
        //
        // Everywhere else it inverts, and the inversion is not about cost. The
        // detector resizes the screen into a fixed 640×640 frame before it
        // looks, so on a wide display anything smaller than a few dozen pixels
        // is simply absent from the boxes; the picture the model receives is up
        // to 1024 wide and read by the model itself. Withholding it on an
        // incomplete tree leaves the model with strictly less than the screen
        // holds — and with no picture to point at.
        //
        // That last part is the reason this is a prerequisite rather than a
        // nicety: a model cannot ask to look more closely at a region of a
        // picture it was never shown. If the scan is the only index, the boxes
        // it returns are also the limit of what the model knows exists, so a
        // control the detector missed is a control nobody will ever ask to see.
        // Region re-look is unreachable by construction. The scan keeps the ids
        // addressable; the image is what makes "there is something over there,
        // look closer" a request the run can act on.
        let tree_less = ctx.tree.is_empty() && ctx.tree_read_failed.is_none();
        let vision_answered = ctx.vision.as_ref().is_some_and(|v| !v.is_empty());
        // Suppress only under a complete tree. `tree_less` is checked as well
        // because an empty list cannot claim completeness, and a tree-less
        // screen is the case this rule was written for.
        let the_scan_answers_alone = vision_answered && ctx.tree_complete && !tree_less;

        let mut perception = plan_perception(PerceptionInputs {
            tree_complete: ctx.tree_complete,
            tree_read_failed: ctx.tree_read_failed.is_some(),
            // A successful read with no roots at all is the tree-less case: the
            // read worked, and the app published nothing. Judged from the tree
            // the prompt will show (pruning folds windows, it never drops
            // roots), so the sentence and the outline cannot disagree.
            tree_unread: tree_less,
            last_verified: state.last_verified,
            consecutive_failures: state.consecutive_failures,
            // A hover's answer is this frame: the control either reacted to
            // the pointer or it did not, and that repaint is only in the
            // picture. Read off the history rather than a state field because
            // the fact is "the step that just ran", which is exactly the last
            // record — and a `pointer_move` that follows a hover ends it
            // deliberately, since moving away is what dismisses the tooltip
            // the probe opened.
            last_step_hovered: state
                .history
                .last()
                .is_some_and(|r| matches!(r.action, DesktopAction::Hover { .. })),
            model_supports_vision: ctx.model_supports_vision,
            // Two conditions, not one: the run may still have images to spend
            // *and* this step may have a capture worth sending.
            image_budget_available: ctx.image_budget_available && can_attach(ctx),
        });
        // One exception to the substitution: a hover probe. A scan is a second
        // reading of *what is there*, and it is exactly what a hover is not
        // asking. The question is whether the control changed when the pointer
        // arrived — a hover style, a tooltip, a menu — and no box list can
        // answer it, however complete. Substituting here would suppress the one
        // frame the probe exists to produce.
        if the_scan_answers_alone
            && perception.attach_image
            && perception.trigger != Some(ImageTrigger::HoverProbe)
        {
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
                // The one-line history says "unchecked"; what it cannot say —
                // and what decides whether retrying is worth a step — is that
                // the screen changed somewhere other than where the action
                // pointed. The same sentence the `computer` tool appends, so a
                // model that reads both is not left to reconcile two wordings.
                if let Some(target) = record.aim_missed {
                    wire.push_str(&format!(
                        "     {}\n",
                        crate::computer::vision::off_target_note(&target)
                    ));
                }
                // A window list is the one result whose entire payload is the
                // list, and the one-line form throws it away: macOS says
                // "Found 3 windows", which names nothing the model can
                // `activate_window`, `move_window` or `close_window` by.
                if let Some(note) = window_list_note(record) {
                    wire.push_str(&note);
                }
            }
        }

        wire.push_str("\nCurrent screen:\n");
        // What the sensitive-UI detector flagged, keyed by element id. The
        // system prompt tells the model never to type into a field marked
        // `[sensitive: ...]`; before this map was filled in, that marker was
        // never written on either outline and the rule was unenforceable. The
        // tree and the scan are marked from the same detector, so an element
        // drawn on one of them is drawn identically.
        let sensitive = crate::computer::sensitive_ui::sensitive_markers(&ctx.tree);
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
            &sensitive,
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

        // The fourth way a step can be blind, and the only one that used to go
        // unmentioned. A tree-less screen attaches a picture on every step, so
        // the run's image allowance is spent fast; past it `plan_perception`
        // stops attaching, and the sentence that explains a picture simply
        // disappears. The model then sees an ordinary-looking step with no
        // image and no reason for it — and a model that does not know it is
        // blind still plans as if it could see. The other three states above
        // are all stated for exactly this reason.
        //
        // The advice is deliberately "say so" rather than "retry": the
        // allowance will not come back in this run, so the honest moves are
        // asking for help and finishing in one action.
        if !perception.attach_image && !ctx.image_budget_available && ctx.screenshot.is_some() {
            wire.push_str(
                "(no picture is attached to this step: the run has used its image allowance. \
                 The screen is still capturable, but you are working without it from here — \
                 prefer actions the tree names, and answer `help` if the goal cannot be met \
                 without seeing the screen.)\n",
            );
        }

        // The visual scan, as a second outline in the same format. It is kept
        // apart from the accessibility tree rather than merged into it so the
        // model can weigh two kinds of evidence separately: one is what the
        // platform reports, the other is what a detector guessed from pixels.
        if let Some(vision) = ctx.vision.as_ref().filter(|v| !v.is_empty()) {
            wire.push_str("\nDetected from the screenshot:\n");
            let source = vision.outline_source();
            // A scan's elements have ids from the same `assign_ids` sequence an
            // accessibility read uses, so the sensitive map from the tree above
            // cannot collide with them — and the scan itself is scanned for the
            // same markers. Without this a password field on a tree-less screen
            // had no marker at all, which is the one place the "never type into
            // a sensitive field" rule most needed to be visible.
            let vision_sensitive =
                crate::computer::sensitive_ui::sensitive_markers(&vision.snapshot.roots);
            let mut seen = TreeOutline::default();
            seen.push_tree(
                &vision.snapshot.roots,
                source,
                // A scan is an addition to the tree, not a replacement for
                // it; half the tree's budget is enough to name what is on
                // screen without pushing the tree itself out.
                80,
                false,
                vision.len(),
                &vision_sensitive,
            );
            wire.push_str(&seen.text);
            wire.push_str(&crate::computer::tree_render::trust_line(source));
            wire.push_str(&vision.note());
            wire.push('\n');
        }

        if perception.attach_image {
            let why = perception
                .trigger
                .map(ImageTrigger::describe)
                .unwrap_or("the tree is not enough");
            // The picture may not be the size of the screen: the encoder
            // shrinks it to the driver's image limit. A model that reads a
            // position off a 1024-wide picture of a 2560-wide screen and emits
            // it as a screen coordinate aims at less than half the distance it
            // meant — and a close-up asked for at that point shows the wrong
            // place. So the scale is stated whenever the two differ.
            //
            // The *screen area* is the region's own size when this is a crop:
            // `shot.width` is the encoded pixel width, which for a crop says
            // nothing about how many screen points it covers (a 1826 px crop
            // of 600 points is right, not wrong). Dividing by it would hand the
            // model a factor that is wrong in the opposite direction.
            let scale_note = match (&attachment, ctx.screenshot.as_ref()) {
                (Some(image), Some(shot)) => {
                    let (area_w, area_h, origin) = match ctx.capture_region {
                        Some(region) => {
                            (region.width as f64, region.height as f64, Some((region.x, region.y)))
                        }
                        None => (shot.width as f64, shot.height as f64, None),
                    };
                    let scaled = image.width != area_w as u32 || image.height != area_h as u32;
                    if !scaled {
                        String::new()
                    } else {
                        let scale_x = area_w / image.width.max(1) as f64;
                        let scale_y = area_h / image.height.max(1) as f64;
                        match origin {
                            Some((x, y)) => format!(
                                " The image is {}×{} but the screen area it shows is {}×{} \
                                 screen points, so an image position is not a screen position: \
                                 multiply by about {:.2} horizontally and {:.2} vertically, \
                                 then add the crop's origin ({}, {}) — or name a region in \
                                 screen points directly.",
                                image.width, image.height, area_w, area_h, scale_x, scale_y, x, y,
                            ),
                            None => format!(
                                " The image is {}×{} and the screen is {}×{} screen points, so \
                                 an image position is not a screen position: multiply by about \
                                 {:.2} horizontally and {:.2} vertically before giving a \
                                 coordinate or a close-up region.",
                                image.width, image.height, area_w, area_h, scale_x, scale_y,
                            ),
                        }
                    }
                }
                _ => String::new(),
            };
            // A crop is a close-up, and a close-up has two traps: the model
            // reads positions off the crop as if they were screen positions,
            // and it forgets that what it cannot see in the crop may still be
            // on the screen. Both are answered by saying what the image covers.
            match ctx.capture_region {
                Some(region) => wire.push_str(&format!(
                    "\nA close-up of the screen is attached because {why}: it covers the \
                     rectangle from ({}, {}) to ({}, {}). Coordinates in it are still screen \
                     points, not offsets within the crop. Anything outside that rectangle is \
                     not shown here — take another screenshot with no region to see the \
                     whole screen again.{scale_note}\n",
                    region.x,
                    region.y,
                    region.x + region.width as i32,
                    region.y + region.height as i32,
                )),
                None => wire.push_str(&format!(
                    "\nAn image of the screen is attached because {why}. Use it to find the \
                     control, then act by element id if the tree names one.{scale_note}\n"
                )),
            }
        }

        wire.push_str(
            "\nWhat is the next single step? Answer with one JSON object. One action, or \
             `\"actions\": [...]` for several that follow from this picture.\n",
        );

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
            width: encoded.width,
            height: encoded.height,
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

/// How many windows a `list_windows` step prints into the prompt.
///
/// A desktop with forty windows is not a list the model can use — it needs the
/// two or three it means to arrange. The tail is counted rather than dropped,
/// so "there are more, and this is not all of them" is never implied away.
const WINDOW_LIST_LIMIT: usize = 20;

/// What a `list_windows` result found, for the prompt.
///
/// A window list is the one result whose whole payload is the list, and
/// [`render_step`] flattens a step to one 120-character line: the titles are
/// what the model needs in order to name a window to `activate_window`,
/// `move_window`, `resize_window`, `minimize_window` or `close_window`, and a
/// summary of "Found 3 windows" names none of them.
///
/// There are two shapes, because the platforms differ. macOS returns typed
/// [`WindowInfo`]s in `data`, so the state (minimised/maximised) is readable
/// and worth printing. Linux and Windows return the desktop tool's own text in
/// the *message* — `"Windows:\n<wmctrl or EnumWindows output>"` — which
/// `summary_of` would cut off mid-line, so the lines are carried through
/// instead. Anything else renders nothing and the step's message stands.
fn window_list_note(record: &StepRecord) -> Option<String> {
    if !matches!(record.action, DesktopAction::ListWindows) {
        return None;
    }
    if let Some(windows) = record
        .result
        .data
        .as_ref()
        .and_then(|d| d.get("windows"))
        .and_then(|w| w.as_array())
    {
        return Some(typed_window_list(windows));
    }

    // Text platforms. `Windows:` is the prefix both the x11 and the PowerShell
    // tool put in front of their output; the count-bearing macOS message never
    // matches it, so a macOS result without `data` still falls through to the
    // one-line summary rather than printing "Found 3 windows" twice.
    let body = record
        .result
        .message
        .strip_prefix("Windows:")?
        .trim_start_matches(['\n', '\r']);
    let lines: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return Some("     the desktop reported no windows\n".to_string());
    }
    let mut note = String::from("     named windows:\n");
    for line in lines.iter().take(WINDOW_LIST_LIMIT) {
        note.push_str(&format!("     - {line}\n"));
    }
    if lines.len() > WINDOW_LIST_LIMIT {
        note.push_str(&format!("     … and {} more\n", lines.len() - WINDOW_LIST_LIMIT));
    }
    Some(note)
}

/// Render typed window records, the shape macOS produces.
fn typed_window_list(windows: &[serde_json::Value]) -> String {
    if windows.is_empty() {
        return "     the desktop reported no windows\n".to_string();
    }
    let mut note = String::new();
    for window in windows.iter().take(WINDOW_LIST_LIMIT) {
        let title = window.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let app = window.get("app_name").and_then(|v| v.as_str());
        // State matters as much as the name: a minimised window is in neither
        // the tree nor the screenshot, so "which one do I restore" is only
        // answerable from this flag.
        let state = if window.get("minimized").and_then(|v| v.as_bool()) == Some(true) {
            " [minimised]"
        } else if window.get("maximized").and_then(|v| v.as_bool()) == Some(true) {
            " [maximised]"
        } else {
            ""
        };
        match app.filter(|a| !a.is_empty()) {
            Some(app) => note.push_str(&format!("     - {app}: {title}{state}\n")),
            None => note.push_str(&format!("     - {title}{state}\n")),
        }
    }
    if windows.len() > WINDOW_LIST_LIMIT {
        note.push_str(&format!("     … and {} more\n", windows.len() - WINDOW_LIST_LIMIT));
    }
    note
}

/// A short human-readable form of an action, for the history lines.
pub fn describe_action(action: &DesktopAction) -> String {
    match action {
        DesktopAction::Click { target, .. } => format!("click {}", describe_target(target)),
        DesktopAction::DoubleClick { target, .. } => {
            format!("double-click {}", describe_target(target))
        }
        DesktopAction::Hover { target } => format!("hover {}", describe_target(target)),
        DesktopAction::PointerMove { to } => format!("move the pointer to ({}, {})", to.x, to.y),
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
        DesktopAction::MinimizeWindow { title_pattern } => {
            format!("minimize the window {title_pattern:?}")
        }
        DesktopAction::MaximizeWindow { title_pattern } => {
            format!("maximize the window {title_pattern:?}")
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
///
/// A step carries one action or a short list of them. Both are accepted, and
/// they deserialize into the same field, because a model that answers
/// `"action"` and one that answers `"actions"` are saying the same thing at
/// different granularity.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
enum WireDecision {
    Act {
        #[serde(default)]
        action: Option<WireAction>,
        #[serde(default)]
        actions: Option<Vec<WireAction>>,
    },
    Done {
        message: String,
    },
    Help {
        reason: String,
    },
}

impl WireDecision {
    /// The actions this decision carries, in the order they were written.
    ///
    /// `"action"` and `"actions"` both work. When a model writes both — some
    /// do, aping the examples — they are not silently concatenated: the
    /// single action is the amendment to the list, so it goes last. The
    /// alternative, dropping one, would discard an action the model meant to
    /// run.
    fn into_actions(self) -> Result<Vec<DesktopAction>, String> {
        let (single, list) = match self {
            WireDecision::Act { action, actions } => (action, actions),
            _ => return Err("not an act decision".to_string()),
        };
        let mut wire: Vec<WireAction> = list.unwrap_or_default();
        if let Some(action) = single {
            wire.push(action);
        }
        if wire.is_empty() {
            return Err("no action in the decision".to_string());
        }
        wire.into_iter().map(WireAction::into_action).collect()
    }
}

/// One action, as the model writes it.
///
/// Deliberately a separate type from [`DesktopAction`]: the wire form carries
/// only the actions a model can usefully decide, and each is validated here
/// rather than by a deserializer that would accept a malformed action and fail
/// later inside the adapter — where the error is about the desktop, not about
/// the answer.
///
/// The set is smaller than [`DesktopAction`] on purpose — process control and
/// system queries are not step-by-step UI decisions — but it is not *arbitrarily*
/// smaller. `drag`, `wait`, the clipboard pair and the window actions are what a
/// person reaches for on an ordinary screen, and their absence made whole
/// classes of UI unreachable no matter how good the perception was: a slider,
/// a kanban column, a text selection and a scrollbar only move by dragging, and
/// a run with no `wait` retried a slow dialog as if it had failed.
///
/// The window set is the whole one: measure, activate, move, resize, minimize,
/// maximize, close, list. Move and resize alone were the reason "put these two
/// side by side" was unreachable — positioning a window you have not measured
/// is a guess, and making it "as wide as its half" needs the display's size,
/// which `maximize_window` supplies without the model knowing it. Still left
/// out: `get_system_status`, the process actions, and the clipboard's
/// read-modify-write pair, none of which is a decision taken from a picture.
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
    /// Press at one place, move, release at another.
    ///
    /// Flat fields rather than nested objects: the model already writes
    /// `element_id`/`x`/`y` for a click, and the same shape twice with a `to_`
    /// prefix is one convention instead of two.
    Drag {
        #[serde(default)]
        element_id: Option<String>,
        #[serde(default)]
        x: Option<i32>,
        #[serde(default)]
        y: Option<i32>,
        #[serde(default)]
        to_element_id: Option<String>,
        #[serde(default)]
        to_x: Option<i32>,
        #[serde(default)]
        to_y: Option<i32>,
    },
    /// Sleep, then look at the screen again.
    ///
    /// A first-class action rather than something the loop does behind the
    /// model's back: only the model knows whether the dialog it just opened is
    /// expected to take a moment, and a run that treats "not yet" as "failed"
    /// burns steps retrying something already in progress.
    Wait {
        #[serde(default = "default_wait_ms")]
        milliseconds: u64,
    },
    /// Read the clipboard into the next step's context.
    ClipboardGet,
    /// Put text on the clipboard, for pasting into a field with `key_press`.
    ClipboardSet {
        text: String,
    },
    LaunchApp {
        name: String,
        #[serde(default)]
        args: Vec<String>,
    },
    ActivateWindow {
        title_pattern: String,
    },
    CloseWindow {
        title_pattern: String,
    },
    ListWindows,
    MoveWindow {
        title_pattern: String,
        x: i32,
        y: i32,
    },
    ResizeWindow {
        title_pattern: String,
        width: u32,
        height: u32,
    },
    /// Measure a window, so a later `move_window` can be aimed rather than
    /// guessed. Also how a side-by-side layout is planned: a person reads the
    /// two frames before dragging one next to the other.
    GetWindowGeometry {
        title_pattern: String,
    },
    MinimizeWindow {
        title_pattern: String,
    },
    /// Fill the screen with the window.
    ///
    /// The counterpart to `resize_window`: "as large as this display allows"
    /// without the model having to know the display's size. It matters for the
    /// layout work `move_window`/`resize_window` exist for — two windows side
    /// by side, one of them made as wide as its half.
    MaximizeWindow {
        title_pattern: String,
    },
    ReadUiTree {
        #[serde(default)]
        app: Option<String>,
    },
    /// Put the pointer on a control and leave it there, to see whether the
    /// control reacts.
    ///
    /// A person answers "is this clickable" by moving over it: a button that
    /// highlights, a row that tints, a link that underlines has a hover state,
    /// and one that does not is either inert or invisible to the pointer. That
    /// is a *measurement*, not the `enabled` flag the platform often will not
    /// publish — which is why this exists as an action instead of a field.
    ///
    /// It is not a click, and it does not press anything.
    Hover {
        #[serde(default)]
        element_id: Option<String>,
        #[serde(default)]
        x: Option<i32>,
        #[serde(default)]
        y: Option<i32>,
    },
    /// Move the pointer to a bare point without touching anything.
    ///
    /// The companion to `hover`: a tooltip or a hover menu the probe opened
    /// covers part of the next frame, and moving away is how it is dismissed.
    PointerMove {
        x: i32,
        y: i32,
    },
    /// Capture the screen, or one rectangle of it.
    ///
    /// `region` is what makes "look closer" possible: the detector reads a
    /// fixed 640×640 resize, so a small control is simply not in its answer,
    /// while a region capture is taken at the screen's own resolution.
    Screenshot {
        #[serde(default)]
        region: Option<WireRegion>,
    },
}

/// A rectangle, as the model writes it.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
struct WireRegion {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

fn default_wait_ms() -> u64 {
    1_000
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
            WireAction::Drag {
                element_id,
                x,
                y,
                to_element_id,
                to_x,
                to_y,
            } => Ok(DesktopAction::Drag {
                from: to_target(element_id, x, y)?,
                to: to_target(to_element_id, to_x, to_y)?,
            }),
            WireAction::Wait { milliseconds } => {
                if milliseconds == 0 {
                    return Err("a wait of zero milliseconds is not a step".to_string());
                }
                Ok(DesktopAction::Wait { milliseconds })
            }
            WireAction::ClipboardGet => Ok(DesktopAction::ClipboardGet),
            WireAction::ClipboardSet { text } => Ok(DesktopAction::ClipboardSet { text }),
            WireAction::CloseWindow { title_pattern } => {
                Ok(DesktopAction::CloseWindow { title_pattern })
            }
            WireAction::ListWindows => Ok(DesktopAction::ListWindows),
            WireAction::MoveWindow { title_pattern, x, y } => {
                Ok(DesktopAction::MoveWindow { title_pattern, x, y })
            }
            WireAction::ResizeWindow { title_pattern, width, height } => {
                if width == 0 || height == 0 {
                    return Err("a window resized to zero is a window closed".to_string());
                }
                Ok(DesktopAction::ResizeWindow { title_pattern, width, height })
            }
            WireAction::ActivateWindow { title_pattern } => {
                Ok(DesktopAction::ActivateWindow { title_pattern })
            }
            WireAction::GetWindowGeometry { title_pattern } => {
                Ok(DesktopAction::GetWindowGeometry { title_pattern })
            }
            WireAction::MinimizeWindow { title_pattern } => {
                Ok(DesktopAction::MinimizeWindow { title_pattern })
            }
            WireAction::MaximizeWindow { title_pattern } => {
                Ok(DesktopAction::MaximizeWindow { title_pattern })
            }
            WireAction::ReadUiTree { app } => Ok(DesktopAction::ReadUiTree { app }),
            WireAction::Hover { element_id, x, y } => Ok(DesktopAction::Hover {
                target: to_target(element_id, x, y)?,
            }),
            WireAction::PointerMove { x, y } => {
                Ok(DesktopAction::PointerMove { to: Point::new(x, y) })
            }
            WireAction::Screenshot { region } => Ok(DesktopAction::Screenshot {
                region: region
                    .map(|r| crate::computer::types::Rect::new(r.x, r.y, r.width, r.height)),
            }),
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
        act @ WireDecision::Act { .. } => {
            let mut actions = act.into_actions().map_err(DriverError::Unparseable)?;
            if actions.len() == 1 {
                Ok(LoopDecision::Action(actions.remove(0)))
            } else {
                Ok(LoopDecision::Batch(actions))
            }
        }
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
            capture_region: None,
        }
    }

    fn ctx(tree_complete: bool) -> StepContext {
        StepContext {
            // A cut tree is a tree that has something in it: the read stopped
            // part way through what it found. A whole tree has a whole root.
            // Neither is empty, because an empty list is the *third* case — an
            // app that publishes no tree — and `compose` distinguishes it by
            // looking at the tree, so an empty fixture would silently turn
            // every truncated-tree assertion into a tree-less one.
            tree: vec![root(tree_complete)],
            tree_complete,
            tree_read_failed: None,
            screenshot: Some(screenshot()),
            model_supports_vision: true,
            image_budget_available: true,
            vision: None,
            capture_region: None,
        }
    }

    /// One window: whole when `complete`, cut by the read's limit otherwise.
    fn root(complete: bool) -> crate::computer::types::UiElement {
        crate::computer::types::UiElement {
            id: "window-1".to_string(),
            role: "window".to_string(),
            label: Some("Calculator".to_string()),
            value: None,
            bounds: Some(crate::computer::types::Rect::new(0, 0, 400, 300)),
            enabled: crate::computer::types::Tri::Unknown,
            focused: crate::computer::types::Tri::Unknown,
            actions: None,
            children: vec![],
            children_count: Some(if complete { 0 } else { 9 }),
            truncated: !complete,
        }
    }

    fn driver() -> ModelDriver {
        ModelDriver::new(Arc::new(ScriptedProvider::default()), "test-model")
    }

    /// A hover is a measurement, and the frame being measured is the *next*
    /// one. The step after a probe must therefore carry a picture even when the
    /// tree is whole and nothing failed — otherwise the run pays for the probe
    /// and is handed text that cannot answer what it asked.
    #[test]
    fn the_step_after_a_hover_carries_the_frame_it_probed() {
        let mut s = state();
        s.step = 1;
        // Exactly what a hover leaves behind: no verdict, no failure count.
        s.last_verified = None;
        s.consecutive_failures = 0;
        s.history = vec![StepRecord {
            step: 0,
            action: DesktopAction::Hover {
                target: ClickTarget::Coordinate(Point::new(10, 10)),
            },
            result: ActionResult::success("the pointer moved onto the control".to_string()),
            verification: StepVerification::Unverifiable,
            screenshot_before: None,
            screenshot_after: None,
            snapshots_taken: 0,
            sensitive: false,
            aim_missed: None,
        }];

        let (_system, user) = driver().render_prompt(&s, &ctx(true));

        assert!(user.contains("image of the screen is attached"), "{user}");
        assert!(user.contains("hovered"), "the reason names the probe: {user}");
    }

    /// A visual scan of a whole tree normally stands in for the picture — but
    /// a scan cannot show that a control *reacted to the pointer*, which is the
    /// only thing a hover probe is asking. Suppressing the frame there would
    /// make the probe cost a step and answer nothing.
    #[test]
    fn a_scan_does_not_stand_in_for_the_frame_a_hover_probed() {
        let mut s = state();
        s.step = 1;
        s.last_verified = None;
        s.history = vec![StepRecord {
            step: 0,
            action: DesktopAction::Hover {
                target: ClickTarget::Coordinate(Point::new(10, 10)),
            },
            result: ActionResult::success("hovered".to_string()),
            verification: StepVerification::Unverifiable,
            screenshot_before: None,
            screenshot_after: None,
            snapshots_taken: 0,
            sensitive: false,
            aim_missed: None,
        }];
        let mut context = ctx(true);
        context.vision = Some(vision_elements());

        let (_system, user) = driver().render_prompt(&s, &context);

        assert!(user.contains("Detected from the screenshot:"), "{user}");
        assert!(user.contains("image of the screen is attached"), "{user}");
    }

    /// And a probe followed by its move-away is spent: the pointer is no longer
    /// on the control, so the frame says nothing about how the control reacted.
    #[test]
    fn a_pointer_move_after_a_hover_ends_the_probe() {
        let mut s = state();
        s.step = 2;
        s.last_verified = None;
        s.history = vec![
            StepRecord {
                step: 0,
                action: DesktopAction::Hover {
                    target: ClickTarget::Coordinate(Point::new(10, 10)),
                },
                result: ActionResult::success("hovered".to_string()),
                verification: StepVerification::Unverifiable,
                screenshot_before: None,
                screenshot_after: None,
                snapshots_taken: 0,
                sensitive: false,
                aim_missed: None,
            },
            StepRecord {
                step: 1,
                action: DesktopAction::PointerMove { to: Point::new(900, 600) },
                result: ActionResult::success("moved".to_string()),
                verification: StepVerification::Verified,
                screenshot_before: None,
                screenshot_after: None,
                snapshots_taken: 0,
                sensitive: false,
                aim_missed: None,
            },
        ];

        let (_system, user) = driver().render_prompt(&s, &ctx(true));

        assert!(
            !user.contains("image of the screen is attached"),
            "the hover it followed is over: {user}"
        );
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

    /// A visual scan on a screen whose tree is whole replaces the image.
    ///
    /// This is the one case where the scan genuinely is a second reading: a
    /// complete tree already names and addresses every control, so the boxes
    /// add nothing the model cannot already act on, and the picture is a
    /// duplicate.
    #[test]
    fn a_visual_scan_replaces_the_image_when_the_tree_is_whole() {
        let mut context = ctx(true);
        context.vision = Some(vision_elements());
        let (_system, user) = driver().render_prompt(&state(), &context);
        assert!(
            !user.contains("image of the screen is attached"),
            "a scan that answered over a whole tree stands in for the image: {user}"
        );
        assert!(user.contains("Detected from the screenshot:"), "{user}");
    }

    /// An incomplete tree keeps the image even when the scan answered.
    ///
    /// This is the ordering the plan turns on: local re-look is empty without
    /// it. The detector reads a 640×640 resize, so on a wide screen a small
    /// control is absent from its boxes — and a control the scan did not name
    /// is one the model will never ask to see closer, because the only picture
    /// it was given is the one the boxes describe. Suppressing the image here
    /// does not save a duplicate; it removes the one view that could show what
    /// the scan missed.
    ///
    /// Falsified before being kept: this failed against a predicate that only
    /// spared tree-less screens, which is what this file held until the plan's
    /// ordering was re-checked.
    #[test]
    fn an_incomplete_tree_keeps_the_image_however_good_the_scan() {
        let mut context = ctx(false);
        context.vision = Some(vision_elements());
        let (_system, user) = driver().render_prompt(&state(), &context);
        assert!(
            user.contains("image of the screen is attached"),
            "an incomplete tree still needs the eye the scan does not have: {user}"
        );
        assert!(
            user.contains("Detected from the screenshot:"),
            "the scan is still an index to aim with: {user}"
        );
    }

    /// On a tree-less screen the scan does **not** replace the image.
    ///
    /// Two reasons, and neither is about cost. The detector sees a fixed
    /// 640×640 resize of the screen, so it misses anything small; the picture
    /// the model receives is up to 1024 wide and is read by the model itself.
    /// And a model cannot ask to look closer at a region of a picture it was
    /// never shown — the image is what makes "look there" a request the run can
    /// act on.
    #[test]
    fn a_visual_scan_keeps_the_image_on_a_tree_less_screen() {
        let mut context = ctx(true);
        context.tree.clear();
        context.tree_complete = false;
        context.vision = Some(vision_elements());
        let (_system, user) = driver().render_prompt(&state(), &context);

        assert!(
            user.contains("image of the screen is attached"),
            "the picture is the only full-resolution view on a tree-less screen: {user}"
        );
        assert!(
            user.contains("Detected from the screenshot:"),
            "the scan is still the index the model aims with: {user}"
        );
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

    /// The system prompt says "never type into a field marked
    /// `[sensitive: ...]`". That rule was unenforceable for as long as the
    /// marker was never written: `compose` passed an empty map to the tree
    /// outline, so a password field in the accessibility tree rendered without
    /// a marker — and on a tree-less screen the scan's copy of the same field
    /// had no marker either.
    #[test]
    fn a_sensitive_field_carries_its_marker_into_the_prompt() {
        use crate::computer::types::{Rect, Tri, UiElement};

        let mut context = ctx(true);
        context.tree = vec![UiElement {
            id: "password-1".to_string(),
            role: "password_text".to_string(),
            label: Some("Password".to_string()),
            value: Some("hunter2".to_string()),
            bounds: Some(Rect::new(0, 0, 200, 24)),
            enabled: Tri::Known(true),
            focused: Tri::Known(false),
            actions: None,
            children: vec![],
            children_count: None,
            truncated: false,
        }];
        let (_system, user) = driver().render_prompt(&state(), &context);

        assert!(
            user.contains("[sensitive: password_field]"),
            "the marker the system prompt names must be on the line: {user}"
        );
        assert!(
            !user.contains("hunter2"),
            "a sensitive field's value is the secret being warned about: {user}"
        );
    }

    /// The same rule on a tree-less screen, where the only element list is the
    /// scan's. A password field found by OCR/detection is exactly as sensitive
    /// as one the platform reported, and the model is told the same thing
    /// about it.
    #[test]
    fn a_sensitive_field_found_visually_carries_its_marker() {
        use crate::computer::types::{Rect, Tri, UiElement};

        let mut context = ctx(false);
        let mut scan = vision_elements();
        let mut roots = vec![UiElement {
            id: String::new(),
            role: "text".to_string(),
            label: Some("Password".to_string()),
            value: None,
            bounds: Some(Rect::new(10, 40, 200, 24)),
            enabled: Tri::Unknown,
            focused: Tri::Unknown,
            actions: None,
            children: vec![],
            children_count: None,
            truncated: false,
        }];
        let id = crate::computer::element_index::next_snapshot_id();
        crate::computer::element_index::assign_ids(&mut roots, id);
        scan.snapshot.roots = roots;

        context.vision = Some(scan);
        let (_system, user) = driver().render_prompt(&state(), &context);

        assert!(user.contains("Detected from the screenshot:"), "{user}");
        assert!(
            user.contains("[sensitive: password_field]"),
            "a visually found password field needs the marker too: {user}"
        );
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

    /// A step that has spent the run's image allowance must say so.
    ///
    /// This is the fourth kind of blindness and the one that used to be
    /// silent. A tree-less screen attaches a picture every step, so the
    /// allowance runs out mid-run; past it the picture simply stops arriving,
    /// and a model that does not know it is blind keeps planning as though it
    /// could see. The other three (failed capture, unreadable tree, truncated
    /// tree) are all stated — this one now is too.
    #[test]
    fn a_step_past_the_image_allowance_says_the_picture_is_gone() {
        let mut context = ctx(false);
        context.image_budget_available = false;
        let (_system, user) = driver().render_prompt(&state(), &context);
        assert!(user.contains("used its image allowance"), "{user}");
        // It must not claim the screen is unseeable in general: the capture
        // still works, and a model told "the screen cannot be captured" would
        // stop asking for close-ups it can still get.
        assert!(!user.contains("could NOT be captured"), "{user}");
        assert!(
            !user.contains("An image of the screen is attached"),
            "the prompt must not promise a picture it did not send: {user}"
        );
    }

    /// The notice is keyed on the budget, not on the model being sighted.
    ///
    /// A text-only model gets no image on any step, and telling it about an
    /// allowance it never had would be noise on every single step.
    #[test]
    fn a_blind_model_is_not_told_about_an_image_allowance() {
        let mut context = ctx(true);
        context.model_supports_vision = false;
        let (_system, user) = driver().render_prompt(&state(), &context);
        assert!(!user.contains("used its image allowance"), "{user}");
    }

    /// A window list is only useful if the titles survive into the prompt.
    ///
    /// `list_windows` is the step that answers "what can I arrange?": every
    /// window action takes a title pattern, and the result's *message* is a
    /// count. The titles live in `data`, which `render_step`'s one 120-char
    /// line never sees — so before this, the model was told "Found 3 windows"
    /// and had nothing to name.
    #[test]
    fn a_window_list_reaches_the_prompt_with_its_titles() {
        let mut s = state();
        s.step = 1;
        let mut result = ActionResult::success("Found 2 windows".to_string());
        result.data = Some(serde_json::json!({
            "windows": [
                {
                    "id": "101",
                    "title": "Notes — Shopping",
                    "app_name": "Notes",
                    "pid": 101,
                    "bounds": null,
                    "minimized": true,
                    "maximized": false,
                    "focused": false
                },
                {
                    "id": "202",
                    "title": "Calculator",
                    "app_name": "Calculator",
                    "pid": 202,
                    "bounds": null,
                    "minimized": false,
                    "maximized": false,
                    "focused": true
                }
            ]
        }));
        s.history = vec![StepRecord {
            step: 0,
            action: DesktopAction::ListWindows,
            result,
            verification: StepVerification::Verified,
            screenshot_before: None,
            screenshot_after: None,
            snapshots_taken: 0,
            sensitive: false,
            aim_missed: None,
        }];

        let (_system, user) = driver().render_prompt(&s, &ctx(false));

        assert!(user.contains("Notes — Shopping"), "the title must survive: {user}");
        assert!(user.contains("Calculator"), "every window, not just the first: {user}");
        // The count alone is exactly what the model already got, and it names
        // nothing it can pass to `activate_window`.
        assert!(user.contains("Notes: Notes — Shopping"), "app and title together: {user}");
        // "Hidden in the Dock" and "behind the front window" are the same
        // observation everywhere else — the tree folds it and the screenshot
        // cannot see it — so the flag is the only place this is readable.
        assert!(user.contains("[minimised]"), "a minimised window must say so: {user}");
    }

    /// Only `list_windows` gets the expansion.
    ///
    /// Any other action whose `data` happens to hold a `windows` key would be
    /// rendered twice — once in its message and once here — and the second
    /// copy would be indistinguishable from a list the model did not ask for.
    #[test]
    fn another_actions_data_is_not_rendered_as_a_window_list() {
        let mut s = state();
        s.step = 1;
        let mut result = ActionResult::success("clicked".to_string());
        result.data = Some(serde_json::json!({ "windows": [{ "title": "not a list" }] }));
        s.history = vec![StepRecord {
            step: 0,
            action: DesktopAction::Click {
                target: ClickTarget::Coordinate(Point::new(1, 2)),
                button: MouseButton::Left,
            },
            result,
            verification: StepVerification::Verified,
            screenshot_before: None,
            screenshot_after: None,
            snapshots_taken: 0,
            sensitive: false,
            aim_missed: None,
        }];

        let (_system, user) = driver().render_prompt(&s, &ctx(false));
        assert!(!user.contains("not a list"), "{user}");
    }

    /// An empty list is stated rather than silently omitted.
    ///
    /// "The desktop reported no windows" is a fact the model can act on (it
    /// may need to launch the app first); a missing line reads as a step that
    /// said nothing, which invites a retry.
    #[test]
    fn an_empty_window_list_says_it_is_empty() {
        let mut s = state();
        s.step = 1;
        let mut result = ActionResult::success("Found 0 windows".to_string());
        result.data = Some(serde_json::json!({ "windows": [] }));
        s.history = vec![StepRecord {
            step: 0,
            action: DesktopAction::ListWindows,
            result,
            verification: StepVerification::Verified,
            screenshot_before: None,
            screenshot_after: None,
            snapshots_taken: 0,
            sensitive: false,
            aim_missed: None,
        }];

        let (_system, user) = driver().render_prompt(&s, &ctx(false));
        assert!(user.contains("reported no windows"), "{user}");
    }

    /// Linux and Windows answer `list_windows` with text, not typed records.
    ///
    /// Their titles arrive in the step's *message* (`"Windows:\n<wmctrl -l>"`,
    /// or the PowerShell `EnumAll` output), and a message goes through
    /// `summary_of`, which cuts at 120 characters. The fixture below is longer
    /// than that on purpose: the title it asserts on sits past the cut, so the
    /// test fails on the old one-line-only rendering instead of passing because
    /// a short list happened to fit.
    #[test]
    fn a_text_window_list_reaches_the_prompt_with_its_lines() {
        let mut s = state();
        s.step = 1;
        s.history = vec![StepRecord {
            step: 0,
            action: DesktopAction::ListWindows,
            result: ActionResult::success(
                "Windows:\n0x03a00007  0 terminal  Terminal — zsh\n\
                 0x03c00003  0 notes  Notes — Shopping list for Saturday\n\
                 0x03d00001  0 calc  Calculator\n\
                 0x03e00009  0 safari  Safari — Release notes\n",
            ),
            verification: StepVerification::Verified,
            screenshot_before: None,
            screenshot_after: None,
            snapshots_taken: 0,
            sensitive: false,
            aim_missed: None,
        }];

        let (_system, user) = driver().render_prompt(&s, &ctx(false));
        // Past the 120-character cut: this is the assertion that fails on the
        // old rendering, where the whole list became one truncated line.
        assert!(user.contains("Release notes"), "the last title must survive: {user}");
        assert!(user.contains("Calculator"), "{user}");
        assert!(user.contains("named windows"), "the lines must be carried as a list: {user}");
    }

    /// A macOS result without typed `data` falls back to its one-line summary.
    ///
    /// The `"Windows:"` prefix is the text platforms' marker. macOS says
    /// "Found 3 windows"; matching that as a list would print the count twice
    /// and imply a list that was not there.
    #[test]
    fn a_count_is_not_mistaken_for_a_window_list() {
        let mut s = state();
        s.step = 1;
        s.history = vec![StepRecord {
            step: 0,
            action: DesktopAction::ListWindows,
            result: ActionResult::success("Found 3 windows".to_string()),
            verification: StepVerification::Verified,
            screenshot_before: None,
            screenshot_after: None,
            snapshots_taken: 0,
            sensitive: false,
            aim_missed: None,
        }];

        let (_system, user) = driver().render_prompt(&s, &ctx(false));
        assert!(user.contains("Found 3 windows"), "{user}");
        assert!(!user.contains("named windows"), "no list was present to name: {user}");
    }

    /// A very long list is truncated with a count, never silently.
    ///
    /// Dropping the tail without saying so would let the model conclude the
    /// window it wants is not on screen, which is a different fact.
    #[test]
    fn a_long_window_list_says_how_many_were_left_out() {
        let windows: Vec<serde_json::Value> = (0..25)
            .map(|i| serde_json::json!({ "id": i.to_string(), "title": format!("w{i}") }))
            .collect();
        let mut result = ActionResult::success("Found 25 windows".to_string());
        result.data = Some(serde_json::json!({ "windows": windows }));
        let mut s = state();
        s.step = 1;
        s.history = vec![StepRecord {
            step: 0,
            action: DesktopAction::ListWindows,
            result,
            verification: StepVerification::Verified,
            screenshot_before: None,
            screenshot_after: None,
            snapshots_taken: 0,
            sensitive: false,
            aim_missed: None,
        }];

        let (_system, user) = driver().render_prompt(&s, &ctx(false));
        assert!(user.contains("and 5 more"), "the omitted tail must be counted: {user}");
        assert!(user.contains("w0"), "{user}");
        assert!(!user.contains("w24"), "past the limit: {user}");
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
                aim_missed: None,
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
                aim_missed: None,
            },
        ];
        let (_system, user) = driver().render_prompt(&s, &ctx(true));
        assert!(user.contains("1. launch Calculator → verified"), "{user}");
        assert!(user.contains("2. click [s1:0.4] → FAILED"), "{user}");
    }

    /// A miss is named in the prompt, not just counted as "unchecked".
    ///
    /// The bug this pins: the aim verdict reached the run as a three-state
    /// boolean and stopped there. "Last step: unchecked" is true of a click
    /// that missed and of a step nothing could check, and the second reading
    /// invites the model to repeat the click verbatim. Naming the box is what
    /// lets it move instead.
    #[test]
    fn a_click_whose_change_landed_elsewhere_says_so_in_the_prompt() {
        let mut s = state();
        s.step = 1;
        s.last_verified = None;
        s.history = vec![StepRecord {
            step: 0,
            action: DesktopAction::Click {
                target: ClickTarget::Coordinate(Point::new(10, 10)),
                button: MouseButton::Left,
            },
            result: ActionResult::success("clicked".to_string()),
            verification: StepVerification::Unverifiable,
            screenshot_before: None,
            screenshot_after: None,
            snapshots_taken: 0,
            sensitive: false,
            aim_missed: Some(crate::computer::Rect::new(0, 0, 64, 64)),
        }];
        let (_system, user) = driver().render_prompt(&s, &ctx(true));
        assert!(user.contains("unchecked"), "{user}");
        assert!(user.contains("the visible change is elsewhere"), "{user}");
        assert!(user.contains("64x64"), "the box is named: {user}");
        assert!(
            user.contains(&crate::computer::vision::off_target_note(&crate::computer::Rect::new(
                0, 0, 64, 64
            ))),
            "the tool and the run must word it the same way: {user}"
        );
    }

    /// A step with nothing to say about its aim adds no line: an empty note
    /// would train the model to skim the one that matters.
    #[test]
    fn a_verified_step_adds_no_aim_note() {
        let mut s = state();
        s.history = vec![StepRecord {
            step: 0,
            action: DesktopAction::Click {
                target: ClickTarget::Coordinate(Point::new(10, 10)),
                button: MouseButton::Left,
            },
            result: ActionResult::success("clicked".to_string()),
            verification: StepVerification::Verified,
            screenshot_before: None,
            screenshot_after: None,
            snapshots_taken: 0,
            sensitive: false,
            aim_missed: None,
        }];
        let (_system, user) = driver().render_prompt(&s, &ctx(true));
        assert!(!user.contains("elsewhere"), "{user}");
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
                aim_missed: None,
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

    // ── the action vocabulary beyond click and type ─────────────────────
    //
    // These were facts about the type, not about any test: `WireAction` had no
    // way to say them, so a model watching a slider, a kanban column or a
    // scrollbar had nothing to say and the run could not do what a person
    // would. Each is parsed from the JSON a model actually writes.

    #[test]
    fn a_drag_between_two_points_becomes_a_drag() {
        let decision = parse_decision(
            r#"{"decision": "act", "action": {"type": "drag", "x": 10, "y": 20,
                "to_x": 300, "to_y": 400}}"#,
        )
        .unwrap();
        assert_eq!(
            decision,
            LoopDecision::Action(DesktopAction::Drag {
                from: ClickTarget::Coordinate(Point::new(10, 20)),
                to: ClickTarget::Coordinate(Point::new(300, 400)),
            })
        );
    }

    #[test]
    fn a_drag_can_name_both_ends_by_element_id() {
        let decision = parse_decision(
            r#"{"decision": "act", "action": {"type": "drag",
                "element_id": "s1:0.1", "to_element_id": "s1:0.2"}}"#,
        )
        .unwrap();
        assert_eq!(
            decision,
            LoopDecision::Action(DesktopAction::Drag {
                from: ClickTarget::ElementId("s1:0.1".to_string()),
                to: ClickTarget::ElementId("s1:0.2".to_string()),
            })
        );
    }

    /// Half a drag is not a drag. The end that was named cannot be guessed.
    #[test]
    fn a_drag_with_no_destination_is_refused() {
        let err =
            parse_decision(r#"{"decision": "act", "action": {"type": "drag", "x": 10, "y": 20}}"#)
                .unwrap_err();
        assert!(err.to_string().contains("no element_id"), "{err}");
    }

    #[test]
    fn a_wait_becomes_a_wait() {
        let decision = parse_decision(
            r#"{"decision": "act", "action": {"type": "wait", "milliseconds": 2500}}"#,
        )
        .unwrap();
        assert_eq!(decision, LoopDecision::Action(DesktopAction::Wait { milliseconds: 2500 }));
    }

    /// A wait with no duration still is a wait — one second is the default the
    /// prompt documents. What is *not* accepted is an explicit zero, which is
    /// not a step at all.
    #[test]
    fn a_wait_defaults_to_a_second_and_refuses_zero() {
        let decision =
            parse_decision(r#"{"decision": "act", "action": {"type": "wait"}}"#).unwrap();
        assert_eq!(decision, LoopDecision::Action(DesktopAction::Wait { milliseconds: 1_000 }));
        let err =
            parse_decision(r#"{"decision": "act", "action": {"type": "wait", "milliseconds": 0}}"#)
                .unwrap_err();
        assert!(err.to_string().contains("zero"), "{err}");
    }

    #[test]
    fn the_clipboard_can_be_read_and_written() {
        let set = parse_decision(
            r#"{"decision": "act", "action": {"type": "clipboard_set", "text": "hello"}}"#,
        )
        .unwrap();
        assert_eq!(
            set,
            LoopDecision::Action(DesktopAction::ClipboardSet { text: "hello".to_string() })
        );
        let get =
            parse_decision(r#"{"decision": "act", "action": {"type": "clipboard_get"}}"#).unwrap();
        assert_eq!(get, LoopDecision::Action(DesktopAction::ClipboardGet));
    }

    /// Every window action the loop can name, in one place.
    ///
    /// The set is no longer a quartet: measuring a window and the two
    /// display-relative sizes are what a side-by-side layout needs beyond move
    /// and resize. All seven parse from the wire rather than only existing on
    /// `DesktopAction`, because an action the decision maker cannot name does
    /// not exist as far as a run is concerned.
    #[test]
    fn the_window_actions_parse() {
        let close = parse_decision(
            r#"{"decision": "act", "action": {"type": "close_window", "title_pattern": "Untitled"}}"#,
        )
        .unwrap();
        assert_eq!(
            close,
            LoopDecision::Action(DesktopAction::CloseWindow {
                title_pattern: "Untitled".to_string()
            })
        );
        let list =
            parse_decision(r#"{"decision": "act", "action": {"type": "list_windows"}}"#).unwrap();
        assert_eq!(list, LoopDecision::Action(DesktopAction::ListWindows));
        let moved = parse_decision(
            r#"{"decision": "act", "action": {"type": "move_window",
                "title_pattern": "Untitled", "x": 0, "y": 0}}"#,
        )
        .unwrap();
        assert_eq!(
            moved,
            LoopDecision::Action(DesktopAction::MoveWindow {
                title_pattern: "Untitled".to_string(),
                x: 0,
                y: 0
            })
        );
        let sized = parse_decision(
            r#"{"decision": "act", "action": {"type": "resize_window",
                "title_pattern": "Untitled", "width": 800, "height": 600}}"#,
        )
        .unwrap();
        assert_eq!(
            sized,
            LoopDecision::Action(DesktopAction::ResizeWindow {
                title_pattern: "Untitled".to_string(),
                width: 800,
                height: 600
            })
        );

        // The three that finished the set. They were in `DesktopAction` — and
        // in the `computer` tool — but not here, so the loop's decision maker
        // could not name them: "put these two side by side" had move and
        // resize, but no way to measure a window first, and no way to say
        // "as large as the display allows" without knowing its size.
        let measured = parse_decision(
            r#"{"decision": "act", "action": {"type": "get_window_geometry",
                "title_pattern": "Untitled"}}"#,
        )
        .unwrap();
        assert_eq!(
            measured,
            LoopDecision::Action(DesktopAction::GetWindowGeometry {
                title_pattern: "Untitled".to_string()
            })
        );
        let minimized = parse_decision(
            r#"{"decision": "act", "action": {"type": "minimize_window",
                "title_pattern": "Untitled"}}"#,
        )
        .unwrap();
        assert_eq!(
            minimized,
            LoopDecision::Action(DesktopAction::MinimizeWindow {
                title_pattern: "Untitled".to_string()
            })
        );
        let maximized = parse_decision(
            r#"{"decision": "act", "action": {"type": "maximize_window",
                "title_pattern": "Untitled"}}"#,
        )
        .unwrap();
        assert_eq!(
            maximized,
            LoopDecision::Action(DesktopAction::MaximizeWindow {
                title_pattern: "Untitled".to_string()
            })
        );
    }

    #[test]
    fn a_zero_size_resize_is_refused() {
        let err = parse_decision(
            r#"{"decision": "act", "action": {"type": "resize_window",
                "title_pattern": "Untitled", "width": 0, "height": 600}}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("zero"), "{err}");
    }

    /// The prompt must say the scrollbar's position is the answer.
    ///
    /// "Scroll to find the thing, and know you reached the bottom" is the one
    /// scenario the plan credits to perception alone (item 4) rather than to an
    /// action. It only works if the model is told that a thumb at the end of
    /// its track *is* the end — otherwise it spends rounds scrolling into
    /// nothing, which is the behaviour the sentence exists to replace.
    #[test]
    fn the_prompt_says_a_scrollbar_position_is_evidence() {
        assert!(DRIVER_SYSTEM_PROMPT.contains("scrollbar"));
        assert!(
            DRIVER_SYSTEM_PROMPT.contains("nothing more this way"),
            "the prompt must name what an end-of-track thumb means"
        );
        // And the other half: a middle thumb is not the end, however bare the
        // pane looks — content may still be arriving.
        assert!(DRIVER_SYSTEM_PROMPT.contains("A thumb in the middle is not the end"));
    }

    /// A drag is one blind gesture, so the prompt must teach the correction.
    ///
    /// The gesture runs from the press to the release inside the platform's
    /// script — there is no frame in the middle for the model to look at. What
    /// a person does instead is drag roughly, look at where the thumb ended up,
    /// and drag again by the remainder. Without that sentence the model spends
    /// one long drag per step aiming for an exact pixel it cannot see, and a
    /// slider lands past its target with no instruction to correct it.
    #[test]
    fn the_prompt_says_a_drag_is_aimed_blind_and_correctable() {
        assert!(
            DRIVER_SYSTEM_PROMPT.contains("a drag is one gesture you cannot watch")
                || DRIVER_SYSTEM_PROMPT.contains("A drag is one gesture you cannot watch"),
            "the prompt must say a drag cannot be watched as it happens"
        );
        assert!(
            DRIVER_SYSTEM_PROMPT.contains("drag again"),
            "the prompt must name the correction: look, then drag by the remainder"
        );
        // And the reason: it is the overshoot that makes one long drag wrong,
        // not the gesture itself.
        assert!(
            DRIVER_SYSTEM_PROMPT.contains("overshoots"),
            "the prompt must say why one long drag fails"
        );
    }

    /// The prompt must say where a `title_pattern` comes from.
    ///
    /// Every window action names a window by a substring of its title, and the
    /// only place those titles are ever printed is the `list_windows` step. A
    /// model told to `move_window` by `title_pattern` but never told where to
    /// find one guesses, and the guessed title matches nothing — which reads
    /// as "the window does not exist" rather than "I never asked for its name".
    #[test]
    fn the_prompt_says_where_a_window_title_comes_from() {
        assert!(
            DRIVER_SYSTEM_PROMPT.contains("list_windows"),
            "the prompt must point at the step that names windows"
        );
        assert!(
            DRIVER_SYSTEM_PROMPT.contains("minimised")
                || DRIVER_SYSTEM_PROMPT.contains("minimized"),
            "the prompt must explain the [minimised] marker it will be shown"
        );
    }

    /// A region screenshot is how the model asks to look closer than the
    /// detector's fixed 640×640 view. Without it, a control too small to detect
    /// was unreachable no matter how good the model was.
    #[test]
    fn a_screenshot_can_name_a_region() {
        let decision = parse_decision(
            r#"{"decision": "act", "action": {"type": "screenshot",
                "region": {"x": 100, "y": 50, "width": 300, "height": 200}}}"#,
        )
        .unwrap();
        assert_eq!(
            decision,
            LoopDecision::Action(DesktopAction::Screenshot {
                region: Some(crate::computer::types::Rect::new(100, 50, 300, 200))
            })
        );
    }

    /// The bare form still works: the region is optional, not required.
    #[test]
    fn a_screenshot_without_a_region_is_the_whole_screen() {
        let decision =
            parse_decision(r#"{"decision": "act", "action": {"type": "screenshot"}}"#).unwrap();
        assert_eq!(decision, LoopDecision::Action(DesktopAction::Screenshot { region: None }));
    }

    /// A hover names its control the same way a click does, so the model can
    /// reuse the id it already read out of the tree. It becomes `Hover`, not
    /// `Click` — the point of the gesture is that nothing is pressed.
    #[test]
    fn a_hover_names_its_control_the_way_a_click_does() {
        let by_id = parse_decision(
            r#"{"decision": "act", "action": {"type": "hover", "element_id": "s1:0.4"}}"#,
        )
        .unwrap();
        assert_eq!(
            by_id,
            LoopDecision::Action(DesktopAction::Hover {
                target: ClickTarget::ElementId("s1:0.4".to_string()),
            })
        );

        let by_point = parse_decision(
            r#"{"decision": "act", "action": {"type": "hover", "x": 120, "y": 340}}"#,
        )
        .unwrap();
        assert_eq!(
            by_point,
            LoopDecision::Action(DesktopAction::Hover {
                target: ClickTarget::Coordinate(Point::new(120, 340)),
            })
        );
    }

    /// A hover with nothing named is not a hover of "somewhere".
    #[test]
    fn a_hover_with_no_target_is_refused() {
        let err =
            parse_decision(r#"{"decision": "act", "action": {"type": "hover"}}"#).unwrap_err();
        assert!(err.to_string().contains("no element_id"), "{err}");
    }

    /// The move-away is a bare point: there is nothing to name, and nothing
    /// is pressed.
    #[test]
    fn a_pointer_move_is_a_bare_point() {
        let decision = parse_decision(
            r#"{"decision": "act", "action": {"type": "pointer_move", "x": 900, "y": 600}}"#,
        )
        .unwrap();
        assert_eq!(
            decision,
            LoopDecision::Action(DesktopAction::PointerMove { to: Point::new(900, 600) })
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

    // ── several actions against one look ────────────────────────────────
    //
    // A person filling a form looks once and acts four times. Every action in
    // a batch has to follow from the picture the decision was made against;
    // what makes the batch worth having is that the loop need not settle and
    // capture between them.

    #[test]
    fn a_list_of_actions_becomes_a_batch_in_the_order_written() {
        let decision = parse_decision(
            r#"{"decision": "act", "actions": [
                {"type": "click", "element_id": "f1"},
                {"type": "type", "text": "Ada"},
                {"type": "key_press", "keys": ["tab"]},
                {"type": "type", "text": "Lovelace"}]}"#,
        )
        .unwrap();
        let LoopDecision::Batch(actions) = decision else {
            panic!("expected a batch, got {decision:?}");
        };
        assert_eq!(actions.len(), 4);
        assert!(matches!(actions[0], DesktopAction::Click { .. }));
        assert!(matches!(actions[1], DesktopAction::Type { .. }));
        assert!(matches!(actions[2], DesktopAction::KeyPress { .. }));
        assert!(matches!(actions[3], DesktopAction::Type { .. }));
    }

    /// A list of one is a single action. The callers that treat `Action`
    /// specially — a screenshot request, most of all — must not have to know
    /// the batched form exists just because the model wrapped one action.
    #[test]
    fn a_batch_of_one_is_a_plain_action() {
        let decision = parse_decision(
            r#"{"decision": "act", "actions": [{"type": "click", "x": 10, "y": 20}]}"#,
        )
        .unwrap();
        assert_eq!(
            decision,
            LoopDecision::Action(DesktopAction::Click {
                target: ClickTarget::Coordinate(Point::new(10, 20)),
                button: MouseButton::Left,
            })
        );
    }

    /// The singular form still works, unchanged. Batched answers are an
    /// addition, not a replacement: a model that answers one action per step
    /// behaves exactly as it did before.
    #[test]
    fn the_single_action_form_still_parses() {
        let decision = parse_decision(
            r#"{"decision": "act", "action": {"type": "key_press", "keys": ["escape"]}}"#,
        )
        .unwrap();
        assert!(matches!(decision, LoopDecision::Action(DesktopAction::KeyPress { .. })));
    }

    /// A bad action anywhere in the batch refuses the whole thing. Running the
    /// good ones would be executing half of an answer the model did not mean.
    #[test]
    fn one_bad_action_refuses_the_whole_batch() {
        let err = parse_decision(
            r#"{"decision": "act", "actions": [
                {"type": "click", "x": 10, "y": 20},
                {"type": "drag", "x": 10, "y": 20}]}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no element_id"), "{err}");
    }

    /// An `act` with neither field names no action, and guessing one would be
    /// inventing a step.
    #[test]
    fn an_act_with_no_action_is_refused() {
        let err = parse_decision(r#"{"decision": "act"}"#).unwrap_err();
        assert!(err.to_string().contains("no action"), "{err}");
    }

    /// Some models write both fields, aping the two examples. Neither is
    /// dropped: the single action goes last, so the batch runs what the model
    /// listed and then what it singled out.
    #[test]
    fn both_fields_together_keep_both() {
        let decision = parse_decision(
            r#"{"decision": "act",
                "actions": [{"type": "type", "text": "first"}],
                "action": {"type": "key_press", "keys": ["enter"]}}"#,
        )
        .unwrap();
        let LoopDecision::Batch(actions) = decision else {
            panic!("expected a batch, got {decision:?}");
        };
        assert_eq!(actions.len(), 2);
        assert!(matches!(actions[0], DesktopAction::Type { .. }));
        assert!(matches!(actions[1], DesktopAction::KeyPress { .. }));
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

    /// A dense screen's picture must arrive at the model's own width.
    ///
    /// The plan's ceiling for a busy screen is "can the model find a small
    /// target on a 1024-wide picture that the detector's 640×640 pass did not
    /// box". That is only true if the picture really travels at 1024: the
    /// encode step is silent about its output size, so a regression that
    /// dropped back to the detector's frame — or to the bandwidth encoder's
    /// 1920 — would leave every test green and quietly narrow the ceiling.
    ///
    /// A truncated tree is the case that sends an image, which is what makes
    /// this the real path rather than a unit test of the encoder.
    #[cfg(feature = "vision")]
    #[tokio::test]
    async fn a_wide_capture_travels_at_the_models_own_width() {
        use image::{Rgb, RgbImage};

        // A 2560-wide screen, the width the plan names as the dense case.
        let mut frame = RgbImage::from_pixel(2560, 1440, Rgb([230, 230, 230]));
        // A small control the 640 pass would lose: 12 px on a 2560-wide screen
        // becomes ~3 px after the detector's resize.
        for y in 700..712 {
            for x in 1800..1812 {
                frame.put_pixel(x, y, Rgb([0, 0, 0]));
            }
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(frame)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode the fixture");
        let base64 = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(png.into_inner())
        };
        let shot = Screenshot::new(base64, 2560, 1440);

        let provider = Arc::new(ScriptedProvider::answering(
            r#"{"decision": "help", "reason": "cannot see"}"#,
        ));
        let driver = ModelDriver::new(provider.clone(), "test-model");
        let mut context = ctx(false);
        context.screenshot = Some(shot);

        driver.decide(&state(), &context).await.unwrap();

        let request = &provider.requests()[0];
        let user = request
            .messages
            .iter()
            .find(|m| m.role == crate::providers::Role::User)
            .expect("a user message");
        let image = user
            .content_blocks
            .as_ref()
            .and_then(|blocks| {
                blocks.iter().find_map(|block| match block {
                    crate::providers::ContentBlock::Image { base64, .. } => Some(base64.clone()),
                    _ => None,
                })
            })
            .expect("the picture travelled");
        let bytes = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(&image)
                .expect("the payload decodes")
        };
        let decoded = image::load_from_memory(&bytes).expect("the payload is an image");
        assert_eq!(
            decoded.width(),
            crate::computer::image_budget::ImageLimit::default().max_width,
            "a dense screen's picture must arrive at the limit, not the detector's 640"
        );
        assert!(
            decoded.width() > 640,
            "the detector reads 640 wide; a picture no wider shows the model nothing more"
        );

        // And the prompt must say the picture is not the size of the screen.
        // Without this the model reads a position off a 1024-wide picture and
        // emits it as a screen point — about 2.5× short on this fixture.
        let text = &user.content;
        assert!(
            text.contains("is not a screen position"),
            "a scaled picture must say it is scaled: {text}"
        );
        assert!(
            text.contains("multiply by about 2.50"),
            "the prompt must give the factor, not just warn: {text}"
        );
    }

    /// A close-up's positions need the crop's origin added, not just a factor.
    ///
    /// The crop's first pixel is `region.x`, so the mapping is
    /// `origin + image × scale`. A note that gave only the factor would place
    /// every position in a close-up short by the distance from the screen's
    /// corner — the same error as reading a raw screen position off a scaled
    /// picture, arriving from the other direction. The fixture is deliberately
    /// a crop nowhere near the origin, so omitting the offset is visible.
    #[tokio::test]
    async fn a_close_up_says_to_add_the_crops_origin() {
        use image::{Rgb, RgbImage};

        // 2560 wide, cropped to a 1200-wide region starting at x = 900.
        let frame = RgbImage::from_pixel(2560, 1440, Rgb([230, 230, 230]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(frame)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode the fixture");
        let base64 = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(png.into_inner())
        };

        let provider = Arc::new(ScriptedProvider::answering(
            r#"{"decision": "help", "reason": "cannot see"}"#,
        ));
        let driver = ModelDriver::new(provider.clone(), "test-model");
        let mut context = ctx(false);
        // The crop is what the model asked for and is now looking at.
        context.screenshot = Some(Screenshot::new(base64, 1200, 700));
        context.capture_region = Some(crate::computer::types::Rect::new(900, 300, 1200, 700));

        driver.decide(&state(), &context).await.unwrap();

        let request = &provider.requests()[0];
        let user = request
            .messages
            .iter()
            .find(|m| m.role == crate::providers::Role::User)
            .expect("a user message");
        let text = &user.content;
        assert!(
            text.contains("it covers the rectangle from (900, 300)"),
            "the close-up must say what it covers: {text}"
        );
        // A 1200-wide crop is over the 1024 limit, so the encoder shrank it and
        // the note must fire — with the origin, not only the factor.
        assert!(
            text.contains("is not a screen position"),
            "a scaled close-up must say it is scaled: {text}"
        );
        assert!(
            text.contains("add the crop's origin (900, 300)"),
            "the offset is what a crop needs and a factor alone does not give: {text}"
        );
    }

    /// A crop the encoder left alone is still not in screen points.
    ///
    /// On a 2× display a 400×250-point crop is captured at 800×500 pixels.
    /// That is under the image limit, so the payload travels unchanged and
    /// `Screenshot.width` (the encoded pixel width) equals the image's width —
    /// nothing was shrunk. Comparing the image against the *capture's pixels*
    /// therefore says "identity mapping" and stays silent, while every position
    /// in the picture is still twice the screen distance the model means.
    ///
    /// The area the picture covers is the region's own size in points, so that
    /// is what the factor divides by: 400 / 800 = 0.50, not 1.00.
    #[tokio::test]
    async fn a_retina_close_up_scales_by_the_regions_points_not_its_pixels() {
        use image::{Rgb, RgbImage};

        // A 2× capture: 400×250 points at (900, 300) arrives as 800×500 px.
        let frame = RgbImage::from_pixel(800, 500, Rgb([240, 240, 240]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(frame)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode the fixture");
        let base64 = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(png.into_inner())
        };

        let provider = Arc::new(ScriptedProvider::answering(
            r#"{"decision": "help", "reason": "cannot see"}"#,
        ));
        let driver = ModelDriver::new(provider.clone(), "test-model");
        let mut context = ctx(false);
        // `Screenshot` reports the encoded size, so it says 800×500 — the same
        // as the payload. The region says 400×250 points.
        context.screenshot = Some(Screenshot::new(base64, 800, 500));
        context.capture_region = Some(crate::computer::types::Rect::new(900, 300, 400, 250));

        driver.decide(&state(), &context).await.unwrap();

        let request = &provider.requests()[0];
        let user = request
            .messages
            .iter()
            .find(|m| m.role == crate::providers::Role::User)
            .expect("a user message");
        let text = &user.content;
        assert!(
            text.contains("is not a screen position"),
            "an unshrunk 2× crop is still not in screen points: {text}"
        );
        assert!(
            text.contains("multiply by about 0.50"),
            "the factor must be points per pixel (400/800), not 1.00: {text}"
        );
        assert!(
            text.contains("add the crop's origin (900, 300)"),
            "the offset is still needed: {text}"
        );
    }

    /// A picture that is already the screen's size says nothing about scale.
    ///
    /// The note is a correction, and a correction on an identity mapping is
    /// noise that costs a sentence of the model's attention on every step.
    #[tokio::test]
    async fn an_unscaled_picture_does_not_mention_scale() {
        let provider = Arc::new(ScriptedProvider::answering(
            r#"{"decision": "help", "reason": "cannot see"}"#,
        ));
        let driver = ModelDriver::new(provider.clone(), "test-model");
        // The one-pixel fixture is under the limit, so it travels unchanged.
        let mut context = ctx(false);
        context.screenshot = Some(Screenshot::new(ONE_PIXEL_PNG.to_string(), 1, 1));

        driver.decide(&state(), &context).await.unwrap();

        let request = &provider.requests()[0];
        let user = request
            .messages
            .iter()
            .find(|m| m.role == crate::providers::Role::User)
            .expect("a user message");
        assert!(
            !user.content.contains("is not a screen position"),
            "an identity mapping needs no correction: {}",
            user.content
        );
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
