//! Session list/create/delete/rename/pin/reset + subscribe/unsubscribe (incl. legacy).

use super::*;
pub(super) async fn handle_sessions_list(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let sessions: Vec<serde_json::Value> = if let Some(ref store) = state.agents.store {
        match store.find_sessions(None, None, None, false).await {
            Ok(rows) => rows
                .into_iter()
                .map(|meta| {
                    let name = meta.name.unwrap_or_else(|| {
                        if meta.message_count == 0 {
                            "New Session".to_string()
                        } else {
                            meta.last_activity.format("%b %d %H:%M").to_string()
                        }
                    });
                    serde_json::json!({
                        "session_id": meta.session_id,
                        "name": name,
                        "agent_id": meta.agent_id,
                        "channel": meta.channel,
                        "message_count": meta.message_count,
                        "last_activity": meta.last_activity.to_rfc3339(),
                        "is_active": meta.is_active,
                        "pinned": meta.pinned,
                        "model": meta.model,
                        "created_at": meta.created_at.to_rfc3339(),
                    })
                })
                .collect(),
            Err(e) => {
                tracing::warn!("Failed to list sessions from store: {}", e);
                Vec::new()
            }
        }
    } else {
        let mgr = state.agents.manager.read().await;
        mgr.list_sessions()
            .await
            .into_iter()
            .map(|id| serde_json::json!({ "session_id": id, "name": id }))
            .collect()
    };

    WsResponse::ok(&req.id, serde_json::json!({ "sessions": sessions }))
}

pub(super) async fn handle_sessions_create(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    let cg = conn.read().await;
    let channel = cg
        .client
        .as_ref()
        .map(|c| c.id.as_str())
        .unwrap_or("ws")
        .to_string();
    drop(cg);

    // Identity comes from the request context rather than being re-derived off
    // the connection: the context already resolves to the handshake identity
    // (`"anonymous"` when unauthenticated), so the two cannot drift apart.
    let user = ctx.user_id().to_string();

    #[derive(Debug, Deserialize)]
    struct CreateParams {
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        agent_id: Option<String>,
    }

    let params: CreateParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let session_id = params
        .session_id
        .unwrap_or_else(|| format!("{}:{}", channel, user));

    {
        let mut mgr = state.agents.manager.write().await;
        mgr.create_session(session_id.clone());
    }

    let mut metadata = crate::agent::session_store::SessionMetadata::new(
        &session_id,
        params.agent_id.as_deref().unwrap_or(""),
        &channel,
        &user,
    );
    metadata.bound_agent_id = params.agent_id.clone();

    if let Some(ref store) = state.agents.store {
        if let Err(e) = store.save_session(&session_id, &metadata, "{}").await {
            warn!("Failed to save session {}: {}", session_id, e);
        }
    }

    // Bind the session to the requested agent so future messages route there.
    if let Some(agent_id) = params.agent_id {
        if !agent_id.is_empty() {
            let route = crate::inbound::RouteResult {
                agent_id,
                workspace_id: None,
                persisted_binding: true,
                is_fallback: false,
            };
            state.agents.router.bind_session(&session_id, &route).await;
        }
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "session_id": session_id,
            "status": "created",
        }),
    )
}

pub(super) async fn handle_sessions_delete(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct DeleteParams {
        session_id: String,
    }

    let params: DeleteParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    {
        let mut mgr = state.agents.manager.write().await;
        mgr.terminate_session(&params.session_id).await;
    }

    if let Some(ref store) = state.agents.store {
        if let Err(e) = store.delete_session(&params.session_id).await {
            warn!("Failed to delete session {}: {}", params.session_id, e);
        }
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({ "status": "deleted", "session_id": params.session_id }),
    )
}

pub(super) async fn handle_sessions_rename(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct RenameParams {
        session_id: String,
        name: String,
    }

    let params: RenameParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let trimmed = params.name.trim();
    if trimmed.is_empty() {
        return WsResponse::err(&req.id, "INVALID_REQUEST", "session name cannot be empty");
    }

    if let Some(ref store) = state.agents.store {
        if let Err(e) = store.set_session_name(&params.session_id, trimmed).await {
            warn!("Failed to rename session {}: {}", params.session_id, e);
            return WsResponse::err(&req.id, "INTERNAL_ERROR", e.to_string());
        }
    }

    // Broadcast the rename event so all connected clients update immediately.
    if let Err(e) = state.events.tx.send(GatewayEvent::SessionRenamed {
        session_id: params.session_id.clone(),
        name: trimmed.to_string(),
    }) {
        tracing::debug!("No receivers for SessionRenamed event: {}", e);
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "status": "renamed",
            "session_id": params.session_id,
            "name": trimmed,
        }),
    )
}

pub(super) async fn handle_sessions_set_pinned(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct SetPinnedParams {
        session_id: String,
        pinned: bool,
    }

    let params: SetPinnedParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if let Some(ref store) = state.agents.store {
        if let Err(e) = store
            .set_session_pinned(&params.session_id, params.pinned)
            .await
        {
            warn!("Failed to set pinned status for session {}: {}", params.session_id, e);
            return WsResponse::err(&req.id, "INTERNAL_ERROR", e.to_string());
        }
    }

    if let Err(e) = state.events.tx.send(GatewayEvent::SessionPinned {
        session_id: params.session_id.clone(),
        pinned: params.pinned,
    }) {
        tracing::debug!("No receivers for SessionPinned event: {}", e);
    }

    WsResponse::ok(&req.id, serde_json::json!({ "status": "ok" }))
}

pub(super) async fn handle_sessions_set_model(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct SetModelParams {
        session_id: String,
        #[serde(default)]
        model: Option<String>,
    }

    let params: SetModelParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    // Treat an empty string the same as clearing the pin.
    let model = params.model.filter(|m| !m.is_empty());

    // Validate the model reference against the model router when pinning.
    // Accepts bare ids and provider-qualified cloud refs (`cloud/<id>`).
    if let Some(ref m) = model {
        if state
            .infra
            .model_router
            .provider_for_model(m)
            .await
            .is_none()
        {
            return WsResponse::err(&req.id, "MODEL_NOT_FOUND", format!("Unknown model: {}", m));
        }
    }

    if let Some(ref store) = state.agents.store {
        if let Err(e) = store
            .set_session_model(&params.session_id, model.as_deref())
            .await
        {
            warn!("Failed to set model for session {}: {}", params.session_id, e);
            return WsResponse::err(&req.id, "INTERNAL_ERROR", e.to_string());
        }
    }

    if let Err(e) = state.events.tx.send(GatewayEvent::SessionModelChanged {
        session_id: params.session_id.clone(),
        model: model.clone(),
    }) {
        tracing::debug!("No receivers for SessionModelChanged event: {}", e);
    }

    WsResponse::ok(&req.id, serde_json::json!({ "status": "ok" }))
}

pub(super) async fn handle_sessions_reset(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct ResetParams {
        session_id: String,
    }

    let params: ResetParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if let Err(e) = state.agents.acp.cancel(params.session_id.clone()).await {
        warn!("Failed to cancel session {} during reset: {}", params.session_id, e);
    }

    if let Some(ref store) = state.agents.store {
        if let Err(e) = store.delete_session(&params.session_id).await {
            warn!("Failed to delete session {} during reset: {}", params.session_id, e);
        }
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "status": "reset",
            "session_id": params.session_id,
        }),
    )
}

pub(super) async fn handle_sessions_subscribe(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    cmd_tx: &mpsc::Sender<WsCommand>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct SubscribeParams {
        session_ids: Vec<String>,
    }

    let params: SubscribeParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if let Err(e) = cmd_tx
        .send(WsCommand::Subscribe(params.session_ids.clone()))
        .await
    {
        warn!("Failed to send subscribe command: {}", e);
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "subscribed": params.session_ids,
        }),
    )
}

pub(super) async fn handle_sessions_unsubscribe(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    cmd_tx: &mpsc::Sender<WsCommand>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct UnsubscribeParams {
        session_ids: Vec<String>,
    }

    let params: UnsubscribeParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if let Err(e) = cmd_tx
        .send(WsCommand::Unsubscribe(params.session_ids.clone()))
        .await
    {
        warn!("Failed to send unsubscribe command: {}", e);
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "unsubscribed": params.session_ids,
        }),
    )
}

pub(super) async fn handle_legacy_subscribe(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    cmd_tx: &mpsc::Sender<WsCommand>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct LegacySubscribeParams {
        session_ids: Vec<String>,
    }

    let params: LegacySubscribeParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if let Err(e) = cmd_tx.send(WsCommand::Subscribe(params.session_ids)).await {
        warn!("Failed to send legacy subscribe command: {}", e);
    }

    WsResponse::ok(&req.id, serde_json::json!({ "status": "subscribed" }))
}

pub(super) async fn handle_legacy_unsubscribe(
    req: &WsRequest,
    _conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    cmd_tx: &mpsc::Sender<WsCommand>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct LegacyUnsubscribeParams {
        session_ids: Vec<String>,
    }

    let params: LegacyUnsubscribeParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if let Err(e) = cmd_tx
        .send(WsCommand::Unsubscribe(params.session_ids))
        .await
    {
        warn!("Failed to send legacy unsubscribe command: {}", e);
    }

    WsResponse::ok(&req.id, serde_json::json!({ "status": "unsubscribed" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::{
        make_test_conn, make_test_ctx, make_test_state, make_test_state_with_store,
    };
    use crate::gateway::GatewayConfig;
    use crate::model_router::{ProviderConfig, ProviderType};

    fn req(id: &str, method: &str, params: serde_json::Value) -> WsRequest {
        WsRequest {
            frame_type: "req".to_string(),
            id: id.to_string(),
            method: method.to_string(),
            params: Some(params),
        }
    }

    async fn register_model(state: &GatewayState, model_id: &str) {
        let config = ProviderConfig {
            provider_type: ProviderType::OpenAi,
            models: vec![model_id.to_string()],
            default_model: model_id.to_string(),
            api_key: "test-key".to_string().into(),
            api_keys: vec![],
            auth_profile: None,
            oauth: None,
            base_url: None,
            timeout: std::time::Duration::from_secs(30),
            max_retries: 3,
            retry_delay_ms: 1000,
        };
        state
            .infra
            .model_router
            .add_provider("openai", config)
            .await
            .expect("register provider");
    }

    #[tokio::test]
    async fn set_model_rejects_unknown_model() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);
        let res = handle_sessions_set_model(
            &req(
                "r1",
                "sessions.set_model",
                serde_json::json!({ "session_id": "s1", "model": "no-such-model" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("MODEL_NOT_FOUND"));
    }

    /// A provider-qualified cloud ref (`cloud/<id>`) is accepted when the cloud
    /// proxy serves the bare id; an unserved ref is still rejected.
    #[tokio::test]
    async fn set_model_accepts_qualified_cloud_ref() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let cloud = ProviderConfig {
            provider_type: ProviderType::OpenAi,
            models: vec!["deepseek-v4-pro".to_string()],
            default_model: "deepseek-v4-pro".to_string(),
            api_key: "test-key".to_string().into(),
            api_keys: vec![],
            auth_profile: None,
            oauth: None,
            base_url: None,
            timeout: std::time::Duration::from_secs(30),
            max_retries: 3,
            retry_delay_ms: 1000,
        };
        state
            .infra
            .model_router
            .add_provider("cloud", cloud)
            .await
            .expect("register cloud");
        let conn = make_test_conn(&["write"]);

        let res = handle_sessions_set_model(
            &req(
                "r1",
                "sessions.set_model",
                serde_json::json!({ "session_id": "s1", "model": "cloud/deepseek-v4-pro" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok, "qualified cloud ref should be accepted: {:?}", res.error);

        let res = handle_sessions_set_model(
            &req(
                "r2",
                "sessions.set_model",
                serde_json::json!({ "session_id": "s1", "model": "cloud/ghost" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("MODEL_NOT_FOUND"));
    }

    #[tokio::test]
    async fn set_model_roundtrip_with_event_and_list() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        register_model(&state, "gpt-4o").await;
        let conn = make_test_conn(&["write"]);
        let mut rx = state.events.tx.subscribe();

        // Pin to the registered concrete model ID.
        let res = handle_sessions_set_model(
            &req(
                "r1",
                "sessions.set_model",
                serde_json::json!({ "session_id": "s1", "model": "gpt-4o" }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok);
        match rx.try_recv() {
            Ok(GatewayEvent::SessionModelChanged { session_id, model }) => {
                assert_eq!(session_id, "s1");
                assert_eq!(model.as_deref(), Some("gpt-4o"));
            }
            other => panic!("expected SessionModelChanged event, got {:?}", other.is_ok()),
        }

        // sessions.list surfaces the pin.
        let res =
            handle_sessions_list(&req("l", "sessions.list", serde_json::json!({})), &state).await;
        let sessions = res
            .payload
            .and_then(|p| p.get("sessions").cloned())
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_default();
        let s1 = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some("s1"))
            .expect("s1 should be listed");
        assert_eq!(s1.get("model").and_then(|v| v.as_str()), Some("gpt-4o"));

        // Clearing the pin stores NULL and emits the event with model: null.
        let res = handle_sessions_set_model(
            &req(
                "r2",
                "sessions.set_model",
                serde_json::json!({ "session_id": "s1", "model": null }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok);
        match rx.try_recv() {
            Ok(GatewayEvent::SessionModelChanged { model, .. }) => assert_eq!(model, None),
            other => panic!("expected SessionModelChanged event, got {:?}", other.is_ok()),
        }
        let res =
            handle_sessions_list(&req("l", "sessions.list", serde_json::json!({})), &state).await;
        let sessions = res
            .payload
            .and_then(|p| p.get("sessions").cloned())
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_default();
        let s1 = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some("s1"))
            .expect("s1 should still be listed");
        assert!(s1.get("model").unwrap().is_null());
    }

    #[tokio::test]
    async fn set_model_empty_string_clears_pin() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        register_model(&state, "gpt-4o").await;
        let conn = make_test_conn(&["write"]);

        for value in [serde_json::json!("gpt-4o"), serde_json::json!("")] {
            let res = handle_sessions_set_model(
                &req(
                    "r",
                    "sessions.set_model",
                    serde_json::json!({ "session_id": "s1", "model": value }),
                ),
                &conn,
                &state,
            )
            .await;
            assert!(res.ok);
        }
        let store = state.agents.store.as_ref().unwrap();
        let loaded = store.load_session("s1").await.unwrap().unwrap();
        assert_eq!(loaded.metadata.model, None);
    }

    #[tokio::test]
    async fn create_persists_agent_binding() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);
        let res = handle_sessions_create(
            &req(
                "c",
                "sessions.create",
                serde_json::json!({ "session_id": "s1", "agent_id": "secretary" }),
            ),
            &conn,
            &state,
            &make_test_ctx(),
        )
        .await;
        assert!(res.ok);

        let store = state.agents.store.as_ref().unwrap();
        let loaded = store.load_session("s1").await.unwrap().unwrap();
        assert_eq!(loaded.metadata.agent_id, "secretary");
        assert_eq!(loaded.metadata.bound_agent_id.as_deref(), Some("secretary"));
    }

    #[tokio::test]
    async fn create_without_id_derives_from_channel_and_user() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        // Test conn has no client/user, so the id is "ws:anonymous".
        let conn = make_test_conn(&["write"]);
        let res = handle_sessions_create(
            &req("c", "sessions.create", serde_json::json!({})),
            &conn,
            &state,
            &make_test_ctx(),
        )
        .await;
        assert!(res.ok);
        let payload = res.payload.unwrap();
        assert_eq!(payload.get("session_id").and_then(|v| v.as_str()), Some("ws:anonymous"));
    }

    #[tokio::test]
    async fn rename_roundtrip_and_empty_rejected() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);
        handle_sessions_create(
            &req("c", "sessions.create", serde_json::json!({ "session_id": "s1" })),
            &conn,
            &state,
            &make_test_ctx(),
        )
        .await;

        let res = handle_sessions_rename(
            &req(
                "r",
                "sessions.rename",
                serde_json::json!({ "session_id": "s1", "name": "  My Chat  " }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok);
        let store = state.agents.store.as_ref().unwrap();
        let loaded = store.load_session("s1").await.unwrap().unwrap();
        assert_eq!(loaded.metadata.name.as_deref(), Some("My Chat"));

        let res = handle_sessions_rename(
            &req(
                "r2",
                "sessions.rename",
                serde_json::json!({ "session_id": "s1", "name": "   " }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("INVALID_REQUEST"));
    }

    #[tokio::test]
    async fn set_pinned_roundtrip_and_event() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);
        let mut rx = state.events.tx.subscribe();
        handle_sessions_create(
            &req("c", "sessions.create", serde_json::json!({ "session_id": "s1" })),
            &conn,
            &state,
            &make_test_ctx(),
        )
        .await;

        let res = handle_sessions_set_pinned(
            &req(
                "p",
                "sessions.set_pinned",
                serde_json::json!({ "session_id": "s1", "pinned": true }),
            ),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok);
        match rx.try_recv() {
            Ok(GatewayEvent::SessionPinned { session_id, pinned }) => {
                assert_eq!(session_id, "s1");
                assert!(pinned);
            }
            other => panic!("expected SessionPinned event, got {:?}", other.is_ok()),
        }

        let res =
            handle_sessions_list(&req("l", "sessions.list", serde_json::json!({})), &state).await;
        let sessions = res
            .payload
            .and_then(|p| p.get("sessions").cloned())
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_default();
        let s1 = sessions
            .iter()
            .find(|s| s.get("session_id").and_then(|v| v.as_str()) == Some("s1"))
            .unwrap();
        assert_eq!(s1.get("pinned").and_then(|v| v.as_bool()), Some(true));
    }

    #[tokio::test]
    async fn delete_and_reset_remove_session() {
        let state = Arc::new(make_test_state_with_store(GatewayConfig::default()).await);
        let conn = make_test_conn(&["write"]);
        for sid in ["s1", "s2"] {
            handle_sessions_create(
                &req("c", "sessions.create", serde_json::json!({ "session_id": sid })),
                &conn,
                &state,
                &make_test_ctx(),
            )
            .await;
        }

        let res = handle_sessions_delete(
            &req("d", "sessions.delete", serde_json::json!({ "session_id": "s1" })),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok);
        let res = handle_sessions_reset(
            &req("x", "sessions.reset", serde_json::json!({ "session_id": "s2" })),
            &conn,
            &state,
        )
        .await;
        assert!(res.ok);

        let store = state.agents.store.as_ref().unwrap();
        assert!(store.load_session("s1").await.unwrap().is_none());
        assert!(store.load_session("s2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_without_store_falls_back_to_manager() {
        // No store wired: the handler falls back to the in-memory session
        // manager and returns an empty list rather than erroring.
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res =
            handle_sessions_list(&req("l", "sessions.list", serde_json::json!({})), &state).await;
        assert!(res.ok);
        let sessions = res
            .payload
            .and_then(|p| p.get("sessions").cloned())
            .and_then(|s| s.as_array().cloned())
            .unwrap_or_default();
        assert!(sessions.is_empty());
    }
}
