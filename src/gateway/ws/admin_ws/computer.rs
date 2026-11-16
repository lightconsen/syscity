//! WS handlers for computer-use runs.
//!
//! Three methods, one lifecycle:
//!
//! - `computer.run` (write) starts a run and returns its `run_id` at once. The
//!   loop is spawned onto the task registry rather than awaited: a run is
//!   minutes long, and WS dispatch is serial per connection — awaiting it here
//!   would block every other call on the same socket, including the
//!   `computer.control` call meant to stop it.
//! - `computer.status` (read) answers "what is it doing now" from the run's
//!   live state.
//! - `computer.control` (write) forwards pause/resume/step/cancel/steer to the
//!   run's [`ExecutionController`], which is the same controller the ACP agent
//!   loop uses. Nothing about pausing is reimplemented here.
//!
//! Progress arrives as `computer.step` events, not as status polls.
//!
//! Three more methods record and replay a task (`computer.record_start`,
//! `computer.record_stop`, `computer.play`). They are a different lifecycle —
//! a recording is not a run — but they share this module because a replay is
//! the same loop with a fixed script, and both act on the one shared desktop.

use std::sync::Arc;
use std::time::Duration;

use super::super::{parse_params, WsRequest, WsResponse};
use crate::gateway::protocol::error_invalid_request;
use crate::gateway::{GatewayEvent, GatewayState};
use crate::planner::workflow::{
    self, FailureStrategy, RecordedStep, StepResult, Workflow, WorkflowPlayer, WorkflowStore,
};

/// How long a run gets to unwind after shutdown cancels it.
///
/// A run's loop checks the controller between steps, so cancelling stops the
/// *next* action immediately; this window is for the action already in flight.
/// Past it the task is dropped, because a slow action must not hold the whole
/// gateway's shutdown open.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// `computer.run` — start a goal and return immediately.
///
/// Params: `{ "goal": "...", "session_id": "..."? }`.
///
/// Refusals are all "nothing started": no adapter, computer use disabled, or a
/// model that cannot drive a screen. A run that starts and then cannot decide
/// is worse than one that never started, because the caller has a `run_id` and
/// nothing that will ever use it.
pub(crate) async fn handle_computer_run(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let params = match parse_params::<RunParams>(req) {
        Ok(params) => params,
        Err(resp) => return resp,
    };
    let goal = params.goal.trim().to_string();
    if goal.is_empty() {
        return error_invalid_request(&req.id, "Missing required parameter `goal`");
    }

    let config = state.config.read().await.clone();
    if !config.computer.enabled {
        return WsResponse::err(
            &req.id,
            "COMPUTER_DISABLED",
            "Computer use is disabled (`[computer] enabled = false`)",
        );
    }

    let run_id = uuid::Uuid::new_v4().to_string();
    let run = state
        .tools
        .computer_runs
        .create(run_id.clone(), goal.clone())
        .await;

    let session_id = params.session_id.clone().filter(|s| !s.is_empty());
    let started = match launch(state, &config, run, session_id, None).await {
        Ok(started) => started,
        Err(mut resp) => {
            // Nothing was started, so the half-built run must not linger in the
            // registry looking like something that is about to move.
            state.tools.computer_runs.remove(&run_id).await;
            resp.id = req.id.clone();
            return resp;
        }
    };

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "run_id": run_id,
            "goal": goal,
            "model": started.model,
            "model_supports_vision": started.model_supports_vision,
            "max_steps": started.max_steps,
            "budget": {
                "max_calls_per_run": config.computer.budget.max_calls_per_run,
                "max_images_per_run": config.computer.budget.max_images_per_run,
            },
        }),
    )
}

/// `computer.runs` — runs a restart interrupted, or that stopped short.
///
/// Params: `{}`; optional `{ "include_finished": true }` to also list runs that
/// stopped for a reason (`max_steps`, `budget`, `need_help`) rather than being
/// cut off mid-step.
///
/// Read from disk rather than from memory: the point of the method is the runs
/// this process never saw. It is also why it is separate from
/// `computer.status` — status answers "what is running", and an interrupted run
/// is precisely not running.
pub(crate) async fn handle_computer_runs(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let params = match parse_params::<serde_json::Value>(req) {
        Ok(params) => params,
        Err(resp) => return resp,
    };
    let include_finished = params
        .get("include_finished")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let store = crate::computer::run_store::RunStore::for_paths(&state.paths);
    let all = store.load_all().await;
    let runs: Vec<serde_json::Value> = all
        .iter()
        .filter(|c| include_finished || c.is_resumable())
        .map(|c| c.view())
        .collect();

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "runs": runs,
            "interrupted": all.iter().filter(|c| c.is_interrupted()).count(),
        }),
    )
}

/// `computer.resume` — continue an interrupted run from step N+1.
///
/// Params: `{ "run_id": "...", "confirm_sensitive": false }`.
///
/// Resuming is a decision, not a repair: the file says what the run had done,
/// and this method starts a *new* loop that carries that history, continues the
/// step counter and the budget counters, and never auto-replays a completed
/// action. A checkpoint that recorded a step aimed at a sensitive control
/// requires `confirm_sensitive: true` — the person approving the resume is
/// approving the goal, not the password field a previous model call happened to
/// aim at.
pub(crate) async fn handle_computer_resume(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let params = match parse_params::<ResumeParams>(req) {
        Ok(params) => params,
        Err(resp) => return resp,
    };
    let run_id = params.run_id.trim().to_string();
    if run_id.is_empty() {
        return error_invalid_request(&req.id, "Missing required parameter `run_id`");
    }

    let config = state.config.read().await.clone();
    if !config.computer.enabled {
        return WsResponse::err(
            &req.id,
            "COMPUTER_DISABLED",
            "Computer use is disabled (`[computer] enabled = false`)",
        );
    }

    // A run already in this process cannot be resumed: two loops on one desktop
    // is not a continuation, it is a race.
    if let Some(live) = state.tools.computer_runs.get(&run_id).await {
        if !live.snapshot().await.phase.is_finished() {
            return WsResponse::err(
                &req.id,
                "RUN_ALREADY_LIVE",
                format!("Run {run_id} is already running in this gateway"),
            );
        }
    }

    let store = crate::computer::run_store::RunStore::for_paths(&state.paths);
    let Some(checkpoint) = store.load(&run_id).await else {
        return WsResponse::err(
            &req.id,
            "RUN_NOT_FOUND",
            format!("No interrupted computer run with id {run_id}"),
        );
    };
    if !checkpoint.is_resumable() {
        return WsResponse::err(
            &req.id,
            "RUN_NOT_RESUMABLE",
            format!("Run {run_id} already reached a conclusion ({:?})", checkpoint.stop_reason),
        );
    }

    let sensitive = checkpoint.sensitive_steps();
    if sensitive > 0 && !params.confirm_sensitive {
        return WsResponse::err(
            &req.id,
            "CONFIRMATION_REQUIRED",
            format!(
                "Run {run_id} recorded {sensitive} step(s) aimed at a sensitive control \
                 (password / payment / destructive confirmation); resend with \
                 `confirm_sensitive: true` to continue it"
            ),
        );
    }

    let started_from = checkpoint.next_step();
    let run = state
        .tools
        .computer_runs
        .create(run_id.clone(), checkpoint.goal.clone())
        .await;

    let session_id = checkpoint.session_id.clone();
    let started = match launch(state, &config, run, session_id, Some(checkpoint)).await {
        Ok(started) => started,
        Err(mut resp) => {
            state.tools.computer_runs.remove(&run_id).await;
            resp.id = req.id.clone();
            return resp;
        }
    };

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "run_id": run_id,
            "goal": serde_json::Value::Null,
            "model": started.model,
            "model_supports_vision": started.model_supports_vision,
            "max_steps": started.max_steps,
            "resumed_from_step": started_from,
            "sensitive_steps": sensitive,
        }),
    )
}

/// What `launch` established, for the response of the method that called it.
struct StartedRun {
    model: String,
    model_supports_vision: bool,
    max_steps: usize,
}

/// Attach a loop to `run` and spawn it.
///
/// Everything that can refuse the run happens here, before the spawn: no
/// adapter, no provider, no model. All three are refusals *of the request*, and
/// a caller that gets a `run_id` back must be able to rely on the run actually
/// moving.
async fn launch(
    state: &Arc<GatewayState>,
    config: &crate::gateway::GatewayConfig,
    run: Arc<crate::computer::run_registry::ComputerRun>,
    session_id: Option<String>,
    checkpoint: Option<crate::computer::run_store::Checkpoint>,
) -> Result<StartedRun, WsResponse> {
    let adapter = state.tools.computer_adapter.read().await.clone();
    let Some(adapter) = adapter else {
        return Err(WsResponse::err(
            "",
            "COMPUTER_UNAVAILABLE",
            "No desktop adapter is available on this host",
        ));
    };

    let (provider, model) = match driver_provider(state).await {
        Ok(pair) => pair,
        Err(resp) => return Err(resp.with_id("")),
    };
    let provider_name = provider.name().to_string();

    // Whether the routed model can see. This is the capability gate for the
    // perception plan: a model without vision is never sent an image, so the
    // run is text-only rather than paying for images it cannot use.
    let model_supports_vision = state
        .infra
        .model_router
        .supports_vision(&provider_name, &model)
        .await;

    let image_limit = crate::computer::image_budget::ImageLimit::from_config(
        config.computer.screenshot_max_width,
        &config.computer.screenshot_format,
        config.computer.screenshot_jpeg_quality,
    );
    let driver = Arc::new(
        crate::computer::driver::ModelDriver::new(provider, model.clone())
            .with_image_limit(image_limit.clone()),
    );

    // A resumed run keeps the ceiling it was started with: raising it silently
    // would make the resume a different run than the one the file describes.
    let loop_config = match checkpoint.as_ref() {
        Some(checkpoint) => checkpoint.loop_config(),
        None => crate::computer::LoopConfig {
            max_steps: config.computer.max_steps,
            settle_delay_ms: config.computer.settle_delay_ms,
            ..Default::default()
        },
    };

    let run_id = run.run_id.clone();
    let observer: crate::computer::run_executor::RunObserver = {
        let tx = state.events.tx.clone();
        let session_id = session_id.clone();
        let run_id = run_id.clone();
        Arc::new(move |event| {
            let payload = match serde_json::to_value(&event) {
                Ok(value) => value,
                Err(e) => {
                    tracing::warn!("computer run event did not serialize: {e}");
                    return;
                }
            };
            let gateway_event = GatewayEvent::ComputerStep {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                event: payload,
            };
            if let Err(e) = tx.send(gateway_event) {
                // No subscribers is the normal case for a headless run; an
                // event nobody hears is not a reason to warn.
                tracing::debug!("no subscribers for computer step event: {e}");
            }
        })
    };

    // The checkpoint the executor writes into: the one passed in (a resume), or
    // a fresh one. Either way it is stamped with the session, so a later
    // restart's notice reaches the conversation that asked for the run — a
    // checkpoint written without one can only be found by `computer.runs`, and
    // the person who started the run is never told.
    let checkpoint = Some(match checkpoint {
        Some(mut checkpoint) => {
            checkpoint.session_id = session_id.clone();
            checkpoint
        }
        None => {
            let mut fresh = crate::computer::run_store::Checkpoint::new(
                run.run_id.clone(),
                run.goal.clone(),
                &loop_config,
            );
            fresh.session_id = session_id.clone();
            fresh
        }
    });

    let plan = crate::computer::run_executor::RunPlan {
        adapter,
        driver,
        loop_config,
        model_supports_vision,
        vision: vision_look(state, config, image_limit).await,
        budget: config.computer.budget.clone(),
        observer: Some(observer),
        store: Some(Arc::new(crate::computer::run_store::RunStore::for_paths(&state.paths))),
        checkpoint,
        allow_apps: state.tools.computer_allow_apps.as_ref().clone(),
        // This run exists because a `computer.run` request asked for it, and
        // that request passed the gate. The adapter's inner calls inherit that
        // approval rather than asking again — they run with a bare context that
        // has no queue to put a second question on.
        approved_by: "computer.run".to_string(),
        conversation_id: session_id.clone().unwrap_or_default(),
    };

    let task_state = state.clone();
    let task_run = run.clone();
    let shutdown = state.shutdown_token.clone();
    let task = tokio::spawn(async move {
        let execution = crate::computer::run_executor::execute(task_run.clone(), plan);
        tokio::pin!(execution);

        // The loop consults its controller between steps, so a cancel stops the
        // *next* action at once. The grace period is for the action already in
        // flight: past it the task is dropped, because one slow step must not
        // hold the whole gateway's shutdown open.
        let outcome = tokio::select! {
            result = &mut execution => Some(result),
            _ = shutdown.cancelled() => {
                tracing::info!("cancelling computer run {} for shutdown", task_run.run_id);
                task_run.controller.cancel().await;
                match tokio::time::timeout(SHUTDOWN_GRACE, &mut execution).await {
                    Ok(result) => Some(result),
                    Err(_) => {
                        tracing::warn!(
                            "computer run {} did not stop within {:?} of shutdown",
                            task_run.run_id,
                            SHUTDOWN_GRACE
                        );
                        None
                    }
                }
            }
        };

        match outcome {
            Some(Ok(outcome)) => {
                tracing::info!("computer run {} finished: {}", outcome.run_id, outcome.describe());
                record_run_badcase(&task_state, &outcome).await;
                if let Some(handoff) = outcome.handoff {
                    post_handoff(&task_state, &handoff).await;
                }
            }
            Some(Err(e)) => {
                tracing::warn!("computer run {} failed to execute: {e}", task_run.run_id);
            }
            None => {}
        }
        task_state.tools.computer_runs.sweep_finished().await;
    });

    state
        .task_registry
        .insert_join(format!("computer:{run_id}"), task)
        .await;

    Ok(StartedRun {
        model,
        model_supports_vision,
        max_steps: loop_config.max_steps,
    })
}

/// `computer.status` — one run, or all of them.
///
/// Params: `{ "run_id": "..." }` for one, `{}` for the list.
pub(crate) async fn handle_computer_status(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let params = match parse_params::<serde_json::Value>(req) {
        Ok(params) => params,
        Err(resp) => return resp,
    };
    let run_id = params.get("run_id").and_then(|v| v.as_str());

    match run_id {
        Some(run_id) if !run_id.is_empty() => match state.tools.computer_runs.get(run_id).await {
            Some(run) => WsResponse::ok(&req.id, run.view().await),
            None => WsResponse::err(
                &req.id,
                "RUN_NOT_FOUND",
                format!("No computer run with id {run_id}"),
            ),
        },
        _ => WsResponse::ok(
            &req.id,
            serde_json::json!({ "runs": state.tools.computer_runs.list().await }),
        ),
    }
}

/// `computer.control` — pause, resume, step, cancel, or steer a run.
///
/// Params: `{ "run_id": "...", "action": "pause|resume|step|cancel|steer",
/// "message": "..." }`. `message` is required for `steer` and ignored
/// otherwise.
///
/// Unknown actions are refused rather than defaulted: "I said stop and it kept
/// going" is the worst failure this method has, so a typo must not silently
/// become a no-op.
pub(crate) async fn handle_computer_control(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let params = match parse_params::<ControlParams>(req) {
        Ok(params) => params,
        Err(resp) => return resp,
    };

    let Some(run) = state.tools.computer_runs.get(&params.run_id).await else {
        return WsResponse::err(
            &req.id,
            "RUN_NOT_FOUND",
            format!("No computer run with id {}", params.run_id),
        );
    };

    let action = params.action.trim().to_ascii_lowercase();
    match action.as_str() {
        "pause" => {
            run.controller.pause().await;
            run.update(|s| {
                s.phase = crate::computer::run_registry::RunPhase::Paused;
            })
            .await;
        }
        "resume" => {
            run.controller.resume().await;
            run.update(|s| {
                if !s.phase.is_finished() {
                    s.phase = crate::computer::run_registry::RunPhase::Running;
                }
            })
            .await;
        }
        "step" => {
            run.controller.step().await;
            run.update(|s| {
                if !s.phase.is_finished() {
                    s.phase = crate::computer::run_registry::RunPhase::Stepping;
                }
            })
            .await;
        }
        "cancel" => {
            run.controller.cancel().await;
            // The phase is marked by the executor when the loop returns; until
            // then it reports `Running`, which is truthful — the loop has not
            // reached its next checkpoint yet.
        }
        "steer" => {
            let Some(message) = params.message.as_deref().map(str::trim) else {
                return error_invalid_request(&req.id, "`steer` needs a `message`");
            };
            if message.is_empty() {
                return error_invalid_request(&req.id, "`steer` needs a non-empty `message`");
            }
            run.controller.steer(message);
        }
        other => {
            return error_invalid_request(
                &req.id,
                format!("Unknown action `{other}`; expected pause, resume, step, cancel or steer"),
            );
        }
    }

    let runtime = run.runtime_state().await;
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "run_id": params.run_id,
            "action": action,
            "runtime_state": format!("{runtime:?}").to_lowercase(),
        }),
    )
}

/// Post a run's handoff note to the approval queue.
///
/// Through the approval queue rather than a plain event because the note asks
/// a question — "do you want to take over?" — and the queue is where a
/// question to a person lives: it is persisted while pending, it has a
/// resolution, and the UI already renders it. Nobody answering is not an
/// error; the note stays readable in the run's own status either way.
async fn post_handoff(state: &Arc<GatewayState>, note: &crate::computer::handoff::HandoffNote) {
    let approval = crate::tools::approval::PendingApproval::new(
        format!("computer-handoff-{}", note.run_id),
        "computer_run",
        serde_json::to_value(note).unwrap_or_default(),
        "computer-run",
    )
    .with_risk_level(crate::tools::approval::RiskLevel::Medium)
    .with_message(note.render())
    .with_session(None);
    let id = state.tools.approval_queue.submit(approval).await;
    tracing::info!("computer run {} posted a handoff note ({id})", note.run_id);
}

/// Put a run that stopped short into the pending badcase pool.
///
/// Stage D′'s producer, and the whole of it: the pool, the dedup and the
/// conversion already existed, so a failure needs only to reach them. The
/// checkpoint is the source of truth for what the run did — it is on disk
/// precisely because the run stopped short — and the outcome is the source of
/// truth for why it stopped.
///
/// Failure here is logged and swallowed. A badcase is a byproduct of the run,
/// not a part of it: the person taking over has the handoff note either way, and
/// failing the task would not put the row back.
async fn record_run_badcase(
    state: &Arc<GatewayState>,
    outcome: &crate::computer::run_executor::RunOutcome,
) {
    if !crate::computer::failure::is_a_failure(outcome.stop_reason) {
        return;
    }
    let Some(store) = state.infra.pending_badcase_store.as_ref() else {
        // No pool wired is a configuration, not an error: a headless CLI run
        // has nowhere to put a badcase and does not need one.
        tracing::debug!(
            "computer run {} stopped short; no pending badcase store is wired",
            outcome.run_id
        );
        return;
    };

    // The checkpoint concludes before the outcome is returned, so a failure
    // always has one: `conclude` deletes only `done` / `cancelled`.
    let run_store = crate::computer::run_store::RunStore::for_paths(&state.paths);
    let Some(checkpoint) = run_store.load(&outcome.run_id).await else {
        tracing::warn!(
            "computer run {} stopped short but left no checkpoint to collect",
            outcome.run_id
        );
        return;
    };
    let Some(failure) = crate::computer::failure::failure_of(&checkpoint, outcome.handoff.as_ref())
    else {
        return;
    };

    match store
        .insert_run_failure(&failure, checkpoint.session_id.clone())
        .await
    {
        Ok(true) => tracing::info!(
            "computer run {} became a pending badcase ({})",
            outcome.run_id,
            failure.stop_reason
        ),
        // A repeat of a failure already in the pool. Not noise: the pool is a
        // set of distinct dead ends, and the run log has the rest.
        Ok(false) => tracing::debug!(
            "computer run {} repeated a failure already in the badcase pool",
            outcome.run_id
        ),
        Err(e) => {
            tracing::warn!("could not record computer run {} as a badcase: {e}", outcome.run_id)
        }
    }
}

/// The visual scan for a run, built from `[computer.vision]`.
///
/// Two facts decide what this returns:
///
/// - **Whether a local model is on disk.** Probed here, not inside the run:
///   the resolver's answer for `local` mode is a privacy promise, and a run
///   must not discover halfway through that its only backend is missing. The
///   probe is a `stat`, never a download.
/// - **Whether an API rung was configured.** Read from the vision config, not
///   from the run's decision provider: the two are independent on purpose, so
///   a text-only model can still drive a run that has a vision model attached
///   for looking.
///
/// `None` — no scan at all — only when the configuration turns vision off.
/// Every other unavailable case is left to the resolver so the reason reaches
/// the run log instead of being swallowed here.
async fn vision_look(
    state: &Arc<GatewayState>,
    config: &crate::gateway::GatewayConfig,
    image_limit: crate::computer::image_budget::ImageLimit,
) -> Option<Arc<crate::computer::vision::look::VisionLook>> {
    let vision_config = config.computer.vision.clone();
    if vision_config.mode.trim().eq_ignore_ascii_case("off") {
        return None;
    }

    let local_present = local_model_present(&vision_config);
    let resolver = crate::computer::vision::resolver::VisionResolver::from_config(
        &vision_config,
        local_present,
    );

    let api = match api_vision(state, &vision_config).await {
        Ok(api) => api,
        Err(e) => {
            tracing::warn!("the AI vision rung is configured but unusable: {e}");
            None
        }
    };

    Some(Arc::new(
        crate::computer::vision::look::VisionLook::new(resolver, api)
            .with_image_limit(image_limit.clone()),
    ))
}

/// Whether the OmniParser weights are already on this machine.
///
/// A `stat`, deliberately: [`crate::computer::vision::model_download`] can
/// fetch the model, and a run start is not the place to spend a minute and
/// 88 MB of somebody's link on that.
fn local_model_present(config: &crate::config::VisionConfig) -> bool {
    if !cfg!(feature = "vision") {
        return false;
    }
    let dir = crate::computer::vision::model_download::vision_model_dir(&config.model_dir);
    dir.join("omniparser.onnx").is_file()
}

/// The provider handle for the API rung, when one is configured.
async fn api_vision(
    state: &Arc<GatewayState>,
    config: &crate::config::VisionConfig,
) -> crate::Result<Option<crate::computer::vision::look::ApiVision>> {
    let (Some(provider_name), Some(model)) = (
        config
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty()),
        config
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty()),
    ) else {
        return Ok(None);
    };

    // Pin the configured provider when it exists; fall back to the default one
    // otherwise. A named provider that is not configured is a reason to say so
    // in the log and continue without the rung — the local one may still work,
    // and a run must not be refused over its *second* choice of eyes.
    let provider = match state.infra.model_router.get_provider(provider_name).await {
        Some(provider) => provider,
        None => {
            tracing::warn!(
                "vision provider {provider_name:?} is not configured; the API vision rung is \
                 unavailable this run"
            );
            return Ok(None);
        }
    };
    Ok(Some(crate::computer::vision::look::ApiVision {
        provider,
        provider_name: provider_name.to_string(),
        model: model.to_string(),
    }))
}

/// A driver construction failure, before any run exists.
pub(crate) struct DriverRefusal {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl DriverRefusal {
    pub(crate) fn with_id(self, id: &str) -> WsResponse {
        WsResponse::err(id, self.code, self.message)
    }
}

/// The provider and model a run decides with.
///
/// Pinned to the concrete provider rather than routed per call: a run's steps
/// are one task, and switching providers midway would change the answer's
/// format (and its vision capability) halfway through a decision sequence.
///
/// Also used by `turns.rerun`: a rerun asks the same question a run would
/// ("what would this model do, given this screen?"), so it must resolve the
/// provider the same way.
pub(crate) async fn driver_provider(
    state: &Arc<GatewayState>,
) -> Result<(Arc<dyn crate::providers::Provider>, String), DriverRefusal> {
    let model = {
        let settings = state.infra.runtime_settings.read().await;
        match settings.get("fast.active_model").and_then(|v| v.as_str()) {
            Some(model) => model.to_string(),
            None => state.config.read().await.model.clone(),
        }
    };
    if model.trim().is_empty() {
        return Err(DriverRefusal {
            code: "NO_MODEL",
            message: "No model is configured; computer use needs one to decide steps".to_string(),
        });
    }

    let provider = match provider_for_model(state, &model).await {
        Some((provider, name)) => {
            tracing::debug!("computer run decides with provider `{name}` (owns `{model}`)");
            provider
        }
        None => state
            .infra
            .model_router
            .create_default_provider()
            .await
            .map_err(|e| DriverRefusal {
                code: "NO_PROVIDER",
                message: format!("No model provider is available: {e}"),
            })?,
    };

    let model = if model.starts_with(&format!("{}/", provider.name())) {
        model
            .split_once('/')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or(model)
    } else {
        model
    };
    Ok((provider, model))
}

/// The provider that *owns* `model`, when the router can name one.
///
/// Deciding with `create_default_provider()` alone is wrong here: that pick is
/// the alphabetically first non-cloud provider, so a run whose model belongs
/// to `mock` would still be decided by `anthropic` whenever both are
/// registered — a different model, a different answer format, and with an
/// unusable credential a 403 instead of the decision. The model is the more
/// specific fact, so it decides the provider; the default is only the fallback
/// for a model the catalog does not know.
async fn provider_for_model(
    state: &Arc<GatewayState>,
    model: &str,
) -> Option<(Arc<dyn crate::providers::Provider>, String)> {
    let router = &state.infra.model_router;

    // An explicitly qualified `provider/model` reference is pinned by hand.
    if let Some((name, _)) = model.split_once('/') {
        if let Some(provider) = router.get_provider(name).await {
            return Some((provider, name.to_string()));
        }
    }

    let name = router.provider_for_model(model).await?;
    let provider = router.get_provider(&name).await?;
    Some((provider, name))
}

// ── Recording and replay ─────────────────────────────────────────────────────

/// `computer.record_start` — begin capturing the actions the `computer` tool
/// takes.
///
/// Params: `{ "name": "..."? }`.
///
/// One recording at a time across the whole gateway. There is one screen, so two
/// recordings cannot usefully be different — and a second `start` that silently
/// discarded the first would lose a capture the caller believed it had.
pub(crate) async fn handle_computer_record_start(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let config = state.config.read().await.clone();
    if !config.computer.enabled {
        return WsResponse::err(
            &req.id,
            "COMPUTER_DISABLED",
            "Computer use is disabled (`[computer] enabled = false`)",
        );
    }
    let params = match parse_params::<serde_json::Value>(req) {
        Ok(params) => params,
        Err(resp) => return resp,
    };
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string);

    match state.tools.computer_recording.start(name.clone()) {
        Ok(()) => {
            // The picture is a second, optional half of the recording. It is
            // started here — after the action recorder is running, so the two
            // cover the same interval — and its failure never fails the
            // recording: losing the video but keeping the replayable workflow
            // is strictly better than losing both.
            let video = if config.computer.record_video {
                start_recording_video(state, &name).await
            } else {
                None
            };
            WsResponse::ok(
                &req.id,
                serde_json::json!({
                    "recording": true,
                    "name": name,
                    "steps": 0,
                    "video": video,
                }),
            )
        }
        Err(workflow::RecordingError::AlreadyRecording) => WsResponse::err(
            &req.id,
            "ALREADY_RECORDING",
            "A recording is already in progress; stop it with `computer.record_stop` first",
        ),
        Err(e) => WsResponse::err(&req.id, "RECORD_START_FAILED", e.to_string()),
    }
}

/// Start the video half of a recording, returning the file path when it began.
///
/// Failure here is reported, not raised: `computer.record_start` has already
/// succeeded at the half that matters (the action recorder), and a missing
/// screen-capture permission on this host must not make recording impossible.
/// The reply's `video: null` is the caller's answer to "was there a picture",
/// and the warning names the fix for whoever is reading the logs.
async fn start_recording_video(
    state: &Arc<GatewayState>,
    name: &Option<String>,
) -> Option<std::path::PathBuf> {
    let Some(session) = state.tools.computer_recording.session_id() else {
        // The action recorder stopped between `start` and here — a second
        // `record_stop` raced this call. Nothing to capture.
        return None;
    };
    let dir = crate::computer::recording::video_dir(&state.paths);
    match crate::computer::recording::start_capture(
        state.tools.computer_recording.clone(),
        session,
        name.clone(),
        dir,
    )
    .await
    {
        Ok(capture) => {
            // Registered so a gateway shutdown stops the capture rather than
            // leaving an ffmpeg child holding the display.
            state
                .task_registry
                .insert_join(format!("computer-recording-video-{session}"), capture.task)
                .await;
            *state.tools.computer_recording_video.write().await = Some(capture.path.clone());
            Some(capture.path)
        }
        Err(e) => {
            tracing::warn!(
                "[computer.record_start] no video for this recording: {e}. The workflow will \
                 still be recorded; enable Screen Recording for this process to capture a video."
            );
            None
        }
    }
}

/// `computer.record_stop` — finish the recording and save it as a workflow.
///
/// Params: `{ "name": "...", "description": "..."? }`.
///
/// The saved workflow has its typed values already turned into `{{parameters}}`
/// ([`Workflow::parameterize`]); the response names them so the caller can bind
/// them on replay without opening the file.
pub(crate) async fn handle_computer_record_stop(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let params = match parse_params::<serde_json::Value>(req) {
        Ok(params) => params,
        Err(resp) => return resp,
    };
    let name = match params.get("name").and_then(|v| v.as_str()).map(str::trim) {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => {
            return error_invalid_request(&req.id, "Missing required parameter `name`");
        }
    };

    // Taken before the recording stops: the capture's own task notices the end
    // of the session and closes the file, and reading this afterwards would be
    // a race with a `record_start` that follows hard on this one.
    let video_path = state.tools.computer_recording_video.write().await.take();
    let mut workflow = match state.tools.computer_recording.stop(Some(name.clone())) {
        Ok(workflow) => workflow,
        Err(workflow::RecordingError::NotRecording) => {
            return WsResponse::err(
                &req.id,
                "NOT_RECORDING",
                "No recording is in progress; start one with `computer.record_start`",
            );
        }
        Err(e) => return WsResponse::err(&req.id, "RECORD_STOP_FAILED", e.to_string()),
    };
    if let Some(description) = params
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        workflow = workflow.with_description(description);
    }

    if let Some(video) = &video_path {
        let dir = crate::computer::recording::video_dir(&state.paths);
        workflow.video = crate::computer::recording::relative_to(&dir, video);
    }

    let store = WorkflowStore::for_paths(&state.paths);
    let path = match store.save(&workflow).await {
        Ok(path) => path,
        Err(e) => {
            // The recording is gone from the recorder either way: reporting the
            // failure and leaving it "recording" would be a lie, and re-saving
            // is the caller's retry, not something to invent here.
            tracing::warn!("could not save workflow {name:?}: {e}");
            return WsResponse::err(&req.id, "WORKFLOW_SAVE_FAILED", e.to_string());
        }
    };

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "recording": false,
            "name": workflow.name,
            "path": path.display().to_string(),
            "steps": workflow.steps.len(),
            "parameters": workflow.parameters,
            // Named so a caller can open it; `null` when the capture was off
            // or produced nothing, which is not an error.
            "video": video_path.as_ref().map(|p| p.display().to_string()),
            "video_ready": video_path.is_some(),
        }),
    )
}

/// `computer.play` — replay a saved workflow.
///
/// Params:
/// `{ "name": "...", "parameters": {...}?, "strategy": "skip|abort|retry"?,
///    "confirm_sensitive": false?, "session_id": "..."? }`.
///
/// Three refusals before anything moves:
///
/// - the workflow must exist;
/// - its parameters must all be bound — a missing value would otherwise replay
///   the literal `{{name}}`, which types a placeholder into somebody's field;
/// - a workflow with a sensitive step needs `confirm_sensitive: true`. The
///   approval given to whatever recorded it does not survive to this run: a
///   password field that was filled once is not a field to fill again unasked.
///
/// The replay is spawned like `computer.run` rather than awaited — a workflow
/// has as many steps as a task, and WS dispatch is serial per connection.
pub(crate) async fn handle_computer_play(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let params = match parse_params::<PlayParams>(req) {
        Ok(params) => params,
        Err(resp) => return resp,
    };
    let config = state.config.read().await.clone();
    if !config.computer.enabled {
        return WsResponse::err(
            &req.id,
            "COMPUTER_DISABLED",
            "Computer use is disabled (`[computer] enabled = false`)",
        );
    }

    let store = WorkflowStore::for_paths(&state.paths);
    let Some(workflow) = store.load(params.name.trim()).await else {
        return WsResponse::err(
            &req.id,
            "WORKFLOW_NOT_FOUND",
            format!("No saved workflow named {:?}", params.name),
        );
    };

    let strategy = match params.strategy.as_deref() {
        None => FailureStrategy::Skip,
        Some(name) => match FailureStrategy::parse(name) {
            Some(strategy) => strategy,
            None => {
                return error_invalid_request(
                    &req.id,
                    format!("Unknown strategy {name:?}; expected skip, abort or retry"),
                );
            }
        },
    };

    if let Some(missing) = unbound_parameters(&workflow, &params.parameters) {
        return WsResponse::err(
            &req.id,
            "PARAMETERS_MISSING",
            format!(
                "Workflow {} needs values for: {}; pass them in `parameters`",
                workflow.name,
                missing.join(", ")
            ),
        );
    }

    let sensitive: Vec<usize> = workflow
        .steps
        .iter()
        .enumerate()
        .filter(|(_, step)| workflow::needs_confirmation(step))
        .map(|(index, _)| index + 1)
        .collect();
    if !sensitive.is_empty() && !params.confirm_sensitive {
        return WsResponse::err(
            &req.id,
            "CONFIRMATION_REQUIRED",
            format!(
                "Workflow {} has {} step(s) that need a person (sensitive control, close, \
                 kill, or shell) at {}; resend with `confirm_sensitive: true` to replay it",
                workflow.name,
                sensitive.len(),
                sensitive
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }

    let run_id = uuid::Uuid::new_v4().to_string();
    let run = state
        .tools
        .computer_runs
        .create(run_id.clone(), format!("replay workflow {}", workflow.name))
        .await;

    let session_id = params.session_id.clone().filter(|s| !s.is_empty());
    let player = WorkflowPlayer::new().with_failure_strategy(strategy);
    let steps = workflow.bind_parameters(params.parameters.clone());
    let total = steps.len();
    let plan = PlayPlan {
        run_id: run_id.clone(),
        session_id: session_id.clone(),
        workflow_name: workflow.name.clone(),
        steps,
        player,
    };

    let spawned = spawn_play(state, run, plan).await;
    if let Err(resp) = spawned {
        state.tools.computer_runs.remove(&run_id).await;
        let mut resp = resp;
        resp.id = req.id.clone();
        return resp;
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "run_id": run_id,
            "workflow": workflow.name,
            "steps": total,
            "sensitive_steps": sensitive,
            "strategy": strategy.as_str(),
        }),
    )
}

/// Everything the spawned replay needs, assembled before the spawn.
struct PlayPlan {
    run_id: String,
    session_id: Option<String>,
    workflow_name: String,
    steps: Vec<RecordedStep>,
    player: WorkflowPlayer,
}

/// Build a session for this replay and spawn it.
///
/// Every refusal happens here, before the spawn, for the same reason `launch`
/// does it: a caller that got a `run_id` back must be able to rely on the replay
/// moving.
async fn spawn_play(
    state: &Arc<GatewayState>,
    run: Arc<crate::computer::run_registry::ComputerRun>,
    plan: PlayPlan,
) -> Result<(), WsResponse> {
    let adapter = {
        let guard = state.tools.computer_adapter.read().await;
        guard.clone()
    };
    let Some(adapter) = adapter else {
        return Err(WsResponse::err(
            "",
            "COMPUTER_UNAVAILABLE",
            "No desktop adapter is available on this host",
        ));
    };

    let task_state = state.clone();
    let total = plan.steps.len();
    let plan_run_id = plan.run_id.clone();
    let allow_apps = state.tools.computer_allow_apps.as_ref().clone();
    let conversation_id = plan.session_id.clone().unwrap_or_default();

    // Before the first step moves anything: would any step have to guess?
    //
    // Only ambiguity is checked, not existence — most workflows open their own
    // window in step 1, so demanding the window now would refuse the very
    // workflows this protects. See `replay_preflight` for why the distinction
    // is the whole design.
    //
    // A tree that cannot be read is *not* a refusal: the replay still runs, and
    // every step resolves its own target exactly as it did before. Losing the
    // preflight costs the early warning, not the replay.
    let described: Vec<(String, crate::computer::DesktopAction)> = plan
        .steps
        .iter()
        .filter_map(|step| match &step.action {
            crate::planner::workflow::WorkflowAction::Desktop(action) => {
                Some((step.description.clone(), action.clone()))
            }
            _ => None,
        })
        .collect();
    if !described.is_empty() {
        let check: Result<(), WsResponse> = async {
            let Ok(roots) = adapter.read_ui_tree(None).await else {
                tracing::debug!(
                    "replay preflight skipped: the UI tree could not be read on this host"
                );
                return Ok(());
            };
            let mut roots = roots;
            let id = crate::computer::element_index::next_snapshot_id();
            crate::computer::element_index::assign_ids(&mut roots, id);
            let snapshot = crate::computer::element_index::UiSnapshot { id, app: None, roots };
            let found = crate::computer::replay_preflight::find_ambiguous(&described, &snapshot);
            if found.is_empty() {
                return Ok(());
            }
            Err(WsResponse::err(
                "",
                "REPLAY_AMBIGUOUS",
                crate::computer::replay_preflight::refuse(&plan.workflow_name, &found),
            ))
        }
        .await;
        check?;
    }

    let task = tokio::spawn(async move {
        // One session for the whole replay, so the ladder's step history and
        // the verification engine see the task, not N unrelated actions.
        let session = crate::computer::session::ComputerSession::new(adapter).with_config(
            crate::computer::session::SessionConfig {
                allow_apps,
                // The replay was asked for by a `computer.play` request that
                // already passed the gate; its inner desktop calls inherit
                // that approval.
                approved_by: "computer.play".to_string(),
                conversation_id,
                ..Default::default()
            },
        );
        let mut outcomes = Vec::with_capacity(total);
        let mut last: Option<StepResult> = None;
        let mut cancelled = false;

        for (index, step) in plan.steps.iter().enumerate() {
            // The same pause / cancel gate a run between steps, on this run's
            // controller: a replay is steerable the way a run is.
            if let Err(why) = run.controller.check_and_wait().await {
                cancelled = true;
                last = Some(StepResult::Failed { error: why.to_string() });
                break;
            }

            let delay = plan.player.delay_for(step);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }

            let result = plan.player.play_step(step, &session).await;
            let verified = match &result {
                StepResult::Success { verified, .. } => *verified,
                _ => None,
            };
            let verdict = match verified {
                Some(true) => crate::computer::use_loop::StepVerification::Verified,
                Some(false) => crate::computer::use_loop::StepVerification::Failed,
                None => crate::computer::use_loop::StepVerification::Unverifiable,
            };
            run.update(|s| {
                s.step = index + 1;
                s.last_action = Some(step.description.clone());
                s.last_verified = verified;
                s.completed.push(crate::computer::run_store::step_line(
                    index,
                    &step.description,
                    &describe_step_result(&result),
                    crate::computer::run_store::verdict_word(verdict),
                ));
            })
            .await;

            broadcast_step(
                &task_state,
                &plan.session_id,
                &plan.run_id,
                &crate::computer::run_executor::RunEvent::Step {
                    run_id: plan.run_id.clone(),
                    // Zero-based, like a run's own step events (they carry the
                    // loop's `record.step`, which starts at 0) and like
                    // `turns.replay`'s timeline. `run.step` below stays a
                    // *count* of completed steps; the event is a position, and
                    // one timeline drawn from a replay and from a live run must
                    // not be off by one between them.
                    step: index,
                    action: step.description.clone(),
                    rung: None,
                    effect: None,
                    screenshot_before_ref: None,
                    screenshot_ref: None,
                    verified,
                    model_calls: 0,
                    images_attached: 0,
                },
            );

            let go_on = result.should_continue(plan.player.failure_strategy());
            outcomes.push(result.clone());
            last = Some(result);
            if !go_on {
                break;
            }
        }

        let succeeded = outcomes.len() == total && outcomes.iter().all(StepResult::succeeded);
        let message = match (&last, succeeded) {
            (_, true) => format!("Replayed {total} step(s) of {:?}", plan.workflow_name),
            (Some(result), false) => format!(
                "Replay of {:?} stopped at step {} of {total}: {}",
                plan.workflow_name,
                outcomes.len(),
                describe_step_result(result)
            ),
            (None, false) => format!("Replay of {:?} had nothing to do", plan.workflow_name),
        };

        // A cancelled replay is `Cancelled` even if the last step succeeded:
        // the phase says why it stopped, and "done" would be a false answer.
        let phase = if cancelled {
            crate::computer::run_registry::RunPhase::Cancelled
        } else if succeeded {
            crate::computer::run_registry::RunPhase::Done
        } else {
            crate::computer::run_registry::RunPhase::Failed
        };
        let stop_reason = if cancelled {
            crate::computer::run_budget::StopReason::Cancelled
        } else if succeeded {
            crate::computer::run_budget::StopReason::Done
        } else {
            crate::computer::run_budget::StopReason::NeedHelp
        };

        run.update(|s| {
            s.phase = phase;
            s.message = Some(message.clone());
        })
        .await;

        broadcast_step(
            &task_state,
            &plan.session_id,
            &plan.run_id,
            &crate::computer::run_executor::RunEvent::Finished {
                run_id: plan.run_id.clone(),
                phase,
                steps: outcomes.len(),
                message,
                stop_reason,
            },
        );
        task_state.tools.computer_runs.sweep_finished().await;
    });

    let key = format!("computer:play:{}", plan_run_id);
    state.task_registry.insert_join(key, task).await;

    Ok(())
}

/// Broadcast one replay event on the same channel a run's steps use.
fn broadcast_step(
    state: &Arc<GatewayState>,
    session_id: &Option<String>,
    run_id: &str,
    event: &crate::computer::run_executor::RunEvent,
) {
    let payload = match serde_json::to_value(event) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!("replay event did not serialize: {e}");
            return;
        }
    };
    let gateway_event = GatewayEvent::ComputerStep {
        session_id: session_id.clone(),
        run_id: run_id.to_string(),
        event: payload,
    };
    if let Err(e) = state.events.tx.send(gateway_event) {
        tracing::debug!("no subscribers for replay step event: {e}");
    }
}

/// One line for a step's result, as the run log renders it.
fn describe_step_result(result: &StepResult) -> String {
    match result {
        StepResult::Success { message, .. } => message.clone(),
        StepResult::Skipped { reason } => format!("skipped: {reason}"),
        StepResult::Failed { error } => format!("failed: {error}"),
        StepResult::Refused { reason } => format!("refused: {reason}"),
    }
}

/// The workflow's parameters this call did not bind, if any.
///
/// Checked before the spawn, deliberately: a `{{name}}` left in a `Type` step
/// would type the braces into somebody's field, and a replay that half-ran
/// before failing on step 4 is worse than one that never started.
fn unbound_parameters(
    workflow: &Workflow,
    supplied: &std::collections::HashMap<String, String>,
) -> Option<Vec<String>> {
    // An empty value is not a binding: it would replace the placeholder with
    // nothing, which for a `Type` step means typing nothing at all.
    let missing: Vec<String> = workflow
        .parameters
        .iter()
        .filter(|name| {
            supplied
                .get(*name)
                .map(|v| v.trim().is_empty())
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(missing)
    }
}

#[derive(serde::Deserialize)]
struct PlayParams {
    name: String,
    #[serde(default)]
    parameters: std::collections::HashMap<String, String>,
    #[serde(default)]
    strategy: Option<String>,
    #[serde(default)]
    confirm_sensitive: bool,
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct RunParams {
    goal: String,
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct ResumeParams {
    run_id: String,
    /// Required when the checkpoint records a step aimed at a sensitive
    /// control: the resume is a second consent for those steps.
    #[serde(default)]
    confirm_sensitive: bool,
}

#[derive(serde::Deserialize)]
struct ControlParams {
    run_id: String,
    action: String,
    #[serde(default)]
    message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;

    fn req(method: &str, params: serde_json::Value) -> WsRequest {
        WsRequest {
            frame_type: "req".into(),
            id: "r1".into(),
            method: method.into(),
            params: Some(params),
        }
    }

    #[tokio::test]
    async fn an_empty_goal_is_refused() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let resp =
            handle_computer_run(&req("computer.run", serde_json::json!({"goal": "  "})), &state)
                .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "INVALID_PARAMS");
    }

    /// Disabled computer use must refuse *before* creating a run, or the
    /// caller gets an id for a run that will never do anything.
    #[tokio::test]
    async fn disabled_computer_use_starts_nothing() {
        let mut config = GatewayConfig::default();
        config.computer.enabled = false;
        let state = Arc::new(make_test_state(config).await);
        let resp = handle_computer_run(
            &req("computer.run", serde_json::json!({"goal": "open the calculator"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "COMPUTER_DISABLED");
        assert_eq!(state.tools.computer_runs.live_count().await, 0);
    }

    /// With computer use on but no adapter, the run is refused rather than
    /// started and immediately stuck.
    #[tokio::test]
    async fn no_adapter_starts_nothing() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        // make_test_state installs no computer adapter.
        let resp = handle_computer_run(
            &req("computer.run", serde_json::json!({"goal": "open the calculator"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "COMPUTER_UNAVAILABLE");
        assert_eq!(state.tools.computer_runs.live_count().await, 0);
    }

    #[tokio::test]
    async fn status_of_an_unknown_run_is_not_found() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let resp = handle_computer_status(
            &req("computer.status", serde_json::json!({"run_id": "nope"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "RUN_NOT_FOUND");
    }

    #[tokio::test]
    async fn status_without_an_id_lists_runs() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        state.tools.computer_runs.create("r1", "g").await;
        let resp =
            handle_computer_status(&req("computer.status", serde_json::json!({})), &state).await;
        assert!(resp.ok);
        let payload = resp.payload.expect("payload");
        assert_eq!(payload["runs"].as_array().unwrap().len(), 1);
        assert_eq!(payload["runs"][0]["run_id"], "r1");
    }

    #[tokio::test]
    async fn pause_is_reflected_in_status() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        state.tools.computer_runs.create("r1", "g").await;

        let resp = handle_computer_control(
            &req("computer.control", serde_json::json!({"run_id": "r1", "action": "pause"})),
            &state,
        )
        .await;
        assert!(resp.ok, "{:?}", resp.error);
        assert_eq!(resp.payload.unwrap()["runtime_state"], "paused");

        let status = handle_computer_status(
            &req("computer.status", serde_json::json!({"run_id": "r1"})),
            &state,
        )
        .await;
        let payload = status.payload.expect("payload");
        assert_eq!(payload["runtime_state"], "paused");
        assert_eq!(payload["phase"], "paused");
    }

    /// "I said stop and it kept going" is the failure this guards: an unknown
    /// action must be refused, not treated as a no-op.
    #[tokio::test]
    async fn an_unknown_control_action_is_refused() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        state.tools.computer_runs.create("r1", "g").await;
        let resp = handle_computer_control(
            &req("computer.control", serde_json::json!({"run_id": "r1", "action": "halt"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn steer_requires_a_message() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        state.tools.computer_runs.create("r1", "g").await;
        let resp = handle_computer_control(
            &req("computer.control", serde_json::json!({"run_id": "r1", "action": "steer"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn steer_reaches_the_controller() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let run = state.tools.computer_runs.create("r1", "g").await;
        let resp = handle_computer_control(
            &req(
                "computer.control",
                serde_json::json!({"run_id": "r1", "action": "steer", "message": "use the search box"}),
            ),
            &state,
        )
        .await;
        assert!(resp.ok, "{:?}", resp.error);
        assert_eq!(run.controller.drain_steers(), vec!["use the search box".to_string()]);
    }

    #[tokio::test]
    async fn cancelling_an_unknown_run_is_not_found() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let resp = handle_computer_control(
            &req("computer.control", serde_json::json!({"run_id": "ghost", "action": "cancel"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "RUN_NOT_FOUND");
    }

    // ── Stage C: runs that survive a restart ────────────────────────────

    /// Write a checkpoint into the state's own layout root, the way a previous
    /// process would have.
    async fn save_checkpoint(
        state: &Arc<GatewayState>,
        run_id: &str,
        sensitive: bool,
    ) -> crate::computer::run_store::Checkpoint {
        use crate::computer::run_store::{Checkpoint, CompletedStep, RunStore};
        use crate::computer::types::{ClickTarget, DesktopAction, MouseButton};
        use crate::computer::use_loop::{LoopConfig, StepVerification};

        let mut cp = Checkpoint::new(run_id, "click Save in Preferences", &LoopConfig::default());
        cp.session_id = Some("s1".to_string());
        cp.completed.push(CompletedStep {
            step: 0,
            action: DesktopAction::Click {
                target: ClickTarget::ElementId("s1:0.2".to_string()),
                button: MouseButton::Left,
            },
            verification: StepVerification::Verified,
            result: "clicked".to_string(),
            screenshot_before_ref: None,
            screenshot_ref: None,
            sensitive,
            rung: None,
        });
        cp.model_calls = 1;
        RunStore::for_paths(&state.paths)
            .save(&cp)
            .await
            .expect("save checkpoint");
        cp
    }

    /// The point of the method: a run this process never saw is listed, with
    /// enough in the row for a person to decide whether to continue it.
    #[tokio::test]
    async fn an_interrupted_run_is_listed_from_disk() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        save_checkpoint(&state, "r-interrupted", false).await;

        let resp = handle_computer_runs(&req("computer.runs", serde_json::json!({})), &state).await;
        assert!(resp.ok, "{:?}", resp.error);
        let payload = resp.payload.expect("payload");
        assert_eq!(payload["interrupted"], 1);
        let runs = payload["runs"].as_array().expect("runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["run_id"], "r-interrupted");
        assert_eq!(runs[0]["goal"], "click Save in Preferences");
        assert_eq!(runs[0]["steps_completed"], 1);
        // The list is what a person resumes from, so the step it would continue
        // at is the number the resume must be asked for.
        assert_eq!(runs[0]["next_step"], 1);
        assert_eq!(runs[0]["interrupted"], true);
    }

    /// The default listing is "what can be continued": an interrupted run and
    /// a run that stopped at a ceiling both qualify, which is the plan's
    /// distinction between a limit and an answer. A file left behind by an
    /// older release carrying `done` is excluded until asked for.
    #[tokio::test]
    async fn the_default_listing_offers_only_continuable_runs() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);

        // Stopped at the budget ceiling: more room would have helped, so it is
        // offered — but it is not "interrupted", and the count says so.
        let mut ceiling = save_checkpoint(&state, "r-budget", false).await;
        ceiling.stop_reason = Some(crate::computer::run_budget::StopReason::Budget);
        crate::computer::run_store::RunStore::for_paths(&state.paths)
            .save(&ceiling)
            .await
            .expect("save");

        // A concluded run: nothing to continue.
        let mut concluded = save_checkpoint(&state, "r-done", false).await;
        concluded.stop_reason = Some(crate::computer::run_budget::StopReason::Done);
        crate::computer::run_store::RunStore::for_paths(&state.paths)
            .save(&concluded)
            .await
            .expect("save");

        let resp = handle_computer_runs(&req("computer.runs", serde_json::json!({})), &state).await;
        let payload = resp.payload.expect("payload");
        assert_eq!(payload["interrupted"], 0, "{payload}");
        let ids: Vec<&str> = payload["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["run_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["r-budget"], "only the ceiling stop is continuable: {payload}");

        // Asking for everything finds the concluded one too, with the reason.
        let all = handle_computer_runs(
            &req("computer.runs", serde_json::json!({"include_finished": true})),
            &state,
        )
        .await;
        let payload = all.payload.expect("payload");
        assert_eq!(payload["runs"].as_array().unwrap().len(), 2);
        assert!(
            payload["runs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["run_id"] == "r-done" && r["stop_reason"] == "done"),
            "{payload}"
        );
    }

    #[tokio::test]
    async fn resuming_an_unknown_run_is_not_found() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let resp = handle_computer_resume(
            &req("computer.resume", serde_json::json!({"run_id": "ghost"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "RUN_NOT_FOUND");
    }

    /// A checkpoint that recorded a step aimed at a sensitive control cannot be
    /// continued by a bare resume: the person approving the resume is approving
    /// the goal, not the password field a previous model call aimed at.
    #[tokio::test]
    async fn resuming_a_sensitive_run_needs_a_second_consent() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        save_checkpoint(&state, "r-sensitive", true).await;

        let resp = handle_computer_resume(
            &req("computer.resume", serde_json::json!({"run_id": "r-sensitive"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        let err = resp.error.unwrap();
        assert_eq!(err.code, "CONFIRMATION_REQUIRED");
        assert!(err.message.contains("sensitive"), "{}", err.message);
        // Nothing started.
        assert_eq!(state.tools.computer_runs.live_count().await, 0);
    }

    /// A concluded run is not resumable, and says so rather than silently
    /// starting a fresh loop at step N.
    #[tokio::test]
    async fn resuming_a_concluded_run_is_refused() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let mut cp = save_checkpoint(&state, "r-done", false).await;
        cp.stop_reason = Some(crate::computer::run_budget::StopReason::Done);
        crate::computer::run_store::RunStore::for_paths(&state.paths)
            .save(&cp)
            .await
            .expect("save");

        let resp = handle_computer_resume(
            &req("computer.resume", serde_json::json!({"run_id": "r-done"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "RUN_NOT_RESUMABLE");
    }

    /// Two loops on one desktop is a race, not a continuation.
    #[tokio::test]
    async fn resuming_a_live_run_is_refused() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        save_checkpoint(&state, "r-live", false).await;
        state.tools.computer_runs.create("r-live", "g").await;

        let resp = handle_computer_resume(
            &req("computer.resume", serde_json::json!({"run_id": "r-live"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "RUN_ALREADY_LIVE");
    }

    /// The resume builds the loop from the checkpoint: a test with no desktop
    /// adapter cannot start it, but the checkpoint is loaded before that
    /// refusal, which is the order that matters — the method refuses for the
    /// reason that is true, not "that file is unreadable".
    #[tokio::test]
    async fn resuming_without_an_adapter_refuses_for_that_reason() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        save_checkpoint(&state, "r-noadapter", false).await;
        let resp = handle_computer_resume(
            &req("computer.resume", serde_json::json!({"run_id": "r-noadapter"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "COMPUTER_UNAVAILABLE");
        // And the half-built run did not linger.
        assert_eq!(state.tools.computer_runs.live_count().await, 0);
    }

    // ── Recording and replay ────────────────────────────────────────────────

    /// Start, note a step, stop: the saved file is the workflow, and its
    /// parameters are already placeholders.
    #[tokio::test]
    async fn a_recording_round_trips_to_a_saved_workflow() {
        use crate::computer::types::{ClickTarget, DesktopAction, MouseButton};

        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let started = handle_computer_record_start(
            &req("computer.record_start", serde_json::json!({"name": "new folder"})),
            &state,
        )
        .await;
        assert!(started.ok, "{started:?}");

        state.tools.computer_recording.note(
            DesktopAction::Type {
                text: "Reports".to_string(),
                target: Some(ClickTarget::ElementLabel("Save As".to_string())),
            },
            "type Reports",
            false,
        );

        let stopped = handle_computer_record_stop(
            &req("computer.record_stop", serde_json::json!({"name": "new folder"})),
            &state,
        )
        .await;
        assert!(stopped.ok, "{stopped:?}");

        let store = WorkflowStore::for_paths(&state.paths);
        let saved = store
            .load("new folder")
            .await
            .expect("the workflow was saved");
        assert_eq!(saved.name, "new folder");
        assert_eq!(saved.steps.len(), 1);
        assert_eq!(saved.parameters, vec!["save_as".to_string()]);
        match &saved.steps[0].action {
            workflow::WorkflowAction::Desktop(DesktopAction::Type { text, .. }) => {
                assert_eq!(text, "{{save_as}}");
            }
            other => panic!("unexpected action: {other:?}"),
        }
        let _ = MouseButton::Left;
    }

    /// Starting a second recording is refused, and the first keeps its steps.
    #[tokio::test]
    async fn a_second_recording_is_refused_and_the_first_survives() {
        use crate::computer::types::{ClickTarget, DesktopAction, MouseButton, Point};

        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        handle_computer_record_start(&req("computer.record_start", serde_json::json!({})), &state)
            .await;
        state.tools.computer_recording.note(
            DesktopAction::Click {
                target: ClickTarget::Coordinate(Point::new(10, 20)),
                button: MouseButton::Left,
            },
            "click",
            false,
        );

        let second = handle_computer_record_start(
            &req("computer.record_start", serde_json::json!({"name": "other"})),
            &state,
        )
        .await;
        assert!(!second.ok);
        assert_eq!(second.error.unwrap().code, "ALREADY_RECORDING");

        let stopped = handle_computer_record_stop(
            &req("computer.record_stop", serde_json::json!({"name": "first"})),
            &state,
        )
        .await;
        assert!(stopped.ok, "{stopped:?}");
        assert_eq!(stopped.payload.unwrap()["steps"], 1);
    }

    #[tokio::test]
    async fn stopping_without_a_recording_is_refused() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let resp = handle_computer_record_stop(
            &req("computer.record_stop", serde_json::json!({"name": "nothing"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "NOT_RECORDING");
    }

    #[tokio::test]
    async fn record_stop_needs_a_name() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        handle_computer_record_start(&req("computer.record_start", serde_json::json!({})), &state)
            .await;
        let resp = handle_computer_record_stop(
            &req("computer.record_stop", serde_json::json!({})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "INVALID_PARAMS");
    }

    /// A workflow that is not on disk is refused by name, not silently
    /// replayed as an empty one.
    #[tokio::test]
    async fn playing_a_missing_workflow_is_refused() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let resp = handle_computer_play(
            &req("computer.play", serde_json::json!({"name": "does-not-exist"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "WORKFLOW_NOT_FOUND");
    }

    /// A parameter left unbound would type `{{name}}` into somebody's field.
    #[tokio::test]
    async fn playing_without_a_parameter_is_refused_and_names_it() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        save_workflow(&state, "needs-param", false).await;

        let resp = handle_computer_play(
            &req("computer.play", serde_json::json!({"name": "needs-param"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        let error = resp.error.unwrap();
        assert_eq!(error.code, "PARAMETERS_MISSING");
        assert!(error.message.contains("save_as"), "{}", error.message);
    }

    /// An empty value is not a binding.
    #[tokio::test]
    async fn playing_with_an_empty_parameter_is_refused() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        save_workflow(&state, "empty-param", false).await;

        let resp = handle_computer_play(
            &req(
                "computer.play",
                serde_json::json!({"name": "empty-param", "parameters": {"save_as": "  "}}),
            ),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "PARAMETERS_MISSING");
    }

    /// A sensitive step is asked about once, up front — and the refusal happens
    /// before any run is registered.
    #[tokio::test]
    async fn playing_a_sensitive_workflow_requires_confirmation() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        save_workflow(&state, "sensitive", true).await;

        let resp = handle_computer_play(
            &req(
                "computer.play",
                serde_json::json!({"name": "sensitive", "parameters": {"save_as": "x"}}),
            ),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "CONFIRMATION_REQUIRED");
        assert_eq!(state.tools.computer_runs.live_count().await, 0);
    }

    /// An unknown strategy is refused rather than defaulted: "I said abort" is
    /// not a thing to get wrong silently.
    #[tokio::test]
    async fn an_unknown_strategy_is_refused() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        save_workflow(&state, "strategy", false).await;

        let resp = handle_computer_play(
            &req(
                "computer.play",
                serde_json::json!({
                    "name": "strategy",
                    "parameters": {"save_as": "x"},
                    "strategy": "explode",
                }),
            ),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "INVALID_PARAMS");
    }

    /// With no adapter, the replay is refused *before* a run is registered —
    /// the caller must not be handed a `run_id` for a replay that cannot move.
    #[tokio::test]
    async fn playing_without_an_adapter_refuses_and_leaves_no_run() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        save_workflow(&state, "no-adapter", false).await;

        let resp = handle_computer_play(
            &req(
                "computer.play",
                serde_json::json!({"name": "no-adapter", "parameters": {"save_as": "x"}}),
            ),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, "COMPUTER_UNAVAILABLE");
        assert_eq!(state.tools.computer_runs.live_count().await, 0);
    }

    /// Write a workflow with one bound `Type` step to the state's store.
    async fn save_workflow(state: &Arc<GatewayState>, name: &str, sensitive: bool) {
        use crate::computer::types::{ClickTarget, DesktopAction};

        let mut wf = Workflow::new(name);
        wf.steps.push(RecordedStep {
            delay_before: Duration::ZERO,
            action: workflow::WorkflowAction::Desktop(DesktopAction::Type {
                text: "{{save_as}}".to_string(),
                target: Some(ClickTarget::ElementLabel("Save As".to_string())),
            }),
            description: "type the folder name".to_string(),
            sensitive,
        });
        wf.parameters = vec!["save_as".to_string()];
        WorkflowStore::for_paths(&state.paths)
            .save(&wf)
            .await
            .expect("workflow saved");
    }
}
