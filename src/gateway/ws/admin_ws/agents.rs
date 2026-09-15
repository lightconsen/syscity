//! WS admin handlers: agents.

use std::sync::Arc;

use serde::Deserialize;

use super::super::{parse_params, WsRequest, WsResponse};
use crate::agent::{write_display_name, write_emoji};
use crate::gateway::GatewayState;

/// Longest accepted display name — keeps a rename from writing a whole
/// paragraph into IDENTITY.md.
const MAX_DISPLAY_NAME_LEN: usize = 120;

/// Longest accepted emoji. An emoji can be several code points (ZWJ sequences,
/// skin-tone modifiers), so this is looser than a single char.
const MAX_EMOJI_LEN: usize = 32;

/// Reject agent ids that would escape `agents_dir()` or name the built-in
/// default agent — the two ways a rename/delete could reach outside its lane.
fn validate_agent_id(id: &str) -> Result<(), &'static str> {
    if id.is_empty() {
        return Err("agent id is required");
    }
    if id == "default" {
        return Err("the default agent cannot be renamed or deleted");
    }
    if id.contains('/') || id.contains('\\') || id.contains("..") || id.starts_with('.') {
        return Err("agent id must not contain path separators");
    }
    Ok(())
}

/// Stop a spawned agent, drop it from the runtime map and announce the
/// shutdown. Returns `false` (no-op) when the agent is not running.
async fn unload_runtime_agent(state: &Arc<GatewayState>, id: &str) -> bool {
    let tx = {
        let agents = state.agents.agents.read().await;
        agents.get(id).map(|h| h.tx.clone())
    };
    let Some(tx) = tx else {
        return false;
    };
    if let Err(e) = tx
        .send(crate::gateway::runtime::AgentCommand::Shutdown)
        .await
    {
        tracing::warn!("Failed to send shutdown to agent {}: {}", id, e);
    }
    {
        let mut agents = state.agents.agents.write().await;
        agents.remove(id);
    }
    if let Err(e) = state
        .events
        .tx
        .send(crate::gateway::GatewayEvent::AgentStatus {
            agent_id: id.to_string(),
            status: crate::gateway::AgentStatus::Shutdown,
        })
    {
        tracing::warn!("Failed to broadcast agent shutdown event for {}: {}", id, e);
    }
    true
}

// ── Agents ──────────────────────────────────────────────────────────────

/// `agents.create` — create a new agent personality (`{ name, description, ... }`).
pub(crate) async fn handle_agents_create(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let config: crate::agent::AgentConfig = match parse_params(req) {
        Ok(c) => c,
        Err(res) => return res,
    };
    let agent_id = format!("agent-{}", uuid::Uuid::new_v4());
    match crate::gateway::agent_spawn::spawn_agent_inner(
        state.clone(),
        agent_id.clone(),
        config.clone(),
    )
    .await
    {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "id": agent_id,
                "status": "created",
                "config": {
                    "max_context_tokens": config.max_context_tokens,
                    "max_concurrent_tools": config.max_concurrent_tools,
                    "temperature": config.temperature,
                    "max_tokens": config.max_tokens,
                },
            }),
        ),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", format!("Failed to create agent: {}", e)),
    }
}

/// `agents.delete` — unload a running agent (`{ id }`).
///
/// This only stops the runtime instance: the personality on disk and its
/// registry entry are left alone, so a disk-backed agent comes back on the
/// next discovery. `agents.purge` is the one that removes it for good.
pub(crate) async fn handle_agents_delete(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let id = match parse_params::<serde_json::Value>(req) {
        Ok(v) => v["id"].as_str().unwrap_or("").to_string(),
        Err(res) => return res,
    };
    if !unload_runtime_agent(state, &id).await {
        return WsResponse::err(&req.id, "NOT_FOUND", "agent not found");
    }
    WsResponse::ok(&req.id, serde_json::json!({ "id": id, "status": "deleted" }))
}

/// `agents.purge` — delete an agent for good (`{ id }`).
///
/// Unloads the running instance (like `agents.delete`), then drops the
/// personality from the registry, clears the agent's `agent_models` /
/// `agent_overrides` entries, and removes `agents/<id>/` — personality files,
/// workspace, data and memory included. This is irreversible.
pub(crate) async fn handle_agents_purge(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let id = match parse_params::<serde_json::Value>(req) {
        Ok(v) => v["id"].as_str().unwrap_or("").to_string(),
        Err(res) => return res,
    };
    if let Err(msg) = validate_agent_id(&id) {
        return WsResponse::err(&req.id, "INVALID_PARAMS", msg);
    }

    let dir = state.paths.agents_dir().join(&id);
    let runtime_agent = {
        let agents = state.agents.agents.read().await;
        agents.contains_key(&id)
    };
    let registered = state.agents.registry.read().await.has(&id);
    let on_disk = tokio::fs::metadata(&dir).await.is_ok();
    if !runtime_agent && !registered && !on_disk {
        return WsResponse::err(&req.id, "NOT_FOUND", "agent not found");
    }

    // Config first: a failed persist must not leave the agent's files gone
    // while config still points at it.
    {
        let mut config_guard = state.config.write().await;
        let config = Arc::make_mut(&mut config_guard);
        let dropped_model = config.agent_models.remove(&id).is_some();
        let dropped_overrides = config.agent_overrides.remove(&id).is_some();
        if dropped_model || dropped_overrides {
            if let Some(config_path) = state.config_path.clone() {
                if let Err(e) =
                    crate::gateway::handlers::config::persist_config_atomic(config, &config_path)
                        .await
                {
                    return WsResponse::err(
                        &req.id,
                        "PERSIST_FAILED",
                        format!("Failed to persist config: {}", e),
                    );
                }
            }
        }
    }

    unload_runtime_agent(state, &id).await;
    state.agents.registry.write().await.remove(&id);
    // Conversations routed to the purged agent fall back to the built-in
    // default instead of resolving to an agent that no longer exists.
    if state.agents.route_resolver.default_agent().await == id {
        state
            .agents
            .route_resolver
            .set_default_agent("default")
            .await;
    }

    if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
        return WsResponse::err(
            &req.id,
            "INTERNAL",
            format!(
                "Agent unregistered but its directory could not be removed ({}): {}",
                dir.display(),
                e
            ),
        );
    }
    WsResponse::ok(&req.id, serde_json::json!({ "id": id, "status": "purged" }))
}

/// `agents.rename` — set an agent's display name and/or emoji
/// (`{ agent_id, display_name?, emoji? }`).
///
/// The display name lives in `IDENTITY.md` and the emoji in `SOUL.md`'s YAML
/// frontmatter, so this rewrites those files in place and re-runs discovery so
/// the registry reports the new values back. A running instance keeps the
/// system prompt it was spawned with — the rename lands on its next spawn.
pub(crate) async fn handle_agents_rename(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct RenameParams {
        agent_id: String,
        #[serde(default)]
        display_name: Option<String>,
        #[serde(default)]
        emoji: Option<String>,
    }

    let params: RenameParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if let Err(msg) = validate_agent_id(&params.agent_id) {
        return WsResponse::err(&req.id, "INVALID_PARAMS", msg);
    }

    let name = match params.display_name.as_deref().map(str::trim) {
        Some("") => {
            return WsResponse::err(&req.id, "INVALID_PARAMS", "display_name cannot be empty")
        }
        Some(n) => {
            // A newline would inject a line into the markdown file, and a
            // marker like `## name` would hijack the next read.
            if n.contains('\n') || n.contains('\r') || n.chars().count() > MAX_DISPLAY_NAME_LEN {
                return WsResponse::err(
                    &req.id,
                    "INVALID_PARAMS",
                    format!(
                        "display_name must be a single line of at most {} characters",
                        MAX_DISPLAY_NAME_LEN
                    ),
                );
            }
            Some(n.to_string())
        }
        None => None,
    };
    let emoji = match params.emoji.as_deref().map(str::trim) {
        Some("") => None,
        Some(e) => {
            if e.contains('\n') || e.contains('\r') || e.chars().count() > MAX_EMOJI_LEN {
                return WsResponse::err(
                    &req.id,
                    "INVALID_PARAMS",
                    format!("emoji must be a single line of at most {} characters", MAX_EMOJI_LEN),
                );
            }
            Some(e.to_string())
        }
        None => None,
    };
    if name.is_none() && emoji.is_none() {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "display_name or emoji is required");
    }

    let dir = state.paths.agents_dir().join(&params.agent_id);
    let registered = state.agents.registry.read().await.has(&params.agent_id);
    if !registered && tokio::fs::metadata(&dir).await.is_err() {
        return WsResponse::err(&req.id, "NOT_FOUND", "agent not found");
    }

    let identity_path = dir.join("IDENTITY.md");
    let soul_path = dir.join("SOUL.md");
    let mut identity = tokio::fs::read_to_string(&identity_path)
        .await
        .unwrap_or_default();
    let mut soul = tokio::fs::read_to_string(&soul_path)
        .await
        .unwrap_or_default();

    if let Some(name) = &name {
        identity = write_display_name(&identity, name);
    }
    if let Some(emoji) = &emoji {
        let (new_soul, new_identity) = write_emoji(&soul, &identity, emoji);
        soul = new_soul;
        identity = new_identity;
    }

    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return WsResponse::err(&req.id, "INTERNAL", format!("Failed to create agent dir: {}", e));
    }
    for (path, contents) in [(&identity_path, &identity), (&soul_path, &soul)] {
        if let Err(e) = tokio::fs::write(path, contents).await {
            return WsResponse::err(
                &req.id,
                "INTERNAL",
                format!("Failed to write {}: {}", path.display(), e),
            );
        }
    }

    // Re-read from disk so the response reports what the registry now serves.
    if let Err(e) = state
        .agents
        .registry
        .write()
        .await
        .discover(&state.paths)
        .await
    {
        tracing::warn!("Failed to rediscover agents after renaming '{}': {}", params.agent_id, e);
    }
    let (display_name, emoji) = {
        let registry = state.agents.registry.read().await;
        match registry.get(&params.agent_id) {
            Some(p) => (p.display_name(), p.emoji()),
            None => (params.agent_id.clone(), "🤖".to_string()),
        }
    };

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "agent_id": params.agent_id,
            "display_name": display_name,
            "emoji": emoji,
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

    /// Write an on-disk personality and make the registry see it, the way a
    /// marketplace install or a hand-authored agent would.
    async fn seed_agent(state: &Arc<GatewayState>, id: &str, identity: &str, soul: &str) {
        let dir = state.paths.agents_dir().join(id);
        tokio::fs::create_dir_all(dir.join("workspace"))
            .await
            .unwrap();
        tokio::fs::write(dir.join("workspace/notes.txt"), "keep me")
            .await
            .unwrap();
        tokio::fs::write(dir.join("IDENTITY.md"), identity)
            .await
            .unwrap();
        tokio::fs::write(dir.join("SOUL.md"), soul).await.unwrap();
        state
            .agents
            .registry
            .write()
            .await
            .discover(&state.paths)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn purge_removes_dir_registry_and_config() {
        let state = state().await;
        seed_agent(&state, "doomed", "# Doomed\n\n## name\nDoomed\n", "soul\n").await;
        {
            let mut guard = state.config.write().await;
            let config = Arc::make_mut(&mut guard);
            config.agent_models.insert("doomed".into(), "gpt-4o".into());
            config
                .apply_agent_override_field("doomed", "temperature", &serde_json::json!(0.3))
                .unwrap();
        }

        let resp =
            handle_agents_purge(&req("r1", Some(serde_json::json!({ "id": "doomed" }))), &state)
                .await;

        assert!(resp.ok, "purge failed: {:?}", resp.error);
        assert_eq!(resp.payload.as_ref().unwrap()["status"], "purged");
        assert!(!state.paths.agents_dir().join("doomed").exists());
        assert!(!state.agents.registry.read().await.has("doomed"));
        let config = state.config.read().await;
        assert!(!config.agent_models.contains_key("doomed"));
        assert!(!config.agent_overrides.contains_key("doomed"));
    }

    #[tokio::test]
    async fn purge_refuses_default_and_path_traversal() {
        let state = state().await;
        for id in ["default", "../escape", "a/b", ".hidden"] {
            let resp =
                handle_agents_purge(&req("r1", Some(serde_json::json!({ "id": id }))), &state)
                    .await;
            assert!(!resp.ok, "id {id} must be refused");
            assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
        }
        // Nothing was created above the agents dir.
        assert!(!state.paths.root().join("escape").exists());
    }

    #[tokio::test]
    async fn purge_unknown_agent_not_found() {
        let state = state().await;
        let resp =
            handle_agents_purge(&req("r1", Some(serde_json::json!({ "id": "ghost" }))), &state)
                .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "NOT_FOUND");
    }

    /// `agents.delete` stays an unload-only: the contract `agents.purge` exists
    /// to complement.
    #[tokio::test]
    async fn delete_does_not_touch_a_disk_backed_agent() {
        let state = state().await;
        seed_agent(&state, "on-disk", "# On Disk\n\n## name\nOn Disk\n", "soul\n").await;

        let resp =
            handle_agents_delete(&req("r1", Some(serde_json::json!({ "id": "on-disk" }))), &state)
                .await;

        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "NOT_FOUND");
        assert!(state.paths.agents_dir().join("on-disk").exists());
        assert!(state.agents.registry.read().await.has("on-disk"));
    }

    #[tokio::test]
    async fn rename_writes_name_and_emoji() {
        let state = state().await;
        seed_agent(
            &state,
            "renamable",
            "# Old Name\n\n## name\nOld Name\n\nNotes.\n",
            "---\nname: Old Name\nemoji: \"🎨\"\n---\n\n# Body\n",
        )
        .await;

        let resp = handle_agents_rename(
            &req(
                "r1",
                Some(serde_json::json!({
                    "agent_id": "renamable",
                    "display_name": "New Name",
                    "emoji": "🐼",
                })),
            ),
            &state,
        )
        .await;

        assert!(resp.ok, "rename failed: {:?}", resp.error);
        let payload = resp.payload.unwrap();
        assert_eq!(payload["display_name"], "New Name");
        assert_eq!(payload["emoji"], "🐼");

        // Written through to disk, and the registry serves the new values.
        let dir = state.paths.agents_dir().join("renamable");
        let identity = tokio::fs::read_to_string(dir.join("IDENTITY.md"))
            .await
            .unwrap();
        assert!(identity.contains("New Name"), "identity: {identity}");
        assert!(identity.contains("Notes."), "body survives: {identity}");
        let soul = tokio::fs::read_to_string(dir.join("SOUL.md"))
            .await
            .unwrap();
        assert!(soul.contains("emoji: \"🐼\""), "soul: {soul}");
        let registry = state.agents.registry.read().await;
        assert_eq!(registry.get("renamable").unwrap().display_name(), "New Name");
    }

    #[tokio::test]
    async fn rename_rejects_empty_or_multiline_name() {
        let state = state().await;
        seed_agent(&state, "renamable", "# Old\n", "soul\n").await;

        for name in [
            "",
            "   ",
            "two\nlines",
            &"x".repeat(MAX_DISPLAY_NAME_LEN + 1),
        ] {
            let resp = handle_agents_rename(
                &req(
                    "r1",
                    Some(serde_json::json!({ "agent_id": "renamable", "display_name": name })),
                ),
                &state,
            )
            .await;
            assert!(!resp.ok, "name {name:?} must be refused");
            assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
        }
        // Rejected before anything was written.
        let identity =
            tokio::fs::read_to_string(state.paths.agents_dir().join("renamable/IDENTITY.md"))
                .await
                .unwrap();
        assert_eq!(identity, "# Old\n");
    }

    #[tokio::test]
    async fn rename_requires_a_field_and_a_known_agent() {
        let state = state().await;
        let resp = handle_agents_rename(
            &req("r1", Some(serde_json::json!({ "agent_id": "ghost", "display_name": "X" }))),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "NOT_FOUND");

        seed_agent(&state, "renamable", "# Old\n", "soul\n").await;
        let resp = handle_agents_rename(
            &req("r1", Some(serde_json::json!({ "agent_id": "renamable" }))),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }
}
