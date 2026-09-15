//! Tool, skill and approval command handlers.

use super::*;

pub(super) async fn handle_tools(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let verbose = args.trim() == "verbose";
    let tool_names = state.tools.registry.list();

    let text = if verbose {
        let mut lines = vec!["🛠️ **Available Tools**".to_string()];
        for name in &tool_names {
            lines.push(format!("- {}", name));
        }
        lines.join("\n")
    } else {
        format!("🛠️ **Tools** ({} total): {}", tool_names.len(), tool_names.join(", "))
    };

    WsResponse::ok(&req.id, serde_json::json!({ "text": text }))
}

pub(super) async fn handle_bash(req: &WsRequest, args: &str) -> WsResponse {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return WsResponse::err(&req.id, "INVALID_ARGS", "Usage: /bash <command>");
    }

    match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(trimmed)
        .output()
        .await
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut lines = vec![format!("💻 `$ {}`", trimmed)];
            if !stdout.is_empty() {
                lines.push("\nstdout:".to_string());
                lines.push(stdout.to_string());
            }
            if !stderr.is_empty() {
                lines.push("\nstderr:".to_string());
                lines.push(stderr.to_string());
            }
            lines.push(format!("\nexit code: {}", output.status.code().unwrap_or(-1)));
            WsResponse::ok(&req.id, serde_json::json!({ "text": lines.join("\n") }))
        }
        Err(e) => WsResponse::err(&req.id, "EXEC_FAILED", format!("Failed to execute: {}", e)),
    }
}

pub(super) async fn handle_skill(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        let mgr = state.tools.skills_manager.read().await;
        let skills = mgr.prefilter_skills("", 50, 0).await;
        let names: Vec<String> = skills.into_iter().map(|s| s.name).collect();
        let text = format!("🎯 **Skills** ({} total): {}", names.len(), names.join(", "));
        return WsResponse::ok(&req.id, serde_json::json!({ "text": text }));
    }

    let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
    let name = parts[0];
    let _input = parts.get(1).unwrap_or(&"");

    let mgr = state.tools.skills_manager.read().await;
    match mgr.get_skill(name).await {
        Some(skill) => {
            let text = format!(
                "🎯 **Skill: {}**\n\nVersion: {}\nDescription: {}\nEnabled: {}\nEligible: {}",
                skill.name, skill.version, skill.description, skill.enabled, skill.is_eligible,
            );
            WsResponse::ok(&req.id, serde_json::json!({ "text": text }))
        }
        None => WsResponse::err(&req.id, "SKILL_NOT_FOUND", format!("Skill '{}' not found.", name)),
    }
}

pub(super) async fn handle_allowlist(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let trimmed = args.trim();
    if trimmed.is_empty() || trimmed == "list" {
        let levels = state.auth.command_gate.user_levels();
        if levels.is_empty() {
            return WsResponse::ok(
                &req.id,
                serde_json::json!({ "text": "🛡️ No custom user levels configured." }),
            );
        }
        let mut lines = vec!["🛡️ **User Levels**".to_string()];
        for (user, level) in levels {
            lines.push(format!("- {}: {}", user, level));
        }
        return WsResponse::ok(&req.id, serde_json::json!({ "text": lines.join("\n") }));
    }

    let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
    let sub = parts[0];

    match sub {
        "add" => {
            let user = parts.get(1).unwrap_or(&"").trim();
            let level_str = parts.get(2).unwrap_or(&"user").trim();
            if user.is_empty() {
                return WsResponse::err(
                    &req.id,
                    "INVALID_ARGS",
                    "Usage: /allowlist add <user> [chat|user|admin]",
                );
            }
            let level = match level_str {
                "chat" => UserLevel::Chat,
                "admin" => UserLevel::Admin,
                _ => UserLevel::User,
            };
            state.auth.command_gate.set_user_level(user, level);
            WsResponse::ok(
                &req.id,
                serde_json::json!({ "text": format!("🛡️ Set {} to level '{}'.", user, level) }),
            )
        }
        "remove" => {
            let user = parts.get(1).unwrap_or(&"").trim();
            if user.is_empty() {
                return WsResponse::err(&req.id, "INVALID_ARGS", "Usage: /allowlist remove <user>");
            }
            state.auth.command_gate.clear_user_level(user);
            WsResponse::ok(
                &req.id,
                serde_json::json!({ "text": format!("🛡️ Cleared level for '{}'.", user) }),
            )
        }
        _ => WsResponse::err(&req.id, "INVALID_ARGS", "Usage: /allowlist [list|add|remove]"),
    }
}

pub(super) async fn handle_approve(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let trimmed = args.trim();
    if trimmed.is_empty() || trimmed == "list" {
        let pending = state
            .tools
            .approval_queue
            .list_pending(ApprovalFilter::default())
            .await;
        if pending.is_empty() {
            return WsResponse::ok(
                &req.id,
                serde_json::json!({ "text": "✅ No pending approvals." }),
            );
        }
        let mut lines = vec![format!("⏳ **Pending Approvals** ({})", pending.len())];
        for pa in &pending {
            lines.push(format!(
                "- {}: {} (risk: {:?}, by: {})",
                pa.id, pa.tool_name, pa.risk_level, pa.requested_by
            ));
        }
        return WsResponse::ok(&req.id, serde_json::json!({ "text": lines.join("\n") }));
    }

    let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
    let id = parts[0];
    let decision_str = parts.get(1).unwrap_or(&"").trim();

    if id.is_empty() || decision_str.is_empty() {
        return WsResponse::err(
            &req.id,
            "INVALID_ARGS",
            "Usage: /approve <id> approve|deny [reason]",
        );
    }

    let decision = match decision_str {
        "approve" | "yes" | "y" => ApprovalDecision::Approve,
        "deny" | "no" | "n" => ApprovalDecision::Deny {
            reason: "Denied via /approve command.".to_string(),
        },
        _ => {
            return WsResponse::err(
                &req.id,
                "INVALID_ARGS",
                "Decision must be 'approve' or 'deny'.",
            );
        }
    };

    if state.tools.approval_queue.resolve(id, decision).await {
        WsResponse::ok(
            &req.id,
            serde_json::json!({ "text": format!("✅ Approval '{}' resolved.", id) }),
        )
    } else {
        WsResponse::err(
            &req.id,
            "NOT_FOUND",
            format!("Approval '{}' not found or already resolved.", id),
        )
    }
}

pub(super) async fn handle_btw(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let question = args.trim();
    if question.is_empty() {
        return WsResponse::err(&req.id, "INVALID_ARGS", "Usage: /btw <question>");
    }

    let messages = vec![crate::providers::Message::user(question)];
    match state.infra.model_router.complete_auto(messages, None).await {
        Ok(response) => {
            let text = format!(
                "💡 **Side question** ({}):\n\n{}",
                response.model, response.message.content
            );
            WsResponse::ok(&req.id, serde_json::json!({ "text": text }))
        }
        Err(e) => {
            WsResponse::err(&req.id, "COMPLETION_FAILED", format!("Failed to get response: {}", e))
        }
    }
}

/// `/learn` — author a skill from whatever the user points at.
///
/// Builds a prompt and runs it as the next user turn, through the same path a
/// composed message takes (`chat.send`): same session, same history, same event
/// stream. The command itself holds no logic beyond that — a second way into the
/// agent would be a second thing to keep in step, and this one gets every
/// client's streaming, persistence and cancellation for free.
pub(super) async fn handle_learn(
    req: &WsRequest,
    conn: &Arc<tokio::sync::RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
    args: &str,
    session_id: Option<&str>,
) -> WsResponse {
    let mut send = req.clone();
    send.method = "chat.send".to_string();
    let mut params = serde_json::json!({
        "message": crate::skills::learn::learn_prompt(args, &state.paths.skills_dir()),
    });
    // Stay in the session the command was run in. `chat.send` would otherwise
    // resolve its own, which for a client that passes an explicit session id
    // would quietly be a different conversation.
    if let Some(sid) = session_id {
        params["session_id"] = serde_json::json!(sid);
    }
    send.params = Some(params);

    crate::gateway::ws::chat::handle_chat_send(&send, conn, state, ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;

    fn req(id: &str) -> WsRequest {
        WsRequest {
            frame_type: "req".into(),
            id: id.into(),
            method: "x".into(),
            params: None,
        }
    }

    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    #[tokio::test]
    async fn tools_empty_registry_ok() {
        let state = state().await;
        let resp = handle_tools(&req("r1"), &state, "").await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("0 total"), "empty registry should show 0 total");
    }

    #[tokio::test]
    async fn tools_verbose_lists_heading() {
        let state = state().await;
        let resp = handle_tools(&req("r1"), &state, "verbose").await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("Available Tools"), "verbose should list heading");
    }

    #[tokio::test]
    async fn bash_empty_args_errors() {
        let resp = handle_bash(&req("r1"), "   ").await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_ARGS");
    }

    #[tokio::test]
    async fn bash_runs_echo_command() {
        let resp = handle_bash(&req("r1"), "echo hello-world").await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("hello-world"), "stdout should be echoed: {}", text);
        assert!(text.contains("exit code: 0"), "should report exit code 0");
    }

    #[tokio::test]
    async fn skill_empty_lists_total() {
        let state = state().await;
        let resp = handle_skill(&req("r1"), &state, "").await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("total"), "should list skills: {}", text);
    }

    #[tokio::test]
    async fn skill_unknown_returns_not_found() {
        let state = state().await;
        let resp = handle_skill(&req("r1"), &state, "ghost-skill").await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "SKILL_NOT_FOUND");
    }

    #[tokio::test]
    async fn allowlist_empty_reports_no_levels() {
        let state = state().await;
        let resp = handle_allowlist(&req("r1"), &state, "").await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("No custom user levels"), "fresh gate has no levels");
    }

    #[tokio::test]
    async fn allowlist_add_remove_roundtrip() {
        let state = state().await;
        let resp = handle_allowlist(&req("r1"), &state, "add bob admin").await;
        assert!(resp.ok);
        assert!(
            resp.payload.as_ref().unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("bob"),
            "add should report the user"
        );

        // Listing now shows bob.
        let resp = handle_allowlist(&req("r2"), &state, "list").await;
        assert!(resp.ok);
        assert!(
            resp.payload.as_ref().unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("bob"),
            "list should include bob"
        );

        let resp = handle_allowlist(&req("r3"), &state, "remove bob").await;
        assert!(resp.ok);
        assert!(
            resp.payload.as_ref().unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("bob"),
            "remove should report the user"
        );

        // Listing is empty again.
        let resp = handle_allowlist(&req("r4"), &state, "").await;
        assert!(resp.ok);
        assert!(
            resp.payload.as_ref().unwrap()["text"]
                .as_str()
                .unwrap()
                .contains("No custom user levels"),
            "after remove the gate should be empty"
        );
    }

    #[tokio::test]
    async fn allowlist_add_empty_user_errors() {
        let state = state().await;
        let resp = handle_allowlist(&req("r1"), &state, "add").await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_ARGS");
    }

    #[tokio::test]
    async fn allowlist_invalid_subcommand_errors() {
        let state = state().await;
        let resp = handle_allowlist(&req("r1"), &state, "bogus").await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_ARGS");
    }

    #[tokio::test]
    async fn approve_empty_reports_no_pending() {
        let state = state().await;
        let resp = handle_approve(&req("r1"), &state, "").await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("No pending approvals"));
    }

    #[tokio::test]
    async fn approve_missing_decision_errors() {
        let state = state().await;
        let resp = handle_approve(&req("r1"), &state, "abc").await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_ARGS");
    }

    #[tokio::test]
    async fn approve_unknown_id_not_found() {
        let state = state().await;
        let resp = handle_approve(&req("r1"), &state, "abc approve").await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn btw_empty_args_errors() {
        let state = state().await;
        let resp = handle_btw(&req("r1"), &state, "").await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_ARGS");
    }
}
