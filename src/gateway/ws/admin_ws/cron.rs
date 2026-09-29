//! WS admin handlers: cron.

use std::sync::Arc;

use serde::Deserialize;

use super::super::{parse_params, WsRequest, WsResponse};
use crate::gateway::GatewayState;

/// Resolve the shared cron scheduler, or error if it is not running.
async fn cron_scheduler(
    state: &Arc<GatewayState>,
) -> Result<std::sync::Arc<tokio::sync::Mutex<crate::cron::cron::CronScheduler>>, WsResponse> {
    match state.scheduler.cron_scheduler.read().await.clone() {
        Some(s) => Ok(s),
        None => {
            Err(WsResponse::err("req".to_string(), "UNAVAILABLE", "Cron scheduler not running"))
        }
    }
}

/// `cron.get` — one job (`{ id }`).
pub(crate) async fn handle_cron_get(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let sched = match cron_scheduler(state).await {
        Ok(s) => s,
        Err(res) => return res,
    };
    let guard = sched.lock().await;
    match guard.get_job(&id).await {
        Some(job) => WsResponse::ok(&req.id, serde_json::to_value(&job).unwrap_or_default()),
        None => WsResponse::err(&req.id, "NOT_FOUND", "cron job not found"),
    }
}

/// `cron.enable` / `cron.disable` — `{ id, enabled }`.
pub(crate) async fn handle_cron_set_enabled(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    enabled: bool,
) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let sched = match cron_scheduler(state).await {
        Ok(s) => s,
        Err(res) => return res,
    };
    let guard = sched.lock().await;
    match guard.set_job_enabled(&id, enabled).await {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({ "success": true, "id": id, "enabled": enabled }),
        ),
        Err(e) => WsResponse::err(&req.id, "NOT_FOUND", e.to_string()),
    }
}

/// `cron.run` — trigger a job immediately (`{ id }`).
pub(crate) async fn handle_cron_run(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let sched = match cron_scheduler(state).await {
        Ok(s) => s,
        Err(res) => return res,
    };
    let guard = sched.lock().await;
    match guard.trigger_job(&id).await {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({ "success": true, "id": id, "triggered": true }),
        ),
        Err(e) => WsResponse::err(&req.id, "NOT_FOUND", e.to_string()),
    }
}

/// `cron.logs` — job state / last-run info (`{ id }`).
pub(crate) async fn handle_cron_logs(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    handle_cron_get(req, state).await
}

/// `cron.add` — add a job (`{ name, schedule, command }`).
pub(crate) async fn handle_cron_add(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        name: String,
        schedule: String,
        command: String,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    use std::str::FromStr;
    let schedule = match cron::Schedule::from_str(&p.schedule) {
        Ok(_) => crate::cron::cron::Schedule::Cron {
            expression: p.schedule.clone(),
            timezone: None,
            stagger_ms: None,
        },
        Err(e) => {
            return WsResponse::err(
                &req.id,
                "INVALID_PARAMS",
                format!("Invalid cron expression: {}", e),
            );
        }
    };
    let job_id = uuid::Uuid::new_v4().to_string();
    let job = crate::cron::cron::CronJob::new(
        job_id.clone(),
        p.name.clone(),
        schedule,
        crate::cron::cron::ExecutionTarget::shell(p.command),
    );
    let sched = match cron_scheduler(state).await {
        Ok(s) => s,
        Err(res) => return res,
    };
    let guard = sched.lock().await;
    match guard.add_job(job).await {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({ "success": true, "id": job_id, "name": p.name }),
        ),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", format!("Failed to add job: {}", e)),
    }
}

/// `cron.remove` — remove a job (`{ id }`).
pub(crate) async fn handle_cron_remove(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let sched = match cron_scheduler(state).await {
        Ok(s) => s,
        Err(res) => return res,
    };
    let guard = sched.lock().await;
    match guard.remove_job(&id).await {
        Ok(()) => WsResponse::ok(&req.id, serde_json::json!({ "success": true, "id": id })),
        Err(e) => WsResponse::err(&req.id, "NOT_FOUND", e.to_string()),
    }
}
