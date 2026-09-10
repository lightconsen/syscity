//! config.get / config.set over WebSocket.

use super::*;
use crate::gateway::apply_config::apply_config_path;
pub(super) async fn handle_config_get(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let config = state.config.read().await;
    let heartbeat = &config.heartbeat;
    let revision = crate::gateway::config_revision(&config);
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "revision": revision,
            "model": config.model,
            "model_provider": config.model_provider,
            "agent_models": config.agent_models,
            "default_agent": {
                "temperature": config.default_agent.temperature,
                "max_tokens": config.default_agent.max_tokens,
                "max_turns": config.default_agent.max_turns,
                "max_concurrent_tools": config.default_agent.max_concurrent_tools,
                "max_context_tokens": config.default_agent.max_context_tokens,
                "system_prompt": config.default_agent.system_prompt,
                "workspace_only": config.default_agent.workspace_only,
            },
            "agent_overrides": config.agent_overrides,
            "heartbeat": {
                "enabled": heartbeat.enabled,
                "interval_seconds": heartbeat.interval_seconds,
                "active_hours_start": heartbeat.active_hours_start,
                "active_hours_end": heartbeat.active_hours_end,
                "max_consecutive_idle": heartbeat.max_consecutive_idle,
            },
            "channels": config.channels.iter().map(|(k, v)| {
                serde_json::json!({
                    "name": k,
                    "channel_type": format!("{:?}", v.channel_type).to_lowercase(),
                    "enabled": v.enabled,
                    "agent_id": v.agent_id,
                    "dm_policy": format!("{:?}", v.dm_policy).to_lowercase(),
                    "require_mention": v.require_mention,
                    "has_credentials": !v.credentials.is_empty(),
                })
            }).collect::<Vec<_>>(),
            "auth_mode": config.security.auth_mode,
            "search": {
                "provider": config.search.provider,
                "providers": config.search.providers,
                "has_api_key": !config.search.api_key.is_empty(),
                "keys": {
                    "tavily": (!config.search.keys.get("tavily").is_none_or(|k| k.is_empty())).to_string(),
                    "serpapi": (!config.search.keys.get("serpapi").is_none_or(|k| k.is_empty())).to_string(),
                    "exa": (!config.search.keys.get("exa").is_none_or(|k| k.is_empty())).to_string(),
                    "firecrawl": (!config.search.keys.get("firecrawl").is_none_or(|k| k.is_empty())).to_string(),
                    "serper": (!config.search.keys.get("serper").is_none_or(|k| k.is_empty())).to_string(),
                    "bocha": (!config.search.keys.get("bocha").is_none_or(|k| k.is_empty())).to_string(),
                    "brave": (!config.search.keys.get("brave").is_none_or(|k| k.is_empty())).to_string(),
                },
            },
        }),
    )
}

pub(super) async fn handle_config_set(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct SetParams {
        path: String,
        value: serde_json::Value,
        #[serde(default)]
        base_revision: Option<String>,
    }

    let params: SetParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    // Empty base_revision is treated as "not supplied" (clients may default
    // the field to "").
    let base_revision = params.base_revision.as_deref().filter(|r| !r.is_empty());

    // Optimistic-lock fast-fail, run *before* the model-router side effect so a
    // stale `path="model"` write cannot switch the router before the conflict
    // is caught. The authoritative check under the write lock follows below.
    if let Some(expected) = base_revision {
        let current = {
            let cfg = state.config.read().await;
            crate::gateway::config_revision(&cfg)
        };
        if expected != current {
            return revision_conflict(req, expected, &current);
        }
    }

    // Handle model switching and agent model bindings outside the config write
    // lock so the lock is not held across an async model-router operation.
    let model_update = if params.path == "model" {
        if let Some(v) = params.value.as_str() {
            match state.infra.model_router.switch_default_model(v).await {
                Ok(()) => Some(v.to_string()),
                Err(e) => {
                    return WsResponse::err(
                        &req.id,
                        "CONFIG_ERROR",
                        format!("Failed to switch model: {}", e),
                    );
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    let agent_model_update = if params.path.starts_with("agent_models.") {
        let agent_id = params.path["agent_models.".len()..].to_string();
        match params.value.as_str().filter(|s| !s.is_empty()) {
            Some(v) => {
                let models = state.infra.model_router.models_with_providers().await;
                if !models.iter().any(|(_, id)| id == v) {
                    return WsResponse::err(
                        &req.id,
                        "MODEL_NOT_FOUND",
                        format!("Unknown model: {v}"),
                    );
                }
                Some((agent_id, v.to_string()))
            }
            // null / empty clears the binding; no validation needed.
            None => Some((agent_id, String::new())),
        }
    } else {
        None
    };

    let mut config_guard = state.config.write().await;

    // Authoritative CAS: under the write lock no other writer can slip between
    // this check and the mutation below.
    if let Some(expected) = base_revision {
        let current = crate::gateway::config_revision(&config_guard);
        if expected != current {
            return revision_conflict(req, expected, &current);
        }
    }

    let config = Arc::make_mut(&mut config_guard);

    // Agents whose running instance must receive an UpdateConfig push after
    // the write lock is dropped (the push re-reads state.config).
    let mut push_override_agent: Option<String> = None;
    let push_default_agent = params.path.starts_with("default_agent.");

    match params.path.as_str() {
        // Scalar `default_agent.*` paths share the same pure application logic
        // as the harness optimizer (single source of truth).
        p if apply_config_path(config, p, &params.value) => {}
        "model" => {
            if let Some(v) = model_update {
                config.model = v;
            }
        }
        "model_provider" => {
            if let Some(v) = params.value.as_str() {
                config.model_provider = v.to_string();
            }
        }
        p if p.starts_with("agent_models.") => {
            if let Some((agent_id, v)) = agent_model_update.clone() {
                if v.is_empty() {
                    config.agent_models.remove(&agent_id);
                } else {
                    config.agent_models.insert(agent_id, v);
                }
            }
        }
        "heartbeat.enabled" => {
            if let Some(v) = params.value.as_bool() {
                config.heartbeat.enabled = v;
            }
        }
        "heartbeat.interval_seconds" => {
            if let Some(v) = params.value.as_u64() {
                config.heartbeat.interval_seconds = v;
            }
        }
        "heartbeat.active_hours_start" => {
            if let Some(v) = params.value.as_str() {
                config.heartbeat.active_hours_start = v.to_string();
            }
        }
        "heartbeat.active_hours_end" => {
            if let Some(v) = params.value.as_str() {
                config.heartbeat.active_hours_end = v.to_string();
            }
        }
        "heartbeat.max_consecutive_idle" => {
            if let Some(v) = params.value.as_u64() {
                config.heartbeat.max_consecutive_idle = v as u32;
            }
        }
        "channels.add" => {
            #[derive(Debug, Deserialize)]
            struct ChannelAddPayload {
                name: String,
                channel_type: String,
                enabled: Option<bool>,
                agent_id: Option<String>,
                credentials: Option<HashMap<String, String>>,
            }
            let payload: ChannelAddPayload = match serde_json::from_value(params.value) {
                Ok(p) => p,
                Err(e) => return WsResponse::err(&req.id, "INVALID_PARAMS", e.to_string()),
            };
            let channel_type = match payload.channel_type.as_str() {
                "telegram" => crate::channels::ChannelType::Telegram,
                "discord" => crate::channels::ChannelType::Discord,
                "slack" => crate::channels::ChannelType::Slack,
                "whatsapp" => crate::channels::ChannelType::Whatsapp,
                "qq" => crate::channels::ChannelType::Qq,
                "feishu" => crate::channels::ChannelType::Feishu,
                "signal" => crate::channels::ChannelType::Signal,
                "imessage" => crate::channels::ChannelType::Imessage,
                "webchat" => crate::channels::ChannelType::Webchat,
                "websocket" => crate::channels::ChannelType::Websocket,
                "web_terminal" => crate::channels::ChannelType::WebTerminal,
                other => {
                    return WsResponse::err(
                        &req.id,
                        "INVALID_CHANNEL_TYPE",
                        format!("Unknown channel type: {}", other),
                    )
                }
            };
            let mut ch = crate::gateway::ChannelConfig::new(channel_type);
            if let Some(v) = payload.enabled {
                ch.enabled = v;
            }
            if let Some(v) = payload.agent_id {
                ch.agent_id = Some(v);
            }
            if let Some(v) = payload.credentials {
                ch.credentials = v;
            }
            config.channels.insert(payload.name.clone(), ch);
        }
        "channels.update" => {
            #[derive(Debug, Deserialize)]
            struct ChannelUpdatePayload {
                name: String,
                enabled: Option<bool>,
                agent_id: Option<String>,
                credentials: Option<HashMap<String, String>>,
            }
            let payload: ChannelUpdatePayload = match serde_json::from_value(params.value) {
                Ok(p) => p,
                Err(e) => return WsResponse::err(&req.id, "INVALID_PARAMS", e.to_string()),
            };
            match config.channels.get_mut(&payload.name) {
                Some(ch) => {
                    if let Some(v) = payload.enabled {
                        ch.enabled = v;
                    }
                    if let Some(v) = payload.agent_id {
                        ch.agent_id = Some(v);
                    }
                    if let Some(v) = payload.credentials {
                        ch.credentials = v;
                    }
                }
                None => {
                    return WsResponse::err(
                        &req.id,
                        "CHANNEL_NOT_FOUND",
                        format!("Channel '{}' not found", payload.name),
                    )
                }
            }
        }
        "channels.remove" => {
            if let Some(name) = params.value.as_str() {
                config.channels.remove(name);
            } else {
                return WsResponse::err(&req.id, "INVALID_PARAMS", "Expected channel name string");
            }
        }
        "channels.set_enabled" => {
            #[derive(Debug, Deserialize)]
            struct SetEnabledPayload {
                name: String,
                enabled: bool,
            }
            let payload: SetEnabledPayload = match serde_json::from_value(params.value) {
                Ok(p) => p,
                Err(e) => return WsResponse::err(&req.id, "INVALID_PARAMS", e.to_string()),
            };
            match config.channels.get_mut(&payload.name) {
                Some(ch) => ch.enabled = payload.enabled,
                None => {
                    return WsResponse::err(
                        &req.id,
                        "CHANNEL_NOT_FOUND",
                        format!("Channel '{}' not found", payload.name),
                    )
                }
            }
        }
        "search.provider" => {
            if let Some(v) = params.value.as_str() {
                config.search.provider = v.to_string();
            }
        }
        "search.providers" => {
            if let Some(arr) = params.value.as_array() {
                config.search.providers = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
            }
        }
        _ if params.path.starts_with("search.keys.") => {
            let key_name = params.path.strip_prefix("search.keys.").unwrap_or("");
            if !key_name.is_empty() {
                match &params.value {
                    serde_json::Value::String(v) if !v.is_empty() => {
                        config.search.keys.insert(key_name.to_string(), v.clone());
                    }
                    _ => {
                        config.search.keys.remove(key_name);
                    }
                }
            }
        }
        p if p.starts_with("agent_overrides.") => {
            let rest = &p["agent_overrides.".len()..];
            let Some((agent_id, field)) = rest.rsplit_once('.') else {
                return WsResponse::err(
                    &req.id,
                    "INVALID_PARAMS",
                    "Expected path agent_overrides.<agent_id>.<field>",
                );
            };
            match config.apply_agent_override_field(agent_id, field, &params.value) {
                Ok(true) => push_override_agent = Some(agent_id.to_string()),
                Ok(false) => {}
                Err(e) => return WsResponse::err(&req.id, "INVALID_PARAMS", e.to_string()),
            }
        }
        _ => {
            return WsResponse::err(
                &req.id,
                "UNKNOWN_CONFIG_PATH",
                format!("Unknown config path: {}", params.path),
            );
        }
    }

    // Mirror sensitive channel credentials into the secret store so a store
    // copy always exists (the plaintext credentials map stays for backward
    // compatibility until `secrets migrate` strips it).
    for (id, channel_config) in config.channels.iter() {
        if let Err(e) = state
            .secrets
            .persist_channel_secrets(id, &channel_config.credentials)
            .await
        {
            tracing::warn!("Failed to persist channel secrets for '{}': {}", id, e);
        }
    }

    // Persist config to disk so changes survive restarts and trigger hot-reload.
    // Keep the write lock held across persistence so concurrent writers cannot
    // overwrite our update before it is serialized.
    if let Some(config_path) = state.config_path.clone() {
        if let Err(e) = persist_config_atomic(&config_guard, &config_path).await {
            return WsResponse::err(
                &req.id,
                "PERSIST_FAILED",
                format!("Config updated in memory but failed to persist: {}", e),
            );
        }
    }
    let new_revision = crate::gateway::config_revision(&config_guard);
    drop(config_guard);

    // Push the recomputed effective config to running agents so the change
    // takes effect from their next turn instead of only on respawn.
    if let Some(agent_id) = push_override_agent {
        push_agent_param_update(state, &agent_id).await;
    }
    if push_default_agent {
        push_default_agent_update(state).await;
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "status": "updated",
            "path": params.path,
            "revision": new_revision,
        }),
    )
}

/// Optimistic-lock conflict response: the config changed since `expected` was
/// read. Built as a literal because `WsResponse::err` always drops the payload,
/// and the client needs the current revision to retry.
/// First 12 hex chars of a hash, for a readable error message.
fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

fn revision_conflict(req: &WsRequest, expected: &str, current: &str) -> WsResponse {
    WsResponse {
        frame_type: "res",
        id: req.id.clone(),
        ok: false,
        payload: Some(serde_json::json!({
            "current_revision": current,
            "expected_revision": expected,
        })),
        error: Some(WsError {
            code: "REVISION_CONFLICT".to_string(),
            message: format!(
                "Config changed since revision {} was read (current {}). \
                 Re-run config.get and retry.",
                short_hash(expected),
                short_hash(current)
            ),
        }),
    }
}

/// Recompute `agent_id`'s effective config (`base_config + agent_overrides`)
/// and push it to the running instance. No-op when the agent is not running —
/// the persisted override is merged at next spawn.
async fn push_agent_param_update(state: &Arc<GatewayState>, agent_id: &str) {
    let handle = {
        let agents = state.agents.agents.read().await;
        agents.get(agent_id).cloned()
    };
    let Some(handle) = handle else {
        debug!("Agent '{}' is not running; override applies on next spawn", agent_id);
        return;
    };
    let mut effective = handle.base_config.clone();
    {
        let config = state.config.read().await;
        config.apply_agent_overrides(agent_id, &mut effective);
    }
    if let Err(e) = handle
        .tx
        .send(crate::gateway::AgentCommand::UpdateConfig(effective))
        .await
    {
        warn!("Failed to push config update to agent '{}': {}", agent_id, e);
    }
}

/// Push the current `default_agent` config to the running `default` agent,
/// re-applying the spawn-time identity augmentation to the system prompt.
///
/// `pub(crate)` so the harness scalar optimizer can hot-update a running
/// default agent with the same push the `config.set` handler uses.
pub(crate) async fn push_default_agent_update(state: &Arc<GatewayState>) {
    let handle = {
        let agents = state.agents.agents.read().await;
        agents.get("default").cloned()
    };
    let Some(handle) = handle else {
        debug!("Default agent is not running; change applies on next spawn");
        return;
    };
    let effective = {
        let config = state.config.read().await;
        let mut effective = crate::gateway::augment_default_agent_config(&config.default_agent);
        effective.agent_id = Some("default".to_string());
        config.apply_agent_overrides("default", &mut effective);
        effective
    };
    if let Err(e) = handle
        .tx
        .send(crate::gateway::AgentCommand::UpdateConfig(effective))
        .await
    {
        warn!("Failed to push config update to default agent: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
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

    async fn register_provider(state: &GatewayState, name: &str, models: &[&str]) {
        let config = ProviderConfig {
            provider_type: ProviderType::OpenAi,
            models: models.iter().map(|s| s.to_string()).collect(),
            default_model: models.first().map(|s| s.to_string()).unwrap_or_default(),
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
            .add_provider(name, config)
            .await
            .expect("register provider");
    }

    async fn set_and_ok(
        state: &Arc<GatewayState>,
        path: &str,
        value: serde_json::Value,
    ) -> WsResponse {
        handle_config_set(
            &req("r", "config.set", serde_json::json!({ "path": path, "value": value })),
            state,
        )
        .await
    }

    #[tokio::test]
    async fn config_get_reports_all_sections() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res = handle_config_get(&req("g", "config.get", serde_json::json!({})), &state).await;
        assert!(res.ok);
        let p = res.payload.expect("payload");
        assert!(p["model"].as_str().is_some());
        assert!(p["model_provider"].as_str().is_some());
        assert!(p["agent_models"].is_object());
        assert!(p["heartbeat"]["enabled"].is_boolean());
        assert!(p["channels"].is_array());
        assert!(p["auth_mode"].as_str().is_some());
        assert!(p["search"]["provider"].as_str().is_some());
    }

    #[tokio::test]
    async fn config_set_agent_models_binds_and_clears() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        register_provider(&state, "openai", &["gpt-4o"]).await;
        let res = set_and_ok(&state, "agent_models.main", serde_json::json!("gpt-4o")).await;
        assert!(res.ok);
        assert_eq!(
            state.config.read().await.agent_models.get("main").cloned(),
            Some("gpt-4o".into())
        );

        // Null clears the binding.
        let res = set_and_ok(&state, "agent_models.main", serde_json::Value::Null).await;
        assert!(res.ok);
        assert!(!state.config.read().await.agent_models.contains_key("main"));
    }

    #[tokio::test]
    async fn config_set_agent_models_rejects_unknown_model() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res = set_and_ok(&state, "agent_models.main", serde_json::json!("nope")).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("MODEL_NOT_FOUND"));
        assert!(!state.config.read().await.agent_models.contains_key("main"));
    }

    #[tokio::test]
    async fn config_set_model_switches_router_and_config() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        register_provider(&state, "openai", &["gpt-4o"]).await;
        let res = set_and_ok(&state, "model", serde_json::json!("gpt-4o")).await;
        assert!(res.ok);
        assert_eq!(state.infra.model_router.get_default_model().await, "gpt-4o");
        assert_eq!(state.config.read().await.model, "gpt-4o");
    }

    #[tokio::test]
    async fn config_set_unknown_path_errors() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res = set_and_ok(&state, "no.such.path", serde_json::json!(1)).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("UNKNOWN_CONFIG_PATH"));
    }

    #[tokio::test]
    async fn config_set_default_agent_roundtrip() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        assert!(
            set_and_ok(&state, "default_agent.temperature", serde_json::json!(0.7))
                .await
                .ok
        );
        assert!(
            set_and_ok(&state, "default_agent.max_tokens", serde_json::json!(2048))
                .await
                .ok
        );
        assert!(
            set_and_ok(&state, "default_agent.max_turns", serde_json::json!(5))
                .await
                .ok
        );
        assert!(
            set_and_ok(&state, "default_agent.workspace_only", serde_json::json!(true))
                .await
                .ok
        );

        let res = handle_config_get(&req("g", "config.get", serde_json::json!({})), &state).await;
        let p = res.payload.expect("payload");
        // Temperature is stored as f32, so the f64 readback is inexact.
        let t = p["default_agent"]["temperature"]
            .as_f64()
            .expect("temperature");
        assert!((t - 0.7).abs() < 1e-6, "temperature {t} should be ~0.7");
        assert_eq!(p["default_agent"]["max_tokens"].as_u64(), Some(2048));
        assert_eq!(p["default_agent"]["max_turns"].as_u64(), Some(5));
        assert_eq!(p["default_agent"]["workspace_only"].as_bool(), Some(true));
    }

    #[tokio::test]
    async fn config_set_heartbeat_roundtrip() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        assert!(
            set_and_ok(&state, "heartbeat.enabled", serde_json::json!(true))
                .await
                .ok
        );
        assert!(
            set_and_ok(&state, "heartbeat.interval_seconds", serde_json::json!(42))
                .await
                .ok
        );
        assert!(
            set_and_ok(&state, "heartbeat.active_hours_start", serde_json::json!("09:00"))
                .await
                .ok
        );

        let res = handle_config_get(&req("g", "config.get", serde_json::json!({})), &state).await;
        let p = res.payload.expect("payload");
        assert_eq!(p["heartbeat"]["enabled"].as_bool(), Some(true));
        assert_eq!(p["heartbeat"]["interval_seconds"].as_u64(), Some(42));
        assert_eq!(p["heartbeat"]["active_hours_start"].as_str(), Some("09:00"));
    }

    #[tokio::test]
    async fn config_set_channels_add_update_remove() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let add = set_and_ok(
            &state,
            "channels.add",
            serde_json::json!({
                "name": "tg",
                "channel_type": "telegram",
                "enabled": true,
            }),
        )
        .await;
        assert!(add.ok);
        let added = state.config.read().await.channels.get("tg").cloned();
        assert!(added.is_some(), "channel should be added to config");
        assert_eq!(added.unwrap().enabled, true);

        // Update flips enabled off.
        let upd = set_and_ok(
            &state,
            "channels.update",
            serde_json::json!({ "name": "tg", "enabled": false }),
        )
        .await;
        assert!(upd.ok);
        assert_eq!(
            state
                .config
                .read()
                .await
                .channels
                .get("tg")
                .unwrap()
                .enabled,
            false
        );

        // Remove deletes the channel.
        let rm = set_and_ok(&state, "channels.remove", serde_json::json!("tg")).await;
        assert!(rm.ok);
        assert!(state.config.read().await.channels.get("tg").is_none());
    }

    #[tokio::test]
    async fn config_set_agent_overrides_roundtrip() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res =
            set_and_ok(&state, "agent_overrides.alice.temperature", serde_json::json!(0.3)).await;
        assert!(res.ok);

        let res = handle_config_get(&req("g", "config.get", serde_json::json!({})), &state).await;
        let p = res.payload.expect("payload");
        let t = p["agent_overrides"]["alice"]["temperature"]
            .as_f64()
            .expect("temperature override");
        assert!((t - 0.3).abs() < 1e-6, "temperature {t} should be ~0.3");

        // Null clears the override; the now-empty entry is dropped.
        let res =
            set_and_ok(&state, "agent_overrides.alice.temperature", serde_json::Value::Null).await;
        assert!(res.ok);
        let res = handle_config_get(&req("g2", "config.get", serde_json::json!({})), &state).await;
        let p = res.payload.expect("payload");
        assert!(p["agent_overrides"]["alice"].is_null());
    }

    #[tokio::test]
    async fn config_set_agent_overrides_rejects_unknown_field() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res = set_and_ok(&state, "agent_overrides.alice.nope", serde_json::json!(1)).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("INVALID_PARAMS"));
    }

    #[tokio::test]
    async fn config_set_agent_overrides_pushes_to_running_agent() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);

        // Insert a fake running agent with a known base config.
        let provider: Arc<dyn crate::providers::Provider> =
            Arc::new(crate::providers::mock::MockProvider::new());
        let tools = Arc::new(crate::tools::ToolRegistry::new());
        let base = crate::agent::AgentConfig {
            temperature: 0.8,
            ..Default::default()
        };
        let agent = Arc::new(crate::agent::Agent::new(base.clone(), provider, tools));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let (query_tx, _query_rx) = tokio::sync::mpsc::channel(1);
        let handle = crate::gateway::AgentHandle {
            id: "alice".to_string(),
            config: base.clone(),
            base_config: base,
            tx,
            query_tx,
            busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent,
        };
        state
            .agents
            .agents
            .write()
            .await
            .insert("alice".to_string(), handle);

        let res =
            set_and_ok(&state, "agent_overrides.alice.temperature", serde_json::json!(0.3)).await;
        assert!(res.ok);

        // The push sends UpdateConfig(base + overrides) on the command channel.
        let cmd = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("push should send a command")
            .expect("channel open");
        match cmd {
            crate::gateway::AgentCommand::UpdateConfig(cfg) => {
                assert!(
                    (cfg.temperature - 0.3).abs() < 1e-6,
                    "pushed temperature {} should be ~0.3",
                    cfg.temperature
                );
            }
            other => panic!("expected UpdateConfig, got {:?}", other),
        }
    }

    async fn set_with_base(
        state: &Arc<GatewayState>,
        path: &str,
        value: serde_json::Value,
        base_revision: Option<&str>,
    ) -> WsResponse {
        let mut params = serde_json::json!({ "path": path, "value": value });
        if let Some(br) = base_revision {
            params["base_revision"] = serde_json::Value::String(br.to_string());
        }
        handle_config_set(&req("r", "config.set", params), state).await
    }

    #[tokio::test]
    async fn config_get_includes_revision() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res = handle_config_get(&req("g", "config.get", serde_json::json!({})), &state).await;
        assert!(res.ok);
        let p = res.payload.expect("payload");
        let revision = p["revision"].as_str().expect("revision");
        assert_eq!(revision.len(), 64, "sha256 hex should be 64 chars");
    }

    #[tokio::test]
    async fn config_set_with_matching_base_revision_succeeds() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let get = handle_config_get(&req("g", "config.get", serde_json::json!({})), &state).await;
        let revision = get.payload.unwrap()["revision"]
            .as_str()
            .unwrap()
            .to_string();

        let res = set_with_base(
            &state,
            "default_agent.temperature",
            serde_json::json!(0.9),
            Some(&revision),
        )
        .await;
        assert!(res.ok, "matching base_revision should succeed");
        let payload = res.payload.unwrap();
        let new_rev = payload["revision"].as_str().unwrap();
        assert_ne!(new_rev, revision, "revision must change after a write");
    }

    #[tokio::test]
    async fn config_set_with_stale_base_revision_conflicts() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let get = handle_config_get(&req("g", "config.get", serde_json::json!({})), &state).await;
        let get_payload = get.payload.unwrap();
        let stale = get_payload["revision"].as_str().unwrap().to_string();

        // Another writer changes the config, invalidating `stale`.
        let other = set_and_ok(&state, "default_agent.temperature", serde_json::json!(0.9)).await;
        assert!(other.ok);

        let pre_max_tokens = state.config.read().await.default_agent.max_tokens;
        let res = set_with_base(
            &state,
            "default_agent.max_tokens",
            serde_json::json!(2048),
            Some(&stale),
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "REVISION_CONFLICT");
        let payload = res.payload.expect("conflict payload");
        assert!(payload["current_revision"].as_str().is_some());
        assert_eq!(payload["expected_revision"], serde_json::json!(stale));
        // The target field must NOT have been mutated.
        assert_eq!(state.config.read().await.default_agent.max_tokens, pre_max_tokens);
    }

    #[tokio::test]
    async fn config_set_stale_base_does_not_switch_router() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        register_provider(&state, "openai", &["gpt-4o"]).await;

        // A bogus base_revision must fail fast — before the model-router side
        // effect — so the default model is not switched.
        let res =
            set_with_base(&state, "model", serde_json::json!("gpt-4o"), Some("deadbeef")).await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().unwrap().code, "REVISION_CONFLICT");
        assert_ne!(
            state.infra.model_router.get_default_model().await,
            "gpt-4o",
            "router must not switch when CAS fails"
        );
    }

    #[tokio::test]
    async fn config_set_empty_base_revision_treated_as_absent() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res =
            set_with_base(&state, "default_agent.temperature", serde_json::json!(0.5), Some(""))
                .await;
        assert!(res.ok, "empty base_revision must be ignored");
    }

    #[tokio::test]
    async fn config_set_success_returns_new_revision() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res = set_and_ok(&state, "heartbeat.enabled", serde_json::json!(true)).await;
        assert!(res.ok);
        let payload = res.payload.unwrap();
        let returned = payload["revision"].as_str().unwrap().to_string();
        let config = state.config.read().await;
        let current = crate::gateway::config_revision(&config);
        assert_eq!(returned, current);
    }
}
