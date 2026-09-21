//! `ShellHookBridge`: turns a parsed [`ShellHooksConfig`] into syscity seams.
//!
//! The bridge owns the config plus an optional audit logger and exposes:
//!
//! - [`ShellHookBridge::tool_hooks`] — a [`ToolHooks`] bundle (one policy
//!   hook + one post-execute hook) that gates `PreToolUse` / `PostToolUse`.
//! - [`ShellHookBridge::check_user_prompt`] — the `UserPromptSubmit` gate.
//! - [`ShellHookBridge::fire_stop`] — the `Stop` fire-and-forget fan-out.
//!
//! When no hooks are configured, `tool_hooks()` returns an empty
//! [`ToolHooks`]; this is deliberate — `ToolRegistry` uses
//! `has_policy_hooks()` to decide whether the `requires_approval` fallback
//! applies, so a no-op policy hook would silently disable approval for
//! high-risk tools that rely on it.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;

use crate::hooks::config::ShellHooksConfig;
use crate::hooks::executor::{
    parse_post, parse_pre, parse_prompt, run_command, HookContext, PreDecision,
};
use crate::hooks::matcher::matching_hooks;
use crate::security::runtime_audit::{AuditEventType, AuditLogger};
use crate::tools::approval::RiskLevel;
use crate::tools::hooks::{PostExecuteDecision, ToolHooks, ToolPolicyDecision};
use crate::tools::{ToolContext, ToolExecutionResult};

/// The shell-hooks bridge. Clone cheap (`Arc`-backed).
#[derive(Debug, Clone)]
pub struct ShellHookBridge {
    config: Arc<ShellHooksConfig>,
    audit: Option<Arc<dyn AuditLogger>>,
}

/// Rank used to fold multiple `PreToolUse` decisions: deny(2) > ask(1) >
/// allow(0). Ties keep the earlier (first) decision.
fn pre_rank(d: &PreDecision) -> u8 {
    match d {
        PreDecision::Allow => 0,
        PreDecision::Ask(_) => 1,
        PreDecision::Deny(_) => 2,
    }
}

/// Rank used to fold multiple `PostToolUse` decisions: block(2) beats
/// replace(1) beats accept(0). Ties take the later decision, mirroring the
/// existing in-order replace chain.
fn post_rank(d: &PostExecuteDecision) -> u8 {
    match d {
        PostExecuteDecision::Accept => 0,
        PostExecuteDecision::ReplaceOutput(_) => 1,
        PostExecuteDecision::Block(_) => 2,
    }
}

impl ShellHookBridge {
    /// Load `hooks.json` at `path` and build a bridge.
    ///
    /// Never fails: a missing file yields an empty bridge, a broken file
    /// yields an empty bridge after a `warn!`, and a `None` audit logger just
    /// disables the paired audit entries.
    pub fn load(path: &Path, audit: Option<Arc<dyn AuditLogger>>) -> Arc<Self> {
        let config = ShellHooksConfig::load(path).unwrap_or_default();
        Arc::new(Self {
            config: Arc::new(config),
            audit,
        })
    }

    /// An empty bridge (no hooks, no audit) — used by tests and defaults.
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            config: Arc::new(ShellHooksConfig::default()),
            audit: None,
        })
    }

    /// `true` when no hooks are configured for any event.
    pub fn is_empty(&self) -> bool {
        self.config.is_empty()
    }

    /// Build the [`ToolHooks`] bundle that gates `PreToolUse`/`PostToolUse`.
    ///
    /// Returns a truly empty `ToolHooks` when no hooks are configured so that
    /// `ToolRegistry`'s `requires_approval` fallback is unaffected.
    pub fn tool_hooks(&self) -> ToolHooks {
        if self.config.is_empty() {
            return ToolHooks::new();
        }
        let pre_bridge = Arc::new(self.clone());
        let post_bridge = pre_bridge.clone();
        ToolHooks::new()
            .policy(move |name, args, ctx| {
                let this = pre_bridge.clone();
                let name = name.to_string();
                let args = args.clone();
                let ctx = (*ctx).clone();
                async move {
                    match this.run_pre(&name, &args, &ctx).await {
                        Some(decision) => decision,
                        None => ToolPolicyDecision::Allow,
                    }
                }
            })
            .post_execute(move |name, args, result, ctx, upstream| {
                let this = post_bridge.clone();
                let name = name.to_string();
                let args = args.clone();
                let result = (*result).clone();
                let ctx = (*ctx).clone();
                async move { this.run_post(&name, &args, &result, &ctx, upstream).await }
            })
    }

    /// Run every matching `PreToolUse` hook and fold the results.
    ///
    /// Returns `Some(decision)` when the call must not proceed, `None` when
    /// it is allowed. A non-`Allow` outcome is mirrored to the audit log.
    async fn run_pre(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolContext,
    ) -> Option<ToolPolicyDecision> {
        let hits = matching_hooks(&self.config.pre_tool_use, name);
        if hits.is_empty() {
            return None;
        }
        let mut acc = PreDecision::Allow;
        let mut winner_matcher: Option<&str> = None;
        for hook in &hits {
            let hctx = HookContext {
                event: "PreToolUse",
                tool_name: Some(name.to_string()),
                tool_input: Some(args.clone()),
                tool_response: None,
                prompt: None,
                user_id: ctx.user_id.clone(),
                channel: None,
                agent_id: None,
                session_id: Some(ctx.conversation_id.clone()),
                cwd: Some(ctx.working_directory().clone()),
                workspace_dir: Some(ctx.workspace_root().clone()),
            };
            let out = run_command(&hook.command, &hctx).await;
            let decision = parse_pre(&out);
            if pre_rank(&decision) > pre_rank(&acc) {
                acc = decision;
                winner_matcher = Some(&hook.matcher);
            }
        }
        let matcher = winner_matcher.map(str::to_string);
        let decision = match acc {
            PreDecision::Allow => return None,
            PreDecision::Deny(reason) => ToolPolicyDecision::Deny { reason },
            PreDecision::Ask(reason) => ToolPolicyDecision::NeedsApproval {
                approval_id: Uuid::new_v4().to_string(),
                tool_name: name.to_string(),
                args: args.clone(),
                risk_level: RiskLevel::High,
                requested_by: ctx.user_id.clone(),
                message: reason,
            },
        };
        self.audit_policy_deny(ctx, &decision, matcher).await;
        Some(decision)
    }

    /// Run every matching `PostToolUse` hook, folding each decision with the
    /// upstream one. A `Block` outcome is mirrored to the audit log.
    async fn run_post(
        &self,
        name: &str,
        args: &Value,
        result: &ToolExecutionResult,
        ctx: &ToolContext,
        upstream: PostExecuteDecision,
    ) -> PostExecuteDecision {
        let hits = matching_hooks(&self.config.post_tool_use, name);
        if hits.is_empty() {
            return upstream;
        }
        let mut acc = upstream;
        for hook in &hits {
            let hctx = HookContext {
                event: "PostToolUse",
                tool_name: Some(name.to_string()),
                tool_input: Some(args.clone()),
                tool_response: serde_json::to_value(result).ok(),
                prompt: None,
                user_id: ctx.user_id.clone(),
                channel: None,
                agent_id: None,
                session_id: Some(ctx.conversation_id.clone()),
                cwd: Some(ctx.working_directory().clone()),
                workspace_dir: Some(ctx.workspace_root().clone()),
            };
            let out = run_command(&hook.command, &hctx).await;
            let decision = parse_post(&out);
            // Ties take the later hook's decision (in-order chain semantics).
            if post_rank(&decision) >= post_rank(&acc) {
                acc = decision;
            }
        }
        if let PostExecuteDecision::Block(feedback) = &acc {
            self.audit_block(ctx, feedback).await;
        }
        acc
    }

    /// Block gate for `UserPromptSubmit`: returns `Some(reason)` when any
    /// hook blocks the message, `None` to let it through.
    pub async fn check_user_prompt(
        &self,
        session_id: &str,
        user_id: &str,
        prompt: &str,
        channel: &str,
    ) -> Option<String> {
        if self.config.user_prompt_submit.is_empty() {
            return None;
        }
        for hook in &self.config.user_prompt_submit {
            let hctx = HookContext {
                event: "UserPromptSubmit",
                tool_name: None,
                tool_input: None,
                tool_response: None,
                prompt: Some(prompt.to_string()),
                user_id: user_id.to_string(),
                channel: Some(channel.to_string()),
                agent_id: None,
                session_id: Some(session_id.to_string()),
                cwd: None,
                workspace_dir: None,
            };
            let out = run_command(&hook.command, &hctx).await;
            if let Some(reason) = parse_prompt(&out) {
                return Some(reason);
            }
        }
        None
    }

    /// Fire-and-forget fan-out for the `Stop` event. Each hook command runs
    /// detached with the shared 10s timeout; output is discarded.
    pub fn fire_stop(&self, session_id: &str, agent_id: &str, channel: &str) {
        if self.config.stop.is_empty() {
            return;
        }
        for hook in &self.config.stop {
            let command = hook.command.clone();
            let hctx = HookContext {
                event: "Stop",
                tool_name: None,
                tool_input: None,
                tool_response: None,
                prompt: None,
                user_id: String::new(),
                channel: Some(channel.to_string()),
                agent_id: Some(agent_id.to_string()),
                session_id: Some(session_id.to_string()),
                cwd: None,
                workspace_dir: None,
            };
            // Detached and self-terminating (HOOK_TIMEOUT bounds the child);
            // intentionally fire-and-forget — a slow Stop hook must never
            // delay the agent turn teardown. The command and context are
            // cloned so nothing borrowed from `self` escapes into the task.
            tokio::spawn(async move {
                let _ = run_command(&command, &hctx).await;
            });
        }
    }

    async fn audit_policy_deny(
        &self,
        ctx: &ToolContext,
        decision: &ToolPolicyDecision,
        matcher: Option<String>,
    ) {
        let Some(audit) = &self.audit else { return };
        let (kind, description) = match decision {
            ToolPolicyDecision::Deny { reason } => ("deny", reason.as_str()),
            ToolPolicyDecision::NeedsApproval { message, .. } => ("ask", message.as_str()),
            ToolPolicyDecision::Allow => return,
        };
        let mut details = serde_json::Map::new();
        details.insert("kind".to_string(), kind.into());
        if let Some(m) = matcher {
            details.insert("matcher".to_string(), m.into());
        }
        audit
            .log_entry(
                AuditEventType::ToolDeny,
                ctx.user_id.clone(),
                "tool".to_string(),
                false,
                description.to_string(),
                Some(serde_json::Value::Object(details)),
            )
            .await;
    }

    async fn audit_block(&self, ctx: &ToolContext, feedback: &str) {
        let Some(audit) = &self.audit else { return };
        audit
            .log_entry(
                AuditEventType::ToolDeny,
                ctx.user_id.clone(),
                "tool".to_string(),
                false,
                feedback.to_string(),
                Some(serde_json::json!({ "kind": "post_execute_block" })),
            )
            .await;
    }
}
