//! Advanced Cron Scheduler for Syscity.
//!
//! Production-grade scheduler supporting AI agent execution, multi-channel
//! delivery, and enterprise reliability features.
//!
//! The implementation is split across sibling submodules: [`types`] holds the
//! model types, [`scheduler`] the timer loop and command handling, [`executor`]
//! job execution, and [`persistence`] the job store.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex as TokioMutex, RwLock};
use tokio::task::{AbortHandle, JoinHandle};
use tracing::{debug, error, info, warn};

use crate::agent::Agent;
use crate::channels::IncomingMessage;
use crate::error::{Result, SyscityError};

mod executor;
mod persistence;
mod scheduler;
mod types;

pub use types::{
    AnnounceDelivery, BackoffStrategy, CronCommand, CronJob, DeliveryMode, DeliveryStatus,
    ExecutionTarget, JobState, RetryConfig, RunLogEntry, RunStatus, Schedule, SessionTarget,
    WakeMode,
};

/// A full schedule snapshot for a platform wake bridge (§4.3): every enabled
/// job's `(job_id, next_run_at_ms)`. Kept as a named alias so the scheduler
/// field / method signatures stay clippy-`type_complexity`-clean.
pub type ScheduleChangeSnapshot = Vec<(String, Option<i64>)>;

/// Maximum timer delay - wake at least once per minute to check for schedule
/// changes
const MAX_TIMER_DELAY_MS: u64 = 60_000;
/// Minimum delay between timer fires to prevent tight loops
const MIN_REFIRE_GAP_MS: u64 = 2_000;
/// Hard upper bound on a single shell command's execution time.
/// A hung process otherwise keeps `running_at_ms` set forever, making
/// the job permanently un-runnable until the scheduler restarts.
const SHELL_EXEC_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Hard upper bound on a single agent task's execution time.
const AGENT_EXEC_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Hard upper bound on the bytes captured from a shell job's stdout or
/// stderr. Excess output is discarded (the child eventually blocks on
/// its pipe and is reaped by `kill_on_drop` when the outer timeout
/// fires). 1 MiB is enough to capture any reasonable error message
/// without letting a misconfigured `cat /dev/urandom` OOM the gateway.
const MAX_SHELL_OUTPUT_BYTES: usize = 1024 * 1024;

/// Maximum size for the runs.jsonl run-history file before new entries
/// are silently dropped. Without this cap a long-running scheduler
/// creates an unbounded file that eventually fills the disk.
/// 10 MiB holds roughly 20k–50k run entries depending on payload size.
const MAX_RUN_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Runtime invariant checks owned by the cron scheduler (registered via
/// `core::invariants::register_builtins`, surfaced through `syscity
/// invariants`).
pub(crate) fn cron_invariant_checks() -> Vec<crate::core::invariants::Invariant> {
    use crate::core::invariants::{Invariant, SKIP_PREFIX};

    vec![Invariant {
        id: "cron/run_log_bounded",
        module: "cron",
        description: "the run-history log stays within its retention cap",
        check: || {
            Box::pin(async move {
                // Mirrors the store path wired in gateway/init/services.rs.
                let log_path = crate::dirs::cron_dir()
                    .join("jobs.json")
                    .with_extension("runs.jsonl");
                let meta = match tokio::fs::metadata(&log_path).await {
                    Ok(meta) => meta,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Err(format!(
                            "{SKIP_PREFIX}no run history at {} (no job has completed yet)",
                            log_path.display()
                        ));
                    }
                    Err(e) => return Err(format!("cannot stat {}: {e}", log_path.display())),
                };
                let size = meta.len();
                if size > MAX_RUN_LOG_BYTES {
                    return Err(format!(
                        "run history {} is {} bytes, above the {}-byte retention cap — \
                         the scheduler trims on write, so manual growth means it is not running",
                        log_path.display(),
                        size,
                        MAX_RUN_LOG_BYTES
                    ));
                }
                Ok(())
            })
        },
    }]
}

/// Advanced cron scheduler with single global timer
///
/// # Concurrency model
///
/// Writes are serialised through the `command_tx` actor channel: only
/// `handle_command` mutates the `jobs` map. Reads, however, bypass the
/// actor and acquire the `RwLock` directly (see `calculate_next_wake_ms`
/// and `run_due_jobs`). This is intentional — the timer needs lock-step
/// access to the schedule without round-tripping through the command
/// queue. Do **not** add reader paths that assume actor serialisation;
/// the only ordering guarantee is "writes are linearised, reads see a
/// consistent snapshot taken under a read lock".
pub struct CronScheduler {
    jobs: Arc<RwLock<HashMap<String, CronJob>>>,
    command_tx: mpsc::Sender<CronCommand>,
    /// Broadcast so both the command-handler and timer tasks can subscribe.
    shutdown_tx: Option<broadcast::Sender<()>>,
    /// JoinHandles of the inner background tasks (command handler, timer).
    /// Tracked so `shutdown()` can await/abort them; the gateway-level
    /// `TaskRegistry` only tracks the outer `start()` wrapper task.
    inner_handles: Vec<JoinHandle<()>>,
    /// Shared agent reference — wrapping in `Arc<RwLock<…>>` lets
    /// `set_agent()` update the running scheduler's agent without
    /// restarting any background tasks.
    agent: Arc<RwLock<Option<Arc<Agent>>>>,
    store_path: Option<PathBuf>,
    /// Optional sender for Announce-mode delivery events.
    announce_tx: Option<mpsc::Sender<AnnounceDelivery>>,
    /// Optional sender for heartbeat wake requests.
    /// When a cron job has `wake_mode: HeartbeatWake`, a wake request is sent
    /// here.
    heartbeat_wake_tx: Option<mpsc::Sender<crate::heartbeat::WakeRequest>>,
    /// Optional sender for schedule-change notifications (§4.3).
    ///
    /// When set, an updated snapshot of `(job_id, next_run_at_ms)` is sent
    /// after any Add / Remove / SetEnabled (after persist + rearm) and once
    /// after startup `load_jobs`. The gateway forwards it to a platform wake
    /// bridge (e.g. WorkManager on Android) so due jobs can nudge the user
    /// even while the app is backgrounded. Scheduler stays device-agnostic —
    /// this is a plain typed channel; `None` (desktop) means no-op.
    schedule_change_tx: Option<mpsc::Sender<ScheduleChangeSnapshot>>,
    /// Notify the timer to re-calculate next wake time
    rearm_notify: Arc<tokio::sync::Notify>,
    /// Abort handles of in-flight job tasks, each tagged with the job
    /// id that spawned it.
    ///
    /// Two paths register here:
    ///
    /// - `CronCommand::Trigger` spawns `execute_job` as a detached task so the
    ///   command actor is not blocked for the full job duration.
    /// - `execute_job` spawns the agent inner future (so timeout aborts
    ///   propagate to the next `.await` inside the agent).
    ///
    /// Each push first reaps finished handles via `push_inflight`, so the
    /// vector stays bounded by the count of currently in-flight tasks
    /// rather than growing once per job execution over the scheduler's
    /// lifetime. `CronCommand::Remove` calls `abort_job` to cancel any
    /// in-flight execution that belongs to the job being deleted, so the
    /// removed job stops burning CPU writing back to a record that has
    /// just been erased. On `shutdown()` we abort every remaining entry
    /// so nothing survives the scheduler.
    inflight: Arc<TokioMutex<Vec<(String, AbortHandle)>>>,
}

impl std::fmt::Debug for CronScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CronScheduler")
            .field("store_path", &self.store_path)
            .field("has_announce_tx", &self.announce_tx.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    #[test]
    fn test_schedule_next_run_at() {
        let now = Utc::now();
        let future = now + ChronoDuration::hours(1);

        let schedule = Schedule::At { timestamp: future };
        assert_eq!(schedule.next_run(now), Some(future));

        // Past time returns None
        let past = now - ChronoDuration::hours(1);
        let schedule = Schedule::At { timestamp: past };
        assert_eq!(schedule.next_run(now), None);
    }

    #[test]
    fn test_schedule_next_run_every() {
        let now = Utc::now();
        let interval = Duration::from_secs(3600); // 1 hour

        let schedule = Schedule::Every { interval, anchor: None };

        let next = schedule.next_run(now);
        assert!(next.is_some());

        // Should be about 1 hour from now
        let diff = next.unwrap() - now;
        assert!(diff.num_seconds() >= 3600);
    }

    #[test]
    fn test_schedule_next_run_every_zero_interval_returns_none() {
        // Regression: previously panicked with divide-by-zero
        let schedule = Schedule::Every {
            interval: Duration::ZERO,
            anchor: None,
        };
        assert_eq!(schedule.next_run(Utc::now()), None);
    }

    #[test]
    fn test_schedule_next_run_every_subsecond_interval_returns_none() {
        // Sub-second intervals truncate to 0 seconds — also rejected
        let schedule = Schedule::Every {
            interval: Duration::from_millis(500),
            anchor: None,
        };
        assert_eq!(schedule.next_run(Utc::now()), None);
    }

    #[test]
    fn test_schedule_next_run_cron_invalid_returns_none() {
        // Regression: invalid cron expression must not panic and must not
        // silently succeed — it returns None so the scheduler skips the job.
        let schedule = Schedule::Cron {
            expression: "not a cron expression".to_string(),
            timezone: None,
            stagger_ms: None,
        };
        assert_eq!(schedule.next_run(Utc::now()), None);
    }

    #[test]
    fn test_execution_target_creation() {
        let shell = ExecutionTarget::shell("echo hello");
        assert!(matches!(shell, ExecutionTarget::Shell { command } if command == "echo hello"));

        let agent = ExecutionTarget::agent("summarize");
        assert!(matches!(agent, ExecutionTarget::Agent { prompt, .. } if prompt == "summarize"));
    }

    #[test]
    fn test_backoff_delay() {
        let retry = RetryConfig {
            max_retries: 5,
            backoff: BackoffStrategy::Exponential,
        };

        assert_eq!(retry.delay_for_attempt(0).as_secs(), 30);
        assert_eq!(retry.delay_for_attempt(1).as_secs(), 60);
        assert_eq!(retry.delay_for_attempt(2).as_secs(), 300);
        assert_eq!(retry.delay_for_attempt(3).as_secs(), 900);
        assert_eq!(retry.delay_for_attempt(4).as_secs(), 3600);
        assert_eq!(retry.delay_for_attempt(10).as_secs(), 3600); // Capped at tier 4

        let fixed = RetryConfig {
            max_retries: 5,
            backoff: BackoffStrategy::Fixed,
        };
        assert_eq!(fixed.delay_for_attempt(0).as_secs(), 30);
        assert_eq!(fixed.delay_for_attempt(5).as_secs(), 30);
    }

    #[test]
    fn test_schedule_cron_expression() {
        let now = Utc::now();
        // Every minute expression (6 fields with seconds)
        let schedule = Schedule::Cron {
            expression: "0 * * * * *".to_string(),
            timezone: None,
            stagger_ms: None,
        };
        let next = schedule.next_run(now);
        assert!(next.is_some());
        let next = next.unwrap();
        assert!(next > now);
        // Should be within ~1 minute
        assert!((next - now).num_seconds() <= 61);
    }

    #[test]
    fn test_schedule_cron_5field_normalization() {
        let now = Utc::now();
        // 5-field cron (no seconds) should be normalized to 6-field
        let schedule = Schedule::Cron {
            expression: "* * * * *".to_string(),
            timezone: None,
            stagger_ms: None,
        };
        let next = schedule.next_run(now);
        assert!(next.is_some());
        assert!(next.unwrap() > now);
    }

    #[test]
    fn test_schedule_is_one_shot() {
        let future = Utc::now() + ChronoDuration::hours(1);
        let at = Schedule::At { timestamp: future };
        assert!(at.is_one_shot());

        let every = Schedule::Every {
            interval: Duration::from_secs(60),
            anchor: None,
        };
        assert!(!every.is_one_shot());

        let cron = Schedule::Cron {
            expression: "0 * * * * *".to_string(),
            timezone: None,
            stagger_ms: None,
        };
        assert!(!cron.is_one_shot());
    }

    #[test]
    fn test_execution_target_agent_with_id() {
        let target = ExecutionTarget::agent_with_id("agent-1", "do something");
        assert!(matches!(
            target,
            ExecutionTarget::Agent {
                agent_id: Some(id),
                prompt,
                context: None,
            } if id == "agent-1" && prompt == "do something"
        ));
    }

    #[test]
    fn test_session_target_default() {
        assert_eq!(SessionTarget::default(), SessionTarget::Isolated);
    }

    /// Job files written before the shared-session rename store `"main"`;
    /// they must keep loading rather than fail the whole jobs file.
    #[test]
    fn test_session_target_reads_retired_main_spelling() {
        let parsed: SessionTarget = serde_json::from_str("\"main\"").unwrap();
        assert_eq!(parsed, SessionTarget::Shared);
        assert_eq!(serde_json::to_string(&SessionTarget::Shared).unwrap(), "\"shared\"");
    }

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.backoff, BackoffStrategy::Exponential);
    }

    #[test]
    fn test_job_state_default() {
        let state = JobState::default();
        assert!(state.next_run_at.is_none());
        assert!(state.last_run_at.is_none());
        assert!(state.running_at_ms.is_none());
        assert_eq!(state.run_count, 0);
        assert!(state.last_error.is_none());
        assert_eq!(state.consecutive_errors, 0);
    }

    #[test]
    fn test_cron_job_new() {
        let future = Utc::now() + ChronoDuration::hours(1);
        let schedule = Schedule::At { timestamp: future };
        let target = ExecutionTarget::shell("echo hello");
        let job = CronJob::new("job-1", "Test Job", schedule.clone(), target);

        assert_eq!(job.id, "job-1");
        assert_eq!(job.name, "Test Job");
        assert_eq!(job.schedule, schedule);
        assert!(job.enabled);
        assert_eq!(job.session, SessionTarget::Isolated);
        assert!(matches!(job.delivery, DeliveryMode::None));
        assert_eq!(job.retry.max_retries, 3);
        assert!(job.state.next_run_at.is_some());
    }

    #[test]
    fn test_cron_job_builder_methods() {
        let future = Utc::now() + ChronoDuration::hours(1);
        let job = CronJob::new(
            "job-2",
            "Builder Test",
            Schedule::At { timestamp: future },
            ExecutionTarget::agent("test"),
        )
        .with_delivery(DeliveryMode::Announce {
            channel: "discord".to_string(),
            to: "#general".to_string(),
        })
        .with_session(SessionTarget::Shared)
        .with_retry(RetryConfig {
            max_retries: 5,
            backoff: BackoffStrategy::Fixed,
        });

        assert!(matches!(
            job.delivery,
            DeliveryMode::Announce { channel, to } if channel == "discord" && to == "#general"
        ));
        assert_eq!(job.session, SessionTarget::Shared);
        assert_eq!(job.retry.max_retries, 5);
        assert_eq!(job.retry.backoff, BackoffStrategy::Fixed);
    }

    #[test]
    fn test_cron_job_should_run() {
        let now = Utc::now();
        let past = now - ChronoDuration::minutes(5);
        let schedule = Schedule::At { timestamp: past };
        let target = ExecutionTarget::shell("echo");

        // Job with past schedule - should run because next_run is past and enabled
        let mut job = CronJob::new("j1", "Test", schedule, target);
        job.state.next_run_at = Some(past);
        assert!(job.should_run(now));

        // Disabled job should not run
        job.enabled = false;
        assert!(!job.should_run(now));
        job.enabled = true;

        // Running job should not run
        job.state.running_at_ms = Some(now.timestamp_millis());
        assert!(!job.should_run(now));
        job.state.running_at_ms = None;

        // Future job should not run yet
        let future = now + ChronoDuration::hours(1);
        job.state.next_run_at = Some(future);
        assert!(!job.should_run(now));

        // No next_run should still return true (legacy behavior)
        job.state.next_run_at = None;
        assert!(job.should_run(now));
    }

    #[test]
    fn test_cron_job_update_next_run() {
        let now = Utc::now();
        let interval = Duration::from_secs(3600);
        let schedule = Schedule::Every { interval, anchor: Some(now) };
        let mut job = CronJob::new("j2", "Interval", schedule, ExecutionTarget::shell("echo"));

        job.update_next_run(now);
        let next = job.state.next_run_at.unwrap();
        // Next run should be about 1 hour from anchor
        let diff = next - now;
        assert!(diff.num_seconds() >= 3600);
    }

    #[test]
    fn test_run_status_variants() {
        assert_eq!(RunStatus::Ok, RunStatus::Ok);
        assert_eq!(RunStatus::Error, RunStatus::Error);
        assert_ne!(RunStatus::Ok, RunStatus::Error);
    }

    #[test]
    fn test_delivery_status_variants() {
        assert_eq!(DeliveryStatus::Delivered, DeliveryStatus::Delivered);
        assert_eq!(
            DeliveryStatus::Failed("x".to_string()),
            DeliveryStatus::Failed("x".to_string())
        );
        assert_ne!(DeliveryStatus::Delivered, DeliveryStatus::Failed("x".to_string()));
    }

    #[test]
    fn test_announce_delivery_creation() {
        let ann = AnnounceDelivery {
            channel: "telegram".to_string(),
            to: "12345".to_string(),
            message: "hello".to_string(),
        };
        assert_eq!(ann.channel, "telegram");
        assert_eq!(ann.to, "12345");
        assert_eq!(ann.message, "hello");
    }

    #[test]
    fn test_cron_scheduler_new() {
        let (scheduler, rx) = CronScheduler::new();
        assert!(scheduler.store_path.is_none());
        assert!(scheduler.announce_tx.is_none());
        assert!(scheduler.schedule_change_tx.is_none());
        // rx should be open
        assert!(!rx.is_closed());
    }

    #[test]
    fn test_cron_scheduler_default() {
        let scheduler: CronScheduler = Default::default();
        assert!(scheduler.store_path.is_none());
    }

    #[tokio::test]
    async fn test_schedule_change_snapshot_on_add_remove_enabled() {
        let (mut scheduler, command_rx) = CronScheduler::new();
        let (schedule_change_tx, mut schedule_change_rx) =
            mpsc::channel::<Vec<(String, Option<i64>)>>(16);
        scheduler.set_schedule_change_tx(schedule_change_tx);
        scheduler.start(command_rx).await.unwrap();

        // Drain the initial (empty) snapshot emitted after load_jobs.
        let initial = tokio::time::timeout(Duration::from_millis(1000), schedule_change_rx.recv())
            .await
            .expect("initial snapshot timed out")
            .expect("channel closed");
        assert!(initial.is_empty(), "fresh scheduler should have no jobs");

        // Add an enabled job with a future next run → snapshot contains it.
        let future = Utc::now() + ChronoDuration::hours(2);
        let job = CronJob::new(
            "j1",
            "job-one",
            Schedule::At { timestamp: future },
            ExecutionTarget::shell("echo hi"),
        )
        .with_delivery(DeliveryMode::None);
        scheduler.add_job(job).await.unwrap();

        let snap = tokio::time::timeout(Duration::from_millis(1000), schedule_change_rx.recv())
            .await
            .expect("add snapshot timed out")
            .expect("channel closed");
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, "j1");
        let at_ms = snap[0].1.expect("next run should be set");
        assert!(at_ms > Utc::now().timestamp_millis());

        // Disable the job → snapshot drops it.
        scheduler.set_job_enabled("j1", false).await.unwrap();
        let snap2 = tokio::time::timeout(Duration::from_millis(1000), schedule_change_rx.recv())
            .await
            .expect("disable snapshot timed out")
            .expect("channel closed");
        assert!(snap2.is_empty(), "disabled job must not be in snapshot");

        // Remove the job → snapshot stays empty.
        scheduler.remove_job("j1").await.unwrap();
        let snap3 = tokio::time::timeout(Duration::from_millis(1000), schedule_change_rx.recv())
            .await
            .expect("remove snapshot timed out")
            .expect("channel closed");
        assert!(snap3.is_empty(), "removed job must not be in snapshot");

        scheduler.shutdown().await.unwrap();
    }

    #[test]
    fn test_cron_command_variants() {
        let job = CronJob::new(
            "id",
            "name",
            Schedule::At { timestamp: Utc::now() },
            ExecutionTarget::shell("echo"),
        );

        let add = CronCommand::Add(job.clone());
        assert!(matches!(add, CronCommand::Add(_)));

        let remove = CronCommand::Remove("id".to_string());
        assert!(matches!(remove, CronCommand::Remove(s) if s == "id"));

        let set_enabled = CronCommand::SetEnabled("id".to_string(), false);
        assert!(matches!(set_enabled, CronCommand::SetEnabled(s, false) if s == "id"));

        let trigger = CronCommand::Trigger("id".to_string());
        assert!(matches!(trigger, CronCommand::Trigger(s) if s == "id"));

        let (tx, _rx) = oneshot::channel();
        let get_next = CronCommand::GetNextRun("id".to_string(), tx);
        assert!(matches!(get_next, CronCommand::GetNextRun(s, _) if s == "id"));

        let (tx, _rx) = oneshot::channel();
        let list = CronCommand::ListJobs(tx);
        assert!(matches!(list, CronCommand::ListJobs(_)));

        let (tx, _rx) = oneshot::channel();
        let get = CronCommand::GetJob("id".to_string(), tx);
        assert!(matches!(get, CronCommand::GetJob(s, _) if s == "id"));
    }

    #[tokio::test]
    async fn test_say_hi_every_2_min_job() {
        let cron_dir = crate::dirs::cron_dir();
        let store_path = cron_dir.join("test-say-hi.json");
        let runs_path = cron_dir.join("test-say-hi.runs.jsonl");

        // Clean up any previous test artifacts
        let _ = tokio::fs::remove_file(&store_path).await;
        let _ = tokio::fs::remove_file(&runs_path).await;

        // Create and start scheduler
        let (mut scheduler, command_rx) = CronScheduler::new();
        scheduler.store_path = Some(store_path.clone());
        scheduler.start(command_rx).await.unwrap();

        // Create the "say-hi-every-2-min" job
        let job = CronJob::new(
            "say-hi-001",
            "say-hi-every-2-min",
            Schedule::Cron {
                expression: "*/2 * * * *".to_string(),
                timezone: None,
                stagger_ms: None,
            },
            ExecutionTarget::Shell {
                command: "echo 'hi from cron'".to_string(),
            },
        )
        .with_delivery(DeliveryMode::None);

        scheduler.add_job(job).await.unwrap();

        // Verify job is in scheduler
        let jobs = scheduler.list_jobs().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "say-hi-every-2-min");
        assert_eq!(jobs[0].id, "say-hi-001");
        assert!(
            matches!(jobs[0].target, ExecutionTarget::Shell { ref command } if command == "echo 'hi from cron'")
        );

        // Verify persistence file written
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(store_path.exists(), "jobs store file should exist");

        let store_content = tokio::fs::read_to_string(&store_path).await.unwrap();
        assert!(store_content.contains("say-hi-every-2-min"));
        assert!(store_content.contains("*/2 * * * *"));

        // Trigger immediate execution
        scheduler.trigger_job("say-hi-001").await.unwrap();

        // Wait for execution to complete
        tokio::time::sleep(Duration::from_millis(800)).await;

        // Verify run log
        assert!(runs_path.exists(), "runs log file should exist after execution");
        let runs_content = tokio::fs::read_to_string(&runs_path).await.unwrap();
        assert!(!runs_content.is_empty(), "runs log should contain at least one entry");
        assert!(runs_content.contains("say-hi-001"), "run log should reference job id");

        // Verify job state updated
        let jobs_after = scheduler.list_jobs().await;
        assert_eq!(jobs_after[0].state.run_count, 1);
        assert!(jobs_after[0].state.last_run_at.is_some());
        assert!(jobs_after[0].state.last_error.is_none());

        // Cleanup
        let _ = tokio::fs::remove_file(&store_path).await;
        let _ = tokio::fs::remove_file(&runs_path).await;

        scheduler.shutdown().await.unwrap();
    }

    #[test]
    fn test_wake_mode_default() {
        assert_eq!(WakeMode::default(), WakeMode::None);
    }

    #[test]
    fn test_wake_mode_is_none() {
        assert!(WakeMode::None.is_none());
        assert!(!WakeMode::HeartbeatWake.is_none());
    }

    #[test]
    fn test_wake_mode_serialize_none_skipped() {
        let job = CronJob::new(
            "test",
            "Test",
            Schedule::At { timestamp: Utc::now() },
            ExecutionTarget::shell("echo"),
        );
        let json = serde_json::to_string(&job).unwrap();
        // WakeMode::None should be skipped due to skip_serializing_if
        assert!(!json.contains("wake_mode"));
    }

    #[test]
    fn test_wake_mode_serialize_heartbeat_wake() {
        let job = CronJob::new(
            "test",
            "Test",
            Schedule::At { timestamp: Utc::now() },
            ExecutionTarget::shell("echo"),
        )
        .with_wake_mode(WakeMode::HeartbeatWake);
        let json = serde_json::to_string(&job).unwrap();
        assert!(json.contains("\"wake_mode\":\"heartbeat_wake\""));
    }

    #[test]
    fn test_wake_mode_roundtrip() {
        let job = CronJob::new(
            "test",
            "Test",
            Schedule::At { timestamp: Utc::now() },
            ExecutionTarget::agent("check status"),
        )
        .with_wake_mode(WakeMode::HeartbeatWake);
        let json = serde_json::to_string(&job).unwrap();
        let deserialized: CronJob = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.wake_mode, WakeMode::HeartbeatWake);
        assert_eq!(deserialized.id, job.id);
        // Also verify the agent target is preserved
        assert!(matches!(deserialized.target, ExecutionTarget::Agent { .. }));
    }

    // ── Negative tests: persistence error handling ──────────────────────────

    #[tokio::test]
    async fn test_save_jobs_fails_on_readonly_path() {
        let jobs: Arc<RwLock<HashMap<String, CronJob>>> = Arc::new(RwLock::new(HashMap::new()));
        let dir = tempfile::tempdir().expect("tempdir");
        let subdir = dir.path().join("locked");
        std::fs::create_dir(&subdir).unwrap();
        let path = subdir.join("cron_jobs.json");

        // Make the *parent directory* read-only so save_jobs cannot create the
        // tmp file (atomic write writes to <path>.tmp first, then renames).
        // We use the directory rather than just the file because rename(2)
        // ignores the destination file's permissions and only checks the
        // directory's write bit.
        let mut perms = std::fs::metadata(&subdir).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&subdir, perms).unwrap();

        let result = CronScheduler::save_jobs(&jobs, &path).await;

        // Restore write perms so the tempdir can be cleaned up.
        let mut perms = std::fs::metadata(&subdir).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        let _ = std::fs::set_permissions(&subdir, perms);

        assert!(result.is_err(), "save_jobs should fail when the target directory is read-only");
    }

    /// M3: save_jobs writes via a tmp file + rename, and on success leaves no
    /// `.tmp` file behind in the target directory.
    #[tokio::test]
    async fn test_save_jobs_is_atomic_and_leaves_no_tmp_file() {
        let mut map: HashMap<String, CronJob> = HashMap::new();
        let future = Utc::now() + ChronoDuration::hours(1);
        let job = CronJob::new(
            "atomic-job",
            "Atomic Test",
            Schedule::At { timestamp: future },
            ExecutionTarget::shell("echo hi"),
        );
        map.insert(job.id.clone(), job);
        let jobs: Arc<RwLock<HashMap<String, CronJob>>> = Arc::new(RwLock::new(map));

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cron_jobs.json");
        let tmp_path = dir.path().join("cron_jobs.json.tmp");

        CronScheduler::save_jobs(&jobs, &path).await.unwrap();

        assert!(path.exists(), "final jobs file should exist after save");
        assert!(!tmp_path.exists(), "tmp file should be renamed away, not left behind");

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("atomic-job"), "saved file should contain the job id");
    }

    #[tokio::test]
    async fn test_log_run_fails_on_readonly_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store_path = dir.path().join("cron_jobs.json");
        // log_run derives the log path as <store_path>.runs.jsonl
        let log_path = dir.path().join("cron_jobs.runs.jsonl");

        // Create the runs.jsonl file and make it read-only so the
        // OpenOptions::append inside log_run fails.
        tokio::fs::write(&log_path, b"").await.unwrap();
        let mut perms = std::fs::metadata(&log_path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&log_path, perms).unwrap();

        let now = Utc::now();
        let result = CronScheduler::log_run(
            "test-job",
            "test-run-id",
            now,
            now + ChronoDuration::seconds(1),
            Ok("output".to_string()),
            Some(DeliveryStatus::Delivered),
            &Some(store_path),
        )
        .await;
        assert!(result.is_err(), "log_run should fail when runs.jsonl is read-only");
    }

    #[tokio::test]
    async fn test_execute_job_persistence_failure_does_not_panic() {
        let jobs: Arc<RwLock<HashMap<String, CronJob>>> = Arc::new(RwLock::new(HashMap::new()));
        let agent: Arc<RwLock<Option<Arc<Agent>>>> = Arc::new(RwLock::new(None));

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cron_jobs.json");

        // Make the store path read-only so save_jobs + log_run fail.
        tokio::fs::write(&path, b"").await.unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();

        let job = CronJob::new(
            "test-job",
            "Test Job",
            Schedule::Every {
                interval: Duration::from_secs(3600),
                anchor: None,
            },
            ExecutionTarget::shell("echo hello"),
        );
        jobs.write().await.insert("test-job".to_string(), job);

        // Execute with force=true; persistence will fail but must not panic
        let inflight = Arc::new(TokioMutex::new(Vec::new()));
        CronScheduler::execute_job(
            executor::CronRuntime {
                jobs: &jobs,
                agent: &agent,
                store_path: &Some(path),
                announce_tx: &None,       // announce_tx
                heartbeat_wake_tx: &None, // heartbeat_wake_tx
                inflight: &inflight,
            },
            "test-job",
            true, // force
        )
        .await;

        // Job state must still be updated despite persistence failure
        let updated = jobs.read().await.get("test-job").cloned().unwrap();
        assert!(
            updated.state.running_at_ms.is_none(),
            "running_at_ms should be cleared after execution"
        );
        assert_eq!(updated.state.run_count, 1, "run_count should be incremented after execution");
        assert!(updated.state.last_error.is_none(), "no error for successful shell execution");
    }

    // ── Regression tests for H1 / H2 / M1 / M2 / M3 / M4 fixes ────────────

    /// M1: a force-trigger must NOT double-execute a job that is already
    /// running. Previously, `force=true` bypassed the running_at_ms check
    /// entirely and would overwrite `running_at_ms` with a fresh
    /// timestamp, leaving two concurrent execution paths.
    #[tokio::test]
    async fn test_force_trigger_respects_running_at_ms() {
        let mut map: HashMap<String, CronJob> = HashMap::new();
        let mut job = CronJob::new(
            "running-job",
            "Running",
            Schedule::At {
                timestamp: Utc::now() + ChronoDuration::hours(1),
            },
            ExecutionTarget::shell("echo hi"),
        );
        // Simulate the job already being in flight.
        job.state.running_at_ms = Some(Utc::now().timestamp_millis());
        map.insert(job.id.clone(), job);
        let jobs: Arc<RwLock<HashMap<String, CronJob>>> = Arc::new(RwLock::new(map));
        let agent: Arc<RwLock<Option<Arc<Agent>>>> = Arc::new(RwLock::new(None));

        let before = jobs.read().await.get("running-job").unwrap().state.clone();

        // Force-trigger should be refused; run_count must not advance and
        // running_at_ms must not change.
        let inflight = Arc::new(TokioMutex::new(Vec::new()));
        CronScheduler::execute_job(
            executor::CronRuntime {
                jobs: &jobs,
                agent: &agent,
                store_path: &None,
                announce_tx: &None,
                heartbeat_wake_tx: &None,
                inflight: &inflight,
            },
            "running-job",
            true, // force
        )
        .await;

        let after = jobs.read().await.get("running-job").unwrap().state.clone();
        assert_eq!(before.running_at_ms, after.running_at_ms, "running_at_ms preserved");
        assert_eq!(
            before.run_count, after.run_count,
            "run_count must not advance on refused trigger"
        );
    }

    /// M4: shell stdout is bounded — a job that emits unbounded output
    /// must not load gigabytes into memory; the captured output should be
    /// at most MAX_SHELL_OUTPUT_BYTES.
    #[tokio::test]
    async fn test_execute_shell_caps_unbounded_stdout() {
        // `yes` produces an infinite stream of "y\n". With our cap the
        // process either blocks on its pipe (and is killed when this test
        // ends and the parent process drops the child) or completes
        // quickly when the OS pipe buffer fills. Either way, the output
        // we observe must not exceed the cap.
        //
        // Wrap in a timeout so a buggy implementation fails fast rather
        // than hanging the test suite.
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            CronScheduler::execute_shell("yes | head -c 5242880"), // 5 MiB
        )
        .await
        .expect("execute_shell should complete within timeout");

        let out = result.expect("shell command should succeed");
        assert!(
            out.len() <= MAX_SHELL_OUTPUT_BYTES,
            "captured stdout {} bytes must not exceed cap {}",
            out.len(),
            MAX_SHELL_OUTPUT_BYTES
        );
    }

    /// H2: the shell timeout path must reap the child process via
    /// `kill_on_drop`. We confirm correctness indirectly by verifying
    /// that timeouts actually return — without `kill_on_drop`, the child
    /// would keep the pipe FDs alive and `wait_with_output`'s drop
    /// could leak. A simpler invariant we can check: a long-running
    /// command, wrapped in a short timeout, must come back as a timeout
    /// error in well under SHELL_EXEC_TIMEOUT.
    #[tokio::test]
    async fn test_shell_timeout_returns_promptly() {
        // 100ms timeout against a 30s sleep — must return as Err quickly,
        // i.e. the timeout actually unblocks the future.
        let start = tokio::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            CronScheduler::execute_shell("sleep 30"),
        )
        .await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected timeout, got {:?}", result);
        assert!(
            elapsed < Duration::from_secs(2),
            "timeout should return promptly (within ~2s) but took {:?}",
            elapsed
        );
    }

    /// M2: a Cron schedule with a `timezone` field must emit a warning
    /// via `warn_unsupported_fields` so the limitation is visible. The
    /// warning is a side-effect we can't capture from inside the test,
    /// but we can at least verify the method does not panic for
    /// supported and unsupported variants.
    #[test]
    fn test_warn_unsupported_fields_does_not_panic() {
        let s1 = Schedule::Cron {
            expression: "0 * * * *".to_string(),
            timezone: Some("America/Los_Angeles".to_string()),
            stagger_ms: None,
        };
        s1.warn_unsupported_fields("test-job");

        let s2 = Schedule::Cron {
            expression: "0 * * * *".to_string(),
            timezone: None,
            stagger_ms: None,
        };
        s2.warn_unsupported_fields("test-job");

        let s3 = Schedule::At { timestamp: Utc::now() };
        s3.warn_unsupported_fields("test-job");
    }

    /// M3: run_due_jobs dispatches due jobs concurrently. We check this
    /// by registering N shell jobs that each sleep 500ms and verifying
    /// the whole batch finishes in under N * 500ms wall clock. Serial
    /// execution would take N * 500ms; concurrent execution finishes in
    /// roughly 500ms regardless of N.
    #[tokio::test]
    async fn test_run_due_jobs_executes_concurrently() {
        let mut map: HashMap<String, CronJob> = HashMap::new();
        let now = Utc::now();
        for i in 0..4 {
            let mut job = CronJob::new(
                format!("concurrent-{}", i),
                format!("Concurrent {}", i),
                Schedule::At {
                    timestamp: now + ChronoDuration::hours(1),
                },
                ExecutionTarget::shell("sleep 0.5"),
            );
            // Make should_run() true: due now, not yet running.
            job.state.next_run_at = Some(now - ChronoDuration::seconds(1));
            job.state.running_at_ms = None;
            map.insert(job.id.clone(), job);
        }
        let jobs: Arc<RwLock<HashMap<String, CronJob>>> = Arc::new(RwLock::new(map));
        let agent: Arc<RwLock<Option<Arc<Agent>>>> = Arc::new(RwLock::new(None));

        let start = tokio::time::Instant::now();
        let inflight = Arc::new(TokioMutex::new(Vec::new()));
        CronScheduler::run_due_jobs(&jobs, &agent, &None, &None, &None, &inflight).await;
        let elapsed = start.elapsed();

        // Serial: ~2.0s (4 × 500ms). Concurrent: ~0.5s. Use 1.5s as a
        // safe upper bound that fails serial but tolerates CI jitter.
        assert!(
            elapsed < Duration::from_millis(1500),
            "4 due jobs each sleeping 500ms must run concurrently; actual elapsed: {:?}",
            elapsed
        );

        // Sanity: all jobs ran (one-shot, so they are removed by
        // execute_job after running).
        let remaining = jobs.read().await;
        assert!(
            remaining.is_empty(),
            "all one-shot jobs should have been removed after execution; remaining: {:?}",
            remaining.keys().collect::<Vec<_>>()
        );
    }

    /// `% 0` panics. Schedule construction with `stagger_ms = Some(0)`
    /// must be treated as "no stagger" and not panic at next-run-time.
    #[test]
    fn test_schedule_cron_stagger_zero_does_not_panic() {
        let sched = Schedule::Cron {
            expression: "0 * * * * *".to_string(), // every minute on :00
            timezone: None,
            stagger_ms: Some(0),
        };
        // Without the guard, this would panic with "attempt to calculate
        // the remainder with a divisor of zero" inside `next_run`.
        let next = sched.next_run(Utc::now());
        assert!(next.is_some(), "stagger_ms=0 must still produce a next run");
    }

    /// Once `consecutive_errors` exceeds `max_retries`, the scheduler
    /// disables the job rather than continuing to retry forever.
    #[tokio::test]
    async fn test_job_disabled_after_max_retries_exhausted() {
        let mut map: HashMap<String, CronJob> = HashMap::new();
        let mut job = CronJob::new(
            "doomed",
            "Doomed",
            // Use a recurring schedule so the failure path retries
            // instead of being a one-shot.
            Schedule::Every {
                interval: Duration::from_secs(60),
                anchor: None,
            },
            ExecutionTarget::shell("exit 1"),
        );
        job.retry.max_retries = 2;
        // Simulate two failures already on the books — the next failed
        // run lands at consecutive_errors = 3, which exceeds max_retries.
        job.state.consecutive_errors = 2;
        map.insert(job.id.clone(), job);
        let jobs: Arc<RwLock<HashMap<String, CronJob>>> = Arc::new(RwLock::new(map));
        let agent: Arc<RwLock<Option<Arc<Agent>>>> = Arc::new(RwLock::new(None));
        let inflight = Arc::new(TokioMutex::new(Vec::new()));

        CronScheduler::execute_job(
            executor::CronRuntime {
                jobs: &jobs,
                agent: &agent,
                store_path: &None,
                announce_tx: &None,
                heartbeat_wake_tx: &None,
                inflight: &inflight,
            },
            "doomed",
            true, // force
        )
        .await;

        let after = jobs.read().await.get("doomed").cloned().unwrap();
        assert!(!after.enabled, "job must be disabled after exhausting retries");
        assert!(after.state.next_run_at.is_none(), "next_run_at cleared on disable");
        assert!(after.state.consecutive_errors > after.retry.max_retries);
    }

    /// `log_run` must record whatever delivery outcome was passed in —
    /// not synthesise a value from the execution result. This is the
    /// regression for the previous hard-coded `Delivered`/`Execution
    /// error` mapping that ignored real delivery failures.
    #[tokio::test]
    async fn test_log_run_records_supplied_delivery_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path().join("cron.json");

        let now = Utc::now();
        CronScheduler::log_run(
            "j",
            "r",
            now,
            now + ChronoDuration::seconds(1),
            Ok("output".to_string()),
            Some(DeliveryStatus::Failed("webhook 500".to_string())),
            &Some(store.clone()),
        )
        .await
        .expect("log_run");

        let log_path = store.with_extension("runs.jsonl");
        let line = tokio::fs::read_to_string(&log_path)
            .await
            .expect("read log");
        let entry: RunLogEntry = serde_json::from_str(line.trim()).expect("parse");
        assert_eq!(entry.status, RunStatus::Ok);
        assert_eq!(
            entry.delivery_status,
            Some(DeliveryStatus::Failed("webhook 500".to_string())),
            "delivery_status must reflect the caller-supplied outcome, not be synthesised from \
             execution result"
        );
    }

    /// `push_inflight` must reap finished abort handles instead of letting
    /// the in-flight vector grow once per spawn forever. Without pruning,
    /// a long-running scheduler accumulates one entry per job execution
    /// across its lifetime.
    #[tokio::test]
    async fn test_push_inflight_reaps_finished_handles() {
        let inflight = Arc::new(TokioMutex::new(Vec::new()));

        // Push 5 handles whose tasks have already completed.
        for _ in 0..5 {
            let handle = tokio::spawn(async {});
            // Let it run to completion.
            handle.await.expect("task ran");
            // Spawn a no-op specifically to get an already-finished
            // abort_handle. The simpler path: spawn, await, then
            // capture the AbortHandle from a fresh handle that we
            // know is done.
            let done = tokio::spawn(async {});
            done.await.expect("ran");
            let h = tokio::spawn(async {});
            let abort = h.abort_handle();
            h.await.expect("ran");
            // `abort` now refers to a finished task.
            CronScheduler::push_inflight(&inflight, "test-job".to_string(), abort).await;
        }

        // The vector contains 5 finished handles plus possibly the last
        // one we just pushed. Before the next push, all 5 should be
        // reaped, and then the new handle is appended — never more than
        // one finished tail + one live entry.
        let len_before_live = inflight.lock().await.len();
        assert!(
            len_before_live <= 5,
            "list should not have grown unboundedly: {}",
            len_before_live
        );

        // Now push a live, never-completing task and verify reap drops
        // the finished tail.
        let live = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let live_abort = live.abort_handle();
        CronScheduler::push_inflight(&inflight, "live-job".to_string(), live_abort).await;

        // After this push, all the previously-finished handles should
        // have been reaped, leaving only the live one.
        let final_len = inflight.lock().await.len();
        assert_eq!(
            final_len, 1,
            "push_inflight should retain only the unfinished live handle, got {}",
            final_len
        );

        // Cleanup
        live.abort();
    }
}
