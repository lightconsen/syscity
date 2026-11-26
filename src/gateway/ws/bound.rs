//! What a connection bound to one agent may do.
//!
//! An agent access token
//! ([`SecurityConfig::agent_tokens`](crate::gateway::config::SecurityConfig::agent_tokens))
//! authenticates as a real principal and carries a `bound_agent`. Scopes alone
//! cannot express "this credential may reach *this* agent": every agent-scoped
//! method is `read` or `chat`, and narrowing scopes to say "one agent" would
//! mean inventing a scope per agent.
//!
//! So a bound connection is constrained twice: its method surface is the
//! single-agent one below (a client that is configured with one agent has no
//! business in `config.set` or `plugins.install`, whatever its scopes say), and
//! every agent-selecting or session-selecting parameter is checked against its
//! binding in [`check_params`]. Both live here so "bound" is enforced in one
//! place: a surface that forgets to ask is a surface that leaks.
//!
//! The default is refusal. A method not in [`ALLOWED`] is refused, so adding a
//! WS method cannot widen an agent-bound connection by accident — the new
//! method is simply not reachable to one until someone decides it should be.

use std::sync::Arc;

use crate::gateway::protocol::{WsRequest, WsResponse};
use crate::gateway::GatewayState;
use crate::security::request_context::RequestContext;

/// The methods a one-agent client needs, and nothing else.
///
/// Grouped by why each is here:
///
/// - **Talking to its agent**: `chat.send`, `chat.history`, `chat.abort`.
/// - **Its own conversations**: the `sessions.*` family. Which sessions it can
///   actually touch is decided by [`session_belongs`], not by this list — the
///   list only says the *kind* of call is in scope.
/// - **Answering its agent**: `ask.respond`, `approvals.*`. Same story: the
///   approval belongs to a session, and [`session_belongs`] decides whether
///   that session is this client's.
/// - **Describing itself**: `agents.list`, `agents.get`, `agents.registry` and
///   `agents.get_config`, whose payloads are narrowed to the bound agent so the
///   client shows one agent rather than a refused sidebar of them.
/// - **Its workspace**: `workspace.list`, `workspace.read`, likewise narrowed.
/// - **Liveness**: `ping`, `methods.list`, `health`, `system.presence`.
/// - **The model picker**: `models.list`, `models.presets`, `models.default`,
///   which only list what the user could already be running.
///
/// Deliberately absent: everything that configures the deployment
/// (`config.*`, `plugins.*`, `providers.*`, `channels.*`, `mcp.*`, `skills.*`,
/// `connectors.*`, `cron.*`, `tasks.*`, `update.*`, `cloud.*`, `onboarding.*`,
/// `device.*`, `acp.*`, `computer.*`, `kb.*`, `memory.*`, `audit.*`,
/// `cost.*`, `traces.*`, `turns.*`, `mention.*`, `security.*`,
/// `auth_profiles.*`, `eval.*`, `logs.*`, `feedback.vote`, `commands.execute`,
/// `debug.tool.invoke`) — and `agents.create`/`delete`/`purge`/`rename`/
/// `update`/`import`/`export`/`default`/`memory.*`, which change *which* agents
/// exist rather than talking to one.
pub const ALLOWED: &[&str] = &[
    // Liveness and self-description.
    "ping",
    "methods.list",
    "health",
    "system.presence",
    // Talking to its agent.
    "chat.send",
    "chat.history",
    "chat.abort",
    // Its own conversations.
    "sessions.list",
    "sessions.create",
    "sessions.delete",
    "sessions.rename",
    "sessions.undo",
    "sessions.plan",
    "sessions.branch",
    "sessions.snapshot",
    "sessions.snapshots",
    "sessions.rollback",
    "sessions.export",
    "sessions.import",
    "sessions.set_pinned",
    "sessions.set_model",
    "sessions.set_mode",
    "sessions.get_mode",
    "sessions.status",
    "sessions.reset",
    "sessions.subscribe",
    "sessions.unsubscribe",
    // Answering its agent.
    "ask.respond",
    "approvals.list",
    "approvals.get",
    "approvals.approve",
    "approvals.deny",
    // The agent itself, and the workspace it works in.
    "agents.list",
    "agents.get",
    "agents.registry",
    "agents.get_config",
    "workspace.list",
    "workspace.read",
    // The model picker.
    "models.list",
    "models.presets",
    "models.default",
];

/// Whether a bound connection may call `method` at all.
///
/// Scope is still checked separately by the dispatcher; this is the second,
/// narrower gate that no scope can widen.
pub fn method_allowed(method: &str) -> bool {
    ALLOWED.contains(&method)
}

/// The refusal a bound connection gets for a method outside its surface.
///
/// It names the method rather than the missing scope: the client is not
/// under-privileged, it is the wrong shape of client for this call, and the
/// distinction is what tells someone debugging a connector "you need the
/// operator's credential" instead of "ask for more scopes".
pub fn method_refusal(id: impl Into<String>, method: &str, bound: &str) -> WsResponse {
    WsResponse::err(
        id,
        "FORBIDDEN",
        format!(
            "This connection is bound to agent '{bound}' and may not call '{method}'. \
             Use the gateway's own credential for operator methods."
        ),
    )
}

/// Check the agent-selecting parameters of a request against the binding.
///
/// Two kinds of parameter name an agent:
///
/// - `agent_id`, on `sessions.create`, `agents.get`, `workspace.*` and
///   `sessions.list`. A different one is refused.
/// - `session_id`, on almost every `sessions.*` and on `chat.*`. The session
///   is checked against the router's binding rather than the parameter
///   directly: naming someone else's session by id is the same act as naming
///   their agent, and an existing session's agent is not the caller's to choose
///   either.
///
/// A session with no binding yet (a fresh `ws:alice` before its first message)
/// is accepted: it is about to be bound to this connection's agent by
/// `chat.send` or `sessions.create`, so refusing it would refuse the client's
/// own new conversation.
///
/// Returns the refusal to send, or `None` to proceed.
pub async fn check_params(
    req: &WsRequest,
    ctx: &RequestContext,
    state: &Arc<GatewayState>,
) -> Option<WsResponse> {
    let bound = ctx.bound_agent()?;

    let params = match req.params.as_ref() {
        Some(serde_json::Value::Object(map)) => map,
        _ => return None,
    };

    if let Some(agent_id) = params.get("agent_id").and_then(|v| v.as_str()) {
        if !agent_id.is_empty() && agent_id != bound {
            return Some(agent_refusal(&req.id, bound, agent_id));
        }
    }

    // `agents.get` and `agents.get_config` carry the agent id under a second
    // name; treating one spelling as authoritative and the other as free text
    // is exactly how a check gets bypassed.
    if matches!(req.method.as_str(), "agents.get" | "agents.get_config") {
        if let Some(agent_id) = params.get("id").and_then(|v| v.as_str()) {
            if !agent_id.is_empty() && agent_id != bound {
                return Some(agent_refusal(&req.id, bound, agent_id));
            }
        }
    }

    if let Some(session_id) = params.get("session_id").and_then(|v| v.as_str()) {
        if !session_belongs(state, bound, session_id).await {
            return Some(WsResponse::err(
                &req.id,
                "FORBIDDEN",
                format!(
                    "Session '{session_id}' does not belong to the agent this connection is \
                     bound to ('{bound}')."
                ),
            ));
        }
    }

    // `sessions.subscribe`/`unsubscribe` take a list.
    if let Some(ids) = params.get("session_ids").and_then(|v| v.as_array()) {
        for id in ids.iter().filter_map(|v| v.as_str()) {
            if !session_belongs(state, bound, id).await {
                return Some(WsResponse::err(
                    &req.id,
                    "FORBIDDEN",
                    format!(
                        "Session '{id}' does not belong to the agent this connection is bound \
                         to ('{bound}')."
                    ),
                ));
            }
        }
    }

    None
}

fn agent_refusal(id: &str, bound: &str, requested: &str) -> WsResponse {
    WsResponse::err(
        id,
        "FORBIDDEN",
        format!("This connection is bound to agent '{bound}' and may not use agent '{requested}'."),
    )
}

/// Whether an approval is part of this agent's work.
///
/// An approval is raised by a tool call inside a conversation, so the
/// conversation is what says whose it is. Two cases are refused for a bound
/// connection:
///
/// - A session that belongs to another agent.
/// - No session at all. Such an approval comes from an operator-level call
///   (`computer.run` from the settings page, a run with no conversation), which
///   a one-agent client has no surface for in the first place.
pub async fn approval_visible(
    state: &Arc<GatewayState>,
    bound: &str,
    session_id: Option<&str>,
) -> bool {
    match session_id {
        Some(session_id) => session_belongs(state, bound, session_id).await,
        None => false,
    }
}

/// Whether a session is this agent's to touch.
///
/// The router is the authority on which agent a session runs as: it is the thing
/// that decides at message time, and its binding is what `chat.send` consults.
/// Asking the same question here keeps the two from disagreeing.
///
/// A session that does not exist yet, or has no binding, resolves to the global
/// default. That is *not* this connection's agent in general — a bound client
/// whose session has not been bound yet is about to have it bound, so the
/// decision belongs to the caller; this function says "no opinion" (`true`) for
/// an unbound session rather than guessing.
pub async fn session_belongs(state: &Arc<GatewayState>, bound: &str, session_id: &str) -> bool {
    if session_id.is_empty() {
        return true;
    }
    let binding = state.agents.router.list_bindings().await;
    match binding.get(session_id) {
        Some((agent_id, _)) => agent_id == bound,
        // No binding: either brand new or created outside the WS path. Do not
        // invent an owner.
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;
    use crate::security::request_context::AuthSource;

    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    fn req(id: &str, method: &str, params: serde_json::Value) -> WsRequest {
        WsRequest {
            frame_type: "req".into(),
            id: id.into(),
            method: method.into(),
            params: Some(params),
        }
    }

    fn bound_ctx(agent: &str) -> RequestContext {
        RequestContext::new("alice", AuthSource::SharedToken).with_bound_agent(Some(agent))
    }

    /// The surface is a closed allow-list: a method that is not named is
    /// refused, so a newly added WS method is not reachable from an agent-bound
    /// connection until someone decides it should be.
    #[test]
    fn an_unnamed_method_is_refused() {
        assert!(!method_allowed("config.set"));
        assert!(!method_allowed("plugins.install"));
        assert!(!method_allowed("computer.run"));
        assert!(!method_allowed("agents.create"));
        assert!(!method_allowed("a.method.that.does.not.exist"));
    }

    #[test]
    fn the_single_agent_surface_is_allowed() {
        for method in [
            "chat.send",
            "chat.history",
            "sessions.list",
            "ask.respond",
            "ping",
        ] {
            assert!(method_allowed(method), "{method} must be reachable to a one-agent client");
        }
    }

    /// An unbound context is not checked at all — every existing credential
    /// keeps exactly the behaviour it had.
    #[tokio::test]
    async fn an_unbound_context_passes_every_check() {
        let state = state().await;
        let ctx = RequestContext::new("shared", AuthSource::SharedToken);
        let create =
            req("r1", "sessions.create", serde_json::json!({ "agent_id": "someone-else" }));
        assert!(check_params(&create, &ctx, &state).await.is_none());
    }

    #[tokio::test]
    async fn a_different_agent_id_is_refused() {
        let state = state().await;
        let ctx = bound_ctx("secretary-alice");
        let create =
            req("r1", "sessions.create", serde_json::json!({ "agent_id": "secretary-bob" }));
        let refusal = check_params(&create, &ctx, &state)
            .await
            .expect("a contradicting agent_id must be refused");
        assert_eq!(refusal.error.as_ref().unwrap().code, "FORBIDDEN");
    }

    #[tokio::test]
    async fn its_own_agent_id_is_accepted() {
        let state = state().await;
        let ctx = bound_ctx("secretary-alice");
        let create =
            req("r1", "sessions.create", serde_json::json!({ "agent_id": "secretary-alice" }));
        assert!(check_params(&create, &ctx, &state).await.is_none());
    }

    /// `agents.get` names the agent under `id`; a check that only looked at
    /// `agent_id` would leave the whole agent directory readable.
    #[tokio::test]
    async fn agents_get_for_another_agent_is_refused() {
        let state = state().await;
        let ctx = bound_ctx("secretary-alice");
        let other = req("r1", "agents.get", serde_json::json!({ "id": "secretary-bob" }));
        assert!(check_params(&other, &ctx, &state).await.is_some());

        let own = req("r2", "agents.get", serde_json::json!({ "id": "secretary-alice" }));
        assert!(check_params(&own, &ctx, &state).await.is_none());
    }

    /// A session bound to another agent is not this client's to name, whether
    /// it is addressed directly or through a subscribe list.
    #[tokio::test]
    async fn another_agents_session_is_refused() {
        let state = state().await;
        state
            .agents
            .router
            .bind_session(
                "ws:bob",
                &crate::inbound::RouteResult {
                    agent_id: "secretary-bob".into(),
                    workspace_id: None,
                    persisted_binding: false,
                    is_fallback: false,
                },
            )
            .await;
        let ctx = bound_ctx("secretary-alice");

        let direct = req("r1", "chat.history", serde_json::json!({ "session_id": "ws:bob" }));
        assert!(check_params(&direct, &ctx, &state).await.is_some());

        let listed =
            req("r2", "sessions.subscribe", serde_json::json!({ "session_ids": ["ws:bob"] }));
        assert!(check_params(&listed, &ctx, &state).await.is_some());
    }

    /// A session already bound to this agent, and a session with no binding at
    /// all (the client's own, about to be bound), both pass.
    #[tokio::test]
    async fn its_own_and_unbound_sessions_are_accepted() {
        let state = state().await;
        state
            .agents
            .router
            .bind_session(
                "ws:alice",
                &crate::inbound::RouteResult {
                    agent_id: "secretary-alice".into(),
                    workspace_id: None,
                    persisted_binding: false,
                    is_fallback: false,
                },
            )
            .await;
        let ctx = bound_ctx("secretary-alice");

        let own = req("r1", "chat.history", serde_json::json!({ "session_id": "ws:alice" }));
        assert!(check_params(&own, &ctx, &state).await.is_none());

        let fresh = req("r2", "chat.history", serde_json::json!({ "session_id": "ws:brand-new" }));
        assert!(check_params(&fresh, &ctx, &state).await.is_none());
    }

    /// A method with no agent-selecting parameters passes the parameter check;
    /// the surface check is what constrains it.
    #[tokio::test]
    async fn a_paramless_method_passes_the_param_check() {
        let state = state().await;
        let ctx = bound_ctx("secretary-alice");
        let ping = req("r1", "ping", serde_json::json!({}));
        assert!(check_params(&ping, &ctx, &state).await.is_none());
    }
}
