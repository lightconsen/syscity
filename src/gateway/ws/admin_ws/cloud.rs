//! WS admin handlers: cloud.

use std::sync::Arc;

#[cfg(feature = "cloud")]
use base64::Engine as _;

#[cfg(feature = "cloud")]
use serde::Deserialize;

#[cfg(feature = "cloud")]
use super::super::parse_params;
use super::super::{WsRequest, WsResponse};
use super::{cloud_status_json, cloud_unavailable};
use crate::gateway::GatewayState;

/// `cloud.status` — `{ enabled, logged_in, user }` (or null without the cloud
/// feature).
pub(crate) async fn handle_cloud_status(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    WsResponse::ok(&req.id, cloud_status_json(state).await)
}

/// `cloud.subscription` — plan + credit balance.
pub(crate) async fn handle_cloud_subscription(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[cfg(feature = "cloud")]
    {
        let cfg = { state.config.read().await.cloud.clone() };
        if !cfg.enabled {
            return cloud_unavailable(req);
        }
        let Some(token) = crate::cloud::session::get_token().await else {
            return cloud_unavailable(req);
        };
        match crate::cloud::client::CloudClient::new(&cfg, token)
            .subscription()
            .await
        {
            Ok(v) => WsResponse::ok(&req.id, v),
            Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
        }
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = state;
        cloud_unavailable(req)
    }
}

/// `cloud.usage` — `{ days }` credit usage for the period.
pub(crate) async fn handle_cloud_usage(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[cfg(feature = "cloud")]
    #[derive(Deserialize)]
    struct UsageParams {
        #[serde(default = "default_usage_days")]
        days: u32,
    }
    #[cfg(feature = "cloud")]
    fn default_usage_days() -> u32 {
        30
    }
    #[cfg(feature = "cloud")]
    {
        let cfg = { state.config.read().await.cloud.clone() };
        if !cfg.enabled {
            return cloud_unavailable(req);
        }
        let Some(token) = crate::cloud::session::get_token().await else {
            return cloud_unavailable(req);
        };
        let days = parse_params::<UsageParams>(req)
            .map(|p| p.days)
            .unwrap_or(30);
        match crate::cloud::client::CloudClient::new(&cfg, token)
            .usage(days)
            .await
        {
            Ok(v) => WsResponse::ok(&req.id, v),
            Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
        }
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (state, req);
        cloud_unavailable(req)
    }
}

/// `cloud.token` — `{ token }` persist a cloud session token (OAuth result).
pub(crate) async fn handle_cloud_token(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[cfg(feature = "cloud")]
    #[derive(Deserialize)]
    struct TokenParams {
        token: String,
    }
    #[cfg(feature = "cloud")]
    {
        let cfg = { state.config.read().await.cloud.clone() };
        if !cfg.enabled {
            return cloud_unavailable(req);
        }
        let params: TokenParams = match parse_params(req) {
            Ok(p) => p,
            Err(res) => return res,
        };
        if let Err(e) = crate::cloud::session::set_token(&params.token).await {
            return WsResponse::err(&req.id, "INTERNAL", e.to_string());
        }
        match crate::cloud::client::CloudClient::new(&cfg, params.token.clone())
            .me()
            .await
        {
            Ok(Some(v)) => {
                // Same unwrap as `cloud_status_json`: /auth/me wraps the
                // identity ({ "user": { ... } }) — return it flat.
                let user = v.get("user").cloned().or(Some(v));
                // Best-effort device registration (P2-9): a stable device
                // identity for future cloud sync. Never fails the login on
                // bind errors (mirrors the removed REST token handler).
                if let Err(e) = crate::cloud::device::bind(&cfg).await {
                    tracing::warn!("Cloud device bind failed: {e}");
                }
                WsResponse::ok(&req.id, serde_json::json!({ "ok": true, "user": user }))
            }
            Ok(None) => WsResponse::err(&req.id, "UNAUTHORIZED", "invalid cloud token"),
            Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
        }
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (state, req);
        cloud_unavailable(req)
    }
}

/// `cloud.logout` — forget the stored session token.
pub(crate) async fn handle_cloud_logout(req: &WsRequest, _state: &Arc<GatewayState>) -> WsResponse {
    #[cfg(feature = "cloud")]
    {
        let _ = crate::cloud::session::clear_token().await;
    }
    WsResponse::ok(&req.id, serde_json::json!({ "ok": true }))
}

// --- cloud.kb.* — cloud knowledge base management (thin WS passthroughs) ---
//
// The cloud server owns KB storage/indexing; these handlers only gate on
// `cloud.enabled` + a session token and forward to `CloudClient`. Upload
// shares the 32 MiB cap with local `kb.ingest`.

/// Shared gate: enabled + token, else `cloud_unavailable`.
#[cfg(feature = "cloud")]
pub(crate) async fn cloud_api_client(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> Result<crate::cloud::client::CloudClient, WsResponse> {
    let cfg = { state.config.read().await.cloud.clone() };
    if !cfg.enabled {
        return Err(cloud_unavailable(req));
    }
    let Some(token) = crate::cloud::session::get_token().await else {
        return Err(cloud_unavailable(req));
    };
    Ok(crate::cloud::client::CloudClient::new(&cfg, token))
}

/// KB-specific alias of [`cloud_api_client`].
#[cfg(feature = "cloud")]
pub(crate) async fn cloud_kb_client(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> Result<crate::cloud::client::CloudClient, WsResponse> {
    cloud_api_client(req, state).await
}

/// `cloud.kb.list` — list the account's knowledge bases.
pub(crate) async fn handle_cloud_kb_list(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[cfg(feature = "cloud")]
    match cloud_kb_client(req, state).await {
        Err(res) => res,
        Ok(client) => match client.list_kbs().await {
            Ok(v) => WsResponse::ok(&req.id, v),
            Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
        },
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (state, req);
        cloud_unavailable(req)
    }
}

/// `cloud.kb.create` — create a knowledge base (`{ name }`).
pub(crate) async fn handle_cloud_kb_create(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[cfg(feature = "cloud")]
    #[derive(Deserialize)]
    struct CreateParams {
        name: String,
    }
    #[cfg(feature = "cloud")]
    {
        let name = match parse_params::<CreateParams>(req) {
            Ok(p) => p.name.trim().to_string(),
            Err(res) => return res,
        };
        if name.is_empty() {
            return WsResponse::err(&req.id, "INVALID_PARAMS", "name is required");
        }
        match cloud_kb_client(req, state).await {
            Err(res) => res,
            Ok(client) => match client.kb_create(&name).await {
                Ok(v) => WsResponse::ok(&req.id, v),
                Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
            },
        }
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (state, req);
        cloud_unavailable(req)
    }
}

/// `cloud.kb.delete` — delete a knowledge base (`{ kb_id }`).
pub(crate) async fn handle_cloud_kb_delete(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[cfg(feature = "cloud")]
    #[derive(Deserialize)]
    struct DeleteParams {
        kb_id: String,
    }
    #[cfg(feature = "cloud")]
    {
        let kb_id = match parse_params::<DeleteParams>(req) {
            Ok(p) => p.kb_id.trim().to_string(),
            Err(res) => return res,
        };
        if kb_id.is_empty() {
            return WsResponse::err(&req.id, "INVALID_PARAMS", "kb_id is required");
        }
        match cloud_kb_client(req, state).await {
            Err(res) => res,
            Ok(client) => match client.kb_delete(&kb_id).await {
                Ok(()) => WsResponse::ok(&req.id, serde_json::json!({ "ok": true })),
                Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
            },
        }
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (state, req);
        cloud_unavailable(req)
    }
}

/// `cloud.kb.upload` — upload one document (base64) to a cloud KB.
pub(crate) async fn handle_cloud_kb_upload(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[cfg(feature = "cloud")]
    #[derive(Deserialize)]
    struct UploadParams {
        kb_id: String,
        filename: String,
        content_base64: String,
        #[serde(default)]
        mime: Option<String>,
    }
    #[cfg(feature = "cloud")]
    {
        let p: UploadParams = match parse_params(req) {
            Ok(p) => p,
            Err(res) => return res,
        };
        let bytes = match base64::engine::general_purpose::STANDARD.decode(&p.content_base64) {
            Ok(b) => b,
            Err(e) => {
                return WsResponse::err(&req.id, "INVALID_CONTENT", format!("Invalid base64: {e}"))
            }
        };
        if bytes.len() > crate::gateway::ws::kb_ws::MAX_KB_UPLOAD_BYTES {
            return WsResponse::err(
                &req.id,
                "INVALID_CONTENT",
                format!(
                    "File exceeds maximum size of {} MB",
                    crate::gateway::ws::kb_ws::MAX_KB_UPLOAD_BYTES / (1024 * 1024)
                ),
            );
        }
        let mime = p
            .mime
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| "application/octet-stream".into());
        match cloud_kb_client(req, state).await {
            Err(res) => res,
            Ok(client) => match client.kb_upload(&p.kb_id, &p.filename, &bytes, &mime).await {
                Ok(v) => WsResponse::ok(&req.id, v),
                Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
            },
        }
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (state, req);
        cloud_unavailable(req)
    }
}

/// `cloud.kb.query` — semantic test-query against a cloud KB.
pub(crate) async fn handle_cloud_kb_query(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[cfg(feature = "cloud")]
    #[derive(Deserialize)]
    struct QueryParams {
        kb_id: String,
        query: String,
        #[serde(default = "default_query_top_k")]
        top_k: usize,
    }
    #[cfg(feature = "cloud")]
    fn default_query_top_k() -> usize {
        5
    }
    #[cfg(feature = "cloud")]
    {
        let p: QueryParams = match parse_params(req) {
            Ok(p) => p,
            Err(res) => return res,
        };
        if p.query.trim().is_empty() {
            return WsResponse::err(&req.id, "INVALID_PARAMS", "query is required");
        }
        let top_k = p.top_k.clamp(1, 50);
        match cloud_kb_client(req, state).await {
            Err(res) => res,
            Ok(client) => match client.kb_query(&p.kb_id, &p.query, top_k).await {
                Ok(v) => WsResponse::ok(&req.id, v),
                Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
            },
        }
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (state, req);
        cloud_unavailable(req)
    }
}

// --- cloud.credits.* — credits marketing (thin WS passthroughs) ---
//
// Check-in / signup bonus / invite / packs / ledger. All gated by
// `cloud_api_client` (enabled + token) and forwarded to the cloud server,
// which owns the marketing rules and credit math.

/// `cloud.credits.claims` — marketing state (check-in streak, signup bonus).
pub(crate) async fn handle_cloud_credits_claims(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[cfg(feature = "cloud")]
    match cloud_api_client(req, state).await {
        Err(res) => res,
        Ok(client) => match client.credits_claims().await {
            Ok(v) => WsResponse::ok(&req.id, v),
            Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
        },
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (state, req);
        cloud_unavailable(req)
    }
}

/// `cloud.credits.daily_claim` — the daily check-in.
pub(crate) async fn handle_cloud_credits_daily_claim(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[cfg(feature = "cloud")]
    match cloud_api_client(req, state).await {
        Err(res) => res,
        Ok(client) => match client.credits_daily_claim().await {
            Ok(v) => WsResponse::ok(&req.id, v),
            Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
        },
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (state, req);
        cloud_unavailable(req)
    }
}

/// `cloud.credits.signup_claim` — the one-time signup bonus.
pub(crate) async fn handle_cloud_credits_signup_claim(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[cfg(feature = "cloud")]
    match cloud_api_client(req, state).await {
        Err(res) => res,
        Ok(client) => match client.credits_signup_claim().await {
            Ok(v) => WsResponse::ok(&req.id, v),
            Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
        },
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (state, req);
        cloud_unavailable(req)
    }
}

/// `cloud.credits.packs` — purchasable credit packs.
pub(crate) async fn handle_cloud_credits_packs(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[cfg(feature = "cloud")]
    match cloud_api_client(req, state).await {
        Err(res) => res,
        Ok(client) => match client.credits_packs().await {
            Ok(v) => WsResponse::ok(&req.id, v),
            Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
        },
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (state, req);
        cloud_unavailable(req)
    }
}

/// `cloud.credits.ledger` — recent credit ledger entries (`{ limit? }`,
/// default 50, clamped 1..=200).
pub(crate) async fn handle_cloud_credits_ledger(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[cfg(feature = "cloud")]
    #[derive(Deserialize, Default)]
    struct LedgerParams {
        limit: Option<u32>,
    }
    #[cfg(feature = "cloud")]
    {
        // Params are optional (the UI may send none).
        let p: LedgerParams = req
            .params
            .as_ref()
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        match cloud_api_client(req, state).await {
            Err(res) => res,
            Ok(client) => match client.credits_ledger(p.limit.unwrap_or(50)).await {
                Ok(v) => WsResponse::ok(&req.id, v),
                Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
            },
        }
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (state, req);
        cloud_unavailable(req)
    }
}

/// `cloud.credits.invite` — the account's invite code + reward progress.
pub(crate) async fn handle_cloud_credits_invite(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[cfg(feature = "cloud")]
    match cloud_api_client(req, state).await {
        Err(res) => res,
        Ok(client) => match client.invite().await {
            Ok(v) => WsResponse::ok(&req.id, v),
            Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
        },
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (state, req);
        cloud_unavailable(req)
    }
}

/// `cloud.credits.invite_redeem` — redeem someone else's invite code
/// (`{ code }`).
pub(crate) async fn handle_cloud_credits_invite_redeem(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[cfg(feature = "cloud")]
    #[derive(Deserialize)]
    struct RedeemParams {
        code: String,
    }
    #[cfg(feature = "cloud")]
    {
        let p: RedeemParams = match parse_params(req) {
            Ok(p) => p,
            Err(res) => return res,
        };
        let code = p.code.trim().to_string();
        if code.is_empty() {
            return WsResponse::err(&req.id, "INVALID_PARAMS", "code is required");
        }
        match cloud_api_client(req, state).await {
            Err(res) => res,
            Ok(client) => match client.invite_redeem(&code).await {
                Ok(v) => WsResponse::ok(&req.id, v),
                Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
            },
        }
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = (state, req);
        cloud_unavailable(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;

    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    /// Default config has cloud disabled → every cloud.kb.* method must
    /// report UNAUTHORIZED (not panic / not reach the network).
    #[tokio::test]
    async fn cloud_kb_methods_unauthorized_when_cloud_disabled() {
        let state = state().await;
        let cases: Vec<(&str, Option<serde_json::Value>)> = vec![
            ("cloud.kb.list", Some(serde_json::json!({}))),
            ("cloud.kb.create", Some(serde_json::json!({ "name": "x" }))),
            ("cloud.kb.delete", Some(serde_json::json!({ "kb_id": "kb1" }))),
            (
                "cloud.kb.upload",
                Some(serde_json::json!({
                    "kb_id": "kb1",
                    "filename": "a.md",
                    "content_base64": "aGk=",
                })),
            ),
            ("cloud.kb.query", Some(serde_json::json!({ "kb_id": "kb1", "query": "q" }))),
        ];
        for (method, params) in cases {
            let req = WsRequest {
                frame_type: "req".into(),
                id: "r1".into(),
                method: method.into(),
                params,
            };
            let resp = match method {
                "cloud.kb.list" => handle_cloud_kb_list(&req, &state).await,
                "cloud.kb.create" => handle_cloud_kb_create(&req, &state).await,
                "cloud.kb.delete" => handle_cloud_kb_delete(&req, &state).await,
                "cloud.kb.upload" => handle_cloud_kb_upload(&req, &state).await,
                _ => handle_cloud_kb_query(&req, &state).await,
            };
            assert!(!resp.ok, "{method} unexpectedly succeeded");
            assert_eq!(
                resp.error.as_ref().unwrap().code,
                "UNAUTHORIZED",
                "{method} wrong error code"
            );
        }
    }

    /// Default config has cloud disabled → every cloud.credits.* method must
    /// report UNAUTHORIZED (not panic / not reach the network).
    #[tokio::test]
    async fn cloud_credits_methods_unauthorized_when_cloud_disabled() {
        let state = state().await;
        let cases: Vec<(&str, Option<serde_json::Value>)> = vec![
            ("cloud.credits.claims", None),
            ("cloud.credits.daily_claim", None),
            ("cloud.credits.signup_claim", None),
            ("cloud.credits.packs", None),
            ("cloud.credits.ledger", Some(serde_json::json!({ "limit": 10 }))),
            ("cloud.credits.invite", None),
            ("cloud.credits.invite_redeem", Some(serde_json::json!({ "code": "ABC123" }))),
        ];
        for (method, params) in cases {
            let req = WsRequest {
                frame_type: "req".into(),
                id: "r1".into(),
                method: method.into(),
                params,
            };
            let resp = match method {
                "cloud.credits.claims" => handle_cloud_credits_claims(&req, &state).await,
                "cloud.credits.daily_claim" => handle_cloud_credits_daily_claim(&req, &state).await,
                "cloud.credits.signup_claim" => {
                    handle_cloud_credits_signup_claim(&req, &state).await
                }
                "cloud.credits.packs" => handle_cloud_credits_packs(&req, &state).await,
                "cloud.credits.ledger" => handle_cloud_credits_ledger(&req, &state).await,
                "cloud.credits.invite" => handle_cloud_credits_invite(&req, &state).await,
                _ => handle_cloud_credits_invite_redeem(&req, &state).await,
            };
            assert!(!resp.ok, "{method} unexpectedly succeeded");
            assert_eq!(
                resp.error.as_ref().unwrap().code,
                "UNAUTHORIZED",
                "{method} wrong error code"
            );
        }
    }
}
