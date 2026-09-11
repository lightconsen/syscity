//! Agent list/get/registry, health, system presence.

use super::*;
pub(super) async fn handle_agents_list(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let agents = {
        let agents = state.agents.agents.read().await;
        agents.keys().cloned().collect::<Vec<_>>()
    };

    WsResponse::ok(&req.id, serde_json::json!({ "agents": agents }))
}

pub(super) async fn handle_agents_get(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct GetParams {
        agent_id: String,
    }

    let params: GetParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let agent = {
        let agents = state.agents.agents.read().await;
        agents.get(&params.agent_id).cloned()
    };

    let personality = {
        let registry = state.agents.registry.read().await;
        registry.get(&params.agent_id).cloned()
    };

    match agent {
        Some(handle) => {
            let cfg = &handle.config;
            WsResponse::ok(
                &req.id,
                serde_json::json!({
                    "agent_id": params.agent_id,
                    "busy": handle.busy.load(std::sync::atomic::Ordering::Acquire),
                    "status": if handle.busy.load(std::sync::atomic::Ordering::Acquire) { "busy" } else { "idle" },
                    "config": {
                        "temperature": cfg.temperature,
                        "max_tokens": cfg.max_tokens,
                        "max_turns": cfg.max_turns,
                        "max_concurrent_tools": cfg.max_concurrent_tools,
                        "workspace_only": cfg.workspace_only,
                        "compaction_model": cfg.compaction_model,
                        "system_prompt": cfg.system_prompt,
                    },
                    "personality": personality.map(|p| serde_json::json!({
                        "display_name": p.display_name(),
                        "is_valid": p.is_valid,
                        "has_heartbeat": !p.heartbeat.is_empty(),
                        "has_soul": !p.soul.is_empty(),
                        "has_identity": !p.identity.is_empty(),
                        "has_memory": !p.memory.is_empty(),
                    })),
                }),
            )
        }
        None => {
            // Agent not spawned but may have a personality on disk. Show the
            // effective config (personality base + persisted overrides) so the
            // UI displays the values the agent will actually run with.
            if let Some(p) = personality {
                let mut cfg = p.to_agent_config();
                {
                    let config = state.config.read().await;
                    config.apply_agent_overrides(&params.agent_id, &mut cfg);
                }
                let config_json = match serde_json::to_value(&cfg) {
                    Ok(v) => v,
                    Err(e) => {
                        return WsResponse::err(
                            &req.id,
                            "SERIALIZE_FAILED",
                            format!("Failed to serialize agent config: {}", e),
                        );
                    }
                };
                WsResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "agent_id": params.agent_id,
                        "busy": false,
                        "status": "stopped",
                        "config": config_json,
                        "personality": {
                            "display_name": p.display_name(),
                            "is_valid": p.is_valid,
                            "has_heartbeat": !p.heartbeat.is_empty(),
                            "has_soul": !p.soul.is_empty(),
                            "has_identity": !p.identity.is_empty(),
                            "has_memory": !p.memory.is_empty(),
                        },
                    }),
                )
            } else {
                error_agent_not_found(&req.id)
            }
        }
    }
}

pub(super) async fn handle_agents_registry(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let registry = state.agents.registry.read().await;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut entries: Vec<serde_json::Value> = Vec::new();

    // 1. Registry-discovered agents from disk
    for id in registry.list() {
        if let Some(p) = registry.get(&id) {
            seen.insert(id.clone());
            entries.push(serde_json::json!({
                "id": p.id,
                "display_name": p.display_name(),
                "emoji": p.emoji(),
                "is_valid": p.is_valid,
                "has_heartbeat": !p.heartbeat.is_empty(),
            }));
        }
    }

    // 2. Runtime-spawned agents not in registry (e.g. default)
    {
        let agents = state.agents.agents.read().await;
        for id in agents.keys() {
            if !seen.contains(id) {
                entries.push(serde_json::json!({
                    "id": id,
                    "display_name": id.as_str(),
                    "emoji": "🤖",
                    "is_valid": true,
                    "has_heartbeat": false,
                }));
            }
        }
    }

    WsResponse::ok(&req.id, serde_json::json!({ "agents": entries, "count": entries.len() }))
}

pub(super) async fn handle_health(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let agent_count = {
        let agents = state.agents.agents.read().await;
        agents.len()
    };

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "status": "healthy",
            "agents": agent_count,
            "protocol_version": PROTOCOL_VERSION,
        }),
    )
}

pub(super) async fn handle_system_presence(req: &WsRequest) -> WsResponse {
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "online": true,
        }),
    )
}

/// `agents.get_config` — the full runtime `AgentConfig` for an agent
/// (`{ agent_id }`). Returns the serializable config for edit round-trips.
pub(crate) async fn handle_agents_get_config(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct GetParams {
        agent_id: String,
    }
    let params: GetParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let agent = {
        let agents = state.agents.agents.read().await;
        agents.get(&params.agent_id).cloned()
    };
    match agent {
        Some(handle) => {
            WsResponse::ok(&req.id, serde_json::to_value(&handle.config).unwrap_or_default())
        }
        None => {
            WsResponse::err(&req.id, "NOT_FOUND", format!("agent '{}' not found", params.agent_id))
        }
    }
}

/// `agents.update` — replace an agent's runtime config (`{ agent_id, config }`).
/// Only edits the running handle's `ConfigCell`; it does not persist to disk.
pub(crate) async fn handle_agents_update(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct UpdateParams {
        agent_id: String,
        config: crate::agent::AgentConfig,
    }
    let params: UpdateParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let agent = {
        let agents = state.agents.agents.read().await;
        agents.get(&params.agent_id).cloned()
    };
    match agent {
        Some(handle) => {
            match handle
                .tx
                .send(crate::gateway::AgentCommand::UpdateConfig(params.config))
                .await
            {
                Ok(()) => WsResponse::ok(
                    &req.id,
                    serde_json::json!({ "agent_id": params.agent_id, "updated": true }),
                ),
                Err(e) => {
                    WsResponse::err(&req.id, "INTERNAL", format!("Failed to update agent: {}", e))
                }
            }
        }
        None => {
            WsResponse::err(&req.id, "NOT_FOUND", format!("agent '{}' not found", params.agent_id))
        }
    }
}

/// `agents.default` — switch the default agent (`{ agent_id }`).
pub(crate) async fn handle_agents_default(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct Params {
        agent_id: String,
    }
    let params: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    state
        .agents
        .route_resolver
        .set_default_agent(&params.agent_id)
        .await;
    WsResponse::ok(&req.id, serde_json::json!({ "agent_id": params.agent_id, "default": true }))
}

/// `agents.memory.get` — the agent's `MEMORY.md` (`{ agent_id }`).
pub(crate) async fn handle_agents_memory_get(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct Params {
        agent_id: String,
    }
    let params: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let dir = state.paths.agents_dir().join(&params.agent_id);
    let path = dir.join("MEMORY.md");
    let content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    WsResponse::ok(&req.id, serde_json::json!({ "agent_id": params.agent_id, "memory": content }))
}

/// `agents.memory.clear` — clear the agent's `MEMORY.md` (`{ agent_id }`).
pub(crate) async fn handle_agents_memory_clear(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct Params {
        agent_id: String,
    }
    let params: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let dir = state.paths.agents_dir().join(&params.agent_id);
    match tokio::fs::create_dir_all(&dir).await {
        Ok(()) => {}
        Err(e) => {
            return WsResponse::err(
                &req.id,
                "INTERNAL",
                format!("Failed to access agent dir: {}", e),
            )
        }
    }
    match tokio::fs::write(dir.join("MEMORY.md"), "").await {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({ "agent_id": params.agent_id, "cleared": true }),
        ),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", format!("Failed to clear memory: {}", e)),
    }
}

/// `agents.export` — the agent's on-disk personality files as JSON
/// (`{ agent_id }` → `{ agent_id, files: { "SOUL.md": ... } }`).
pub(crate) async fn handle_agents_export(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct Params {
        agent_id: String,
    }
    let params: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let dir = state.paths.agents_dir().join(&params.agent_id);
    const MD_FILES: &[&str] = &[
        "PERSONALITY.md",
        "SOUL.md",
        "IDENTITY.md",
        "BOOTSTRAP.md",
        "USER.md",
        "AGENTS.md",
        "TOOLS.md",
        "HEARTBEAT.md",
        "MEMORY.md",
    ];
    let mut files = serde_json::Map::new();
    for name in MD_FILES {
        let content = tokio::fs::read_to_string(dir.join(name))
            .await
            .unwrap_or_default();
        if !content.is_empty() {
            files.insert(name.to_string(), serde_json::Value::String(content));
        }
    }
    WsResponse::ok(&req.id, serde_json::json!({ "agent_id": params.agent_id, "files": files }))
}

/// `agents.import` — write an agent's personality files and rediscover it
/// (`{ agent_id, files: { "SOUL.md": ... } }`).
pub(crate) async fn handle_agents_import(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct Params {
        agent_id: String,
        files: std::collections::HashMap<String, String>,
    }
    let params: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let dir = state.paths.agents_dir().join(&params.agent_id);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return WsResponse::err(&req.id, "INTERNAL", format!("Failed to create agent dir: {}", e));
    }
    for (name, content) in &params.files {
        if name.contains('/') || name.contains("..") {
            return WsResponse::err(
                &req.id,
                "INVALID_PARAMS",
                format!("Invalid file name: {}", name),
            );
        }
        if let Err(e) = tokio::fs::write(dir.join(name), content).await {
            return WsResponse::err(
                &req.id,
                "INTERNAL",
                format!("Failed to write {}: {}", name, e),
            );
        }
    }
    let discovered = state.agents.registry.write().await.discover().await;
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "agent_id": params.agent_id,
            "imported": true,
            "registry_size": discovered.unwrap_or(0),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;

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

    #[tokio::test]
    async fn agents_list_empty_ok() {
        let state = state().await;
        let resp = handle_agents_list(&req("r1", None), &state).await;
        assert!(resp.ok);
        let agents = resp.payload.as_ref().unwrap()["agents"].as_array().unwrap();
        assert!(agents.is_empty(), "fresh state has no agents");
    }

    #[tokio::test]
    async fn agents_get_missing_params_errors() {
        let state = state().await;
        let resp = handle_agents_get(&req("r1", None), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_REQUEST");
    }

    #[tokio::test]
    async fn agents_get_unknown_agent_not_found() {
        let state = state().await;
        let params = Some(serde_json::json!({ "agent_id": "ghost" }));
        let resp = handle_agents_get(&req("r1", params), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "AGENT_NOT_FOUND");
    }

    #[tokio::test]
    async fn agents_get_stopped_shows_effective_config_with_overrides() {
        let state = state().await;

        // Register a stopped agent (personality only, not spawned).
        let personality = crate::agent::AgentPersonality {
            id: "alice".into(),
            identity: "Alice".into(),
            is_valid: true,
            ..Default::default()
        };
        state
            .agents
            .registry
            .write()
            .await
            .insert_for_test(personality);

        // Baseline: personality-derived temperature.
        let params = Some(serde_json::json!({ "agent_id": "alice" }));
        let resp = handle_agents_get(&req("r1", params), &state).await;
        assert!(resp.ok);
        let payload = resp.payload.unwrap();
        assert_eq!(payload["status"], "stopped");
        let base_t = payload["config"]["temperature"].as_f64().unwrap();
        assert!((base_t - 0.7).abs() < 1e-6, "base temperature {base_t}");

        // Persisted override must be reflected in the stopped-agent display.
        {
            let mut guard = state.config.write().await;
            let config = Arc::make_mut(&mut guard);
            config
                .apply_agent_override_field("alice", "temperature", &serde_json::json!(0.3))
                .unwrap();
        }
        let params = Some(serde_json::json!({ "agent_id": "alice" }));
        let resp = handle_agents_get(&req("r2", params), &state).await;
        assert!(resp.ok);
        let payload = resp.payload.unwrap();
        let t = payload["config"]["temperature"].as_f64().unwrap();
        assert!((t - 0.3).abs() < 1e-6, "effective temperature {t} should be ~0.3");
    }

    #[tokio::test]
    async fn agents_get_invalid_params_errors() {
        let state = state().await;
        let resp =
            handle_agents_get(&req("r1", Some(serde_json::json!({ "nope": 1 }))), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_REQUEST");
    }

    #[tokio::test]
    async fn agents_registry_empty_ok() {
        let state = state().await;
        let resp = handle_agents_registry(&req("r1", None), &state).await;
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["count"], 0);
        assert!(payload["agents"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn health_reports_healthy_zero_agents() {
        let state = state().await;
        let resp = handle_health(&req("r1", None), &state).await;
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["status"], "healthy");
        assert_eq!(payload["agents"], 0);
        assert!(payload["protocol_version"].is_number());
    }

    #[tokio::test]
    async fn system_presence_online() {
        let resp = handle_system_presence(&req("r1", None)).await;
        assert!(resp.ok);
        assert_eq!(resp.payload.as_ref().unwrap()["online"], true);
    }
    #[tokio::test]
    async fn agents_get_config_unknown_not_found() {
        let state = state().await;
        let resp = handle_agents_get_config(
            &req("r1", Some(serde_json::json!({ "agent_id": "nope" }))),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn agents_update_unknown_not_found() {
        let state = state().await;
        let mut cfg = crate::agent::AgentConfig::default();
        cfg.system_prompt = "x".into();
        let resp = handle_agents_update(
            &req("r1", Some(serde_json::json!({ "agent_id": "nope", "config": cfg }))),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn agents_update_missing_config_errors() {
        let state = state().await;
        let resp = handle_agents_update(
            &req("r1", Some(serde_json::json!({ "agent_id": "default" }))),
            &state,
        )
        .await;
        assert!(!resp.ok);
    }
    #[tokio::test]
    async fn agents_default_switches_resolver() {
        let state = state().await;
        let resp = handle_agents_default(
            &req("r1", Some(serde_json::json!({ "agent_id": "main" }))),
            &state,
        )
        .await;
        assert!(resp.ok, "default failed: {:?}", resp.error);
        assert_eq!(state.agents.route_resolver.default_agent().await, "main");
    }

    #[tokio::test]
    async fn agents_memory_get_unknown_returns_empty() {
        let state = state().await;
        let resp = handle_agents_memory_get(
            &req("r1", Some(serde_json::json!({ "agent_id": "__no_such_agent__" }))),
            &state,
        )
        .await;
        assert!(resp.ok, "memory.get failed: {:?}", resp.error);
        assert_eq!(resp.payload.as_ref().unwrap()["memory"], "");
    }

    #[tokio::test]
    async fn agents_import_invalid_filename_errors() {
        let state = state().await;
        let resp = handle_agents_import(
            &req(
                "r1",
                Some(serde_json::json!({
                    "agent_id": "t-import",
                    "files": { "../evil.md": "x" }
                })),
            ),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }
}
