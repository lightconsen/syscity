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

use crate::computer::element_index::SnapshotCache;
use crate::computer::reflection::{FailureAnalysis, ReflectionEngine};
use crate::computer::rollback::RollbackManager;
use crate::computer::sensitive_ui::{SensitiveUiDetector, SensitiveUiFinding};
use crate::computer::use_loop::StepVerification;
use crate::computer::verification::{VerificationConfig, VerificationCriteria, VerificationEngine};
use crate::computer::vision::{is_screen_mutating_action, pixel_diff, ScreenDiff, ScreenState};
use crate::computer::{
    ActionEffect, ActionResult, ActionTarget, ClickTarget, ComputerAdapter, ComputerError,
    DesktopAction, Escalation, Rung, Screenshot, UiElement,
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
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            verification: VerificationConfig::default(),
            settle: SettleConfig::default(),
            auto_rollback_threshold: 3,
            snapshot_paths: Vec::new(),
            verify: true,
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
        }
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

            let result = match self.dispatch(&action, rung).await {
                Ok(result) => result,
                Err(e) => {
                    // A rung that could not run is not a failed action: the
                    // next rung is exactly the answer.
                    tracing::info!("rung {:?} unavailable: {}", rung, e);
                    last_error = Some(e);
                    continue;
                }
            };
            let mut result = result;
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

            let (verification, diff) = if self.config.verify {
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
                (StepVerification::Unverifiable, None)
            };

            let evidence = score_evidence(verification, Some(&driver_evidence));

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

        Err(last_error.unwrap_or_else(|| {
            ComputerError::ToolFailed("no action rung could be executed".to_string())
        }))
    }

    /// Which rungs to try, in order, for this action.
    async fn ladder_for(&self, action: &DesktopAction) -> Vec<Rung> {
        let target = match action {
            DesktopAction::Click { target, .. } | DesktopAction::DoubleClick { target, .. } => {
                target
            }
            _ => return vec![Rung::Pixel],
        };

        if matches!(target, ClickTarget::Coordinate(_)) {
            return vec![Rung::Pixel];
        }

        let mut ladder = Vec::new();
        // The ax rung needs a resolved element with an addressable path.
        if let Ok(resolved) = self.adapter.resolve_target(target).await {
            if resolved
                .element
                .as_ref()
                .map(|e| e.supports_ax_action())
                .unwrap_or(false)
            {
                ladder.push(Rung::AxAction);
            }
        }
        ladder.push(Rung::Pixel);
        if matches!(action, DesktopAction::Click { .. }) {
            ladder.push(Rung::Keyboard);
        }
        ladder
    }

    /// Perform the action at one rung.
    async fn dispatch(
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
            // The keyboard rung operates on whatever already has focus — the
            // pixel rung in front of it is what put focus there.
            (Rung::Keyboard, DesktopAction::Click { .. }) => {
                self.adapter
                    .execute(DesktopAction::KeyPress {
                        keys: vec!["Return".to_string()],
                    })
                    .await
            }
            _ => self.adapter.execute(action.clone()).await,
        }
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
    ) -> crate::computer::Result<(StepVerification, Option<ScreenDiff>)> {
        match action {
            DesktopAction::Screenshot { .. } => Ok((StepVerification::Verified, None)),
            DesktopAction::Wait { .. } => Ok((StepVerification::Verified, None)),
            DesktopAction::LaunchApp { name, wait_for_ready, .. } => {
                if *wait_for_ready {
                    self.verifier
                        .verify(
                            &VerificationCriteria::ProcessRunning { name: name.clone() },
                            &ActionResult::success(""),
                            None,
                        )
                        .await
                        .map(|v| (StepVerification::from_checked(v), None))
                } else {
                    Ok((StepVerification::Unverifiable, None))
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
                .map(|v| (StepVerification::from_checked(v), None)),
            _ if is_screen_mutating_action(action) => {
                self.verify_by_diff(tree_before, screenshot_before, screenshot_after, state_after)
                    .await
            }
            _ => Ok((StepVerification::Unverifiable, None)),
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
        StepVerification::Failed => ActionEffect::SuspectedNoop,
        StepVerification::Unverifiable => match driver.map(|d| d.effect) {
            Some(ActionEffect::Confirmed) => ActionEffect::Confirmed,
            _ => ActionEffect::Unverifiable,
        },
    };

    let escalation = match (effect, rung) {
        (ActionEffect::Confirmed, _) => None,
        (ActionEffect::SuspectedNoop, Rung::AxAction) => Some(Escalation::Px),
        (ActionEffect::SuspectedNoop, Rung::Pixel) => Some(Escalation::Foreground),
        (ActionEffect::SuspectedNoop, Rung::Keyboard) => Some(Escalation::Page),
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

    fn headless_session() -> ComputerSession {
        ComputerSession::new(Arc::new(HeadlessComputerAdapter::new()))
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
        let (outcome, _) = session
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
                    &DesktopAction::Type { text: "x".into() },
                    &ActionResult::error("boo"),
                    StepVerification::Failed,
                    ActionEffect::SuspectedNoop,
                )
                .await;
        }
        assert_eq!(session.consecutive_failures().await, 2);

        session
            .account(
                &DesktopAction::Type { text: "x".into() },
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
                &DesktopAction::Type { text: "x".into() },
                &ActionResult::error("boo"),
                StepVerification::Failed,
                ActionEffect::SuspectedNoop,
            )
            .await;
        session.reset().await;
        assert_eq!(session.consecutive_failures().await, 0);
        assert!(session.notes().await.is_empty());
    }
}
