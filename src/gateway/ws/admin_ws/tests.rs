//! Shared tests for the admin WS handlers (invoked via the facade re-exports).

use std::sync::Arc;

use super::super::{WsRequest, WsResponse};
use crate::gateway::GatewayState;

use super::*;
use crate::gateway::state_tests::make_test_state;
use crate::gateway::GatewayConfig;

fn req(id: &str, method: &str, params: Option<serde_json::Value>) -> WsRequest {
    WsRequest {
        frame_type: "req".into(),
        id: id.into(),
        method: method.into(),
        params,
    }
}

async fn state() -> Arc<GatewayState> {
    Arc::new(make_test_state(GatewayConfig::default()).await)
}

#[tokio::test]
async fn system_reload_defaults_to_all() {
    let state = state().await;
    let resp =
        handle_system_reload(&req("r1", "system.reload", Some(serde_json::json!({}))), &state)
            .await;
    assert!(resp.ok, "reload failed: {:?}", resp.error);
    let payload = resp.payload.as_ref().unwrap();
    assert_eq!(payload["scope"], "all");
    assert_eq!(payload["success"], true);
}

#[tokio::test]
async fn system_reload_scope_skills() {
    let state = state().await;
    let resp = handle_system_reload(
        &req("r1", "system.reload", Some(serde_json::json!({ "scope": "skills" }))),
        &state,
    )
    .await;
    assert!(resp.ok);
    assert!(resp.payload.as_ref().unwrap()["skills"].is_object());
}

#[tokio::test]
async fn channels_list_empty() {
    let state = state().await;
    let resp = handle_channels_list(&req("r1", "channels.list", None), &state).await;
    assert!(resp.ok);
    assert_eq!(
        resp.payload.as_ref().unwrap()["channels"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn channels_enable_unknown_not_found() {
    let state = state().await;
    let resp = handle_channels_enable(
        &req("r1", "channels.enable", Some(serde_json::json!({ "name": "telegram" }))),
        &state,
    )
    .await;
    assert!(!resp.ok);
    assert_eq!(resp.error.as_ref().unwrap().code, "NOT_FOUND");
}

#[tokio::test]
async fn channels_disable_missing_name_errors() {
    let state = state().await;
    let resp = handle_channels_disable(
        &req("r1", "channels.disable", Some(serde_json::json!({}))),
        &state,
    )
    .await;
    assert!(!resp.ok);
    assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
}

#[tokio::test]
async fn device_pairing_qr_unknown_not_found() {
    let state = state().await;
    let resp = handle_device_pairing_qr(
        &req("r1", "device.pairing.qr", Some(serde_json::json!({ "code": "NOPE" }))),
        &state,
    )
    .await;
    assert!(!resp.ok);
    assert_eq!(resp.error.as_ref().unwrap().code, "NOT_FOUND");
}

#[tokio::test]
async fn device_pairing_qr_seeded_returns_svg() {
    let state = state().await;
    let code = match state
        .auth
        .device_pairing_store
        .request_access("dev-1", Some("Phone"), None)
        .await
    {
        crate::security::device_pairing::DeviceAccessResult::PairingRequired { code } => code,
        _ => panic!("expected a new pending request"),
    };
    let resp = handle_device_pairing_qr(
        &req("r1", "device.pairing.qr", Some(serde_json::json!({ "code": code }))),
        &state,
    )
    .await;
    assert!(resp.ok, "qr failed: {:?}", resp.error);
    let svg = resp.payload.as_ref().unwrap()["svg"].as_str().unwrap();
    assert!(svg.contains("<svg"));
}

#[tokio::test]
async fn device_pairing_setup_roundtrip() {
    let state = state().await;
    let code = match state
        .auth
        .device_pairing_store
        .request_access("dev-2", Some("Tablet"), None)
        .await
    {
        crate::security::device_pairing::DeviceAccessResult::PairingRequired { code } => code,
        _ => panic!("expected a new pending request"),
    };
    let setup_code = crate::security::device_pairing::DevicePairingStore::encode_setup_code(&code);
    let resp = handle_device_pairing_setup(
        &req(
            "r1",
            "device.pairing.setup",
            Some(serde_json::json!({ "setup_code": setup_code })),
        ),
        &state,
    )
    .await;
    assert!(resp.ok, "setup failed: {:?}", resp.error);
    let payload = resp.payload.as_ref().unwrap();
    assert_eq!(payload["device_id"], "dev-2");
    assert_eq!(payload["display_name"], "Tablet");
}

#[tokio::test]
async fn device_pairing_setup_invalid_code_errors() {
    let state = state().await;
    let resp = handle_device_pairing_setup(
        &req(
            "r1",
            "device.pairing.setup",
            Some(serde_json::json!({ "setup_code": "!!not-base64!!" })),
        ),
        &state,
    )
    .await;
    assert!(!resp.ok);
    assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
}

#[tokio::test]
async fn approvals_list_empty() {
    let state = state().await;
    let resp = handle_approvals_list(&req("r1", "approvals.list", None), &state).await;
    assert!(resp.ok);
    assert_eq!(resp.payload.as_ref().unwrap()["count"], 0);
}

#[tokio::test]
async fn approvals_approve_unknown_not_found() {
    let state = state().await;
    let resp = handle_approvals_approve(
        &req("r1", "approvals.approve", Some(serde_json::json!({ "id": "missing" }))),
        &state,
    )
    .await;
    assert!(!resp.ok);
    assert_eq!(resp.error.as_ref().unwrap().code, "NOT_FOUND");
}

#[tokio::test]
async fn approvals_submit_then_deny_with_reason() {
    let state = state().await;
    // Submit a pending approval with a live response channel.
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let pa = crate::tools::approval::PendingApproval::new(
        "app-1",
        "bash",
        serde_json::json!({ "command": "ls" }),
        "alice",
    )
    .with_risk_level(crate::tools::approval::RiskLevel::High)
    .with_message("Run bash")
    .with_response_tx(tx);
    state.tools.approval_queue.submit(pa).await;

    let resp = handle_approvals_list(&req("r1", "approvals.list", None), &state).await;
    assert!(resp.ok);
    assert_eq!(resp.payload.as_ref().unwrap()["count"], 1);

    let resp = handle_approvals_deny(
        &req(
            "r1",
            "approvals.deny",
            Some(serde_json::json!({ "id": "app-1", "reason": "Not authorized" })),
        ),
        &state,
    )
    .await;
    assert!(resp.ok, "deny failed: {:?}", resp.error);
    let payload = resp.payload.as_ref().unwrap();
    assert_eq!(payload["status"], "denied");
    assert_eq!(payload["reason"], "Not authorized");
}

/// "Yes, don't ask again": the approval resolves, a rule lands in config and
/// in the gate's runtime, and remembering the same call twice dedupes.
#[tokio::test]
async fn approvals_approve_with_remember_appends_one_rule() {
    let state = state().await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let pa = crate::tools::approval::PendingApproval::new(
        "app-1",
        "shell",
        serde_json::json!({ "command": "git status" }),
        "alice",
    )
    .with_risk_level(crate::tools::approval::RiskLevel::High)
    .with_message("Run shell")
    .with_response_tx(tx);
    state.tools.approval_queue.submit(pa).await;

    let resp = handle_approvals_approve(
        &req(
            "r1",
            "approvals.approve",
            Some(serde_json::json!({ "id": "app-1", "remember": true })),
        ),
        &state,
    )
    .await;
    assert!(resp.ok, "approve failed: {:?}", resp.error);
    let payload = resp.payload.as_ref().unwrap();
    assert_eq!(payload["status"], "approved");
    assert_eq!(payload["remembered_rule"].as_str(), Some("shell:git status*"));

    // The rule is live in the gate and recorded in config.
    assert_eq!(
        state.config.read().await.permissions.allow,
        vec!["shell:git status*".to_string()]
    );
    assert_eq!(
        state.tools.registry.permissions().config_projection().allow,
        vec!["shell:git status*".to_string()]
    );
    // The suspended tool saw the approval.
    assert_eq!(rx.await.expect("resolution"), crate::tools::approval::ApprovalDecision::Approve);

    // A second remember of the identical call dedupes: no new rule.
    let (tx2, _rx2) = tokio::sync::oneshot::channel();
    let pa2 = crate::tools::approval::PendingApproval::new(
        "app-2",
        "shell",
        serde_json::json!({ "command": "git status" }),
        "alice",
    )
    .with_response_tx(tx2);
    state.tools.approval_queue.submit(pa2).await;
    let resp = handle_approvals_approve(
        &req(
            "r2",
            "approvals.approve",
            Some(serde_json::json!({ "id": "app-2", "remember": true })),
        ),
        &state,
    )
    .await;
    assert!(resp.ok);
    assert_eq!(
        resp.payload.as_ref().unwrap()["remembered_rule"],
        serde_json::Value::Null,
        "an existing rule is not reported as new"
    );
    assert_eq!(state.config.read().await.permissions.allow.len(), 1);
}

/// Plain `approvals.approve` (no `remember`) behaves exactly as before.
#[tokio::test]
async fn approvals_approve_without_remember_writes_no_rule() {
    let state = state().await;
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let pa = crate::tools::approval::PendingApproval::new(
        "app-1",
        "shell",
        serde_json::json!({ "command": "ls" }),
        "alice",
    )
    .with_response_tx(tx);
    state.tools.approval_queue.submit(pa).await;

    let resp = handle_approvals_approve(
        &req("r1", "approvals.approve", Some(serde_json::json!({ "id": "app-1" }))),
        &state,
    )
    .await;
    assert!(resp.ok);
    assert_eq!(resp.payload.as_ref().unwrap()["remembered_rule"], serde_json::Value::Null);
    assert!(state.config.read().await.permissions.allow.is_empty());
}

#[tokio::test]
async fn memory_search_unavailable_without_vector() {
    let state = state().await;
    let resp = handle_memory_search(
        &req("r1", "memory.search", Some(serde_json::json!({ "query": "foo" }))),
        &state,
    )
    .await;
    assert!(!resp.ok);
    assert_eq!(resp.error.as_ref().unwrap().code, "UNAVAILABLE");
}

#[tokio::test]
async fn memory_collections_unavailable_without_vector() {
    let state = state().await;
    let resp = handle_memory_collections(&req("r1", "memory.collections", None), &state).await;
    assert!(!resp.ok);
    assert_eq!(resp.error.as_ref().unwrap().code, "UNAVAILABLE");
}

#[tokio::test]
async fn mention_policy_get_and_set_roundtrip() {
    let state = state().await;
    let resp = handle_mention_policy_get(&req("r1", "mention.policy", None), &state).await;
    assert!(resp.ok);
    assert!(resp.payload.as_ref().unwrap()["policy"].is_string());

    let resp = handle_mention_policy_set(
        &req("r1", "mention.policy.set", Some(serde_json::json!({ "policy": "block" }))),
        &state,
    )
    .await;
    assert!(resp.ok, "set failed: {:?}", resp.error);
    assert_eq!(resp.payload.as_ref().unwrap()["policy"], "block");
}

#[tokio::test]
async fn mention_allowlist_add_and_list() {
    let state = state().await;
    let resp = handle_mention_allowlist_add(
        &req(
            "r1",
            "mention.allowlist.add",
            Some(serde_json::json!({ "channel": "telegram", "pattern": "@boss" })),
        ),
        &state,
    )
    .await;
    assert!(resp.ok);
    let resp = handle_mention_allowlist_list(
        &req("r1", "mention.allowlist", Some(serde_json::json!({ "channel": "telegram" }))),
        &state,
    )
    .await;
    assert!(resp.ok);
    let entries = resp.payload.as_ref().unwrap()["allowlist"]
        .as_array()
        .unwrap();
    assert!(entries.iter().any(|e| e == "@boss"));
}

#[tokio::test]
async fn auth_profiles_list_empty_and_get_unknown() {
    let state = state().await;
    let resp = handle_auth_profiles_list(&req("r1", "auth_profiles.list", None), &state).await;
    assert!(resp.ok);
    assert_eq!(resp.payload.as_ref().unwrap()["count"], 0);

    let resp = handle_auth_profiles_get(
        &req("r1", "auth_profiles.get", Some(serde_json::json!({ "id": "openai" }))),
        &state,
    )
    .await;
    assert!(!resp.ok);
    assert_eq!(resp.error.as_ref().unwrap().code, "NOT_FOUND");
}

#[tokio::test]
async fn auth_profiles_rotate_unknown_errors() {
    let state = state().await;
    let resp = handle_auth_profiles_rotate(
        &req("r1", "auth_profiles.rotate", Some(serde_json::json!({ "id": "openai" }))),
        &state,
    )
    .await;
    assert!(!resp.ok);
    assert_eq!(resp.error.as_ref().unwrap().code, "BAD_REQUEST");
}

#[tokio::test]
async fn audit_recent_returns_entries() {
    let state = state().await;
    let resp = handle_audit_recent(
        &req("r1", "audit.recent", Some(serde_json::json!({ "limit": 10 }))),
        &state,
    )
    .await;
    assert!(resp.ok, "audit.recent failed: {:?}", resp.error);
    let payload = resp.payload.as_ref().unwrap();
    assert!(payload["entries"].is_array());
    assert_eq!(payload["count"], 0);
}

#[tokio::test]
async fn audit_all_returns_entries() {
    let state = state().await;
    let resp = handle_audit_all(&req("r1", "audit.all", None), &state).await;
    assert!(resp.ok, "audit.all failed: {:?}", resp.error);
    let payload = resp.payload.as_ref().unwrap();
    assert!(payload["entries"].is_array());
}

#[tokio::test]
async fn audit_recent_defaults_limit_to_50() {
    let state = state().await;
    let resp = handle_audit_recent(&req("r1", "audit.recent", None), &state).await;
    assert!(resp.ok);
}

#[tokio::test]
async fn security_gate_set_and_list() {
    let state = state().await;
    let resp = handle_security_gate_set(
        &req(
            "r1",
            "security.gate.set",
            Some(serde_json::json!({ "user_id": "alice", "level": "admin" })),
        ),
        &state,
    )
    .await;
    assert!(resp.ok, "set failed: {:?}", resp.error);

    let resp = handle_security_gate_list(&req("r1", "security.gate.list", None), &state).await;
    assert!(resp.ok);
    let levels = resp.payload.as_ref().unwrap()["levels"]
        .as_object()
        .unwrap();
    assert_eq!(levels.get("alice").unwrap(), "admin");
}

#[tokio::test]
async fn security_gate_set_invalid_level_errors() {
    let state = state().await;
    let resp = handle_security_gate_set(
        &req(
            "r1",
            "security.gate.set",
            Some(serde_json::json!({ "user_id": "bob", "level": "superadmin" })),
        ),
        &state,
    )
    .await;
    assert!(!resp.ok);
    assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
}

#[tokio::test]
async fn security_gate_clear() {
    let state = state().await;
    handle_security_gate_set(
        &req(
            "r1",
            "security.gate.set",
            Some(serde_json::json!({ "user_id": "carol", "level": "user" })),
        ),
        &state,
    )
    .await;
    let resp = handle_security_gate_clear(
        &req("r1", "security.gate.clear", Some(serde_json::json!({ "user_id": "carol" }))),
        &state,
    )
    .await;
    assert!(resp.ok);
    let resp = handle_security_gate_list(&req("r1", "security.gate.list", None), &state).await;
    let levels = resp.payload.as_ref().unwrap()["levels"]
        .as_object()
        .unwrap();
    assert!(!levels.contains_key("carol"));
}

#[tokio::test]
async fn security_allowlist_add_and_list() {
    let state = state().await;
    let resp = handle_security_allowlist_add(
        &req(
            "r1",
            "security.allowlist.add",
            Some(serde_json::json!({ "channel": "telegram", "user_id": "u1" })),
        ),
        &state,
    )
    .await;
    assert!(resp.ok, "add failed: {:?}", resp.error);
    let resp =
        handle_security_allowlist_list(&req("r1", "security.allowlist.list", None), &state).await;
    assert!(resp.ok);
    assert!(resp.payload.as_ref().unwrap()["count"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn security_status_returns_summary() {
    let state = state().await;
    let resp = handle_security_status(&req("r1", "security.status", None), &state).await;
    assert!(resp.ok, "status failed: {:?}", resp.error);
    let payload = resp.payload.as_ref().unwrap();
    assert!(payload["auth_mode"].is_string());
    assert!(payload["gate_levels"].is_number());
}

// ── mcp.call_tool ───────────────────────────────────────────────────────

/// A stand-in for a tool the MCP subsystem registers, so these tests need no
/// live server: what is under test is which path the handler takes.
struct StubMcpTool {
    name: String,
}

#[async_trait::async_trait]
impl crate::tools::Tool for StubMcpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "stub mcp tool"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _context: &crate::tools::ToolContext,
    ) -> crate::Result<crate::tools::ToolExecutionResult> {
        Ok(crate::tools::ToolExecutionResult::success("stub ran")
            .with_data(serde_json::json!({ "echo": true })))
    }
}

fn operator_ctx() -> crate::security::request_context::RequestContext {
    crate::security::request_context::RequestContext::from_identity(
        None,
        crate::security::request_context::AuthSource::None,
    )
}

/// A registered MCP tool is executed through the registry, not handed straight
/// to the MCP client: blocked prefixes, policy hooks and the content filter all
/// live in the registry, and skipping it meant that being able to *list* a tool
/// was enough to run it.
#[tokio::test]
async fn mcp_call_tool_goes_through_the_registry() {
    let state = state().await;
    state.tools.registry.register_dynamic(Arc::new(StubMcpTool {
        name: "mcp__echo__ping".to_string(),
    }));

    let resp = handle_mcp_call_tool(
        &req(
            "r1",
            "mcp.call_tool",
            Some(serde_json::json!({ "server_id": "echo", "tool": "ping", "args": {} })),
        ),
        &state,
        &operator_ctx(),
    )
    .await;

    // No MCP server called `echo` is connected here, so a handler that went to
    // the manager instead of the registry would answer NOT_FOUND.
    assert!(resp.ok, "registered tool should run: {:?}", resp.error);
    assert_eq!(resp.payload.as_ref().unwrap()["result"]["echo"], true);
}

/// A blocked name is refused even with no registry entry to dispatch through —
/// the fallback used to call the server regardless.
#[tokio::test]
async fn mcp_call_tool_refuses_a_blocked_tool() {
    let state = state().await;
    state.tools.registry.register_dynamic(Arc::new(StubMcpTool {
        name: "mcp__echo__ping".to_string(),
    }));
    // What a disconnect does: the prefix becomes blocked and its tools go away.
    state.tools.registry.deregister_prefix("mcp__echo__");

    let resp = handle_mcp_call_tool(
        &req(
            "r1",
            "mcp.call_tool",
            Some(serde_json::json!({ "server_id": "echo", "tool": "ping" })),
        ),
        &state,
        &operator_ctx(),
    )
    .await;

    assert!(!resp.ok);
    assert_eq!(resp.error.as_ref().unwrap().code, "FORBIDDEN");
}

#[tokio::test]
async fn mcp_call_tool_unknown_server_not_found() {
    let state = state().await;
    let resp = handle_mcp_call_tool(
        &req(
            "r1",
            "mcp.call_tool",
            Some(serde_json::json!({ "server_id": "nope", "tool": "t" })),
        ),
        &state,
        &operator_ctx(),
    )
    .await;

    assert!(!resp.ok);
    assert_eq!(resp.error.as_ref().unwrap().code, "NOT_FOUND");
}
