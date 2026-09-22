//! Tool execution: the cache-aware pipeline, spilling and post-execute policy.

use super::*;

impl ToolRegistry {
    /// Execute a tool by name with optional caching, hooks, and approval flow.
    /// Checks both static and dynamic registries.
    ///
    /// # Policy and Approval Flow
    ///
    /// 1. Run policy hooks — if any hook returns `Deny`, return error
    ///    immediately
    /// 2. If any hook returns `NeedsApproval` and approval_queue is configured,
    ///    suspend execution and wait for human approval
    /// 3. Run before-hooks
    /// 4. Execute the tool
    /// 5. Run after-hooks
    pub async fn execute(
        &self,
        name: &str,
        args: Value,
        context: &ToolContext,
    ) -> Option<crate::Result<ToolExecutionResult>> {
        let policy_decision = self.evaluate_policy(name, &args, context).await;

        match policy_decision {
            ToolPolicyDecision::Allow => {
                // Proceed with execution
            }
            ToolPolicyDecision::Deny { reason } => {
                return Some(Err(crate::error::SyscityError::Validation(format!(
                    "Tool '{}' denied: {}",
                    name, reason
                ))));
            }
            ToolPolicyDecision::NeedsApproval {
                approval_id,
                tool_name,
                args: approval_args,
                risk_level,
                requested_by,
                message,
            } => {
                // Check if approval queue is configured
                let approval_queue = match &self.approval_queue {
                    Some(q) => q.clone(),
                    None => {
                        return Some(Err(crate::error::SyscityError::Validation(
                            "Tool requires approval but no approval queue configured".into(),
                        )));
                    }
                };

                // Create oneshot channel for the approval resolution
                let (tx, rx) = tokio::sync::oneshot::channel();

                // Create pending approval
                let approval =
                    PendingApproval::new(&approval_id, &tool_name, approval_args, requested_by)
                        .with_risk_level(risk_level)
                        .with_message(message)
                        .with_session(Some(context.conversation_id.clone()))
                        .with_response_tx(tx);

                // Submit to approval queue
                approval_queue.submit(approval).await;

                // Wait for human decision (with 5-minute timeout)
                const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
                match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
                    Ok(Ok(ApprovalDecision::Approve)) => {
                        tracing::info!(
                            "Approval {} granted, proceeding with tool execution",
                            approval_id
                        );
                        // Proceed with execution below
                    }
                    Ok(Ok(ApprovalDecision::Deny { reason })) => {
                        return Some(Err(crate::error::SyscityError::Validation(format!(
                            "Tool '{}' denied by user: {}",
                            name, reason
                        ))));
                    }
                    Ok(Err(_)) => {
                        return Some(Err(crate::error::SyscityError::Validation(
                            "Approval channel closed".into(),
                        )));
                    }
                    Err(_) => {
                        return Some(Err(crate::error::SyscityError::Timeout(format!(
                            "Tool '{}' approval request timed out after {:?}",
                            name, APPROVAL_TIMEOUT
                        ))));
                    }
                }
            }
        }

        // Run before-hooks
        self.active_hooks().run_before(name, &args).await;

        // Check cache first
        let cache_key = Self::cache_key(name, &args);
        if let Some(cached_result) = self.get_cached(&cache_key) {
            tracing::debug!("Cache hit for tool: {}", name);
            let result = Ok(cached_result);
            if let Ok(ref exec_result) = result {
                self.active_hooks()
                    .run_after(name, &args, exec_result)
                    .await;
            }
            return self
                .finalize_result(name, &args, context, Some(result))
                .await;
        }

        // Execute the tool — clone args so the original remains for after-hooks
        let execution_result: Option<crate::Result<ToolExecutionResult>> = {
            // Try static tools first
            if let Some(tool) = self.get(name) {
                let _t_exec = std::time::Instant::now();
                let result = tool.execute(args.clone(), context).await;
                info!("[Timing] tool.execute({}) returned in {:?}", name, _t_exec.elapsed());
                if let Ok(ref exec_result) = result {
                    self.store_cached(cache_key, exec_result.clone());
                }
                Some(result)
            } else {
                // Try dynamic tools
                let dynamic_tool = self
                    .dynamic_tools
                    .read()
                    .ok()
                    .and_then(|map| map.get(name).cloned());

                if let Some(tool) = dynamic_tool {
                    if !self.is_blocked(name) && !self.is_degraded(name) {
                        let result = tool.execute(args.clone(), context).await;
                        if let Ok(ref exec_result) = result {
                            self.store_cached(cache_key, exec_result.clone());
                        }
                        Some(result)
                    } else {
                        Some(Err(crate::error::SyscityError::Validation(format!(
                            "Tool '{}' is blocked or degraded",
                            name
                        ))))
                    }
                } else {
                    None
                }
            }
        };

        // Run after-hooks
        if let Some(Ok(ref exec_result)) = execution_result {
            let _t_hook = std::time::Instant::now();
            self.active_hooks()
                .run_after(name, &args, exec_result)
                .await;
            info!("[Timing] run_after({}) done in {:?}", name, _t_hook.elapsed());
        }

        let _t_filter = std::time::Instant::now();
        let result = self
            .finalize_result(name, &args, context, execution_result)
            .await;
        info!("[Timing] filter_and_audit({}) done in {:?}", name, _t_filter.elapsed());
        result
    }

    /// Apply content filtering and audit logging to a tool execution result.
    async fn filter_and_audit(
        &self,
        name: &str,
        context: &ToolContext,
        result: Option<crate::Result<ToolExecutionResult>>,
    ) -> Option<crate::Result<ToolExecutionResult>> {
        // ── Audit: tool invocation ─────────────────────────────────────────
        if let Some(ref audit) = self.audit_log {
            let allowed = matches!(result, Some(Ok(_)));
            audit
                .log_entry(
                    crate::security::runtime_audit::AuditEventType::ToolInvocation,
                    context.user_id.clone(),
                    name.to_string(),
                    allowed,
                    format!("Tool '{}' executed", name),
                    None,
                )
                .await;
        }

        // ── Content filtering ──────────────────────────────────────────────
        let result = match result {
            Some(Ok(exec_result)) => {
                // Let the tool itself decide whether content filtering applies
                let skip_filter = self
                    .get(name)
                    .map(|t| t.skip_content_filter())
                    .unwrap_or(false);

                if skip_filter {
                    Some(Ok(exec_result))
                } else if let Some(ref filter) = self.content_filter {
                    let outcome = filter.filter_result(&exec_result);

                    // Audit: content filter action
                    if let Some(ref audit) = self.audit_log {
                        if outcome.action != crate::security::content_filter::FilterAction::Pass {
                            let details = serde_json::json!({
                                "action": format!("{:?}", outcome.action),
                                "pii_findings": outcome.pii_findings.len(),
                                "secret_findings": outcome.secret_findings.len(),
                                "summary": outcome.summary,
                            });
                            audit
                                .log_entry(
                                    crate::security::runtime_audit::AuditEventType::ContentFilter,
                                    context.user_id.clone(),
                                    name.to_string(),
                                    outcome.action
                                        != crate::security::content_filter::FilterAction::Blocked,
                                    outcome.summary.clone(),
                                    Some(details),
                                )
                                .await;
                        }
                    }

                    let filtered = crate::tools::ToolExecutionResult {
                        success: if outcome.action
                            == crate::security::content_filter::FilterAction::Blocked
                        {
                            false
                        } else {
                            outcome.success
                        },
                        output: outcome.output,
                        error: if outcome.action
                            == crate::security::content_filter::FilterAction::Blocked
                        {
                            Some(outcome.summary)
                        } else {
                            exec_result.error
                        },
                        data: outcome.data,
                        execution_time: exec_result.execution_time,
                    };
                    Some(Ok(filtered))
                } else {
                    Some(Ok(exec_result))
                }
            }
            other => other,
        };

        result
    }

    /// Run the post-execute hook chain on a finished result, then apply
    /// content filtering and audit logging.
    ///
    /// This is the single choke point every execution path funnels through
    /// (buffered, streaming, cached, and bare `execute_call`). Post-execute
    /// hooks see the raw result and may replace its output or confiscate it
    /// with corrective feedback; the content filter always has the final
    /// word on whatever the hooks leave behind.
    pub(super) async fn finalize_result(
        &self,
        name: &str,
        args: &Value,
        context: &ToolContext,
        result: Option<crate::Result<ToolExecutionResult>>,
    ) -> Option<crate::Result<ToolExecutionResult>> {
        let result = match result {
            Some(Ok(exec_result)) if self.active_hooks().has_post_execute_hooks() => {
                let decision = self
                    .active_hooks()
                    .run_post_execute(name, args, &exec_result, context)
                    .await;
                if let PostExecuteDecision::Block(feedback) = &decision {
                    self.audit_post_execute_block(name, context, feedback).await;
                }
                Some(Ok(Self::apply_post_execute(name, exec_result, decision)))
            }
            other => other,
        };
        // Spill bounds whatever the hooks produced; a hook Block yields
        // success=false and is naturally skipped.
        let result = match result {
            Some(Ok(exec_result)) => {
                Some(Ok(self.maybe_spill(name, args, context, exec_result).await))
            }
            other => other,
        };
        self.filter_and_audit(name, context, result).await
    }

    /// Spill an oversized successful output to a workspace file, replacing
    /// the model-facing output with a head/tail preview plus a retrieval
    /// hint. Best-effort: a failed write keeps the original output — a
    /// successful call must never turn into a failure here.
    async fn maybe_spill(
        &self,
        name: &str,
        args: &Value,
        context: &ToolContext,
        result: ToolExecutionResult,
    ) -> ToolExecutionResult {
        let Some(threshold) = self.spill_threshold else {
            return result;
        };
        if !result.success || result.output.len() <= threshold {
            return result;
        }
        // Re-reading a previously spilled artifact is exempt: spilling it
        // again would force the model into a read -> spill -> read loop. Every
        // other oversized output (including `file_read` of a large file)
        // spills, preserving the tail.
        let root = context.workspace_root().clone();
        if crate::tools::spill::arg_targets_spilled_file(&root, args) {
            return result;
        }
        let tool_name = name.to_string();
        let output = result.output.clone();
        let spilled = tokio::task::spawn_blocking(move || {
            crate::tools::spill::spill_output(&root, &tool_name, &output, threshold)
        })
        .await;

        match spilled {
            Ok(Ok(outcome)) => {
                info!(
                    "Tool '{}' output spilled ({} bytes) to {}",
                    name,
                    outcome.total_bytes,
                    outcome.path.display()
                );
                let mut result = ToolExecutionResult {
                    output: outcome.replacement,
                    ..result
                };
                let spill_meta = serde_json::json!({
                    "path": outcome.rel_path,
                    "total_bytes": outcome.total_bytes,
                });
                result.data = Some(match result.data.take() {
                    Some(mut data) => {
                        data["spill"] = spill_meta;
                        data
                    }
                    None => serde_json::json!({ "spill": spill_meta }),
                });
                result
            }
            Ok(Err(e)) => {
                warn!("Failed to spill tool '{}' output ({}); keeping original", name, e);
                result
            }
            Err(e) => {
                warn!("Spill task failed for tool '{}' ({}); keeping original", name, e);
                result
            }
        }
    }

    /// Apply a post-execute decision to a finished result.
    fn apply_post_execute(
        name: &str,
        result: ToolExecutionResult,
        decision: PostExecuteDecision,
    ) -> ToolExecutionResult {
        match decision {
            PostExecuteDecision::Accept => result,
            PostExecuteDecision::ReplaceOutput(output) => ToolExecutionResult { output, ..result },
            PostExecuteDecision::Block(feedback) => {
                warn!("Tool '{}' result blocked by post-execute policy: {}", name, feedback);
                ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(feedback),
                    data: None,
                    execution_time: result.execution_time,
                }
            }
        }
    }

    /// Audit a post-execute block as a `ToolDeny` event — the auditable
    /// counterpart to the tool's own `ToolInvocation` entry.
    async fn audit_post_execute_block(&self, name: &str, ctx: &ToolContext, feedback: &str) {
        let Some(ref audit) = self.audit_log else {
            return;
        };
        let details = serde_json::json!({ "kind": "post_execute_block" });
        audit
            .log_entry(
                crate::security::runtime_audit::AuditEventType::ToolDeny,
                ctx.user_id.clone(),
                name.to_string(),
                false,
                format!("Tool '{}' result blocked by post-execute policy: {}", name, feedback),
                Some(details),
            )
            .await;
    }

    /// Execute a tool by name, skipping the cache layer but still running the
    /// full policy, approval, hooks, and audit pipeline.
    ///
    /// Returns `None` only when the tool name is unknown (not registered).
    /// Blocked, degraded, or policy-denied tools return `Some(Err(...))`
    /// so callers can distinguish "not found" from "rejected".
    ///
    /// This is `pub(crate)` for use by other modules in the crate
    /// (e.g. streaming execution paths) that need to bypass the cache
    /// without sacrificing safety checks, though currently no external
    /// caller exists.
    #[cfg(test)]
    pub(crate) async fn execute_no_cache(
        &self,
        name: &str,
        args: Value,
        context: &ToolContext,
    ) -> Option<crate::Result<ToolExecutionResult>> {
        // Run policy evaluation (approval, denials, hooks all handled here).
        let policy_decision = self.evaluate_policy(name, &args, context).await;
        match policy_decision {
            ToolPolicyDecision::Allow => { /* proceed */ }
            ToolPolicyDecision::Deny { reason } => {
                return Some(Err(crate::error::SyscityError::Validation(format!(
                    "Tool '{}' denied: {}",
                    name, reason
                ))));
            }
            ToolPolicyDecision::NeedsApproval { .. } => {
                // execute_no_cache does not support the full approval flow;
                // callers should use `execute()` instead.
                return Some(Err(crate::error::SyscityError::Validation(format!(
                    "Tool '{}' requires approval; use execute() instead of execute_no_cache",
                    name,
                ))));
            }
        }

        // Run before-hooks
        self.active_hooks().run_before(name, &args).await;

        // Execute the tool
        let execution_result: Option<crate::Result<ToolExecutionResult>> = {
            // Try static tools first
            if let Some(tool) = self.get(name) {
                Some(tool.execute(args.clone(), context).await)
            } else {
                // Try dynamic tools
                let dynamic_tool = self
                    .dynamic_tools
                    .read()
                    .ok()
                    .and_then(|map| map.get(name).cloned());
                if let Some(tool) = dynamic_tool {
                    if !self.is_blocked(name) && !self.is_degraded(name) {
                        Some(tool.execute(args.clone(), context).await)
                    } else {
                        Some(Err(crate::error::SyscityError::Validation(format!(
                            "Tool '{}' is blocked or degraded",
                            name
                        ))))
                    }
                } else {
                    None
                }
            }
        };

        // Run after-hooks
        if let Some(Ok(ref exec_result)) = execution_result {
            let _t_hook = std::time::Instant::now();
            self.active_hooks()
                .run_after(name, &args, exec_result)
                .await;
            info!("[Timing] run_after({}) done in {:?}", name, _t_hook.elapsed());
        }

        let _t_filter = std::time::Instant::now();
        let result = self
            .finalize_result(name, &args, context, execution_result)
            .await;
        info!("[Timing] filter_and_audit({}) done in {:?}", name, _t_filter.elapsed());
        result
    }

    /// Parse tool call arguments, handling provider-specific edge cases.
    ///
    /// Some providers (DeepSeek) append trailing text after the JSON object
    /// or emit multiple JSON values. This uses a streaming parser that
    /// extracts only the first valid JSON value and ignores trailing content.
    pub(crate) fn parse_tool_args(&self, raw: &str, tool_name: &str) -> crate::Result<Value> {
        let s = raw.trim();
        if s.is_empty() {
            return Ok(serde_json::json!({}));
        }

        // Fast path: direct parse works for clean JSON.
        if let Ok(val) = serde_json::from_str::<Value>(s) {
            return Ok(val);
        }

        // Fallback: streaming parser extracts only the first JSON value,
        // ignoring any trailing text or multiple objects.
        let stream = serde_json::Deserializer::from_str(s);
        if let Some(value) = stream.into_iter::<Value>().next() {
            return match value {
                Ok(val) => Ok(val),
                Err(e) => Err(crate::error::SyscityError::Validation(format!(
                    "Invalid arguments for tool {}: {}",
                    tool_name, e
                ))),
            };
        }

        Err(crate::error::SyscityError::Validation(format!(
            "Empty arguments for tool {}",
            tool_name
        )))
    }
}
