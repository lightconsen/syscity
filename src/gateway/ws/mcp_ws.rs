//! MCP server management handlers (list/presets/add/remove/connect/disconnect/auth_cancel).

use super::*;
pub(super) async fn handle_mcp_list(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let connected = state.tools.mcp_manager.list_servers().await;
    let config_guard = state.config.read().await;
    let mut servers: Vec<serde_json::Value> = Vec::new();
    for (id, cfg) in config_guard.mcp.servers.iter() {
        // Only a boolean — never the stored token values.
        let env_configured = state.secrets.route("mcp-env").has_entity(id).await;
        servers.push(serde_json::json!({
            "id": id,
            "transport": match cfg.transport {
                crate::mcp::McpTransport::Stdio => "stdio",
                crate::mcp::McpTransport::Sse => "sse",
                crate::mcp::McpTransport::StreamableHttp => "streamable_http",
                crate::mcp::McpTransport::InProcess => "in_process",
                #[cfg(feature = "cloud")]
                crate::mcp::McpTransport::Cloud { .. } => "cloud",
            },
            "command": cfg.command,
            "args": cfg.args,
            "url": cfg.url,
            "auto_connect": cfg.auto_connect,
            "connected": connected.contains(id),
            "env_configured": env_configured,
        }));
    }
    WsResponse::ok(&req.id, serde_json::json!({ "servers": servers }))
}

/// Metadata for one env variable a preset requires from the user.
#[derive(Debug, Clone, Deserialize)]
struct McpEnvVarMeta {
    #[serde(default)]
    required: bool,
    #[serde(default)]
    description: Option<String>,
}

/// A single entry in `~/.syscity/mcp.toml`.
#[derive(Debug, Deserialize)]
struct McpPresetEntry {
    display_name: String,
    description: String,
    logo_url: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    transport: String,
    /// Remote HTTP URL (SSE or streamable_http)
    url: Option<String>,
    /// OAuth / bearer auth configuration
    auth_type: Option<String>,
    client_id: Option<String>,
    auth_url: Option<String>,
    token_url: Option<String>,
    scopes: Option<String>,
    /// Env variables the user must supply (name → metadata).
    #[serde(default)]
    env: HashMap<String, McpEnvVarMeta>,
}

/// Env-var metadata from the embedded defaults, so installs with a stale
/// `~/.syscity/mcp.toml` (no `env` sections) still surface the env modal.
fn env_metadata_fallback() -> HashMap<String, HashMap<String, McpEnvVarMeta>> {
    toml::from_str::<HashMap<String, McpPresetEntry>>(crate::mcp::DEFAULT_PRESETS_TOML)
        .map(|m| m.into_iter().map(|(k, e)| (k, e.env)).collect())
        .unwrap_or_default()
}

/// Return MCP presets from `~/.syscity/mcp.toml`, each annotated with
/// whether the preset is currently enabled (present in config.toml).
pub(super) async fn handle_mcp_presets(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let presets: Vec<serde_json::Value> = match &state.mcps_path {
        Some(path) if path.exists() => match tokio::fs::read_to_string(path).await {
            Ok(content) => match toml::from_str::<HashMap<String, McpPresetEntry>>(&content) {
                Ok(map) => {
                    let cfg = state.config.read().await;
                    let fallback = env_metadata_fallback();
                    let mut presets: Vec<serde_json::Value> = map
                        .into_iter()
                        .map(|(name, entry)| {
                            let enabled = cfg.mcp.servers.contains_key(&name);
                            // Stale mcp.toml files lack env metadata — fill from
                            // the embedded defaults rather than rewriting the file.
                            let env_map = if entry.env.is_empty() {
                                fallback.get(&name).cloned().unwrap_or_default()
                            } else {
                                entry.env.clone()
                            };
                            let mut env_list: Vec<serde_json::Value> = env_map
                                .into_iter()
                                .map(|(var_name, meta)| {
                                    serde_json::json!({
                                        "name": var_name,
                                        "required": meta.required,
                                        "description": meta.description,
                                    })
                                })
                                .collect();
                            env_list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                            serde_json::json!({
                                "name": name,
                                "display_name": entry.display_name,
                                "description": entry.description,
                                "logo_url": entry.logo_url,
                                "command": entry.command,
                                "args": entry.args,
                                "transport": entry.transport,
                                "url": entry.url,
                                "auth_type": entry.auth_type,
                                "client_id": entry.client_id,
                                "auth_url": entry.auth_url,
                                "token_url": entry.token_url,
                                "scopes": entry.scopes,
                                "env": env_list,
                                "enabled": enabled,
                            })
                        })
                        .collect();
                    // Deterministic order: raw hash-map iteration is arbitrary
                    // and reshuffles on every refresh; sort by display name so
                    // the list stays stable across clicks and restarts.
                    presets.sort_by(|a, b| {
                        a["display_name"]
                            .as_str()
                            .unwrap_or_default()
                            .cmp(b["display_name"].as_str().unwrap_or_default())
                    });
                    presets
                }
                Err(e) => {
                    warn!("Failed to parse mcp.toml: {}", e);
                    Vec::new()
                }
            },
            Err(e) => {
                warn!("Failed to read mcp.toml: {}", e);
                Vec::new()
            }
        },
        _ => Vec::new(),
    };
    WsResponse::ok(&req.id, serde_json::json!({ "presets": presets }))
}

pub(super) async fn handle_mcp_add(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct McpAddPayload {
        id: String,
        transport: String,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        auth_type: Option<String>,
        client_id: Option<String>,
        auth_url: Option<String>,
        token_url: Option<String>,
        scopes: Option<String>,
        #[serde(default)]
        working_dir: Option<String>,
        #[serde(default = "default_true")]
        auto_connect: bool,
    }
    fn default_true() -> bool {
        true
    }

    let payload: McpAddPayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let transport = match payload.transport.as_str() {
        "sse" => crate::mcp::McpTransport::Sse,
        "streamable_http" => crate::mcp::McpTransport::StreamableHttp,
        _ => crate::mcp::McpTransport::Stdio,
    };

    // Split submitted env into `$VAR` references (kept in config.toml, resolved
    // at connect) and literal secret tokens (stored in ~/.syscity/mcp_env).
    let (env_refs, env_literals): (HashMap<String, String>, HashMap<String, String>) = payload
        .env
        .into_iter()
        .partition(|(_, v)| v.starts_with('$'));

    let config = crate::mcp::McpServerConfig {
        transport,
        command: payload.command,
        args: payload.args,
        url: payload.url,
        env: env_refs,
        resolved_env: env_literals.clone(),
        working_dir: payload.working_dir.map(std::path::PathBuf::from),
        auto_connect: payload.auto_connect,
        auth_type: payload.auth_type,
        client_id: payload.client_id,
        auth_url: payload.auth_url,
        token_url: payload.token_url,
        scopes: payload.scopes,
        ..Default::default()
    };

    let has_env = !env_literals.is_empty();
    let requires_auth = matches!(config.auth_type.as_deref(), Some("oauth2"))
        && !state.tools.mcp_manager.has_stored_token(&payload.id).await;
    let will_connect = payload.auto_connect && !requires_auth;

    // Literal tokens never touch config.toml — the persisted config carries a
    // cleared `resolved_env`.
    let persisted = crate::mcp::McpServerConfig {
        resolved_env: HashMap::new(),
        ..config.clone()
    };

    if has_env && will_connect {
        // Validate-first: connect with the submitted tokens before persisting
        // anything. On failure nothing is written, so re-enabling is clean and
        // the modal can surface the error.
        match state
            .tools
            .mcp_manager
            .connect(&payload.id, config.clone())
            .await
        {
            Ok(tools) => {
                if let Err(e) = state
                    .secrets
                    .route("mcp-env")
                    .set_all(&payload.id, &env_literals)
                    .await
                {
                    return WsResponse::err(
                        &req.id,
                        "MCP_ENV_SAVE_FAILED",
                        format!("Failed to save env tokens: {}", e),
                    );
                }
                {
                    let mut cfg_guard = state.config.write().await;
                    Arc::make_mut(&mut cfg_guard)
                        .mcp
                        .servers
                        .insert(payload.id.clone(), persisted.clone());
                }
                if let Err(e) = persist_config(state).await {
                    return e;
                }
                // Register tools immediately so agents can use them without
                // a daemon restart.
                crate::gateway::lifecycle::register_mcp_tools(
                    state,
                    &payload.id,
                    &tools,
                    persisted.max_tools,
                )
                .await;
                return WsResponse::ok(
                    &req.id,
                    serde_json::json!({
                        "status": "added",
                        "id": payload.id,
                        "requires_auth": false,
                    }),
                );
            }
            Err(e) => {
                return WsResponse::err(
                    &req.id,
                    "MCP_CONNECT_FAILED",
                    format!("Could not validate this server with the provided credentials: {}", e),
                );
            }
        }
    }

    if has_env {
        // No synchronous connect to validate against (auto_connect off or
        // oauth) — persist the tokens so they are not dropped.
        if let Err(e) = state
            .secrets
            .route("mcp-env")
            .set_all(&payload.id, &env_literals)
            .await
        {
            return WsResponse::err(
                &req.id,
                "MCP_ENV_SAVE_FAILED",
                format!("Failed to save env tokens: {}", e),
            );
        }
        {
            let mut cfg_guard = state.config.write().await;
            Arc::make_mut(&mut cfg_guard)
                .mcp
                .servers
                .insert(payload.id.clone(), persisted.clone());
        }
        if let Err(e) = persist_config(state).await {
            return e;
        }
        return WsResponse::ok(
            &req.id,
            serde_json::json!({
                "status": "added",
                "id": payload.id,
                "requires_auth": requires_auth,
            }),
        );
    }

    {
        let mut cfg_guard = state.config.write().await;
        Arc::make_mut(&mut cfg_guard)
            .mcp
            .servers
            .insert(payload.id.clone(), config.clone());
    }

    // Persist the config to disk first so it survives a failed connect.
    if let Err(e) = persist_config(state).await {
        return e;
    }

    if will_connect {
        match state
            .tools
            .mcp_manager
            .connect(&payload.id, config.clone())
            .await
        {
            Ok(tools) => {
                // Register tools immediately so agents can use them without
                // a daemon restart.
                crate::gateway::lifecycle::register_mcp_tools(
                    state,
                    &payload.id,
                    &tools,
                    config.max_tools,
                )
                .await;
            }
            Err(e) => {
                return WsResponse::err(
                    &req.id,
                    "MCP_CONNECT_FAILED",
                    format!("Saved config but failed to connect: {}", e),
                );
            }
        }
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "status": "added",
            "id": payload.id,
            "requires_auth": requires_auth,
        }),
    )
}

pub(super) async fn handle_mcp_remove(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct McpRemovePayload {
        id: String,
    }
    let payload: McpRemovePayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    if let Err(e) = state.tools.mcp_manager.disconnect(&payload.id).await {
        warn!("Failed to disconnect MCP server {}: {}", payload.id, e);
    }
    let prefix = format!("mcp__{}__", payload.id);
    state.tools.registry.deregister_prefix(&prefix);

    // Drop any stored OAuth tokens so a re-added server cannot reuse a
    // stale/revoked token.
    state.tools.mcp_manager.clear_oauth_token(&payload.id).await;

    // Drop any stored env tokens too.
    if let Err(e) = state
        .secrets
        .route("mcp-env")
        .delete_entity(&payload.id)
        .await
    {
        warn!("Failed to delete MCP env store for {}: {}", payload.id, e);
    }

    {
        let mut cfg_guard = state.config.write().await;
        Arc::make_mut(&mut cfg_guard)
            .mcp
            .servers
            .remove(&payload.id);
    }

    if let Err(e) = persist_config(state).await {
        return e;
    }

    WsResponse::ok(&req.id, serde_json::json!({ "status": "removed", "id": payload.id }))
}

pub(super) async fn handle_mcp_connect(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct McpConnectPayload {
        id: String,
    }
    let payload: McpConnectPayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let config = {
        let cfg = state.config.read().await;
        match cfg.mcp.servers.get(&payload.id) {
            Some(c) => c.clone(),
            None => {
                return WsResponse::err(
                    &req.id,
                    "MCP_NOT_FOUND",
                    format!("MCP server '{}' not configured", payload.id),
                )
            }
        }
    };

    // If the server uses OAuth, check for stored tokens first
    if config.auth_type.as_deref() == Some("oauth2") {
        if !state.tools.mcp_manager.has_stored_token(&payload.id).await {
            // No valid stored token — start the OAuth flow
            match state
                .tools
                .mcp_manager
                .start_oauth_flow(&payload.id, &config)
                .await
            {
                Ok(auth_url) => {
                    return WsResponse::err(
                        &req.id,
                        "MCP_AUTH_REQUIRED",
                        serde_json::json!({
                            "auth_url": auth_url,
                            "server_id": payload.id,
                        })
                        .to_string(),
                    );
                }
                Err(e) => {
                    return WsResponse::err(
                        &req.id,
                        "MCP_AUTH_FAILED",
                        format!("Failed to start OAuth flow: {}", e),
                    );
                }
            }
        }

        // Load stored token and set on a fresh client before connecting
        let tokens = state.tools.mcp_manager.load_stored_token(&payload.id).await;
        if let Some(tokens) = tokens {
            let mut client = crate::mcp::McpClient::new().with_timeout(config.timeout_secs);
            client.set_access_token(tokens.access_token.clone());

            match client.connect(config.clone()).await {
                Ok(()) => {
                    let tools = client.get_tools().to_vec();
                    let client_arc = std::sync::Arc::new(tokio::sync::RwLock::new(client));
                    // Register through the manager using the pre-authenticated client
                    if let Err(e) = state
                        .tools
                        .mcp_manager
                        .register_client(&payload.id, client_arc, config.clone())
                        .await
                    {
                        return WsResponse::err(&req.id, "MCP_CONNECT_FAILED", format!("{}", e));
                    }
                    crate::gateway::lifecycle::register_mcp_tools(
                        state,
                        &payload.id,
                        &tools,
                        config.max_tools,
                    )
                    .await;
                    return WsResponse::ok(
                        &req.id,
                        serde_json::json!({
                            "status": "connected",
                            "id": payload.id,
                            "tool_count": tools.len(),
                        }),
                    );
                }
                Err(e) => {
                    return WsResponse::err(&req.id, "MCP_CONNECT_FAILED", format!("{}", e));
                }
            }
        }
    }

    match state
        .tools
        .mcp_manager
        .connect(&payload.id, config.clone())
        .await
    {
        Ok(tools) => {
            crate::gateway::lifecycle::register_mcp_tools(
                state,
                &payload.id,
                &tools,
                config.max_tools,
            )
            .await;
            WsResponse::ok(
                &req.id,
                serde_json::json!({
                    "status": "connected",
                    "id": payload.id,
                    "tool_count": tools.len(),
                }),
            )
        }
        Err(e) => WsResponse::err(&req.id, "MCP_CONNECT_FAILED", format!("{}", e)),
    }
}

pub(super) async fn handle_mcp_disconnect(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct McpDisconnectPayload {
        id: String,
    }
    let payload: McpDisconnectPayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    match state.tools.mcp_manager.disconnect(&payload.id).await {
        Ok(()) => {
            let prefix = format!("mcp__{}__", payload.id);
            state.tools.registry.deregister_prefix(&prefix);
            WsResponse::ok(
                &req.id,
                serde_json::json!({ "status": "disconnected", "id": payload.id }),
            )
        }
        Err(e) => WsResponse::err(&req.id, "MCP_DISCONNECT_FAILED", format!("{}", e)),
    }
}

pub(super) async fn handle_mcp_auth_cancel(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct McpAuthCancelPayload {
        server_id: String,
    }
    let payload: McpAuthCancelPayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    state
        .tools
        .mcp_manager
        .cancel_oauth(&payload.server_id)
        .await;
    WsResponse::ok(
        &req.id,
        serde_json::json!({ "status": "cancelled", "server_id": payload.server_id }),
    )
}
