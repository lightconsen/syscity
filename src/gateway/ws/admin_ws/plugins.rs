//! WS admin handlers: plugins.

use std::sync::Arc;

use serde::Deserialize;

use super::super::{parse_params, WsRequest, WsResponse};
use crate::gateway::GatewayState;

/// `plugins.reload_all` — re-initialize the plugin manager (reload all).
pub(crate) async fn handle_plugins_reload_all(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    match state.infra.plugin_manager.initialize().await {
        Ok(_) => WsResponse::ok(&req.id, serde_json::json!({ "success": true })),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
    }
}

/// `plugins.sign` — sign a plugin manifest with an ed25519 key.
pub(crate) async fn handle_plugins_sign(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        name: String,
        secret_key: String,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let signing_key = if p.secret_key.is_empty() {
        match state
            .secrets
            .route("plugin")
            .get(&crate::secrets::SecretId::new("plugin", &p.name, "secret_key"))
            .await
        {
            Ok(Some(key)) => key,
            _ => {
                return WsResponse::err(
                    &req.id,
                    "BAD_REQUEST",
                    format!(
                        "No signing key for plugin '{}'; submit secret_key in the request body",
                        p.name
                    ),
                );
            }
        }
    } else {
        if let Err(e) = state
            .secrets
            .route("plugin")
            .set(
                &crate::secrets::SecretId::new("plugin", &p.name, "secret_key"),
                &p.secret_key,
                crate::secrets::SecretOrigin::UserEntered,
            )
            .await
        {
            eprintln!("Failed to store plugin secret_key for '{}' ({:?})", p.name, e);
        }
        p.secret_key.clone()
    };

    let manifest_path = state
        .paths
        .config_dir()
        .join("plugins")
        .join(&p.name)
        .join("plugin.json");
    let manifest_text = match tokio::fs::read_to_string(&manifest_path).await {
        Ok(t) => t,
        Err(e) => {
            return WsResponse::err(
                &req.id,
                "NOT_FOUND",
                format!("Plugin '{}' not found at {:?}: {}", p.name, manifest_path, e),
            );
        }
    };
    let mut manifest: crate::plugins::manifest::PluginManifest =
        match serde_json::from_str(&manifest_text) {
            Ok(m) => m,
            Err(e) => {
                return WsResponse::err(&req.id, "BAD_REQUEST", format!("Invalid manifest: {}", e));
            }
        };
    if let Err(e) = crate::plugins::verification::sign_manifest(&mut manifest, &signing_key) {
        return WsResponse::err(&req.id, "INTERNAL", format!("Failed to sign manifest: {}", e));
    }
    if let Err(e) = tokio::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    )
    .await
    {
        return WsResponse::err(
            &req.id,
            "INTERNAL",
            format!("Failed to write signed manifest: {}", e),
        );
    }
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "success": true,
            "message": format!("Plugin '{}' signed successfully", p.name),
            "signer_public_key": manifest.signer_public_key,
        }),
    )
}

// ── Plugins ─────────────────────────────────────────────────────────────

/// `plugins.list` — installed plugins.
pub(crate) async fn handle_plugins_list(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let plugins = state.infra.plugin_manager.list_plugins().await;
    let plugin_list: Vec<_> = plugins
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id(),
                "name": p.name(),
                "enabled": p.enabled,
                "capabilities": p.manifest.capabilities,
            })
        })
        .collect();
    WsResponse::ok(
        &req.id,
        serde_json::json!({ "plugins": plugin_list, "count": plugin_list.len() }),
    )
}

/// `plugins.enable` / `plugins.disable` — toggle a plugin.
pub(crate) async fn handle_plugins_set_enabled(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    enabled: bool,
) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        id: String,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    match state.infra.plugin_manager.set_enabled(&p.id, enabled).await {
        Ok(()) => WsResponse::ok(&req.id, serde_json::json!({ "success": true })),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
    }
}

/// `plugins.install` — install a plugin by name.
pub(crate) async fn handle_plugins_install(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        name: String,
        registry: Option<String>,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    match state
        .infra
        .plugin_manager
        .install_plugin(&p.name, p.registry.as_deref())
        .await
    {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({ "success": true, "message": format!("Plugin '{}' installed", p.name) }),
        ),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", format!("Failed to install plugin: {}", e)),
    }
}

/// `plugins.search` — search the plugin registry.
pub(crate) async fn handle_plugins_search(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        q: String,
        registry: Option<String>,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    match state
        .infra
        .plugin_manager
        .search_registry(&p.q, p.registry.as_deref())
        .await
    {
        Ok(results) => WsResponse::ok(&req.id, serde_json::json!({ "results": results })),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
    }
}

/// `plugins.unload` — unload a plugin (disable at runtime, keep on disk).
pub(crate) async fn handle_plugins_unload(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        id: String,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    match state.infra.plugin_manager.unload_plugin(&p.id).await {
        Ok(_) => WsResponse::ok(&req.id, serde_json::json!({ "success": true })),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
    }
}

/// `plugins.reload` — reload a plugin's manifest/runtime.
pub(crate) async fn handle_plugins_reload(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        id: String,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    match state.infra.plugin_manager.reload_plugin(&p.id).await {
        Ok(_) => WsResponse::ok(&req.id, serde_json::json!({ "success": true })),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
    }
}

/// `plugins.uninstall` — remove a plugin from disk.
pub(crate) async fn handle_plugins_uninstall(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        name: String,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    match state.infra.plugin_manager.uninstall_plugin(&p.name).await {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({ "success": true, "message": format!("Plugin '{}' uninstalled", p.name) }),
        ),
        Err(e) => {
            WsResponse::err(&req.id, "INTERNAL", format!("Failed to uninstall plugin: {}", e))
        }
    }
}
