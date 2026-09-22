//! HTTP router construction for the gateway control plane.

use super::*;

// ── build_router ─────────────────────────────────────────────────────

/// Build the HTTP router.
pub(crate) async fn build_router(state: Arc<GatewayState>) -> Router {
    // Public tier: Webhooks (no authentication, signature verification per-channel)
    let public_router = crate::gateway::webhooks::create_webhook_router(state.clone());

    // (Local OAuth login/logout was removed — the UI authenticates via the
    // Syscity Cloud OAuth flow instead; the auth_router is now empty.)

    // Admin tier: Essential APIs (not deprecated)
    let essential_public_router = Router::new()
        .route("/health", get(crate::gateway::health_handler))
        .route("/ready", get(crate::gateway::ready_handler))
        .route("/live", get(crate::gateway::live_handler))
        .route("/metrics", get(crate::gateway::metrics_handler))
        .route("/api/v1/artifacts/*path", get(crate::gateway::artifact_handler));

    // Authenticated essential APIs (auth required)
    let essential_auth_router = Router::new()
        .route("/v1/chat/completions", post(crate::gateway::openai_chat_completions_handler))
        .route("/v1/models", get(crate::gateway::openai_list_models_handler))
        // The credential exchange the WS protocol needs: a browser cannot set
        // headers on a WebSocket upgrade, so without this the token would have
        // to travel in the upgrade URL. See `handlers/ws_ticket.rs`.
        .route("/api/v1/ws-ticket", post(crate::gateway::handlers::ws_ticket::ws_ticket_handler))
        // (Everything else in the former admin tier — models, reload, channels,
        // device pairing, providers, plugins, cron, skills — is WS-only now,
        // driven by the admin WS methods in ws/admin_ws.rs.)
        .layer(from_fn_with_state(state.clone(), crate::gateway::middleware::auth_middleware));

    let essential_router = essential_public_router.merge(essential_auth_router);

    // Apply remaining middleware layers to essential routes
    let admin_router = essential_router
        .layer(from_fn_with_state(
            state.clone(),
            crate::gateway::middleware::rate_limit_middleware,
        ))
        .layer(from_fn_with_state(
            state.clone(),
            crate::gateway::middleware::tailscale_auth_middleware,
        ))
        .layer(from_fn_with_state(
            state.clone(),
            crate::gateway::middleware::trusted_proxy_auth_middleware,
        ))
        .layer(from_fn(crate::gateway::middleware::security_headers_middleware))
        .with_state(state.clone());

    // WebSocket sub-router with mandatory auth validation middleware
    let ws_router = Router::new()
        .route("/ws", get(crate::gateway::ws::ws_handler))
        .layer(from_fn_with_state(state.clone(), crate::gateway::ws::ws_auth_middleware))
        .with_state(state.clone());

    // Build CORS layer from config
    let cors_layer = {
        let config = state.config.read().await;
        if config.security.cors.enabled {
            let mut cors = CorsLayer::new();
            if config.security.cors.allow_credentials {
                cors = cors.allow_credentials(true);
            }
            // A wildcard is `Any`, never a mirror of the request's own Origin.
            // Mirroring is the dangerous reading of "allow anything": it pairs
            // with `allow_credentials` and tells the browser that *any* site
            // may make credentialed calls. Wildcard + credentials is refused at
            // startup (`validate_auth_config`), so the two never meet here.
            for origin in &config.security.cors.allowed_origins {
                if origin == "*" {
                    cors = cors.allow_origin(tower_http::cors::Any);
                } else if let Ok(header_value) = origin.parse() {
                    cors = cors.allow_origin([header_value]);
                }
            }
            let methods: Vec<_> = config
                .security
                .cors
                .allowed_methods
                .iter()
                .filter_map(|m| m.parse().ok())
                .collect();
            if !methods.is_empty() {
                cors = cors.allow_methods(methods);
            }
            let headers: Vec<_> = config
                .security
                .cors
                .allowed_headers
                .iter()
                .filter_map(|h| h.parse().ok())
                .collect();
            if !headers.is_empty() {
                cors = cors.allow_headers(headers);
            }
            cors.max_age(std::time::Duration::from_secs(config.security.cors.max_age_secs as u64))
        } else {
            CorsLayer::new()
        }
    };

    // SPA frontend routes (serve built React app from embedded assets)
    let frontend_router = Router::new()
        .route("/", get(crate::gateway::web_terminal_html_handler))
        .route("/favicon.ico", get(crate::gateway::favicon_handler))
        // Cloud OAuth return URL (default `cloud.redirect_base`): serves the
        // SPA, whose App.tsx reads `#token=` here and persists it over WS
        // (`cloud.token`). Asset URLs are rewritten to absolute because the
        // route is nested (see cloud_login_callback_html_handler).
        .route(
            "/cloud/login/callback",
            get(crate::gateway::cloud_login_callback_html_handler),
        )
        .route("/syscity.png", get(crate::gateway::syscity_png_handler))
        .route("/manifest.webmanifest", get(crate::gateway::manifest_handler))
        .route("/registerSW.js", get(crate::gateway::register_sw_handler))
        .route("/sw.js", get(crate::gateway::sw_js_handler))
        .route("/assets/*path", get(crate::gateway::asset_handler));

    // Cloud session endpoints (feature `cloud`). Public tier: these are how
    // the web SPA logs into Syscity Cloud and persists the session token.
    #[cfg(feature = "cloud")]
    let cloud_router = Router::new()
        .route("/api/v1/login", get(crate::gateway::handlers::cloud::login_handler))
        .with_state(state.clone());

    // Public engine status is now exposed via the WS `status.get` method
    // (ws/admin_ws.rs) — the REST endpoint was removed.

    // Merge all routers and apply global CORS
    let app = frontend_router
        .merge(public_router)
        .merge(admin_router)
        .merge(ws_router);

    #[cfg(feature = "cloud")]
    let app = app.merge(cloud_router);

    // Every request body is bounded. Only two routes accept one
    // (`/v1/chat/completions` and the webhooks), and neither has a legitimate
    // body anywhere near this size — while an unbounded body on an
    // unauthenticated route is a free way to consume memory.
    app.layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(cors_layer)
}

/// Largest accepted HTTP request body (10 MiB). Generous for an OpenAI-style
/// chat request; far below anything that would matter as a memory attack.
const MAX_REQUEST_BODY_BYTES: usize = 10 * 1024 * 1024;
