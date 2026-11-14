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

use std::sync::Arc;
use std::time::Duration;

use super::super::{parse_params, WsRequest, WsResponse};
use crate::gateway::protocol::error_invalid_request;
use crate::gateway::{GatewayEvent, GatewayState};

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

    let adapter = state.tools.computer_adapter.read().await.clone();
    let Some(adapter) = adapter else {
        return WsResponse::err(
            &req.id,
            "COMPUTER_UNAVAILABLE",
            "No desktop adapter is available on this host",
        );
    };

    let (provider, model) = match driver_provider(state).await {
        Ok(pair) => pair,
        Err(resp) => return resp.with_id(&req.id),
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

    let loop_config = crate::computer::LoopConfig {
        max_steps: config.computer.max_steps,
        settle_delay_ms: config.computer.settle_delay_ms,
        ..Default::default()
    };

    let run_id = uuid::Uuid::new_v4().to_string();
    let run = state
        .tools
        .computer_runs
        .create(run_id.clone(), goal.clone())
        .await;

    let session_id = params.session_id.clone().filter(|s| !s.is_empty());
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

    let plan = crate::computer::run_executor::RunPlan {
        adapter,
        driver,
        loop_config,
        model_supports_vision,
        vision: vision_look(state, &config, image_limit).await,
        budget: config.computer.budget.clone(),
        observer: Some(observer),
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

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "run_id": run_id,
            "goal": goal,
            "model": model,
            "model_supports_vision": model_supports_vision,
            "max_steps": loop_config.max_steps,
            "budget": {
                "max_calls_per_run": config.computer.budget.max_calls_per_run,
                "max_images_per_run": config.computer.budget.max_images_per_run,
            },
        }),
    )
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
struct DriverRefusal {
    code: &'static str,
    message: String,
}

impl DriverRefusal {
    fn with_id(self, id: &str) -> WsResponse {
        WsResponse::err(id, self.code, self.message)
    }
}

/// The provider and model a run decides with.
///
/// Pinned to the concrete provider rather than routed per call: a run's steps
/// are one task, and switching providers midway would change the answer's
/// format (and its vision capability) halfway through a decision sequence.
async fn driver_provider(
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

#[derive(serde::Deserialize)]
struct RunParams {
    goal: String,
    #[serde(default)]
    session_id: Option<String>,
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
}
