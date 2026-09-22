//! Tests for gateway startup, shutdown, the router and the MCP helpers.

use super::helpers::init_mcp_servers;
use super::shutdown::is_socket_lifetime_task;
use super::start::delegation_event_forwarder;
use super::*;
use crate::gateway::state_tests::make_test_state;

async fn state() -> Arc<GatewayState> {
    Arc::new(make_test_state(GatewayConfig::default()).await)
}

#[tokio::test]
async fn build_router_serves_live_endpoint() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let state = state().await;
    let app = build_router(state).await;

    let req = Request::builder().uri("/live").body(Body::empty()).unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["alive"], true);
}

#[tokio::test]
async fn build_router_serves_cloud_login_callback() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let state = state().await;
    let app = build_router(state).await;

    // The cloud OAuth return URL (default cloud.redirect_base) must serve
    // the SPA so its App.tsx can read `#token=` and persist the session.
    let req = Request::builder()
        .uri("/cloud/login/callback")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("<html") || html.contains("Syscity"));
}

#[tokio::test]
async fn build_router_serves_web_ws_route() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let state = state().await;
    let app = build_router(state).await;

    // The /ws route is registered (GET requires an upgrade; a plain GET
    // must be rejected with a non-panicking response).
    let req = Request::builder().uri("/ws").body(Body::empty()).unwrap();
    let response = app.oneshot(req).await.unwrap();
    // A missing upgrade should not produce a 200; any client error is fine.
    assert!(response.status().is_client_error() || response.status() == StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn stop_gateway_clean_shutdown() {
    let state = state().await;
    let token = CancellationToken::new();
    let result = stop_gateway(&token, &state).await;
    assert!(result.is_ok(), "stop_gateway should succeed: {:?}", result);
    assert!(token.is_cancelled());
}

/// The engine's fire-and-forget writes are not in the registry, so the
/// drain above cannot see them. Shutdown still has to wait for them: a turn
/// that is already marked complete in memory is lost if storage closes
/// under its insert.
#[tokio::test]
async fn stop_gateway_waits_for_engine_writes() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let state = state().await;
    let written = Arc::new(AtomicBool::new(false));

    // Longer than the rest of a test shutdown, so the write is still in
    // flight when shutdown would otherwise return.
    let guard = crate::agent::writes::pending().guard();
    let written_task = written.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        drop(guard);
        written_task.store(true, Ordering::SeqCst);
    });

    stop_gateway(&state.shutdown_token, &state).await.unwrap();

    assert!(
        written.load(Ordering::SeqCst),
        "shutdown closed storage while an engine write was still in flight"
    );
}

/// A task that is winding down gets to finish what it was writing.
///
/// The token is cancelled at the top of `stop_gateway`, but a task holding
/// an in-flight write only finishes a moment later. Aborting the instant
/// the token flips would truncate that write.
#[tokio::test]
async fn stop_gateway_drains_tasks_that_are_winding_down() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let state = state().await;
    let wrote = Arc::new(AtomicBool::new(false));
    let shutdown = state.shutdown_token.clone();
    let wrote_task = wrote.clone();
    let handle = tokio::spawn(async move {
        shutdown.cancelled().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        wrote_task.store(true, Ordering::SeqCst);
    });
    state.task_registry.insert_join("writer", handle).await;

    stop_gateway(&state.shutdown_token, &state).await.unwrap();

    assert!(
        wrote.load(Ordering::SeqCst),
        "the drain must let a task finish the write it is in the middle of"
    );
}

/// A task that never notices the shutdown token is aborted, but only after
/// it has had the window — and either way shutdown returns.
#[tokio::test]
async fn stop_gateway_aborts_tasks_that_ignore_the_shutdown_token() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct OnDrop(Arc<AtomicBool>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let state = state().await;
    let finished = Arc::new(AtomicBool::new(false));
    // Held open for the whole test: the task below must stay parked, not
    // finish on its own.
    let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
    let finished_task = finished.clone();
    let handle = tokio::spawn(async move {
        let _dropped = OnDrop(finished_task);
        let _ = rx.await;
    });
    state.task_registry.insert_join("stuck", handle).await;

    let started = tokio::time::Instant::now();
    stop_gateway(&state.shutdown_token, &state).await.unwrap();

    assert!(
        started.elapsed() >= BACKGROUND_DRAIN_TIMEOUT,
        "the task should have been given the drain window before being aborted"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !finished.load(Ordering::SeqCst) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        finished.load(Ordering::SeqCst),
        "a task that ignores shutdown must still be aborted, not left running"
    );
}

/// A connection pump is not a unit of work: its peer may never leave, so
/// shutdown aborts it rather than spending its window waiting.
#[tokio::test]
async fn stop_gateway_does_not_wait_on_socket_lifetime_tasks() {
    let state = state().await;
    let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let _ = rx.await;
    });
    state
        .task_registry
        .insert_join("ws:conn:test:recv", handle)
        .await;

    let started = tokio::time::Instant::now();
    stop_gateway(&state.shutdown_token, &state).await.unwrap();

    assert!(
        started.elapsed() < BACKGROUND_DRAIN_TIMEOUT,
        "a socket-lifetime task must not consume the drain window"
    );
    assert!(is_socket_lifetime_task("ws:conn:1:send"));
    assert!(is_socket_lifetime_task("openai:sse:abc"));
    assert!(!is_socket_lifetime_task("hooks:after:x:0"));
}

/// The task driving a shutdown is running *this* function, so it can never
/// be awaited and must not be aborted either: `/restart` reaches its
/// `process::exit` on the far side of the await.
#[tokio::test]
async fn stop_gateway_leaves_the_task_running_it_detached() {
    let state = state().await;
    let state_for_task = state.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        // Let the registry take the handle first, so this task really is
        // one of the tasks shutdown has to decide about.
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop_gateway(&state_for_task.shutdown_token, &state_for_task)
            .await
            .unwrap();
        // Surviving another yield is the assertion: an abort would have
        // landed at one of these await points.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = done_tx.send(());
    });
    state
        .task_registry
        .insert_join("system:restart", task)
        .await;

    timeout(Duration::from_secs(30), done_rx)
        .await
        .expect("shutdown cancelled the task that is running it")
        .expect("the restart task was dropped before it could finish");
}

#[tokio::test]
async fn init_mcp_servers_empty_config_noop() {
    let state = state().await;
    init_mcp_servers(state, &GatewayConfig::default());
}

#[tokio::test]
async fn init_mcp_servers_skips_non_auto_connect() {
    let state = state().await;
    let mut config = GatewayConfig::default();
    let mut server = crate::mcp::McpServerConfig::default();
    server.auto_connect = false;
    config.mcp.servers.insert("ghost".to_string(), server);
    init_mcp_servers(state, &config);
}

#[tokio::test]
async fn register_mcp_tools_without_client_noop() {
    let state = state().await;
    register_mcp_tools(&state, "ghost", &[], 0).await;
    let registry = state.tools.registry.clone();
    assert!(
        !registry
            .list()
            .iter()
            .any(|n| n.starts_with("mcp__ghost__")),
        "no MCP tools should be registered without a connected client"
    );
}

/// The store's event sink reaches WS clients as `DelegationTaskUpdated`
/// carrying the ROOT USER session — that routing is the whole point of
/// the `parent_session` column, so the forwarder is exercised end to end.
#[tokio::test]
async fn delegation_forwarder_publishes_task_updates_with_the_root_session() {
    let (sink_tx, sink_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let store = Arc::new(
        crate::delegation::DelegationTaskStore::new("sqlite::memory:")
            .await
            .expect("store")
            .with_event_sink(sink_tx),
    );
    let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(16);
    let shutdown = CancellationToken::new();

    let forwarder = tokio::spawn(delegation_event_forwarder(
        store.clone(),
        sink_rx,
        event_tx,
        shutdown.clone(),
    ));

    // A depth-2 child whose parent_session names the parent run: the
    // resolution must climb to the parent row's user session.
    store
        .create_task(crate::delegation::NewTask {
            id: "parent",
            root_id: "root",
            parent_id: None,
            depth: 1,
            agent_id: "manager",
            title: "Plan",
            parent_session: Some("user-session"),
        })
        .await
        .unwrap();
    store
        .create_task(crate::delegation::NewTask {
            id: "child",
            root_id: "root",
            parent_id: Some("parent"),
            depth: 2,
            agent_id: "worker",
            title: "Do",
            parent_session: Some("delegation:parent"),
        })
        .await
        .unwrap();

    let event = event_rx.recv().await.expect("first event");
    match event {
        GatewayEvent::DelegationTaskUpdated { session_id, task } => {
            assert_eq!(task.task_id, "parent");
            assert_eq!(session_id, "user-session");
            assert_eq!(task.status, "running");
            assert_eq!(task.duration_ms, None);
        }
        other => panic!("unexpected event: {other:?}"),
    }

    let event = event_rx.recv().await.expect("second event");
    match event {
        GatewayEvent::DelegationTaskUpdated { session_id, task } => {
            assert_eq!(task.task_id, "child");
            assert_eq!(
                session_id, "user-session",
                "the delegated child resolves to the root user session"
            );
        }
        other => panic!("unexpected event: {other:?}"),
    }

    // Terminal status carries the computed duration.
    store.set_status("child", "completed").await.unwrap();
    let event = event_rx.recv().await.expect("third event");
    match event {
        GatewayEvent::DelegationTaskUpdated { task, .. } => {
            assert_eq!(task.status, "completed");
            assert!(task.duration_ms.is_some(), "terminal rows carry a duration");
        }
        other => panic!("unexpected event: {other:?}"),
    }

    shutdown.cancel();
    forwarder.await.expect("forwarder exits on shutdown");
}
