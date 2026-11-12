//! `CronScheduler` job execution: shell and agent targets, result delivery,
//! and run logging.

use super::*;

/// Shared runtime handles threaded through cron command handling and job
/// execution (the scheduler state every command needs read/write access to).
pub(super) struct CronRuntime<'a> {
    pub jobs: &'a Arc<RwLock<HashMap<String, CronJob>>>,
    pub agent: &'a Arc<RwLock<Option<Arc<Agent>>>>,
    pub store_path: &'a Option<PathBuf>,
    pub announce_tx: &'a Option<mpsc::Sender<AnnounceDelivery>>,
    pub heartbeat_wake_tx: &'a Option<mpsc::Sender<crate::heartbeat::WakeRequest>>,
    pub inflight: &'a Arc<TokioMutex<Vec<(String, AbortHandle)>>>,
}

impl CronScheduler {
    /// Execute a job
    ///
    /// When `force` is true, the job runs regardless of `should_run` /
    /// `next_run_at`. Used by manual trigger (`Trigger` command).
    /// Timer-driven execution passes `false`.
    pub(super) async fn execute_job(rt: CronRuntime<'_>, job_id: &str, force: bool) {
        let CronRuntime {
            jobs,
            agent,
            store_path,
            announce_tx,
            heartbeat_wake_tx,
            inflight,
        } = rt;
        let job = {
            let mut jobs_lock = jobs.write().await;
            let job = match jobs_lock.get_mut(job_id) {
                Some(j) => j,
                None => {
                    warn!("Job {} not found for execution", job_id);
                    return;
                }
            };

            // Check if should run (skip when forced)
            let now = Utc::now();
            if !force && !job.should_run(now) {
                return;
            }

            // Even when `force=true`, refuse to double-execute a job that is
            // already running. Without this guard, two concurrent
            // `execute_job` calls would both clear and rewrite
            // `running_at_ms`, run the underlying work twice, and corrupt
            // `run_count` / `consecutive_errors`.
            if job.state.running_at_ms.is_some() {
                warn!(
                    "Trigger ignored: cron job '{}' is already running (running_at_ms={:?})",
                    job.name, job.state.running_at_ms
                );
                return;
            }

            // Mark as running
            job.state.running_at_ms = Some(now.timestamp_millis());
            job.clone()
        };

        info!("Executing job: {}", job.name);
        let run_id = uuid::Uuid::new_v4().to_string();
        let started_at = Utc::now();

        // Execute based on target type. Each path is wrapped in a hard
        // timeout — without it a hung process or stalled agent keeps
        // `running_at_ms` set forever, making the job permanently
        // un-runnable. The shell path relies on `kill_on_drop(true)` inside
        // `execute_shell` so the child process is reaped when the future
        // is cancelled. The agent path spawns into a separate task and
        // uses `abort_handle()` so that on timeout we propagate
        // cancellation to the next `.await` point inside the agent —
        // dropping the future alone would not.
        let result = match &job.target {
            ExecutionTarget::Shell { command } => {
                match tokio::time::timeout(SHELL_EXEC_TIMEOUT, Self::execute_shell(command)).await {
                    Ok(r) => r,
                    Err(_) => {
                        // The future is dropped here; `kill_on_drop` on
                        // the child Child handle inside execute_shell
                        // sends SIGKILL to the underlying `sh` process.
                        Err(SyscityError::Internal(format!(
                            "Shell command timed out after {:?}: '{}' (job={})",
                            SHELL_EXEC_TIMEOUT, command, job_id
                        )))
                    }
                }
            }
            ExecutionTarget::Agent { prompt, agent_id, .. } => {
                let agent_clone = {
                    let agent_guard = agent.read().await;
                    agent_guard.as_ref().map(Arc::clone)
                };
                if let Some(agent_ref) = agent_clone {
                    let job_clone = job.clone();
                    let prompt_owned = prompt.clone();
                    let agent_id_owned = agent_id.clone();
                    let handle = tokio::spawn(async move {
                        Self::execute_agent(
                            &agent_ref,
                            &job_clone,
                            &prompt_owned,
                            agent_id_owned.as_deref(),
                        )
                        .await
                    });
                    let abort_handle = handle.abort_handle();
                    // Register the agent inner spawn so `shutdown()` can
                    // abort it. Without this, the inner task would survive
                    // the scheduler.
                    Self::push_inflight(inflight, job_id.to_string(), abort_handle.clone()).await;
                    match tokio::time::timeout(AGENT_EXEC_TIMEOUT, handle).await {
                        Ok(Ok(r)) => r,
                        Ok(Err(e)) => Err(SyscityError::Internal(format!(
                            "Agent task join error for job '{}' (id={}): {}",
                            job.name, job_id, e
                        ))),
                        Err(_) => {
                            // Explicitly abort so cancellation actually
                            // reaches the agent at its next `.await`,
                            // rather than letting the task continue
                            // detached after we give up waiting.
                            abort_handle.abort();
                            Err(SyscityError::Internal(format!(
                                "Agent task timed out after {:?} for job '{}' (id={})",
                                AGENT_EXEC_TIMEOUT, job.name, job_id
                            )))
                        }
                    }
                } else {
                    Err(SyscityError::Internal(format!(
                        "No agent configured for cron job '{}' (id={})",
                        job.name, job_id
                    )))
                }
            }
        };

        let completed_at = Utc::now();

        // Update job state. We update everything that requires the write
        // lock here, but defer the actual delivery (which can `.await` for
        // up to ~90s on webhook retries) until after the lock is released.
        // Holding the write lock across delivery would serialise every
        // other scheduler operation (Add/Remove/Trigger/timer) behind one
        // slow HTTP call.
        let delivery_intent = {
            let mut jobs_lock = jobs.write().await;
            if let Some(j) = jobs_lock.get_mut(job_id) {
                j.state.running_at_ms = None;
                j.state.last_run_at = Some(completed_at);
                j.state.run_count += 1;

                // Apply side effects before building the delivery payload.
                // The payload construction is extracted to keep the
                // write-lock section focused on state mutation.
                match &result {
                    Ok(_) => {
                        j.state.last_error = None;
                        j.state.consecutive_errors = 0;
                        info!("Job '{}' completed successfully", j.name);
                    }
                    Err(e) => {
                        let error_msg = format!("{}", e);
                        j.state.last_error = Some(error_msg.clone());
                        j.state.consecutive_errors += 1;
                        error!("Job '{}' failed: {}", j.name, error_msg);
                    }
                }

                let delivery_payload =
                    Self::build_delivery_payload(&result, &j.name, &j.id, completed_at);

                // Capture what delivery needs so we can `.await` it outside
                // the lock. Cloning is cheap relative to the alternative of
                // freezing the whole scheduler for the duration of an HTTP
                // round-trip.
                let intent = if matches!(j.delivery, DeliveryMode::None) {
                    None
                } else {
                    let message = serde_json::to_string(&delivery_payload)
                        .unwrap_or_else(|_| delivery_payload.to_string());
                    Some((j.delivery.clone(), message, j.name.clone()))
                };

                // Update next run (or schedule retry on error).
                //
                // Note: one-shot (`Schedule::At`) jobs are removed below, so
                // there is no point computing a retry slot for them — the
                // job ceases to exist after this block.
                match &result {
                    Ok(_) => j.update_next_run(completed_at),
                    Err(_) if j.schedule.is_one_shot() => {
                        // One-shot: no retry, no next_run — the job is about
                        // to be removed.
                    }
                    Err(_) => {
                        if j.state.consecutive_errors <= j.retry.max_retries {
                            // delay_for_attempt is 0-indexed: attempt 0 → 30s,
                            // attempt 1 → 60s, etc. consecutive_errors is the
                            // count of failures so far (>=1 here), so subtract
                            // one to start the first retry at the 30s tier.
                            let attempt = j.state.consecutive_errors.saturating_sub(1);
                            let delay = j.retry.delay_for_attempt(attempt);
                            let retry_at = completed_at
                                + chrono::Duration::from_std(delay)
                                    .unwrap_or_else(|_| chrono::Duration::seconds(60));
                            warn!("Scheduling retry for job '{}' at {:?}", j.name, retry_at);
                            j.state.next_run_at = Some(retry_at);
                        } else {
                            // Max retries exhausted — disable the job so it
                            // stops burning resources on a doomed task.
                            // Operator must re-enable explicitly after
                            // fixing the underlying cause.
                            error!(
                                "Job '{}' disabled after {} consecutive failures (max_retries={})",
                                j.name, j.state.consecutive_errors, j.retry.max_retries
                            );
                            j.enabled = false;
                            j.state.next_run_at = None;
                        }
                    }
                }

                // Remove one-shot jobs after execution
                if j.schedule.is_one_shot() {
                    info!("Removing one-shot job: {}", j.name);
                    jobs_lock.remove(job_id);
                }

                intent
            } else {
                None
            }
        };

        // Deliver result OUTSIDE the write lock. The delivery channel can
        // be a webhook with up to ~30s × 3 retries; under the old
        // structure that would have frozen the whole scheduler. With the
        // lock released, concurrent Add/Remove/Trigger and the timer
        // continue to make progress.
        //
        // Capture the outcome so the run log can record whether delivery
        // actually succeeded — not just whether execution succeeded.
        let delivery_status = if let Some((delivery, message, job_name)) = delivery_intent {
            match Self::deliver_result(&delivery, &message, announce_tx).await {
                Ok(()) => Some(DeliveryStatus::Delivered),
                Err(e) => {
                    warn!("Delivery failed for job '{}': {}", job_name, e);
                    Some(DeliveryStatus::Failed(e.to_string()))
                }
            }
        } else {
            // DeliveryMode::None — there is no delivery to report on.
            None
        };

        // Persist
        if let Some(ref path) = store_path {
            if let Err(e) = Self::save_jobs(jobs, path).await {
                warn!("Failed to persist cron jobs after run: {e}");
            }
        }

        // Log the run
        if let Err(e) = Self::log_run(
            job_id,
            &run_id,
            started_at,
            completed_at,
            result,
            delivery_status,
            store_path,
        )
        .await
        {
            warn!("Failed to log cron run: {e}");
        }

        // Send heartbeat wake if configured and job succeeded
        if matches!(job.wake_mode, WakeMode::HeartbeatWake) {
            if let Some(ref tx) = heartbeat_wake_tx {
                let agent_id = match &job.target {
                    ExecutionTarget::Agent { agent_id, .. } => agent_id.clone().unwrap_or_default(),
                    _ => String::from("*"),
                };
                info!(
                    "Cron job '{}' completed with heartbeat wake — waking agent {}",
                    job.name, agent_id
                );
                // Use `try_send` rather than `send().await`: this hop is
                // strictly best-effort, and an `.await` here would stall
                // the job-completion path indefinitely if the heartbeat
                // channel is saturated. Both `Full` and `Closed` are
                // logged but otherwise ignored — the next cron tick
                // will get another chance.
                if let Err(e) = tx.try_send(crate::heartbeat::WakeRequest {
                    agent_id,
                    priority: crate::heartbeat::WakePriority::Action,
                    prompt: None,
                }) {
                    warn!("Cron job '{}' could not send heartbeat wake request: {}", job.name, e);
                }
            }
        }
    }

    /// Execute shell command
    ///
    /// Spawn is used (rather than `output()`) for two correctness reasons:
    ///
    /// 1. `kill_on_drop(true)` guarantees that if our caller's
    ///    `tokio::time::timeout` fires, the underlying `sh` process is
    ///    SIGKILL'd as the `Child` is dropped. Without this, the timeout only
    ///    stops *us* waiting; the child keeps running, holds FDs, and may spawn
    ///    even more work.
    /// 2. stdout/stderr are drained on background tasks but only the first
    ///    `MAX_SHELL_OUTPUT_BYTES` are retained. Bytes past the cap are read
    ///    and discarded so the child can keep writing without blocking on a
    ///    full pipe buffer (which would otherwise cause SIGPIPE / non-zero exit
    ///    on downstream tools in a pipeline, or hang the child indefinitely).
    ///    The retained buffer cannot OOM the gateway because it is capped.
    pub(super) async fn execute_shell(command: &str) -> Result<String> {
        use tokio::io::AsyncReadExt;

        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| SyscityError::Internal(format!("Failed to execute shell: {}", e)))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SyscityError::Internal("shell child has no stdout pipe".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| SyscityError::Internal("shell child has no stderr pipe".to_string()))?;

        /// Drain `reader` to EOF, returning at most `MAX_SHELL_OUTPUT_BYTES`
        /// of the head of the stream. Bytes past the cap are discarded but
        /// still read from the pipe so the child does not block on a full
        /// OS pipe buffer.
        async fn drain_capped<R: AsyncReadExt + Unpin>(
            mut reader: R,
            stream_name: &str,
        ) -> Vec<u8> {
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if buf.len() < MAX_SHELL_OUTPUT_BYTES {
                            let remaining = MAX_SHELL_OUTPUT_BYTES - buf.len();
                            let take = n.min(remaining);
                            buf.extend_from_slice(&chunk[..take]);
                        }
                        // Anything past the cap is intentionally discarded
                        // but the pipe has already been drained.
                    }
                    Err(e) => {
                        warn!("error reading cron shell {}: {}", stream_name, e);
                        break;
                    }
                }
            }
            buf
        }

        let stdout_task = tokio::spawn(drain_capped(stdout, "stdout"));
        let stderr_task = tokio::spawn(drain_capped(stderr, "stderr"));

        let status = child
            .wait()
            .await
            .map_err(|e| SyscityError::Internal(format!("Failed to wait on shell: {}", e)))?;

        let stdout_bytes = stdout_task.await.unwrap_or_default();
        let stderr_bytes = stderr_task.await.unwrap_or_default();

        if stdout_bytes.len() >= MAX_SHELL_OUTPUT_BYTES {
            warn!(
                "cron shell stdout truncated at {} bytes (job may be producing unbounded output)",
                MAX_SHELL_OUTPUT_BYTES
            );
        }

        if status.success() {
            Ok(String::from_utf8_lossy(&stdout_bytes).to_string())
        } else {
            let stderr_str = String::from_utf8_lossy(&stderr_bytes);
            Err(SyscityError::Internal(format!("Shell error: {}", stderr_str)))
        }
    }

    /// Execute via agent
    ///
    /// `agent_id` is attached to the outgoing message metadata so
    /// downstream routing can dispatch the work to a specific sub-agent.
    /// `None` lets the routing layer pick the default agent.
    async fn execute_agent(
        agent: &Arc<Agent>,
        job: &CronJob,
        prompt: &str,
        agent_id: Option<&str>,
    ) -> Result<String> {
        let session_id = match job.session {
            SessionTarget::Shared => "cron:shared".to_string(),
            SessionTarget::Isolated => format!("cron:{}", job.id),
        };

        let mut metadata = crate::channels::MessageMetadata::new()
            .with_extra("job_id", job.id.clone())
            .with_extra("job_name", job.name.clone());
        if let Some(id) = agent_id {
            metadata = metadata.with_extra("agent_id", id.to_string());
        }

        let message = IncomingMessage::new("system", &session_id, prompt)
            .with_provenance(crate::channels::InputProvenance::InternalSystem {
                source: "cron".to_string(),
            })
            .with_metadata(metadata);

        let response = agent.process_message(message).await?;
        Ok(response.content)
    }

    /// Deliver result
    async fn deliver_result(
        delivery: &DeliveryMode,
        output: &str,
        announce_tx: &Option<mpsc::Sender<AnnounceDelivery>>,
    ) -> Result<()> {
        match delivery {
            DeliveryMode::None => Ok(()),
            DeliveryMode::Announce { channel, to } => {
                info!("Delivering result to {}:{}", channel, to);
                if let Some(tx) = announce_tx {
                    tx.send(AnnounceDelivery {
                        channel: channel.clone(),
                        to: to.clone(),
                        message: output.to_string(),
                    })
                    .await
                    .map_err(|_| SyscityError::Internal("Announce channel closed".to_string()))?;
                } else {
                    debug!(
                        "No announce_tx configured; output: {}",
                        output.chars().take(100).collect::<String>()
                    );
                }
                Ok(())
            }
            DeliveryMode::Webhook { url, headers } => {
                info!("Delivering result to webhook: {}", url);

                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .map_err(|e| {
                        SyscityError::Internal(format!("Failed to create HTTP client: {}", e))
                    })?;

                const MAX_ATTEMPTS: u32 = 3;
                let mut last_error = String::new();

                for attempt in 1..=MAX_ATTEMPTS {
                    let mut request = client.post(url).body(output.to_string());

                    for (key, value) in headers {
                        request = request.header(key, value);
                    }

                    match request.send().await {
                        Ok(response) => {
                            if response.status().is_success() {
                                debug!("Webhook delivery succeeded on attempt {}", attempt);
                                return Ok(());
                            }
                            let status = response.status();
                            last_error = format!("HTTP {}", status);
                            warn!(
                                "Webhook delivery failed on attempt {}/{}: status {}",
                                attempt, MAX_ATTEMPTS, status
                            );
                        }
                        Err(e) => {
                            last_error = e.to_string();
                            warn!(
                                "Webhook delivery failed on attempt {}/{}: {}",
                                attempt, MAX_ATTEMPTS, e
                            );
                        }
                    }

                    if attempt < MAX_ATTEMPTS {
                        let delay = Duration::from_secs(1 << (attempt - 1));
                        debug!(
                            "Retrying webhook delivery in {:?} (attempt {})",
                            delay,
                            attempt + 1
                        );
                        tokio::time::sleep(delay).await;
                    }
                }

                Err(SyscityError::Internal(format!(
                    "Webhook delivery failed after {} attempts: {}",
                    MAX_ATTEMPTS, last_error
                )))
            }
        }
    }

    /// Log a job run
    pub(super) async fn log_run(
        job_id: &str,
        run_id: &str,
        started_at: DateTime<Utc>,
        completed_at: DateTime<Utc>,
        result: Result<String>,
        delivery_status: Option<DeliveryStatus>,
        store_path: &Option<PathBuf>,
    ) -> Result<()> {
        let entry = match result {
            Ok(output) => RunLogEntry {
                run_id: run_id.to_string(),
                job_id: job_id.to_string(),
                started_at,
                completed_at: Some(completed_at),
                status: RunStatus::Ok,
                output: Some(output),
                error: None,
                delivery_status,
            },
            Err(e) => RunLogEntry {
                run_id: run_id.to_string(),
                job_id: job_id.to_string(),
                started_at,
                completed_at: Some(completed_at),
                status: RunStatus::Error,
                output: None,
                error: Some(format!("{}", e)),
                // When execution itself failed, the delivery field still
                // reflects whatever happened with the error-payload
                // delivery (or `None` for `DeliveryMode::None`).
                delivery_status,
            },
        };

        debug!("Job run logged: {} - {:?}", entry.job_id, entry.status);

        // Human-facing digest: one markdown line per run in the default
        // workspace, so "did the scheduled jobs run OK" is a glance at a
        // file, not a jsonl parse. Best-effort — never fail the run log.
        let line = render_cron_log_line(
            job_id,
            started_at,
            completed_at,
            &entry.status,
            entry.error.as_deref(),
            entry.delivery_status.as_ref(),
        );
        let digest_path = crate::dirs::workspace_data_dir().join("cron-log.md");
        if let Err(e) = append_line(&digest_path, &line).await {
            warn!("Failed to append cron run digest to {}: {}", digest_path.display(), e);
        }

        // Persist to JSONL file if store_path is configured
        if let Some(ref path) = store_path {
            let log_path = path.with_extension("runs.jsonl");
            let line = serde_json::to_string(&entry).map_err(|e| {
                SyscityError::Internal(format!("Failed to serialize run log: {}", e))
            })?;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .await
                .map_err(|e| SyscityError::Internal(format!("Failed to open run log: {}", e)))?;

            // Skip the write when the file exceeds the cap so a
            // long-running scheduler does not fill the disk with
            // run-history JSONL. Operators should rotate or archive
            // the file periodically.
            let too_big = file
                .metadata()
                .await
                .map(|m| m.len() >= MAX_RUN_LOG_BYTES)
                .unwrap_or(false);
            if too_big {
                warn!(
                    "Run log {} exceeds {} bytes, skipping entry",
                    log_path.display(),
                    MAX_RUN_LOG_BYTES
                );
                return Ok(());
            }

            use tokio::io::AsyncWriteExt;
            file.write_all(line.as_bytes())
                .await
                .map_err(|e| SyscityError::Internal(format!("Failed to write run log: {}", e)))?;
            file.write_all(b"\n")
                .await
                .map_err(|e| SyscityError::Internal(format!("Failed to write run log: {}", e)))?;
        }

        Ok(())
    }
}

/// Render one cron run as a single markdown digest line for
/// `workspace/cron-log.md`.
///
/// Kept pure for testability; the `log_run` caller appends the line.
fn render_cron_log_line(
    job_id: &str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    status: &RunStatus,
    error: Option<&str>,
    delivery_status: Option<&DeliveryStatus>,
) -> String {
    let mark = if matches!(status, RunStatus::Ok) {
        "✅"
    } else {
        "❌"
    };
    let secs = (completed_at - started_at).num_seconds().max(0);
    let mut line = format!(
        "- `{}` {} **{}** — {} ({}s)",
        started_at.format("%Y-%m-%d %H:%M"),
        mark,
        job_id,
        if matches!(status, RunStatus::Ok) {
            "ok".to_string()
        } else {
            // Keep the digest to one line; the full error is in runs.jsonl.
            let first = error
                .and_then(|e| e.lines().next())
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .unwrap_or("unknown error");
            format!("failed: {}", first.chars().take(120).collect::<String>())
        },
        secs
    );
    // A failed delivery is itself noteworthy even when execution succeeded.
    if let Some(delivery) = delivery_status {
        if !matches!(delivery, DeliveryStatus::Delivered) {
            line.push_str(&format!(", delivery: {:?}", delivery));
        }
    }
    line
}

/// Append one line to a digest file (creating parent dirs), newline-terminated.
async fn append_line(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    use tokio::io::AsyncWriteExt;
    file.write_all(line.as_bytes()).await?;
    file.write_all(b"\n").await
}

#[cfg(test)]
mod digest_tests {
    use super::*;

    fn at(secs: u64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs as i64, 0).unwrap()
    }

    #[test]
    fn render_ok_run() {
        let line = render_cron_log_line(
            "daily-brief",
            at(1000),
            at(1003),
            &RunStatus::Ok,
            None,
            Some(&DeliveryStatus::Delivered),
        );
        assert!(line.contains("daily-brief"));
        assert!(line.contains("✅"));
        assert!(line.contains("(3s)"));
        assert!(!line.contains("delivery:"), "delivered stays implicit");
    }

    #[test]
    fn render_failed_run_keeps_one_line() {
        let line = render_cron_log_line(
            "sync",
            at(2000),
            at(2000),
            &RunStatus::Error,
            Some("first line\nsecond line"),
            Some(&DeliveryStatus::Failed("webhook 500".to_string())),
        );
        assert!(line.contains("❌"));
        assert!(line.contains("failed: first line"));
        assert!(!line.contains("second line"));
        assert!(line.contains("delivery: Failed"));
    }
}
