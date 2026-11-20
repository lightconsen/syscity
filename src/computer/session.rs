//! Computer session — the perceive → act → verify step, shared by the
//! production `computer` tool and the [`ComputerUseLoop`] orchestrator.
//!
//! [`ComputerUseLoop`]: crate::computer::use_loop::ComputerUseLoop
//!
//! The loop used to be the only place where three-state verification,
//! consecutive-failure accounting, auto-rollback and reflection lived, and
//! nothing in production ever constructed one — the `computer` tool did its
//! own ad-hoc before/after diff and printed the result as free text. This
//! session is that logic, factored out so both callers run the same code:
//!
//! - **three-state verification** — `Verified` / `Failed` / `Unverifiable`;
//!   a check that could not run moves neither counter
//! - **effect + escalation** — what was established, and which rung to try
//!   next (`px` → `foreground` → `page`)
//! - **auto-rollback** — once, at the configured consecutive-failure
//!   threshold
//! - **reflection** — a rule-based root cause for the failure, so the caller
//!   can act on it instead of retrying blindly
//! - **settling** — poll the screen until it stops changing, reusing the
//!   capture verification needs, instead of sleeping a fixed 250 ms
//! - **pinned notes** — session-scoped key/value memory, because a value read
//!   at step 30 is gone by step 300

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::computer::budget::StepBudget;
use crate::computer::display::CoordinateSpace;
use crate::computer::element_index::SnapshotCache;
use crate::computer::page::{PageAction, PageActor, PageTarget};
use crate::computer::reflection::{FailureAnalysis, ReflectionEngine};
use crate::computer::rollback::RollbackManager;
use crate::computer::sensitive_ui::{scan_sensitive, SensitiveUiFinding};
use crate::computer::use_loop::StepVerification;
use crate::computer::verification::{VerificationConfig, VerificationCriteria, VerificationEngine};
use crate::computer::vision::{
    action_changes_geometry, is_screen_mutating_action, pixel_diff, ScreenDiff, ScreenState,
};
use crate::computer::{
    ActionEffect, ActionResult, ActionTarget, ClickTarget, ComputerAdapter, ComputerError,
    DeliveryMode, DesktopAction, Escalation, Rect, Rung, Screenshot, UiElement,
};

/// Whether a step may confirm a typed value by reading it back off the screen.
///
/// The accessibility tree answers this question when it publishes values, and
/// [`typed_value_hint`] is that answer. Some fields do not publish one — a
/// canvas widget, an Electron input whose value the app never exposes, a
/// terminal — and there the tree says nothing about whether the text landed.
/// A person in that position does not give up on the question: they lean in
/// and read the field.
///
/// That is the entire feature: crop the focused field out of the frame the
/// step already captured, OCR the crop, and compare it with what was typed. It
/// is perceptual compensation for a tree that cannot answer, not a second
/// opinion about the run, and it changes no model and no structure.
///
/// # The text never leaves this module
///
/// What OCR reads is matched against the typed text and then dropped. The
/// verdict — a `StepVerification` and an `ActionEffect` — is all that travels
/// on, and `EffectHint` has no field that could carry the text even by
/// mistake. This is deliberate, and it is the difference between reading a
/// field to check a typo and reading a password back to a model.
///
/// # Why a password is never read
///
/// Two independent reasons, because one is an argument and two is a guarantee:
///
/// 1. A field that masks what it holds cannot be read. Every masked character
///    renders the same, so the crop is a row of dots and OCR returns either
///    nothing or a row of `•` — there is no text in the picture to leak.
/// 2. A field the tree marked sensitive is not cropped at all. The crop is
///    skipped before any pixels are taken, so a widget that both masks poorly
///    and renders a live password is never looked at. The findings come from
///    [`scan_sensitive`] on the same tree the field was found in, so "the tree
///    said sensitive" is the same question the prompt's `[sensitive: ...]`
///    marker answers.
///
/// A miss is a `None` — "the screen could not be read" — never a `Failed`. A
/// wrong OCR reading is not evidence that the typing failed, and turning it
/// into one would send the ladder to retype into a field that is already
/// correct.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ReadBack {
    /// Read back only when the models are already installed.
    ///
    /// The models are a separate optional download; when they are absent the
    /// read-back is skipped, and the step falls back to the pixel diff as it
    /// did before this existed. This is the default because it is the one that
    /// cannot turn typing into a fetch.
    #[default]
    InstalledModels,
    /// Never read back. The tree is the only source that may answer.
    Off,
    /// Read back from models under this directory. Same as
    /// [`InstalledModels`](Self::InstalledModels) for a directory with the
    /// files in it, and for a configured `[computer.vision].model_dir` in all
    /// cases — a fetch still never happens.
    ModelDir(String),
}

impl ReadBack {
    /// What `[computer.vision]` says about reading a field back.
    ///
    /// `mode = "off"` is the one setting that turns this off: it says the tree
    /// is the only input this deployment wants, and reading pixels back is
    /// exactly the input it meant. Every other mode leaves the decision to
    /// whether the models are on the machine, which is a `stat` per step and
    /// never a download.
    pub fn from_vision_config(config: &crate::config::VisionConfig) -> Self {
        if config.mode.trim().eq_ignore_ascii_case("off") {
            return Self::Off;
        }
        match config.model_dir.trim() {
            "" => Self::InstalledModels,
            dir => Self::ModelDir(dir.to_string()),
        }
    }
}

/// How long a field may be read back for, in one step.
///
/// The two ONNX sessions cost seconds to load on the first use of a process
/// and tens of milliseconds per frame after that. This is not a budget the
/// step pays from — it is a ceiling, so a corrupt model cannot hold a step
/// open. A step whose own budget is shorter stops at that instead; the answer
/// is then `None`, which is what "not checked" means everywhere else here.
#[cfg(feature = "vision")]
const READ_BACK_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait for the screen to stop changing after an action.
#[derive(Debug, Clone, Copy)]
pub struct SettleConfig {
    /// Delay before the first post-action capture.
    pub initial_delay_ms: u64,
    /// Poll spacing when the first capture still differs from the baseline.
    pub poll_interval_ms: u64,
    /// Maximum number of post-action captures.
    pub max_rounds: usize,
    /// Pixel-change percentage below which the screen counts as settled.
    pub stable_threshold: f32,
    /// The shortest time an action gets to show *any* effect before "no
    /// change" is accepted as the answer.
    ///
    /// Two consecutive identical captures are ambiguous: the screen may have
    /// stopped changing, or it may not have started yet. Without this floor
    /// the loop reads the second case as the first — and a repaint that lands
    /// one poll late is reported to the model as "0.0% changed in pixels",
    /// which is exactly the claim that makes a working click look dead and
    /// send the ladder typing into a field it already filled.
    ///
    /// Measured on WebKitGTK in the bench image: a context menu opens 205 ms
    /// after the right-click on a good run and 1.4 s on a slow one, so a
    /// window built from polls alone (150 ms + 3 × 120 ms) ends before the
    /// paint it was waiting for. The floor is only paid when nothing has
    /// changed yet; an action that does visibly change the screen still
    /// settles as soon as two captures agree.
    pub min_window_ms: u64,
}

impl Default for SettleConfig {
    fn default() -> Self {
        Self {
            initial_delay_ms: 150,
            poll_interval_ms: 120,
            // Enough rounds to cover `min_window_ms` at this poll spacing,
            // since `poll_interval_ms` is what sets the loop's real cadence.
            max_rounds: 12,
            stable_threshold: 0.5,
            min_window_ms: 1_200,
        }
    }
}

/// One value the agent chose to keep across steps.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PinnedNote {
    pub key: String,
    pub value: String,
    /// Session step at which the note was written.
    pub step: usize,
}

/// What happened to one action.
#[derive(Debug, Clone)]
pub struct StepOutcome {
    pub action: DesktopAction,
    pub result: ActionResult,
    pub verification: StepVerification,
    pub evidence: Option<crate::computer::ActionEvidence>,
    /// Post-action screen state, when one was captured.
    pub state_after: Option<ScreenState>,
    /// Pre-action capture, when the action mutates the screen.
    ///
    /// The session takes this baseline for its own diff; it is handed back
    /// because a replayed timeline needs the frame the decision was made
    /// against, and once the step is over only the caller can keep it.
    pub screenshot_before: Option<crate::computer::Screenshot>,
    /// Pre/post comparison, when both sides were captured.
    pub diff: Option<ScreenDiff>,
    /// Sensitive UI found in the tree before the action.
    pub sensitive_before: Vec<SensitiveUiFinding>,
    /// Sensitive UI found in the tree after the action.
    pub sensitive_after: Vec<SensitiveUiFinding>,
    /// Whether the action aimed at an element the detector flagged.
    pub sensitive_target: bool,
    /// Root cause analysis, present when the step failed.
    pub reflection: Option<FailureAnalysis>,
    /// Number of filesystem snapshots taken before the action.
    pub snapshots_taken: usize,
    /// Whether the auto-rollback fired on this step.
    pub rolled_back: bool,
    /// Time spent waiting for the screen to stop changing, when a screenshot
    /// was compared. `None` for non-mutating actions and for steps that never
    /// reached a settle.
    pub settle_ms: Option<u64>,
    /// The step's time budget on the way out: what it had, what it spent.
    ///
    /// `None` when the session has no budget. Present otherwise so a caller
    /// can tell "the desktop was slow" from "the ceiling was tight" without
    /// guessing from wall-clock.
    pub budget: Option<serde_json::Value>,
}

/// A step refused because the platform's hit test found something else at
/// the point the action would land on.
struct OcclusionRefusal {
    result: ActionResult,
    evidence: crate::computer::ActionEvidence,
}

/// Mutable per-session state.
#[derive(Debug, Default)]
struct SessionState {
    step: usize,
    consecutive_failures: usize,
    last_verification: Option<StepVerification>,
    last_error: Option<String>,
    rollback_triggered: bool,
    notes: Vec<PinnedNote>,
    sensitive: Vec<SensitiveUiFinding>,
    snapshot_counts: Vec<usize>,
    /// Pixel → point mapping of the most recent screenshot the session took.
    coordinate_space: Option<CoordinateSpace>,
}

/// Configuration for a [`ComputerSession`].
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub verification: VerificationConfig,
    pub settle: SettleConfig,
    /// Consecutive failed steps that trigger one automatic rollback.
    pub auto_rollback_threshold: usize,
    /// Filesystem paths to snapshot before each mutating action.
    pub snapshot_paths: Vec<String>,
    /// Whether to check each action at all. When false every step reports
    /// `Unverifiable`, which is what "not checked" means.
    pub verify: bool,
    /// Whether a step may read a typed value back off the screen. See
    /// [`ReadBack`].
    pub read_back: ReadBack,
    /// Where input may land. `Background` refuses an input-layer action that
    /// cannot reach the app without taking the foreground, instead of
    /// delivering it anyway.
    pub delivery: DeliveryMode,
    /// How long one step may take, from its first capture to its last.
    ///
    /// `None` keeps the historical behaviour — no ceiling beyond whatever the
    /// caller wraps around the whole tool call. When set, every capture, read,
    /// wait and settle inside the step is capped by what is left, so the step
    /// ends with a budget refusal instead of being cut off from outside.
    pub step_budget: Option<Duration>,
    /// The apps this session may act on (`[computer] allow_apps`).
    ///
    /// Shared, not copied, so a `config.set` that tightens the list reaches a
    /// session that was built before it. An empty list is no boundary, which is
    /// the default.
    pub allow_apps: crate::computer::app_scope::AppAllowlist,
    /// The tool whose approval covers this session's desktop work.
    ///
    /// A session is built by a caller that has already passed the gate — the
    /// `computer` tool, or `computer.run`'s request. The adapter's inner calls
    /// (AppleScript, the native desktop-control tool) require approval too, and
    /// they run with a bare context that has no ask queue: without this, each
    /// one would be refused by the gate the session was already granted.
    ///
    /// Empty means "no approval carries down", which is the honest default for
    /// a session built outside an approved call.
    pub approved_by: String,
    /// The conversation this session acts for, which an inherited grant is
    /// checked against. Empty outside a conversation.
    pub conversation_id: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            verification: VerificationConfig::default(),
            settle: SettleConfig::default(),
            auto_rollback_threshold: 3,
            snapshot_paths: Vec::new(),
            verify: true,
            read_back: ReadBack::InstalledModels,
            delivery: DeliveryMode::Foreground,
            step_budget: None,
            allow_apps: crate::computer::app_scope::AppAllowlist::default(),
            approved_by: String::new(),
            conversation_id: String::new(),
        }
    }
}

/// The verification / rollback / memory layer for desktop actions.
pub struct ComputerSession {
    adapter: Arc<dyn ComputerAdapter>,
    verifier: VerificationEngine,
    reflection: ReflectionEngine,
    rollback_manager: Option<Arc<Mutex<RollbackManager>>>,
    config: SessionConfig,
    state: Mutex<SessionState>,
    snapshots: SnapshotCache,
    /// The page side of the ladder, when this session may land in a web page.
    page: Option<PageBinding>,
}

/// A page actor and the conversation whose tab it may act on.
struct PageBinding {
    actor: Arc<dyn PageActor>,
    owner: String,
}

impl ComputerSession {
    pub fn new(adapter: Arc<dyn ComputerAdapter>) -> Self {
        let verifier = VerificationEngine::new(adapter.clone());
        Self {
            adapter,
            verifier,
            reflection: ReflectionEngine::new(),
            rollback_manager: None,
            config: SessionConfig::default(),
            state: Mutex::new(SessionState::default()),
            snapshots: SnapshotCache::new(4),
            page: None,
        }
    }

    /// Give the ladder a page rung, acting on the browser page that belongs
    /// to `owner` — a conversation id, the same scope the browser tool uses
    /// for tabs. Without one the ladder ends at the keyboard.
    pub fn with_page(mut self, actor: Arc<dyn PageActor>, owner: impl Into<String>) -> Self {
        self.page = Some(PageBinding { actor, owner: owner.into() });
        self
    }

    /// Whether this session can fall back to the page.
    pub fn has_page(&self) -> bool {
        self.page.is_some()
    }

    pub fn with_config(mut self, config: SessionConfig) -> Self {
        self.verifier =
            VerificationEngine::new(self.adapter.clone()).with_config(config.verification);
        self.config = config;
        self
    }

    pub fn with_rollback_manager(mut self, manager: Arc<Mutex<RollbackManager>>) -> Self {
        self.rollback_manager = Some(manager);
        self
    }

    /// The budget for one step, started now.
    ///
    /// Built per step rather than per session: "one minute" means one minute
    /// of *this* action, not a clock that started when the conversation did.
    fn step_budget(&self) -> StepBudget {
        match self.config.step_budget {
            Some(total) => StepBudget::new(total),
            None => StepBudget::unlimited(),
        }
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    pub fn adapter(&self) -> &Arc<dyn ComputerAdapter> {
        &self.adapter
    }

    /// Snapshot cache of this session. Adapters own their own; this one is
    /// used when the caller resolves targets through the session.
    pub fn snapshots(&self) -> &SnapshotCache {
        &self.snapshots
    }

    /// Current step counter.
    pub async fn step(&self) -> usize {
        self.state.lock().await.step
    }

    /// Number of consecutive failed steps.
    pub async fn consecutive_failures(&self) -> usize {
        self.state.lock().await.consecutive_failures
    }

    /// Verification verdict for the previous step.
    pub async fn last_verification(&self) -> Option<StepVerification> {
        self.state.lock().await.last_verification
    }

    /// Error text from the previous failed step.
    pub async fn last_error(&self) -> Option<String> {
        self.state.lock().await.last_error.clone()
    }

    /// Reset counters and memory. Called when a new task starts.
    pub async fn reset(&self) {
        let mut state = self.state.lock().await;
        *state = SessionState::default();
    }

    // ── Pinned notes ──────────────────────────────────────────────────────

    /// Remember a value under `key`, replacing any earlier value.
    pub async fn note(&self, key: &str, value: &str) -> usize {
        let mut state = self.state.lock().await;
        let step = state.step;
        if let Some(existing) = state.notes.iter_mut().find(|n| n.key == key) {
            existing.value = value.to_string();
            existing.step = step;
        } else {
            state.notes.push(PinnedNote {
                key: key.to_string(),
                value: value.to_string(),
                step,
            });
        }
        state.notes.len()
    }

    /// All pinned notes, oldest first.
    pub async fn notes(&self) -> Vec<PinnedNote> {
        self.state.lock().await.notes.clone()
    }

    /// Drop all pinned notes.
    pub async fn clear_notes(&self) {
        self.state.lock().await.notes.clear();
    }

    // ── Sensitive UI ──────────────────────────────────────────────────────

    /// Record findings from the most recent perception pass.
    pub async fn record_sensitive(&self, findings: Vec<SensitiveUiFinding>) {
        self.state.lock().await.sensitive = findings;
    }

    /// Findings from the most recent perception pass.
    pub async fn sensitive_findings(&self) -> Vec<SensitiveUiFinding> {
        self.state.lock().await.sensitive.clone()
    }

    /// Whether the element being acted on is one the detector flagged.
    pub async fn is_sensitive_target(&self, target: Option<&ActionTarget>) -> bool {
        let Some(element) = target.and_then(|t| t.element.as_ref()) else {
            return false;
        };
        let state = self.state.lock().await;
        state.sensitive.iter().any(|f| {
            (!f.element_id.is_empty() && f.element_id == element.id)
                || element
                    .label
                    .as_ref()
                    .map(|label| f.description.contains(label.as_str()))
                    .unwrap_or(false)
        })
    }

    // ── Coordinate space ──────────────────────────────────────────────────

    /// Pixel → screen-point mapping of the most recent screenshot.
    pub async fn coordinate_space(&self) -> Option<CoordinateSpace> {
        self.state.lock().await.coordinate_space
    }

    /// Remember the mapping that belongs to a screenshot the caller is about
    /// to show the model.
    pub async fn record_coordinate_space(&self, space: Option<CoordinateSpace>) {
        if space.is_some() {
            self.state.lock().await.coordinate_space = space;
        }
    }

    /// Compute and remember the mapping from the current screen.
    ///
    /// Used when an action arrives in screenshot pixels but no screenshot has
    /// passed through the session yet (e.g. `screen_state` ran first). An
    /// action in image pixels without a mapping would silently click at half
    /// the intended position on a Retina display, so the caller gets an error
    /// instead of a guess.
    pub async fn refresh_coordinate_space(&self) -> crate::computer::Result<CoordinateSpace> {
        let screenshot = self.adapter.screenshot(None).await?;
        let geometry = self.adapter.display_geometry().await?;
        let space = CoordinateSpace::from_screenshot(&screenshot, &geometry).ok_or_else(|| {
            ComputerError::Other(
                "cannot map screenshot pixels to screen points: display geometry unknown \
                     (use element ids or screen-point coordinates)"
                    .to_string(),
            )
        })?;
        self.record_coordinate_space(Some(space)).await;
        Ok(space)
    }

    /// Best-effort: remember the mapping for a screenshot the session produced.
    async fn note_space_for(&self, screenshot: &Screenshot) {
        self.note_space(screenshot.width, screenshot.height).await;
    }

    /// Best-effort: remember the mapping for an image size the session saw.
    async fn note_space(&self, image_width: u32, image_height: u32) {
        if image_width == 0 || image_height == 0 {
            return;
        }
        let geometry = match self.adapter.display_geometry().await {
            Ok(geometry) => geometry,
            Err(e) => {
                tracing::debug!("display geometry unavailable: {e}");
                return;
            }
        };
        let space = CoordinateSpace::new(image_width, image_height, &geometry);
        self.record_coordinate_space(space).await;
    }

    /// Best-effort: an adapter that returns screenshot dimensions as data
    /// (rather than a `Screenshot`) still tells us which space it showed.
    async fn note_space_from_result(&self, result: &ActionResult) {
        let Some(data) = result.data.as_ref() else {
            return;
        };
        let width = data.get("width").and_then(serde_json::Value::as_u64);
        let height = data.get("height").and_then(serde_json::Value::as_u64);
        if let (Some(width), Some(height)) = (width, height) {
            self.note_space(width as u32, height as u32).await;
        }
    }

    // ── Snapshot bookkeeping (used by the loop's undo) ────────────────────

    pub(crate) async fn push_snapshot_count(&self, count: usize) {
        self.state.lock().await.snapshot_counts.push(count);
    }

    /// Undo the last `n` steps by rolling back the snapshots they took.
    pub async fn undo_steps(&self, n: usize) -> crate::computer::Result<usize> {
        let (n, total) = {
            let mut state = self.state.lock().await;
            let n = n.min(state.snapshot_counts.len());
            if n == 0 {
                return Ok(0);
            }
            let total: usize = state.snapshot_counts[state.snapshot_counts.len() - n..]
                .iter()
                .sum();
            let keep = state.snapshot_counts.len() - n;
            state.snapshot_counts.truncate(keep);
            (n, total)
        };

        if let Some(ref mgr) = self.rollback_manager {
            let mut mgr = mgr.lock().await;
            mgr.rollback_last(total)
                .await
                .map_err(|e| ComputerError::Other(format!("Rollback failed: {}", e)))?;
        }
        Ok(n)
    }

    /// Undo the most recent step.
    pub async fn undo_last_step(&self) -> crate::computer::Result<bool> {
        self.undo_steps(1).await.map(|n| n > 0)
    }

    // ── The step ──────────────────────────────────────────────────────────

    /// Execute one action and establish what it did.
    ///
    /// Click-like actions on an element walk the ladder
    /// `ax_action → pixel → keyboard`: each rung is attempted only while the
    /// previous one is *observed* to have had no effect. Escalation is
    /// evidence-driven, not retry-driven — a rung we could not check does not
    /// hand off to the next, because a blind second click is how a UI ends up
    /// in a state nobody intended.
    pub async fn execute(&self, action: DesktopAction) -> crate::computer::Result<StepOutcome> {
        self.execute_with_budget(action, None).await
    }

    /// Execute one action in a batch, deciding whether to look afterwards.
    ///
    /// `look_after` is the batch's whole point. A batch is several actions a
    /// person would take off one glance (`click → type → Tab → type`), and the
    /// cost being removed is the settle-and-capture between them. So every
    /// action but the last runs *unobserved*: it is dispatched, its failure is
    /// still an error, and it is still accounted — but there is no post-action
    /// capture and no pixel diff, because the diff would be paid for and then
    /// thrown away by the next action in the same batch.
    ///
    /// The last action is observed exactly as [`Self::execute`] would, so the
    /// step that follows the batch sees where the batch actually ended up.
    ///
    /// One thing an unobserved action keeps is the *refusal* checks: the app
    /// boundary, the disabled-target rule and the ladder's own errors all
    /// still run. What it gives up is the escalation loop's evidence (a diff),
    /// which means it also gives up escalation — a batched click that silently
    /// did nothing is not re-tried on another rung, because the next action
    /// was already planned on top of it and a retry would run it twice. The
    /// model learns the batch did not land from the last action's frame; a
    /// mis-landed intermediate action is a reason to batch more conservatively,
    /// which the prompt says.
    pub async fn execute_in_batch(
        &self,
        action: DesktopAction,
        look_after: bool,
    ) -> crate::computer::Result<StepOutcome> {
        self.execute_with_budget_observed(action, None, look_after)
            .await
    }

    /// Execute one action under a budget the caller set for this call.
    ///
    /// The tool wraps a step in the same ceiling the tool registry applies to
    /// the whole call, with a margin, so the step ends by saying it ran out of
    /// time rather than by being killed mid-sentence from outside. A session
    /// configured with a `step_budget` still has one; a budget passed here
    /// overrides it, because it belongs to *this* call.
    pub async fn execute_with_budget(
        &self,
        action: DesktopAction,
        budget: Option<StepBudget>,
    ) -> crate::computer::Result<StepOutcome> {
        self.execute_with_budget_observed(action, budget, true)
            .await
    }

    /// The body of [`Self::execute_with_budget`], with the option to skip the
    /// post-action look.
    ///
    /// `observed == false` is the batched case: dispatch and account, but do
    /// not settle, capture or diff. See [`Self::execute_in_batch`].
    async fn execute_with_budget_observed(
        &self,
        action: DesktopAction,
        budget: Option<StepBudget>,
        observed: bool,
    ) -> crate::computer::Result<StepOutcome> {
        let budget = match budget {
            Some(budget) => budget,
            None => self.step_budget(),
        };
        let mutating = is_screen_mutating_action(&action);
        // A baseline is only worth taking when the step will compare against
        // it. In a batch, only the last action is observed.
        let watch_screen = mutating && observed;

        // The per-app boundary, checked before anything is captured or
        // dispatched. It has to come first: a step that refuses because the app
        // is not on the list must leave no trace of having started, and the
        // filesystem snapshots below are a side effect.
        if let Some(reason) = self.app_scope_refusal(&action).await {
            return Ok(self
                .refused_outcome(
                    action,
                    &reason,
                    "act only on the apps on the [computer] allow_apps list, or ask the operator \
                     to add this one — the action was not sent",
                    0,
                    &budget,
                )
                .await);
        }

        // Filesystem snapshots, so a step that wrecks something can be undone.
        let snapshots_taken = self.snapshot_before().await;
        if snapshots_taken > 0 {
            self.push_snapshot_count(snapshots_taken).await;
        }

        let ladder = self.ladder_for(&action).await;
        let mut tried = Vec::new();
        let mut last_error: Option<ComputerError> = None;
        // The rung that was actually dispatched last, for refusal evidence.
        let mut attempted: Option<Rung> = None;

        // A control that said it is disabled gets a refusal, not a click.
        // Clicking it anyway means clicking whatever draws on top of it, and
        // the pixel rung would report success for a press nothing received.
        if let Some(reason) = self.disabled_target(&action).await {
            let recovery = "wait for the control to become enabled, or ask the user how to \
                            proceed";
            return Ok(self
                .refused_outcome(action, &reason, recovery, snapshots_taken, &budget)
                .await);
        }

        for (index, rung) in ladder.iter().copied().enumerate() {
            let last_rung = index + 1 == ladder.len();

            // A rung started with the budget already spent has no time to
            // dispatch or verify; starting it would be the schedule's
            // problem, not the desktop's.
            if budget.is_exhausted() {
                last_error = Some(ComputerError::BudgetExhausted(format!(
                    "the step spent its {} ms before rung {rung:?} could run",
                    budget.total().map(|t| t.as_millis()).unwrap_or_default()
                )));
                break;
            }

            // Baseline for this attempt: one screenshot, and the tree only
            // when a diff will need it.
            let screenshot_before = if watch_screen {
                crate::computer::budget::within(&budget, "the baseline screenshot", async {
                    self.adapter.screenshot(None).await
                })
                .await
                .ok()
                .and_then(Result::ok)
            } else {
                None
            };
            let tree_before = if watch_screen {
                crate::computer::budget::within(&budget, "the baseline tree read", async {
                    self.adapter.read_ui_tree(None).await
                })
                .await
                .ok()
                .and_then(Result::ok)
            } else {
                None
            };
            // Every capture the session takes names a coordinate space the
            // caller may quote back in a later action.
            if let Some(screenshot) = screenshot_before.as_ref() {
                self.note_space_for(screenshot).await;
            }

            attempted = Some(rung);

            // The pixel rung aims at a point, and a point is not an element:
            // a dialog, a notification, or another window can be drawn over
            // it. Ask the platform what would actually receive the event
            // first. Only a positive "something else is there" refuses; an
            // unanswered hit test is not clearance, but it is not a refusal
            // either, and must not block an action nobody could check.
            if rung == Rung::Pixel {
                if let Some(refusal) = self.occluded_target(&action).await {
                    let (rolled_back, reflection) = self
                        .account(
                            &action,
                            &refusal.result,
                            StepVerification::Unverifiable,
                            ActionEffect::Refused,
                        )
                        .await;
                    let mut outcome = StepOutcome {
                        action,
                        result: refusal.result,
                        verification: StepVerification::Unverifiable,
                        evidence: Some(refusal.evidence),
                        state_after: None,
                        screenshot_before: None,
                        diff: None,
                        sensitive_before: Vec::new(),
                        sensitive_after: self.sensitive_findings().await,
                        sensitive_target: false,
                        reflection,
                        snapshots_taken,
                        rolled_back,
                        settle_ms: None,
                        budget: Some(budget.summary()),
                    };
                    if let Some(evidence) = outcome.evidence.as_mut() {
                        evidence.tried = tried;
                    }
                    return Ok(outcome);
                }
            }

            // The boundary, again, for the rungs that deliver to a point or a
            // focus rather than to an element. An action that named an allowed
            // app passed the pre-dispatch check, but the pixel and keyboard
            // rungs are fallbacks that run *after* another rung had no effect
            // — the moment a covering window or a different frontmost app
            // decides who receives the event. Asking here is what keeps "may
            // act on Safari" from typing into whatever happens to be in front
            // when the accessibility rung answers "no effect".
            if let Some(reason) = self.rung_border_refusal(&action, rung).await {
                let (rolled_back, reflection) = self
                    .account(
                        &action,
                        &ActionResult::error(format!("refused: {reason}")),
                        StepVerification::Unverifiable,
                        ActionEffect::Refused,
                    )
                    .await;
                let mut evidence =
                    crate::computer::ActionEvidence::new(rung, ActionEffect::Refused)
                        .with_delivery(crate::computer::DeliveryDisposition::NotDelivered)
                        .with_recovery(reason.clone())
                        .with_tried(tried.clone());
                evidence.escalation = None;
                return Ok(StepOutcome {
                    action,
                    result: ActionResult::error(format!("refused: {reason}")),
                    verification: StepVerification::Unverifiable,
                    evidence: Some(evidence),
                    state_after: None,
                    screenshot_before: None,
                    diff: None,
                    sensitive_before: Vec::new(),
                    sensitive_after: self.sensitive_findings().await,
                    sensitive_target: false,
                    reflection,
                    snapshots_taken,
                    rolled_back,
                    settle_ms: None,
                    budget: Some(budget.summary()),
                });
            }

            let result = match self.dispatch(&action, rung, &budget).await {
                Ok(result) => result,
                Err(e) => {
                    // A rung that could not run is not a failed action: the
                    // next rung is exactly the answer.
                    tracing::info!("rung {:?} unavailable: {}", rung, e);
                    // Background delivery was asked for and the platform said
                    // no. Escalating would deliver the same action in the
                    // foreground — the one thing that must not happen here.
                    let terminal = matches!(e, ComputerError::BackgroundInputUnavailable(_));
                    last_error = Some(e);
                    if terminal {
                        break;
                    }
                    // The keyboard rung presses keys into whatever holds
                    // focus; it is a fallback for a click that ran but had no
                    // effect. After a pixel rung that never ran, a stray
                    // Return would land in some other window — stop instead.
                    if rung == Rung::Pixel {
                        break;
                    }
                    continue;
                }
            };
            let mut result = result;
            self.note_space_from_result(&result).await;
            let mut driver_evidence = result.evidence.clone().unwrap_or_else(|| {
                crate::computer::ActionEvidence::new(rung, ActionEffect::Unverifiable)
            });
            driver_evidence.tried = tried.clone();
            result.evidence = Some(driver_evidence.clone());

            // Settle, then capture the state verification compares against.
            let settle_start = std::time::Instant::now();
            let state_after = if watch_screen {
                self.settle_and_capture(screenshot_before.as_ref(), &budget)
                    .await
            } else {
                None
            };
            // How long the screen took to stop changing is evidence about the
            // action: a step that settled instantly did nothing observable.
            let settle_ms = state_after
                .is_some()
                .then(|| settle_start.elapsed().as_millis() as u64);

            // An unobserved action has nothing to verify against: no
            // baseline, no frame after, no tree. Saying `Unverifiable` is the
            // honest answer — the batch's last action is what gets observed.
            let (verification, diff, hint) = if self.config.verify && observed {
                self.verify_step(
                    &action,
                    &result,
                    tree_before.as_deref(),
                    screenshot_before.as_ref(),
                    state_after.as_ref().map(|s| &s.screenshot),
                    state_after.as_ref(),
                )
                .await?
            } else {
                (StepVerification::Unverifiable, None, None)
            };

            let mut evidence = score_evidence(verification, Some(&driver_evidence));
            // A tree-derived verdict outranks the pixel verdict: the screen
            // changed, but the field says only half the text arrived.
            if let Some(hint) = hint {
                evidence.effect = hint.effect;
                evidence.escalation = None;
                evidence.recovery = hint.recovery;
            }

            let keep_going = !last_rung && matches!(evidence.effect, ActionEffect::SuspectedNoop);

            if keep_going {
                tracing::info!("{} had no visible effect; escalating", evidence.summary());
                tried.push(rung);
                continue;
            }

            // Both senses of the screen travel with the result: the tree for
            // structure, the screenshot for what the tree cannot describe.
            // Either alone is the failure mode this pairs away.
            if result.screenshot_after.is_none() {
                if let Some(state) = state_after.as_ref() {
                    result.screenshot_after = Some(state.screenshot.clone());
                }
            }

            let sensitive_before = tree_before
                .as_deref()
                .map(scan_sensitive)
                .unwrap_or_default();
            let sensitive_after = state_after
                .as_ref()
                .map(|state| scan_sensitive(&state.ui_tree))
                .unwrap_or_default();
            let sensitive_target = self.target_is_sensitive(&action, &sensitive_before).await;
            self.record_sensitive(sensitive_after.clone()).await;

            result.evidence = Some(evidence.clone());
            let (rolled_back, reflection) = self
                .account(&action, &result, verification, evidence.effect)
                .await;

            return Ok(StepOutcome {
                action,
                result,
                verification,
                evidence: Some(evidence),
                state_after,
                // The baseline this rung was verified against: the last rung
                // that actually ran owns it.
                screenshot_before,
                diff,
                sensitive_before,
                sensitive_after,
                sensitive_target,
                reflection,
                snapshots_taken,
                rolled_back,
                settle_ms,
                budget: Some(budget.summary()),
            });
        }

        let error = last_error.unwrap_or_else(|| {
            ComputerError::ToolFailed("no action rung could be executed".to_string())
        });

        // Every rung failed before delivering anything. When the reason is one
        // the caller can act on — a stale id, a missing permission — say so as
        // a refusal: the same request will be refused the same way, so the
        // model needs to change something, not retry.
        if let Some(recovery) = refusal_recovery(&error) {
            let mut evidence = crate::computer::ActionEvidence::new(
                attempted.unwrap_or(Rung::Pixel),
                ActionEffect::Refused,
            )
            .with_delivery(crate::computer::DeliveryDisposition::NotDelivered)
            .with_recovery(recovery.to_string())
            .with_tried(tried);
            evidence.escalation = None;
            let message = format!("refused: {error} — {recovery}");
            let result = ActionResult::error(message);
            // Nothing was delivered, so nothing was verified — but nothing
            // failed either. Recording a refusal as a verification failure
            // makes three refusals in a row roll back work that may be fine.
            let verification = StepVerification::Unverifiable;
            let (rolled_back, reflection) = self
                .account(&action, &result, verification, ActionEffect::Refused)
                .await;
            return Ok(StepOutcome {
                action,
                result,
                verification,
                evidence: Some(evidence),
                state_after: None,
                screenshot_before: None,
                diff: None,
                sensitive_before: Vec::new(),
                sensitive_after: self.sensitive_findings().await,
                sensitive_target: false,
                reflection,
                snapshots_taken,
                rolled_back,
                settle_ms: None,
                budget: Some(budget.summary()),
            });
        }

        // No rung could run, and none of them was a refusal: the ladder
        // failed for a reason retrying might fix (a timeout, a tool failure).
        // The evidence still travels — which rungs were tried, which was
        // last — because "nothing worked" without "what was tried" is what
        // makes a failure unactionable.
        let mut evidence = crate::computer::ActionEvidence::new(
            attempted.unwrap_or(Rung::Pixel),
            ActionEffect::Refused,
        )
        .with_delivery(crate::computer::DeliveryDisposition::NotDelivered)
        .with_tried(tried);
        evidence.escalation = None;
        Err(ComputerError::NoRung {
            error: error.to_string(),
            evidence: Box::new(evidence),
        })
    }

    /// Which rungs to try, in order, for this action.
    ///
    /// A click on an addressable element walks `ax_action → pixel → keyboard`.
    /// Typing into a named field walks `set_value → focus, type → type`: the
    /// accessibility value is exact and needs no focus, clicking the field is
    /// what a person does, and typing into whatever holds focus is all that
    /// remains when the field cannot be addressed. When the target is web
    /// content and a page is attached, `page` is the last resort: the same
    /// element, found in the document instead of on the screen.
    ///
    /// A coordinate has only one rung that can carry it — the point is the
    /// whole instruction, and no accessibility path reaches a point. It gets
    /// the pixel rung and nothing else; adding a keyboard rung for a click
    /// would press a key at whatever holds focus, which is not the click the
    /// model asked for. Retrying a coordinate is the model's decision on the
    /// next step, made with a fresh look at the screen, and that is the right
    /// place for it: the session cannot know a better point.
    async fn ladder_for(&self, action: &DesktopAction) -> Vec<Rung> {
        // A hover is a pointer position and nothing else. The accessibility
        // rung presses an element, the keyboard rung focuses and activates
        // it — both would turn "does this react to the pointer" into "click
        // it", which is the one thing a probe must not do.
        if matches!(action, DesktopAction::Hover { .. } | DesktopAction::PointerMove { .. }) {
            return vec![Rung::Pixel];
        }
        let target = match action {
            DesktopAction::Click { target, .. }
            | DesktopAction::DoubleClick { target, .. }
            | DesktopAction::Type { target: Some(target), .. }
            | DesktopAction::Scroll { target, .. }
            | DesktopAction::Drag { from: target, .. } => target,
            _ => return vec![Rung::Pixel],
        };

        if matches!(target, ClickTarget::Coordinate(_)) {
            return vec![Rung::Pixel];
        }

        let resolved = self.adapter.resolve_target(target).await.ok();
        let mut ladder = Vec::new();
        // The accessibility rung is offered only when the element says it
        // supports the action, or when the platform did not publish an action
        // list (then the old behaviour stands: try it, and let the rung's own
        // result decide). An element that declared its actions and did not
        // include this one is not worth an external round trip.
        if resolved
            .as_ref()
            .and_then(|r| r.element.as_ref())
            .map(|e| {
                !e.supports_ax_action(&ax_action_for(target, action))
                    .is_false()
            })
            .unwrap_or(false)
        {
            ladder.push(Rung::AxAction);
        }
        ladder.push(Rung::Pixel);
        // A double click has no keyboard equivalent; a click and a type do.
        if matches!(action, DesktopAction::Click { .. } | DesktopAction::Type { .. }) {
            ladder.push(Rung::Keyboard);

            // The page rung is only for pages, and only for a target the page
            // can be asked about by name. A double click stays out: one DOM
            // click is not a double click.
            if self.page.is_some() && page_target(target, resolved.as_ref()).is_addressable() {
                ladder.push(Rung::Page);
            }
        }
        ladder
    }

    /// Why the pixel rung should not be dispatched, when the platform's own
    /// hit test says something else is at the point.
    ///
    /// Only a named occluder refuses. Every other answer — the platform has
    /// no hit test, the point is outside every window, the target itself
    /// receives the event — lets the action through: "nobody could check" is
    /// not evidence that the way is blocked.
    async fn occluded_target(&self, action: &DesktopAction) -> Option<OcclusionRefusal> {
        let target = match action {
            DesktopAction::Click { target, .. } | DesktopAction::DoubleClick { target, .. } => {
                target
            }
            DesktopAction::Type { target: Some(target), .. } => target,
            _ => return None,
        };
        // A bare coordinate has no intended element to compare against: the
        // caller named a point and meant that point, so there is nothing the
        // hit test could contradict.
        let resolved = match target {
            ClickTarget::Coordinate(_) => return None,
            other => self.adapter.resolve_target(other).await.ok(),
        };
        let resolved = resolved?;
        let Ok(point) = resolved.coordinates() else {
            return None;
        };
        let hit = self.adapter.hit_test(point, Some(&resolved)).await;
        let occluder = hit.occluder()?.clone();
        let recovery = format!(
            "{} is in the way at ({}, {}); dismiss or move it, re-read the UI tree and target \
             the element again — the click was not sent",
            occluder.summary(),
            point.x,
            point.y
        );
        let mut evidence = crate::computer::ActionEvidence::new(Rung::Pixel, ActionEffect::Refused)
            .with_delivery(crate::computer::DeliveryDisposition::NotDelivered)
            .with_recovery(recovery.clone())
            .with_occluder(occluder.clone());
        evidence.escalation = None;
        Some(OcclusionRefusal {
            result: ActionResult::error(format!("refused: {recovery}")),
            evidence,
        })
    }

    /// Why this action should not be attempted, when the target said so.
    ///
    /// Only a positive "disabled" answer blocks the action. `Tri::Absent` and
    /// `Tri::Unknown` both let it through: the first means the platform does
    /// not publish the state, the second that the read failed, and neither is
    /// evidence that the control is unusable.
    async fn disabled_target(&self, action: &DesktopAction) -> Option<String> {
        let target = match action {
            DesktopAction::Click { target, .. }
            | DesktopAction::DoubleClick { target, .. }
            | DesktopAction::Type { target: Some(target), .. } => target,
            _ => return None,
        };
        if matches!(target, ClickTarget::Coordinate(_)) {
            return None;
        }
        let resolved = self.adapter.resolve_target(target).await.ok()?;
        let element = resolved.element.as_ref()?;
        if !element.enabled.is_false() {
            return None;
        }
        let what = match element.label.as_deref().filter(|l| !l.is_empty()) {
            Some(label) => format!("{} {:?}", element.role, label),
            None => element.role.clone(),
        };
        Some(format!("{what} is disabled"))
    }

    /// Perform the action at one rung, under the session's approval.
    ///
    /// The desktop adapter drives the screen through the tool registry, and
    /// those inner tools now require approval. The session is built by a
    /// caller that already has it (the `computer` tool after its own gate, a
    /// run after `computer.run`'s), so the approval is carried down here rather
    /// than asked for again — and, on the inner call, asked for in a context
    /// with no queue to put the question on, which is why an un-inherited gate
    /// refused every step.
    ///
    /// Scoped to the task, not stored on the shared adapter: this adapter
    /// serves every conversation, and a grant parked on it would be read by
    /// whichever step ran next. Scoped around the *whole* dispatch rather than
    /// only the foreground path, because the background path
    /// (`perform_background`) reaches `applescript` just the same.
    async fn dispatch(
        &self,
        action: &DesktopAction,
        rung: Rung,
        budget: &StepBudget,
    ) -> crate::computer::Result<ActionResult> {
        let grant = crate::tools::DesktopGrant::new(
            self.config.approved_by.as_str(),
            self.config.conversation_id.as_str(),
        );
        crate::tools::with_desktop_grant(grant, self.dispatch_granted(action, rung, budget)).await
    }

    async fn dispatch_granted(
        &self,
        action: &DesktopAction,
        rung: Rung,
        budget: &StepBudget,
    ) -> crate::computer::Result<ActionResult> {
        // A `wait` is the one action whose whole content is time. Asking for
        // more than the step has left would turn a requested wait into a
        // timeout from above; sleeping what fits and saying so is the honest
        // answer.
        let action = match action {
            DesktopAction::Wait { milliseconds } if budget.total().is_some() => {
                let left = budget.remaining().unwrap_or_default().as_millis() as u64;
                DesktopAction::Wait {
                    milliseconds: (*milliseconds).min(left),
                }
            }
            other => other.clone(),
        };
        let action = &action;
        // The pointer and keyboard rungs are the two that can take the screen
        // away from whoever is using it. In background mode they go through
        // the adapter's background path, which refuses when the platform has
        // no way to reach the app without the foreground.
        if self.config.delivery == DeliveryMode::Background
            && matches!(rung, Rung::Pixel | Rung::Keyboard)
        {
            let target = self.resolved_target(action).await;
            return crate::computer::budget::within(budget, "the background dispatch", async {
                self.adapter
                    .perform_background(action, target.as_ref())
                    .await
            })
            .await?;
        }

        // The dispatch itself is the part that can hang (an AppleScript
        // telling System Events to do something that never answers). Give it
        // the budget, not a magic number.
        crate::computer::budget::within(
            budget,
            "the action dispatch",
            self.dispatch_inner(action, rung),
        )
        .await
        .unwrap_or_else(|_| Err(ComputerError::BudgetExhausted("dispatch".to_string())))
    }

    async fn dispatch_inner(
        &self,
        action: &DesktopAction,
        rung: Rung,
    ) -> crate::computer::Result<ActionResult> {
        match (rung, action) {
            (Rung::AxAction, DesktopAction::Click { target, .. })
            | (Rung::AxAction, DesktopAction::DoubleClick { target, .. }) => {
                let resolved = self.adapter.resolve_target(target).await?;
                self.adapter
                    .perform_ax_action(&resolved, crate::computer::AxAction::Press)
                    .await
            }
            // Setting the value needs neither focus nor keystrokes, and it
            // cannot land in the wrong field.
            (Rung::AxAction, DesktopAction::Type { text, target: Some(target) }) => {
                let resolved = self.adapter.resolve_target(target).await?;
                self.adapter
                    .perform_ax_action(
                        &resolved,
                        crate::computer::AxAction::SetValue { value: text.clone() },
                    )
                    .await
            }
            // Click into the field, then type. The click is what makes the
            // keystrokes land in this field and nowhere else; if it fails, the
            // keyboard rung is not a safe answer, so stop the ladder here.
            (Rung::Pixel, DesktopAction::Type { text, target: Some(target) }) => {
                let resolved = self.adapter.resolve_target(target).await?;
                let point = resolved.coordinates()?;
                let focus = self
                    .adapter
                    .click_at(
                        crate::computer::Point::new(point.x, point.y),
                        crate::computer::MouseButton::Left,
                    )
                    .await?;
                if !focus.success {
                    return Err(ComputerError::ToolFailed(format!(
                        "could not focus the field before typing: {}",
                        focus.message
                    )));
                }
                self.adapter.type_text(text).await
            }
            (Rung::Keyboard, DesktopAction::Type { text, .. }) => {
                self.adapter.type_text(text).await
            }
            // The keyboard rung operates on whatever already has focus — the
            // pixel rung in front of it is what put focus there. Which key
            // activates a control depends on what the control is: a focused
            // check box answers to Space, and Return on a focused text field
            // submits the form instead.
            (Rung::Keyboard, DesktopAction::Click { target, .. }) => {
                let key = self.activation_key(target).await;
                self.adapter
                    .execute(DesktopAction::KeyPress { keys: vec![key.to_string()] })
                    .await
            }
            // The page rung asks the document. It is only reached after the
            // three screen-level rungs left the page unchanged.
            (Rung::Page, DesktopAction::Click { .. } | DesktopAction::Type { .. }) => {
                let Some(binding) = self.page.as_ref() else {
                    return Err(ComputerError::UnsupportedPlatform(
                        "no browser page is attached to this session".to_string(),
                    ));
                };
                let target = self.page_target_for(action).await;
                let page_action = match action {
                    DesktopAction::Type { text, .. } => PageAction::Type(text.clone()),
                    _ => PageAction::Click,
                };
                binding
                    .actor
                    .act(&binding.owner, &target, page_action)
                    .await
            }
            _ => self.adapter.execute(action.clone()).await,
        }
    }

    /// The element an action names, resolved, for a rung that needs to know
    /// what it is aiming at.
    async fn resolved_target(&self, action: &DesktopAction) -> Option<ActionTarget> {
        let target = match action {
            DesktopAction::Click { target, .. } | DesktopAction::DoubleClick { target, .. } => {
                target
            }
            DesktopAction::Type { target: Some(target), .. } => target,
            _ => return None,
        };
        self.adapter.resolve_target(target).await.ok()
    }

    /// The page's name for an action's element, resolved through the same
    /// tree the other rungs use.
    async fn page_target_for(&self, action: &DesktopAction) -> PageTarget {
        let target = match action {
            DesktopAction::Click { target, .. }
            | DesktopAction::Type { target: Some(target), .. } => target,
            _ => return PageTarget::new("", None),
        };
        let resolved = if matches!(target, ClickTarget::Coordinate(_)) {
            None
        } else {
            self.adapter.resolve_target(target).await.ok()
        };
        page_target(target, resolved.as_ref())
    }

    /// The key that activates the element a click aims at, once it has focus.
    async fn activation_key(&self, target: &ClickTarget) -> &'static str {
        self.adapter
            .resolve_target(target)
            .await
            .ok()
            .and_then(|resolved| resolved.element)
            .map(|element| activation_key(&element.role))
            .unwrap_or("enter")
    }

    /// Whether the element this action aims at is one the detector flagged.
    async fn target_is_sensitive(
        &self,
        action: &DesktopAction,
        findings: &[SensitiveUiFinding],
    ) -> bool {
        if findings.is_empty() {
            return false;
        }
        let target = match action {
            DesktopAction::Click { target, .. } | DesktopAction::DoubleClick { target, .. } => {
                target
            }
            DesktopAction::Type { .. } => {
                // Typing goes wherever focus is; the focused field is the
                // target, and the findings record the focused text field
                // among the elements.
                return findings.iter().any(|f| {
                    f.category != crate::computer::sensitive_ui::SensitiveUiType::PaymentPage
                });
            }
            _ => return false,
        };
        let Ok(resolved) = self.adapter.resolve_target(target).await else {
            return false;
        };
        let Some(element) = resolved.element.as_ref() else {
            return false;
        };
        findings.iter().any(|f| {
            (!f.element_id.is_empty() && f.element_id == element.id)
                || element
                    .label
                    .as_ref()
                    .map(|label| !label.is_empty() && f.description.contains(label.as_str()))
                    .unwrap_or(false)
        })
    }

    /// Take the pre-action filesystem snapshots.
    async fn snapshot_before(&self) -> usize {
        if self.config.snapshot_paths.is_empty() {
            return 0;
        }
        let Some(ref mgr) = self.rollback_manager else {
            return 0;
        };
        let mut taken = 0;
        let mut mgr = mgr.lock().await;
        for path_str in &self.config.snapshot_paths {
            let path = std::path::Path::new(path_str);
            let result = if tokio::fs::metadata(path)
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
                mgr.snapshot_directory(path).await
            } else {
                mgr.snapshot_file(path).await
            };
            match result {
                Ok(_) => taken += 1,
                Err(e) => tracing::warn!("failed to snapshot '{}': {}", path_str, e),
            }
        }
        taken
    }

    /// Wait for the screen to stop changing, returning the settled state.
    ///
    /// The first capture is the one verification needs anyway, so a settled
    /// screen costs one screenshot instead of a fixed sleep plus one.
    async fn settle_and_capture(
        &self,
        before: Option<&Screenshot>,
        budget: &StepBudget,
    ) -> Option<ScreenState> {
        let cfg = self.config.settle;
        let mut previous = before.cloned();
        let mut settled_shot = None;
        let started = std::time::Instant::now();
        let floor = Duration::from_millis(cfg.min_window_ms);
        // With no baseline there is nothing to be "not changed from", so the
        // floor would only cost time without answering anything.
        let mut observed_change = before.is_none();

        for round in 0..cfg.max_rounds.max(1) {
            let delay = if round == 0 {
                cfg.initial_delay_ms
            } else {
                cfg.poll_interval_ms
            };
            // Settling is where a step spends most of its time, and it is
            // also the most expandable part: stopping it at the deadline is
            // honest (the last capture is still a capture) where a timeout
            // cut from outside is not.
            let delay = Duration::from_millis(delay);
            if budget.is_exhausted() {
                tracing::info!("settle stopped: the step budget is spent");
                break;
            }
            tokio::time::sleep(budget.cap(delay)).await;
            if budget.is_exhausted() {
                tracing::info!("settle stopped after its wait: the step budget is spent");
                break;
            }

            // The capture is part of what the step pays for: a screenshot
            // that outlasts the budget is a setTimeout from outside, which is
            // exactly what the budget exists to avoid.
            //
            // `screenshot_fresh` rather than `screenshot`: a settle poll that
            // the adapter answers from its own short-lived cache returns the
            // frame it already gave us, and two identical frames are this
            // loop's definition of "settled". That is how a click whose
            // repaint was 200 ms away got reported as "0.0% changed".
            let shot = match crate::computer::budget::within(
                budget,
                "a settle screenshot",
                self.adapter.screenshot_fresh(None),
            )
            .await
            {
                Ok(Ok(shot)) => shot,
                Ok(Err(e)) => {
                    tracing::warn!("post-action screenshot failed: {}", e);
                    return None;
                }
                Err(e) => {
                    tracing::info!("settle stopped: {}", e);
                    break;
                }
            };

            // Which frame "settled" is measured against. Until the action has
            // shown *some* effect, the comparison that matters is against the
            // baseline: the repaint may not have begun, and two frames of the
            // old screen agreeing with each other says nothing about the new
            // one. Once a change is seen, consecutive frames are the right
            // pair — that is what "stopped moving" means.
            let reference = if observed_change {
                previous.as_ref()
            } else {
                before.or(previous.as_ref())
            };
            let change = reference
                .filter(|r| !r.base64.is_empty() && !shot.base64.is_empty())
                .map(|r| pixel_diff(r, &shot).change_percentage);
            let stable = change.is_some_and(|pct| pct < cfg.stable_threshold);
            if change.is_some_and(|pct| pct >= cfg.stable_threshold) {
                observed_change = true;
            }

            settled_shot = Some(shot.clone());
            previous = Some(shot);
            if stable {
                // Settled — but "the screen stopped changing" and "the screen
                // has not started changing" look identical from two captures.
                // Until something has changed, keep looking: a repaint that
                // lands one poll late used to be reported to the model as
                // "0.0% changed", which reads as "the click did nothing".
                if observed_change || started.elapsed() >= floor {
                    break;
                }
            }
        }

        let shot = settled_shot?;
        // The tree read is the other half of the post-action capture, and it
        // is the half that walks a full accessibility tree. If it cannot
        // finish in what is left, the screenshot alone still tells the
        // verifier something; an empty tree tells it exactly as much as a
        // timed-out one would.
        let tree = crate::computer::budget::within(
            budget,
            "the post-action tree read",
            self.adapter.read_ui_tree(None),
        )
        .await
        .map(|result| result.unwrap_or_default())
        .unwrap_or_else(|e| {
            tracing::info!("post-action tree read skipped: {}", e);
            Vec::new()
        });
        Some(ScreenState::from_parts(tree, shot))
    }

    /// Three-state verification for one step.
    ///
    /// `Unverifiable` is not a pass and not a failure: it is the answer when
    /// no check could run.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn verify_step(
        &self,
        action: &DesktopAction,
        _result: &ActionResult,
        tree_before: Option<&[UiElement]>,
        screenshot_before: Option<&Screenshot>,
        screenshot_after: Option<&Screenshot>,
        state_after: Option<&ScreenState>,
    ) -> crate::computer::Result<(StepVerification, Option<ScreenDiff>, Option<EffectHint>)> {
        match action {
            DesktopAction::Screenshot { .. } => Ok((StepVerification::Verified, None, None)),
            DesktopAction::Wait { .. } => Ok((StepVerification::Verified, None, None)),
            // The probe succeeded if the pointer moved, and that is the most
            // the step can claim. Whether the control reacted is written on
            // the *next* frame, which this step does not have — and it cannot
            // be diffed here even if it did, because the capture draws the
            // pointer and the pointer just moved onto the target. "Verified"
            // would say "the control responds" on the strength of the cursor
            // having arrived.
            DesktopAction::Hover { .. } => Ok((StepVerification::Unverifiable, None, None)),
            DesktopAction::PointerMove { .. } => Ok((StepVerification::Verified, None, None)),
            DesktopAction::LaunchApp { name, wait_for_ready, .. } => {
                if *wait_for_ready {
                    self.verifier
                        .verify(
                            &VerificationCriteria::ProcessRunning { name: name.clone() },
                            &ActionResult::success(""),
                            None,
                        )
                        .await
                        .map(|v| (StepVerification::from_checked(v), None, None))
                } else {
                    Ok((StepVerification::Unverifiable, None, None))
                }
            }
            DesktopAction::ActivateWindow { title_pattern } => self
                .verifier
                .verify(
                    &VerificationCriteria::WindowTitleContains { pattern: title_pattern.clone() },
                    &ActionResult::success(""),
                    None,
                )
                .await
                .map(|v| (StepVerification::from_checked(v), None, None)),
            // Typing has a precise expected effect — the field's value — and
            // pixel diffing cannot see the difference between all of the text
            // and half of it. Use the tree when it carries values; fall back
            // to pixels when it does not.
            DesktopAction::Type { text, .. } => {
                let tree_answered = match (tree_before, state_after) {
                    (Some(before), Some(state)) => typed_value_hint(before, &state.ui_tree, text),
                    _ => None,
                };
                if let Some(hint) = tree_answered {
                    return Ok((hint.verification, None, Some(hint)));
                }
                // The tree had no value to compare. Read the field off the
                // frame instead, if the caller allows it — the answer is still
                // only "did the text land", never the text.
                #[cfg(feature = "vision")]
                if let Some(state) = state_after {
                    // `screenshot_before` is what the tree-less branch crops
                    // from: with no tree there is no box, and the change
                    // between the two frames is the only thing that names one.
                    if let Some(hint) = self
                        .read_back_typed(text, action, tree_before, screenshot_before, state)
                        .await
                    {
                        return Ok((hint.verification, None, Some(hint)));
                    }
                }
                self.verify_by_diff(
                    action,
                    tree_before,
                    screenshot_before,
                    screenshot_after,
                    state_after,
                )
                .await
                .map(|(verification, diff)| (verification, diff, None))
            }
            _ if is_screen_mutating_action(action) => self
                .verify_by_diff(
                    action,
                    tree_before,
                    screenshot_before,
                    screenshot_after,
                    state_after,
                )
                .await
                .map(|(verification, diff)| (verification, diff, None)),
            _ => Ok((StepVerification::Unverifiable, None, None)),
        }
    }

    /// Read a typed value back off the screen, as a person would.
    ///
    /// This is the answer when [`typed_value_hint`] has none: the field is
    /// there, the text went somewhere, and the tree does not publish what the
    /// field now holds. The frame the step just took does. Crop the field,
    /// read it, compare — and report only whether the typed text is in it.
    ///
    /// There are two ways to find the box, because the tree's way does not
    /// reach the screens this exists for:
    ///
    /// - **The tree names exactly one editable field.** Its bounds are the box.
    ///   This is also the only path that can ask "is this field private?"
    ///   before any pixels are taken, so a tree-marked password is never
    ///   cropped.
    /// - **The tree names none** — a tree-less app, or one that publishes
    ///   controls the walker cannot classify as editable. Then the change
    ///   between the two frames is the box: see [`typed_box_from_change`] for
    ///   how it is bounded, and for the changes it refuses rather than reads.
    ///
    /// The second path cannot ask the sensitivity question, and it does not
    /// pretend to: what it reads is what is *drawn* in the changed rectangle,
    /// and a field that masks what it holds draws bullets. That is the same
    /// argument [`ReadBack`] makes for the tree path — the picture has no text
    /// in it to leak — with one difference worth stating: a tree-less secure
    /// field whose renderer shows what it holds would be read. There is no
    /// marker to check, so the guarantee is the renderer's, not this code's.
    ///
    /// `None` is every way of not answering:
    ///
    /// - read-back is off, or this build has no `vision` feature;
    /// - the field cannot be identified — two or more editable elements in the
    ///   tree, or no tree and no single text-shaped change — so there is no
    ///   box to lean in on;
    /// - the tree marked the field sensitive, or it masks what it holds. See
    ///   [`ReadBack`] for why this is checked *before* any pixels are taken;
    /// - the models are not installed. A `stat` of the three files decides
    ///   that before an image is decoded, so a machine that never fetched the
    ///   vision models pays for a file check and nothing else.
    /// - the read is not trustworthy: OCR returned nothing, or the recognition
    ///   confidence is below what [`OcrText::is_reliable`] calls solid.
    ///
    /// A wrong reading is not a failed action *by itself*; what makes it one
    /// is the confidence gate in [`judge_read_back`]. Only a confident reading
    /// that lacks the typed text produces `Failed`, and even then the recovery
    /// is "look and retype", not a second keystroke.
    ///
    /// [`OcrText::is_reliable`]: crate::computer::vision::ocr_file::OcrText::is_reliable
    #[cfg(feature = "vision")]
    async fn read_back_typed(
        &self,
        typed: &str,
        action: &DesktopAction,
        tree_before: Option<&[UiElement]>,
        screenshot_before: Option<&Screenshot>,
        state: &ScreenState,
    ) -> Option<EffectHint> {
        if typed.is_empty() {
            return None;
        }
        let model_dir = match &self.config.read_back {
            ReadBack::Off => return None,
            ReadBack::InstalledModels => String::new(),
            ReadBack::ModelDir(dir) => dir.clone(),
        };
        let shot = &state.screenshot;
        // Two ways to find the box, and the tree's is preferred because it can
        // also say what the field *is*. See [`typed_box_from_change`] for the
        // other one.
        //
        // An empty tree and no tree are the same case here — a screen that
        // publishes no editable field — so both fall through to the frame.
        // Two or more fields are not: there is no box that can be attributed
        // to this typing, which is the rule [`editable_fields`] exists for.
        let fields = tree_before.map(editable_fields).unwrap_or_default();
        let crop = match fields.as_slice() {
            [field] => {
                // Asked before any pixels are taken: a field the tree calls
                // private is not looked at, whether or not its renderer masks
                // what it holds.
                if !crate::computer::sensitive_ui::scan_sensitive(std::slice::from_ref(field))
                    .is_empty()
                    || masks(field)
                {
                    return None;
                }
                let bounds = field.bounds?;
                if bounds.width == 0 || bounds.height == 0 {
                    return None;
                }
                let space = self.coordinate_space().await?;
                // A mapping built for another frame would crop the wrong part
                // of this one; the frame in hand has to be the frame the space
                // describes.
                if space.image_width != shot.width || space.image_height != shot.height {
                    return None;
                }
                space.screen_rect_to_image(&bounds)
            }
            // No field the tree can name — the screen this feature is really
            // for. The tree was never going to answer here, and neither would
            // a scan: what changed between the two frames is the only thing
            // that says where the typing went.
            [] => {
                let before = screenshot_before?;
                let change = pixel_diff(before, shot);
                // Where the action aimed, in the same pixel space. A `type`
                // with no target aimed at the focus, and nothing here knows
                // where that is — so the counting rule is what answers, and
                // `aim_in_image` already returns `None` for it.
                let aim = self.aim_in_image(action, shot).await;
                typed_box_from_change(before, shot, &change, aim)?
            }
            // Two or more: there is no box that can be attributed to this
            // typing. Guessing one would read the wrong field and compare the
            // other's text against it.
            _ => return None,
        };

        // A `stat` of the three model files, before anything expensive: "the
        // models were never fetched" is the common case on a machine that
        // never asked for vision, and it must not cost a decode.
        let preset = crate::computer::vision::model_download::configured_preset();
        if crate::computer::vision::model_download::ocr_model_paths_for(&model_dir, preset)
            .is_none()
        {
            tracing::debug!(
                "no OCR models ({}) installed; the typed value could not be read back",
                preset.key()
            );
            return None;
        }

        let read = crate::computer::budget::within(
            &self.step_budget(),
            "reading the field back",
            tokio::time::timeout(
                READ_BACK_TIMEOUT,
                crate::computer::vision::ocr_file::ocr_screenshot(shot, Some(crop), &model_dir),
            ),
        )
        .await
        .ok()? // out of step budget
        .ok()? // read-back timed out
        .ok()?; // OCR could not run at all
        judge_read_back(typed, &read)
    }

    /// Compare pre/post screen state.
    ///
    /// `geometry_counts` is true only for the actions whose whole point is a
    /// bounds change. For everything else a `moved:`-only diff is the desktop
    /// reflowing, not the action landing — and reporting it as verified is
    /// how a click that did nothing kept telling the model it had worked.
    ///
    /// When the action aimed at a place and the diff can say where its pixels
    /// changed, the aim is recorded on the diff and a change that happened
    /// only elsewhere is not counted as verification: the screen moved, but
    /// nothing here says because of this action. Without that test a clock
    /// repainting in the corner counted as proof the click landed.
    async fn verify_by_diff(
        &self,
        action: &DesktopAction,
        tree_before: Option<&[UiElement]>,
        screenshot_before: Option<&Screenshot>,
        screenshot_after: Option<&Screenshot>,
        state_after: Option<&ScreenState>,
    ) -> crate::computer::Result<(StepVerification, Option<ScreenDiff>)> {
        let (Some(shot_before), Some(shot_after)) = (screenshot_before, screenshot_after) else {
            return Ok((StepVerification::Unverifiable, None));
        };
        if shot_before.base64.is_empty() || shot_after.base64.is_empty() {
            return Ok((StepVerification::Unverifiable, None));
        }

        let tree_after = match state_after {
            Some(state) => state.ui_tree.clone(),
            None => self.adapter.read_ui_tree(None).await.unwrap_or_default(),
        };
        let before =
            ScreenState::from_parts(tree_before.unwrap_or(&[]).to_vec(), shot_before.clone());
        let after = ScreenState::from_parts(tree_after, shot_after.clone());
        let mut diff = before.diff(&after);
        if let Some(aim) = self.aim_in_image(action, shot_after).await {
            diff = diff.with_aim(aim);
        }
        let changed = diff.evidences_action(action_changes_geometry(action));
        let landed_elsewhere = diff.changed_elsewhere();
        if landed_elsewhere {
            tracing::info!(
                "verification: the screen changed, but not where the action aimed ({})",
                diff.summary()
            );
        } else if !changed {
            tracing::info!(
                "verification: no change attributable to the action ({})",
                diff.summary()
            );
        }
        // A change away from the target is not a failure — it is the absence
        // of evidence either way. A link in a long page navigates without the
        // link's own pixels changing, so "nothing moved where I clicked" is
        // routinely true of a click that worked. What it does mean is that
        // this diff cannot stand as proof *this* action landed, which is
        // exactly `Unverifiable`: no counter moves, nothing rolls back, and
        // the model is told the screen moved somewhere else.
        let verdict = if landed_elsewhere {
            StepVerification::Unverifiable
        } else if changed {
            StepVerification::Verified
        } else {
            StepVerification::Failed
        };
        Ok((verdict, Some(diff)))
    }

    /// The box an action was aimed at, in the pixels of the compared capture.
    ///
    /// `None` whenever the answer would be a guess: an action with no target
    /// (a key press, a launch), a target that no longer resolves, an element
    /// whose geometry was not published, or a capture whose pixel-to-point
    /// mapping the session never learned. Each of those is "not established",
    /// and the diff falls back to the traffic-light answer rather than
    /// claiming the change happened somewhere it did not.
    ///
    /// A bare coordinate aims at the pointer, and what a click causes is
    /// usually drawn at or beside it — a context menu opens just below the
    /// cursor — so the box is padded by a couple of diff blocks. An element's
    /// box is padded too: a button's own pixels often do not change when it
    /// opens a menu *next to* it, and the box is a claim about where the
    /// action could plausibly show, not a pixel mask.
    ///
    /// A drag aims at the **span between its ends**. A slider's thumb lands
    /// somewhere between where it was grabbed and where it was released, and a
    /// drag that moved a row of a list repaints that row; both are inside the
    /// rectangle the two endpoints span. That is the difference between "the
    /// screen changed somewhere" and "the drag did what it was aimed at doing",
    /// which is the question the verdict is trying to answer.
    async fn aim_in_image(&self, action: &DesktopAction, screenshot: &Screenshot) -> Option<Rect> {
        // A scroll's content moves under the pointer — the whole pane, not one
        // box — so there is nothing to attribute a change to.
        let endpoints: Vec<&ClickTarget> = match action {
            DesktopAction::Click { target, .. }
            | DesktopAction::DoubleClick { target, .. }
            | DesktopAction::Type { target: Some(target), .. } => vec![target],
            DesktopAction::Drag { from, to } => vec![from, to],
            _ => return None,
        };
        // The changed regions are in image pixels, so the aim must be too.
        // Without the mapping, absolution is impossible: on a Retina display
        // screen points are half the pixel count.
        let space = self.coordinate_space().await?;
        if space.image_width != screenshot.width || space.image_height != screenshot.height {
            return None;
        }
        let mut span: Option<Rect> = None;
        for target in endpoints {
            let Some(rect) = self.aim_box_for(target).await else {
                // One end is a guess, so the span is a guess. Saying nothing is
                // the honest answer; half a span would exclude the end that
                // did resolve.
                return None;
            };
            span = Some(match span {
                None => rect,
                Some(acc) => {
                    let x0 = acc.x.min(rect.x);
                    let y0 = acc.y.min(rect.y);
                    let x1 = acc
                        .x
                        .saturating_add(acc.width as i32)
                        .max(rect.x.saturating_add(rect.width as i32));
                    let y1 = acc
                        .y
                        .saturating_add(acc.height as i32)
                        .max(rect.y.saturating_add(rect.height as i32));
                    Rect::new(x0, y0, (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32)
                }
            });
        }
        Some(space.screen_rect_to_image(&span?))
    }

    /// Where one end of an action lands, padded to what its effect could touch.
    ///
    /// Split out of [`aim_in_image`](Self::aim_in_image) because a drag has two
    /// ends and they are resolved the same way; a closure that both awaited
    /// and borrowed the adapter's target resolution would not compile, and
    /// writing the resolution twice is how the two ends drift apart.
    async fn aim_box_for(&self, target: &ClickTarget) -> Option<Rect> {
        /// Half-width, in screen points, of the box a bare coordinate aims at.
        const POINT_AIM_PADDING: i32 = 32;
        /// How far outside an element's published box its effect may show.
        const ELEMENT_AIM_PADDING: i32 = 48;
        let padded = |rect: Rect, padding: i32| {
            Rect::new(
                rect.x.saturating_sub(padding),
                rect.y.saturating_sub(padding),
                rect.width
                    .saturating_add((padding * 2) as u32)
                    .max(rect.width),
                rect.height
                    .saturating_add((padding * 2) as u32)
                    .max(rect.height),
            )
        };
        let point_box = |point: crate::computer::Point| {
            padded(Rect::new(point.x, point.y, 0, 0), POINT_AIM_PADDING)
        };
        match target {
            ClickTarget::Coordinate(point) => Some(point_box(*point)),
            other => {
                let resolved = self.adapter.resolve_target(other).await.ok()?;
                let bounds = resolved
                    .element
                    .as_ref()
                    .and_then(|element| element.bounds)
                    .filter(|bounds| bounds.width > 0 && bounds.height > 0);
                match (bounds, resolved.point) {
                    (Some(bounds), _) => Some(padded(bounds, ELEMENT_AIM_PADDING)),
                    (None, Some(point)) => Some(point_box(point)),
                    (None, None) => None,
                }
            }
        }
    }

    /// Record an action that failed before it could be verified.
    ///
    /// Returns the new consecutive-failure count.
    pub async fn record_error(&self, message: &str) -> usize {
        let mut state = self.state.lock().await;
        state.step += 1;
        state.consecutive_failures += 1;
        state.last_verification = Some(StepVerification::Failed);
        state.last_error = Some(message.to_string());
        state.consecutive_failures
    }

    /// A refusal, as a step: nothing delivered, nothing failed.
    ///
    /// One constructor for every pre-dispatch refusal, so "the boundary said
    /// no" looks the same in the result, in the evidence and in the run
    /// timeline whichever boundary it was — a disabled control, an app that is
    /// not on the list. It counts through [`account`](Self::account) with
    /// `Refused`, which is what keeps it out of the auto-rollback threshold.
    async fn refused_outcome(
        &self,
        action: DesktopAction,
        reason: &str,
        recovery: &str,
        snapshots_taken: usize,
        budget: &StepBudget,
    ) -> StepOutcome {
        let mut evidence =
            crate::computer::ActionEvidence::new(Rung::AxAction, ActionEffect::Refused)
                .with_delivery(crate::computer::DeliveryDisposition::NotDelivered)
                .with_recovery(recovery.to_string());
        evidence.escalation = None;
        let result = ActionResult::error(format!("refused: {reason} — {recovery}"));
        // Nothing was delivered, so nothing failed either; a refusal must not
        // count toward the auto-rollback threshold.
        let verification = StepVerification::Unverifiable;
        let (rolled_back, reflection) = self
            .account(&action, &result, verification, ActionEffect::Refused)
            .await;
        StepOutcome {
            action,
            result,
            verification,
            evidence: Some(evidence),
            state_after: None,
            screenshot_before: None,
            diff: None,
            sensitive_before: Vec::new(),
            sensitive_after: self.sensitive_findings().await,
            sensitive_target: false,
            reflection,
            snapshots_taken,
            rolled_back,
            settle_ms: None,
            budget: Some(budget.summary()),
        }
    }

    /// Why the per-app boundary refuses this action, when it does.
    ///
    /// Two questions, asked in one place because they must not be able to
    /// disagree. First: does the app the action *names* pass the list — the
    /// app it says outright, or the app the tree snapshot says its element
    /// came from. Second, for an action that names no app at all: which app
    /// would *receive* it, and is that one on the list?
    ///
    /// The second question is what turns "a coordinate click names nothing,
    /// so it is always refused" into a check: a session that may act on
    /// Safari can click a point while Safari is in front, because the platform
    /// can say that Safari is what would get the event — and a session that
    /// may not act on Finder still has its click refused when Finder is. When
    /// the platform cannot say, the answer stays no: "nobody could check" has
    /// never been clearance at this layer.
    ///
    /// An action that *names* an allowed app is judged on that name here. Its
    /// delivered receiver is checked later, per rung, by
    /// [`rung_border_refusal`](Self::rung_border_refusal) — the accessibility
    /// rung acts on the element itself and may succeed whatever is in front,
    /// so a receiver mismatch must not pre-empt it.
    ///
    /// Actions that are not input at all (a screenshot, a tree read, a system
    /// query) are not aimed at an app and are not what an allowlist exists to
    /// stop, so they pass. Reading a window's content is not acting on it.
    async fn app_scope_refusal(&self, action: &DesktopAction) -> Option<String> {
        if self.config.allow_apps.is_empty() || !aims_at_an_app(action) {
            return None;
        }
        // A drag has two endpoints, and the drop lands at the second: a drag
        // that starts inside the list and ends outside it moves data across
        // the boundary, so both have to be refused together.
        for claim in self.claimed_apps(action).await {
            match claim {
                Claim::App(app) => {
                    if let Some(reason) = self.config.allow_apps.refusal(Some(&app)) {
                        return Some(reason);
                    }
                }
                // The action names an element (or a process) but the app
                // behind it could not be established. That is the older
                // situation and keeps the older verdict: an action nobody can
                // place inside the list is not assumed to be inside it.
                Claim::Unresolved => {
                    return Some(format_unresolved_app(self.config.allow_apps.entries()))
                }
            }
        }
        // Every part that claims an app passed. What is left is the part that
        // claims nothing — and that is answered by asking who would receive
        // the event, not by refusing on the absence of a claim.
        self.unattributed_receivers(action).await
    }

    /// The app each part of `action` claims, where it claims one.
    ///
    /// A part that names nothing (a coordinate, a bare key press) contributes
    /// nothing here — it is [`unattributed_receivers`]'s question. A part that
    /// names something the adapter cannot place contributes
    /// [`Claim::Unresolved`], which is a refusal rather than a question: the
    /// caller said where it meant to act, and nothing corroborated it.
    async fn claimed_apps(&self, action: &DesktopAction) -> Vec<Claim> {
        let targets: Vec<&ClickTarget> = match action {
            DesktopAction::Drag { from, to } => vec![from, to],
            DesktopAction::Click { target, .. }
            | DesktopAction::DoubleClick { target, .. }
            | DesktopAction::Hover { target }
            | DesktopAction::Scroll { target, .. } => vec![target],
            DesktopAction::Type { target: Some(target), .. } => vec![target],
            _ => Vec::new(),
        };
        // An element target is a claim about one app, and the adapter's
        // snapshot is what corroborates it. A coordinate in the same position
        // claims nothing, and is left to `unattributed_receivers`.
        if !targets.is_empty() {
            let mut claims = Vec::new();
            for target in targets {
                if matches!(target, ClickTarget::Coordinate(_)) {
                    continue;
                }
                match self.app_of_target(target).await {
                    Some(app) => claims.push(Claim::App(app)),
                    None => claims.push(Claim::Unresolved),
                }
            }
            return claims;
        }
        let mut claims = Vec::new();
        match self.action_app(action).await {
            Some(app) => claims.push(Claim::App(app)),
            // The action names a target but only in terms the platform cannot
            // turn into an app — a process id. Refusing is the older, safer
            // answer, and it is the named side that has to produce it: no
            // receiving app exists to ask about.
            None if names_an_app(action) => claims.push(Claim::Unresolved),
            None => {}
        }
        claims
    }

    /// The receiving app for every part of `action` that names none.
    ///
    /// The pre-dispatch half of the boundary. An action whose parts are all
    /// named has nothing to ask here — its claimed app was already judged, and
    /// its delivery is checked per rung.
    async fn unattributed_receivers(&self, action: &DesktopAction) -> Option<String> {
        let mut receivers: Vec<Option<String>> = Vec::new();
        let mut unnamed_parts = 0usize;
        match action {
            // A click lands at a point, not in "the frontmost app": the window
            // drawn there decides, and that may be a background window of a
            // different app.
            DesktopAction::Click { target, .. }
            | DesktopAction::DoubleClick { target, .. }
            | DesktopAction::Hover { target }
            | DesktopAction::Scroll { target, .. } => {
                if let ClickTarget::Coordinate(point) = target {
                    unnamed_parts += 1;
                    receivers.push(self.adapter.application_at(*point).await);
                }
            }
            DesktopAction::Type { target, .. } => match target {
                // Typing blind: the keystrokes go to whatever holds focus.
                None => {
                    unnamed_parts += 1;
                    receivers.push(self.adapter.active_app().await);
                }
                Some(ClickTarget::Coordinate(point)) => {
                    unnamed_parts += 1;
                    receivers.push(self.adapter.application_at(*point).await);
                }
                Some(_) => {}
            },
            // Keys go to whatever holds focus. There is no point to hit test:
            // the keyboard has one receiver, and it is the frontmost app.
            DesktopAction::KeyPress { .. } => {
                unnamed_parts += 1;
                receivers.push(self.adapter.active_app().await);
            }
            DesktopAction::Drag { from, to } => {
                for target in [from, to] {
                    if let ClickTarget::Coordinate(point) = target {
                        unnamed_parts += 1;
                        receivers.push(self.adapter.application_at(*point).await);
                    }
                }
            }
            // A pointer move is a bare point: the app drawn there receives the
            // move event, exactly as it would receive a click.
            DesktopAction::PointerMove { to } => {
                unnamed_parts += 1;
                receivers.push(self.adapter.application_at(*to).await);
            }
            _ => {}
        }
        if unnamed_parts == 0 {
            // The clipboard is the one action with no receiver to ask about
            // that the boundary still gates: it carries data out of the app
            // that wrote it, and where it will be pasted is not knowable at
            // the time it is set.
            if matches!(action, DesktopAction::ClipboardSet { .. }) {
                return self.config.allow_apps.refusal(None);
            }
            return None;
        }
        receivers
            .iter()
            .find_map(|app| self.config.allow_apps.refusal_reaching(app.as_deref()))
    }

    /// Why one rung must not deliver this action, for the per-app boundary.
    ///
    /// The pre-dispatch check judges what an action *names*. This judges where
    /// a rung would actually deliver. The two input rungs reach a screen and a
    /// focus, and on this ladder they are the fallbacks that run after another
    /// rung failed — exactly the moment a covering window or a different
    /// frontmost app decides who receives the event. An action that named an
    /// allowed app must not then have its keystrokes typed into someone else's
    /// window, which is the edge the name-level check cannot see.
    ///
    /// The accessibility and page rungs are deliberately not checked: they act
    /// on an element, not on a point or a focus, so the app that holds the
    /// foreground has nothing to do with where they land.
    ///
    /// Only a positive mismatch refuses. A platform that cannot say which app
    /// is at the point or in front gets the benefit of the evidence that
    /// already passed the name check: unlike the unnamed case, something *has*
    /// established where this action is aimed.
    async fn rung_border_refusal(&self, action: &DesktopAction, rung: Rung) -> Option<String> {
        if self.config.allow_apps.is_empty() {
            return None;
        }
        let receiving = match (rung, action) {
            (Rung::Pixel, DesktopAction::Click { target, .. })
            | (Rung::Pixel, DesktopAction::DoubleClick { target, .. })
            | (Rung::Pixel, DesktopAction::Scroll { target, .. })
            // The pixel rung of a typing step is a click into the field, and
            // whatever it focuses is where the keystrokes go next — so the
            // point belongs to this check just as much as a click does.
            | (Rung::Pixel, DesktopAction::Type { target: Some(target), .. }) => {
                // Only for a named target: a coordinate click was already
                // placed by the pre-dispatch check, and asking again a few
                // calls later would only be able to disagree with the answer
                // the action was admitted on.
                if matches!(target, ClickTarget::Coordinate(_)) {
                    return None;
                }
                let resolved = self.adapter.resolve_target(target).await.ok()?;
                let point = resolved.point?;
                self.adapter.application_at(point).await
            }
            (Rung::Keyboard, _) => self.adapter.active_app().await,
            _ => return None,
        };
        let receiving = receiving?;
        // Same app as the action named: the fallback is delivering where the
        // action was always aimed.
        if let Some(named) = self.action_app(action).await {
            if named.eq_ignore_ascii_case(&receiving) {
                return None;
            }
        }
        if self.config.allow_apps.allows(&receiving) {
            return None;
        }
        Some(format!(
            "the {rung:?} fallback would deliver this action to {receiving:?}, which is not on the \
             [computer] allow_apps list — the action was not sent to that app. Bring the app the \
             action names to the front (or ask the operator to add {receiving:?}) and retry"
        ))
    }

    /// The application an action is aimed at, as well as this session can tell.
    ///
    /// Two sources: the action names one (`launch_app`, or a window action's
    /// title pattern), or the resolved target says which app the snapshot came
    /// from. A resolved app is evidence rather than a claim; a name the caller
    /// supplied is a claim, and is used because the alternative is refusing
    /// every launch and every window move.
    async fn action_app(&self, action: &DesktopAction) -> Option<String> {
        match action {
            DesktopAction::LaunchApp { name, .. } => Some(name.clone()),
            // A window action names its window, not its app: use the pattern
            // as the best available answer. `title_pattern` is a glob, so a
            // strict list may not match it — the refusal says so, and the
            // caller can target an element instead.
            DesktopAction::ActivateWindow { title_pattern }
            | DesktopAction::CloseWindow { title_pattern }
            | DesktopAction::GetWindowGeometry { title_pattern }
            | DesktopAction::MoveWindow { title_pattern, .. }
            | DesktopAction::ResizeWindow { title_pattern, .. }
            | DesktopAction::MinimizeWindow { title_pattern }
            | DesktopAction::MaximizeWindow { title_pattern } => Some(title_pattern.clone()),
            // A process action names its target only when the caller gave a
            // name; a pid identifies a process, not an app, so the boundary
            // cannot place it and refuses rather than guesses.
            DesktopAction::KillProcess { name, .. }
            | DesktopAction::RestartProcess { name, .. }
            | DesktopAction::SetProcessPriority { name, .. } => name.clone(),
            // A scroll names its target the way a click does, but it is not
            // one of the actions the ladder aims through `resolved_target`, so
            // ask the adapter directly: a session that may act on Safari must
            // not have scrolling its window refused while clicking the same
            // element is allowed.
            DesktopAction::Scroll { target, .. } => self.app_of_target(target).await,
            other => self
                .resolved_target(other)
                .await
                .and_then(|resolved| resolved.app),
        }
    }

    /// The app an element target belongs to, resolved through the adapter's
    /// snapshot. A bare point belongs to whatever draws under it, which is no
    /// answer at all.
    async fn app_of_target(&self, target: &ClickTarget) -> Option<String> {
        if matches!(target, ClickTarget::Coordinate(_)) {
            return None;
        }
        self.adapter
            .resolve_target(target)
            .await
            .ok()
            .and_then(|resolved| resolved.app)
    }

    /// Update failure counters, trigger auto-rollback, and reflect.
    async fn account(
        &self,
        action: &DesktopAction,
        result: &ActionResult,
        verification: StepVerification,
        effect: ActionEffect,
    ) -> (bool, Option<FailureAnalysis>) {
        let mut reflection = None;
        let mut rolled_back = false;

        let (failures, attempt) = {
            let mut state = self.state.lock().await;
            state.step += 1;
            state.last_verification = Some(verification);
            match verification {
                StepVerification::Verified => {
                    state.consecutive_failures = 0;
                    state.rollback_triggered = false;
                    state.last_error = None;
                }
                StepVerification::Failed => {
                    state.consecutive_failures += 1;
                    state.last_error = Some(format!("verified failed ({})", effect.as_str()));
                }
                // Move no counter: counting it as a failure would roll back
                // work that may be fine; counting it as a success would reset
                // failures that were real.
                StepVerification::Unverifiable => {}
            }
            (state.consecutive_failures, state.consecutive_failures as u32)
        };

        if verification == StepVerification::Failed {
            let analysis = self.reflection.analyze_failure(
                &VerificationCriteria::Success,
                action,
                result,
                attempt,
            );
            reflection = Some(analysis);

            let should_rollback = {
                let mut state = self.state.lock().await;
                if failures >= self.config.auto_rollback_threshold && !state.rollback_triggered {
                    state.rollback_triggered = true;
                    true
                } else {
                    false
                }
            };
            if should_rollback {
                tracing::warn!("auto-rollback after {} consecutive failures", failures);
                if let Some(ref mgr) = self.rollback_manager {
                    let mut mgr = mgr.lock().await;
                    match mgr.rollback().await {
                        Ok(()) => {
                            rolled_back = true;
                            tracing::info!("auto-rollback completed");
                        }
                        Err(e) => tracing::error!("auto-rollback failed: {}", e),
                    }
                }
            }
        }

        (rolled_back, reflection)
    }
}

/// The accessibility action a rung would perform for this target.
///
/// The whole action is passed because `SetValue` carries text: the gate only
/// asks *which* action a rung would send, never *what* it writes.
fn ax_action_for(target: &ClickTarget, action: &DesktopAction) -> crate::computer::AxAction {
    match (target, action) {
        (_, DesktopAction::Type { target: Some(_), .. }) => {
            crate::computer::AxAction::SetValue { value: String::new() }
        }
        _ => crate::computer::AxAction::Press,
    }
}

/// A verdict derived from the UI tree rather than from pixels.
///
/// The pixel diff can only answer "something changed". Some actions have an
/// exact expected effect — typing `hello` should leave the field holding
/// `hello`, not `hel` — and the tree can say whether it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectHint {
    pub effect: ActionEffect,
    pub verification: StepVerification,
    pub recovery: Option<String>,
}

/// Compare editable field values before and after typing.
///
/// Returns `None` when the tree cannot answer — no editable field changed,
/// more than one did, or a field is masked (a secure field reports bullets,
/// not the text) — and the caller falls back to the pixel diff.
pub(crate) fn typed_value_hint(
    before: &[UiElement],
    after: &[UiElement],
    typed: &str,
) -> Option<EffectHint> {
    if typed.is_empty() {
        return None;
    }
    let before_fields = editable_values(before);
    let after_fields = editable_values(after);
    if before_fields.is_empty() || before_fields.len() != after_fields.len() {
        return None;
    }
    let mut changed: Option<&str> = None;
    for ((role, before_value), (after_role, after_value)) in before_fields.iter().zip(&after_fields)
    {
        if role != after_role || is_masked(role, before_value) || is_masked(role, after_value) {
            return None;
        }
        if before_value != after_value {
            if changed.is_some() {
                // More than one field moved: we cannot attribute the text.
                return None;
            }
            changed = Some(after_value);
        }
    }
    let actual = changed?;
    if actual.contains(typed) {
        return Some(EffectHint {
            effect: ActionEffect::Confirmed,
            verification: StepVerification::Verified,
            recovery: None,
        });
    }
    Some(EffectHint {
        effect: ActionEffect::Partial,
        verification: StepVerification::Failed,
        recovery: Some(
            "only part of the text landed in the field — click the field, select all, and retype; \
             do not retype on top of it without clearing first"
                .to_string(),
        ),
    })
}

/// Normalized role: lowercase, alphanumerics only, so `AXTextField`,
/// `text field` and `textfield` compare equal.
fn normalized_role(role: &str) -> String {
    role.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Name an element the way a page can be asked about it.
///
/// The tree is the better source — it carries the role the page itself
/// exposed — but a label-only target already says enough to look it up.
fn page_target(target: &ClickTarget, resolved: Option<&ActionTarget>) -> PageTarget {
    if let Some(element) = resolved.and_then(|r| r.element.as_ref()) {
        return PageTarget::new(element.role.clone(), element.label.clone());
    }
    match target {
        ClickTarget::ElementRoleLabel { role, label } => {
            PageTarget::new(role.clone(), Some(label.clone()))
        }
        ClickTarget::ElementLabel(label) => PageTarget::new("", Some(label.clone())),
        _ => PageTarget::new("", None),
    }
}

/// The key that activates an element once it holds focus.
///
/// Toggle-like controls — buttons, check boxes, radio buttons, switches, tab
/// items — answer to Space. Things activated from a menu or a list — links,
/// menu items, rows — answer to Return. The distinction matters because the
/// wrong key is not a no-op: Return on a focused text field submits the form.
fn activation_key(role: &str) -> &'static str {
    let role = normalized_role(role);
    let space = role.contains("button")
        || role.contains("checkbox")
        || role.contains("radiobutton")
        || role.contains("switch")
        || role.contains("toggle")
        || role.contains("tabitem")
        || role.contains("segmentedcontrol")
        || role.ends_with("tab");
    if space {
        "space"
    } else {
        "enter"
    }
}

fn is_editable(role: &str) -> bool {
    let role = normalized_role(role);
    role.contains("textfield")
        || role.contains("textarea")
        || role.contains("combobox")
        || role.contains("searchfield")
        || role == "edit"
}

fn is_masked(role: &str, value: &str) -> bool {
    normalized_role(role).contains("secure")
        || normalized_role(role).contains("password")
        || value.starts_with('•')
        || value.starts_with('\u{2022}')
}

/// `(normalized role, value)` for every editable field, depth-first.
fn editable_values(tree: &[UiElement]) -> Vec<(String, String)> {
    fn walk(element: &UiElement, out: &mut Vec<(String, String)>) {
        if is_editable(&element.role) {
            out.push((normalized_role(&element.role), element.value.clone().unwrap_or_default()));
        }
        for child in &element.children {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    for root in tree {
        walk(root, &mut out);
    }
    out
}

/// Every editable element in the tree, depth-first.
///
/// The read-back needs the element, not its value — the bounds are the box to
/// crop and the id is what makes it addressable. `&'a UiElement` borrows the
/// tree the step started from. Gated with its only caller: a build without the
/// vision feature cannot crop anything to read.
#[cfg(feature = "vision")]
fn editable_fields(tree: &[UiElement]) -> Vec<&UiElement> {
    fn walk<'a>(element: &'a UiElement, out: &mut Vec<&'a UiElement>) {
        if is_editable(&element.role) {
            out.push(element);
        }
        for child in &element.children {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    for root in tree {
        walk(root, &mut out);
    }
    out
}

/// Whether a field renders what it holds as bullets.
///
/// The same test [`is_masked`] makes of a value, asked of an element whose
/// value may be absent — which is exactly the field this read-back exists for.
/// A secure or password role is enough on its own: the platform does not have
/// to have published the masked text for the field to be one. Gated with its
/// only caller, like [`editable_fields`].
#[cfg(feature = "vision")]
fn masks(field: &UiElement) -> bool {
    let role = normalized_role(&field.role);
    role.contains("secure")
        || role.contains("password")
        || field
            .value
            .as_deref()
            .is_some_and(|value| is_masked(&field.role, value))
}

/// Which rectangle of the screen the typing went into, when the tree cannot
/// say.
///
/// This is the tree-less half of the read-back, and it exists because the
/// tree's half is unavailable exactly where the answer matters most. A screen
/// that publishes no tree — a canvas app, an Electron input that exposes
/// nothing, a remote desktop — is also a screen whose scan supplies no
/// editable field: the detector names a `text_field` in the *outline*, but
/// that element never enters `tree_before`, and OCR's elements carry
/// `role: "text"`, which [`is_editable`] does not match. So on such a screen
/// there is no `editable_fields` answer and no box to crop, and "type, then
/// look to confirm" — the thing a person does without thinking — had no
/// implementation.
///
/// What is left is what a person has: the frame before, the frame after, and
/// the knowledge that typing changes pixels. The box that changed is the box
/// to lean in on. Three ways of not answering, each for a reason that would
/// make a guess wrong:
///
/// - **Nothing in either frame.** There is no comparison, so no change.
/// - **The change is not text-shaped.** A whole window repainting: a box that
///   covers a large part of the frame is not evidence about one field, and
///   cropping it would ask OCR "what does this screen say" and then compare
///   that with the typed text. A containment test that could pass by accident
///   is worse than no test.
/// - **More than one box changed and nothing says which is the field.** Two
///   fields changing at once means the typing cannot be attributed to either;
///   that is the same rule [`editable_fields`] applies when it refuses two
///   candidates.
///
/// # Why the caller's target matters
///
/// Counting changed boxes alone is not enough on a real desktop: a menu-bar
/// clock repaints every second, a spinner turns, a video plays. On such a
/// screen "exactly one box changed" is almost never true, and the read-back
/// would be dead on the very screens it exists for.
///
/// So when the action named a target, the target decides: a changed box that
/// overlaps it is this typing, and a changed box elsewhere is the clock. The
/// crop is clipped to the target's own box, which also stops a neighbour's
/// text from being read in and matched against what was typed.
///
/// `aim` is in the pixel space of `after`, like the regions. `None` means the
/// action named no target — a bare `type` into whatever holds focus — and the
/// counting rule is what is left. That case is the honest ceiling of this
/// function: it cannot tell a field from a clock, so it only answers when the
/// screen offers exactly one candidate.
///
/// All boxes are in the pixel space of `after` — the same space
/// [`ScreenState::diff`] reports its regions in.
///
/// A caret is the case that makes this narrow: it blinks inside the field, so
/// the changed box is the caret's body, not the field's. Widening it by a
/// fixed margin is deliberate — the reading needs a box big enough to hold
/// text, and a caret's box is not.
#[cfg(feature = "vision")]
fn typed_box_from_change(
    before: &Screenshot,
    after: &Screenshot,
    change: &crate::computer::vision::PixelDiffResult,
    aim: Option<Rect>,
) -> Option<Rect> {
    /// How far past a changed box the reading looks, in image pixels.
    ///
    /// A caret blinks *inside* the field, so its box is a few pixels wide and
    /// says nothing about where the text is. The field it sits in is the thing
    /// that changed; the margin is what turns one into the other.
    const TEXT_MARGIN: i32 = 24;
    /// The largest share of the frame a "field" may cover.
    ///
    /// Beyond this the change is the screen, not a field — a page load, a
    /// theme switch, a window that just came forward. Cropping it and calling
    /// the result a field's value would confirm any typing anywhere.
    const MAX_FRAME_SHARE: f32 = 0.5;

    if before.width == 0 || before.height == 0 || after.width == 0 || after.height == 0 {
        return None;
    }
    // The regions are only comparable when both frames are in one space. A
    // capture taken while the display reconfigured — or a crop against a
    // full-screen frame — would compare unrelated pixels, and the box it
    // produced would be a place, not the place.
    if before.width != after.width || before.height != after.height {
        return None;
    }
    // A region list that was cut short cannot say how many boxes changed, and
    // both rules below turn on counting them.
    if change.regions_truncated {
        return None;
    }
    let frame = after.width as f32 * after.height as f32;
    let region = match aim {
        Some(aim) => {
            // The boxes that touch the target. A box somewhere else on the
            // screen is not this typing, however much it changed.
            let mut touching = change
                .changed_regions
                .iter()
                .filter(|region| region.overlaps(&aim));
            let first = *touching.next()?;
            if touching.next().is_some() {
                // Two changes inside the target's own box: the field and
                // something drawn over it, or a menu that opened under the
                // pointer. Nothing here says which is the text.
                return None;
            }
            if first.width as f32 * first.height as f32 > frame * MAX_FRAME_SHARE {
                return None;
            }
            first
        }
        None => {
            let [region] = change.changed_regions.as_slice() else {
                return None;
            };
            let box_area = region.width as f32 * region.height as f32;
            if box_area > frame * MAX_FRAME_SHARE {
                return None;
            }
            *region
        }
    };

    // The margin widens the change to something readable, and the frame is the
    // enclosing bound. When the action named a target, the target is the other
    // bound: a crop that reached past it would read a neighbour's text and
    // compare that with what was typed, which is a confirmation the typing did
    // not earn.
    let mut x = region.x.saturating_sub(TEXT_MARGIN).max(0);
    let mut y = region.y.saturating_sub(TEXT_MARGIN).max(0);
    let mut right =
        (region.x as i64 + region.width as i64 + TEXT_MARGIN as i64).min(after.width as i64) as i32;
    let mut bottom = (region.y as i64 + region.height as i64 + TEXT_MARGIN as i64)
        .min(after.height as i64) as i32;
    if let Some(aim) = aim {
        let aim_right = aim.x.saturating_add(aim.width as i32);
        let aim_bottom = aim.y.saturating_add(aim.height as i32);
        x = x.max(aim.x);
        y = y.max(aim.y);
        right = right.min(aim_right);
        bottom = bottom.min(aim_bottom);
    }
    let width = (right - x).max(0) as u32;
    let height = (bottom - y).max(0) as u32;
    if width == 0 || height == 0 {
        return None;
    }
    Some(Rect::new(x, y, width, height))
}

/// Turn what OCR read into a verdict, dropping the reading itself.
///
/// Free rather than a method: the comparison needs no session, no adapter
/// and no display, so it is testable with two strings — and keeping it out
/// of the impl is what lets the test prove the recognized text never
/// reaches the `EffectHint` it produces.
///
/// Nothing found is not evidence of an empty field — a font the recognizer
/// cannot parse reads as nothing too — and low confidence is not evidence
/// of a typo. Both are `None`, "the screen could not answer".
///
/// Whitespace is stripped from both sides before comparing: a field
/// renders `hello world` the same way at the right point size, and an OCR
/// line break is an artifact of the detector, not a character in the
/// field.
#[cfg(feature = "vision")]
pub(crate) fn judge_read_back(
    typed: &str,
    read: &crate::computer::vision::ocr_file::OcrText,
) -> Option<EffectHint> {
    if read.text.trim().is_empty() || !read.is_reliable() {
        return None;
    }
    let shown: String = read.text.chars().filter(|c| !c.is_whitespace()).collect();
    let wanted: String = typed.chars().filter(|c| !c.is_whitespace()).collect();
    if wanted.is_empty() {
        return None;
    }
    if shown.contains(&wanted) {
        return Some(EffectHint {
            effect: ActionEffect::Confirmed,
            verification: StepVerification::Verified,
            recovery: None,
        });
    }
    Some(EffectHint {
        effect: ActionEffect::Partial,
        verification: StepVerification::Failed,
        recovery: Some(
            "the field does not read back as the text that was typed — click it, select all, \
             and retype; the reading is optical and is not proof the text is missing, so \
             check the field before typing again"
                .to_string(),
        ),
    })
}

/// Whether an action is aimed at an application at all.
///
/// The distinction the per-app boundary turns on. A screenshot, a tree read, a
/// clipboard read and a system query observe the machine; they are not what an
/// allowlist exists to stop, and refusing them would make a locked-down session
/// unable to see what it is allowed to work on. Everything the ladder can
/// deliver — a click, a keystroke, a launch, a window move, a process kill — is,
/// including the actions that name their target only by coordinate. That is
/// deliberate: an action whose app cannot be established is not thereby inside
/// the list.
fn aims_at_an_app(action: &DesktopAction) -> bool {
    !matches!(
        action,
        DesktopAction::Screenshot { .. }
            | DesktopAction::ReadUiTree { .. }
            | DesktopAction::ListWindows
            | DesktopAction::Wait { .. }
            | DesktopAction::ClipboardGet
            | DesktopAction::GetSystemStatus
            | DesktopAction::ListProcesses { .. }
    )
}

/// What an action says about which app it is aimed at.
///
/// Three states rather than two, because the two kinds of "no app" call for
/// opposite answers: silence (a coordinate, a bare key press) is answered by
/// asking who would receive the event, while a claim that cannot be
/// corroborated (an element whose app the snapshot does not carry) has to be
/// refused — something was asserted, and nothing bore it out.
enum Claim {
    /// The action names this app, or an element the adapter says belongs to it.
    App(String),
    /// The action names a target that cannot be placed in an app.
    Unresolved,
}

/// The refusal for a named target whose app could not be established.
fn format_unresolved_app(entries: Vec<String>) -> String {
    format!(
        "this action targets an element whose application could not be established, and \
         [computer] allow_apps is set ({}) — it cannot be shown to stay inside the list. \
         Re-read the UI tree so the app is recorded with the element, then target a fresh \
         element id; the action was not sent",
        entries.join(", ")
    )
}

/// Whether the action's *defined* target is an app, as opposed to a coordinate
/// or a focused window.
///
/// Different from [`aims_at_an_app`], which asks whether the action reaches an
/// app at all. This asks whether the action itself carries a name that the
/// boundary can match against the list; a process identified only by pid does
/// not, which is why it is refused rather than asked about.
fn names_an_app(action: &DesktopAction) -> bool {
    match action {
        DesktopAction::LaunchApp { .. }
        | DesktopAction::ActivateWindow { .. }
        | DesktopAction::CloseWindow { .. }
        | DesktopAction::GetWindowGeometry { .. }
        | DesktopAction::MoveWindow { .. }
        | DesktopAction::ResizeWindow { .. }
        | DesktopAction::MinimizeWindow { .. }
        | DesktopAction::MaximizeWindow { .. } => true,
        DesktopAction::KillProcess { name, .. }
        | DesktopAction::RestartProcess { name, .. }
        | DesktopAction::SetProcessPriority { name, .. } => name.is_some(),
        _ => false,
    }
}

/// What the caller must change when an action was refused before it ran.
///
/// Only errors the *caller* can fix are listed: a timeout or a tool failure
/// may succeed on retry and must not be turned into a refusal.
fn refusal_recovery(error: &ComputerError) -> Option<&'static str> {
    match error {
        ComputerError::StaleElement(_) => Some(
            "re-read the UI tree (screen_state) and target a fresh element id — the snapshot this \
             id came from is gone",
        ),
        ComputerError::ElementNotFound(_) => Some(
            "no element matched — re-read the UI tree and target a current element id, label, or \
             screen coordinates",
        ),
        // Different advice from "not found": the element may exist, the read
        // just did not reach it. Re-aiming would waste the turn.
        ComputerError::TreeIncomplete(_) => Some(
            "the element was not in the part of the tree that was read — re-read the UI tree with \
             a larger max_depth, then target a fresh element id",
        ),
        ComputerError::AmbiguousTarget(_) => Some(
            "more than one element matched — use the element id of the one you mean, or add its \
             role to the target",
        ),
        ComputerError::AccessibilityDenied => Some(
            "grant Accessibility permission to the gateway (System Settings → Privacy & Security \
             → Accessibility), then retry",
        ),
        ComputerError::UnsupportedPlatform(_) => Some(
            "this adapter cannot perform the action — target screen coordinates instead of an \
             element, or use another interface",
        ),
        ComputerError::NoDisplay => {
            Some("no display is available; desktop actions cannot run until one is")
        }
        ComputerError::BlankScreen(_) => Some(
            "the screen is blank — nobody is logged in to the remote desktop, or the session is \
             locked. A desktop has to be open for actions to land anywhere",
        ),
        ComputerError::BackgroundInputUnavailable(_) => Some(
            "this action cannot be delivered without the foreground — target an element the \
             accessibility tree can act on, use the page rung, or ask for foreground input",
        ),
        ComputerError::NotInitialized => {
            Some("the computer adapter is not initialized; configure desktop access first")
        }
        // The step ran out of the time the caller gave it. Retrying the same
        // step with the same ceiling will hit the same wall; the fix is either
        // more budget or a smaller step (a narrower tree read, an element id
        // instead of a screen-wide settle).
        ComputerError::BudgetExhausted(_) => Some(
            "the step spent its time budget before this action could be delivered — raise the \
             step budget, or narrow the step (target an element id, read the tree shallower)",
        ),
        _ => None,
    }
}

/// Combine what the screen showed with what the driver established.
///
/// The driver's `Confirmed` survives only when nothing contradicted it — a
/// failed diff means the action was delivered but had no visible effect, which
/// is `SuspectedNoop`, not `Confirmed`. And "the platform accepted the call"
/// is not `Confirmed` at all: a driver that reached the element but could not
/// observe what happened there says `DeliveredUnverified`, and the effect
/// stays `Unverifiable`. Nothing in this function may promote an unobserved
/// effect to a confirmed one.
pub fn score_evidence(
    verification: StepVerification,
    driver: Option<&crate::computer::ActionEvidence>,
) -> crate::computer::ActionEvidence {
    let rung = driver.map(|d| d.rung).unwrap_or(Rung::Pixel);
    let tried = driver.map(|d| d.tried.clone()).unwrap_or_default();
    let driver_effect = driver.map(|d| d.effect);
    let driver_delivery = driver.map(|d| d.delivery).unwrap_or_default();

    let effect = match verification {
        StepVerification::Verified => ActionEffect::Confirmed,
        StepVerification::Failed => match driver_effect {
            // The driver knows something the pixels do not: the action was
            // delivered and partly took, or never ran at all.
            Some(ActionEffect::Partial) => ActionEffect::Partial,
            Some(ActionEffect::Refused) => ActionEffect::Refused,
            _ => ActionEffect::SuspectedNoop,
        },
        StepVerification::Unverifiable => match driver_effect {
            // A driver that claims `Confirmed` without having observed the
            // effect is reporting a successful *call*, not a result. If it
            // also admitted it could not verify (`DeliveredUnverified`), the
            // honest effect is `Unverifiable`; keeping its `Confirmed` here is
            // how an ax rung that silently did nothing reads as done.
            Some(ActionEffect::Confirmed)
                if driver_delivery == crate::computer::DeliveryDisposition::DeliveredUnverified =>
            {
                ActionEffect::Unverifiable
            }
            Some(ActionEffect::Confirmed) => ActionEffect::Confirmed,
            Some(ActionEffect::Partial) => ActionEffect::Partial,
            Some(ActionEffect::Refused) => ActionEffect::Refused,
            _ => ActionEffect::Unverifiable,
        },
    };

    // The delivery disposition follows the final effect: an effect that was
    // observed is a delivery that was verified, whatever the driver thought.
    let delivery = match effect {
        ActionEffect::Confirmed => crate::computer::DeliveryDisposition::DeliveredVerified,
        ActionEffect::Refused => crate::computer::DeliveryDisposition::NotDelivered,
        ActionEffect::SuspectedNoop | ActionEffect::Partial => {
            if driver_delivery == crate::computer::DeliveryDisposition::NotDelivered {
                crate::computer::DeliveryDisposition::NotDelivered
            } else {
                crate::computer::DeliveryDisposition::DeliveredUnverified
            }
        }
        ActionEffect::Unverifiable => driver_delivery,
    };

    let escalation = match (effect, rung) {
        // Partial and Refused never escalate: one already changed the screen,
        // the other never ran; another rung either doubles or repeats.
        (ActionEffect::Confirmed | ActionEffect::Partial | ActionEffect::Refused, _) => None,
        (ActionEffect::SuspectedNoop, Rung::AxAction) => Some(Escalation::Px),
        (ActionEffect::SuspectedNoop, Rung::Pixel) => Some(Escalation::Foreground),
        (ActionEffect::SuspectedNoop, Rung::Keyboard) => Some(Escalation::Page),
        // The page is the last rung there is.
        (ActionEffect::SuspectedNoop, Rung::Page) => None,
        // A pixel click we could not check may simply have gone to a window
        // that was not frontmost.
        (ActionEffect::Unverifiable, Rung::Pixel) => Some(Escalation::Foreground),
        (ActionEffect::Unverifiable, _) => None,
    };

    crate::computer::ActionEvidence {
        rung,
        effect,
        delivery,
        retry: delivery.retry(),
        escalation,
        tried,
        recovery: driver.and_then(|d| d.recovery.clone()),
        occluder: driver.and_then(|d| d.occluder.clone()),
    }
}

/// Render pinned notes for tool output. Bounded so a long task cannot grow
/// the prompt without limit.
pub fn render_notes(notes: &[PinnedNote]) -> String {
    const MAX_NOTES: usize = 30;
    const MAX_VALUE_CHARS: usize = 400;

    if notes.is_empty() {
        return String::new();
    }
    let mut out = String::from("Pinned notes (kept across steps):\n");
    for note in notes.iter().take(MAX_NOTES) {
        let value = if note.value.chars().count() > MAX_VALUE_CHARS {
            let truncated: String = note.value.chars().take(MAX_VALUE_CHARS).collect();
            format!("{truncated}…")
        } else {
            note.value.clone()
        };
        out.push_str(&format!("- {} = {}\n", note.key, value));
    }
    if notes.len() > MAX_NOTES {
        out.push_str(&format!("… and {} more\n", notes.len() - MAX_NOTES));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::headless::HeadlessComputerAdapter;
    use crate::computer::types::{MouseButton, Rect, Tri, WaitCondition};
    use crate::computer::ActionEvidence;

    fn headless_session() -> ComputerSession {
        ComputerSession::new(Arc::new(HeadlessComputerAdapter::new()))
    }

    /// An adapter that refuses every element-targeted click as stale, the way
    /// a real one does after its snapshot has been evicted.
    struct StaleAdapter;

    #[async_trait::async_trait]
    impl ComputerAdapter for StaleAdapter {
        async fn screenshot(&self, _region: Option<Rect>) -> crate::computer::Result<Screenshot> {
            Ok(Screenshot::new(String::new(), 100, 100))
        }

        async fn read_ui_tree(
            &self,
            _app: Option<&str>,
        ) -> crate::computer::Result<Vec<UiElement>> {
            Ok(Vec::new())
        }

        async fn execute(&self, action: DesktopAction) -> crate::computer::Result<ActionResult> {
            match action {
                DesktopAction::Click { target, .. } | DesktopAction::DoubleClick { target, .. } => {
                    Err(ComputerError::StaleElement(format!("{target:?}")))
                }
                _ => Ok(ActionResult::success("ok")),
            }
        }

        async fn wait_for(
            &self,
            _condition: WaitCondition,
            _timeout: Duration,
        ) -> crate::computer::Result<bool> {
            Ok(true)
        }

        async fn display_geometry(
            &self,
        ) -> crate::computer::Result<crate::computer::DisplayGeometry> {
            Ok(crate::computer::DisplayGeometry::new(vec![crate::computer::DisplayInfo::new(
                0, 0, 100, 100,
            )
            .primary()]))
        }
    }

    /// An adapter that resolves every element target to one element of a known
    /// role, records what reached it, and can pretend the accessibility
    /// set-value rung is unsupported.
    struct LadderAdapter {
        role: String,
        set_value_works: bool,
        /// What the element says about accepting input. `Known(true)` unless a
        /// test is about the disabled pre-check.
        enabled: Tri,
        /// How long every call takes. Zero unless a test is about the budget:
        /// a real desktop call is not instantaneous, and a budget test needs
        /// calls that can outlast it.
        delay: Duration,
        log: tokio::sync::Mutex<Vec<String>>,
    }

    impl LadderAdapter {
        fn new(role: &str, set_value_works: bool) -> Self {
            Self {
                role: role.to_string(),
                set_value_works,
                enabled: Tri::Known(true),
                delay: Duration::ZERO,
                log: tokio::sync::Mutex::new(Vec::new()),
            }
        }

        /// Make every call take `ms` milliseconds, as a slow desktop would.
        fn slow(mut self, ms: u64) -> Self {
            self.delay = Duration::from_millis(ms);
            self
        }

        async fn pause(&self) {
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
        }

        fn disabled(mut self) -> Self {
            self.enabled = Tri::Known(false);
            self
        }

        /// The element did not publish its state: not evidence of anything.
        fn state_unknown(mut self) -> Self {
            self.enabled = Tri::Unknown;
            self
        }

        async fn recorded(&self) -> Vec<String> {
            self.log.lock().await.clone()
        }

        async fn record(&self, entry: impl Into<String>) {
            self.log.lock().await.push(entry.into());
        }
    }

    #[async_trait::async_trait]
    impl ComputerAdapter for LadderAdapter {
        async fn screenshot(&self, _region: Option<Rect>) -> crate::computer::Result<Screenshot> {
            self.pause().await;
            Ok(Screenshot::new(String::new(), 100, 100))
        }

        async fn read_ui_tree(
            &self,
            _app: Option<&str>,
        ) -> crate::computer::Result<Vec<UiElement>> {
            self.pause().await;
            Ok(Vec::new())
        }

        async fn resolve_target(
            &self,
            target: &ClickTarget,
        ) -> crate::computer::Result<ActionTarget> {
            let mut resolved = ActionTarget {
                point: Some(crate::computer::Point::new(10, 20)),
                element: Some(crate::computer::element_index::ResolvedElement {
                    id: "e1".to_string(),
                    role: self.role.clone(),
                    label: Some("Field".to_string()),
                    center: Some(crate::computer::Point::new(10, 20)),
                    bounds: Some(crate::computer::Rect::new(10, 20, 20, 10)),
                    path: vec![0, 0],
                    actions: None,
                    enabled: self.enabled,
                }),
                app: Some("Test".to_string()),
                snapshot: None,
            };
            if let ClickTarget::Coordinate(point) = target {
                resolved.point = Some(*point);
            }
            Ok(resolved)
        }

        async fn perform_ax_action(
            &self,
            _target: &ActionTarget,
            action: crate::computer::AxAction,
        ) -> crate::computer::Result<ActionResult> {
            self.pause().await;
            let confirmed =
                || crate::computer::ActionEvidence::new(Rung::AxAction, ActionEffect::Confirmed);
            match action {
                crate::computer::AxAction::SetValue { value } => {
                    self.record(format!("set_value:{value}")).await;
                    if self.set_value_works {
                        Ok(ActionResult::success("value set").with_evidence(confirmed()))
                    } else {
                        Err(ComputerError::UnsupportedPlatform(
                            "this control has no settable value".to_string(),
                        ))
                    }
                }
                crate::computer::AxAction::Press => {
                    self.record("press").await;
                    Ok(ActionResult::success("pressed").with_evidence(confirmed()))
                }
                crate::computer::AxAction::Focus => {
                    self.record("focus").await;
                    Ok(ActionResult::success("focused").with_evidence(confirmed()))
                }
            }
        }

        async fn execute(&self, action: DesktopAction) -> crate::computer::Result<ActionResult> {
            self.pause().await;
            match action {
                DesktopAction::Click { target, .. } => {
                    self.record(format!("click:{target:?}")).await;
                    Ok(ActionResult::success("clicked"))
                }
                DesktopAction::Type { text, .. } => {
                    self.record(format!("type:{text}")).await;
                    Ok(ActionResult::success("typed"))
                }
                DesktopAction::KeyPress { keys } => {
                    self.record(format!("key:{}", keys.join("+"))).await;
                    Ok(ActionResult::success("pressed"))
                }
                // A wait really spends its time, so a test can see that the
                // step capped it.
                DesktopAction::Wait { milliseconds } => {
                    self.record(format!("wait:{milliseconds}")).await;
                    tokio::time::sleep(Duration::from_millis(milliseconds)).await;
                    Ok(ActionResult::success("waited"))
                }
                _ => Ok(ActionResult::success("ok")),
            }
        }

        async fn wait_for(
            &self,
            _condition: WaitCondition,
            _timeout: Duration,
        ) -> crate::computer::Result<bool> {
            Ok(true)
        }

        async fn display_geometry(
            &self,
        ) -> crate::computer::Result<crate::computer::DisplayGeometry> {
            Ok(crate::computer::DisplayGeometry::new(vec![crate::computer::DisplayInfo::new(
                0, 0, 100, 100,
            )
            .primary()]))
        }
    }

    /// A session whose settling does no waiting, so ladder tests stay fast.
    fn ladder_session(adapter: &Arc<LadderAdapter>) -> ComputerSession {
        ComputerSession::new(adapter.clone()).with_config(SessionConfig {
            settle: SettleConfig {
                initial_delay_ms: 0,
                poll_interval_ms: 0,
                max_rounds: 1,
                stable_threshold: 0.5,
                min_window_ms: 0,
            },
            ..SessionConfig::default()
        })
    }

    /// The step the read-back tests type through: a `type` with no target, so
    /// the tree-less branch has to fall back to the counting rule. Tests that
    /// want a target build their own.
    #[cfg(feature = "vision")]
    fn type_step() -> DesktopAction {
        DesktopAction::Type {
            text: String::new(),
            target: None,
        }
    }

    fn field(role: &str, value: &str) -> UiElement {
        UiElement {
            id: String::new(),
            role: role.to_string(),
            label: None,
            value: Some(value.to_string()),
            bounds: Some(Rect::new(0, 0, 100, 20)),
            enabled: crate::computer::Tri::Known(true),
            focused: crate::computer::Tri::Unknown,
            actions: None,
            children: Vec::new(),
            children_count: None,
            truncated: false,
        }
    }

    #[tokio::test]
    async fn a_check_that_cannot_run_establishes_nothing() {
        // The bug this replaces: the old diff check answered `true` when it had
        // no screenshots to compare, so "could not tell" and "it worked" were
        // the same answer.
        let session = headless_session();
        let (outcome, diff) = session
            .verify_by_diff(&DesktopAction::Screenshot { region: None }, None, None, None, None)
            .await
            .unwrap();
        assert_eq!(outcome, StepVerification::Unverifiable);
        assert!(diff.is_none());
        assert!(!outcome.verified(), "unverifiable is not verified");

        // An empty screenshot is the same situation: nothing to compare.
        let empty = Screenshot::new(String::new(), 0, 0);
        let (outcome, _) = session
            .verify_by_diff(
                &DesktopAction::Screenshot { region: None },
                None,
                Some(&empty),
                Some(&empty),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, StepVerification::Unverifiable);
    }

    #[tokio::test]
    async fn an_action_with_no_check_is_unverifiable_not_successful() {
        // `_ => Ok(true)` was a claim of success that nothing had established.
        let session = headless_session();
        let (outcome, _, _) = session
            .verify_step(
                &DesktopAction::GetSystemStatus,
                &ActionResult::success(""),
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, StepVerification::Unverifiable);
    }

    #[tokio::test]
    async fn an_unverifiable_step_moves_no_counter() {
        let session = headless_session();
        session
            .account(
                &DesktopAction::GetSystemStatus,
                &ActionResult::success("ok"),
                StepVerification::Unverifiable,
                ActionEffect::Unverifiable,
            )
            .await;
        assert_eq!(session.consecutive_failures().await, 0);
        assert_eq!(session.last_verification().await, Some(StepVerification::Unverifiable));
    }

    #[tokio::test]
    async fn failures_accumulate_and_verified_resets() {
        let session = headless_session();
        for _ in 0..2 {
            session
                .account(
                    &DesktopAction::Type { text: "x".into(), target: None },
                    &ActionResult::error("boo"),
                    StepVerification::Failed,
                    ActionEffect::SuspectedNoop,
                )
                .await;
        }
        assert_eq!(session.consecutive_failures().await, 2);

        session
            .account(
                &DesktopAction::Type { text: "x".into(), target: None },
                &ActionResult::success("ok"),
                StepVerification::Verified,
                ActionEffect::Confirmed,
            )
            .await;
        assert_eq!(session.consecutive_failures().await, 0);
    }

    #[test]
    fn a_failed_ax_action_escalates_to_pixels() {
        let driver = crate::computer::ActionEvidence::new(Rung::AxAction, ActionEffect::Confirmed);
        let evidence = score_evidence(StepVerification::Failed, Some(&driver));
        assert_eq!(evidence.effect, ActionEffect::SuspectedNoop);
        assert_eq!(evidence.escalation, Some(Escalation::Px));
    }

    #[test]
    fn an_unchecked_ax_action_still_counts_as_confirmed() {
        // The AX action reported no error; nothing contradicted it.
        let driver = crate::computer::ActionEvidence::new(Rung::AxAction, ActionEffect::Confirmed);
        let evidence = score_evidence(StepVerification::Unverifiable, Some(&driver));
        assert_eq!(evidence.effect, ActionEffect::Confirmed);
        assert_eq!(evidence.escalation, None);
    }

    #[test]
    fn an_unchecked_pixel_click_offers_foreground() {
        let evidence = score_evidence(StepVerification::Unverifiable, None);
        assert_eq!(evidence.effect, ActionEffect::Unverifiable);
        assert_eq!(evidence.escalation, Some(Escalation::Foreground));
    }

    #[test]
    fn a_verified_action_needs_no_escalation() {
        let evidence = score_evidence(
            StepVerification::Verified,
            Some(&crate::computer::ActionEvidence::new(Rung::Pixel, ActionEffect::Unverifiable)),
        );
        assert_eq!(evidence.effect, ActionEffect::Confirmed);
        assert!(evidence.escalation.is_none());
    }

    #[tokio::test]
    async fn notes_replace_by_key_and_survive_reads() {
        let session = headless_session();
        session.note("invoice_id", "INV-1").await;
        session.note("invoice_id", "INV-2").await;
        session.note("total", "42.00").await;
        let notes = session.notes().await;
        assert_eq!(notes.len(), 2);
        assert!(notes
            .iter()
            .any(|n| n.key == "invoice_id" && n.value == "INV-2"));
        assert!(render_notes(&notes).contains("total = 42.00"));
    }

    #[tokio::test]
    async fn reset_clears_counters_and_notes() {
        let session = headless_session();
        session.note("k", "v").await;
        session
            .account(
                &DesktopAction::Type { text: "x".into(), target: None },
                &ActionResult::error("boo"),
                StepVerification::Failed,
                ActionEffect::SuspectedNoop,
            )
            .await;
        session.reset().await;
        assert_eq!(session.consecutive_failures().await, 0);
        assert!(session.notes().await.is_empty());
    }

    // ── Partial / refused ─────────────────────────────────────────────────

    #[test]
    fn typing_that_fully_lands_is_confirmed_by_the_field() {
        let before = vec![field("AXTextField", "")];
        let after = vec![field("AXTextField", "hello")];
        let hint = typed_value_hint(&before, &after, "hello").expect("the field answers");
        assert_eq!(hint.effect, ActionEffect::Confirmed);
        assert_eq!(hint.verification, StepVerification::Verified);
        assert!(hint.recovery.is_none());
    }

    #[test]
    fn typing_that_half_lands_is_partial_not_a_noop() {
        // The screen changed, so a pixel diff would call this verified — but
        // the field holds three of the five characters.
        let before = vec![field("AXTextField", "")];
        let after = vec![field("AXTextField", "hel")];
        let hint = typed_value_hint(&before, &after, "hello").expect("the field answers");
        assert_eq!(hint.effect, ActionEffect::Partial);
        assert_eq!(hint.verification, StepVerification::Failed);
        let recovery = hint.recovery.expect("partial delivery comes with a hint");
        assert!(recovery.contains("select all"), "{recovery}");
        assert!(!recovery.contains("hel"), "the hint must not quote the text");
    }

    #[test]
    fn masked_and_ambiguous_fields_fall_back_to_pixels() {
        // A secure field reports bullets, not the text.
        let masked_before = vec![field("AXSecureTextField", "")];
        let masked_after = vec![field("AXSecureTextField", "•••••")];
        assert!(typed_value_hint(&masked_before, &masked_after, "hello").is_none());

        // Two fields moved: the text cannot be attributed to one of them.
        let ambiguous_before = vec![field("AXTextField", ""), field("AXTextField", "")];
        let ambiguous_after = vec![field("AXTextField", "he"), field("AXTextField", "llo")];
        assert!(typed_value_hint(&ambiguous_before, &ambiguous_after, "hello").is_none());

        // Nothing changed at all.
        let same = vec![field("AXTextField", "abc")];
        assert!(typed_value_hint(&same, &same, "hello").is_none());
    }

    // ── Reading a typed value back off the screen ─────────────────────────
    //
    // `typed_value_hint` answers when the tree publishes the field's value.
    // These cover the other case: the tree has the field but not its value, so
    // the frame the step captured is read instead — and only the verdict
    // travels on.

    #[cfg(feature = "vision")]
    fn read_back(text: &str, confidence: f32) -> crate::computer::vision::ocr_file::OcrText {
        crate::computer::vision::ocr_file::OcrText {
            text: text.to_string(),
            mean_confidence: confidence,
            blocks: 1,
        }
    }

    /// The ordinary case: the field reads back as what was typed.
    #[cfg(feature = "vision")]
    #[test]
    fn a_field_that_reads_back_as_the_text_is_confirmed() {
        let hint = judge_read_back("hello", &read_back("hello", 0.95)).expect("an answer");
        assert_eq!(hint.effect, ActionEffect::Confirmed);
        assert_eq!(hint.verification, StepVerification::Verified);
        assert!(hint.recovery.is_none());
    }

    /// The leak-proofing is structural, not a promise.
    ///
    /// A field that reads back as something else produces a verdict with no
    /// way to carry what was read: `EffectHint` has an effect, a verification
    /// and a recovery sentence, and that is all. The test asserts the reading
    /// is absent from the only text the hint does carry, and that the hint is a
    /// fixed sender-chosen sentence — so a future field added to `EffectHint`
    /// has to break this test to leak.
    #[cfg(feature = "vision")]
    #[test]
    fn what_the_field_read_back_is_never_carried_in_the_verdict() {
        let secret = "correct horse battery staple";
        let hint = judge_read_back("wrong", &read_back(secret, 0.98)).expect("an answer");
        assert_eq!(hint.effect, ActionEffect::Partial);
        assert_eq!(hint.verification, StepVerification::Failed);

        let carried = format!("{hint:?}");
        assert!(!carried.contains(secret), "the reading leaked into the verdict: {carried}");
        assert!(!carried.contains("horse"), "the reading leaked into the verdict: {carried}");
        assert_eq!(
            hint.recovery.as_deref(),
            Some(
                "the field does not read back as the text that was typed — click it, select all, \
                 and retype; the reading is optical and is not proof the text is missing, so \
                 check the field before typing again"
            ),
        );
    }

    /// A reading is not allowed to be evidence of an empty field.
    ///
    /// A font the recognizer cannot parse reads as nothing, and so does a field
    /// whose text scrolled out of view. Neither is "the typing failed", so both
    /// leave the answer to the pixel diff.
    #[cfg(feature = "vision")]
    #[test]
    fn an_empty_or_uncertain_reading_is_not_a_verdict() {
        assert!(judge_read_back("hello", &read_back("", 0.0)).is_none());
        assert!(judge_read_back("hello", &read_back("   \n ", 0.9)).is_none());
        // Confident enough to say *something* was there, not confident enough
        // to say it was not the typed text.
        assert!(judge_read_back("hello", &read_back("he11o", 0.4)).is_none());
        // The typed text is empty: there is nothing to look for.
        assert!(judge_read_back("", &read_back("hello", 0.99)).is_none());
    }

    /// Whitespace is not a character the reader can see.
    #[cfg(feature = "vision")]
    #[test]
    fn a_line_break_from_the_reader_is_not_a_typo() {
        // OCR returns one block per line, so a long value arrives with a
        // newline the field does not contain.
        let hint = judge_read_back("hello world", &read_back("hello\nworld", 0.9));
        assert_eq!(hint.expect("an answer").effect, ActionEffect::Confirmed);
        // And a typed space the reader dropped.
        let hint = judge_read_back("hello world", &read_back("helloworld", 0.9));
        assert_eq!(hint.expect("an answer").effect, ActionEffect::Confirmed);
    }

    /// A field the tree calls private is never cropped.
    ///
    /// Two separate reasons, tested separately because either one alone would
    /// be enough — the role says password, and the mask says bullets. Either
    /// makes the field unreadable to this feature.
    #[cfg(feature = "vision")]
    #[test]
    fn a_field_that_masks_what_it_holds_has_no_box_to_read() {
        assert!(masks(&field("AXSecureTextField", "")));
        assert!(masks(&field("password_field", "")));
        assert!(masks(&field("AXTextField", "•••••")));
        assert!(!masks(&field("AXTextField", "hello")));
        // A field that does not publish a value is not thereby a secret one;
        // it is the field this feature exists for.
        let mut empty = field("AXTextField", "");
        empty.value = None;
        assert!(!masks(&empty));
    }

    /// The tree's own sensitivity scan is asked before any pixels are taken.
    #[cfg(feature = "vision")]
    #[test]
    fn a_field_the_tree_marks_sensitive_is_not_read() {
        let mut private = field("AXTextField", "");
        private.label = Some("Password".to_string());
        assert!(
            !crate::computer::sensitive_ui::scan_sensitive(std::slice::from_ref(&private))
                .is_empty(),
            "the fixture has to be found by the scan this feature uses"
        );
        // A role that names no secret and carries no secret-looking label is
        // readable; the point is that the answer comes from the scan, not from
        // a role list maintained twice.
        let plain = field("AXTextField", "");
        assert!(
            crate::computer::sensitive_ui::scan_sensitive(std::slice::from_ref(&plain)).is_empty()
        );
    }

    /// Finding the field depends on it being the *only* editable one.
    ///
    /// With two fields there is no box that can be attributed to the typing,
    /// which is the same rule `typed_value_hint` applies for the same reason.
    #[cfg(feature = "vision")]
    #[test]
    fn only_one_field_can_be_read_back() {
        let one = vec![field("AXTextField", "")];
        assert_eq!(editable_fields(&one).len(), 1);

        let two = vec![field("AXTextField", ""), field("AXTextField", "")];
        assert_eq!(editable_fields(&two).len(), 2);

        let none = vec![field("AXButton", "")];
        assert!(editable_fields(&none).is_empty());

        // Nested fields count: a tree is a tree.
        let mut window = field("AXWindow", "");
        window.children.push(field("AXTextField", ""));
        assert_eq!(editable_fields(&[window]).len(), 1);
    }

    /// Read-back is off when the configuration says the tree is the only input.
    #[test]
    fn vision_off_turns_the_read_back_off_with_it() {
        use crate::config::VisionConfig;

        let off = VisionConfig {
            mode: "off".to_string(),
            ..Default::default()
        };
        assert_eq!(ReadBack::from_vision_config(&off), ReadBack::Off);

        let auto = VisionConfig {
            mode: "auto".to_string(),
            ..Default::default()
        };
        assert_eq!(ReadBack::from_vision_config(&auto), ReadBack::InstalledModels);

        let dir = VisionConfig {
            mode: "auto".to_string(),
            model_dir: "/models/vision".to_string(),
            ..Default::default()
        };
        assert_eq!(
            ReadBack::from_vision_config(&dir),
            ReadBack::ModelDir("/models/vision".to_string())
        );

        // A typo in the mode must not silently disable reading; it must not
        // silently enable it either — `auto` is what an unknown string means
        // everywhere else in the vision config.
        let typo = VisionConfig {
            mode: "lcoal".to_string(),
            model_dir: "/models/vision".to_string(),
            ..Default::default()
        };
        assert_eq!(
            ReadBack::from_vision_config(&typo),
            ReadBack::ModelDir("/models/vision".to_string())
        );
    }

    /// The step falls back to the pixel diff when read-back cannot answer.
    ///
    /// Every arm of [`ReadBack`] is exercised against an adapter with a frame
    /// and no models, and each has to end in `None` — "the screen could not
    /// answer" — rather than a refusal, a download or a hang. The verification
    /// that consumes it then falls through to the pixel diff exactly as it did
    /// before this feature existed.
    #[cfg(feature = "vision")]
    #[tokio::test]
    async fn a_typed_step_falls_back_to_pixels_however_read_back_is_configured() {
        let before = vec![field("AXTextField", "")];
        for read_back in [
            ReadBack::Off,
            // The default: no models under the configuration's directory.
            ReadBack::ModelDir("/nonexistent/syscity-read-back-test".to_string()),
        ] {
            let session = ComputerSession::new(Arc::new(HeadlessComputerAdapter::new()))
                .with_config(SessionConfig {
                    read_back: read_back.clone(),
                    ..Default::default()
                });
            let state =
                ScreenState::from_parts(before.clone(), Screenshot::new(String::new(), 100, 100));
            assert!(
                session
                    .read_back_typed("hello", &type_step(), Some(&before), None, &state)
                    .await
                    .is_none(),
                "{read_back:?} answered with no frame mapping to crop from"
            );
            // The tree-less branch has two of its own ways to refuse, and both
            // have to reach the same `None`: no frame before to compare
            // against, and a frame with nothing in it to see.
            assert!(
                session
                    .read_back_typed("hello", &type_step(), None, None, &state)
                    .await
                    .is_none(),
                "{read_back:?} answered with no baseline frame"
            );
        }

        // And the verification entry point agrees: an `Unverifiable` diff, not
        // a failure and not a hang.
        let session = ComputerSession::new(Arc::new(HeadlessComputerAdapter::new()));
        let (outcome, _, _) = session
            .verify_step(
                &DesktopAction::Type {
                    text: "hello".to_string(),
                    target: None,
                },
                &ActionResult::success("typed"),
                Some(&before),
                None,
                None,
                None,
            )
            .await
            .expect("a step still answers");
        assert_eq!(outcome, StepVerification::Unverifiable);
    }

    // ── The tree-less half of the read-back ───────────────────────────────
    //
    // On a screen that publishes no editable field there is no `editable_fields`
    // answer, so the box to crop has to come from somewhere else: the change
    // between the frame before the typing and the frame after it. These pin
    // which changes are allowed to name a field and which are not.

    #[cfg(feature = "vision")]
    fn change_in(regions: Vec<Rect>, truncated: bool) -> crate::computer::vision::PixelDiffResult {
        crate::computer::vision::PixelDiffResult {
            // The percentage is the diff's other answer and this function does
            // not read it; the regions are the whole input.
            change_percentage: 1.0,
            changed_regions: regions,
            regions_truncated: truncated,
        }
    }

    /// The ordinary case: one box changed, so that box is the field.
    ///
    /// The box is widened by a margin before it is returned, because a caret
    /// blinks *inside* the field and its own box is a few pixels wide — big
    /// enough to name the place, too small to read text out of.
    #[cfg(feature = "vision")]
    #[test]
    fn a_single_changed_box_names_the_field() {
        let before = Screenshot::new(String::new(), 800, 600);
        let after = Screenshot::new(String::new(), 800, 600);
        let crop = typed_box_from_change(
            &before,
            &after,
            &change_in(vec![Rect::new(300, 200, 10, 18)], false),
            None,
        )
        .expect("one changed box is an answer");
        // Widened by the margin on every side, and the box it covers contains
        // the change rather than the change's exact pixels.
        assert!(crop.contains(300, 200), "{crop:?} must cover the change");
        assert!(crop.contains(309, 217), "{crop:?} must cover the change");
        assert!(crop.width > 10, "{crop:?} is the caret, not the field");
        assert!(crop.height > 18, "{crop:?} is the caret, not the field");
        // Never outside the frame: a crop the decoder clamps anyway would make
        // the test above pass for the wrong reason.
        assert!(crop.x >= 0 && crop.y >= 0);
        assert!(crop.x as u32 + crop.width <= 800);
        assert!(crop.y as u32 + crop.height <= 600);
    }

    /// Two boxes changing at once cannot be attributed to this typing.
    ///
    /// Same rule the tree path applies to two editable fields: picking one of
    /// two is a guess with the verifier's name on it, and it would compare the
    /// other field's text against what was typed.
    #[cfg(feature = "vision")]
    #[test]
    fn two_changed_boxes_are_not_attributed_to_one_typing() {
        let shot = Screenshot::new(String::new(), 800, 600);
        assert!(typed_box_from_change(
            &shot,
            &shot,
            &change_in(vec![Rect::new(10, 10, 10, 18), Rect::new(400, 300, 12, 18)], false),
            None,
        )
        .is_none());
    }

    /// A change the size of the screen is the screen, not a field.
    ///
    /// Without the size ceiling, a page load or a window coming forward would
    /// become "the field", OCR would read the whole screen, and any typed text
    /// appearing anywhere in it would "confirm" the typing.
    #[cfg(feature = "vision")]
    #[test]
    fn a_change_covering_most_of_the_screen_is_not_a_field() {
        let shot = Screenshot::new(String::new(), 800, 600);
        assert!(typed_box_from_change(
            &shot,
            &shot,
            &change_in(vec![Rect::new(0, 0, 800, 600)], false),
            None
        )
        .is_none());
        // And a box that is large but still field-shaped gets through: the
        // ceiling is a sanity bound, not a second editor.
        assert!(typed_box_from_change(
            &shot,
            &shot,
            &change_in(vec![Rect::new(100, 100, 400, 100)], false),
            None
        )
        .is_some());
    }

    /// A truncated region list cannot say how many boxes changed.
    ///
    /// The cap keeps the eight largest regions, so a second changed field may
    /// simply be missing from the list. Counting what is there would turn a
    /// cut list into "exactly one box changed" — the one reading the rule
    /// above must not make.
    #[cfg(feature = "vision")]
    #[test]
    fn a_truncated_region_list_is_not_read_as_one_box() {
        let shot = Screenshot::new(String::new(), 800, 600);
        assert!(typed_box_from_change(
            &shot,
            &shot,
            &change_in(vec![Rect::new(300, 200, 10, 18)], true),
            None
        )
        .is_none());
    }

    /// Nothing changed, so there is nothing to lean in on.
    #[cfg(feature = "vision")]
    #[test]
    fn no_change_names_no_field() {
        let shot = Screenshot::new(String::new(), 800, 600);
        assert!(typed_box_from_change(&shot, &shot, &change_in(Vec::new(), false), None).is_none());
        // An unmeasured frame is not an unchanged one.
        let empty = Screenshot::new(String::new(), 0, 0);
        assert!(typed_box_from_change(
            &empty,
            &empty,
            &change_in(vec![Rect::new(1, 1, 10, 10)], false),
            None
        )
        .is_none());
    }

    /// The tree-less path is reached only when the tree names no field.
    ///
    /// The tree path is asked first and wins: it is the only one that can
    /// check the field's sensitivity before any pixels are taken, so a screen
    /// that publishes a password field must never fall through to the frame.
    /// Here the tree has the field and no models are installed, so the answer
    /// is `None` from the tree path — and the frame, which *did* change, must
    /// not be read instead.
    #[cfg(feature = "vision")]
    #[tokio::test]
    async fn a_tree_that_names_the_field_is_not_bypassed_by_the_frame() {
        use base64::Engine as _;
        fn shot(white: Option<Rect>) -> Screenshot {
            let mut img = image::RgbImage::new(200, 120);
            if let Some(r) = white {
                for y in r.y as u32..(r.y as u32 + r.height).min(120) {
                    for x in r.x as u32..(r.x as u32 + r.width).min(200) {
                        img.put_pixel(x, y, image::Rgb([255, 255, 255]));
                    }
                }
            }
            let mut buf = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut buf, image::ImageFormat::Png)
                .unwrap();
            Screenshot::new(
                base64::engine::general_purpose::STANDARD.encode(buf.into_inner()),
                200,
                120,
            )
        }
        let before = vec![field("AXTextField", "")];
        let session = ComputerSession::new(Arc::new(HeadlessComputerAdapter::new())).with_config(
            SessionConfig {
                read_back: ReadBack::ModelDir("/nonexistent/syscity-read-back-test".to_string()),
                ..Default::default()
            },
        );
        let changed = ScreenState::from_parts(Vec::new(), shot(Some(Rect::new(20, 20, 40, 16))));
        assert!(
            session
                .read_back_typed("hello", &type_step(), Some(&before), Some(&shot(None)), &changed)
                .await
                .is_none(),
            "the tree path answered, so the frame must not be consulted"
        );
    }

    /// The whole tree-less chain, against the models on this machine.
    ///
    /// The unit tests above pin the box arithmetic and the refusals; this one
    /// proves the chain they feed — no tree, two frames, a changed box, a
    /// crop, OCR, a verdict — actually produces a verdict, and that the
    /// verdict still carries no text. Without it, every piece could be right
    /// and the wiring between them wrong, which is the exact class of defect
    /// this feature exists to close.
    ///
    /// The frame is drawn at runtime from 5×7 glyph bitmaps, the same shape
    /// `ocr_file`'s own test uses: no font is available in a test and none
    /// should be. The glyphs are "HELO" — the recognizer's spelling of them is
    /// the model's business, so the assertion is about *agreement*, not a
    /// spelling: the same picture typed as either spelling must confirm, and a
    /// picture typed as something else must not.
    ///
    /// The skip is loud on stderr, like the rest of this suite's
    /// model-dependent tests: a machine without the optional models must see
    /// that this never ran.
    #[cfg(feature = "vision")]
    #[tokio::test]
    async fn a_tree_less_typing_is_read_back_from_the_changed_box() {
        use base64::Engine as _;
        use image::{Rgb, RgbImage};

        /// 5×7 bitmaps, one per row, most significant bit leftmost.
        const GLYPHS: &[[u8; 7]] = &[
            // H
            [
                0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
            ],
            // E
            [
                0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
            ],
            // L
            [
                0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
            ],
            // O
            [
                0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
            ],
        ];
        const SCALE: u32 = 12;

        /// A frame with the glyphs drawn into one band, or a blank one.
        fn frame(with_text: bool) -> Screenshot {
            let mut img = RgbImage::from_pixel(400, 200, Rgb([255, 255, 255]));
            if with_text {
                let (left, top) = (40, 60);
                for (index, glyph) in GLYPHS.iter().enumerate() {
                    let origin = left + index as u32 * (5 * SCALE + SCALE);
                    for (row, bits) in glyph.iter().enumerate() {
                        for column in 0..5u32 {
                            if bits & (1 << (4 - column)) == 0 {
                                continue;
                            }
                            for dy in 0..SCALE {
                                for dx in 0..SCALE {
                                    img.put_pixel(
                                        origin + column * SCALE + dx,
                                        top + row as u32 * SCALE + dy,
                                        Rgb([0, 0, 0]),
                                    );
                                }
                            }
                        }
                    }
                }
            }
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut png, image::ImageFormat::Png)
                .expect("encode the fixture");
            Screenshot::new(
                base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
                400,
                200,
            )
        }

        if crate::computer::vision::model_download::ocr_model_paths("").is_none() {
            eprintln!(
                "SKIPPED a_tree_less_typing_is_read_back_from_the_changed_box: no OCR models \
                 under {}",
                crate::computer::vision::model_download::vision_model_dir("").display()
            );
            return;
        }

        let session = ComputerSession::new(Arc::new(HeadlessComputerAdapter::new()));
        let blank = frame(false);
        let typed = frame(true);
        // No tree at all: this is the screen the branch exists for.
        let state = ScreenState::from_parts(Vec::new(), typed.clone());

        // First: what the differing region says, read the same way the
        // feature reads it. Two OCR runs over the *same* pixels are not
        // guaranteed to be identical readings — glyph fixtures are small and
        // the recognizer's margin is thin — so the assertion is on the path
        // and not on the recognizer's spelling of it. `HELU` once, `HELD` the
        // next: both are "it read the place that changed".
        let change = crate::computer::vision::pixel_diff(&blank, &typed);
        let crop = typed_box_from_change(&blank, &typed, &change, None).expect("one box changed");
        let read = crate::computer::vision::ocr_file::ocr_screenshot(&typed, Some(crop), "")
            .await
            .expect("the models are installed");
        assert!(
            !read.text.trim().is_empty() && read.is_reliable(),
            "the changed box holds nothing the reader can read: {read:?}"
        );
        let shown: String = read.text.chars().filter(|c| !c.is_whitespace()).collect();

        // The text the box shows is confirmed — and the box came from the
        // change between the two frames, not from a tree.
        let hint = session
            .read_back_typed(&shown, &type_step(), None, Some(&blank), &state)
            .await
            .expect("the changed box was read and agreed");
        assert_eq!(hint.effect, ActionEffect::Confirmed);
        assert_eq!(hint.verification, StepVerification::Verified);
        // The leak rule holds on this path too: the hint has no field that
        // could carry the reading.
        assert!(!format!("{hint:?}").contains(&shown), "the reading leaked");

        // A real typo is the case this feature exists for: text that is not
        // what the screen shows must not confirm. `zzzznotonscreen` shares no
        // run with any reading of these glyphs, so no recognizer's spelling
        // could turn it into a match.
        let wrong = "zzzznotonscreen";
        let hint = session
            .read_back_typed(wrong, &type_step(), None, Some(&blank), &state)
            .await
            .expect("the changed box was read");
        assert_eq!(hint.effect, ActionEffect::Partial);
        assert_eq!(hint.verification, StepVerification::Failed);
        assert!(!format!("{hint:?}").contains(&shown), "the reading leaked");

        // And with nothing changed between the frames there is no box, so no
        // reading is attempted — the step falls back to the diff as before.
        assert!(
            session
                .read_back_typed(
                    &shown,
                    &type_step(),
                    None,
                    Some(&blank),
                    &ScreenState::from_parts(Vec::new(), blank.clone())
                )
                .await
                .is_none(),
            "an unchanged screen names no field"
        );
    }

    /// A clock repaints every second; the field still has to be readable.
    ///
    /// This is the case that makes counting changed boxes insufficient on a
    /// real desktop. With a target, the box that overlaps it is this typing
    /// and the one that does not is the clock — and the crop is clipped to the
    /// target, so the clock's digits are not read into the comparison.
    #[cfg(feature = "vision")]
    #[test]
    fn a_change_away_from_the_target_is_not_this_typing() {
        let shot = Screenshot::new(String::new(), 800, 600);
        let change = change_in(vec![Rect::new(20, 8, 30, 14), Rect::new(300, 200, 120, 24)], false);
        // Aimed at the field: the field's box is the answer.
        let aimed =
            typed_box_from_change(&shot, &shot, &change, Some(Rect::new(280, 190, 160, 44)))
                .expect("the box at the target is this typing");
        assert!(aimed.contains(300, 200), "{aimed:?} must cover the field");
        assert!(!aimed.contains(25, 10), "{aimed:?} must not cover the clock");
        // Clipped to the target, not merely centred on it: the reading must
        // not reach a neighbour's text either, which the margin alone would
        // have allowed.
        assert!(aimed.x >= 280 && aimed.y >= 190, "{aimed:?} starts outside the target's box");
        assert!(
            aimed.x as u32 + aimed.width <= 440 && aimed.y as u32 + aimed.height <= 234,
            "{aimed:?} reaches outside the target's box"
        );
        // Aimed nowhere near either change: nothing can be attributed.
        assert!(typed_box_from_change(&shot, &shot, &change, Some(Rect::new(600, 400, 100, 40)))
            .is_none());
        // No target at all: two boxes, so no answer — the counting rule is
        // what the target exists to replace.
        assert!(typed_box_from_change(&shot, &shot, &change, None).is_none());
    }

    #[test]
    fn a_partial_driver_verdict_is_not_turned_into_a_noop() {
        let driver = crate::computer::ActionEvidence::new(Rung::Pixel, ActionEffect::Partial);
        let evidence = score_evidence(StepVerification::Failed, Some(&driver));
        assert_eq!(evidence.effect, ActionEffect::Partial);
        assert!(evidence.escalation.is_none(), "partial must not escalate");
        assert!(!evidence.effect.should_escalate());
        assert!(evidence.effect.needs_inspection());
    }

    #[test]
    fn a_refused_driver_verdict_never_offers_another_rung() {
        let driver = crate::computer::ActionEvidence::new(Rung::AxAction, ActionEffect::Refused);
        let evidence = score_evidence(StepVerification::Unverifiable, Some(&driver));
        assert_eq!(evidence.effect, ActionEffect::Refused);
        assert!(evidence.escalation.is_none());
    }

    #[tokio::test]
    async fn an_action_that_cannot_resolve_is_refused_with_a_hint() {
        let session = ComputerSession::new(Arc::new(StaleAdapter));
        let outcome = session
            .execute(DesktopAction::Click {
                target: ClickTarget::ElementId("s00000001:0.1".to_string()),
                button: MouseButton::Left,
            })
            .await
            .expect("a refusal is an outcome, not a crash");
        let evidence = outcome.evidence.expect("evidence");
        assert_eq!(evidence.effect, ActionEffect::Refused);
        let recovery = evidence.recovery.expect("refusals carry a recovery hint");
        assert!(recovery.contains("re-read the UI tree"), "{recovery}");
        // A refusal delivered nothing, so it is neither verified nor failed:
        // counting it as a failure is how three refusals in a row rolled back
        // work that was fine.
        assert_eq!(outcome.verification, StepVerification::Unverifiable);
        assert!(!outcome.rolled_back);
        assert!(!outcome.result.success);
        assert!(outcome.result.message.contains("refused"));
        assert!(outcome.result.message.contains(recovery.as_str()));
    }

    #[test]
    fn only_actionable_errors_become_refusals() {
        assert!(refusal_recovery(&ComputerError::StaleElement("x".into())).is_some());
        assert!(refusal_recovery(&ComputerError::AccessibilityDenied)
            .unwrap()
            .contains("Accessibility permission"));
        // A timeout may succeed on retry; calling it a refusal would tell the
        // caller to stop trying when it should try again.
        assert!(refusal_recovery(&ComputerError::Timeout).is_none());
        assert!(refusal_recovery(&ComputerError::ToolFailed("x".into())).is_none());
    }

    // ── Keyboard rung and the typing ladder ──────────────────────────────

    #[test]
    fn activation_keys_follow_the_control_role() {
        for role in [
            "button",
            "AXButton",
            "check box",
            "AXCheckBox",
            "radio button",
            "switch",
            "toggle",
            "tab",
            "tab item",
            "pop up button",
            "segmented control",
        ] {
            assert_eq!(activation_key(role), "space", "{role} answers to Space");
        }
        for role in ["link", "menu item", "row", "text field", "unknown"] {
            assert_eq!(activation_key(role), "enter", "{role} answers to Return");
        }
    }

    #[tokio::test]
    async fn the_keyboard_rung_presses_the_key_the_control_answers_to() {
        for (role, key) in [("check box", "space"), ("link", "enter")] {
            let adapter = Arc::new(LadderAdapter::new(role, true));
            let session = ladder_session(&adapter);
            session
                .dispatch(
                    &DesktopAction::Click {
                        target: ClickTarget::ElementId("e1".to_string()),
                        button: MouseButton::Left,
                    },
                    Rung::Keyboard,
                    &StepBudget::unlimited(),
                )
                .await
                .expect("keyboard rung");
            let expected = format!("key:{key}");
            assert_eq!(
                adapter.recorded().await.last().map(String::as_str),
                Some(expected.as_str())
            );
        }
    }

    #[tokio::test]
    async fn typing_pushes_the_value_through_accessibility_when_it_can() {
        let adapter = Arc::new(LadderAdapter::new("text field", true));
        let session = ladder_session(&adapter);
        let outcome = session
            .execute(DesktopAction::Type {
                text: "42".to_string(),
                target: Some(ClickTarget::ElementId("e1".to_string())),
            })
            .await
            .expect("typed");
        assert_eq!(
            adapter.recorded().await,
            vec!["set_value:42".to_string()],
            "no click and no keystroke when the value can be set directly"
        );
        assert_eq!(outcome.evidence.expect("evidence").rung, Rung::AxAction);
    }

    #[tokio::test]
    async fn typing_falls_back_to_focusing_the_field_then_typing() {
        let adapter = Arc::new(LadderAdapter::new("text field", false));
        let session = ladder_session(&adapter);
        let outcome = session
            .execute(DesktopAction::Type {
                text: "42".to_string(),
                target: Some(ClickTarget::ElementId("e1".to_string())),
            })
            .await
            .expect("typed");
        let log = adapter.recorded().await;
        assert!(
            log.iter().any(|entry| entry.starts_with("click:")),
            "the field is clicked before typing: {log:?}"
        );
        assert_eq!(log.last().map(String::as_str), Some("type:42"));
        assert_eq!(outcome.evidence.expect("evidence").rung, Rung::Pixel);
    }

    #[tokio::test]
    async fn typing_without_a_target_types_into_the_current_focus() {
        let adapter = Arc::new(LadderAdapter::new("text field", true));
        let session = ladder_session(&adapter);
        session
            .execute(DesktopAction::Type {
                text: "42".to_string(),
                target: None,
            })
            .await
            .expect("typed");
        assert_eq!(
            adapter.recorded().await,
            vec!["type:42".to_string()],
            "no target means no click and no set_value"
        );
    }

    // ── Page rung ────────────────────────────────────────────────────────

    /// A page that answers whatever it is asked, recording the question.
    struct FakePage {
        log: tokio::sync::Mutex<Vec<String>>,
    }

    impl FakePage {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                log: tokio::sync::Mutex::new(Vec::new()),
            })
        }

        async fn recorded(&self) -> Vec<String> {
            self.log.lock().await.clone()
        }
    }

    #[async_trait::async_trait]
    impl PageActor for FakePage {
        async fn act(
            &self,
            owner: &str,
            target: &PageTarget,
            action: PageAction,
        ) -> crate::computer::Result<ActionResult> {
            self.log
                .lock()
                .await
                .push(format!("{owner}:{}:{:?}:{action:?}", target.role, target.label));
            Ok(ActionResult::success("done")
                .with_evidence(ActionEvidence::new(Rung::Page, ActionEffect::Confirmed)))
        }
    }

    fn click_element(id: &str) -> DesktopAction {
        DesktopAction::Click {
            target: ClickTarget::ElementId(id.to_string()),
            button: MouseButton::Left,
        }
    }

    #[tokio::test]
    async fn the_page_rung_is_the_last_resort_for_a_named_element() {
        let adapter = Arc::new(LadderAdapter::new("button", true));
        let session = ladder_session(&adapter).with_page(FakePage::new(), "conv-1");
        assert_eq!(
            session.ladder_for(&click_element("e1")).await,
            vec![Rung::AxAction, Rung::Pixel, Rung::Keyboard, Rung::Page]
        );
    }

    #[tokio::test]
    async fn a_session_without_a_page_stops_at_the_keyboard_rung() {
        let adapter = Arc::new(LadderAdapter::new("button", true));
        let session = ladder_session(&adapter);
        let ladder = session.ladder_for(&click_element("e1")).await;
        assert!(!ladder.contains(&Rung::Page), "{ladder:?}");

        // Dispatching it anyway must refuse, not fall through to a blind
        // click: "no page" is not "the pixel rung again".
        let error = session
            .dispatch(&click_element("e1"), Rung::Page, &StepBudget::unlimited())
            .await
            .expect_err("no page is attached");
        assert!(matches!(error, ComputerError::UnsupportedPlatform(_)));
    }

    #[tokio::test]
    async fn a_coordinate_has_no_name_to_ask_the_page_about() {
        let adapter = Arc::new(LadderAdapter::new("button", true));
        let session = ladder_session(&adapter).with_page(FakePage::new(), "conv-1");
        let ladder = session
            .ladder_for(&DesktopAction::Click {
                target: ClickTarget::Coordinate(crate::computer::Point::new(5, 5)),
                button: MouseButton::Left,
            })
            .await;
        assert_eq!(ladder, vec![Rung::Pixel]);
    }

    /// A probe is not a click that lost its nerve.
    ///
    /// The default treatment for a named element starts at `AxAction`, whose
    /// "click" is `AXPress` — so a hover given that ladder would press the
    /// control it was only looking at, and the measurement would have caused
    /// the event it was trying to predict. Hover and its move-away get the
    /// pixel rung and nothing else, named element or not.
    #[tokio::test]
    async fn a_hover_only_ever_runs_the_pixel_rung() {
        let adapter = Arc::new(LadderAdapter::new("button", true));
        let session = ladder_session(&adapter).with_page(FakePage::new(), "conv-1");

        for action in [
            DesktopAction::Hover {
                target: ClickTarget::ElementId("e1".to_string()),
            },
            DesktopAction::Hover {
                target: ClickTarget::Coordinate(crate::computer::Point::new(5, 5)),
            },
            DesktopAction::PointerMove {
                to: crate::computer::Point::new(5, 5),
            },
        ] {
            let ladder = session.ladder_for(&action).await;
            assert_eq!(ladder, vec![Rung::Pixel], "{action:?} → {ladder:?}");
        }
    }

    /// A drag aims at the span between its ends, not at a point.
    ///
    /// This is what lets "the screen changed" be read as "the drag did what it
    /// was aimed at". A slider's thumb ends up somewhere between the grab and
    /// the release, and the whole track between the two endpoints repaints, so
    /// a box around either single point would exclude the change that proves
    /// it worked.
    ///
    /// The box is asserted as a *span*, not a pixel count: both endpoints plus
    /// their padding, and strictly wider than either end alone.
    #[tokio::test]
    async fn a_drag_aims_at_the_span_between_its_ends() {
        let session = headless_session();
        let shot = Screenshot::new(String::new(), 1440, 900);
        // The headless adapter reports no displays, so the mapping is stated
        // here rather than probed. Without it `aim_in_image` refuses rather
        // than guess, which is what the sibling test below pins.
        session
            .record_coordinate_space(CoordinateSpace::new(
                shot.width,
                shot.height,
                &crate::computer::DisplayGeometry::new(vec![crate::computer::DisplayInfo::new(
                    0,
                    0,
                    shot.width,
                    shot.height,
                )
                .primary()]),
            ))
            .await;
        let drag = session
            .aim_in_image(
                &DesktopAction::Drag {
                    from: ClickTarget::Coordinate(crate::computer::Point::new(100, 100)),
                    to: ClickTarget::Coordinate(crate::computer::Point::new(400, 100)),
                },
                &shot,
            )
            .await
            .expect("a span between two known points");
        assert_eq!(drag.y, 100 - 32, "the span keeps the row the drag stayed on");
        assert_eq!(drag.height, 64, "the span is one padded row tall");
        assert!(
            drag.width > 400 - 100,
            "the span must reach from the grab to the release: {drag:?}"
        );

        // A click at one of those same points gets a point box, and the span
        // is strictly wider — the drag box is not just "the first end".
        let click = session
            .aim_in_image(
                &DesktopAction::Click {
                    target: ClickTarget::Coordinate(crate::computer::Point::new(100, 100)),
                    button: MouseButton::Left,
                },
                &shot,
            )
            .await
            .expect("a point box");
        assert_eq!(click.width, 64);
        assert!(drag.width > click.width, "{drag:?} vs {click:?}");
    }

    /// Without a pixel-to-point mapping there is no aim, not a guessed one.
    ///
    /// The same refusal covers a drag: a span computed in the wrong space is
    /// worse than no span, because it would excuse a miss.
    #[tokio::test]
    async fn an_aim_without_a_mapping_is_refused() {
        let session = headless_session();
        let shot = Screenshot::new(String::new(), 1440, 900);
        assert!(session
            .aim_in_image(
                &DesktopAction::Drag {
                    from: ClickTarget::Coordinate(crate::computer::Point::new(100, 100)),
                    to: ClickTarget::Coordinate(crate::computer::Point::new(400, 100)),
                },
                &shot,
            )
            .await
            .is_none());
    }

    /// A scroll has no aim box, and must not be given a pretend one.
    ///
    /// Its content moves under the pointer across the whole pane. A box around
    /// the pointer would report a scroll that landed in the wrong pane as a
    /// miss, when the truth is that a scroll has no one place it can show.
    #[tokio::test]
    async fn a_scroll_has_no_aim_box() {
        let session = headless_session();
        let shot = Screenshot::new(String::new(), 1440, 900);
        assert!(session
            .aim_in_image(
                &DesktopAction::Scroll {
                    target: ClickTarget::Coordinate(crate::computer::Point::new(100, 100)),
                    direction: crate::computer::ScrollDirection::Down,
                    amount: 3,
                },
                &shot,
            )
            .await
            .is_none());
    }

    /// Hover establishes that the pointer arrived, and nothing more.
    ///
    /// The frame a hover was verified against is the frame the cursor was
    /// drawn into — the pointer *is* the change, so a pixel diff would report
    /// "the control reacted" for every target, live or not. The honest verdict
    /// is `Unverifiable`; the next step's picture is the evidence.
    #[tokio::test]
    async fn a_hover_is_unchecked_because_the_cursor_is_in_the_frame() {
        let session = headless_session();
        let (outcome, diff, hint) = session
            .verify_step(
                &DesktopAction::Hover {
                    target: ClickTarget::ElementId("e1".to_string()),
                },
                &ActionResult::success("the pointer is now over the target"),
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, StepVerification::Unverifiable);
        assert!(!outcome.verified());
        assert!(diff.is_none());
        assert!(hint.is_none());

        // The move-away has no such ambiguity: it either runs or errors.
        let (outcome, _, _) = session
            .verify_step(
                &DesktopAction::PointerMove {
                    to: crate::computer::Point::new(900, 600),
                },
                &ActionResult::success("moved"),
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, StepVerification::Verified);
    }

    /// The pointer is shared by the whole machine, so a probe is input.
    #[test]
    fn a_hover_takes_the_input_lock() {
        use crate::computer::types::DesktopAction as Action;
        assert!(Action::Hover {
            target: ClickTarget::Coordinate(crate::computer::Point::new(1, 1)),
        }
        .needs_input_lock());
        assert!(Action::PointerMove {
            to: crate::computer::Point::new(1, 1),
        }
        .needs_input_lock());
    }

    #[tokio::test]
    async fn the_page_rung_hands_the_page_a_name_not_a_coordinate() {
        let adapter = Arc::new(LadderAdapter::new("button", true));
        let page = FakePage::new();
        let session = ladder_session(&adapter).with_page(page.clone(), "conv-1");

        session
            .dispatch(&click_element("e1"), Rung::Page, &StepBudget::unlimited())
            .await
            .expect("page click");
        session
            .dispatch(
                &DesktopAction::Type {
                    text: "hello".to_string(),
                    target: Some(ClickTarget::ElementId("e1".to_string())),
                },
                Rung::Page,
                &StepBudget::unlimited(),
            )
            .await
            .expect("page type");

        assert_eq!(
            page.recorded().await,
            vec![
                "conv-1:button:Some(\"Field\"):Click".to_string(),
                "conv-1:button:Some(\"Field\"):Type(\"hello\")".to_string(),
            ],
            "the page is addressed by role and name from the UI tree"
        );
    }

    // ── Background delivery ─────────────────────────────────────────────

    /// An adapter whose element has no accessibility path (so the ax rung is
    /// unavailable) and that records which delivery path ran.
    struct BackgroundAdapter {
        supports: bool,
        log: tokio::sync::Mutex<Vec<String>>,
    }

    impl BackgroundAdapter {
        fn new(supports: bool) -> Arc<Self> {
            Arc::new(Self {
                supports,
                log: tokio::sync::Mutex::new(Vec::new()),
            })
        }

        async fn recorded(&self) -> Vec<String> {
            self.log.lock().await.clone()
        }

        async fn record(&self, entry: impl Into<String>) {
            self.log.lock().await.push(entry.into());
        }
    }

    #[async_trait::async_trait]
    impl ComputerAdapter for BackgroundAdapter {
        async fn screenshot(&self, _region: Option<Rect>) -> crate::computer::Result<Screenshot> {
            Ok(Screenshot::new(String::new(), 100, 100))
        }

        async fn read_ui_tree(
            &self,
            _app: Option<&str>,
        ) -> crate::computer::Result<Vec<UiElement>> {
            Ok(Vec::new())
        }

        async fn resolve_target(
            &self,
            target: &ClickTarget,
        ) -> crate::computer::Result<ActionTarget> {
            let mut resolved = ActionTarget {
                point: Some(crate::computer::Point::new(10, 20)),
                element: Some(crate::computer::element_index::ResolvedElement {
                    id: "e1".to_string(),
                    role: "AXButton".to_string(),
                    label: Some("Save".to_string()),
                    center: Some(crate::computer::Point::new(10, 20)),
                    bounds: Some(crate::computer::Rect::new(10, 20, 20, 10)),
                    // No path: the accessibility rung cannot run, which is
                    // the case background delivery has to answer for.
                    path: Vec::new(),
                    actions: None,
                    enabled: crate::computer::Tri::Known(true),
                }),
                app: Some("Test".to_string()),
                snapshot: None,
            };
            if let ClickTarget::Coordinate(point) = target {
                resolved.point = Some(*point);
            }
            Ok(resolved)
        }

        async fn execute(&self, action: DesktopAction) -> crate::computer::Result<ActionResult> {
            self.record(format!("foreground:{action:?}")).await;
            Ok(ActionResult::success("done in the foreground"))
        }

        fn supports_background_input(&self) -> bool {
            self.supports
        }

        async fn perform_background(
            &self,
            action: &DesktopAction,
            target: Option<&ActionTarget>,
        ) -> crate::computer::Result<ActionResult> {
            if !self.supports {
                return Err(ComputerError::BackgroundInputUnavailable(
                    "nothing can reach this app without the foreground".to_string(),
                ));
            }
            let app = target.and_then(|t| t.app.clone()).unwrap_or_default();
            self.record(format!("background:{app}:{action:?}")).await;
            Ok(ActionResult::success("done in the background")
                .with_evidence(ActionEvidence::new(Rung::Pixel, ActionEffect::Confirmed)))
        }

        async fn wait_for(
            &self,
            _condition: WaitCondition,
            _timeout: Duration,
        ) -> crate::computer::Result<bool> {
            Ok(true)
        }
    }

    fn background_session(adapter: &Arc<BackgroundAdapter>) -> ComputerSession {
        ComputerSession::new(adapter.clone()).with_config(SessionConfig {
            settle: SettleConfig {
                initial_delay_ms: 0,
                poll_interval_ms: 0,
                max_rounds: 1,
                stable_threshold: 0.5,
                min_window_ms: 0,
            },
            delivery: DeliveryMode::Background,
            ..SessionConfig::default()
        })
    }

    #[tokio::test]
    async fn background_delivery_goes_through_the_background_path() {
        let adapter = BackgroundAdapter::new(true);
        let session = background_session(&adapter);
        let outcome = session
            .execute(click_element("e1"))
            .await
            .expect("delivered");

        assert_eq!(
            adapter.recorded().await,
            vec![format!("background:Test:{:?}", click_element("e1"))],
            "no foreground click and no keystroke"
        );
        assert_eq!(outcome.result.message, "done in the background");
    }

    #[tokio::test]
    async fn background_delivery_refuses_instead_of_taking_the_foreground() {
        // The failure this guards against: asking for background delivery and
        // getting a foreground click anyway, because the ladder "escalated".
        let adapter = BackgroundAdapter::new(false);
        let session = background_session(&adapter);
        let outcome = session
            .execute(click_element("e1"))
            .await
            .expect("a refusal is an outcome, not a crash");

        assert!(adapter.recorded().await.is_empty(), "nothing was delivered");
        let evidence = outcome.evidence.expect("evidence");
        assert_eq!(evidence.effect, ActionEffect::Refused);
        assert!(evidence.escalation.is_none(), "a refusal must not escalate");
        let recovery = evidence.recovery.expect("refusal carries a hint");
        assert!(recovery.contains("foreground"), "{recovery}");
        assert!(!outcome.result.success);
    }

    #[tokio::test]
    async fn foreground_delivery_is_unchanged_when_it_is_asked_for() {
        let adapter = BackgroundAdapter::new(true);
        let session = ComputerSession::new(adapter.clone()).with_config(SessionConfig {
            settle: SettleConfig {
                initial_delay_ms: 0,
                poll_interval_ms: 0,
                max_rounds: 1,
                stable_threshold: 0.5,
                min_window_ms: 0,
            },
            ..SessionConfig::default()
        });
        session
            .execute(click_element("e1"))
            .await
            .expect("delivered");
        let log = adapter.recorded().await;
        assert!(log.iter().any(|entry| entry.starts_with("foreground:")), "{log:?}");
        assert!(!log.iter().any(|entry| entry.starts_with("background:")), "{log:?}");
    }

    // ── The disabled pre-check (§2) ──────────────────────────────────────

    /// A control that said it is disabled is refused before any rung runs.
    #[tokio::test]
    async fn a_disabled_control_is_refused_not_clicked() {
        let adapter = Arc::new(LadderAdapter::new("AXButton", true).disabled());
        let session = ladder_session(&adapter);
        let outcome = session
            .execute(click_element("e1"))
            .await
            .expect("a refusal is an outcome");

        assert!(adapter.recorded().await.is_empty(), "nothing was dispatched");
        let evidence = outcome.evidence.expect("evidence");
        assert_eq!(evidence.effect, ActionEffect::Refused);
        assert_eq!(evidence.delivery, crate::computer::DeliveryDisposition::NotDelivered);
        assert!(evidence.escalation.is_none());
        let message = outcome.result.message;
        assert!(message.contains("disabled"), "{message}");
    }

    /// A refusal is not a verification failure: three in a row must not
    /// trigger the auto-rollback that counts failures.
    #[tokio::test]
    async fn repeated_refusals_do_not_count_as_failures() {
        let adapter = Arc::new(LadderAdapter::new("AXButton", true).disabled());
        let session = ladder_session(&adapter);
        for _ in 0..4 {
            let outcome = session.execute(click_element("e1")).await.expect("refusal");
            assert_eq!(outcome.verification, StepVerification::Unverifiable);
            assert!(!outcome.rolled_back, "a refusal must not roll back work");
        }
    }

    /// An unpublished state is not a reason to refuse: refusing would skip a
    /// control that is perfectly clickable.
    #[tokio::test]
    async fn an_unpublished_state_does_not_block_the_action() {
        let adapter = Arc::new(LadderAdapter::new("AXButton", true).state_unknown());
        let session = ladder_session(&adapter);
        session
            .execute(click_element("e1"))
            .await
            .expect("the action proceeds");
        let log = adapter.recorded().await;
        assert!(!log.is_empty(), "the ladder ran: {log:?}");
    }

    /// A coordinate target has no element state to consult.
    #[tokio::test]
    async fn a_coordinate_click_is_not_pre_checked() {
        let adapter = Arc::new(LadderAdapter::new("AXButton", true).disabled());
        let session = ladder_session(&adapter);
        session
            .execute(DesktopAction::Click {
                target: ClickTarget::Coordinate(crate::computer::Point::new(5, 5)),
                button: crate::computer::MouseButton::Left,
            })
            .await
            .expect("coordinates are delivered as given");
        let log = adapter.recorded().await;
        assert!(!log.is_empty(), "{log:?}");
    }

    // ── The step budget (§5.3) ───────────────────────────────────────────

    /// A step that runs out of time stops itself and says so, with a recovery
    /// hint, instead of being killed from above as "tool timed out".
    #[tokio::test]
    async fn a_spent_budget_is_a_refusal_with_the_budget_in_evidence() {
        let adapter = Arc::new(LadderAdapter::new("AXButton", true).slow(50));
        let session = ladder_session(&adapter);
        let outcome = session
            .execute_with_budget(
                click_element("e1"),
                Some(StepBudget::new(Duration::from_millis(20))),
            )
            .await
            .expect("a spent budget is an outcome, not a crash");

        assert!(!outcome.result.success, "nothing was confirmed");
        let evidence = outcome.evidence.expect("evidence travels with a refusal");
        assert_eq!(evidence.effect, ActionEffect::Refused);
        assert!(
            evidence
                .recovery
                .as_deref()
                .is_some_and(|r| r.contains("budget")),
            "the refusal names the ceiling: {:?}",
            evidence.recovery
        );
        assert!(
            adapter.recorded().await.is_empty(),
            "no rung was dispatched after the budget ran out"
        );
        let budget = outcome.budget.expect("the step reports its budget");
        assert_eq!(budget["total_ms"], 20);
        assert_eq!(budget["remaining_ms"], 0);
    }

    /// A budget generous enough for the work changes nothing: the action is
    /// delivered and verified exactly as it is without one.
    #[tokio::test]
    async fn a_generous_budget_leaves_the_step_unchanged() {
        let adapter = Arc::new(LadderAdapter::new("AXButton", true));
        let session = ladder_session(&adapter);
        let outcome = session
            .execute_with_budget(click_element("e1"), Some(StepBudget::new(Duration::from_secs(5))))
            .await
            .expect("delivered");

        assert!(outcome.result.success, "{}", outcome.result.message);
        assert!(!adapter.recorded().await.is_empty(), "the action ran");
        let budget = outcome.budget.expect("budget reported");
        assert_eq!(budget["total_ms"], 5000);
        assert!(budget["elapsed_ms"].as_u64().expect("elapsed") < 5000, "{budget}");
    }

    /// The configured ceiling works the same way, so a caller that sets it
    /// once on the session does not have to pass it per call.
    #[tokio::test]
    async fn a_session_configured_budget_applies_to_the_step() {
        let adapter = Arc::new(LadderAdapter::new("AXButton", true).slow(50));
        let session = ComputerSession::new(adapter.clone()).with_config(SessionConfig {
            settle: SettleConfig {
                initial_delay_ms: 0,
                poll_interval_ms: 0,
                max_rounds: 1,
                stable_threshold: 0.5,
                min_window_ms: 0,
            },
            step_budget: Some(Duration::from_millis(20)),
            ..SessionConfig::default()
        });
        let outcome = session
            .execute(click_element("e1"))
            .await
            .expect("an outcome");
        assert!(!outcome.result.success);
        assert!(adapter.recorded().await.is_empty());
        assert_eq!(
            outcome.budget.expect("budget")["total_ms"],
            20,
            "the configured ceiling is the one reported"
        );
    }

    /// `wait` is the one action whose content is time: asking for longer than
    /// the step has left sleeps what fits instead of overrunning the caller.
    #[tokio::test]
    async fn a_wait_longer_than_the_budget_is_capped() {
        let adapter = Arc::new(LadderAdapter::new("AXButton", true));
        let session = ladder_session(&adapter);
        let started = std::time::Instant::now();
        let outcome = session
            .execute_with_budget(
                DesktopAction::Wait { milliseconds: 5_000 },
                Some(StepBudget::new(Duration::from_millis(200))),
            )
            .await
            .expect("a capped wait is still a wait");
        assert!(outcome.result.success);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the wait was capped, not slept in full: {:?}",
            started.elapsed()
        );
        // The adapter is told a number it can actually sleep, not 5 s.
        let log = adapter.recorded().await;
        let waited: u64 = log
            .iter()
            .find_map(|entry| entry.strip_prefix("wait:"))
            .and_then(|ms| ms.parse().ok())
            .unwrap_or_else(|| panic!("a capped wait reached the adapter: {log:?}"));
        assert!(waited <= 200, "sleeps what fits: {waited}");
        let budget = outcome.budget.expect("budget");
        assert!(budget["remaining_ms"].as_u64().expect("remaining") <= 5, "{budget}");
    }

    /// An adapter whose screen stays on the baseline for a while and only
    /// then repaints, the way WebKitGTK does when a native popup is opened.
    ///
    /// Every capture before `paint_after` returns the baseline frame; from
    /// then on it returns a visibly different one.
    struct LatePaintingAdapter {
        screens: std::sync::Mutex<Vec<(std::time::Instant, Screenshot)>>,
        paint_after: Duration,
        started: std::time::Instant,
    }

    impl LatePaintingAdapter {
        fn new(paint_after: Duration) -> Self {
            Self {
                screens: std::sync::Mutex::new(Vec::new()),
                paint_after,
                started: std::time::Instant::now(),
            }
        }

        /// Frames in the order they were handed out, for asserting the loop
        /// kept looking rather than stopping at the first agreement.
        fn captures(&self) -> usize {
            self.screens.lock().expect("lock").len()
        }
    }

    #[async_trait::async_trait]
    impl ComputerAdapter for LatePaintingAdapter {
        async fn screenshot(&self, _region: Option<Rect>) -> crate::computer::Result<Screenshot> {
            // Distinct base64 per frame, so `pixel_diff` sees the change a
            // real repaint would produce.
            let painted = self.started.elapsed() >= self.paint_after;
            let frame = if painted {
                "after-the-paint"
            } else {
                "before-the-paint"
            };
            let shot = Screenshot::new(frame.to_string(), 100, 100);
            self.screens
                .lock()
                .expect("lock")
                .push((std::time::Instant::now(), shot.clone()));
            Ok(shot)
        }

        async fn read_ui_tree(
            &self,
            _app: Option<&str>,
        ) -> crate::computer::Result<Vec<UiElement>> {
            Ok(Vec::new())
        }

        async fn execute(&self, _action: DesktopAction) -> crate::computer::Result<ActionResult> {
            Ok(ActionResult::success("clicked"))
        }

        async fn wait_for(
            &self,
            _condition: WaitCondition,
            _timeout: Duration,
        ) -> crate::computer::Result<bool> {
            Ok(true)
        }

        async fn display_geometry(
            &self,
        ) -> crate::computer::Result<crate::computer::DisplayGeometry> {
            Ok(crate::computer::DisplayGeometry::new(vec![crate::computer::DisplayInfo::new(
                0, 0, 100, 100,
            )
            .primary()]))
        }
    }

    /// A repaint that lands after the first couple of polls must still be
    /// seen. The bug this pins: the settle loop compared consecutive frames
    /// only, so two captures taken before the paint agreed with each other,
    /// the loop declared the screen settled, and the step reported "0.0%
    /// changed in pixels" — to the model, "your click did nothing".
    ///
    /// Measured on WebKitGTK in the bench image: a context menu that a
    /// right-click opens lands 205 ms after the click on a good run and
    /// 1.4 s on a slow one, against a window of 150 ms + 3 x 120 ms.
    #[tokio::test]
    async fn a_repaint_that_lands_after_the_first_polls_is_still_seen() {
        let adapter = Arc::new(LatePaintingAdapter::new(Duration::from_millis(400)));
        let session = ComputerSession::new(adapter.clone()).with_config(SessionConfig {
            settle: SettleConfig {
                initial_delay_ms: 50,
                poll_interval_ms: 50,
                max_rounds: 30,
                stable_threshold: 0.5,
                min_window_ms: 1_200,
            },
            ..SessionConfig::default()
        });

        let outcome = session
            .execute(DesktopAction::Click {
                target: ClickTarget::Coordinate(crate::computer::Point::new(10, 10)),
                button: MouseButton::Left,
            })
            .await
            .expect("an outcome");

        assert!(
            adapter.captures() > 3,
            "kept polling past the pre-paint agreements, saw {} captures",
            adapter.captures()
        );
        let diff = outcome.diff.expect("a diff");
        assert!(
            diff.has_meaningful_changes(),
            "the late repaint is the change: {}",
            diff.summary()
        );
        assert_eq!(
            outcome.verification,
            StepVerification::Verified,
            "a repaint is a working action: {}",
            diff.summary()
        );
    }

    /// The counterpart: an action that really does nothing must still be
    /// reported as nothing. The floor is a window, not a promise.
    #[tokio::test]
    async fn a_screen_that_never_repaints_is_still_reported_as_unchanged() {
        // A paint time longer than any window this test allows.
        let adapter = Arc::new(LatePaintingAdapter::new(Duration::from_secs(30)));
        let session = ComputerSession::new(adapter.clone()).with_config(SessionConfig {
            settle: SettleConfig {
                initial_delay_ms: 10,
                poll_interval_ms: 10,
                max_rounds: 40,
                stable_threshold: 0.5,
                min_window_ms: 120,
            },
            ..SessionConfig::default()
        });

        let started = std::time::Instant::now();
        let outcome = session
            .execute(DesktopAction::Click {
                target: ClickTarget::Coordinate(crate::computer::Point::new(10, 10)),
                button: MouseButton::Left,
            })
            .await
            .expect("an outcome");

        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the window ends the search instead of polling forever: {:?}",
            started.elapsed()
        );
        let diff = outcome.diff.expect("a diff");
        assert!(!diff.has_meaningful_changes(), "{}", diff.summary());
        assert_eq!(outcome.verification, StepVerification::Failed);
    }

    /// A desktop whose only change is somewhere the click never aimed.
    ///
    /// The baseline frame is blank; every frame after it has one repainted
    /// block at `block`. That is a clock ticking in the corner, a notification
    /// fading, a neighbouring window redrawing — the screen really did change,
    /// and none of it is evidence the click landed.
    #[cfg(feature = "vision")]
    struct CornerPaintingAdapter {
        captures: std::sync::Mutex<usize>,
        block: Rect,
    }

    #[cfg(feature = "vision")]
    impl CornerPaintingAdapter {
        fn new(block: Rect) -> Self {
            Self {
                captures: std::sync::Mutex::new(0),
                block,
            }
        }

        /// A 200x200 frame, blank on the first capture and painted afterwards.
        fn frame(&self) -> Screenshot {
            use base64::Engine as _;
            let mut count = self.captures.lock().expect("lock");
            let painted = *count > 0;
            *count += 1;
            let mut img = image::RgbImage::new(200, 200);
            if painted {
                for y in self.block.y as u32..(self.block.y as u32 + self.block.height) {
                    for x in self.block.x as u32..(self.block.x as u32 + self.block.width) {
                        img.put_pixel(x, y, image::Rgb([255, 255, 255]));
                    }
                }
            }
            let mut buf = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut buf, image::ImageFormat::Png)
                .expect("a PNG");
            Screenshot::new(
                base64::engine::general_purpose::STANDARD.encode(buf.into_inner()),
                200,
                200,
            )
        }
    }

    #[cfg(feature = "vision")]
    #[async_trait::async_trait]
    impl ComputerAdapter for CornerPaintingAdapter {
        async fn screenshot(&self, _region: Option<Rect>) -> crate::computer::Result<Screenshot> {
            Ok(self.frame())
        }

        async fn read_ui_tree(
            &self,
            _app: Option<&str>,
        ) -> crate::computer::Result<Vec<UiElement>> {
            Ok(Vec::new())
        }

        async fn execute(&self, _action: DesktopAction) -> crate::computer::Result<ActionResult> {
            Ok(ActionResult::success("clicked"))
        }

        async fn wait_for(
            &self,
            _condition: WaitCondition,
            _timeout: Duration,
        ) -> crate::computer::Result<bool> {
            Ok(true)
        }

        async fn display_geometry(
            &self,
        ) -> crate::computer::Result<crate::computer::DisplayGeometry> {
            Ok(crate::computer::DisplayGeometry::new(vec![crate::computer::DisplayInfo::new(
                0, 0, 200, 200,
            )
            .primary()]))
        }
    }

    #[cfg(feature = "vision")]
    fn corner_session(adapter: &Arc<CornerPaintingAdapter>) -> ComputerSession {
        ComputerSession::new(adapter.clone()).with_config(SessionConfig {
            settle: SettleConfig {
                initial_delay_ms: 0,
                poll_interval_ms: 0,
                max_rounds: 1,
                stable_threshold: 0.5,
                min_window_ms: 0,
            },
            ..SessionConfig::default()
        })
    }

    /// The user-facing bug this pins: "the click aimed correctly" and "the
    /// click took effect" used to be one answer, because verification only
    /// asked whether the screen changed at all. A corner repaint counted as
    /// proof that a click at (10,10) worked.
    #[cfg(feature = "vision")]
    #[tokio::test]
    async fn a_change_far_from_the_click_does_not_verify_it() {
        let adapter = Arc::new(CornerPaintingAdapter::new(Rect::new(150, 150, 32, 32)));
        let session = corner_session(&adapter);
        let outcome = session
            .execute(DesktopAction::Click {
                target: ClickTarget::Coordinate(crate::computer::Point::new(10, 10)),
                button: MouseButton::Left,
            })
            .await
            .expect("an outcome");

        let diff = outcome.diff.expect("a diff");
        assert!(diff.has_meaningful_changes(), "the screen did change: {}", diff.summary());
        assert!(diff.evidences_action(false), "and the diff knows it");
        assert_eq!(
            diff.aim.as_ref().map(|aim| aim.at_target),
            Some(false),
            "but not where the click aimed"
        );
        assert_eq!(
            outcome.verification,
            StepVerification::Unverifiable,
            "a change elsewhere is not this click working, and it is not proof it \
             failed either: {}",
            diff.summary()
        );
        assert_eq!(
            session.consecutive_failures().await,
            0,
            "an unattributed change must not accumulate as a failure"
        );
        assert!(
            diff.summary_for_action(false).contains("elsewhere"),
            "the model is told where the change was: {}",
            diff.summary_for_action(false)
        );
    }

    /// The control: the same adapter, the repaint under the click. The aim
    /// test must not turn a working click into a failure.
    #[cfg(feature = "vision")]
    #[tokio::test]
    async fn a_change_under_the_click_verifies_it() {
        let adapter = Arc::new(CornerPaintingAdapter::new(Rect::new(0, 0, 32, 32)));
        let session = corner_session(&adapter);
        let outcome = session
            .execute(DesktopAction::Click {
                target: ClickTarget::Coordinate(crate::computer::Point::new(10, 10)),
                button: MouseButton::Left,
            })
            .await
            .expect("an outcome");

        let diff = outcome.diff.expect("a diff");
        assert_eq!(diff.aim.as_ref().map(|aim| aim.at_target), Some(true));
        assert_eq!(outcome.verification, StepVerification::Verified, "{}", diff.summary());
    }
}
