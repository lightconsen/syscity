//! Cron + task-scheduler handlers.

use super::*;
pub(super) async fn handle_cron_list(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let jobs = match state.scheduler.cron_scheduler.read().await.clone() {
        Some(s) => s.lock().await.list_jobs().await,
        None => Vec::new(),
    };
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "jobs": jobs,
            "count": jobs.len(),
        }),
    )
}

pub(super) async fn handle_tasks_schedule(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct TaskSchedulePayload {
        id: String,
        name: String,
        #[serde(default)]
        description: String,
        schedule: ScheduleInput,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", content = "value")]
    enum ScheduleInput {
        #[serde(rename = "once")]
        Once(String),
        #[serde(rename = "interval")]
        Interval(u64),
        #[serde(rename = "cron")]
        Cron(String),
    }

    let payload: TaskSchedulePayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let scheduler = match state.scheduler.task_scheduler.read().await.clone() {
        Some(s) => s,
        None => {
            return WsResponse::err(
                &req.id,
                "SCHEDULER_UNAVAILABLE",
                "Task scheduler is not running",
            )
        }
    };

    let schedule = match payload.schedule {
        ScheduleInput::Once(s) => crate::planner::Schedule::once(s),
        ScheduleInput::Interval(seconds) => crate::planner::Schedule::interval(seconds),
        ScheduleInput::Cron(expr) => crate::planner::Schedule::cron(expr),
    };

    let task =
        crate::planner::ScheduledTask::new(payload.id.clone(), payload.name, schedule, vec![])
            .with_description(payload.description);

    let scheduler = scheduler.lock().await;
    match scheduler.add(task).await {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "status": "scheduled",
                "id": payload.id,
            }),
        ),
        Err(e) => WsResponse::err(&req.id, "SCHEDULE_FAILED", format!("{}", e)),
    }
}

pub(super) async fn handle_tasks_list(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let tasks = match state.scheduler.task_scheduler.read().await.clone() {
        Some(s) => s.lock().await.list().await,
        None => Vec::new(),
    };
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "tasks": tasks,
            "count": tasks.len(),
        }),
    )
}

pub(super) async fn handle_tasks_delete(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct TaskDeletePayload {
        id: String,
    }
    let payload: TaskDeletePayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let scheduler = match state.scheduler.task_scheduler.read().await.clone() {
        Some(s) => s,
        None => {
            return WsResponse::err(
                &req.id,
                "SCHEDULER_UNAVAILABLE",
                "Task scheduler is not running",
            )
        }
    };

    let scheduler = scheduler.lock().await;
    match scheduler.remove(&payload.id).await {
        Ok(true) => {
            WsResponse::ok(&req.id, serde_json::json!({ "status": "deleted", "id": payload.id }))
        }
        Ok(false) => {
            WsResponse::err(&req.id, "NOT_FOUND", format!("Task '{}' not found", payload.id))
        }
        Err(e) => WsResponse::err(&req.id, "DELETE_FAILED", format!("{}", e)),
    }
}

pub(super) async fn handle_tasks_enable(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct TaskEnablePayload {
        id: String,
    }
    let payload: TaskEnablePayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let scheduler = match state.scheduler.task_scheduler.read().await.clone() {
        Some(s) => s,
        None => {
            return WsResponse::err(
                &req.id,
                "SCHEDULER_UNAVAILABLE",
                "Task scheduler is not running",
            )
        }
    };

    let scheduler = scheduler.lock().await;
    match scheduler.enable(&payload.id).await {
        Ok(true) => {
            WsResponse::ok(&req.id, serde_json::json!({ "status": "enabled", "id": payload.id }))
        }
        Ok(false) => {
            WsResponse::err(&req.id, "NOT_FOUND", format!("Task '{}' not found", payload.id))
        }
        Err(e) => WsResponse::err(&req.id, "ENABLE_FAILED", format!("{}", e)),
    }
}

pub(super) async fn handle_tasks_disable(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct TaskDisablePayload {
        id: String,
    }
    let payload: TaskDisablePayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let scheduler = match state.scheduler.task_scheduler.read().await.clone() {
        Some(s) => s,
        None => {
            return WsResponse::err(
                &req.id,
                "SCHEDULER_UNAVAILABLE",
                "Task scheduler is not running",
            )
        }
    };

    let scheduler = scheduler.lock().await;
    match scheduler.disable(&payload.id).await {
        Ok(true) => {
            WsResponse::ok(&req.id, serde_json::json!({ "status": "disabled", "id": payload.id }))
        }
        Ok(false) => {
            WsResponse::err(&req.id, "NOT_FOUND", format!("Task '{}' not found", payload.id))
        }
        Err(e) => WsResponse::err(&req.id, "DISABLE_FAILED", format!("{}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::cron::CronScheduler;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;
    use crate::planner::TaskScheduler;

    fn req(id: &str, params: Option<serde_json::Value>) -> WsRequest {
        WsRequest {
            frame_type: "req".into(),
            id: id.into(),
            method: "x".into(),
            params,
        }
    }

    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    async fn state_with_task_scheduler() -> Arc<GatewayState> {
        let state = state().await;
        *state.scheduler.task_scheduler.write().await =
            Some(Arc::new(tokio::sync::Mutex::new(TaskScheduler::new())));
        state
    }

    async fn state_with_cron_scheduler() -> Arc<GatewayState> {
        let state = state().await;
        let (scheduler, _rx) = CronScheduler::new();
        *state.scheduler.cron_scheduler.write().await =
            Some(Arc::new(tokio::sync::Mutex::new(scheduler)));
        state
    }

    fn schedule_params(id: &str) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "id": id,
            "name": "test task",
            "schedule": { "type": "interval", "value": 10 },
        }))
    }

    #[tokio::test]
    async fn cron_list_empty_without_scheduler() {
        let state = state().await;
        let resp = handle_cron_list(&req("r1", None), &state).await;
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["count"], 0);
        assert!(payload["jobs"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cron_list_with_scheduler_empty() {
        let state = state_with_cron_scheduler().await;
        let resp = handle_cron_list(&req("r1", None), &state).await;
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["count"], 0);
    }

    #[tokio::test]
    async fn tasks_schedule_missing_params_errors() {
        let state = state().await;
        let resp = handle_tasks_schedule(&req("r1", None), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn tasks_schedule_scheduler_unavailable() {
        let state = state().await;
        let resp = handle_tasks_schedule(&req("r1", schedule_params("t1")), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "SCHEDULER_UNAVAILABLE");
    }

    #[tokio::test]
    async fn tasks_schedule_interval_success() {
        let state = state_with_task_scheduler().await;
        let resp = handle_tasks_schedule(&req("r1", schedule_params("t1")), &state).await;
        assert!(resp.ok, "schedule should succeed: {:?}", resp.error);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["status"], "scheduled");
        assert_eq!(payload["id"], "t1");
    }

    #[tokio::test]
    async fn tasks_list_empty_without_scheduler() {
        let state = state().await;
        let resp = handle_tasks_list(&req("r1", None), &state).await;
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["count"], 0);
    }

    #[tokio::test]
    async fn tasks_list_after_schedule_contains_task() {
        let state = state_with_task_scheduler().await;
        let _ = handle_tasks_schedule(&req("r1", schedule_params("t1")), &state).await;
        let resp = handle_tasks_list(&req("r2", None), &state).await;
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["count"], 1);
    }

    #[tokio::test]
    async fn tasks_delete_missing_params_errors() {
        let state = state().await;
        let resp = handle_tasks_delete(&req("r1", None), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn tasks_delete_scheduler_unavailable() {
        let state = state().await;
        let resp =
            handle_tasks_delete(&req("r1", Some(serde_json::json!({ "id": "t1" }))), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "SCHEDULER_UNAVAILABLE");
    }

    #[tokio::test]
    async fn tasks_delete_not_found() {
        let state = state_with_task_scheduler().await;
        let resp =
            handle_tasks_delete(&req("r1", Some(serde_json::json!({ "id": "ghost" }))), &state)
                .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn tasks_delete_success() {
        let state = state_with_task_scheduler().await;
        let _ = handle_tasks_schedule(&req("r1", schedule_params("t1")), &state).await;
        let resp =
            handle_tasks_delete(&req("r2", Some(serde_json::json!({ "id": "t1" }))), &state).await;
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["status"], "deleted");
    }

    #[tokio::test]
    async fn tasks_enable_disable_cycle() {
        let state = state_with_task_scheduler().await;
        let _ = handle_tasks_schedule(&req("r1", schedule_params("t1")), &state).await;

        let resp =
            handle_tasks_enable(&req("r2", Some(serde_json::json!({ "id": "ghost" }))), &state)
                .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "NOT_FOUND");

        let resp =
            handle_tasks_disable(&req("r3", Some(serde_json::json!({ "id": "t1" }))), &state).await;
        assert!(resp.ok);
        assert_eq!(resp.payload.as_ref().unwrap()["status"], "disabled");

        let resp =
            handle_tasks_disable(&req("r4", Some(serde_json::json!({ "id": "ghost" }))), &state)
                .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "NOT_FOUND");

        let resp =
            handle_tasks_enable(&req("r5", Some(serde_json::json!({ "id": "t1" }))), &state).await;
        assert!(resp.ok);
        assert_eq!(resp.payload.as_ref().unwrap()["status"], "enabled");
    }

    #[tokio::test]
    async fn tasks_enable_missing_params_errors() {
        let state = state().await;
        let resp = handle_tasks_enable(&req("r1", None), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }
}
