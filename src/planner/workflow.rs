//! Workflow recorder and replay — record a desktop task once, then run it again.
//!
//! ```text
//! computer tool ──▶ Recording ──▶ Workflow (parameterised, on disk)
//!                                          ↓
//!                               WorkflowPlayer ──▶ ComputerSession (ladder + verification)
//! ```
//!
//! Three properties make a recording worth keeping, and each one is a decision
//! rather than an implementation detail:
//!
//! - **A replay repeats a *task*, not a trajectory.** A recorded click records
//!   the element it was aimed at, by name — never the snapshot id the model saw,
//!   which names nothing outside the tree that produced it. A target that cannot
//!   be given a name is recorded as it was, and the replay then fails loudly
//!   instead of clicking whatever now sits at those coordinates.
//! - **What the model typed is what varies.** Every typed string becomes a
//!   `{{parameter}}`; binding different values is the whole point of keeping the
//!   workflow. A workflow that must type a constant can be edited — it is a JSON
//!   file — or recorded again for the case that differs.
//! - **A sensitive step needs a person every time it is replayed.** The
//!   recording knows a step aimed at a password / payment / destructive
//!   confirmation; a replay must not inherit the approval given to the recording
//!   session. [`Recording::note`] carries that fact and the replay path refuses
//!   to skip it silently.
//!
//! Replay goes through [`ComputerSession`], not straight to the adapter, so a
//! replayed step walks the same action ladder and gets the same three-state
//! verification a fresh decision would. Calling `adapter.execute` directly would
//! make "the replay did the same thing" a claim about bytes sent rather than
//! about the screen.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::computer::element_index::ResolvedElement;
use crate::computer::session::ComputerSession;
use crate::computer::use_loop::StepVerification;
use crate::computer::{ActionEffect, ClickTarget, ComputerAdapter, DesktopAction};

/// How long a replay waits out a recorded pause between two steps, at most.
///
/// A recording's delay is mostly the person: opening a menu, reading the screen,
/// deciding. Replaying that literally adds minutes to a task the machine can do
/// at once. The pause is kept — some of it is a dialog animating — but only up
/// to this, which is longer than any UI transition and shorter than any thought.
pub const MAX_REPLAY_DELAY: Duration = Duration::from_secs(3);

/// A single recorded action with the delay before it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedStep {
    /// Time elapsed since the previous step.
    #[serde(default, rename = "delay_before_ms", with = "delay_ms")]
    pub delay_before: Duration,
    /// The action to execute.
    pub action: WorkflowAction,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Whether the step aimed at a password / payment / destructive
    /// confirmation control when it was recorded.
    ///
    /// Captured from the step's own evidence rather than re-derived: the replay
    /// runs on a screen that has moved on, and "is this the same dialog" is not
    /// a question the replay can answer.
    #[serde(default)]
    pub sensitive: bool,
}

/// Actions that can be recorded in a workflow.
///
/// Adjacently tagged (`{"kind": "desktop", "action": {...}}`) rather than
/// internally tagged: `DesktopAction` has unit variants, which internally tagged
/// newtype variants cannot carry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "action", rename_all = "snake_case")]
pub enum WorkflowAction {
    /// A desktop GUI action.
    Desktop(DesktopAction),
    /// A shell command.
    Shell { command: String },
    /// Wait for a duration.
    Wait { milliseconds: u64 },
}

/// A parameterised, replayable workflow.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub steps: Vec<RecordedStep>,
    /// Parameter names inferred from the recording (e.g. `filename`).
    #[serde(default)]
    pub parameters: Vec<String>,
    /// The screen recording taken while the workflow was recorded, relative to
    /// the artifact directory.
    ///
    /// `None` unless `[computer] record_video` was on and the capture produced
    /// frames. Kept on the workflow rather than in a side file because it is
    /// part of what was recorded: a replay that goes wrong is best explained by
    /// the picture of the original, and a reference nobody can find from the
    /// workflow is a reference nobody uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<String>,
}

impl Workflow {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            steps: Vec::new(),
            parameters: Vec::new(),
            video: None,
        }
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Replace parameter placeholders with actual values.
    pub fn bind_parameters(&self, values: HashMap<String, String>) -> Vec<RecordedStep> {
        self.guarded_bind(&values)
    }

    /// The same, with `sensitive` carried through untouched.
    ///
    /// The description is rebound too. It is what the replay's run log shows
    /// ("type \"INV-42\" into text field \"Name\""), and a workflow recorded with
    /// `{{name}}` in the step would otherwise print the placeholder to a person
    /// reading the run while the screen shows the value they bound.
    fn guarded_bind(&self, values: &HashMap<String, String>) -> Vec<RecordedStep> {
        self.steps
            .iter()
            .cloned()
            .map(|mut step| {
                step.action = Self::substitute_in_action(&step.action, values);
                step.description = substitute_placeholders(&step.description, values);
                step
            })
            .collect()
    }

    fn substitute_in_action(
        action: &WorkflowAction,
        values: &HashMap<String, String>,
    ) -> WorkflowAction {
        let mut result = action.clone();
        match &mut result {
            WorkflowAction::Shell { command } => {
                *command = substitute_placeholders(command, values);
            }
            WorkflowAction::Desktop(DesktopAction::Type { text, .. }) => {
                *text = substitute_placeholders(text, values);
            }
            WorkflowAction::Desktop(DesktopAction::LaunchApp { name, args, .. }) => {
                *name = substitute_placeholders(name, values);
                for arg in args.iter_mut() {
                    *arg = substitute_placeholders(arg, values);
                }
            }
            _ => {}
        }
        result
    }

    /// Turn the values a recording typed into `{{parameters}}`.
    ///
    /// One rule, applied uniformly: **a typed string is an input**. That is the
    /// only distinction the recorder can make without guessing, and it is the
    /// right one — a workflow exists to be run again with different data. The
    /// name comes from the field the text went into when the recording resolved
    /// one (`Save As` → `save_as`), so the caller can read the parameters without
    /// opening the file.
    ///
    /// A literal that appears in several steps gets one parameter, not several:
    /// typing the same folder name twice is one value the caller sets once.
    pub fn parameterize(&mut self) {
        let mut chosen: Vec<(String, String)> = Vec::new();
        for step in &self.steps {
            let WorkflowAction::Desktop(DesktopAction::Type { text, target }) = &step.action else {
                continue;
            };
            // A single value name. Multi-line text is a document, not a field
            // value, and naming it after a field would be misleading.
            let literal = text.trim();
            if literal.is_empty() || literal.len() > 200 || literal.contains('\n') {
                continue;
            }
            if chosen.iter().any(|(existing, _)| existing == literal) {
                continue;
            }
            let base = parameter_name(literal, target.as_ref());
            chosen.push((literal.to_string(), unique_name(&base, &chosen)));
        }

        if chosen.is_empty() {
            return;
        }

        let mut values = HashMap::new();
        for (literal, name) in &chosen {
            values.insert(literal.clone(), format!("{{{{{name}}}}}"));
        }
        // Longest first, so a name that contains another name is substituted
        // whole rather than left with a dangling brace.
        let mut ordered: Vec<(String, String)> = values.into_iter().collect();
        ordered.sort_by_key(|(literal, _)| std::cmp::Reverse(literal.len()));

        for step in &mut self.steps {
            step.action = Self::substitute_literals(&step.action, &ordered);
            step.description = apply_literals(&step.description, &ordered);
        }
        self.parameters = chosen.iter().map(|(_, name)| name.clone()).collect();
        self.parameters.sort();
        self.parameters.dedup();
    }

    /// Replace each literal with its placeholder, longest literal first.
    fn substitute_literals(
        action: &WorkflowAction,
        replacements: &[(String, String)],
    ) -> WorkflowAction {
        let mut result = action.clone();
        match &mut result {
            WorkflowAction::Desktop(DesktopAction::Type { text, .. }) => {
                *text = apply_literals(text, replacements);
            }
            WorkflowAction::Shell { command } => {
                *command = apply_literals(command, replacements);
            }
            WorkflowAction::Desktop(DesktopAction::LaunchApp { name, args, .. }) => {
                *name = apply_literals(name, replacements);
                for arg in args.iter_mut() {
                    *arg = apply_literals(arg, replacements);
                }
            }
            _ => {}
        }
        result
    }
}

fn apply_literals(text: &str, replacements: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (literal, placeholder) in replacements {
        if literal.is_empty() {
            continue;
        }
        if out.contains(literal.as_str()) {
            out = out.replace(literal.as_str(), placeholder);
        }
    }
    out
}

/// Fill `{{name}}` placeholders with the values bound to them.
///
/// The inverse of [`apply_literals`], and the same one-directional rule: only
/// a bound name is replaced. An unbound placeholder is left standing, because
/// the gateway refuses to replay a workflow with unbound parameters — a
/// substitution that silently emptied one would hide the bug rather than fix
/// it.
fn substitute_placeholders(text: &str, values: &HashMap<String, String>) -> String {
    let mut out = text.to_string();
    for (name, value) in values {
        let placeholder = format!("{{{{{name}}}}}");
        if out.contains(placeholder.as_str()) {
            out = out.replace(placeholder.as_str(), value);
        }
    }
    out
}

/// The parameter name a typed value should take.
///
/// A path and a URL name themselves; anything else is named after the field it
/// was typed into, falling back to `text` when the recording could not resolve
/// one. A field label is the better name because it is what the person sees on
/// screen ("Save As" → `save_as`) and it survives a re-recording of the same
/// task with different text.
fn parameter_name(literal: &str, target: Option<&ClickTarget>) -> String {
    if literal.starts_with("http://") || literal.starts_with("https://") {
        return "url".to_string();
    }
    if literal.starts_with('/') || literal.starts_with("~/") {
        let basename = literal
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        let slug = slug(basename);
        return if slug.is_empty() {
            "path".to_string()
        } else {
            slug
        };
    }
    let label = match target {
        Some(ClickTarget::ElementLabel(label)) => Some(label.as_str()),
        Some(ClickTarget::ElementRoleLabel { label, .. }) => Some(label.as_str()),
        _ => None,
    };
    label
        .map(slug)
        .filter(|slug| !slug.is_empty())
        .unwrap_or_else(|| "text".to_string())
}

fn slug(raw: &str) -> String {
    let mut out = String::new();
    let mut last_was_separator = true;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator {
            out.push('_');
            last_was_separator = true;
        }
        if out.len() >= 32 {
            break;
        }
    }
    out.trim_matches('_').to_string()
}

fn unique_name(base: &str, chosen: &[(String, String)]) -> String {
    if !chosen.iter().any(|(_, name)| name == base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}_{n}");
        if !chosen.iter().any(|(_, name)| name == &candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Whether an action is one a workflow records and replays.
///
/// Reads are excluded: a recording of "click here, type this" that also captured
/// every `screenshot` and `read_ui_tree` the decision maker asked for would
/// replay the *observation* schedule of a model, which the next run does not
/// need and which would fill a workflow with steps that do nothing.
pub fn is_replayable(action: &DesktopAction) -> bool {
    matches!(
        action,
        DesktopAction::Click { .. }
            | DesktopAction::DoubleClick { .. }
            | DesktopAction::Type { .. }
            | DesktopAction::KeyPress { .. }
            | DesktopAction::Scroll { .. }
            | DesktopAction::Drag { .. }
            | DesktopAction::LaunchApp { .. }
            | DesktopAction::ActivateWindow { .. }
            | DesktopAction::CloseWindow { .. }
            | DesktopAction::MoveWindow { .. }
            | DesktopAction::ResizeWindow { .. }
            | DesktopAction::MinimizeWindow { .. }
            | DesktopAction::MaximizeWindow { .. }
            | DesktopAction::ClipboardSet { .. }
            | DesktopAction::KillProcess { .. }
            | DesktopAction::RestartProcess { .. }
            | DesktopAction::SetProcessPriority { .. }
    )
}

/// Whether a recorded step must be confirmed by a person before it is replayed.
///
/// Two independent reasons, both known at record time:
///
/// - the step aimed at a sensitive control, and the approve-everything the
///   recorder's session was given is not a standing permission; or
/// - the step is destructive by shape — closing a window, killing a process —
///   where "it worked last time" says nothing about what is open now.
pub fn needs_confirmation(step: &RecordedStep) -> bool {
    if step.sensitive {
        return true;
    }
    matches!(
        &step.action,
        WorkflowAction::Desktop(
            DesktopAction::CloseWindow { .. }
                | DesktopAction::KillProcess { .. }
                | DesktopAction::RestartProcess { .. },
        ) | WorkflowAction::Shell { .. }
    )
}

/// The name a resolved element can be found by on a later run, if it has one.
///
/// A role and a label are both kept when the platform published both — a bare
/// label can match a different control on a different screen, and the role is
/// what stops a replay clicking a "Save" that is a menu item rather than a
/// button.
fn name_target(resolved: &ResolvedElement) -> Option<ClickTarget> {
    let label = resolved
        .label
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())?;
    let role = resolved.role.trim();
    Some(if role.is_empty() {
        ClickTarget::ElementLabel(label.to_string())
    } else {
        ClickTarget::ElementRoleLabel {
            role: role.to_string(),
            label: label.to_string(),
        }
    })
}

/// Rewrite a recorded action's element-id targets into element names.
///
/// The pure half of [`stabilize`], and the whole of the decision: a target that
/// resolves to an element with a name is recorded by name, and one that does not
/// keeps its id so the replay reports a stale target instead of guessing.
pub fn stable_target(action: &DesktopAction, resolved: Option<&ResolvedElement>) -> DesktopAction {
    let replacement = resolved.and_then(name_target);
    let mut out = action.clone();
    let rewrite = |target: &mut ClickTarget| {
        if matches!(target, ClickTarget::ElementId(_)) {
            if let Some(replacement) = replacement.clone() {
                *target = replacement;
            }
        }
    };
    match &mut out {
        DesktopAction::Click { target, .. }
        | DesktopAction::DoubleClick { target, .. }
        | DesktopAction::Scroll { target, .. } => rewrite(target),
        DesktopAction::Drag { from, to } => {
            rewrite(from);
            rewrite(to);
        }
        DesktopAction::Type { target: Some(target), .. } => rewrite(target),
        _ => {}
    }
    out
}

/// Resolve every element id a recorded action names into something a later run
/// can find.
///
/// Called by the recorder, which is the only place that has both the action and
/// the adapter that ran it. A resolution failure is not an error: the id stays,
/// and the replay reports it as a refused step rather than guessing.
pub async fn stabilize(action: &DesktopAction, adapter: &dyn ComputerAdapter) -> DesktopAction {
    let mut out = action.clone();
    let mut targets: Vec<ClickTarget> = Vec::new();
    match &out {
        DesktopAction::Click { target, .. }
        | DesktopAction::DoubleClick { target, .. }
        | DesktopAction::Scroll { target, .. } => targets.push(target.clone()),
        DesktopAction::Drag { from, to } => {
            targets.push(from.clone());
            targets.push(to.clone());
        }
        DesktopAction::Type { target: Some(target), .. } => targets.push(target.clone()),
        _ => return out,
    }
    if !targets
        .iter()
        .any(|t| matches!(t, ClickTarget::ElementId(_)))
    {
        return out;
    }

    // One resolve per distinct element, so a drag between two ids reads the
    // tree twice rather than once per occurrence.
    let mut resolved: Vec<(ClickTarget, Option<ClickTarget>)> = Vec::new();
    for target in targets {
        if resolved.iter().any(|(asked, _)| asked == &target) {
            continue;
        }
        let named = match &target {
            ClickTarget::ElementId(_) => match adapter.resolve_target(&target).await {
                Ok(found) => found.element.as_ref().and_then(name_target),
                Err(_) => None,
            },
            _ => None,
        };
        resolved.push((target, named));
    }

    let lookup = |target: &ClickTarget| -> Option<ClickTarget> {
        resolved
            .iter()
            .find(|(asked, _)| asked == target)
            .and_then(|(_, named)| named.clone())
    };
    let rewrite = |target: &mut ClickTarget| {
        if matches!(target, ClickTarget::ElementId(_)) {
            if let Some(named) = lookup(target) {
                *target = named;
            }
        }
    };
    match &mut out {
        DesktopAction::Click { target, .. }
        | DesktopAction::DoubleClick { target, .. }
        | DesktopAction::Scroll { target, .. } => rewrite(target),
        DesktopAction::Drag { from, to } => {
            rewrite(from);
            rewrite(to);
        }
        DesktopAction::Type { target: Some(target), .. } => rewrite(target),
        _ => {}
    }
    out
}

// ── Recording ────────────────────────────────────────────────────────────────

/// Records user actions into a workflow.
#[derive(Debug)]
pub struct WorkflowRecorder {
    steps: Vec<RecordedStep>,
    recording: bool,
    last_action_time: Option<Instant>,
}

impl Default for WorkflowRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowRecorder {
    pub fn new() -> Self {
        Self {
            steps: Vec::new(),
            recording: false,
            last_action_time: None,
        }
    }

    /// Start recording.
    pub fn start_recording(&mut self) {
        self.recording = true;
        self.steps.clear();
        self.last_action_time = Some(Instant::now());
    }

    /// Stop recording and return the captured workflow.
    pub fn stop_recording(&mut self, name: impl Into<String>) -> Workflow {
        self.recording = false;
        Workflow {
            name: name.into(),
            description: None,
            steps: std::mem::take(&mut self.steps),
            parameters: Vec::new(),
            video: None,
        }
    }

    /// Record a desktop action.
    pub fn record_desktop(&mut self, action: DesktopAction, description: impl Into<String>) {
        self.record_step(action, description, false);
    }

    /// Record a desktop action, saying whether it touched a sensitive control.
    pub fn record_step(
        &mut self,
        action: DesktopAction,
        description: impl Into<String>,
        sensitive: bool,
    ) {
        if !self.recording {
            return;
        }
        let delay = self.delay_since_last();
        self.steps.push(RecordedStep {
            delay_before: delay,
            action: WorkflowAction::Desktop(action),
            description: description.into(),
            sensitive,
        });
    }

    /// Record a shell command.
    pub fn record_shell(&mut self, command: impl Into<String>, description: impl Into<String>) {
        if !self.recording {
            return;
        }
        let delay = self.delay_since_last();
        self.steps.push(RecordedStep {
            delay_before: delay,
            action: WorkflowAction::Shell { command: command.into() },
            description: description.into(),
            sensitive: false,
        });
    }

    /// Record a wait step.
    pub fn record_wait(&mut self, milliseconds: u64, description: impl Into<String>) {
        if !self.recording {
            return;
        }
        let delay = self.delay_since_last();
        self.steps.push(RecordedStep {
            delay_before: delay,
            action: WorkflowAction::Wait { milliseconds },
            description: description.into(),
            sensitive: false,
        });
    }

    fn delay_since_last(&mut self) -> Duration {
        let now = Instant::now();
        let delay = match self.last_action_time {
            Some(t) => now.duration_since(t),
            None => Duration::ZERO,
        };
        self.last_action_time = Some(now);
        delay
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Whether a recording is in progress.
    pub fn is_recording(&self) -> bool {
        self.recording
    }
}

/// Why a recording could not be started or stopped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordingError {
    /// A recording is already running. Starting again would discard it.
    #[error("a recording is already in progress")]
    AlreadyRecording,
    /// No recording is in progress.
    #[error("no recording is in progress")]
    NotRecording,
    /// The recording captured nothing worth keeping.
    #[error("the recording captured no replayable steps")]
    Empty,
}

/// The shared recording handle.
///
/// One per gateway, not one per conversation, because there is one screen: two
/// conversations cannot usefully record different things at the same time, and a
/// second `record_start` is refused rather than silently discarding the first.
///
/// Everything here is a short synchronous section behind a `std` mutex — no
/// `.await` is held across it, which is why the recorder needs no async lock.
#[derive(Debug, Default)]
pub struct Recording {
    inner: Mutex<Option<RecordingSession>>,
    /// Monotonic id for the current session.
    ///
    /// The video side needs to know whether the recording it is capturing is
    /// still the one in progress: a `stop` then a fast `start` must not let the
    /// first encoder write into the second recording's file. An id is enough —
    /// the two sides never need to exchange anything else.
    next_id: std::sync::atomic::AtomicU64,
}

#[derive(Debug)]
struct RecordingSession {
    id: u64,
    name: Option<String>,
    started: Instant,
    recorder: WorkflowRecorder,
}

impl Recording {
    pub fn new() -> Self {
        Self::default()
    }

    /// The id of the recording in progress, or `None` when there is none.
    pub fn session_id(&self) -> Option<u64> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|s| s.id)
    }

    /// Begin recording, under the given name when one was chosen up front.
    pub fn start(&self, name: Option<String>) -> Result<(), RecordingError> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = guard.as_ref() {
            if session.recorder.is_recording() {
                return Err(RecordingError::AlreadyRecording);
            }
        }
        let mut recorder = WorkflowRecorder::new();
        recorder.start_recording();
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        *guard = Some(RecordingSession {
            id,
            name: name.filter(|n| !n.trim().is_empty()),
            started: Instant::now(),
            recorder,
        });
        Ok(())
    }

    /// Finish the recording and return it, parameterised.
    pub fn stop(&self, name: Option<String>) -> Result<Workflow, RecordingError> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut session) = guard.take() else {
            return Err(RecordingError::NotRecording);
        };
        let workflow_name = name
            .filter(|n| !n.trim().is_empty())
            .or(session.name.take())
            .unwrap_or_else(|| format!("workflow-{}", session.started.elapsed().as_secs()));
        let mut workflow = session.recorder.stop_recording(workflow_name);
        workflow.parameterize();
        if workflow.steps.is_empty() {
            return Err(RecordingError::Empty);
        }
        Ok(workflow)
    }

    /// Whether a recording is in progress.
    pub fn is_recording(&self) -> bool {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().is_some_and(|s| s.recorder.is_recording())
    }

    /// How many steps have been captured so far.
    pub fn len(&self) -> usize {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|s| s.recorder.len()).unwrap_or(0)
    }

    /// Whether nothing has been captured yet.
    ///
    /// Not the same as "no recording is running": a recording that just started
    /// and a recording with no steps would both answer `true` here, and only
    /// [`is_recording`](Self::is_recording) tells them apart.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Note one executed step.
    pub fn note(&self, action: DesktopAction, description: impl Into<String>, sensitive: bool) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = guard.as_mut() {
            session.recorder.record_step(action, description, sensitive);
        }
    }
}

// ── Replay ───────────────────────────────────────────────────────────────────

/// Result of replaying a single step.
#[derive(Debug, Clone, PartialEq)]
pub enum StepResult {
    Success {
        message: String,
        /// `None` when nothing could confirm or deny the step.
        verified: Option<bool>,
    },
    Skipped {
        reason: String,
    },
    Failed {
        error: String,
    },
    /// The step was never delivered: the platform or the recording's target
    /// refused it. Repeating it produces the same refusal, so it is not retried.
    Refused {
        reason: String,
    },
}

impl StepResult {
    /// Whether the replay should go on to the next step under `strategy`.
    pub fn should_continue(&self, strategy: FailureStrategy) -> bool {
        match self {
            StepResult::Success { .. } | StepResult::Skipped { .. } => true,
            // A refusal is terminal for this step whatever the strategy: the
            // next attempt would be refused by the same screen.
            StepResult::Refused { .. } => matches!(strategy, FailureStrategy::Skip),
            StepResult::Failed { .. } => matches!(strategy, FailureStrategy::Skip),
        }
    }

    pub fn succeeded(&self) -> bool {
        matches!(self, StepResult::Success { .. })
    }
}

/// Strategy when a step fails during replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureStrategy {
    /// Abort the entire workflow.
    Abort,
    /// Skip the failed step and continue.
    Skip,
    /// Retry the step up to N times.
    Retry { max_retries: u32, delay_ms: u64 },
}

impl FailureStrategy {
    /// Parse the name `computer.play` takes on the wire.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "abort" => Some(Self::Abort),
            // The default: a replay of a working task should get as far as it
            // can. Aborting on one skipped step turns a mostly-good run into a
            // run that reported nothing.
            "skip" => Some(Self::Skip),
            "retry" => Some(Self::Retry { max_retries: 2, delay_ms: 500 }),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Abort => "abort",
            Self::Skip => "skip",
            Self::Retry { .. } => "retry",
        }
    }
}

/// Replays a workflow against a [`ComputerSession`].
pub struct WorkflowPlayer {
    failure_strategy: FailureStrategy,
    max_delay: Duration,
}

impl Default for WorkflowPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowPlayer {
    pub fn new() -> Self {
        Self {
            failure_strategy: FailureStrategy::Skip,
            max_delay: MAX_REPLAY_DELAY,
        }
    }

    pub fn with_failure_strategy(mut self, strategy: FailureStrategy) -> Self {
        self.failure_strategy = strategy;
        self
    }

    /// Cap on the recorded pause before each step.
    pub fn with_max_delay(mut self, max_delay: Duration) -> Self {
        self.max_delay = max_delay;
        self
    }

    pub fn failure_strategy(&self) -> FailureStrategy {
        self.failure_strategy
    }

    /// The pause to wait out before `step`.
    pub fn delay_for(&self, step: &RecordedStep) -> Duration {
        step.delay_before.min(self.max_delay)
    }

    /// Replay one bound step.
    ///
    /// Through the session, so the step gets the ladder (an element that moved
    /// is still found) and the three-state verification (a step that reported
    /// success but changed nothing is `Failed`, not `Success`).
    pub async fn play_step(&self, step: &RecordedStep, session: &ComputerSession) -> StepResult {
        let max_retries = match self.failure_strategy {
            FailureStrategy::Retry { max_retries, .. } => max_retries,
            _ => 0,
        };

        let mut last_error = String::from("the step was never attempted");
        for attempt in 0..=max_retries {
            match self.try_play_step(step, session).await {
                StepResult::Failed { error } => {
                    last_error = error;
                    if attempt < max_retries {
                        if let FailureStrategy::Retry { delay_ms, .. } = self.failure_strategy {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        }
                    }
                }
                // A refusal is terminal: the same request is refused by the same
                // screen, so retrying only spends time reaching the same answer.
                terminal => return terminal,
            }
        }

        match self.failure_strategy {
            FailureStrategy::Skip => StepResult::Skipped {
                reason: format!("Failed but skipped: {last_error}"),
            },
            _ => StepResult::Failed { error: last_error },
        }
    }

    /// Replay a whole workflow without pausing or asking anyone.
    ///
    /// The gateway's `computer.play` does not use this: it drives the steps
    /// itself so it can consult the run's controller and a person between them.
    /// Kept as the plain path for a caller that wants the results in one call.
    pub async fn replay(
        &self,
        workflow: &Workflow,
        parameters: HashMap<String, String>,
        session: &ComputerSession,
    ) -> Vec<StepResult> {
        let steps = workflow.bind_parameters(parameters);
        let mut results = Vec::with_capacity(steps.len());

        for step in &steps {
            let delay = self.delay_for(step);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let result = self.play_step(step, session).await;
            let go_on = result.should_continue(self.failure_strategy);
            results.push(result);
            if !go_on {
                break;
            }
        }
        results
    }

    /// Attempt one step once, reporting rather than deciding.
    async fn try_play_step(&self, step: &RecordedStep, session: &ComputerSession) -> StepResult {
        match &step.action {
            WorkflowAction::Desktop(action) => {
                let outcome = match session.execute(action.clone()).await {
                    Ok(outcome) => outcome,
                    Err(e) => return StepResult::Failed { error: e.to_string() },
                };
                if matches!(
                    outcome.evidence.as_ref().map(|e| e.effect),
                    Some(ActionEffect::Refused)
                ) {
                    let reason = outcome
                        .evidence
                        .as_ref()
                        .and_then(|e| e.recovery.clone())
                        .unwrap_or_else(|| outcome.result.message.clone());
                    return StepResult::Refused { reason };
                }
                if !outcome.result.success {
                    return StepResult::Failed {
                        error: outcome.result.message.clone(),
                    };
                }
                // A failed verification is a failed step: the action went out and
                // the screen says it did not land. `Unverifiable` is neither, so
                // it stays a success carrying `None` — treating "nothing could
                // check it" as a failure is how a good step gets retried into
                // doing what it already did.
                if outcome.verification == StepVerification::Failed {
                    return StepResult::Failed {
                        error: format!(
                            "the action ran but the screen did not change: {}",
                            outcome.result.message
                        ),
                    };
                }
                let verified = match outcome.verification {
                    StepVerification::Verified => Some(true),
                    StepVerification::Failed => Some(false),
                    StepVerification::Unverifiable => None,
                };
                StepResult::Success {
                    message: outcome.result.message,
                    verified,
                }
            }
            WorkflowAction::Shell { command } => {
                match tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .output()
                    .await
                {
                    Ok(output) if output.status.success() => StepResult::Success {
                        message: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                        verified: None,
                    },
                    Ok(output) => StepResult::Failed {
                        error: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                    },
                    Err(e) => StepResult::Failed {
                        error: format!("shell failed: {e}"),
                    },
                }
            }
            WorkflowAction::Wait { milliseconds } => {
                tokio::time::sleep(Duration::from_millis(*milliseconds)).await;
                StepResult::Success {
                    message: format!("Waited {milliseconds}ms"),
                    verified: None,
                }
            }
        }
    }
}

// ── Storage ──────────────────────────────────────────────────────────────────

/// A workflow on disk, with a little provenance.
///
/// The file is the workflow plus what a person needs to decide whether to run
/// it: when it was recorded and how many steps it has. Versioned so a later
/// change to the step format can be recognised instead of misread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowFile {
    /// Format version. Absent in the first release, so it defaults to 1.
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(flatten)]
    pub workflow: Workflow,
    /// Unix seconds when the recording was saved.
    #[serde(default)]
    pub recorded_at: u64,
}

fn default_version() -> u32 {
    1
}

impl WorkflowFile {
    const CURRENT_VERSION: u32 = 1;

    pub fn new(workflow: Workflow) -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            workflow,
            recorded_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }
}

/// The file name for a workflow, safe to use on every platform.
///
/// A workflow is named by a person ("New folder in Finder"), and that name
/// reaches a filesystem. Slugs keep it readable; the collision suffix keeps two
/// names that slug alike from silently replacing each other. The name in the
/// JSON is the one the person typed either way.
pub fn workflow_file_stem(name: &str) -> String {
    let mut out = String::new();
    let mut last_sep = true;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_sep = false;
        } else if !last_sep && (ch.is_whitespace() || ch == '-' || ch == '_') {
            out.push('-');
            last_sep = true;
        }
    }
    let stem = out.trim_matches('-');
    if stem.is_empty() {
        "workflow".to_string()
    } else {
        stem.chars().take(64).collect()
    }
}

/// Reads and writes workflows under one directory.
///
/// Same tmp+rename as `RunStore`, and the same rule about unreadable files: one
/// corrupt workflow is reported and skipped, because a person who saved six of
/// them must not lose the list to the seventh.
#[derive(Debug, Clone, Default)]
pub struct WorkflowStore {
    dir: Option<PathBuf>,
}

impl WorkflowStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_dir(dir: PathBuf) -> Self {
        Self { dir: Some(dir) }
    }

    pub fn for_paths(paths: &crate::dirs::SyscityPaths) -> Self {
        Self::with_dir(paths.workflows_dir())
    }

    /// Where workflows live when no root was installed (a library caller, a
    /// unit test that only wants the default).
    pub fn default_dir() -> PathBuf {
        match crate::dirs::installed_paths() {
            Some(paths) => paths.workflows_dir(),
            None => crate::dirs::syscity_dir().join("workflows"),
        }
    }

    pub fn dir(&self) -> PathBuf {
        self.dir.clone().unwrap_or_else(Self::default_dir)
    }

    pub fn path_for(&self, name: &str) -> PathBuf {
        self.dir()
            .join(format!("{}.json", workflow_file_stem(name)))
    }

    async fn ensure_dir(&self) -> crate::Result<()> {
        let dir = self.dir();
        if !dir.exists() {
            tokio::fs::create_dir_all(&dir).await.map_err(|e| {
                crate::error::SyscityError::Storage {
                    context: format!("Failed to create workflows directory: {dir:?}"),
                    details: e.to_string(),
                }
            })?;
        }
        Ok(())
    }

    /// Save, returning the path written.
    pub async fn save(&self, workflow: &Workflow) -> crate::Result<PathBuf> {
        self.ensure_dir().await?;
        let path = self.path_for(&workflow.name);
        let file = WorkflowFile::new(workflow.clone());
        let json = serde_json::to_string_pretty(&file).map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to serialize workflow: {e}"))
        })?;

        let mut tmp_path = path.clone();
        let mut tmp_name = path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        tmp_name.push(".tmp");
        tmp_path.set_file_name(tmp_name);

        tokio::fs::write(&tmp_path, &json).await.map_err(|e| {
            crate::error::SyscityError::Storage {
                context: format!("Failed to write workflow tmp: {tmp_path:?}"),
                details: e.to_string(),
            }
        })?;

        tokio::fs::rename(&tmp_path, &path).await.map_err(|e| {
            // Best-effort: a stale tmp file is not the reason this failed.
            let _ = std::fs::remove_file(&tmp_path);
            crate::error::SyscityError::Storage {
                context: format!("Failed to finalize workflow: {path:?}"),
                details: e.to_string(),
            }
        })?;
        Ok(path)
    }

    /// Load one workflow by name.
    pub async fn load(&self, name: &str) -> Option<Workflow> {
        let path = self.path_for(name);
        let content = tokio::fs::read_to_string(&path).await.ok()?;
        match serde_json::from_str::<WorkflowFile>(&content) {
            Ok(file) => Some(file.workflow),
            Err(e) => {
                tracing::warn!("[workflow] unreadable workflow {path:?}: {e}");
                None
            }
        }
    }

    /// Every stored workflow, newest first.
    pub async fn list(&self) -> Vec<WorkflowFile> {
        let mut out = Vec::new();
        let mut entries = match tokio::fs::read_dir(self.dir()).await {
            Ok(entries) => entries,
            Err(_) => return out,
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => match serde_json::from_str::<WorkflowFile>(&content) {
                    Ok(file) => out.push(file),
                    Err(e) => tracing::warn!("[workflow] unreadable workflow {path:?}: {e}"),
                },
                Err(e) => tracing::warn!("[workflow] could not read workflow {path:?}: {e}"),
            }
        }
        out.sort_by_key(|f| std::cmp::Reverse(f.recorded_at));
        out
    }

    /// Remove a workflow. Returns whether there was one.
    pub async fn delete(&self, name: &str) -> bool {
        let path = self.path_for(name);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                tracing::warn!("[workflow] could not delete workflow {path:?}: {e}");
                false
            }
        }
    }
}

// ── Serde helpers ────────────────────────────────────────────────────────────

mod delay_ms {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(value.as_millis() as u64)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(deserializer)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::{ClickTarget, MouseButton, Point};

    fn click(x: i32, y: i32) -> DesktopAction {
        DesktopAction::Click {
            target: ClickTarget::Coordinate(Point::new(x, y)),
            button: MouseButton::Left,
        }
    }

    #[test]
    fn test_workflow_recorder_start_stop() {
        let mut recorder = WorkflowRecorder::new();
        recorder.start_recording();
        recorder.record_desktop(click(100, 200), "Click OK");
        recorder.record_wait(500, "Wait for dialog");
        let workflow = recorder.stop_recording("test_workflow");

        assert_eq!(workflow.name, "test_workflow");
        assert_eq!(workflow.steps.len(), 2);
        assert!(matches!(workflow.steps[0].action, WorkflowAction::Desktop(..)));
        assert!(matches!(workflow.steps[1].action, WorkflowAction::Wait { .. }));
    }

    #[test]
    fn test_workflow_parameter_binding() {
        let mut workflow = Workflow::new("deploy");
        workflow.steps.push(RecordedStep {
            delay_before: Duration::ZERO,
            action: WorkflowAction::Shell {
                command: "git clone {{repo}} /tmp/{{name}}".to_string(),
            },
            description: "Clone repo".to_string(),
            sensitive: false,
        });
        workflow.parameters = vec!["repo".to_string(), "name".to_string()];

        let mut values = HashMap::new();
        values.insert("repo".to_string(), "https://github.com/foo/bar".to_string());
        values.insert("name".to_string(), "myproject".to_string());

        let bound = workflow.bind_parameters(values);
        match &bound[0].action {
            WorkflowAction::Shell { command } => {
                assert_eq!(command, "git clone https://github.com/foo/bar /tmp/myproject");
            }
            _ => panic!("Expected shell action"),
        }
    }

    #[test]
    fn test_workflow_recorder_no_record_when_stopped() {
        let mut recorder = WorkflowRecorder::new();
        // Not started
        recorder.record_desktop(DesktopAction::Wait { milliseconds: 10 }, "should not appear");
        let workflow = recorder.stop_recording("empty");
        assert!(workflow.steps.is_empty());
    }

    #[test]
    fn test_step_result_display() {
        let r = StepResult::Success {
            message: "done".to_string(),
            verified: Some(true),
        };
        assert!(r.succeeded());
    }

    #[test]
    fn test_failure_strategy_clone_eq() {
        let s1 = FailureStrategy::Retry { max_retries: 3, delay_ms: 100 };
        let s2 = s1.clone();
        assert_eq!(s1, s2);
    }

    // ── Parameterisation ─────────────────────────────────────────────────────

    fn typed(text: &str, target: Option<ClickTarget>) -> RecordedStep {
        RecordedStep {
            delay_before: Duration::ZERO,
            action: WorkflowAction::Desktop(DesktopAction::Type { text: text.to_string(), target }),
            description: format!("type {text}"),
            sensitive: false,
        }
    }

    #[test]
    fn a_typed_value_becomes_a_parameter_named_after_its_field() {
        let mut workflow = Workflow::new("new-folder");
        workflow
            .steps
            .push(typed("Reports", Some(ClickTarget::ElementLabel("Save As".to_string()))));

        workflow.parameterize();

        assert_eq!(workflow.parameters, vec!["save_as".to_string()]);
        match &workflow.steps[0].action {
            WorkflowAction::Desktop(DesktopAction::Type { text, .. }) => {
                assert_eq!(text, "{{save_as}}");
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn a_typed_path_is_named_for_its_basename() {
        let mut workflow = Workflow::new("save");
        workflow
            .steps
            .push(typed("/Users/me/Projects/report.csv", None));
        workflow.parameterize();
        assert_eq!(workflow.parameters, vec!["report_csv".to_string()]);
    }

    #[test]
    fn one_literal_typed_twice_is_one_parameter() {
        let mut workflow = Workflow::new("rename");
        workflow.steps.push(typed("Reports", None));
        workflow.steps.push(typed("Reports", None));
        workflow.parameterize();
        assert_eq!(workflow.parameters, vec!["text".to_string()]);
        assert_eq!(workflow.parameters.len(), 1);
    }

    #[test]
    fn two_different_values_get_distinct_names() {
        let mut workflow = Workflow::new("two-fields");
        workflow.steps.push(typed("alice", None));
        workflow.steps.push(typed("hunter2", None));
        workflow.parameterize();
        assert_eq!(workflow.parameters, vec!["text".to_string(), "text_2".to_string()]);
    }

    #[test]
    fn binding_a_parameterised_workflow_produces_the_new_value() {
        let mut workflow = Workflow::new("new-folder");
        workflow
            .steps
            .push(typed("Reports", Some(ClickTarget::ElementLabel("Save As".to_string()))));
        workflow.parameterize();

        let mut values = HashMap::new();
        values.insert("save_as".to_string(), "Invoices".to_string());
        let bound = workflow.bind_parameters(values);
        match &bound[0].action {
            WorkflowAction::Desktop(DesktopAction::Type { text, .. }) => {
                assert_eq!(text, "Invoices");
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    /// The step's description is what the replay's run log prints, so a
    /// placeholder left in it would show the person reading the run
    /// `{{save_as}}` while the field on screen holds the value they bound.
    #[test]
    fn the_step_description_is_parameterised_and_rebound_with_the_action() {
        let mut workflow = Workflow::new("save-report");
        let mut step = typed("Reports", Some(ClickTarget::ElementLabel("Save As".to_string())));
        step.description = "type \"Reports\" into the Save As field".to_string();
        workflow.steps.push(step);
        workflow.parameterize();

        assert_eq!(workflow.steps[0].description, "type \"{{save_as}}\" into the Save As field");

        let mut values = HashMap::new();
        values.insert("save_as".to_string(), "Invoices".to_string());
        let bound = workflow.bind_parameters(values);
        assert_eq!(bound[0].description, "type \"Invoices\" into the Save As field");
    }

    // ── Target stabilisation ─────────────────────────────────────────────────

    fn resolved(role: &str, label: Option<&str>) -> ResolvedElement {
        ResolvedElement {
            id: "s1:0.3".to_string(),
            role: role.to_string(),
            label: label.map(str::to_string),
            center: Some(Point::new(10, 20)),
            bounds: None,
            path: vec![0, 3],
            actions: None,
            enabled: crate::computer::Tri::Known(true),
        }
    }

    #[test]
    fn a_snapshot_id_becomes_the_element_name() {
        let action = DesktopAction::Click {
            target: ClickTarget::ElementId("s1:0.3".to_string()),
            button: MouseButton::Left,
        };
        let stable = stable_target(&action, Some(&resolved("button", Some("New Folder"))));
        assert_eq!(
            stable,
            DesktopAction::Click {
                target: ClickTarget::ElementRoleLabel {
                    role: "button".to_string(),
                    label: "New Folder".to_string(),
                },
                button: MouseButton::Left,
            }
        );
    }

    #[test]
    fn an_element_with_no_name_keeps_its_id() {
        let action = DesktopAction::Click {
            target: ClickTarget::ElementId("s1:0.3".to_string()),
            button: MouseButton::Left,
        };
        assert_eq!(stable_target(&action, Some(&resolved("button", None))), action);
    }

    #[test]
    fn a_coordinate_target_is_never_rewritten() {
        let action = click(10, 20);
        assert_eq!(stable_target(&action, Some(&resolved("button", Some("OK")))), action);
    }

    // ── Recording handle ─────────────────────────────────────────────────────

    #[test]
    fn a_second_start_is_refused_rather_than_discarding_the_first() {
        let recording = Recording::new();
        recording.start(Some("one".to_string())).expect("starts");
        recording.note(click(1, 2), "Click", false);
        assert_eq!(recording.start(Some("two".to_string())), Err(RecordingError::AlreadyRecording));
        let workflow = recording.stop(None).expect("the first recording survives");
        assert_eq!(workflow.name, "one");
        assert_eq!(workflow.steps.len(), 1);
    }

    #[test]
    fn stopping_without_a_recording_is_an_error() {
        let recording = Recording::new();
        assert_eq!(recording.stop(None), Err(RecordingError::NotRecording));
    }

    #[test]
    fn an_empty_recording_is_refused() {
        let recording = Recording::new();
        recording.start(None).expect("starts");
        assert_eq!(recording.stop(Some("empty".to_string())), Err(RecordingError::Empty));
    }

    #[test]
    fn a_step_noted_outside_a_recording_is_dropped() {
        let recording = Recording::new();
        recording.note(click(1, 2), "Click", false);
        assert_eq!(recording.len(), 0);
        assert!(!recording.is_recording());
    }

    // ── Confirmation and serialisation ───────────────────────────────────────

    #[test]
    fn a_sensitive_step_and_a_destructive_one_need_a_person() {
        let mut sensitive = typed("hunter2", None);
        sensitive.sensitive = true;
        assert!(needs_confirmation(&sensitive));

        let close = RecordedStep {
            delay_before: Duration::ZERO,
            action: WorkflowAction::Desktop(DesktopAction::CloseWindow {
                title_pattern: "Untitled".to_string(),
            }),
            description: "close".to_string(),
            sensitive: false,
        };
        assert!(needs_confirmation(&close));

        let plain = RecordedStep {
            delay_before: Duration::ZERO,
            action: WorkflowAction::Desktop(click(1, 2)),
            description: "click".to_string(),
            sensitive: false,
        };
        assert!(!needs_confirmation(&plain));
    }

    #[test]
    fn a_workflow_round_trips_through_json_with_millisecond_delays() {
        let mut workflow = Workflow::new("new-folder");
        workflow.steps.push(RecordedStep {
            delay_before: Duration::from_millis(250),
            action: WorkflowAction::Desktop(click(10, 20)),
            description: "Click New Folder".to_string(),
            sensitive: true,
        });
        workflow.parameters = vec!["save_as".to_string()];

        let json = serde_json::to_string_pretty(&workflow).expect("serializes");
        assert!(json.contains("\"delay_before_ms\": 250"), "{json}");
        let back: Workflow = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, workflow);
    }

    #[test]
    fn reads_are_not_replayable_but_actions_are() {
        assert!(!is_replayable(&DesktopAction::Screenshot { region: None }));
        assert!(!is_replayable(&DesktopAction::ReadUiTree { app: None }));
        assert!(!is_replayable(&DesktopAction::Wait { milliseconds: 10 }));
        assert!(is_replayable(&click(1, 2)));
        assert!(is_replayable(&DesktopAction::KeyPress { keys: vec!["cmd".into()] }));
    }

    #[test]
    fn the_recorded_pause_is_capped_but_kept() {
        let player = WorkflowPlayer::new().with_max_delay(Duration::from_millis(500));
        let step = RecordedStep {
            delay_before: Duration::from_secs(30),
            action: WorkflowAction::Wait { milliseconds: 1 },
            description: String::new(),
            sensitive: false,
        };
        assert_eq!(player.delay_for(&step), Duration::from_millis(500));

        let short = RecordedStep {
            delay_before: Duration::from_millis(120),
            ..step
        };
        assert_eq!(player.delay_for(&short), Duration::from_millis(120));
    }

    #[test]
    fn the_default_strategy_skips_rather_than_aborting() {
        assert_eq!(WorkflowPlayer::new().failure_strategy(), FailureStrategy::Skip);
    }
}
