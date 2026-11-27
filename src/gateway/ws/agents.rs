//! Agent list/get/registry, health, system presence.

use super::*;

/// Filter a list of agent ids to what the caller may see.
///
/// Two gates, in order:
///
/// 1. An agent-bound connection sees only its one agent (unchanged): the client
///    is configured with a single agent's credential, so a sidebar offering the
///    others is a list of refusals waiting to happen.
/// 2. Every remaining caller is filtered by the router's own authorization —
///    the same `allows_principal` that `route()` applies. An agent the caller
///    cannot use is not listed, because a list is a promise that the name can
///    be acted on: `sessions.create` and `chat.send` both refuse it.
///
/// An unrestricted agent (no `allowed_users`) stays visible to everyone, so a
/// single-operator deployment's list is unchanged.
async fn restrict_agents(
    ids: impl IntoIterator<Item = String>,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> Vec<String> {
    let principal = ctx.user_id();
    let ids: Vec<String> = match ctx.bound_agent() {
        Some(bound) => ids.into_iter().filter(|id| id == bound).collect(),
        None => ids.into_iter().collect(),
    };
    let mut visible = Vec::with_capacity(ids.len());
    for id in ids {
        if state.agents.router.allows_principal(&id, principal).await {
            visible.push(id);
        }
    }
    visible
}

/// Refuse a request naming an agent the caller may not use.
///
/// `agents.list` and `agents.registry` leave an agent out of the listing when
/// the router would refuse it; the handlers that take an agent id explicitly
/// have to refuse it themselves, or the listing is only a suggestion — the id
/// is still known, still typed, and still answers. Returns the refusal frame,
/// or `None` when the caller may proceed (including every unrestricted agent,
/// which has no `allowed_users` and is visible to everyone).
pub(crate) async fn deny_agent(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
    agent_id: &str,
) -> Option<WsResponse> {
    if state
        .agents
        .router
        .allows_principal(agent_id, ctx.user_id())
        .await
    {
        return None;
    }
    warn!(
        "Refused {} for user {}: agent '{}' is not available to them",
        req.method,
        ctx.user_id(),
        agent_id
    );
    Some(WsResponse::err(
        &req.id,
        "FORBIDDEN",
        format!("Agent '{agent_id}' is not available to this user"),
    ))
}

pub(super) async fn handle_agents_list(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    let agents = {
        let agents = state.agents.agents.read().await;
        agents.keys().cloned().collect::<Vec<_>>()
    };
    let agents = restrict_agents(agents, state, ctx).await;

    // `agents` stays a plain array of ids so every existing client keeps
    // working; `owners` is the additive half — id → owner, for the ids the
    // deployment recorded one for (`agents.create` does).
    let mut owners = serde_json::Map::new();
    {
        let handles = state.agents.agents.read().await;
        for id in &agents {
            if let Some(owner) = handles.get(id).and_then(|h| h.config.owner.clone()) {
                owners.insert(id.clone(), serde_json::Value::String(owner));
            }
        }
    }
    WsResponse::ok(&req.id, serde_json::json!({ "agents": agents, "owners": owners }))
}

pub(super) async fn handle_agents_get(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct GetParams {
        agent_id: String,
    }

    let params: GetParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if let Some(refusal) = deny_agent(req, state, ctx, &params.agent_id).await {
        return refusal;
    }

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
    ctx: &RequestContext,
) -> WsResponse {
    // The same authorization `agents.list` applies: a registry entry for an
    // agent the caller may not use is a name the sidebar would offer and every
    // action would then refuse.
    let principal = ctx.user_id();
    let may_use = |id: &str| ctx.may_use_agent(id);
    let registry = state.agents.registry.read().await;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut entries: Vec<serde_json::Value> = Vec::new();

    // 1. Registry-discovered agents from disk
    for id in registry.list() {
        if !may_use(&id) || !state.agents.router.allows_principal(&id, principal).await {
            continue;
        }
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
        for (id, handle) in agents.iter() {
            if seen.contains(id) || !may_use(id) {
                continue;
            }
            if !state.agents.router.allows_principal(id, principal).await {
                continue;
            }
            let owner = handle.config.owner.clone();
            entries.push(serde_json::json!({
                "id": id,
                "display_name": id.as_str(),
                "emoji": "🤖",
                "is_valid": true,
                "has_heartbeat": false,
                "owner": owner,
            }));
        }
    }

    // Ownership for the disk-discovered entries too: it lives on the running
    // handle's config (stamped from `[agent_overrides]`), and a registry entry
    // for an agent that has not been spawned yet has none.
    {
        let agents = state.agents.agents.read().await;
        for entry in entries.iter_mut() {
            let Some(id) = entry.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            if entry.get("owner").is_some() {
                continue;
            }
            if let Some(owner) = agents.get(id).and_then(|h| h.config.owner.clone()) {
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert("owner".to_string(), serde_json::Value::String(owner));
                }
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
    ctx: &RequestContext,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct GetParams {
        agent_id: String,
    }
    let params: GetParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if let Some(refusal) = deny_agent(req, state, ctx, &params.agent_id).await {
        return refusal;
    }
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
pub(crate) async fn handle_agents_update(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct UpdateParams {
        agent_id: String,
        config: crate::agent::AgentConfig,
    }
    let params: UpdateParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if let Some(refusal) = deny_agent(req, state, ctx, &params.agent_id).await {
        return refusal;
    }
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

/// `agents.default` — switch the global default agent (`{ agent_id }`).
///
/// The default is where messages go when nothing else matched them (no
/// `@mention`, no session binding, no channel or workspace default), so it is
/// the routing authority itself — [`AgentRouter::set_default_agent`] — that is
/// changed here, not a cached copy. Sessions already bound to an agent keep
/// it; this only decides where new, unrouted conversations land.
///
/// A name that is not a real agent is refused rather than stored: an unrouted
/// message would then be routed to an agent that can never handle it, which
/// looks identical to the gateway silently dropping messages.
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
    if params.agent_id.trim().is_empty() {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "agent_id must not be empty");
    }

    let exists = {
        let agents = state.agents.agents.read().await;
        agents.contains_key(&params.agent_id)
    } || state.agents.registry.read().await.has(&params.agent_id);
    // The built-in default is always a legitimate destination even when it is
    // not currently spawned (a fresh gateway, or a test state) — it is the one
    // name that cannot be purged.
    if !exists && params.agent_id != crate::inbound::DEFAULT_AGENT_ID {
        return error_agent_not_found(&req.id);
    }

    state
        .agents
        .router
        .set_default_agent(params.agent_id.clone())
        .await;
    WsResponse::ok(&req.id, serde_json::json!({ "agent_id": params.agent_id, "default": true }))
}

/// `agents.memory.get` — the agent's `MEMORY.md` (`{ agent_id }`).
pub(crate) async fn handle_agents_memory_get(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct Params {
        agent_id: String,
    }
    let params: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if let Some(refusal) = deny_agent(req, state, ctx, &params.agent_id).await {
        return refusal;
    }
    // `agent_id` comes from the caller and is about to become a path: refuse
    // anything that would walk out of the agents directory. Two of these four
    // handlers write (`memory.clear`, `import`).
    if let Err(msg) = validate_agent_id(&params.agent_id) {
        return WsResponse::err(&req.id, "INVALID_PARAMS", msg);
    }
    let dir = state.paths.agents_dir().join(&params.agent_id);
    let path = dir.join("MEMORY.md");
    let content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    WsResponse::ok(&req.id, serde_json::json!({ "agent_id": params.agent_id, "memory": content }))
}

/// `agents.memory.clear` — clear the agent's `MEMORY.md` (`{ agent_id }`).
pub(crate) async fn handle_agents_memory_clear(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct Params {
        agent_id: String,
    }
    let params: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if let Some(refusal) = deny_agent(req, state, ctx, &params.agent_id).await {
        return refusal;
    }
    // `agent_id` comes from the caller and is about to become a path: refuse
    // anything that would walk out of the agents directory. Two of these four
    // handlers write (`memory.clear`, `import`).
    if let Err(msg) = validate_agent_id(&params.agent_id) {
        return WsResponse::err(&req.id, "INVALID_PARAMS", msg);
    }
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
pub(crate) async fn handle_agents_export(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct Params {
        agent_id: String,
    }
    let params: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if let Some(refusal) = deny_agent(req, state, ctx, &params.agent_id).await {
        return refusal;
    }
    // `agent_id` comes from the caller and is about to become a path: refuse
    // anything that would walk out of the agents directory. Two of these four
    // handlers write (`memory.clear`, `import`).
    if let Err(msg) = validate_agent_id(&params.agent_id) {
        return WsResponse::err(&req.id, "INVALID_PARAMS", msg);
    }
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
pub(crate) async fn handle_agents_import(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct Params {
        agent_id: String,
        files: std::collections::HashMap<String, String>,
    }
    let params: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if let Some(refusal) = deny_agent(req, state, ctx, &params.agent_id).await {
        return refusal;
    }
    // `agent_id` comes from the caller and is about to become a path: refuse
    // anything that would walk out of the agents directory. Two of these four
    // handlers write (`memory.clear`, `import`).
    if let Err(msg) = validate_agent_id(&params.agent_id) {
        return WsResponse::err(&req.id, "INVALID_PARAMS", msg);
    }
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
    let discovered = state
        .agents
        .registry
        .write()
        .await
        .discover(&state.paths)
        .await;
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
    use crate::gateway::state_tests::{make_test_ctx, make_test_state};
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
        let resp = handle_agents_list(&req("r1", None), &state, &make_test_ctx()).await;
        assert!(resp.ok);
        let agents = resp.payload.as_ref().unwrap()["agents"].as_array().unwrap();
        assert!(agents.is_empty(), "fresh state has no agents");
    }

    #[tokio::test]
    async fn agents_get_missing_params_errors() {
        let state = state().await;
        let resp = handle_agents_get(&req("r1", None), &state, &make_test_ctx()).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn agents_get_unknown_agent_not_found() {
        let state = state().await;
        let params = Some(serde_json::json!({ "agent_id": "ghost" }));
        let resp = handle_agents_get(&req("r1", params), &state, &make_test_ctx()).await;
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
        let resp = handle_agents_get(&req("r1", params), &state, &make_test_ctx()).await;
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
        let resp = handle_agents_get(&req("r2", params), &state, &make_test_ctx()).await;
        assert!(resp.ok);
        let payload = resp.payload.unwrap();
        let t = payload["config"]["temperature"].as_f64().unwrap();
        assert!((t - 0.3).abs() < 1e-6, "effective temperature {t} should be ~0.3");
    }

    /// `agent_id` becomes a path: a traversal must be refused before the join,
    /// not left to the filesystem.
    #[tokio::test]
    async fn agents_memory_get_rejects_path_traversal() {
        let state = state().await;
        for id in ["../../etc", "a/b", "..", ".hidden", ""] {
            let resp = handle_agents_memory_get(
                &req("r1", Some(serde_json::json!({ "agent_id": id }))),
                &state,
                &make_test_ctx(),
            )
            .await;
            assert!(!resp.ok, "agent_id {id:?} must be refused");
            assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS", "agent_id {id:?}");
        }
    }

    /// The check is about path shape only — the built-in default agent is a
    /// legitimate target for reading and export, even though the mutating
    /// handlers refuse it.
    #[tokio::test]
    async fn agents_memory_get_allows_the_default_agent() {
        let state = state().await;
        let resp = handle_agents_memory_get(
            &req("r1", Some(serde_json::json!({ "agent_id": "default" }))),
            &state,
            &make_test_ctx(),
        )
        .await;
        assert!(resp.ok, "the default agent must remain readable: {:?}", resp.error);
    }

    #[tokio::test]
    async fn agents_import_rejects_path_traversal() {
        let state = state().await;
        let resp = handle_agents_import(
            &req(
                "r1",
                Some(serde_json::json!({
                    "agent_id": "../../escape",
                    "files": { "SOUL.md": "x" }
                })),
            ),
            &state,
            &make_test_ctx(),
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn agents_get_invalid_params_errors() {
        let state = state().await;
        let resp = handle_agents_get(
            &req("r1", Some(serde_json::json!({ "nope": 1 }))),
            &state,
            &make_test_ctx(),
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn agents_registry_empty_ok() {
        let state = state().await;
        let resp = handle_agents_registry(&req("r1", None), &state, &make_test_ctx()).await;
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
            &make_test_ctx(),
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
            &make_test_ctx(),
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
            &make_test_ctx(),
        )
        .await;
        assert!(!resp.ok);
    }
    /// Switching the default must change where a message actually goes, not
    /// just what a getter reports: the value the client switches is the value
    /// the router reads for its global fallback.
    #[tokio::test]
    async fn agents_default_switches_where_unrouted_messages_go() {
        let state = state().await;
        state
            .agents
            .registry
            .write()
            .await
            .insert_for_test(crate::agent::AgentPersonality {
                id: "secretary".into(),
                identity: "Secretary".into(),
                is_valid: true,
                ..Default::default()
            });

        // Nothing is bound and no route matches, so this is the fallback.
        let before = state
            .agents
            .router
            .route(&crate::channels::IncomingMessage::new("u1", "s-under-test", "hi"), None)
            .await
            .routed()
            .expect("routed");
        assert_eq!(before.agent_id, crate::inbound::DEFAULT_AGENT_ID);

        let resp = handle_agents_default(
            &req("r1", Some(serde_json::json!({ "agent_id": "secretary" }))),
            &state,
        )
        .await;
        assert!(resp.ok, "default failed: {:?}", resp.error);
        assert_eq!(state.agents.router.default_agent().await, "secretary");

        let after = state
            .agents
            .router
            .route(&crate::channels::IncomingMessage::new("u2", "s-fresh", "hi"), None)
            .await
            .routed()
            .expect("routed");
        assert_eq!(after.agent_id, "secretary");
        assert!(after.is_fallback);
    }

    /// An unknown name is refused and leaves the previous default in place.
    /// Storing it would route every unrouted message to an agent that can
    /// never handle one — indistinguishable, from the outside, from the
    /// gateway dropping the message.
    #[tokio::test]
    async fn agents_default_refuses_an_unknown_agent() {
        let state = state().await;
        let resp = handle_agents_default(
            &req("r1", Some(serde_json::json!({ "agent_id": "ghost" }))),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "AGENT_NOT_FOUND");
        assert_eq!(state.agents.router.default_agent().await, crate::inbound::DEFAULT_AGENT_ID);
    }

    /// The built-in default is always a valid target, even in a state where it
    /// is not spawned and has no personality on disk.
    #[tokio::test]
    async fn agents_default_accepts_the_built_in_default() {
        let state = state().await;
        let resp = handle_agents_default(
            &req("r1", Some(serde_json::json!({ "agent_id": "default" }))),
            &state,
        )
        .await;
        assert!(resp.ok, "default failed: {:?}", resp.error);
        assert_eq!(state.agents.router.default_agent().await, "default");
    }

    #[tokio::test]
    async fn agents_memory_get_unknown_returns_empty() {
        let state = state().await;
        let resp = handle_agents_memory_get(
            &req("r1", Some(serde_json::json!({ "agent_id": "__no_such_agent__" }))),
            &state,
            &make_test_ctx(),
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
            &make_test_ctx(),
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }

    // ── Principal filtering ──────────────────────────────────────────────

    fn ctx_for(user: &str) -> RequestContext {
        RequestContext::new(user, crate::security::request_context::AuthSource::Session)
    }

    /// Seed the registry with a personality, which is what any disk-backed
    /// agent looks like to `agents.list` / `agents.registry` without spawning
    /// one.
    async fn seed_personality(state: &Arc<GatewayState>, id: &str) {
        state
            .agents
            .registry
            .write()
            .await
            .insert_for_test(crate::agent::AgentPersonality {
                id: id.into(),
                identity: id.into(),
                is_valid: true,
                ..Default::default()
            });
    }

    /// Insert a running handle. `agents.list` reads the runtime map, not the
    /// registry, so a test about its filtering must put something there.
    async fn seed_handle(state: &Arc<GatewayState>, id: &str, owner: Option<&str>) {
        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::mock::MockProvider::new());
        let tools = Arc::new(crate::tools::ToolRegistry::new());
        let mut cfg = crate::agent::AgentConfig::default();
        cfg.owner = owner.map(str::to_string);
        let agent = Arc::new(crate::agent::Agent::new(cfg.clone(), provider, tools));
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let (query_tx, _qrx) = tokio::sync::mpsc::channel(4);
        state.agents.agents.write().await.insert(
            id.to_string(),
            crate::gateway::AgentHandle {
                id: id.to_string(),
                config: cfg.clone(),
                base_config: cfg,
                tx,
                query_tx,
                busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                agent,
            },
        );
    }

    async fn listed_ids(resp: &WsResponse) -> Vec<String> {
        let mut ids: Vec<String> = resp.payload.as_ref().unwrap()["agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        ids.sort();
        ids
    }

    /// An agent with `allowed_users` set is not listed for a caller outside
    /// that list — a sidebar entry that every action then refuses is worse
    /// than an absent one. An unrestricted agent stays visible to everyone.
    #[tokio::test]
    async fn agents_list_hides_agents_the_caller_may_not_use() {
        let state = state().await;
        for id in ["mine", "theirs", "team"] {
            seed_handle(&state, id, None).await;
        }
        state
            .agents
            .router
            .set_agent_principals("mine", vec!["alice".to_string()])
            .await;
        state
            .agents
            .router
            .set_agent_principals("theirs", vec!["bob".to_string()])
            .await;
        // `team` has no list: unrestricted.

        let alice = handle_agents_list(&req("r1", None), &state, &ctx_for("alice")).await;
        assert_eq!(listed_ids(&alice).await, vec!["mine", "team"]);

        let bob = handle_agents_list(&req("r2", None), &state, &ctx_for("bob")).await;
        assert_eq!(listed_ids(&bob).await, vec!["team", "theirs"]);

        // The unrestricted agent is the one both lists share, and an
        // anonymous caller (no per-user identity) still sees it.
        let anon = handle_agents_list(&req("r3", None), &state, &make_test_ctx()).await;
        assert_eq!(listed_ids(&anon).await, vec!["team"]);
    }

    /// `agents.registry` applies the same gate, so the two surfaces cannot
    /// disagree about which names exist.
    #[tokio::test]
    async fn agents_registry_hides_agents_the_caller_may_not_use() {
        let state = state().await;
        for id in ["mine", "theirs"] {
            seed_personality(&state, id).await;
        }
        state
            .agents
            .router
            .set_agent_principals("theirs", vec!["bob".to_string()])
            .await;

        let resp = handle_agents_registry(&req("r1", None), &state, &ctx_for("alice")).await;
        assert!(resp.ok);
        let mut ids: Vec<String> = resp.payload.as_ref().unwrap()["agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["id"].as_str().map(str::to_string))
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["mine"]);
        assert_eq!(resp.payload.as_ref().unwrap()["count"], 1);
    }

    /// `agents.list` carries the recorded owner so a client can group "my
    /// agents" — for the agents a owner was recorded for, and only those.
    #[tokio::test]
    async fn agents_list_reports_owners_for_agents_that_have_one() {
        let state = state().await;
        seed_handle(&state, "owned", Some("alice")).await;
        seed_handle(&state, "team", None).await;

        let resp = handle_agents_list(&req("r1", None), &state, &ctx_for("alice")).await;
        assert_eq!(listed_ids(&resp).await, vec!["owned", "team"]);
        let owners = &resp.payload.as_ref().unwrap()["owners"];
        assert_eq!(owners["owned"], "alice");
        assert!(owners.get("team").is_none(), "a shared agent has no owner key");
    }

    /// A connection bound to one agent sees only that agent, whatever the
    /// router's lists say — the binding is the narrower gate.
    #[tokio::test]
    async fn agents_list_narrows_to_the_bound_agent() {
        let state = state().await;
        for id in ["alpha", "beta"] {
            seed_handle(&state, id, None).await;
        }
        let ctx = ctx_for("alice").with_bound_agent(Some("beta"));
        let resp = handle_agents_list(&req("r1", None), &state, &ctx).await;
        assert_eq!(listed_ids(&resp).await, vec!["beta"]);
    }

    /// Leaving an agent out of the list is only half the promise: every
    /// handler that takes an agent id must refuse the ones the list hides, or
    /// the id a client remembers from before (or guesses) still answers.
    #[tokio::test]
    async fn the_agent_scoped_handlers_refuse_an_agent_the_caller_may_not_use() {
        let state = state().await;
        seed_handle(&state, "bob-bot", None).await;
        state
            .agents
            .router
            .set_agent_principals("bob-bot", vec!["bob".to_string()])
            .await;
        let alice = ctx_for("alice");
        let params = || serde_json::json!({ "agent_id": "bob-bot" });

        let refusals = [
            handle_agents_get(&req("r1", Some(params())), &state, &alice).await,
            handle_agents_get_config(&req("r2", Some(params())), &state, &alice).await,
            handle_agents_memory_get(&req("r3", Some(params())), &state, &alice).await,
            handle_agents_memory_clear(&req("r4", Some(params())), &state, &alice).await,
            handle_agents_export(&req("r5", Some(params())), &state, &alice).await,
            handle_agents_import(
                &req(
                    "r6",
                    Some(serde_json::json!({ "agent_id": "bob-bot", "files": { "SOUL.md": "x" } })),
                ),
                &state,
                &alice,
            )
            .await,
            handle_agents_update(
                &req(
                    "r7",
                    Some(serde_json::json!({
                        "agent_id": "bob-bot",
                        "config": crate::agent::AgentConfig::default(),
                    })),
                ),
                &state,
                &alice,
            )
            .await,
        ];
        for (i, resp) in refusals.iter().enumerate() {
            assert!(!resp.ok, "call {i} must be refused: {resp:?}");
            assert_eq!(resp.error.as_ref().unwrap().code, "FORBIDDEN", "call {i}");
        }

        // The refusal is a refusal, not a rename: bob's own calls still answer.
        let bob = ctx_for("bob");
        assert!(
            handle_agents_get_config(&req("ok", Some(params())), &state, &bob)
                .await
                .ok,
            "bob must still reach his own agent"
        );
    }

    /// An unrestricted agent has no `allowed_users`, so the same handlers must
    /// stay open to everyone — a single-operator deployment must not notice
    /// this change.
    #[tokio::test]
    async fn an_unrestricted_agent_stays_readable_to_everyone() {
        let state = state().await;
        seed_handle(&state, "team", None).await;
        let params = || serde_json::json!({ "agent_id": "team" });

        for who in ["alice", "bob"] {
            let ctx = ctx_for(who);
            assert!(
                handle_agents_get_config(&req("r1", Some(params())), &state, &ctx)
                    .await
                    .ok,
                "{who} must reach the team agent"
            );
            assert!(
                handle_agents_memory_get(&req("r2", Some(params())), &state, &ctx)
                    .await
                    .ok,
                "{who} must read the team agent's memory"
            );
        }
    }
}
