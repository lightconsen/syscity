//! Buffered and streaming call entry points.

use super::*;

impl ToolRegistry {
    /// Execute a function call from an LLM.
    /// Checks both static and dynamic registries.
    /// Enforces the timeout configured in `ToolContext`.
    pub async fn execute_call(
        &self,
        call: &FunctionCall,
        context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let args: Value = self.parse_tool_args(&call.arguments, &call.name)?;
        let tool_name = call.name.clone();
        let timeout = context.timeout();

        // Pre-execute gate: policy hooks run for every buffered tool call.
        // Uses the hooks-only variant so the built-in `requires_approval`
        // fallback is NOT activated here — without installed policy hooks a
        // `requires_approval` tool keeps its historical ungated behaviour.
        let policy_decision = self
            .evaluate_policy_hooks_only(&tool_name, &args, context)
            .await;
        match policy_decision {
            ToolPolicyDecision::Allow => {}
            ToolPolicyDecision::Deny { reason } => {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Tool '{}' denied: {}",
                    tool_name, reason
                )));
            }
            ToolPolicyDecision::NeedsApproval { .. } => {
                // Route through the full execute() approval flow so the call
                // suspends for human review (mirrors execute_call_streaming).
                let result = self.execute(&tool_name, args.clone(), context).await;
                return match result {
                    Some(r) => r,
                    None => Err(crate::error::SyscityError::Validation(format!(
                        "Tool '{}' was found but could not be executed",
                        tool_name
                    ))),
                };
            }
        }

        // Try static tools first
        if let Some(tool) = self.get(&tool_name) {
            let exec_start = std::time::Instant::now();
            let exec_future = tool.execute(args.clone(), context);
            let result: crate::Result<ToolExecutionResult> =
                tokio::time::timeout(timeout, exec_future)
                    .await
                    .map_err(|_| {
                        crate::error::SyscityError::Timeout(format!(
                            "Tool '{}' timed out after {:?} (actual execution: {:?}).{}",
                            tool_name,
                            timeout,
                            exec_start.elapsed(),
                            self.uncertainty_note(&tool_name)
                        ))
                    })?;
            info!(
                "execute_call: tool={} completed in {:?} (timeout={:?})",
                tool_name,
                exec_start.elapsed(),
                timeout
            );
            return match self
                .finalize_result(&tool_name, &args, context, Some(result))
                .await
            {
                Some(r) => r,
                None => Err(crate::error::SyscityError::Validation(format!(
                    "Tool '{}' finalization failed",
                    tool_name
                ))),
            };
        }

        // Try dynamic tools
        let dynamic_tool = self
            .dynamic_tools
            .read()
            .ok()
            .and_then(|map| map.get(&tool_name).cloned());

        if let Some(tool) = dynamic_tool {
            if !self.is_blocked(&tool_name) && !self.is_degraded(&tool_name) {
                let exec_start = std::time::Instant::now();
                let exec_future = tool.execute(args.clone(), context);
                let result: crate::Result<ToolExecutionResult> =
                    tokio::time::timeout(timeout, exec_future)
                        .await
                        .map_err(|_| {
                            crate::error::SyscityError::Timeout(format!(
                                "Tool '{}' timed out after {:?} (actual execution: {:?}).{}",
                                tool_name,
                                timeout,
                                exec_start.elapsed(),
                                self.uncertainty_note(&tool_name)
                            ))
                        })?;
                info!(
                    "execute_call: tool={} completed in {:?} (timeout={:?})",
                    tool_name,
                    exec_start.elapsed(),
                    timeout
                );
                return match self
                    .finalize_result(&tool_name, &args, context, Some(result))
                    .await
                {
                    Some(r) => r,
                    None => Err(crate::error::SyscityError::Validation(format!(
                        "Tool '{}' finalization failed",
                        tool_name
                    ))),
                };
            }
        }

        Err(crate::error::SyscityError::Validation(format!(
            "Unknown tool: {}. Available tools: {}",
            tool_name,
            self.list().join(", ")
        )))
    }

    /// Execute a function call from an LLM with streaming output.
    ///
    /// Policy hooks, approval, and before-hooks are run before chunks are
    /// yielded. `on_chunk` is invoked for every [`ToolExecutionChunk`]
    /// produced by the tool. After the stream completes, after-hooks,
    /// content filtering, and audit logging are applied and the final
    /// [`ToolExecutionResult`] is returned.
    ///
    /// This method owns the tool reference internally, so it works for both
    /// static and dynamically-registered tools without lifetime issues.
    pub async fn execute_call_streaming<F, Fut>(
        &self,
        call: &FunctionCall,
        context: &ToolContext,
        mut on_chunk: F,
    ) -> crate::Result<ToolExecutionResult>
    where
        F: FnMut(ToolExecutionChunk) -> Fut + Send,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let args: Value = self.parse_tool_args(&call.arguments, &call.name)?;

        let tool_name = call.name.clone();

        let policy_decision = self.evaluate_policy(&tool_name, &args, context).await;
        match policy_decision {
            // Allow → proceed to execution below.
            ToolPolicyDecision::Allow => {}
            ToolPolicyDecision::Deny { reason } => {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Tool '{}' denied: {}",
                    tool_name, reason
                )));
            }
            ToolPolicyDecision::NeedsApproval { .. } => {
                // For streaming tools, fall back to buffered execution so the
                // approval flow can suspend and resume in a single future.
                let result = self.execute(&tool_name, args.clone(), context).await;
                return match result {
                    Some(Ok(exec_result)) => {
                        if !exec_result.output.is_empty() {
                            on_chunk(ToolExecutionChunk::Output(exec_result.output.clone())).await;
                        }
                        if let Some(error) = exec_result.error.clone() {
                            on_chunk(ToolExecutionChunk::Error(error)).await;
                        }
                        if let Some(data) = exec_result.data.clone() {
                            on_chunk(ToolExecutionChunk::Data(data)).await;
                        }
                        Ok(exec_result)
                    }
                    Some(Err(e)) => {
                        on_chunk(ToolExecutionChunk::Error(e.to_string())).await;
                        Err(e)
                    }
                    None => Err(crate::error::SyscityError::Validation(format!(
                        "Tool '{}' was found but could not be executed (may have been \
                         deregistered)",
                        tool_name,
                    ))),
                };
            }
        }

        // Run before-hooks.
        self.active_hooks().run_before(&tool_name, &args).await;

        // Look up the tool and consume its stream.
        let collected = if let Some(tool) = self.get(&tool_name) {
            consume_stream(tool.execute_stream(args.clone(), context), &mut on_chunk).await
        } else {
            let dynamic_tool = self
                .dynamic_tools
                .read()
                .ok()
                .and_then(|map| map.get(&tool_name).cloned());
            if let Some(tool) = dynamic_tool {
                if !self.is_blocked(&tool_name) && !self.is_degraded(&tool_name) {
                    consume_stream(tool.execute_stream(args.clone(), context), &mut on_chunk).await
                } else {
                    return Err(crate::error::SyscityError::Validation(format!(
                        "Tool '{}' is blocked or degraded",
                        tool_name
                    )));
                }
            } else {
                return Err(crate::error::SyscityError::Validation(format!(
                    "Unknown tool: {}. Available tools: {}",
                    tool_name,
                    self.list().join(", ")
                )));
            }
        };

        // Apply after-hooks, content filtering, and audit logging.
        match self
            .finalize_stream_result(&tool_name, &args, context, collected)
            .await
        {
            Some(Ok(result)) => Ok(result),
            Some(Err(e)) => Err(e),
            None => Err(crate::error::SyscityError::Validation(format!(
                "Tool '{}' finalization failed",
                tool_name
            ))),
        }
    }

    /// Apply content filtering and audit logging to a collected streaming
    /// result, and run after-hooks.
    ///
    /// This is the streaming equivalent of the post-processing performed by
    /// [`execute`](ToolRegistry::execute) after a buffered call.
    pub async fn finalize_stream_result(
        &self,
        name: &str,
        args: &Value,
        context: &ToolContext,
        collected: ToolExecutionResult,
    ) -> Option<crate::Result<ToolExecutionResult>> {
        self.active_hooks().run_after(name, args, &collected).await;
        self.finalize_result(name, args, context, Some(Ok(collected)))
            .await
    }
}
