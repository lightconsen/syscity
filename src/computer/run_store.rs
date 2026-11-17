//! Checkpoints for computer-use runs, so a restart does not erase them.
//!
//! A run is minutes long and takes real actions on somebody's screen. When the
//! gateway goes down mid-run, the naive outcome is that the run simply ceases
//! to exist: the registry was in memory, the steps are gone, and the next
//! person to look at the desktop cannot tell what it was doing. This module
//! keeps one JSON file per run under `<layout root>/computer-runs/`, written
//! after every completed step with the same tmp+rename that makes a crash
//! mid-write leave the previous checkpoint intact.
//!
//! Two deliberate choices, both following the existing goal-store decision
//! (`lifecycle.rs`: "a restart must not re-arm autonomous loops without
//! explicit human consent"):
//!
//! - **Nothing resumes on startup.** The files are read to build a list, and a
//!   person resumes one with `computer.resume`.
//! - **A conclusion is kept, a resume is not offered.** The file survives
//!   `done` and `cancelled` — it is the run's only durable trace once the
//!   process's in-memory state and the live event broadcast are gone — but
//!   [`Checkpoint::is_resumable`] is false for both, so it never appears as an
//!   offer to continue. [`RunStore::prune_finished`] bounds the directory by
//!   age instead of by deleting every conclusion.
//!
//! The checkpoint is not a transcript. It carries what a resume needs — the
//! goal, the loop's configuration, the actions already taken with their
//! verification and screenshot refs, the step counter and the budget counters —
//! and nothing that would let it *skip* a dangerous action: a sensitive-UI
//! action is not replayed from the file, it is asked again (see
//! [`Checkpoint::resumes_from`] and the executor).
//!
//! The cost figures (`model_calls`, `images_attached`, `images_repeated`,
//! `tokens_in`, `tokens_out`) are written on every step and again on
//! conclusion, so the file answers "what did this run cost" for a run that
//! finished as well as for one that did not — the live summary in memory is
//! gone once the run leaves the registry.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::computer::run_budget::{RunBudget, StopReason};
use crate::computer::types::DesktopAction;
use crate::computer::use_loop::{LoopConfig, StepVerification};

/// Directory name for run checkpoints under the layout root.
const RUNS_DIR_NAME: &str = "computer-runs";

/// Where run checkpoints live when the caller does not pin a directory.
///
/// `installed_paths` rather than `paths`: a store built before a layout root is
/// installed (a library caller, a unit test) must be able to answer this
/// question without the answer being a panic.
pub fn computer_runs_dir() -> PathBuf {
    match crate::dirs::installed_paths() {
        Some(paths) => paths.computer_runs_dir(),
        None => crate::dirs::syscity_dir().join(RUNS_DIR_NAME),
    }
}

/// One line per completed step, shared by the run's live view and by a resumed
/// run rebuilding that view from its checkpoint.
///
/// One function rather than two formats: a resumed run's `completed_steps` must
/// read exactly like a run that never stopped, or a person comparing the two
/// before and after a restart would think a step changed.
pub fn step_line(step: usize, action: &str, result: &str, verdict: &str) -> String {
    let flat = result.replace(['\n', '\r'], " ");
    let mut chars = flat.chars();
    let head: String = chars.by_ref().take(100).collect();
    let shown = if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    };
    format!("{}. {action} → {shown} ({verdict})", step + 1)
}

/// The verdict word for a step, as the handoff note uses it.
pub fn verdict_word(verification: StepVerification) -> &'static str {
    match verification {
        StepVerification::Verified => "ok",
        StepVerification::Failed => "failed",
        StepVerification::Unverifiable => "unchecked",
    }
}

/// The step's verification as the three-state boolean the run events carry.
///
/// `None` is not "false": an unverifiable step is one nothing could confirm
/// *or* deny, and a timeline that drew it as a failure would be claiming a
/// verdict the adapter never reached.
pub fn verified_word(verification: StepVerification) -> Option<bool> {
    match verification {
        StepVerification::Verified => Some(true),
        StepVerification::Failed => Some(false),
        StepVerification::Unverifiable => None,
    }
}

/// One completed step, as the checkpoint records it.
///
/// `DesktopAction` and `StepVerification` are the loop's own types rather than
/// strings: a resumed run hands the history back to the decision closure, and
/// it must be the same shape the driver already renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedStep {
    pub step: usize,
    pub action: DesktopAction,
    pub verification: StepVerification,
    /// What the adapter said happened, in one line.
    pub result: String,
    /// CAS digest of the pre-action capture, when one was stored.
    ///
    /// A checkpoint written before this field existed has none, and a resumed
    /// run's replayed history is the post-action line alone — which is why it
    /// is optional rather than defaulted to a placeholder digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_before_ref: Option<String>,
    /// CAS digest of the post-action capture, when one was stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_ref: Option<String>,
    /// Whether the action aimed at an element the sensitive-UI detector
    /// flagged.
    ///
    /// A resumed run does *not* skip these: the person who approves a resume is
    /// approving the goal, not the individual password change that a previous
    /// model call happened to aim at. Recording it here is what lets the resume
    /// path say "step 3 touched a password field; it will be asked again".
    #[serde(default)]
    pub sensitive: bool,
    /// Which rung of the action ladder finally delivered the action.
    ///
    /// Absent for a step the adapter reported no evidence for, and for a
    /// checkpoint written before this field existed. Kept because the
    /// distribution over a failed run is the difference between "it could not
    /// find the control" (all `ax_action`, never escalated) and "it found it and
    /// the rung did not work" (escalated through `pixel` / `keyboard`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rung: Option<crate::computer::types::Rung>,
    /// What the adapter established about the screen's response.
    ///
    /// The same fact `computer.step` events carry and the same one a turn's
    /// `ActionTraceEvent` records, kept here so a run's checkpoint can be drawn
    /// as the identical timeline `turns.replay` draws for a turn. Absent for a
    /// checkpoint written before this field existed, and for a step whose
    /// adapter reported no evidence at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<crate::computer::types::ActionEffect>,
    /// The box the step aimed at when its visible change landed elsewhere.
    ///
    /// The one fact from the pre/post diff that outlives the step: a resumed
    /// run's prompt renders one line per past step, and "unchecked" without
    /// "it aimed at (x,y) and nothing there moved" invites the same click
    /// again. Absent for every other step and for a checkpoint written before
    /// this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aim_missed: Option<crate::computer::Rect>,
}

/// What a checkpoint needs to rebuild a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub run_id: String,
    pub goal: String,
    /// The conversation that started the run, when there was one. A resumed run
    /// keeps reporting to the same session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The turn that started the run, when one did.
    ///
    /// A run does not write into a turn's `full.json` — it has its own file and
    /// its own step stream — so this is the only thing that lets `turns.replay`
    /// answer "this turn, and the run it kicked off" as one timeline. Read by
    /// [`RunStore::load_for_turn`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// The loop configuration the run started with.
    pub max_steps: usize,
    pub settle_delay_ms: u64,
    pub verify_after_each: bool,
    /// Steps that already ran, in order.
    pub completed: Vec<CompletedStep>,
    /// Model calls already spent. Carried so a resume does not hand the run a
    /// fresh budget and quietly double what the caller allowed.
    pub model_calls: usize,
    pub images_attached: usize,
    /// How many of those images showed a frame the run had already sent.
    ///
    /// Kept so the file's cost line matches the live summary the gateway
    /// showed: a stalled run is recognisable by this number, not by the total.
    #[serde(default)]
    pub images_repeated: usize,
    /// Tokens the run has been billed for so far.
    ///
    /// Carried for the same reason as the call counter — a resumed run's
    /// summary is the run's total, not the last process's share — and because
    /// the file is the only place the figure survives a restart.
    #[serde(default)]
    pub tokens_in: u64,
    #[serde(default)]
    pub tokens_out: u64,
    /// Set once the run reaches a conclusion. A checkpoint with this set is not
    /// resumable — it exists only until the file is swept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// When the checkpoint was last written (RFC 3339).
    pub updated_at: String,
}

impl Checkpoint {
    pub fn new(run_id: impl Into<String>, goal: impl Into<String>, config: &LoopConfig) -> Self {
        Self {
            run_id: run_id.into(),
            goal: goal.into(),
            session_id: None,
            turn_id: None,
            max_steps: config.max_steps,
            settle_delay_ms: config.settle_delay_ms,
            verify_after_each: config.verify_after_each,
            completed: Vec::new(),
            model_calls: 0,
            images_attached: 0,
            images_repeated: 0,
            tokens_in: 0,
            tokens_out: 0,
            stop_reason: None,
            updated_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// The loop configuration to resume with.
    pub fn loop_config(&self) -> LoopConfig {
        LoopConfig {
            max_steps: self.max_steps,
            settle_delay_ms: self.settle_delay_ms,
            verify_after_each: self.verify_after_each,
            ..Default::default()
        }
    }

    /// Whether this run was cut off rather than concluded.
    ///
    /// Only an interrupted run is worth offering to resume: a finished one has
    /// an answer, and a cancelled one had a person say stop.
    pub fn is_interrupted(&self) -> bool {
        self.stop_reason.is_none()
    }

    /// Whether [`crate::computer::run_executor`] can continue this run.
    ///
    /// Everything except a conclusion qualifies. A concluded run's file is kept
    /// as its trace, but continuing a `done` run would re-run a goal that has
    /// an answer, and continuing a `cancelled` one would override a person who
    /// said stop. A file that was cut off, ran out of room, or stopped to ask a
    /// person is resumable in all three cases: continuing from step N+1 is a
    /// meaningful offer.
    pub fn is_resumable(&self) -> bool {
        !matches!(self.stop_reason, Some(StopReason::Done | StopReason::Cancelled))
    }

    /// The listing `computer.runs` returns for one file.
    pub fn view(&self) -> serde_json::Value {
        serde_json::json!({
            "run_id": self.run_id,
            "goal": self.goal,
            // The conversation and turn this run belongs to, so a client can
            // show a conversation its own runs after a reload. A run's live
            // state is in memory and dies with the process; the file is what
            // is left, and without these two keys a panel reopened tomorrow
            // could only list every run on the machine — including other
            // people's sessions on a shared gateway.
            "session_id": self.session_id,
            "turn_id": self.turn_id,
            "steps_completed": self.completed.len(),
            "next_step": self.next_step(),
            "model_calls": self.model_calls,
            "images_attached": self.images_attached,
            "images_repeated": self.images_repeated,
            "tokens_in": self.tokens_in,
            "tokens_out": self.tokens_out,
            "interrupted": self.is_interrupted(),
            "stop_reason": self.stop_reason,
            "sensitive_steps": self.sensitive_steps(),
            "updated_at": self.updated_at,
            "completed_steps": self
                .completed
                .iter()
                .map(|s| {
                    crate::computer::run_store::step_line(
                        s.step,
                        &crate::computer::driver::describe_action(&s.action),
                        &s.result,
                        verdict_word(s.verification),
                    )
                })
                .collect::<Vec<_>>(),
            // The same steps with their frame refs, so a listing can draw the
            // action/screenshot timeline without a second call per step.
            "steps": self
                .completed
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "step": s.step,
                        "action": crate::computer::driver::describe_action(&s.action),
                        "result": s.result,
                        "verified": verified_word(s.verification),
                        "before_ref": s.screenshot_before_ref,
                        "after_ref": s.screenshot_ref,
                        "sensitive": s.sensitive,
                        "rung": s.rung.map(|r| r.rung_name()),
                        // What the adapter established about the screen's
                        // response. A client drawing a step as "done" when the
                        // adapter reported `refused` would be showing the
                        // opposite of what happened — the same reason the live
                        // event carries it.
                        "effect": s.effect.map(|e| e.as_str()),
                        // Present only when the change was away from the
                        // target, so the timeline can distinguish "nothing
                        // could check this" from "it visibly missed".
                        "aim_missed": s.aim_missed,
                    })
                })
                .collect::<Vec<_>>(),
        })
    }

    /// The first step number a resumed run should execute.
    ///
    /// Steps are 0-indexed, so a run with three completed steps resumes at 3.
    /// A dangerous step is *not* subtracted here: it is re-asked, and counting
    /// it would mean skipping it.
    pub fn next_step(&self) -> usize {
        self.completed.len()
    }

    /// How many of the recorded steps touched something a person should see
    /// again before the resume continues.
    pub fn sensitive_steps(&self) -> usize {
        self.completed.iter().filter(|s| s.sensitive).count()
    }

    /// How many steps were delivered at each rung of the action ladder.
    ///
    /// Steps with no reported evidence are counted under `"unknown"` rather than
    /// dropped: a run whose every step is `unknown` is telling you the adapter
    /// cannot say, which is a different fact from a run that clicked and had
    /// nothing happen. The map always carries every rung key, so a reader
    /// comparing two runs does not have to tell "zero" from "key missing".
    pub fn rung_distribution(&self) -> std::collections::BTreeMap<&'static str, usize> {
        let mut out = std::collections::BTreeMap::new();
        for rung in ["ax_action", "pixel", "keyboard", "page", "unknown"] {
            out.insert(rung, 0);
        }
        for step in &self.completed {
            let key = step.rung.map(|r| r.rung_name()).unwrap_or("unknown");
            *out.entry(key).or_insert(0) += 1;
        }
        out
    }

    /// The failed step, when the run recorded one.
    ///
    /// A "failure" here is the last step the verification could not confirm:
    /// the handoff's `stuck_at` is about where the *run* stopped, while this is
    /// about which action is the evidence of the obstacle. `None` when every
    /// step verified — a run that hit its ceiling while still working has no
    /// failed step to point at, and inventing one would be worse than saying so.
    pub fn first_failed_step(&self) -> Option<&CompletedStep> {
        self.completed
            .iter()
            .rev()
            .find(|s| s.verification == StepVerification::Failed)
    }

    /// The history a resumed loop starts from.
    ///
    /// The result messages are carried as the adapter reported them; the
    /// post-action captures are not, because a checkpoint stores a CAS digest
    /// and re-reading a frame into every prompt after a restart would spend the
    /// resumed turn's context on images the decision maker has already acted
    /// on. The digest stays in [`CompletedStep::screenshot_ref`] for the
    /// replay path.
    pub fn resume_history(&self) -> Vec<crate::computer::use_loop::StepRecord> {
        self.completed
            .iter()
            .map(|step| crate::computer::use_loop::StepRecord {
                step: step.step,
                action: step.action.clone(),
                result: if step.verification == StepVerification::Failed {
                    crate::computer::ActionResult::error(step.result.clone())
                } else {
                    crate::computer::ActionResult::success(step.result.clone())
                },
                verification: step.verification,
                screenshot_before: None,
                screenshot_after: None,
                snapshots_taken: 0,
                sensitive: step.sensitive,
                aim_missed: step.aim_missed,
            })
            .collect()
    }

    /// The state a resumed loop continues from.
    pub fn resume_state(&self) -> crate::computer::use_loop::LoopResume {
        crate::computer::use_loop::LoopResume {
            history: self.resume_history(),
            start_step: self.next_step(),
            last_verified: match self.completed.last().map(|s| s.verification) {
                Some(StepVerification::Verified) => Some(true),
                Some(StepVerification::Failed) => Some(false),
                Some(StepVerification::Unverifiable) | None => None,
            },
            consecutive_failures: self
                .completed
                .iter()
                .rev()
                .take_while(|s| s.verification != StepVerification::Verified)
                .count(),
        }
    }

    /// A one-line description for a listing.
    pub fn describe(&self) -> String {
        format!(
            "{} — {} step(s) done, {} model call(s), updated {}",
            self.goal,
            self.completed.len(),
            self.model_calls,
            self.updated_at
        )
    }
}

/// File-based checkpoint store.
///
/// One file per run at `<dir>/<run_id>.json`, written atomically.
pub struct RunStore {
    /// `None` means the process-default directory, resolved on first use:
    /// constructing a store must not require an installed layout root.
    dir: Option<PathBuf>,
}

impl Default for RunStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RunStore {
    pub fn new() -> Self {
        Self { dir: None }
    }

    /// A store writing into an explicit directory (tests, or a gateway that
    /// wants its runs somewhere other than the layout root).
    pub fn with_dir(dir: PathBuf) -> Self {
        Self { dir: Some(dir) }
    }

    /// A store for the layout root a gateway was built with.
    pub fn for_paths(paths: &crate::dirs::SyscityPaths) -> Self {
        Self::with_dir(paths.computer_runs_dir())
    }

    fn dir(&self) -> PathBuf {
        self.dir.clone().unwrap_or_else(computer_runs_dir)
    }

    async fn ensure_dir(&self) -> crate::Result<()> {
        let dir = self.dir();
        if !dir.exists() {
            tokio::fs::create_dir_all(&dir).await.map_err(|e| {
                crate::error::SyscityError::Storage {
                    context: format!("Failed to create computer-runs directory: {dir:?}"),
                    details: e.to_string(),
                }
            })?;
        }
        Ok(())
    }

    fn state_path(&self, run_id: &str) -> PathBuf {
        self.dir().join(format!("{run_id}.json"))
    }

    /// A run id that is safe to use as a file name.
    ///
    /// The ids this gateway mints are UUIDs, but `computer.resume` and
    /// `computer.status` take one from the caller, and the checkpoint path is
    /// built by joining it onto a directory. A caller must not be able to name
    /// `../…` and address a file outside the runs directory.
    fn valid_run_id(run_id: &str) -> bool {
        !run_id.is_empty()
            && !run_id.contains('/')
            && !run_id.contains('\\')
            && !run_id.contains("..")
            && !run_id.contains('\0')
    }

    /// Write the checkpoint, atomically.
    ///
    /// Serialise to `<file>.tmp`, then rename over the real file: a crash
    /// mid-write leaves the previous checkpoint intact instead of a truncated
    /// one, which is the difference between "resume from one step earlier" and
    /// "this run's history is unreadable".
    pub async fn save(&self, checkpoint: &Checkpoint) -> crate::Result<()> {
        self.ensure_dir().await?;
        let path = self.state_path(&checkpoint.run_id);
        let json = serde_json::to_string_pretty(checkpoint).map_err(|e| {
            crate::error::SyscityError::Internal(format!(
                "Failed to serialize computer run checkpoint: {e}"
            ))
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
                context: format!("Failed to write computer run checkpoint tmp: {tmp_path:?}"),
                details: e.to_string(),
            }
        })?;

        tokio::fs::rename(&tmp_path, &path).await.map_err(|e| {
            // Best-effort cleanup of the stale tmp file; a failure here is not
            // the reason this call is failing.
            let _ = std::fs::remove_file(&tmp_path);
            crate::error::SyscityError::Storage {
                context: format!("Failed to finalize computer run checkpoint: {path:?}"),
                details: e.to_string(),
            }
        })?;
        Ok(())
    }

    /// Load every checkpoint. Unreadable or unparseable files are skipped with
    /// a warning: one corrupt checkpoint must not hide the others.
    pub async fn load_all(&self) -> Vec<Checkpoint> {
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
                Ok(content) => match serde_json::from_str::<Checkpoint>(&content) {
                    Ok(checkpoint) => out.push(checkpoint),
                    Err(e) => {
                        tracing::warn!("[computer] unreadable run checkpoint {path:?}: {e}");
                    }
                },
                Err(e) => tracing::warn!("[computer] could not read run checkpoint {path:?}: {e}"),
            }
        }
        out.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));
        out
    }

    /// Load one checkpoint, if it exists.
    ///
    /// A caller-supplied id that is not safe as a file name is a miss, not an
    /// error: "no run with that id" is the truth either way, and the read has
    /// nothing to offer a path-shaped id.
    pub async fn load(&self, run_id: &str) -> Option<Checkpoint> {
        if !Self::valid_run_id(run_id) {
            return None;
        }
        let path = self.state_path(run_id);
        let content = tokio::fs::read_to_string(&path).await.ok()?;
        match serde_json::from_str::<Checkpoint>(&content) {
            Ok(checkpoint) => Some(checkpoint),
            Err(e) => {
                tracing::warn!("[computer] unreadable run checkpoint {path:?}: {e}");
                None
            }
        }
    }

    /// Every run started by one turn, oldest first.
    ///
    /// For `turns.replay`: a turn that kicked off a computer run left the
    /// action steps in the run's own checkpoint, not in the turn's `full.json`.
    /// Without this lookup the timeline of such a turn shows a tool call and
    /// nothing that happened on the screen.
    pub async fn load_for_turn(&self, turn_id: &str) -> Vec<Checkpoint> {
        let mut out: Vec<Checkpoint> = self
            .load_all()
            .await
            .into_iter()
            .filter(|c| c.turn_id.as_deref() == Some(turn_id))
            .collect();
        // A turn is one point in time; its runs are ordered by when they
        // started writing, which for a single turn is the order they ran.
        out.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));
        out
    }

    /// Remove a run's checkpoint.
    ///
    /// Not called on conclusion any more — see [`RunCheckpointer::conclude`] —
    /// but kept for an operator who wants a run gone, and for
    /// [`RunStore::prune_finished`].
    pub async fn delete(&self, run_id: &str) {
        if !Self::valid_run_id(run_id) {
            return;
        }
        let path = self.state_path(run_id);
        if path.exists() {
            if let Err(e) = tokio::fs::remove_file(&path).await {
                tracing::warn!("[computer] could not delete run checkpoint {path:?}: {e}");
            }
        }
    }

    /// Delete concluded runs older than `max_age`, and report how many went.
    ///
    /// Resumable runs are never touched, however old: "you can continue this"
    /// is an offer that does not expire on a clock. A concluded run is a
    /// record, and a record older than `max_age` has stopped being the one a
    /// person looks at. An unparseable file is also left alone — deleting
    /// something this program could not read would destroy evidence of a bug.
    pub async fn prune_finished(&self, max_age: std::time::Duration) -> usize {
        // `spawn_blocking`: the loop is synchronous, and a directory scan on
        // the async runtime would block every other task it is serving.
        let dir = self.dir();
        tokio::task::spawn_blocking(move || prune_finished_in(&dir, max_age))
            .await
            .unwrap_or(0)
    }
}

/// [`RunStore::prune_finished`]'s body, synchronous so it can run off-runtime.
fn prune_finished_in(dir: &std::path::Path, max_age: std::time::Duration) -> usize {
    let mut removed = 0usize;
    let now = chrono::Utc::now();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(checkpoint) = serde_json::from_str::<Checkpoint>(&content) else {
            continue;
        };
        if checkpoint.is_resumable() {
            continue;
        }
        let age = chrono::DateTime::parse_from_rfc3339(&checkpoint.updated_at)
            .map(|t| now.signed_duration_since(t.with_timezone(&chrono::Utc)))
            .unwrap_or_else(|_| chrono::Duration::zero());
        let age = std::time::Duration::from_secs(age.num_seconds().max(0) as u64);
        if age <= max_age {
            continue;
        }
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!("[computer] could not prune finished run {path:?}: {e}");
            continue;
        }
        removed += 1;
    }
    removed
}

/// Appends steps to a checkpoint as a run progresses, and settles it at the end.
///
/// A run's steps arrive through the observer path, which is also where the
/// screenshot digest is computed; this writer takes the same facts rather than
/// re-deriving them, so the checkpoint and the event stream cannot disagree
/// about what step 3 did.
pub struct RunCheckpointer {
    store: Arc<RunStore>,
    checkpoint: tokio::sync::Mutex<Checkpoint>,
}

impl RunCheckpointer {
    pub fn new(store: Arc<RunStore>, checkpoint: Checkpoint) -> Self {
        Self {
            store,
            checkpoint: tokio::sync::Mutex::new(checkpoint),
        }
    }

    /// Record one completed step and write the file.
    ///
    /// A write failure is logged, never propagated: losing a checkpoint costs a
    /// resume, but failing the step that just succeeded would be a worse deal.
    pub async fn step(&self, step: &CompletedStep, spent: &RunBudget) {
        let snapshot = {
            let mut checkpoint = self.checkpoint.lock().await;
            checkpoint.completed.push(step.clone());
            checkpoint.model_calls = spent.model_calls();
            checkpoint.images_attached = spent.images_attached();
            checkpoint.images_repeated = spent.images_repeated();
            checkpoint.tokens_in = spent.tokens_in();
            checkpoint.tokens_out = spent.tokens_out();
            checkpoint.updated_at = chrono::Utc::now().to_rfc3339();
            checkpoint.clone()
        };
        if let Err(e) = self.store.save(&snapshot).await {
            tracing::warn!("[computer] could not checkpoint run {}: {e}", snapshot.run_id);
        }
    }

    /// Settle the checkpoint when the run ends.
    ///
    /// The file is **kept** for every conclusion, including `done` and
    /// `cancelled`. A finished run is not resumable — [`Checkpoint::is_resumable`]
    /// says so, and `computer.runs` hides it unless asked — but it is the only
    /// record of what the run did that outlives the process: the run's live
    /// state is in memory, its events were broadcast to whoever was connected at
    /// the time, and the screenshots it references are content-addressed. Delete
    /// the file and a *successful* run leaves no trace at all, which contradicts
    /// "过程可查" more sharply than a failed one does.
    ///
    /// The directory is bounded by age instead ([`RunStore::prune_finished`]),
    /// so keeping conclusions cannot grow it forever.
    pub async fn conclude(&self, reason: StopReason, spent: &RunBudget) {
        let snapshot = {
            let mut checkpoint = self.checkpoint.lock().await;
            checkpoint.stop_reason = Some(reason);
            checkpoint.model_calls = spent.model_calls();
            checkpoint.images_attached = spent.images_attached();
            checkpoint.images_repeated = spent.images_repeated();
            checkpoint.tokens_in = spent.tokens_in();
            checkpoint.tokens_out = spent.tokens_out();
            checkpoint.updated_at = chrono::Utc::now().to_rfc3339();
            checkpoint.clone()
        };
        if let Err(e) = self.store.save(&snapshot).await {
            tracing::warn!("[computer] could not finalize run {}: {e}", snapshot.run_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::types::{ClickTarget, MouseButton};

    fn checkpoint(run_id: &str) -> Checkpoint {
        let mut cp = Checkpoint::new(run_id, "open the calculator", &LoopConfig::default());
        cp.completed.push(CompletedStep {
            step: 0,
            action: DesktopAction::Click {
                target: ClickTarget::ElementId("s1:0.2".to_string()),
                button: MouseButton::Left,
            },
            verification: StepVerification::Verified,
            result: "clicked".to_string(),
            screenshot_before_ref: Some("sha256:before".to_string()),
            screenshot_ref: Some("sha256:abc".to_string()),
            sensitive: false,
            rung: Some(crate::computer::types::Rung::AxAction),
            effect: Some(crate::computer::types::ActionEffect::Confirmed),
            aim_missed: None,
        });
        cp.model_calls = 1;
        cp
    }

    #[tokio::test]
    async fn a_checkpoint_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = RunStore::with_dir(dir.path().to_path_buf());
        let cp = checkpoint("r1");
        store.save(&cp).await.expect("save");

        let loaded = store.load("r1").await.expect("load");
        assert_eq!(loaded.goal, "open the calculator");
        assert_eq!(loaded.completed.len(), 1);
        assert_eq!(loaded.completed[0].screenshot_ref.as_deref(), Some("sha256:abc"));
        assert_eq!(loaded.model_calls, 1);
        assert!(loaded.is_resumable());
    }

    /// The write is atomic, so a crash between the tmp write and the rename
    /// leaves the previous checkpoint readable.
    #[tokio::test]
    async fn a_stale_tmp_file_does_not_replace_a_good_checkpoint() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = RunStore::with_dir(dir.path().to_path_buf());
        store.save(&checkpoint("r1")).await.expect("save");

        // Simulate the crash: a tmp file with garbage next to the real one.
        std::fs::write(dir.path().join("r1.json.tmp"), b"{ this is not json").expect("write tmp");

        let loaded = store.load("r1").await.expect("the real file is intact");
        assert_eq!(loaded.completed.len(), 1);
    }

    /// A run the gateway was cut off in the middle of is resumable, and says
    /// so as "interrupted" rather than as a stop reason it never reached.
    #[tokio::test]
    async fn a_finished_run_is_not_resumable() {
        let mut cp = checkpoint("r1");
        cp.stop_reason = Some(StopReason::Done);
        assert!(!cp.is_resumable());
        assert!(!cp.is_interrupted());

        // `cancelled` had a person say stop; continuing it is not an offer to
        // make.
        cp.stop_reason = Some(StopReason::Cancelled);
        assert!(!cp.is_resumable());
    }

    /// Running out of room is a reason to offer a resume, not a conclusion:
    /// the plan's whole distinction between a ceiling and an answer.
    #[tokio::test]
    async fn a_ceiling_stop_is_resumable() {
        let mut cp = checkpoint("r1");
        cp.stop_reason = Some(StopReason::MaxSteps);
        assert!(cp.is_resumable());
        assert!(!cp.is_interrupted());
    }

    #[tokio::test]
    async fn an_interrupted_checkpoint_is_resumable() {
        let cp = checkpoint("r1");
        assert!(cp.is_interrupted());
        assert!(cp.is_resumable());
    }

    /// A run interrupted at step 3 resumes at step 3, not 4: the sensitive
    /// step is executed again rather than skipped, and skipping it would be
    /// exactly the bug this counter guards.
    #[test]
    fn the_next_step_counts_what_was_done() {
        let mut cp = checkpoint("r1");
        cp.completed.push(CompletedStep {
            step: 1,
            action: DesktopAction::KeyPress {
                keys: vec!["return".to_string()],
            },
            verification: StepVerification::Failed,
            result: "no effect".to_string(),
            screenshot_before_ref: None,
            screenshot_ref: None,
            sensitive: false,
            rung: None,
            effect: None,
            aim_missed: None,
        });
        assert_eq!(cp.next_step(), 2);
    }

    #[test]
    fn sensitive_steps_are_counted_for_the_resume_notice() {
        let mut cp = checkpoint("r1");
        cp.completed[0].sensitive = true;
        assert_eq!(cp.sensitive_steps(), 1);
    }

    /// Where a miss aimed survives the process that saw it.
    ///
    /// A resumed run's prompt is built from `resume_history`, which has no
    /// diff — only what the checkpoint kept. If the box is dropped here, the
    /// next process can say a click was "unchecked" and nothing more, and the
    /// model restarts by retrying it.
    #[tokio::test]
    async fn a_miss_reaches_the_resumed_history() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = RunStore::with_dir(dir.path().to_path_buf());
        let mut cp = checkpoint("r1");
        cp.completed[0].verification = StepVerification::Unverifiable;
        cp.completed[0].aim_missed = Some(crate::computer::Rect::new(4, 8, 64, 64));
        store.save(&cp).await.expect("save");

        let loaded = store.load("r1").await.expect("load");
        let history = loaded.resume_history();
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0].aim_missed,
            Some(crate::computer::Rect::new(4, 8, 64, 64)),
            "the box must survive the file, not just the struct"
        );
    }

    /// A checkpoint written before this field existed still loads. The field
    /// is a note, not a fact the run depends on, so an absent one is `None`
    /// rather than a deserialisation error.
    #[test]
    fn an_old_checkpoint_without_an_aim_still_loads() {
        let mut cp = checkpoint("r1");
        let mut json = serde_json::to_value(&cp).expect("serialize");
        let steps = json["completed"].as_array_mut().expect("steps");
        steps[0]
            .as_object_mut()
            .expect("object")
            .remove("aim_missed");
        cp = serde_json::from_value(json).expect("an old checkpoint loads");
        assert_eq!(cp.completed[0].aim_missed, None);
    }

    /// A panel reopened after a restart has only the file to go on, so the file
    /// has to say which conversation the run belongs to. Without these keys the
    /// listing can only answer "every run on this machine", which on a shared
    /// gateway is other people's sessions too.
    #[test]
    fn a_saved_run_names_the_conversation_and_turn_it_belongs_to() {
        let mut cp = checkpoint("r1");
        cp.session_id = Some("s-42".to_string());
        cp.turn_id = Some("t-7".to_string());

        let view = cp.view();
        assert_eq!(view["session_id"], "s-42");
        assert_eq!(view["turn_id"], "t-7");
    }

    /// A run started outside a turn has neither, and the view must say so with
    /// a missing value rather than an empty string — "" is a session id that
    /// no conversation has, and a client filtering on it would silently match
    /// nothing while looking like it matched something.
    #[test]
    fn a_run_outside_a_turn_has_no_conversation_to_name() {
        let view = checkpoint("r1").view();
        assert!(view["session_id"].is_null());
        assert!(view["turn_id"].is_null());
    }

    /// The step list a listing draws carries what the adapter established, not
    /// only whether a verification passed: `refused` and `suspected_noop` both
    /// read as "not verified", and a panel that showed them the same way would
    /// hide the one fact that says "change the request, don't retry".
    #[test]
    fn each_saved_step_carries_what_the_adapter_established() {
        let mut cp = checkpoint("r1");
        cp.completed[0].effect = Some(crate::computer::types::ActionEffect::Refused);

        let step = &cp.view()["steps"][0];
        assert_eq!(step["effect"], "refused");
        assert_eq!(step["rung"], "ax_action", "the rung travels with it");
    }

    #[tokio::test]
    async fn load_all_skips_a_corrupt_file_and_keeps_the_rest() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = RunStore::with_dir(dir.path().to_path_buf());
        store.save(&checkpoint("good")).await.expect("save");
        std::fs::write(dir.path().join("bad.json"), b"not json").expect("write bad");

        let all = store.load_all().await;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].run_id, "good");
    }

    #[tokio::test]
    async fn deleting_a_checkpoint_removes_it_from_the_listing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = RunStore::with_dir(dir.path().to_path_buf());
        store.save(&checkpoint("r1")).await.expect("save");
        store.delete("r1").await;
        assert!(store.load_all().await.is_empty());
        assert!(store.load("r1").await.is_none());
    }

    #[tokio::test]
    async fn loading_from_a_missing_directory_is_empty_not_an_error() {
        let store = RunStore::with_dir(PathBuf::from("/nonexistent/syscity-runs"));
        assert!(store.load_all().await.is_empty());
        assert!(store.load("nothing").await.is_none());
    }

    /// The store carries the loop's settings across the restart: a resumed run
    /// must not silently switch to the default step ceiling.
    #[test]
    fn the_loop_config_survives_in_the_checkpoint() {
        let config = LoopConfig {
            max_steps: 7,
            settle_delay_ms: 3,
            verify_after_each: false,
            ..Default::default()
        };
        let cp = Checkpoint::new("r1", "g", &config);
        let back = cp.loop_config();
        assert_eq!(back.max_steps, 7);
        assert_eq!(back.settle_delay_ms, 3);
        assert!(!back.verify_after_each);
    }

    /// A successful run leaves a file behind.
    ///
    /// This is the plan's "过程可查": the live state is in memory and the
    /// events went to whoever was connected, so without the file a *successful*
    /// run has nothing to read back. The conclusion used to delete it.
    #[tokio::test]
    async fn concluding_a_run_keeps_its_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Arc::new(RunStore::with_dir(dir.path().to_path_buf()));
        let checkpointer = RunCheckpointer::new(store.clone(), checkpoint("r1"));

        let mut spent = RunBudget::new(crate::config::RunBudgetConfig::default());
        spent.record_call().expect("one call");
        spent.record_tokens(120, 30);

        checkpointer.conclude(StopReason::Done, &spent).await;

        let kept = store
            .load("r1")
            .await
            .expect("a finished run keeps its file");
        assert_eq!(kept.stop_reason, Some(StopReason::Done));
        assert_eq!(kept.tokens_in, 120);
        assert_eq!(kept.tokens_out, 30);
        assert!(!kept.is_resumable(), "but it is not offered as a resume");
        assert!(!kept.is_interrupted());
    }

    /// Pruning removes an old conclusion and nothing else: a resumable run is
    /// an offer that does not expire, and an unreadable file is evidence.
    #[tokio::test]
    async fn pruning_removes_only_old_conclusions() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = RunStore::with_dir(dir.path().to_path_buf());

        let old_done = |id: &str| {
            let mut cp = checkpoint(id);
            cp.stop_reason = Some(StopReason::Done);
            cp.updated_at = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
            cp
        };
        let old_open = |id: &str| {
            let mut cp = checkpoint(id);
            cp.updated_at = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
            cp
        };
        let mut recent_done = checkpoint("recent");
        recent_done.stop_reason = Some(StopReason::Done);

        store.save(&old_done("old-done")).await.expect("save");
        store.save(&old_open("old-open")).await.expect("save");
        store.save(&recent_done).await.expect("save");
        std::fs::write(dir.path().join("corrupt.json"), b"not json").expect("write corrupt");

        let removed = store
            .prune_finished(std::time::Duration::from_secs(7 * 24 * 3600))
            .await;
        assert_eq!(removed, 1, "only the old conclusion went");

        assert!(store.load("old-done").await.is_none(), "the old conclusion is gone");
        assert!(store.load("old-open").await.is_some(), "a resumable run never expires");
        assert!(store.load("recent").await.is_some(), "a recent conclusion is kept");
        assert!(
            dir.path().join("corrupt.json").exists(),
            "an unreadable file is evidence, not rubbish"
        );
    }
}
