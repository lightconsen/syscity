//! WS admin handlers: approvals.

use std::sync::Arc;

use serde::Deserialize;

use super::super::{parse_params, WsRequest, WsResponse};
use crate::gateway::GatewayState;
use tracing::warn;

// ── Approvals (human-in-the-loop tool approval) ─────────────────────────

/// `approvals.list` — pending tool-call approval requests.
pub(crate) async fn handle_approvals_list(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let approvals = state
        .tools
        .approval_queue
        .list_pending(crate::tools::approval::ApprovalFilter::default())
        .await;
    WsResponse::ok(&req.id, serde_json::json!({ "approvals": approvals, "count": approvals.len() }))
}

/// `approvals.get` — a single pending approval (`{ id }`).
pub(crate) async fn handle_approvals_get(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let id = match parse_params::<serde_json::Value>(req) {
        Ok(v) => v["id"].as_str().unwrap_or("").to_string(),
        Err(res) => return res,
    };
    match state.tools.approval_queue.get(&id).await {
        Some(approval) => {
            WsResponse::ok(&req.id, serde_json::to_value(&approval).unwrap_or_default())
        }
        None => WsResponse::err(&req.id, "NOT_FOUND", format!("Approval '{}' not found", id)),
    }
}

/// `approvals.approve` — approve a pending tool call (`{ id, remember? }`).
///
/// With `remember: true` the approved call also becomes a
/// `[permissions].allow` rule ("don't ask again"), persisted through the
/// config machinery. Resolution happens first and is never gated on the
/// rule write: the tool must run even if persisting fails.
pub(crate) async fn handle_approvals_approve(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        id: String,
        #[serde(default)]
        remember: bool,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    // Capture the call identity before resolution removes the request.
    let remember_rules = if p.remember {
        state.tools.approval_queue.get(&p.id).await.map(|summary| {
            crate::tools::permissions::remember_rules(&summary.tool_name, &summary.args)
        })
    } else {
        None
    };

    if state
        .tools
        .approval_queue
        .resolve(&p.id, crate::tools::approval::ApprovalDecision::Approve)
        .await
    {
        // Best-effort: an approval must not fail because a rule could not be
        // recorded. A compound command remembers one rule per segment, so the
        // list can be longer than one.
        let mut remembered_rules = Vec::new();
        if let Some(rules) = remember_rules {
            match remember_allow_rules(state, &rules).await {
                Ok(newly) => remembered_rules = newly,
                Err(e) => warn!("Failed to remember allow rules {rules:?}: {e}"),
            }
        }
        WsResponse::ok(
            &req.id,
            serde_json::json!({ "id": p.id, "status": "approved", "remembered_rules": remembered_rules }),
        )
    } else {
        WsResponse::err(&req.id, "NOT_FOUND", format!("Approval '{}' not found", p.id))
    }
}

/// Append `rules` to `[permissions].allow` under the config write lock and
/// persist to disk. Returns the rules that were actually new (an approval of
/// a compound command remembers one rule per chain segment, most of which
/// may already be present). The gateway's own internal write: no CAS —
/// external clients keep using `base_revision` on `config.set`.
async fn remember_allow_rules(
    state: &Arc<GatewayState>,
    rules: &[String],
) -> crate::Result<Vec<String>> {
    let mut guard = state.config.write().await;
    let mut newly = Vec::new();
    {
        let config = Arc::make_mut(&mut guard);
        for rule in rules {
            if config.permissions.allow.iter().any(|r| r == rule) {
                continue;
            }
            config.permissions.allow.push(rule.to_string());
            newly.push(rule.to_string());
        }
    }
    if newly.is_empty() {
        return Ok(newly);
    }
    if let Some(config_path) = state.config_path.clone() {
        let config = Arc::make_mut(&mut guard);
        crate::gateway::handlers::config::persist_config_atomic(config, &config_path)
            .await
            .map_err(|e| crate::SyscityError::Validation(format!("persist failed: {e}")))?;
    }
    let updated = guard.permissions.clone();
    drop(guard);
    state.tools.registry.permissions().reload(&updated);
    Ok(newly)
}

/// `approvals.deny` — deny a pending tool call (`{ id, reason? }`).
pub(crate) async fn handle_approvals_deny(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        id: String,
        #[serde(default)]
        reason: Option<String>,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let reason = p.reason.unwrap_or_else(|| "Denied by operator".to_string());
    if state
        .tools
        .approval_queue
        .resolve(&p.id, crate::tools::approval::ApprovalDecision::Deny { reason: reason.clone() })
        .await
    {
        WsResponse::ok(
            &req.id,
            serde_json::json!({ "id": p.id, "status": "denied", "reason": reason }),
        )
    } else {
        WsResponse::err(&req.id, "NOT_FOUND", format!("Approval '{}' not found", p.id))
    }
}
