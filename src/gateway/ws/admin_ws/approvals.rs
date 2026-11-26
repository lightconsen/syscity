//! WS admin handlers: approvals.

use std::sync::Arc;

use serde::Deserialize;

use super::super::{parse_params, WsRequest, WsResponse};
use crate::gateway::GatewayState;
use crate::security::request_context::RequestContext;
use tracing::warn;

// ── Approvals (human-in-the-loop tool approval) ─────────────────────────

/// The approvals this caller may see, as summaries.
///
/// An unbound caller sees every pending approval. A connection bound to one
/// agent sees only those raised in that agent's conversations: an approval is
/// a decision about someone's work, and a one-agent client has no business
/// approving another agent's tool call.
async fn visible_approvals(
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> Vec<crate::tools::approval::PendingApprovalSummary> {
    let all = state
        .tools
        .approval_queue
        .list_pending(crate::tools::approval::ApprovalFilter::default())
        .await;
    let Some(bound) = ctx.bound_agent() else {
        return all;
    };
    let mut visible = Vec::with_capacity(all.len());
    for approval in all {
        if crate::gateway::ws::bound::approval_visible(state, bound, approval.session_id.as_deref())
            .await
        {
            visible.push(approval);
        }
    }
    visible
}

/// `approvals.list` — pending tool-call approval requests.
pub(crate) async fn handle_approvals_list(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    let approvals = visible_approvals(state, ctx).await;
    WsResponse::ok(&req.id, serde_json::json!({ "approvals": approvals, "count": approvals.len() }))
}

/// `approvals.get` — a single pending approval (`{ id }`).
pub(crate) async fn handle_approvals_get(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    match state.tools.approval_queue.get(&id).await {
        Some(approval) => {
            if let Some(bound) = ctx.bound_agent() {
                if !crate::gateway::ws::bound::approval_visible(
                    state,
                    bound,
                    approval.session_id.as_deref(),
                )
                .await
                {
                    // Reported as not-found: the caller may not know the
                    // approval exists, and distinguishing "not yours" from
                    // "not there" would leak the inventory.
                    return WsResponse::err(
                        &req.id,
                        "NOT_FOUND",
                        format!("Approval '{}' not found", id),
                    );
                }
            }
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
    ctx: &RequestContext,
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

    if let Some(bound) = ctx.bound_agent() {
        let session = state
            .tools
            .approval_queue
            .get(&p.id)
            .await
            .and_then(|a| a.session_id);
        if !crate::gateway::ws::bound::approval_visible(state, bound, session.as_deref()).await {
            return WsResponse::err(&req.id, "NOT_FOUND", format!("Approval '{}' not found", p.id));
        }
    }

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
        // An operator may approve something that was also posted to a chat. The
        // chat entry has to go with it, or the person's next ordinary message is
        // read as an answer to a prompt nobody is waiting on.
        state.channels.channel_answers.clear_prompt(&p.id).await;
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
    // The document before this write, so only the appended rules are written
    // and the rest of the user's file — comments included — is untouched.
    let before = Arc::make_mut(&mut guard).clone();
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
        crate::gateway::handlers::config::persist_config_atomic(
            config,
            &config_path,
            Some(&before),
        )
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
    ctx: &RequestContext,
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

    if let Some(bound) = ctx.bound_agent() {
        let session = state
            .tools
            .approval_queue
            .get(&p.id)
            .await
            .and_then(|a| a.session_id);
        if !crate::gateway::ws::bound::approval_visible(state, bound, session.as_deref()).await {
            return WsResponse::err(&req.id, "NOT_FOUND", format!("Approval '{}' not found", p.id));
        }
    }
    let reason = p.reason.unwrap_or_else(|| "Denied by operator".to_string());
    if state
        .tools
        .approval_queue
        .resolve(&p.id, crate::tools::approval::ApprovalDecision::Deny { reason: reason.clone() })
        .await
    {
        state.channels.channel_answers.clear_prompt(&p.id).await;
        WsResponse::ok(
            &req.id,
            serde_json::json!({ "id": p.id, "status": "denied", "reason": reason }),
        )
    } else {
        WsResponse::err(&req.id, "NOT_FOUND", format!("Approval '{}' not found", p.id))
    }
}
