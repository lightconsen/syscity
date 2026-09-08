//! WS admin handlers: marketplace.

use std::sync::Arc;

use serde::Deserialize;

use super::super::{parse_params, WsRequest, WsResponse};
use crate::gateway::GatewayState;

// ── Connectors catalog (marketplace) ────────────────────────────────────

/// `connectors.catalog` — the marketplace catalog (cached) joined with each
/// entry's installed state.
///
/// Optional params: `{ lang }` — forwarded as `Accept-Language` when a sync
/// happens (`catalog.json` is bilingual: `zh*` → Chinese, else English). The
/// single-slot cache tracks its language bucket in `meta.json`; a request for
/// a different bucket triggers a re-sync (which skips `If-None-Match`, so the
/// cloud answers 200 with the new language).
///
/// On a first visit with an empty cache the handler one-shot syncs from the
/// cloud catalog URL when cloud mode is active (feature + `cloud.enabled` +
/// logged in), so member/cloud entries are visible immediately. Returns
/// `{ version, synced, entries }` — the shape the UI consumes.
pub(crate) async fn handle_connectors_catalog(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    // Params are optional (the UI may send none): `{}` or absent = no
    // language preference. `lang` is only consumed under the cloud feature
    // (it becomes the catalog sync's Accept-Language).
    #[derive(Deserialize, Default)]
    #[cfg_attr(not(feature = "cloud"), allow(dead_code))]
    struct Params {
        lang: Option<String>,
    }
    #[cfg_attr(not(feature = "cloud"), allow(unused_variables))]
    let p: Params = req
        .params
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let manager = &state.tools.connector_manager;
    let doc: Option<crate::mcp::connectors::catalog::CatalogDocument> = {
        let cached = match manager.cached_catalog().await {
            Ok(Some(doc)) => Some(doc),
            Ok(None) => None,
            Err(e) => return WsResponse::err(&req.id, "INTERNAL", e.to_string()),
        };
        // Sync when the cache is missing, empty, or holds a different
        // language than the one requested now (a cached-but-empty catalog,
        // e.g. first sync raced an empty cloud catalog, must not pin the
        // empty view forever).
        #[cfg(feature = "cloud")]
        let wants_lang = crate::mcp::connectors::catalog::catalog_lang_bucket(p.lang.as_deref());
        #[cfg(feature = "cloud")]
        let cached_lang = manager.cached_catalog_lang().await;
        #[cfg(feature = "cloud")]
        let needs_sync = match &cached {
            Some(d) => d.connectors.is_empty() || cached_lang.as_deref() != Some(wants_lang),
            None => true,
        };
        #[cfg(feature = "cloud")]
        let synced = if needs_sync {
            let cfg = state.config.read().await.cloud.clone();
            if cfg.enabled && crate::cloud::session::logged_in().await {
                let url = format!("{}/catalog.json", cfg.api_base.trim_end_matches('/'));
                if let Ok((fresh, _)) = manager.sync_catalog(&url, p.lang.as_deref()).await {
                    Some(fresh)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        #[cfg(not(feature = "cloud"))]
        let synced: Option<crate::mcp::connectors::catalog::CatalogDocument> = None;
        // A fresh sync wins (e.g. language change: the cache still holds the
        // previous language); fall back to the cache when no sync ran or it
        // failed (degraded offline mode keeps serving the stale catalog).
        synced.or(cached)
    };

    let installed = manager.list().await.unwrap_or_default();
    let installed_by_id: std::collections::HashMap<String, _> =
        installed.into_iter().map(|s| (s.id.clone(), s)).collect();

    let entries: Vec<_> = doc
        .iter()
        .flat_map(|d| d.connectors.iter())
        .map(|e| {
            let inst = installed_by_id.get(&e.id);
            // Experts are "installed" once their role dir exists in agents/.
            let expert_installed =
                e.entry_type == "expert" && crate::dirs::agents_dir().join(&e.id).is_dir();
            serde_json::json!({
                "id": e.id,
                "version": e.version,
                "display_name": e.display_name,
                "description": e.description,
                "icon": e.icon,
                "type": e.entry_type,
                "kind": e.kind,
                "visibility": e.visibility,
                "credits_per_use": e.credits_per_use,
                "category": e.category,
                "installed": inst.is_some() || expert_installed,
                "installed_version": inst.map(|s| s.version.clone()),
                "state": inst.map(|s| serde_json::to_value(s.state).unwrap_or_default()),
            })
        })
        .collect();

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "version": doc.as_ref().map(|d| d.version).unwrap_or(0),
            "synced": doc.is_some(),
            "entries": entries,
        }),
    )
}

/// `connectors.catalog_install` — install a catalog entry (connector,
/// skill, or expert role).
pub(crate) async fn handle_connectors_catalog_install(
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
    let manager = &state.tools.connector_manager;
    let Some(doc) = manager.cached_catalog().await.ok().flatten() else {
        return WsResponse::err(
            &req.id,
            "INTERNAL",
            "marketplace catalog is empty — sync it first",
        );
    };
    let Some(entry) = doc
        .connectors
        .into_iter()
        .filter(|e| e.id == p.id)
        .max_by_key(|e| crate::mcp::connectors::catalog::catalog_version_key(&e.version))
    else {
        return WsResponse::err(&req.id, "NOT_FOUND", format!("no catalog entry for '{}'", p.id));
    };

    if entry.entry_type == "expert" {
        match manager.install_expert(&entry).await {
            Ok(agents) => {
                let _ = state.agents.registry.write().await.discover().await;
                WsResponse::ok(
                    &req.id,
                    serde_json::json!({ "id": entry.id, "type": "expert", "agents": agents, "installed": true }),
                )
            }
            Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
        }
    } else {
        match manager.upgrade(&entry).await {
            Ok(summary) => {
                WsResponse::ok(&req.id, serde_json::to_value(&summary).unwrap_or_default())
            }
            Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
        }
    }
}
