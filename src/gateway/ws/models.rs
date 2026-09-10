//! models.list / presets / fetch_remote / add / remove / set_default.

use super::*;

/// Whether a provider's models should be listed for the given login state.
///
/// Cloud models require a signed-in session (the cloud provider is registered
/// on `cloud.enabled`, independent of login), so anonymous users don't see
/// unusable cloud entries in the picker.
fn provider_visible(provider: &str, logged_in: bool) -> bool {
    #[cfg(feature = "cloud")]
    {
        provider != "cloud" || logged_in
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (provider, logged_in);
        true
    }
}

pub(super) async fn handle_models_list(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    // List (provider, model_id) pairs from provider configs + catalog.
    let pairs = state.infra.model_router.models_with_providers().await;

    // Cloud models only make sense to a signed-in session — the cloud provider
    // is registered on cloud.enabled (independent of login), so without a
    // session token its models are unusable. Hide them for anonymous users
    // ("登录后云模型自动进入模型列表" — docs/cloud-integration.md).
    #[cfg(feature = "cloud")]
    let logged_in = crate::cloud::session::logged_in(&state.secrets).await;
    #[cfg(not(feature = "cloud"))]
    let logged_in = false;

    // Per-provider credential metadata so the UI can render the add/update form
    // (e.g. show that a key is already saved) without exposing the raw key.
    let provider_meta: std::collections::HashMap<String, (bool, Option<String>, Option<String>)> = {
        let router_config = state.infra.model_router.router_config().await;
        router_config
            .providers
            .iter()
            .map(|(name, pcfg)| {
                let masked = pcfg
                    .api_key
                    .inline_value()
                    .filter(|k| !k.is_empty())
                    .map(crate::secrets::mask_secret);
                let has_key = masked.is_some()
                    || matches!(pcfg.api_key, crate::model_router::ProviderKey::Ref(_));
                (name.clone(), (has_key, masked, pcfg.base_url.clone()))
            })
            .collect()
    };

    // Cloud models carry a server-configured billing multiplier (credits per
    // 1K tokens). Fetched with a TTL cache; failures keep the previous table
    // and never fail the listing.
    #[cfg(feature = "cloud")]
    let cloud_multipliers = if pairs.iter().any(|(p, _)| p == "cloud") && logged_in {
        let cloud_cfg = { state.config.read().await.cloud.clone() };
        crate::cloud::multipliers::credit_multipliers(&cloud_cfg, &state.secrets).await
    } else {
        None
    };

    let entries: Vec<serde_json::Value> = pairs
        .iter()
        .filter(|(provider, _model)| provider_visible(provider, logged_in))
        .map(|(provider, model)| {
            let (has_api_key, api_key_masked, base_url) = provider_meta
                .get(provider)
                .cloned()
                .unwrap_or((false, None, None));
            let entry = serde_json::json!({
                "id": model,
                "name": model,
                "provider": provider,
                "provider_name": crate::model_router::provider_display_name(provider),
                "has_api_key": has_api_key,
                "api_key_masked": api_key_masked,
                "base_url": base_url,
            });
            // Cloud entries carry the server-configured billing multiplier
            // (cache above); local models never do.
            #[cfg(feature = "cloud")]
            let entry = {
                let mut entry = entry;
                if provider == "cloud" {
                    if let Some(table) = &cloud_multipliers {
                        if let Some(mult) = table.get(model.as_str()) {
                            entry["credit_multiplier"] = serde_json::json!(mult);
                        }
                    }
                }
                entry
            };
            entry
        })
        .collect();
    let default_model = {
        let config = state.config.read().await;
        config.model.clone()
    };
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "models": entries,
            "default_model": default_model,
        }),
    )
}

pub(super) async fn handle_models_presets(
    req: &WsRequest,
    _state: &Arc<GatewayState>,
) -> WsResponse {
    let presets = crate::model_router::provider_presets();
    let builtins = crate::providers::preset::builtin_providers();
    let list: Vec<serde_json::Value> = presets
        .into_iter()
        .map(|(name, p)| {
            // Enrich with protocol/auth info from the TOML registry when the
            // preset exists there (custom does not).
            let builtin = builtins.get(name.as_str());
            let protocol = builtin.and_then(|b| b.variants.first()).map(|v| v.protocol);
            let needs_api_key = builtin
                .and_then(|b| b.variants.first())
                .map(|v| v.auth_method != crate::providers::AuthMethod::None)
                .unwrap_or(true);
            // Fall back to the TOML registry base URL when the legacy preset
            // does not define one (e.g. Anthropic, Gemini).
            let base_url = p.default_base_url.or_else(|| {
                builtin
                    .and_then(|b| b.variants.first())
                    .map(|v| v.default_base_url.clone())
            });
            serde_json::json!({
                "name": name,
                "display_name": p.display_name,
                "base_url": base_url,
                "models": p.models,
                "protocol": protocol,
                "needs_api_key": needs_api_key,
            })
        })
        .collect();
    WsResponse::ok(&req.id, serde_json::json!({ "presets": list }))
}

/// Build the list-models endpoint URL for a protocol.
pub(super) fn models_endpoint_url(protocol: crate::providers::Protocol, base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    match protocol {
        crate::providers::Protocol::OpenAi => format!("{base}/models"),
        crate::providers::Protocol::Anthropic => format!("{base}/v1/models"),
        crate::providers::Protocol::Gemini => format!("{base}/models"),
    }
}

/// Parse model IDs from an OpenAI/Anthropic-style `{ "data": [{ "id": ... }] }` body.
pub(super) fn parse_data_models(body: &serde_json::Value) -> Vec<String> {
    body.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse model IDs from a Gemini `{ "models": [{ "name": "models/..." }] }` body.
pub(super) fn parse_gemini_models(body: &serde_json::Value) -> Vec<String> {
    body.get("models")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                .map(|n| n.strip_prefix("models/").unwrap_or(n).to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub(super) async fn handle_models_fetch_remote(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct FetchRemotePayload {
        provider: String,
        base_url: Option<String>,
        api_key: Option<String>,
        /// Protocol override, required for providers not in the registry.
        protocol: Option<crate::providers::Protocol>,
    }
    let payload: FetchRemotePayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    // Resolve protocol / default base URL / auth method from the TOML registry.
    let builtins = crate::providers::preset::builtin_providers();
    let variant = builtins
        .get(payload.provider.as_str())
        .and_then(|b| b.variants.first());

    let protocol = match payload.protocol.or_else(|| variant.map(|v| v.protocol)) {
        Some(p) => p,
        None => {
            return WsResponse::err(
                &req.id,
                "PROTOCOL_REQUIRED",
                format!(
                    "Unknown provider '{}'; an explicit protocol is required",
                    payload.provider
                ),
            );
        }
    };

    let base_url = match payload
        .base_url
        .filter(|u| !u.is_empty())
        .or_else(|| variant.map(|v| v.default_base_url.clone()))
    {
        Some(u) => u,
        None => {
            return WsResponse::err(
                &req.id,
                "BASE_URL_REQUIRED",
                format!("Provider '{}' requires a base_url", payload.provider),
            );
        }
    };

    if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
        return WsResponse::err(
            &req.id,
            "INVALID_BASE_URL",
            "base_url must start with http:// or https://".to_string(),
        );
    }

    let auth_method = variant
        .map(|v| v.auth_method.clone())
        .unwrap_or(crate::providers::AuthMethod::Bearer);

    // Fall back to the provider's stored key when the request does not carry one
    // (e.g. the UI is editing an already-configured provider whose field is
    // blank to mean "keep the saved key"). Prefer the resolved auth-profile key,
    // then the raw config value.
    let api_key = if let Some(key) = payload.api_key.as_ref().filter(|k| !k.is_empty()) {
        Some(key.clone())
    } else if let Some(key) = state
        .infra
        .model_router
        .auth_profiles
        .current_key(&payload.provider)
        .await
    {
        Some(key)
    } else {
        let router_config = state.infra.model_router.router_config().await;
        router_config
            .providers
            .get(&payload.provider)
            .and_then(|p| p.api_key.inline_value())
            .map(|s| s.to_string())
    };

    let static_fallback = || {
        crate::model_router::provider_presets()
            .get(&payload.provider)
            .map(|p| p.models.clone())
            .unwrap_or_default()
    };

    let url = match variant.and_then(|v| v.models_endpoint.as_deref()) {
        Some(endpoint) => format!("{}{}", base_url.trim_end_matches('/'), endpoint),
        None => models_endpoint_url(protocol, &base_url),
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return WsResponse::ok(
                &req.id,
                serde_json::json!({
                    "models": static_fallback(),
                    "source": "static",
                    "error": format!("HTTP client error: {e}"),
                }),
            );
        }
    };

    let mut request = client.get(&url);
    match (&auth_method, &api_key) {
        (crate::providers::AuthMethod::Bearer, Some(key)) => {
            request = request.bearer_auth(key);
        }
        (crate::providers::AuthMethod::ApiKeyHeader, Some(key)) => {
            request = request
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01");
        }
        (crate::providers::AuthMethod::GoogleApiKey, Some(key)) => {
            request = request.header("x-goog-api-key", key);
        }
        (crate::providers::AuthMethod::CustomHeader { name }, Some(key)) => {
            request = request.header(name, key);
        }
        _ => {}
    }

    match request.send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(body) => {
                let models = match protocol {
                    crate::providers::Protocol::Gemini => parse_gemini_models(&body),
                    _ => parse_data_models(&body),
                };
                if models.is_empty() {
                    WsResponse::ok(
                        &req.id,
                        serde_json::json!({
                            "models": static_fallback(),
                            "source": "static",
                            "error": "Provider returned an empty model list",
                        }),
                    )
                } else {
                    WsResponse::ok(
                        &req.id,
                        serde_json::json!({ "models": models, "source": "remote" }),
                    )
                }
            }
            Err(e) => WsResponse::ok(
                &req.id,
                serde_json::json!({
                    "models": static_fallback(),
                    "source": "static",
                    "error": format!("Failed to parse provider response: {e}"),
                }),
            ),
        },
        Ok(resp) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "models": static_fallback(),
                "source": "static",
                "error": format!("Provider returned HTTP {}", resp.status()),
            }),
        ),
        Err(e) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "models": static_fallback(),
                "source": "static",
                "error": format!("Failed to reach provider: {e}"),
            }),
        ),
    }
}

pub(super) async fn handle_models_add(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct ModelAddPayload {
        provider: String,
        models: Vec<String>,
        default_model: Option<String>,
        api_key: Option<String>,
        base_url: Option<String>,
    }
    let payload: ModelAddPayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let provider_name = payload.provider.clone();
    if payload.models.is_empty() {
        return WsResponse::err(
            &req.id,
            "INVALID_PAYLOAD",
            "models must contain at least one model".to_string(),
        );
    }

    // A provider name is unique. When it already exists this request is an
    // update: the provider's models/default/credentials are replaced wholesale.
    let is_update = state
        .infra
        .model_router
        .provider_exists(&provider_name)
        .await;

    let preset = crate::model_router::provider_preset_for_name(&provider_name);
    let (provider_type, preset_base_url) = match &preset {
        Some(p) => (p.protocol.clone(), p.default_base_url.clone()),
        None => (crate::model_router::ProviderType::Custom { name: provider_name.clone() }, None),
    };

    // On update, blank credentials mean "keep the existing ones" — never wipe
    // the stored key or base URL just because the form left them empty.
    let existing_cfg = state
        .infra
        .model_router
        .router_config()
        .await
        .providers
        .get(&provider_name)
        .cloned();

    let base_url = match &payload.base_url {
        Some(u) if !u.is_empty() => Some(u.clone()),
        _ => existing_cfg
            .as_ref()
            .and_then(|c| c.base_url.clone())
            .or(preset_base_url),
    };

    let api_key = match &payload.api_key {
        Some(k) if !k.is_empty() => crate::model_router::ProviderKey::from(k.clone()),
        _ => existing_cfg.map(|c| c.api_key.clone()).unwrap_or_default(),
    };

    let default_model = payload
        .default_model
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| payload.models[0].clone());

    let provider_config = crate::model_router::ProviderConfig {
        provider_type,
        models: payload.models.clone(),
        default_model: default_model.clone(),
        api_key,
        api_keys: Vec::new(),
        auth_profile: None,
        oauth: None,
        base_url,
        timeout: std::time::Duration::from_secs(30),
        max_retries: 3,
        retry_delay_ms: 1000,
    };

    // Register (or replace) the provider; add_provider registers its models in
    // the catalog, update_provider refreshes the catalog source.
    let result = if is_update {
        state
            .infra
            .model_router
            .update_provider(&provider_name, provider_config)
            .await
    } else {
        state
            .infra
            .model_router
            .add_provider(&provider_name, provider_config)
            .await
    };
    if let Err(e) = result {
        return WsResponse::err(
            &req.id,
            "PROVIDER_ERROR",
            format!("Failed to {} provider: {}", if is_update { "update" } else { "register" }, e),
        );
    }

    // Reflect the router's live state back into the persisted GatewayConfig.
    sync_config_from_router(state).await;

    // Promote to default when none is configured yet, or the update removed
    // the model that was the default.
    let default_missing = {
        let config = state.config.read().await;
        config.model.is_empty()
            || state
                .infra
                .model_router
                .provider_for_model(&config.model)
                .await
                .is_none()
    };
    if default_missing {
        if let Err(e) = state
            .infra
            .model_router
            .switch_default_model(&default_model)
            .await
        {
            warn!("Failed to switch default model to {}: {}", default_model, e);
        }
        sync_config_from_router(state).await;
    }

    if let Some(config_path) = state.config_path.clone() {
        let config_guard = state.config.read().await;
        if let Err(e) = persist_config_atomic(&config_guard, &config_path).await {
            return WsResponse::err(
                &req.id,
                "PERSIST_FAILED",
                format!(
                    "Provider {} but failed to persist config: {}",
                    if is_update { "updated" } else { "added" },
                    e
                ),
            );
        }
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({ "status": if is_update { "updated" } else { "added" } }),
    )
}

/// Copy the router's live provider/default state back into `GatewayConfig` so
/// the persisted `config.toml` matches what routing actually uses.
pub(super) async fn sync_config_from_router(state: &GatewayState) {
    let router_config = state.infra.model_router.router_config().await;
    let mut config_guard = state.config.write().await;
    let config = Arc::make_mut(&mut config_guard);
    config.providers = router_config.providers.clone();
    config.model = router_config.default_model.clone();
    if let Some(provider) = router_config.provider_for_model(&config.model) {
        config.model_provider = provider.to_string();
    }
}

pub(super) async fn handle_models_remove(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct RemovePayload {
        model_id: String,
    }
    let payload: RemovePayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if let Err(e) = state
        .infra
        .model_router
        .remove_model(&payload.model_id)
        .await
    {
        return WsResponse::err(&req.id, "MODEL_NOT_FOUND", format!("{}", e));
    }

    sync_config_from_router(state).await;

    if let Some(config_path) = state.config_path.clone() {
        let config_guard = state.config.read().await;
        if let Err(e) = persist_config_atomic(&config_guard, &config_path).await {
            return WsResponse::err(
                &req.id,
                "PERSIST_FAILED",
                format!("Model removed but failed to persist config: {}", e),
            );
        }
    }

    WsResponse::ok(&req.id, serde_json::json!({ "status": "removed" }))
}

pub(super) async fn handle_models_set_default(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct SetDefaultPayload {
        model_id: String,
    }
    let payload: SetDefaultPayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if let Err(e) = state
        .infra
        .model_router
        .switch_default_model(&payload.model_id)
        .await
    {
        return WsResponse::err(&req.id, "MODEL_NOT_FOUND", format!("{}", e));
    }

    sync_config_from_router(state).await;

    if let Some(config_path) = state.config_path.clone() {
        let config_guard = state.config.read().await;
        if let Err(e) = persist_config_atomic(&config_guard, &config_path).await {
            return WsResponse::err(
                &req.id,
                "PERSIST_FAILED",
                format!("Default model set but failed to persist config: {}", e),
            );
        }
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({ "status": "ok", "default_model": payload.model_id }),
    )
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

    fn provider_config(models: &[&str]) -> ProviderConfig {
        ProviderConfig {
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
        }
    }

    async fn register_provider(state: &GatewayState, name: &str, models: &[&str]) {
        state
            .infra
            .model_router
            .add_provider(name, provider_config(models))
            .await
            .expect("register provider");
    }

    /// Cloud models must be hidden from signed-out users (the picker is the
    /// one place the user sees models; the provider is registered regardless
    /// of login state).
    #[test]
    fn provider_visible_hides_cloud_models_from_anonymous_users() {
        #[cfg(feature = "cloud")]
        {
            assert!(!provider_visible("cloud", false), "cloud hidden when signed out");
            assert!(provider_visible("cloud", true), "cloud shown when signed in");
            assert!(provider_visible("openai", false), "local providers always visible");
        }
        // Default builds compile no cloud provider — nothing to filter.
        #[cfg(not(feature = "cloud"))]
        {
            assert!(provider_visible("cloud", false));
            assert!(provider_visible("openai", false));
        }
    }

    #[tokio::test]
    async fn models_list_empty_state() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res = handle_models_list(&req("l", "models.list", serde_json::json!({})), &state).await;
        assert!(res.ok);
        let payload = res.payload.expect("payload");
        assert_eq!(payload["models"].as_array().map(|a| a.len()), Some(0));
        assert!(payload["default_model"].as_str().is_some());
    }

    #[tokio::test]
    async fn models_list_shows_provider_models_and_default() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        register_provider(&state, "openai", &["gpt-4o-mini"]).await;
        let res = handle_models_list(&req("l", "models.list", serde_json::json!({})), &state).await;
        let models = res
            .payload
            .as_ref()
            .and_then(|p| p["models"].as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(models.len(), 1);
        // A concrete model ID, not an alias name.
        assert_eq!(models[0]["id"].as_str(), Some("gpt-4o-mini"));
        assert_eq!(models[0]["name"].as_str(), Some("gpt-4o-mini"));
        assert_eq!(models[0]["provider"].as_str(), Some("openai"));
        assert_eq!(models[0]["provider_name"].as_str(), Some("OpenAI"));
        // Credential metadata is exposed without the raw key.
        assert_eq!(models[0]["has_api_key"].as_bool(), Some(true));
        assert_eq!(models[0]["api_key_masked"].as_str(), Some("tes••••-key"));
        assert_eq!(models[0]["base_url"], serde_json::Value::Null);
        // Default model is the configured default (not auto-promoted).
        let default = res
            .payload
            .as_ref()
            .and_then(|p| p["default_model"].as_str())
            .unwrap_or_default();
        assert_eq!(default, "claude-3-sonnet-20240229");
    }

    #[tokio::test]
    async fn models_presets_includes_builtin_providers() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res =
            handle_models_presets(&req("p", "models.presets", serde_json::json!({})), &state).await;
        assert!(res.ok);
        let presets = res
            .payload
            .as_ref()
            .and_then(|p| p["presets"].as_array())
            .cloned()
            .unwrap_or_default();
        assert!(!presets.is_empty(), "built-in provider presets expected");
        for entry in presets.iter().take(10) {
            assert!(entry["name"].as_str().is_some());
            assert!(entry["display_name"].as_str().is_some());
        }
    }

    #[tokio::test]
    async fn models_add_registers_provider_models() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res = handle_models_add(
            &req(
                "a",
                "models.add",
                serde_json::json!({
                    "provider": "openai",
                    "models": ["gpt-4o", "gpt-4-turbo"],
                }),
            ),
            &state,
        )
        .await;
        assert!(res.ok);

        // Both models are registered under the provider.
        let pairs = state.infra.model_router.models_with_providers().await;
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&("openai".to_string(), "gpt-4o".to_string())));
        assert!(pairs.contains(&("openai".to_string(), "gpt-4-turbo".to_string())));
    }

    #[tokio::test]
    async fn models_add_updates_existing_provider() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        register_provider(&state, "openai", &["gpt-4o"]).await;
        let res = handle_models_add(
            &req(
                "a",
                "models.add",
                serde_json::json!({
                    "provider": "openai",
                    "models": ["gpt-4o", "gpt-4-turbo"],
                    "default_model": "gpt-4-turbo",
                }),
            ),
            &state,
        )
        .await;
        assert!(res.ok);
        assert_eq!(res.payload.as_ref().and_then(|p| p["status"].as_str()), Some("updated"));

        // The provider keeps a single entry whose models were replaced.
        let pairs = state.infra.model_router.models_with_providers().await;
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&("openai".to_string(), "gpt-4o".to_string())));
        assert!(pairs.contains(&("openai".to_string(), "gpt-4-turbo".to_string())));
        assert_eq!(state.infra.model_router.get_default_model().await, "gpt-4-turbo");
    }

    #[tokio::test]
    async fn models_add_update_preserves_existing_key() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        register_provider(&state, "openai", &["gpt-4o"]).await;
        let res = handle_models_add(
            &req(
                "a",
                "models.add",
                serde_json::json!({
                    "provider": "openai",
                    "models": ["gpt-4o", "gpt-4-turbo"],
                    // No api_key: the existing key must survive the update.
                }),
            ),
            &state,
        )
        .await;
        assert!(res.ok);
        let cfg = state.infra.model_router.router_config().await;
        let p = cfg.providers.get("openai").unwrap();
        assert_eq!(p.api_key.inline_value(), Some("test-key"));
        // A blank key string behaves the same as an omitted one.
        let res = handle_models_add(
            &req(
                "a",
                "models.add",
                serde_json::json!({
                    "provider": "openai",
                    "models": ["gpt-4o"],
                    "api_key": "",
                }),
            ),
            &state,
        )
        .await;
        assert!(res.ok);
        let cfg = state.infra.model_router.router_config().await;
        let p = cfg.providers.get("openai").unwrap();
        assert_eq!(p.api_key.inline_value(), Some("test-key"));
    }

    #[tokio::test]
    async fn models_add_update_replaces_key_when_provided() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        register_provider(&state, "openai", &["gpt-4o"]).await;
        let res = handle_models_add(
            &req(
                "a",
                "models.add",
                serde_json::json!({
                    "provider": "openai",
                    "models": ["gpt-4o"],
                    "api_key": "new-key",
                }),
            ),
            &state,
        )
        .await;
        assert!(res.ok);
        let cfg = state.infra.model_router.router_config().await;
        let p = cfg.providers.get("openai").unwrap();
        assert_eq!(p.api_key.inline_value(), Some("new-key"));
    }

    #[tokio::test]
    async fn models_add_empty_models_errors() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res = handle_models_add(
            &req("a", "models.add", serde_json::json!({ "provider": "openai", "models": [] })),
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("INVALID_PAYLOAD"));
    }

    #[tokio::test]
    async fn models_remove_model() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        register_provider(&state, "openai", &["gpt-4o"]).await;
        let res = handle_models_remove(
            &req("r", "models.remove", serde_json::json!({ "model_id": "gpt-4o" })),
            &state,
        )
        .await;
        assert!(res.ok);
        assert!(state
            .infra
            .model_router
            .models_with_providers()
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn models_remove_unknown_model_errors() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res = handle_models_remove(
            &req("r", "models.remove", serde_json::json!({ "model_id": "nope" })),
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("MODEL_NOT_FOUND"));
    }

    #[tokio::test]
    async fn models_set_default_switches_default() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        register_provider(&state, "openai", &["gpt-4o"]).await;
        register_provider(&state, "anthropic", &["claude-sonnet"]).await;
        let res = handle_models_set_default(
            &req("s", "models.set_default", serde_json::json!({ "model_id": "claude-sonnet" })),
            &state,
        )
        .await;
        assert!(res.ok);
        assert_eq!(
            res.payload
                .as_ref()
                .and_then(|p| p["default_model"].as_str()),
            Some("claude-sonnet")
        );
        assert_eq!(state.infra.model_router.get_default_model().await, "claude-sonnet");
    }

    #[tokio::test]
    async fn models_set_default_unknown_errors() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let res = handle_models_set_default(
            &req("s", "models.set_default", serde_json::json!({ "model_id": "nope" })),
            &state,
        )
        .await;
        assert!(!res.ok);
        assert_eq!(res.error.as_ref().map(|e| e.code.as_str()), Some("MODEL_NOT_FOUND"));
    }
}
