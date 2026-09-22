//! Policy gating: permission evaluation, approval routing and decision audit.

use super::*;

impl ToolRegistry {
    /// Execute a tool by name with optional caching, hooks, and approval flow.
    /// Checks both static and dynamic registries.
    ///
    /// # Policy and Approval Flow
    ///
    /// Run policy hooks and the built-in `requires_approval` fallback.
    ///
    /// If no explicit policy hooks are configured but the tool advertises
    /// `requires_approval`, this synthesises a `NeedsApproval` decision
    /// automatically so that high-risk tools (device access, etc.) are
    /// never executed silently without the caller going through approval.
    pub(super) async fn evaluate_policy(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolContext,
    ) -> ToolPolicyDecision {
        self.run_gate(name, args, ctx, false).await
    }

    /// The approval a tool advertising `requires_approval` needs.
    ///
    /// One constructor for both policy paths, so what a caller sees cannot
    /// depend on which of them asked.
    fn approval_fallback(&self, name: &str, args: &Value) -> ToolPolicyDecision {
        let risk_level = self.tool_capabilities(name).risk_level;
        self.needs_approval(name, args, risk_level, format!("Tool '{}' requires approval", name))
    }

    /// A `[permissions].ask` rule matched: route to approval with the rule's
    /// reason and the tool's real risk level.
    fn force_ask(&self, name: &str, args: &Value, reason: String) -> ToolPolicyDecision {
        let risk_level = self.tool_capabilities(name).risk_level;
        self.needs_approval(name, args, risk_level, reason)
    }

    /// The one `NeedsApproval` constructor, so every ask path looks the same
    /// to the approval queue and the audit log.
    fn needs_approval(
        &self,
        name: &str,
        args: &Value,
        risk_level: crate::tools::approval::RiskLevel,
        message: String,
    ) -> ToolPolicyDecision {
        ToolPolicyDecision::NeedsApproval {
            approval_id: format!(
                "fallback-{}-{}",
                name,
                uuid::Uuid::new_v4()
                    .to_string()
                    .split('-')
                    .next()
                    .unwrap_or("0000")
            ),
            tool_name: name.to_string(),
            args: args.clone(),
            risk_level,
            requested_by: "system".to_string(),
            message,
        }
    }

    /// The permission engine: rules + mode, evaluated before any hook.
    ///
    /// Reads the true (post-wrapper-fix) capabilities, so plan mode and the
    /// `write` category see what the tool really is.
    fn evaluate_permissions(&self, name: &str, args: &Value, ctx: &ToolContext) -> EngineDecision {
        let caps = self.tool_capabilities(name);
        self.permissions.evaluate(
            name,
            args,
            &ctx.identity.conversation_id,
            caps.read_only,
            &caps.categories,
        )
    }

    /// The one policy gate both execution paths share: permission engine,
    /// then hooks, then the `requires_approval` fallback.
    ///
    /// Order is locked: a `deny` rule blocks before hooks can even speak; a
    /// hook `Deny` beats every pre-approval; a hook `NeedsApproval` is
    /// honoured as explicit configuration except under `bypass`, where it is
    /// downgraded to allow (and audited); an `allow` rule or mode
    /// pre-approval skips the fallback; an `ask` rule forces the fallback
    /// with the rule's reason. `hooks_only_fallback` preserves the two
    /// paths' existing difference: the buffered path additionally requires
    /// [`can_ask_a_human`](crate::tools::ask_user::can_ask_a_human) before
    /// the fallback fires.
    async fn run_gate(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolContext,
        hooks_only_fallback: bool,
    ) -> ToolPolicyDecision {
        let engine = self.evaluate_permissions(name, args, ctx);

        let decision = if let EngineDecision::Deny(reason) = engine {
            // A deny rule blocks before hooks can speak.
            ToolPolicyDecision::Deny { reason }
        } else {
            let hook = self.active_hooks().run_policy(name, args, ctx).await;
            match hook {
                ToolPolicyDecision::Deny { reason } => ToolPolicyDecision::Deny { reason },
                ToolPolicyDecision::NeedsApproval { .. }
                    if engine == EngineDecision::AllowNow
                        && self
                            .permissions
                            .effective_mode(&ctx.identity.conversation_id)
                            == PermissionMode::Bypass =>
                {
                    // bypass skips approval prompts; an explicit hook ask is
                    // downgraded with it and audited below.
                    ToolPolicyDecision::Allow
                }
                other => match (other, engine) {
                    (ToolPolicyDecision::Allow, EngineDecision::Ask(reason)) => {
                        self.force_ask(name, args, reason)
                    }
                    (ToolPolicyDecision::Allow, EngineDecision::AllowNow) => {
                        ToolPolicyDecision::Allow
                    }
                    (ToolPolicyDecision::Allow, EngineDecision::PassThrough) => {
                        // requires_approval fallback — only when no policy hook
                        // exists, so an explicitly-configured policy hook is
                        // always authoritative.
                        if !self.active_hooks().has_policy_hooks()
                            && self.get_capabilities(name).requires_approval
                            && (!hooks_only_fallback
                                || crate::tools::ask_user::can_ask_a_human(ctx))
                        {
                            self.approval_fallback(name, args)
                        } else {
                            ToolPolicyDecision::Allow
                        }
                    }
                    (other, _) => other,
                },
            }
        };

        self.audit_policy_decision(name, ctx, &decision).await;
        decision
    }

    /// Evaluate the registered policy hooks, plus the `requires_approval`
    /// fallback when there is a human to answer it — auditing any non-allow
    /// decision.
    ///
    /// Used by the buffered [`execute_call`](Self::execute_call) path. With no
    /// policy hooks installed, a tool that advertises `requires_approval` is
    /// now gated **and only where the question can be put to someone**: see
    /// [`can_ask_a_human`]. A policy hook that returns `NeedsApproval` is
    /// honoured either way (the caller routes it through the full approval
    /// flow).
    pub(super) async fn evaluate_policy_hooks_only(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolContext,
    ) -> ToolPolicyDecision {
        self.run_gate(name, args, ctx, true).await
    }

    /// Audit a non-allow policy decision as a `ToolDeny` event so a denied or
    /// approval-flagged call forms an auditable pair with any later
    /// `ToolInvocation` entry.
    async fn audit_policy_decision(
        &self,
        name: &str,
        ctx: &ToolContext,
        decision: &ToolPolicyDecision,
    ) {
        if decision.is_allow() {
            return;
        }
        let Some(ref audit) = self.audit_log else {
            return;
        };
        let (kind, description) = match decision {
            ToolPolicyDecision::Deny { reason } => ("deny", reason.clone()),
            ToolPolicyDecision::NeedsApproval { message, .. } => ("ask", message.clone()),
            ToolPolicyDecision::Allow => return,
        };
        let details = serde_json::json!({ "kind": kind });
        audit
            .log_entry(
                crate::security::runtime_audit::AuditEventType::ToolDeny,
                ctx.user_id.clone(),
                name.to_string(),
                false,
                format!("Tool '{}' {} by policy: {}", name, kind, description),
                Some(details),
            )
            .await;
    }
}
