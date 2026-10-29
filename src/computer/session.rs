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

use crate::computer::display::CoordinateSpace;
use crate::computer::element_index::SnapshotCache;
use crate::computer::page::{PageAction, PageActor, PageTarget};
use crate::computer::reflection::{FailureAnalysis, ReflectionEngine};
use crate::computer::rollback::RollbackManager;
use crate::computer::sensitive_ui::{SensitiveUiDetector, SensitiveUiFinding};
use crate::computer::use_loop::StepVerification;
use crate::computer::verification::{VerificationConfig, VerificationCriteria, VerificationEngine};
use crate::computer::vision::{is_screen_mutating_action, pixel_diff, ScreenDiff, ScreenState};
use crate::computer::{
    ActionEffect, ActionResult, ActionTarget, ClickTarget, ComputerAdapter, ComputerError,
    DeliveryMode, DesktopAction, Escalation, Rung, Screenshot, UiElement,
};

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
}

impl Default for SettleConfig {
    fn default() -> Self {
        Self {
            initial_delay_ms: 150,
            poll_interval_ms: 120,
            max_rounds: 4,
            stable_threshold: 0.5,
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
    /// Where input may land. `Background` refuses an input-layer action that
    /// cannot reach the app without taking the foreground, instead of
    /// delivering it anyway.
    pub delivery: DeliveryMode,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            verification: VerificationConfig::default(),
            settle: SettleConfig::default(),
            auto_rollback_threshold: 3,
            snapshot_paths: Vec::new(),
            verify: true,
            delivery: DeliveryMode::Foreground,
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
        let mutating = is_screen_mutating_action(&action);

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

        for (index, rung) in ladder.iter().copied().enumerate() {
            let last_rung = index + 1 == ladder.len();

            // Baseline for this attempt: one screenshot, and the tree only
            // when a diff will need it.
            let screenshot_before = if mutating {
                self.adapter.screenshot(None).await.ok()
            } else {
                None
            };
            let tree_before = if mutating {
                self.adapter.read_ui_tree(None).await.ok()
            } else {
                None
            };
            // Every capture the session takes names a coordinate space the
            // caller may quote back in a later action.
            if let Some(screenshot) = screenshot_before.as_ref() {
                self.note_space_for(screenshot).await;
            }

            attempted = Some(rung);
            let result = match self.dispatch(&action, rung).await {
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
            let state_after = if mutating {
                self.settle_and_capture(screenshot_before.as_ref()).await
            } else {
                None
            };

            let (verification, diff, hint) = if self.config.verify {
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
                diff,
                sensitive_before,
                sensitive_after,
                sensitive_target,
                reflection,
                snapshots_taken,
                rolled_back,
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
            .with_recovery(recovery.to_string())
            .with_tried(tried);
            evidence.escalation = None;
            let message = format!("refused: {error} — {recovery}");
            let result = ActionResult::error(message);
            let verification = StepVerification::Failed;
            let (rolled_back, reflection) = self
                .account(&action, &result, verification, ActionEffect::Refused)
                .await;
            return Ok(StepOutcome {
                action,
                result,
                verification,
                evidence: Some(evidence),
                state_after: None,
                diff: None,
                sensitive_before: Vec::new(),
                sensitive_after: self.sensitive_findings().await,
                sensitive_target: false,
                reflection,
                snapshots_taken,
                rolled_back,
            });
        }

        Err(error)
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
    async fn ladder_for(&self, action: &DesktopAction) -> Vec<Rung> {
        let target = match action {
            DesktopAction::Click { target, .. }
            | DesktopAction::DoubleClick { target, .. }
            | DesktopAction::Type { target: Some(target), .. } => target,
            _ => return vec![Rung::Pixel],
        };

        if matches!(target, ClickTarget::Coordinate(_)) {
            return vec![Rung::Pixel];
        }

        let resolved = self.adapter.resolve_target(target).await.ok();
        let mut ladder = Vec::new();
        // The ax rung needs a resolved element with an addressable path.
        if resolved
            .as_ref()
            .and_then(|r| r.element.as_ref())
            .map(|e| e.supports_ax_action())
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

    /// Perform the action at one rung.
    async fn dispatch(
        &self,
        action: &DesktopAction,
        rung: Rung,
    ) -> crate::computer::Result<ActionResult> {
        // The pointer and keyboard rungs are the two that can take the screen
        // away from whoever is using it. In background mode they go through
        // the adapter's background path, which refuses when the platform has
        // no way to reach the app without the foreground.
        if self.config.delivery == DeliveryMode::Background
            && matches!(rung, Rung::Pixel | Rung::Keyboard)
        {
            let target = self.resolved_target(action).await;
            return self
                .adapter
                .perform_background(action, target.as_ref())
                .await;
        }

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
                let focus = self
                    .adapter
                    .click_at(
                        crate::computer::Point::new(resolved.x, resolved.y),
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
    async fn settle_and_capture(&self, before: Option<&Screenshot>) -> Option<ScreenState> {
        let cfg = self.config.settle;
        let mut previous = before.cloned();
        let mut settled_shot = None;

        for round in 0..cfg.max_rounds.max(1) {
            let delay = if round == 0 {
                cfg.initial_delay_ms
            } else {
                cfg.poll_interval_ms
            };
            tokio::time::sleep(Duration::from_millis(delay)).await;

            let shot = match self.adapter.screenshot(None).await {
                Ok(shot) => shot,
                Err(e) => {
                    tracing::warn!("post-action screenshot failed: {}", e);
                    return None;
                }
            };

            let stable = match &previous {
                Some(prev) => {
                    !prev.base64.is_empty()
                        && !shot.base64.is_empty()
                        && pixel_diff(prev, &shot).change_percentage < cfg.stable_threshold
                }
                None => false,
            };
            settled_shot = Some(shot.clone());
            previous = Some(shot);
            if stable {
                break;
            }
        }

        let shot = settled_shot?;
        let tree = self.adapter.read_ui_tree(None).await.unwrap_or_default();
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
                if let (Some(before), Some(state)) = (tree_before, state_after) {
                    if let Some(hint) = typed_value_hint(before, &state.ui_tree, text) {
                        return Ok((hint.verification, None, Some(hint)));
                    }
                }
                self.verify_by_diff(tree_before, screenshot_before, screenshot_after, state_after)
                    .await
                    .map(|(verification, diff)| (verification, diff, None))
            }
            _ if is_screen_mutating_action(action) => self
                .verify_by_diff(tree_before, screenshot_before, screenshot_after, state_after)
                .await
                .map(|(verification, diff)| (verification, diff, None)),
            _ => Ok((StepVerification::Unverifiable, None, None)),
        }
    }

    /// Compare pre/post screen state.
    async fn verify_by_diff(
        &self,
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
        let diff = before.diff(&after);
        let changed = diff.has_meaningful_changes();
        if !changed {
            tracing::info!("verification: no visible change ({})", diff.summary());
        }
        let verdict = if changed {
            StepVerification::Verified
        } else {
            StepVerification::Failed
        };
        Ok((verdict, Some(diff)))
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
        ComputerError::BackgroundInputUnavailable(_) => Some(
            "this action cannot be delivered without the foreground — target an element the \
             accessibility tree can act on, use the page rung, or ask for foreground input",
        ),
        ComputerError::NotInitialized => {
            Some("the computer adapter is not initialized; configure desktop access first")
        }
        _ => None,
    }
}

/// Scan a tree for password / payment / destructive-confirmation UI.
fn scan_sensitive(tree: &[UiElement]) -> Vec<SensitiveUiFinding> {
    let detector = SensitiveUiDetector::new();
    let mut findings = Vec::new();
    for root in tree {
        findings.extend(detector.scan_tree(root));
    }
    findings
}

/// Combine what the screen showed with what the driver established.
///
/// The driver's `Confirmed` survives only when nothing contradicted it — a
/// failed diff means the action was delivered but had no visible effect, which
/// is `SuspectedNoop`, not `Confirmed`.
pub fn score_evidence(
    verification: StepVerification,
    driver: Option<&crate::computer::ActionEvidence>,
) -> crate::computer::ActionEvidence {
    let rung = driver.map(|d| d.rung).unwrap_or(Rung::Pixel);
    let tried = driver.map(|d| d.tried.clone()).unwrap_or_default();

    let effect = match verification {
        StepVerification::Verified => ActionEffect::Confirmed,
        StepVerification::Failed => match driver.map(|d| d.effect) {
            // The driver knows something the pixels do not: the action was
            // delivered and partly took, or never ran at all.
            Some(ActionEffect::Partial) => ActionEffect::Partial,
            Some(ActionEffect::Refused) => ActionEffect::Refused,
            _ => ActionEffect::SuspectedNoop,
        },
        StepVerification::Unverifiable => match driver.map(|d| d.effect) {
            Some(ActionEffect::Confirmed) => ActionEffect::Confirmed,
            Some(ActionEffect::Partial) => ActionEffect::Partial,
            Some(ActionEffect::Refused) => ActionEffect::Refused,
            _ => ActionEffect::Unverifiable,
        },
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
        escalation,
        tried,
        recovery: driver.and_then(|d| d.recovery.clone()),
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
    use crate::computer::types::{MouseButton, Rect, WaitCondition};
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
        log: tokio::sync::Mutex<Vec<String>>,
    }

    impl LadderAdapter {
        fn new(role: &str, set_value_works: bool) -> Self {
            Self {
                role: role.to_string(),
                set_value_works,
                log: tokio::sync::Mutex::new(Vec::new()),
            }
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
            Ok(ActionTarget {
                x: 10,
                y: 20,
                element: Some(crate::computer::element_index::ResolvedElement {
                    id: "e1".to_string(),
                    role: self.role.clone(),
                    label: Some("Field".to_string()),
                    center: crate::computer::Point::new(10, 20),
                    path: vec![0, 0],
                }),
                app: Some("Test".to_string()),
                snapshot: None,
            })
            .map(|mut resolved| {
                if let ClickTarget::Coordinate(point) = target {
                    resolved.x = point.x;
                    resolved.y = point.y;
                }
                resolved
            })
        }

        async fn perform_ax_action(
            &self,
            _target: &ActionTarget,
            action: crate::computer::AxAction,
        ) -> crate::computer::Result<ActionResult> {
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
            },
            ..SessionConfig::default()
        })
    }

    fn field(role: &str, value: &str) -> UiElement {
        UiElement {
            id: String::new(),
            role: role.to_string(),
            label: None,
            value: Some(value.to_string()),
            bounds: Rect::new(0, 0, 100, 20),
            enabled: true,
            focused: false,
            children: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_check_that_cannot_run_establishes_nothing() {
        // The bug this replaces: the old diff check answered `true` when it had
        // no screenshots to compare, so "could not tell" and "it worked" were
        // the same answer.
        let session = headless_session();
        let (outcome, diff) = session
            .verify_by_diff(None, None, None, None)
            .await
            .unwrap();
        assert_eq!(outcome, StepVerification::Unverifiable);
        assert!(diff.is_none());
        assert!(!outcome.verified(), "unverifiable is not verified");

        // An empty screenshot is the same situation: nothing to compare.
        let empty = Screenshot::new(String::new(), 0, 0);
        let (outcome, _) = session
            .verify_by_diff(None, Some(&empty), Some(&empty), None)
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
        assert_eq!(outcome.verification, StepVerification::Failed);
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
            .dispatch(&click_element("e1"), Rung::Page)
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

    #[tokio::test]
    async fn the_page_rung_hands_the_page_a_name_not_a_coordinate() {
        let adapter = Arc::new(LadderAdapter::new("button", true));
        let page = FakePage::new();
        let session = ladder_session(&adapter).with_page(page.clone(), "conv-1");

        session
            .dispatch(&click_element("e1"), Rung::Page)
            .await
            .expect("page click");
        session
            .dispatch(
                &DesktopAction::Type {
                    text: "hello".to_string(),
                    target: Some(ClickTarget::ElementId("e1".to_string())),
                },
                Rung::Page,
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
                x: 10,
                y: 20,
                element: Some(crate::computer::element_index::ResolvedElement {
                    id: "e1".to_string(),
                    role: "AXButton".to_string(),
                    label: Some("Save".to_string()),
                    center: crate::computer::Point::new(10, 20),
                    // No path: the accessibility rung cannot run, which is
                    // the case background delivery has to answer for.
                    path: Vec::new(),
                }),
                app: Some("Test".to_string()),
                snapshot: None,
            };
            if let ClickTarget::Coordinate(point) = target {
                resolved.x = point.x;
                resolved.y = point.y;
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
}
