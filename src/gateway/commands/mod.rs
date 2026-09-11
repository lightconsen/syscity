//! Slash Command System for Syscity Gateway
//!
//! Provides `/` commands via WebSocket RPC.
//! Commands are exposed via `commands.list` and executed via
//! `commands.execute`.

mod admin;
mod agents;
mod model;
mod session;
mod tools;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::acp::{AcpSessionId, SpawnMode, SubagentConfig, ThreadBinding};
use crate::agent::TranscriptFormat;
use crate::gateway::command_provider::{CommandProviderHint, CommandProviderResolver};
use crate::gateway::protocol::*;
use crate::gateway::GatewayState;
use crate::mcp::{McpServerConfig, McpToolWrapper};
use crate::security::request_context::RequestContext;
use crate::tools::approval::{ApprovalDecision, ApprovalFilter};
use crate::tools::command_gate::UserLevel;

// ── Command Definitions
// ───────────────────────────────────────────────────────

/// Command category for grouping
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandCategory {
    Session,
    Model,
    Status,
    Agents,
    Tools,
    Admin,
}

/// Command tier for progressive disclosure
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandTier {
    Essential,
    Standard,
    Power,
}

/// Where a command is valid
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandScope {
    /// Works everywhere (default)
    #[default]
    Global,
    /// DM only
    DirectMessage,
    /// Channel only
    Channel,
}

/// Metadata for a single slash command
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDef {
    pub key: String,
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    pub category: CommandCategory,
    pub tier: CommandTier,
    pub local: bool,
    pub requires_admin: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub scope: CommandScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_hint: Option<CommandProviderHint>,
}

impl CommandDef {
    fn new(key: &str, name: &str, description: &str, category: CommandCategory) -> Self {
        Self {
            key: key.to_string(),
            name: name.to_string(),
            description: description.to_string(),
            args: None,
            category,
            tier: CommandTier::Standard,
            local: false,
            requires_admin: false,
            aliases: Vec::new(),
            scope: CommandScope::Global,
            provider_hint: None,
        }
    }

    fn with_args(mut self, args: &str) -> Self {
        self.args = Some(args.to_string());
        self
    }

    fn with_aliases(mut self, aliases: &[&str]) -> Self {
        self.aliases = aliases.iter().map(|a| a.to_string()).collect();
        self
    }

    fn local(mut self) -> Self {
        self.local = true;
        self
    }

    fn admin(mut self) -> Self {
        self.requires_admin = true;
        self
    }

    fn essential(mut self) -> Self {
        self.tier = CommandTier::Essential;
        self
    }

    fn power(mut self) -> Self {
        self.tier = CommandTier::Power;
        self
    }
}

/// Built-in command catalog
pub fn built_in_commands() -> Vec<CommandDef> {
    vec![
        // Session
        CommandDef::new("new", "new", "Start a new session", CommandCategory::Session)
            .with_args("[model]")
            .with_aliases(&["clear"])
            .local()
            .essential(),
        CommandDef::new("reset", "reset", "Reset the current session", CommandCategory::Session)
            .with_args("[soft|hard]")
            .essential(),
        CommandDef::new("stop", "stop", "Abort the current run", CommandCategory::Session)
            .essential(),
        CommandDef::new(
            "compact",
            "compact",
            "Compact the session context",
            CommandCategory::Session,
        )
        .with_args("[instructions]"),
        CommandDef::new(
            "export-session",
            "export-session",
            "Export session to HTML",
            CommandCategory::Session,
        )
        .with_args("[path]"),
        // Model
        CommandDef::new(
            "model",
            "model",
            "Show or switch the active model",
            CommandCategory::Model,
        )
        .with_args("[name|#|status]"),
        CommandDef::new("think", "think", "Set thinking level", CommandCategory::Model)
            .with_args("<level>"),
        CommandDef::new("verbose", "verbose", "Toggle verbose output", CommandCategory::Model)
            .with_args("on|off|full"),
        CommandDef::new("fast", "fast", "Show or set fast mode", CommandCategory::Model)
            .with_args("[on|off|status]"),
        CommandDef::new("trace", "trace", "Toggle plugin trace", CommandCategory::Model)
            .with_args("on|off"),
        CommandDef::new(
            "reasoning",
            "reasoning",
            "Set reasoning visibility",
            CommandCategory::Model,
        )
        .with_args("[on|off|stream]"),
        CommandDef::new("queue", "queue", "Set queue behavior", CommandCategory::Model)
            .with_args("<mode>"),
        // Status / Query
        CommandDef::new("help", "help", "Show help summary", CommandCategory::Status)
            .with_aliases(&["commands"])
            .essential(),
        CommandDef::new("status", "status", "Show runtime status", CommandCategory::Status)
            .essential(),
        CommandDef::new("tools", "tools", "Show available tools", CommandCategory::Status)
            .with_args("[compact|verbose]"),
        CommandDef::new("whoami", "whoami", "Show your sender ID", CommandCategory::Status)
            .essential(),
        CommandDef::new("usage", "usage", "Show usage statistics", CommandCategory::Status)
            .with_args("[off|tokens|full|cost]"),
        CommandDef::new(
            "context",
            "context",
            "Show context assembly info",
            CommandCategory::Status,
        )
        .with_args("[list|detail|json]"),
        // Agents / ACP
        CommandDef::new("subagents", "subagents", "Manage sub-agents", CommandCategory::Agents)
            .with_args("list|kill|log|info|send|steer|spawn"),
        CommandDef::new("acp", "acp", "Manage ACP sessions", CommandCategory::Agents)
            .with_args("spawn|cancel|steer|close|sessions|status|..."),
        CommandDef::new("skill", "skill", "List or show skill details", CommandCategory::Agents)
            .with_args("[name]"),
        CommandDef::new("session", "session", "Manage session timeouts", CommandCategory::Session)
            .with_args("idle|max-age <duration|off>"),
        CommandDef::new("kill", "kill", "Abort sub-agent runs", CommandCategory::Agents)
            .with_args("<id|#|all>"),
        CommandDef::new("steer", "steer", "Send steering to a sub-agent", CommandCategory::Agents)
            .with_args("<id> <message>")
            .with_aliases(&["tell"]),
        CommandDef::new("focus", "focus", "Bind thread to session target", CommandCategory::Agents)
            .with_args("<target>"),
        CommandDef::new("unfocus", "unfocus", "Remove thread binding", CommandCategory::Agents),
        // Skills / Approval
        CommandDef::new(
            "allowlist",
            "allowlist",
            "Manage command allowlist",
            CommandCategory::Agents,
        )
        .with_args("[list|add|remove] ..."),
        CommandDef::new(
            "approve",
            "approve",
            "Resolve an approval prompt",
            CommandCategory::Agents,
        )
        .with_args("<id> <decision>")
        .with_aliases(&["y"]),
        CommandDef::new(
            "btw",
            "btw",
            "Side question without changing context",
            CommandCategory::Agents,
        )
        .with_args("<question>"),
        // Admin (owner-only)
        CommandDef::new("config", "config", "Read or write config", CommandCategory::Admin)
            .with_args("show|get|set|unset")
            .admin()
            .power(),
        CommandDef::new("plugins", "plugins", "Inspect or toggle plugins", CommandCategory::Admin)
            .with_args("list|install|enable|disable")
            .admin()
            .power(),
        CommandDef::new("mcp", "mcp", "Manage MCP server connections", CommandCategory::Admin)
            .with_args("show|connect|disconnect|tools|resources|prompts|call|read")
            .admin()
            .power(),
        CommandDef::new("debug", "debug", "Runtime debug overrides", CommandCategory::Admin)
            .with_args("show|set|unset|reset")
            .admin()
            .power(),
        CommandDef::new("restart", "restart", "Restart the gateway", CommandCategory::Admin)
            .admin()
            .power(),
        CommandDef::new("bash", "bash", "Run a host shell command", CommandCategory::Admin)
            .with_args("<command>")
            .admin()
            .power(),
        CommandDef::new(
            "goal",
            "goal",
            "Execute a goal with auto-check conditions",
            CommandCategory::Agents,
        )
        .with_args("<description> [--max-rounds N]"),
    ]
}

/// Parsed arguments for the `/help` command
#[derive(Debug, Default)]
struct HelpArgs {
    page: usize,
    tier: Option<CommandTier>,
}

impl HelpArgs {
    fn parse(args: &str) -> Self {
        let mut page = 1usize;
        let mut tier = None;

        // Handle `--tier <value>` (space-separated) and `--tier=<value>` patterns
        let tokens: Vec<&str> = args.split_whitespace().collect();
        let mut i = 0;
        while i < tokens.len() {
            let t = tokens[i];
            if let Some(val) = t.strip_prefix("--tier=") {
                tier = Self::parse_tier(val);
            } else if t == "--tier" {
                if let Some(val) = tokens.get(i + 1) {
                    tier = Self::parse_tier(val);
                    i += 1; // skip next token
                }
            } else if let Ok(n) = t.parse::<usize>() {
                page = n;
            }
            i += 1;
        }
        if page == 0 {
            page = 1;
        }
        Self { page, tier }
    }

    fn parse_tier(s: &str) -> Option<CommandTier> {
        match s.to_lowercase().as_str() {
            "essential" => Some(CommandTier::Essential),
            "standard" => Some(CommandTier::Standard),
            "power" => Some(CommandTier::Power),
            _ => None,
        }
    }
}

/// Structured help response payload
#[derive(Debug, Serialize)]
struct HelpPayload {
    text: String,
    page: usize,
    total_pages: usize,
    total_commands: usize,
    tier: Option<CommandTier>,
}

// ── handlers
// ──────────────────────────────────────────────────────────────────

/// Handle `commands.list` — return the built-in command catalog
pub fn handle_commands_list() -> serde_json::Value {
    let commands = built_in_commands();
    serde_json::json!({
        "commands": commands,
    })
}

/// Parameters for `commands.execute`
#[derive(Debug, Deserialize)]
struct ExecuteParams {
    command: String,
    #[serde(default)]
    args: String,
    #[serde(default)]
    session_id: Option<String>,
}

/// Handle `commands.execute` — parse command and dispatch
pub async fn handle_commands_execute(
    req: &WsRequest,
    conn: &Arc<RwLock<ProtocolConnection>>,
    state: &Arc<GatewayState>,
    ctx: &RequestContext,
) -> WsResponse {
    let params: ExecuteParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let normalized = params
        .command
        .to_lowercase()
        .trim_start_matches('/')
        .to_string();

    debug!("Executing command: /{} args='{}'", normalized, params.args);

    // Determine session_id for persistence
    let mut session_id = params.session_id.clone();
    if session_id.is_none() {
        session_id = conn.read().await.subscriptions.first().cloned();
    }

    // Persist user command input
    let user_text = if params.args.is_empty() {
        format!("/{}", normalized)
    } else {
        format!("/{}", params.command)
    };
    if let Some(ref sid) = session_id {
        if let Some(ref store) = state.agents.store {
            tracing::info!("Persisting command /{} to session {}", normalized, sid);
            if let Err(e) = store
                .append_message(&crate::agent::session_store::AppendMessageParams {
                    session_id: sid,
                    role: "user",
                    content: &user_text,
                    ..Default::default()
                })
                .await
            {
                tracing::warn!("Failed to save command input to session history: {}", e);
            }
        } else {
            tracing::warn!("No session_store available, cannot persist command /{}", normalized);
        }
    } else {
        tracing::warn!("No session_id for command /{}, cannot persist", normalized);
    }

    // Execute command and capture response
    let response = async {
        // Find the command definition
        let commands = built_in_commands();
        let def = match commands.iter().find(|c| {
            c.key == normalized || c.name == normalized || c.aliases.contains(&normalized)
        }) {
            Some(d) => d.clone(),
            None => {
                return WsResponse::err(
                    &req.id,
                    "COMMAND_NOT_FOUND",
                    format!("Unknown command: /{}", normalized),
                );
            }
        };

        // Check admin requirement
        if def.requires_admin {
            let conn_guard = conn.read().await;
            let scopes = &conn_guard.scopes;
            if !scopes_allow(scopes, "commands.execute.admin") {
                return error_forbidden(&req.id, SCOPE_ADMIN);
            }
        }

        // Local commands are only available in channel message contexts, not
        // via the WebSocket RPC path.
        if def.local {
            return WsResponse::err(
                &req.id,
                "LOCAL_COMMAND_NOT_AVAILABLE",
                format!("Command /{} is only available in channel conversations", def.key),
            );
        }

        // WebSocket RPC connections are treated as direct messages.
        let is_direct = true;
        match def.scope {
            CommandScope::Global => {}
            CommandScope::DirectMessage if is_direct => {}
            CommandScope::Channel => {
                return WsResponse::err(
                    &req.id,
                    "WRONG_SCOPE",
                    format!("Command /{} is only available in channels", def.key),
                );
            }
            _ => {
                return WsResponse::err(
                    &req.id,
                    "WRONG_SCOPE",
                    format!("Command /{} cannot be used here", def.key),
                );
            }
        }

        // Tier check against the user's configured level. The identity comes
        // from the request context so this authorization decision cannot drift
        // from the one the audit log records.
        let user_level = state.auth.command_gate.user_level(ctx.user_id());
        let required_level = match def.tier {
            CommandTier::Essential | CommandTier::Standard => UserLevel::User,
            CommandTier::Power => UserLevel::Admin,
        };
        if user_level < required_level {
            return WsResponse::err(
                &req.id,
                "INSUFFICIENT_TIER",
                format!(
                    "Command /{} requires {:?} access; you have {:?}",
                    def.key, required_level, user_level
                ),
            );
        }

        // Log any provider/model hint inferred for this command.
        if let Some(hint) = CommandProviderResolver::resolve(&def, user_level, None) {
            debug!(
                command = %def.key,
                provider = ?hint.provider,
                model = ?hint.model,
                reason = %hint.reason,
                "Provider hint resolved"
            );
        }

        // Dispatch by canonical key so aliases resolve to the same handler
        match def.key.as_str() {
            "help" | "commands" => session::handle_help(req, &params.args),
            "status" => session::handle_status(req, state).await,
            "whoami" => session::handle_whoami(req, conn, ctx).await,
            "stop" => session::handle_stop(req, conn, state).await,
            "reset" => session::handle_reset(req, conn, state).await,
            "model" => model::handle_model(req, conn, state, &params.args).await,
            "think" => model::handle_think(req, state, &params.args).await,
            "verbose" => model::handle_verbose(req, state, &params.args).await,
            "trace" => model::handle_trace(req, state, &params.args).await,
            "fast" => model::handle_fast(req, state, &params.args).await,
            "reasoning" => model::handle_reasoning(req, state, &params.args).await,
            "queue" => model::handle_queue(req, state, &params.args).await,
            "tools" => tools::handle_tools(req, state, &params.args).await,
            "usage" => session::handle_usage(req, state, &params.args).await,
            "context" => session::handle_context(req, conn, state).await,
            "compact" => session::handle_compact(req, conn, state, &params.args).await,
            "session" => session::handle_session(req, conn, state, &params.args).await,
            "export-session" => {
                session::handle_export_session(req, conn, state, &params.args).await
            }
            "subagents" => agents::handle_subagents(req, conn, state, ctx, &params.args).await,
            "acp" => agents::handle_acp(req, conn, state, &params.args).await,
            "steer" | "tell" => agents::handle_steer(req, conn, state, ctx, &params.args).await,
            "kill" => agents::handle_kill(req, conn, state, &params.args).await,
            "focus" => agents::handle_focus(req, conn, state, &params.args).await,
            "unfocus" => agents::handle_unfocus(req, conn, state).await,
            "skill" => tools::handle_skill(req, state, &params.args).await,
            "allowlist" => tools::handle_allowlist(req, state, &params.args).await,
            "approve" => tools::handle_approve(req, state, &params.args).await,
            "btw" => tools::handle_btw(req, state, &params.args).await,
            "config" => admin::handle_config(req, state, &params.args).await,
            "plugins" => admin::handle_plugins(req, state, &params.args).await,
            "mcp" => admin::handle_mcp(req, state, &params.args).await,
            "debug" => admin::handle_debug(req, state, &params.args).await,
            "restart" => admin::handle_restart(req, state).await,
            "bash" => tools::handle_bash(req, &params.args).await,
            "goal" => agents::handle_goal(req, conn, state, &params.args).await,
            _ => WsResponse::err(
                &req.id,
                "NOT_HANDLED",
                format!("Command /{} is not handled server-side", def.key),
            ),
        }
    }
    .await;

    // Persist command result
    let result_text = if response.ok {
        response
            .payload
            .as_ref()
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        response
            .error
            .as_ref()
            .map(|e| format!("Command error: {}", e.message))
            .unwrap_or_else(|| "Command error".to_string())
    };

    if let Some(ref sid) = session_id {
        if let Some(ref store) = state.agents.store {
            if let Err(e) = store
                .append_message(&crate::agent::session_store::AppendMessageParams {
                    session_id: sid,
                    role: "assistant",
                    content: &result_text,
                    ..Default::default()
                })
                .await
            {
                tracing::warn!("Failed to save command result to session history: {}", e);
            }
        }
    }

    response
}

// ── Helper ────────────────────────────────────────────────────────────────────
#[allow(clippy::result_large_err)]
fn parse_params<T: serde::de::DeserializeOwned>(req: &WsRequest) -> Result<T, WsResponse> {
    match &req.params {
        Some(p) => match serde_json::from_value::<T>(p.clone()) {
            Ok(v) => Ok(v),
            Err(e) => Err(WsResponse::err(
                &req.id,
                "INVALID_PARAMS",
                format!("Invalid parameters: {}", e),
            )),
        },
        None => Err(WsResponse::err(&req.id, "INVALID_PARAMS", "Missing parameters")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_help_args_default() {
        let args = HelpArgs::parse("");
        assert_eq!(args.page, 1);
        assert!(args.tier.is_none());
    }

    #[test]
    fn test_help_args_page_number() {
        let args = HelpArgs::parse("3");
        assert_eq!(args.page, 3);
    }

    #[test]
    fn test_help_args_page_zero_clamped() {
        let args = HelpArgs::parse("0");
        assert_eq!(args.page, 1);
    }

    #[test]
    fn test_help_args_tier_flag() {
        let args = HelpArgs::parse("--tier essential");
        assert_eq!(args.page, 1);
        assert_eq!(args.tier, Some(CommandTier::Essential));
    }

    #[test]
    fn test_help_args_tier_equals() {
        let args = HelpArgs::parse("--tier=power");
        assert_eq!(args.page, 1);
        assert_eq!(args.tier, Some(CommandTier::Power));
    }

    #[test]
    fn test_help_args_page_and_tier() {
        let args = HelpArgs::parse("2 --tier standard");
        assert_eq!(args.page, 2);
        assert_eq!(args.tier, Some(CommandTier::Standard));
    }

    #[test]
    fn test_help_args_invalid_tier_ignored() {
        let args = HelpArgs::parse("--tier invalid");
        assert!(args.tier.is_none());
        assert_eq!(args.page, 1);
    }

    #[test]
    fn test_help_pagination_page_count() {
        let cmds = built_in_commands();
        let total = cmds.len();
        let expected_pages = total.div_ceil(8).max(1);
        let page1_slice: Vec<&CommandDef> = cmds.iter().take(8).collect();
        assert_eq!(page1_slice.len(), 8.min(total));
        assert!(expected_pages >= 1);
    }

    #[test]
    fn test_tier_filter_essential() {
        let cmds = built_in_commands();
        let essential: Vec<&CommandDef> = cmds
            .iter()
            .filter(|c| c.tier == CommandTier::Essential)
            .collect();
        assert!(!essential.is_empty());
        assert!(essential.iter().any(|c| c.key == "help"));
        assert!(essential.iter().any(|c| c.key == "new"));
    }

    #[test]
    fn test_tier_filter_power() {
        let cmds = built_in_commands();
        let power: Vec<&CommandDef> = cmds
            .iter()
            .filter(|c| c.tier == CommandTier::Power)
            .collect();
        assert!(!power.is_empty());
        assert!(power.iter().any(|c| c.key == "config"));
        assert!(power.iter().any(|c| c.key == "bash"));
    }
}
