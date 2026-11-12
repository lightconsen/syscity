//! Cron model types: jobs, schedules, execution targets, and delivery modes.

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use cron::Schedule as CronSchedule;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tracing::{debug, warn};

/// Delivery event emitted for `DeliveryMode::Announce` jobs.
///
/// The gateway (or any consumer) receives these on the channel returned by
/// `CronScheduler::with_announce_tx` and forwards them to the
/// appropriate messaging channel (Discord, Telegram, WhatsApp, etc.).
#[derive(Debug, Clone)]
pub struct AnnounceDelivery {
    /// Target channel name (e.g. `"discord"`, `"telegram"`)
    pub channel: String,
    /// Recipient / room identifier (e.g. channel ID or user handle)
    pub to: String,
    /// Message content to deliver
    pub message: String,
}

/// Execution target - what to run
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ExecutionTarget {
    /// Execute shell command
    Shell { command: String },
    /// Execute via AI agent
    Agent {
        agent_id: Option<String>,
        prompt: String,
        context: Option<String>,
    },
}

impl ExecutionTarget {
    /// Create a shell execution target
    pub fn shell(command: impl Into<String>) -> Self {
        Self::Shell { command: command.into() }
    }

    /// Create an agent execution target
    pub fn agent(prompt: impl Into<String>) -> Self {
        Self::Agent {
            agent_id: None,
            prompt: prompt.into(),
            context: None,
        }
    }

    /// Create an agent execution target with specific agent
    pub fn agent_with_id(agent_id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self::Agent {
            agent_id: Some(agent_id.into()),
            prompt: prompt.into(),
            context: None,
        }
    }
}

/// Session target - where to execute
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SessionTarget {
    /// Run in the scheduler's shared cron session (`cron:shared`), which keeps
    /// conversation context across runs. Accepts the retired `"main"` spelling
    /// so job files written before the rename still load.
    #[serde(alias = "main")]
    Shared,
    /// Run in isolated session (clean state: cron:{job_id})
    #[default]
    Isolated,
}

/// Delivery mode for job results
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum DeliveryMode {
    /// No delivery (fire-and-forget)
    None,
    /// Send to messaging channel
    Announce { channel: String, to: String },
    /// POST to webhook URL
    Webhook {
        url: String,
        headers: HashMap<String, String>,
    },
}

/// Schedule types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Schedule {
    /// Run at a specific time (one-shot)
    At { timestamp: DateTime<Utc> },
    /// Run every N seconds
    Every {
        interval: Duration,
        anchor: Option<DateTime<Utc>>,
    },
    /// Cron expression
    ///
    /// **Note on the `timezone` field**: cron expressions are currently
    /// always evaluated in UTC. The `timezone` field is preserved for
    /// forward compatibility but has no effect today; a warning is
    /// emitted at job registration time when a non-`None` value is
    /// supplied so the limitation is visible. Schedules that need local
    /// time must convert externally for now.
    Cron {
        expression: String,
        /// **Currently unsupported** — always evaluated as UTC. Setting
        /// this field logs a one-time warning at job registration but
        /// otherwise has no effect on schedule resolution.
        timezone: Option<String>,
        stagger_ms: Option<u64>,
    },
}

impl Schedule {
    /// Calculate the next run time after `from`
    pub fn next_run(&self, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Schedule::At { timestamp } => {
                if *timestamp > from {
                    Some(*timestamp)
                } else {
                    None
                }
            }
            Schedule::Every { interval, anchor } => {
                let interval_secs = interval.as_secs();
                if interval_secs == 0 {
                    warn!(
                        "Schedule::Every interval is zero — job will never run (interval must be \
                         >= 1 second)"
                    );
                    return None;
                }

                let anchor = anchor.unwrap_or(from);

                if from < anchor {
                    Some(anchor)
                } else {
                    let elapsed = from.signed_duration_since(anchor);
                    let interval_i64 = interval_secs as i64;
                    let periods = elapsed
                        .num_seconds()
                        .checked_div(interval_i64)?
                        .checked_add(1)?;
                    let offset_secs = periods.checked_mul(interval_i64)?;
                    Some(anchor + ChronoDuration::seconds(offset_secs))
                }
            }
            Schedule::Cron {
                expression,
                timezone: _,
                stagger_ms,
            } => {
                // Parse cron expression
                // The cron crate v0.14 expects 6 fields (with seconds), so we need to
                // convert 5-field expressions to 6-field by prepending "0" for seconds
                let normalized = if expression.split_whitespace().count() == 5 {
                    format!("0 {}", expression.trim())
                } else {
                    expression.clone()
                };

                let schedule = match CronSchedule::from_str(&normalized) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(
                            "invalid cron expression {:?}: {} — job will never run",
                            expression, e
                        );
                        return None;
                    }
                };

                // Get next occurrence
                let next = schedule.upcoming(Utc).next()?;

                // Add stagger if configured. `% 0` panics, so we treat
                // `Some(0)` the same as `None` — no jitter.
                match stagger_ms {
                    Some(stagger) if *stagger > 0 => {
                        let jitter = rand::random::<u64>() % stagger;
                        Some(next + ChronoDuration::milliseconds(jitter as i64))
                    }
                    _ => Some(next),
                }
            }
        }
    }

    /// Check if this is a one-shot schedule that should be deleted after
    /// execution
    pub fn is_one_shot(&self) -> bool {
        matches!(self, Schedule::At { .. })
    }

    /// Emit one-shot warnings for partially-supported schedule fields.
    ///
    /// Called once per job at registration / load time (Add command and
    /// `load_jobs`) so that contract violations don't disappear into a
    /// silent loop. Currently surfaces:
    ///
    /// - `Schedule::Cron { timezone: Some(_) }` — field ignored, always
    ///   evaluated in UTC.
    pub fn warn_unsupported_fields(&self, job_name: &str) {
        if let Schedule::Cron { timezone: Some(tz), .. } = self {
            warn!(
                "cron job '{}' specifies timezone={:?} which is currently unsupported — schedule \
                 will be evaluated in UTC",
                job_name, tz
            );
        }
    }
}

/// Backoff strategy for retries
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum BackoffStrategy {
    /// Fixed delay between retries
    Fixed,
    /// Exponential backoff
    Exponential,
}

/// Retry configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub backoff: BackoffStrategy,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            backoff: BackoffStrategy::Exponential,
        }
    }
}

impl RetryConfig {
    /// Calculate delay for a specific retry attempt
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let base_delay_secs = match self.backoff {
            BackoffStrategy::Fixed => 30,
            BackoffStrategy::Exponential => {
                // Tiered exponential: 30s, 1m, 5m, 15m, 1h
                match attempt {
                    0 => 30,
                    1 => 60,
                    2 => 300,
                    3 => 900,
                    _ => 3600,
                }
            }
        };
        Duration::from_secs(base_delay_secs)
    }
}

/// Job execution state
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JobState {
    /// When the job is scheduled to run next
    pub next_run_at: Option<DateTime<Utc>>,
    /// When the job last ran
    pub last_run_at: Option<DateTime<Utc>>,
    /// When the job started running (if currently running)
    pub running_at_ms: Option<i64>,
    /// Total execution count
    pub run_count: u32,
    /// Last error message
    pub last_error: Option<String>,
    /// Consecutive error count
    pub consecutive_errors: u32,
}

/// Wake mode for cron-triggered heartbeat
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum WakeMode {
    /// No wake — run cron job normally
    #[default]
    None,
    /// Wake the heartbeat runner immediately after job execution
    HeartbeatWake,
}

impl WakeMode {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// A cron job
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    pub schedule: Schedule,
    pub target: ExecutionTarget,
    pub session: SessionTarget,
    pub delivery: DeliveryMode,
    pub retry: RetryConfig,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub state: JobState,
    /// Wake mode — when set to `HeartbeatWake`, sends a wake request to the
    /// heartbeat runner after the job completes.
    #[serde(default, skip_serializing_if = "WakeMode::is_none")]
    pub wake_mode: WakeMode,
}

impl CronJob {
    /// Create a new cron job
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        schedule: Schedule,
        target: ExecutionTarget,
    ) -> Self {
        let now = Utc::now();
        let next_run = schedule.next_run(now);

        Self {
            id: id.into(),
            name: name.into(),
            schedule,
            target,
            session: SessionTarget::default(),
            delivery: DeliveryMode::None,
            retry: RetryConfig::default(),
            enabled: true,
            created_at: now,
            state: JobState {
                next_run_at: next_run,
                ..Default::default()
            },
            wake_mode: WakeMode::None,
        }
    }

    /// Set the wake mode for heartbeat integration
    pub fn with_wake_mode(mut self, wake_mode: WakeMode) -> Self {
        self.wake_mode = wake_mode;
        self
    }

    /// Set the delivery mode
    pub fn with_delivery(mut self, delivery: DeliveryMode) -> Self {
        self.delivery = delivery;
        self
    }

    /// Set the session target
    pub fn with_session(mut self, session: SessionTarget) -> Self {
        self.session = session;
        self
    }

    /// Set retry configuration
    pub fn with_retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Check if the job should run at the given time
    pub fn should_run(&self, now: DateTime<Utc>) -> bool {
        if !self.enabled {
            return false;
        }

        if self.state.running_at_ms.is_some() {
            return false;
        }

        match self.state.next_run_at {
            Some(next) => now >= next,
            None => true,
        }
    }

    /// Update the next run time based on schedule
    pub fn update_next_run(&mut self, after: DateTime<Utc>) {
        self.state.next_run_at = self.schedule.next_run(after);
        if let Some(next) = self.state.next_run_at {
            debug!("Job {} next run at {}", self.name, next);
        }
    }
}

/// Run status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Ok,
    Error,
}

/// Delivery status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Delivered,
    Failed(String),
}

/// Run log entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunLogEntry {
    pub run_id: String,
    pub job_id: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub status: RunStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub delivery_status: Option<DeliveryStatus>,
}

/// Commands for the scheduler
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum CronCommand {
    Add(CronJob),
    Remove(String),
    SetEnabled(String, bool),
    Trigger(String),
    GetNextRun(String, oneshot::Sender<Option<DateTime<Utc>>>),
    ListJobs(oneshot::Sender<Vec<CronJob>>),
    GetJob(String, oneshot::Sender<Option<CronJob>>),
}
