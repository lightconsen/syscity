//! Session and status command handlers.

use super::*;

pub(super) fn handle_help(req: &WsRequest, args: &str) -> WsResponse {
    let help_args = HelpArgs::parse(args);
    let all_commands = built_in_commands();

    // Apply tier filter
    let filtered: Vec<&CommandDef> = if let Some(tier) = help_args.tier {
        all_commands.iter().filter(|c| c.tier == tier).collect()
    } else {
        all_commands.iter().collect()
    };

    let total_commands = filtered.len();
    let page_size = 8usize;
    let total_pages = total_commands.div_ceil(page_size).max(1);
    let page = help_args.page.clamp(1, total_pages);
    let start = (page - 1) * page_size;
    let _end = start + page_size.min(total_commands.saturating_sub(start));
    let page_commands: Vec<&CommandDef> =
        filtered.into_iter().skip(start).take(page_size).collect();

    let mut lines = vec!["📋 **Syscity Commands**".to_string(), "".to_string()];

    let categories = [
        (CommandCategory::Session, "🗂️ Session"),
        (CommandCategory::Model, "🧠 Model"),
        (CommandCategory::Status, "ℹ️ Status"),
        (CommandCategory::Agents, "🤖 Agents"),
        (CommandCategory::Tools, "🛠️ Tools"),
        (CommandCategory::Admin, "🔒 Admin"),
    ];

    for (cat, title) in &categories {
        let cat_cmds: Vec<&&CommandDef> = page_commands
            .iter()
            .filter(|c| c.category == *cat)
            .collect();
        if cat_cmds.is_empty() {
            continue;
        }
        lines.push(format!("### {}", title));
        for c in cat_cmds {
            let admin_mark = if c.requires_admin { " `[admin]`" } else { "" };
            let args = c.args.as_deref().unwrap_or("");
            let args_display = if args.is_empty() {
                "".to_string()
            } else {
                format!(" `{}`", args)
            };
            let alias_display = if c.aliases.is_empty() {
                "".to_string()
            } else {
                let aliases: Vec<String> = c.aliases.iter().map(|a| format!("/{}", a)).collect();
                format!(" (alias: {})", aliases.join(", "))
            };
            lines.push(format!(
                "- `/{}{}`{}{}{}",
                c.name, args_display, alias_display, c.description, admin_mark
            ));
        }
        lines.push("".to_string());
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!(HelpPayload {
            text: lines.join("\n"),
            page,
            total_pages,
            total_commands,
            tier: help_args.tier,
        }),
    )
}

pub(super) async fn handle_status(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let agents = state.agents.agents.read().await.len();
    let sessions = state.agents.manager.read().await.len();

    let text = format!(
        "📊 **Status**\n\nActive agents: {}\nActive sessions: {}\nStatus: healthy",
        agents, sessions
    );

    WsResponse::ok(&req.id, serde_json::json!({ "text": text }))
}

pub(super) async fn handle_whoami(
    req: &WsRequest,
    conn: &Arc<RwLock<ProtocolConnection>>,
    ctx: &RequestContext,
) -> WsResponse {
    let guard = conn.read().await;
    let user = ctx.user_id();
    let scopes = &guard.scopes;

    let text = format!("👤 **Whoami**\n\nUser: `{}`\nScopes: `{}`", user, scopes.join(", "));

    WsResponse::ok(&req.id, serde_json::json!({ "text": text }))
}

pub(super) async fn handle_stop(
    req: &WsRequest,
    conn: &Arc<RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let session_id = conn.read().await.subscriptions.first().cloned();

    if let Some(sid) = session_id {
        if let Err(e) = state.agents.acp.cancel(sid.clone()).await {
            warn!("Failed to send stop signal for session {}: {}", sid, e);
        }
        return WsResponse::ok(
            &req.id,
            serde_json::json!({ "text": format!("⏹️ Stop signal sent for session `{}`.", sid) }),
        );
    }

    WsResponse::ok(&req.id, serde_json::json!({ "text": "⏹️ No active session to stop." }))
}

pub(super) async fn handle_reset(
    req: &WsRequest,
    conn: &Arc<RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let session_id = conn.read().await.subscriptions.first().cloned();

    if let Some(sid) = session_id {
        {
            let mut mgr = state.agents.manager.write().await;
            mgr.terminate_session(&sid).await;
            mgr.create_session(sid.clone());
        }
        // Clear persisted history so the session truly resets
        if let Some(ref store) = state.agents.store {
            if let Err(e) = store.delete_session(&sid).await {
                tracing::warn!("Failed to delete session {} during reset: {}", sid, e);
            }
        }
        return WsResponse::ok(
            &req.id,
            serde_json::json!({ "text": format!("🔄 Session `{}` reset.", sid) }),
        );
    }

    WsResponse::ok(&req.id, serde_json::json!({ "text": "🔄 No active session to reset." }))
}

pub(super) async fn handle_compact(
    req: &WsRequest,
    conn: &Arc<RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let session_id = conn.read().await.subscriptions.first().cloned();
    let instructions = args.trim();

    let Some(sid) = session_id else {
        return WsResponse::ok(
            &req.id,
            serde_json::json!({ "text": "🗜️ No active session to compact." }),
        );
    };

    // Resolve agent for session
    let route = state.agents.router.resolve_by_session(&sid).await;
    let agents = state.agents.agents.read().await;
    let agent_handle = match agents.get(&route.agent_id) {
        Some(h) => h.clone(),
        None => {
            return WsResponse::ok(
                &req.id,
                serde_json::json!({ "text": "🗜️ Agent not found for this session." }),
            );
        }
    };
    drop(agents);

    // Run context compaction via the Summarize strategy
    let compact_result = agent_handle.agent.compact_context(&sid).await;

    // Flush transcript to disk as a compaction step
    let export_result = state
        .infra
        .transcript_store
        .export(&sid, TranscriptFormat::Markdown)
        .await;

    let mut lines = vec![format!("🗜️ **Compacted session `{}`**", sid)];

    match compact_result {
        Some((before, after)) => {
            if after < before {
                lines.push(format!("Messages compressed: {} → {}", before, after));
            } else {
                lines.push(format!("Messages: {} (no reduction needed)", before));
            }
        }
        None => {
            lines.push("No context found to compact.".to_string());
        }
    }

    match export_result {
        Ok(path) => lines.push(format!("Transcript flushed to `{}`.", path.display())),
        Err(e) => lines.push(format!("Transcript export failed: {}", e)),
    }

    if !instructions.is_empty() {
        lines.push(format!("Instructions: {}", instructions));
    }

    WsResponse::ok(&req.id, serde_json::json!({ "text": lines.join("\n") }))
}

pub(super) async fn handle_usage(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let mode_arg = args.trim();
    let valid_modes = ["off", "tokens", "full", "cost"];

    if !mode_arg.is_empty() && valid_modes.contains(&mode_arg) {
        let mut settings = state.infra.runtime_settings.write().await;
        settings.insert("usage.mode".to_string(), serde_json::json!(mode_arg));
        return WsResponse::ok(
            &req.id,
            serde_json::json!({
                "text": format!("📊 Usage display mode set to '{}'.", mode_arg),
                "mode": mode_arg,
            }),
        );
    }

    if !mode_arg.is_empty() && !valid_modes.contains(&mode_arg) {
        return WsResponse::err(
            &req.id,
            "INVALID_ARGS",
            format!("Usage: /usage [{}]", valid_modes.join("|")),
        );
    }

    let (mode, tokens, calls) = {
        let settings = state.infra.runtime_settings.read().await;
        let mode = settings
            .get("usage.mode")
            .and_then(|v| v.as_str())
            .unwrap_or("full")
            .to_string();
        let tokens = settings
            .get("usage.tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let calls = settings
            .get("usage.calls")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        (mode, tokens, calls)
    };

    let guard = state.agents.cost_guard.as_ref();
    let daily_cents = guard.daily_spend_cents();
    let daily_dollars = daily_cents as f64 / 100.0;
    let hourly_actions = guard.hourly_action_count();
    let daily_limit = guard.daily_limit_cents;
    let hourly_limit = guard.hourly_action_limit;
    let exceeded = guard.is_exceeded();

    let text = match mode.as_str() {
        "off" => "📊 Usage tracking display is disabled.".to_string(),
        "tokens" => {
            format!("📊 **Usage (tokens)**\n\nEstimated tokens: {}\nTool calls: {}", tokens, calls)
        }
        "cost" => format!(
            "📊 **Usage (cost)**\n\nDaily spend: ${:.2} ({} cents)\nHourly actions: {}",
            daily_dollars, daily_cents, hourly_actions
        ),
        _ => {
            let mut lines = vec!["📊 **Usage**".to_string()];
            lines.push(format!("Estimated tokens: {}", tokens));
            lines.push(format!("Tool calls: {}", calls));
            lines.push(format!("Daily spend: ${:.2} ({} cents)", daily_dollars, daily_cents));
            lines.push(format!("Hourly actions: {}", hourly_actions));
            if daily_limit > 0 {
                lines.push(format!("Daily limit: ${:.2}", daily_limit as f64 / 100.0));
            }
            if hourly_limit > 0 {
                lines.push(format!("Hourly action limit: {}", hourly_limit));
            }
            if exceeded {
                lines.push("⚠️ Budget limit exceeded.".to_string());
            }
            lines.join("\n")
        }
    };

    WsResponse::ok(&req.id, serde_json::json!({ "text": text, "mode": mode }))
}

pub(super) async fn handle_context(
    req: &WsRequest,
    conn: &Arc<RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let session_id = conn.read().await.subscriptions.first().cloned();

    let Some(sid) = session_id else {
        return WsResponse::ok(
            &req.id,
            serde_json::json!({ "text": "📜 No active session to inspect." }),
        );
    };

    // Resolve agent for session
    let route = state.agents.router.resolve_by_session(&sid).await;
    let agents = state.agents.agents.read().await;
    let Some(agent_handle) = agents.get(&route.agent_id) else {
        return WsResponse::ok(
            &req.id,
            serde_json::json!({ "text": "📜 Agent not found for this session." }),
        );
    };

    match agent_handle.agent.context_info(&sid).await {
        Some((msg_count, token_count, max_tokens, sys_len, tool_iters)) => {
            let settings = state.infra.runtime_settings.read().await;
            let mut lines = vec![format!("📜 **Context for `{}`**", sid)];
            lines.push(format!("Messages: {}", msg_count));
            lines.push(format!("Tokens: {} / {}", token_count, max_tokens));
            lines.push(format!("System prompt length: {} chars", sys_len));
            lines.push(format!("Tool iterations: {}", tool_iters));
            if !settings.is_empty() {
                lines.push("\nRuntime settings:".to_string());
                for (k, v) in settings.iter() {
                    if k.starts_with("context.") || k.starts_with("session.") {
                        lines.push(format!("  {} = {}", k, v));
                    }
                }
            }
            WsResponse::ok(&req.id, serde_json::json!({ "text": lines.join("\n") }))
        }
        None => WsResponse::ok(
            &req.id,
            serde_json::json!({ "text": format!("📜 No context found for session `{}`.", sid) }),
        ),
    }
}

fn parse_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    if s == "off" {
        return Some(std::time::Duration::from_secs(u64::MAX));
    }
    let num_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if num_end == 0 {
        return None;
    }
    let num: u64 = s[..num_end].parse().ok()?;
    let unit = &s[num_end..];
    match unit {
        "s" | "sec" | "secs" => Some(std::time::Duration::from_secs(num)),
        "m" | "min" | "mins" => Some(std::time::Duration::from_secs(num * 60)),
        "h" | "hr" | "hrs" => Some(std::time::Duration::from_secs(num * 3600)),
        "d" | "day" | "days" => Some(std::time::Duration::from_secs(num * 86400)),
        _ => None,
    }
}

pub(super) async fn handle_session(
    req: &WsRequest,
    _conn: &Arc<RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return WsResponse::err(
            &req.id,
            "INVALID_ARGS",
            "Usage: /session idle|max-age <duration|off>",
        );
    }

    let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
    let sub = parts[0];
    let rest = parts.get(1).unwrap_or(&"").trim();

    match sub {
        "idle" | "max-age" => {
            if rest.is_empty() {
                let settings = state.infra.runtime_settings.read().await;
                let key = format!("session.{}", sub);
                let current = settings
                    .get(&key)
                    .and_then(|v| v.as_str())
                    .unwrap_or("default");
                return WsResponse::ok(
                    &req.id,
                    serde_json::json!({ "text": format!("⏱️ Session {}: {}", sub, current) }),
                );
            }
            match parse_duration(rest) {
                Some(duration) => {
                    {
                        let mut mgr = state.agents.manager.write().await;
                        mgr.set_timeout(duration);
                    }
                    let mut settings = state.infra.runtime_settings.write().await;
                    let key = format!("session.{}", sub);
                    settings.insert(key.clone(), serde_json::json!(rest));
                    WsResponse::ok(
                        &req.id,
                        serde_json::json!({ "text": format!("⏱️ Session {} set to '{}'.", sub, rest) }),
                    )
                }
                None => WsResponse::err(
                    &req.id,
                    "INVALID_ARGS",
                    "Invalid duration. Use: 30m, 1h, 2d, or off",
                ),
            }
        }
        _ => {
            WsResponse::err(&req.id, "INVALID_ARGS", "Usage: /session idle|max-age <duration|off>")
        }
    }
}

pub(super) async fn handle_export_session(
    req: &WsRequest,
    conn: &Arc<RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
    args: &str,
) -> WsResponse {
    let session_id = conn.read().await.subscriptions.first().cloned();
    let _path_hint = args.trim();

    if let Some(sid) = session_id {
        match state
            .infra
            .transcript_store
            .export(&sid, TranscriptFormat::Html)
            .await
        {
            Ok(path) => {
                let text =
                    format!("📄 **Session `{}` exported**\n\nHTML: `{}`", sid, path.display());
                return WsResponse::ok(&req.id, serde_json::json!({ "text": text }));
            }
            Err(e) => {
                return WsResponse::err(
                    &req.id,
                    "EXPORT_FAILED",
                    format!("Failed to export session: {}", e),
                );
            }
        }
    }

    WsResponse::ok(&req.id, serde_json::json!({ "text": "📄 No active session to export." }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::{make_test_conn, make_test_ctx, make_test_state};
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
    async fn help_returns_commands() {
        let resp = handle_help(&req("r1"), "");
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert!(
            payload["text"]
                .as_str()
                .unwrap()
                .contains("Syscity Commands"),
            "help should list the header"
        );
        assert!(
            payload["total_commands"].as_u64().unwrap_or(0) > 0,
            "help should enumerate built-in commands"
        );
    }

    #[tokio::test]
    async fn status_empty_state() {
        let state = state().await;
        let resp = handle_status(&req("r1"), &state).await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("Active agents: 0"), "empty state has no agents");
    }

    #[tokio::test]
    async fn whoami_anonymous_default() {
        let conn = make_test_conn(&[]);
        let resp = handle_whoami(&req("r1"), &conn, &make_test_ctx()).await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("anonymous"), "no user_id means anonymous");
    }

    #[tokio::test]
    async fn whoami_lists_scopes() {
        let conn = make_test_conn(&["chat", "admin"]);
        let resp = handle_whoami(&req("r1"), &conn, &make_test_ctx()).await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("chat"), "scopes should be listed");
        assert!(text.contains("admin"), "scopes should be listed");
    }

    #[tokio::test]
    async fn stop_without_subscription_reports_none() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_stop(&req("r1"), &conn, &state).await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("No active session"), "no subscription to cancel");
    }

    #[tokio::test]
    async fn reset_without_subscription_reports_none() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_reset(&req("r1"), &conn, &state).await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("No active session"), "no subscription to reset");
    }

    #[tokio::test]
    async fn compact_without_subscription_reports_none() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_compact(&req("r1"), &conn, &state, "").await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("No active session"), "no subscription to compact");
    }

    #[tokio::test]
    async fn context_without_subscription_reports_none() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_context(&req("r1"), &conn, &state).await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("No active session"), "no subscription to inspect");
    }

    #[tokio::test]
    async fn export_without_subscription_reports_none() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_export_session(&req("r1"), &conn, &state, "").await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("No active session"), "no subscription to export");
    }

    #[tokio::test]
    async fn usage_set_mode_persists() {
        let state = state().await;
        let resp = handle_usage(&req("r1"), &state, "tokens").await;
        assert!(resp.ok);
        let settings = state.infra.runtime_settings.read().await;
        assert_eq!(settings.get("usage.mode").and_then(|v| v.as_str()), Some("tokens"));
    }

    #[tokio::test]
    async fn usage_status_default_full() {
        let state = state().await;
        let resp = handle_usage(&req("r1"), &state, "").await;
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["mode"].as_str(), Some("full"));
        assert!(payload["text"].as_str().unwrap().contains("Usage"));
    }

    #[tokio::test]
    async fn usage_invalid_mode_errors() {
        let state = state().await;
        let resp = handle_usage(&req("r1"), &state, "bogus").await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_ARGS");
    }

    #[tokio::test]
    async fn session_empty_args_errors() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_session(&req("r1"), &conn, &state, "").await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_ARGS");
    }

    #[tokio::test]
    async fn session_idle_status_default() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_session(&req("r1"), &conn, &state, "idle").await;
        assert!(resp.ok);
        let text = resp.payload.as_ref().unwrap()["text"].as_str().unwrap();
        assert!(text.contains("default"), "unset idle defaults to default");
    }

    #[tokio::test]
    async fn session_idle_set_persists() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_session(&req("r1"), &conn, &state, "idle 30m").await;
        assert!(resp.ok);
        let settings = state.infra.runtime_settings.read().await;
        assert_eq!(settings.get("session.idle").and_then(|v| v.as_str()), Some("30m"));
    }

    #[tokio::test]
    async fn session_invalid_duration_errors() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_session(&req("r1"), &conn, &state, "idle 12furlongs").await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_ARGS");
    }

    #[tokio::test]
    async fn session_invalid_sub_errors() {
        let state = state().await;
        let conn = make_test_conn(&[]);
        let resp = handle_session(&req("r1"), &conn, &state, "bogus").await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_ARGS");
    }
}
