//! Tool-call dispatch loops (plain and progress-reporting).
//! (Split out of the former single-file `agent_engine.rs`; same `impl Agent`.)

use crate::agent::turns::ToolCallRecord;
use crate::observe::TurnMetricsCollector;
use crate::providers::{Message, Role, ToolCall, ToolResult};
use tracing::{debug, error, info, warn};

use super::super::*;

/// Tools whose success changes on-disk state, invalidating any earlier
/// identical read/run result. After one of these succeeds the cross-turn
/// duplicate filter is reset so the model can re-run a command or re-read a
/// file it just touched (e.g. edit a script, then re-run the same test
/// command to verify).
fn tool_mutates_filesystem(name: &str) -> bool {
    matches!(name, "file_write" | "file_edit")
}

/// Synthetic `Role::Tool` result content used when a proposed tool call is not
/// executed but the model must still be asked for a final answer (duplicate /
/// disallowed / budget-exhausted). Kept 1:1 with the pending `tool_calls` ids
/// so strict-pairing providers (DeepSeek) accept the batch.
const NOT_EXECUTED_NOTE: &str = "[Not executed — provide your final answer now \
                                 using results already gathered.]";

/// Fraction of the tool-iteration budget after which every subsequent tool
/// batch carries a "converge now" reminder.
///
/// Multi-step research tasks routinely spend their last third of the budget on
/// exploration they cannot finish; the reminder is a nudge, not a hard stop —
/// the hard stop remains `increment_tool_iteration`.
const BUDGET_REMINDER_AT_PERCENT: usize = 70;

/// The reminder appended to the last tool result of a batch, when the agent has
/// spent [`BUDGET_REMINDER_AT_PERCENT`] of its tool-iteration budget.
fn budget_reminder(context: &Context) -> Option<String> {
    let max = context.max_tool_iterations();
    if max == 0 {
        return None;
    }
    let used = context.tool_iterations();
    if used * 100 < max * BUDGET_REMINDER_AT_PERCENT {
        return None;
    }
    let left = max.saturating_sub(used);
    Some(format!(
        "\n\n[Tool budget: {used} of {max} calls used, {left} left. Stop opening new lines of \
         investigation. Consolidate what you already have, verify only the specific fact you \
         still need, and deliver the final answer — in the exact format the user's message \
         asked for.]"
    ))
}

impl Agent {
    /// Handle tool calls from the LLM
    pub(crate) async fn handle_tool_calls(
        &self,
        context: &mut Context,
        original_response: &crate::providers::CompletionResponse,
        tool_calls: &[ToolCall],
        user_id: &str,
    ) -> crate::Result<crate::providers::CompletionResponse> {
        let cfg = self.config_snapshot();
        // Check iteration limit before processing
        if !context.increment_tool_iteration() {
            warn!(
                "Tool iteration limit reached ({}), running a final no-tool round",
                context.tool_iterations()
            );

            // Budget spent: do NOT execute this batch and do NOT return a canned
            // English message. Record the pending assistant message plus one
            // synthetic Tool result per call (1:1 id pairing so prune_if_needed
            // keeps the batch), then run one final LLM round with tools
            // suppressed so the agent writes a real user-facing summary.
            let mut pending_assistant = original_response.message.clone();
            pending_assistant.tool_calls = Some(tool_calls.to_vec());

            let not_executed = "[Not executed — tool-iteration budget exhausted. Provide your \
                                final answer now using results already gathered, in the format \
                                the user's message asked for.]";
            let mut batch = Vec::with_capacity(1 + tool_calls.len());
            batch.push(pending_assistant);
            for call in tool_calls {
                batch.push(Message {
                    role: Role::Tool,
                    content: not_executed.to_string(),
                    content_blocks: None,
                    reasoning_content: None,
                    name: None,
                    tool_calls: None,
                    tool_call_id: Some(call.id.clone()),
                    metadata: None,
                });
            }
            context.add_batch(batch);

            info!("handle_tool_calls: budget exhausted — requesting final summary round");
            return Box::pin(self.get_completion_inner(context, user_id, true)).await;
        }

        // Filter out duplicate tool calls before adding assistant message
        // This ensures the tool_call count matches the tool result count,
        // which is required by APIs like DeepSeek that enforce strict pairing.
        let filtered_tool_calls: Vec<ToolCall> = tool_calls
            .iter()
            .take(cfg.max_concurrent_tools)
            .filter(|tc| {
                let tool_name = &tc.function.name;
                let tool_args = &tc.function.arguments;
                if context.is_tool_call_duplicate(tool_name, tool_args) {
                    warn!("Duplicate tool call detected: {} with same args, skipping", tool_name);
                    false
                } else {
                    true
                }
            })
            .filter(|tc| {
                if !context.is_tool_allowed(&tc.function.name) {
                    warn!(
                        "Tool '{}' is not allowed in this delegation scope, skipping",
                        tc.function.name
                    );
                    false
                } else {
                    true
                }
            })
            .cloned()
            .collect();

        if filtered_tool_calls.is_empty() {
            // Every proposed call was a duplicate or is disallowed in this
            // scope. Returning the pending assistant message here would hand
            // the user a blank reply (a tool-call turn usually carries no text
            // content). Instead record the pending assistant message plus one
            // synthetic Tool result per call (1:1 id pairing so
            // prune_if_needed keeps the batch), then run one final no-tool
            // round so the model writes a real user-facing answer.
            let mut pending_assistant = original_response.message.clone();
            pending_assistant.tool_calls = Some(tool_calls.to_vec());

            let mut batch = Vec::with_capacity(1 + tool_calls.len());
            batch.push(pending_assistant);
            for call in tool_calls {
                batch.push(Message {
                    role: Role::Tool,
                    content: NOT_EXECUTED_NOTE.to_string(),
                    content_blocks: None,
                    reasoning_content: None,
                    name: None,
                    tool_calls: None,
                    tool_call_id: Some(call.id.clone()),
                    metadata: None,
                });
            }
            context.add_batch(batch);

            info!(
                "handle_tool_calls: all {} proposed calls were filtered — running final no-tool round",
                tool_calls.len()
            );
            return Box::pin(self.get_completion_inner(context, user_id, true)).await;
        }

        // Execute tools FIRST, before adding the assistant message.
        // This avoids context.prune_if_needed() removing the assistant because
        // its tool_call IDs don't yet have matching tool results.
        let tool_context = self
            .build_tool_context(user_id, context.id(), context.delegation().cloned())
            .with_timeout(std::time::Duration::from_secs(120));

        let mut results = Vec::new();

        for tool_call in &filtered_tool_calls {
            let tool_name = tool_call.function.name.clone();
            let tool_args = tool_call.function.arguments.clone();

            // Record this tool call before executing
            context.record_tool_call(&tool_name, &tool_args);

            debug!("Executing tool: {}", tool_name);

            let result = match self
                .tools
                .execute_call(&tool_call.function, &tool_context)
                .await
            {
                Ok(exec_result) => {
                    // Reset circuit-breaker on success
                    self.tools.reset_failure(&tool_call.function.name);
                    // Filesystem state changed: earlier identical reads/runs may
                    // now be re-run legitimately (edit → re-run same command).
                    if tool_mutates_filesystem(&tool_call.function.name) {
                        context.clear_executed_tool_calls();
                    }
                    let tool_result = exec_result.to_tool_result(&tool_call.id);
                    // Extract artifacts from successful tool results
                    self.extract_and_store_artifacts(
                        context.id(),
                        &tool_result.content,
                        &tool_call.function.name,
                    );
                    info!("Tool {} executed successfully", tool_call.function.name);
                    tool_result
                }
                Err(e) => {
                    // Record failure for circuit-breaker
                    self.tools.record_failure(&tool_call.function.name);
                    error!("Tool {} failed: {}", tool_call.function.name, e);
                    ToolResult::error(&tool_call.id, format!("Tool execution failed: {}", e))
                }
            };

            results.push(result);
        }

        // Soft budget reminder, carried on the last result of the batch.
        if let Some(note) = budget_reminder(context) {
            if let Some(last) = results.last_mut() {
                last.content.push_str(&note);
            }
        }

        // NOW add assistant message with ONLY non-duplicate tool calls,
        // immediately followed by tool results as a single atomic batch.
        // This prevents prune_if_needed() from removing the assistant before
        // its tool results are added.
        let mut assistant_msg = original_response.message.clone();
        assistant_msg.tool_calls = Some(filtered_tool_calls.clone());

        let mut batch = Vec::with_capacity(1 + results.len());
        batch.push(assistant_msg);
        for result in &results {
            batch.push(Message {
                role: Role::Tool,
                content: result.content.clone(),
                content_blocks: None,
                reasoning_content: None,
                name: None,
                tool_calls: None,
                tool_call_id: Some(result.tool_call_id.clone()),
                metadata: None,
            });
        }
        context.add_batch(batch);

        // Check execution controller before next iteration
        {
            let ctrl_guard = self.execution_controller.read().await;
            if let Some(ref ctrl) = *ctrl_guard {
                if let Err(reason) = ctrl.check_and_wait().await {
                    return Ok(crate::providers::CompletionResponse {
                        message: Message {
                            role: Role::Assistant,
                            content: format!("Execution halted: {}", reason),
                            content_blocks: None,
                            reasoning_content: None,
                            name: None,
                            tool_calls: None,
                            tool_call_id: None,
                            metadata: None,
                        },
                        usage: None,
                        model: "system".to_string(),
                        finish_reason: Some("cancelled".to_string()),
                    });
                }
            }
        }

        // Get final response (boxed to avoid recursive async issue)
        Box::pin(self.get_completion(context, user_id)).await
    }

    /// Handle tool calls with progress callbacks
    pub(crate) async fn handle_tool_calls_with_progress(
        &self,
        context: &mut Context,
        collector: &mut TurnMetricsCollector,
        original_response: &crate::providers::CompletionResponse,
        tool_calls: &[ToolCall],
        progress_cb: ProgressCallback,
        user_id: &str,
    ) -> crate::Result<crate::providers::CompletionResponse> {
        let cfg = self.config_snapshot();
        // Accumulate token usage from the LLM response that produced these tool calls
        if let Some(ref usage) = original_response.usage {
            context.accumulate_turn_token_usage(usage);
        }

        // Check iteration limit before processing
        if !context.increment_tool_iteration() {
            warn!(
                "Tool iteration limit reached ({}), running a final no-tool round",
                context.tool_iterations()
            );

            // The budget is spent. Do NOT execute this batch and do NOT return a
            // canned English message. Instead: record the pending assistant
            // message plus one synthetic Tool result per call (1:1 id pairing so
            // prune_if_needed keeps the batch), then run one final LLM round with
            // tools suppressed so the agent writes a real user-facing summary.
            let mut pending_assistant = original_response.message.clone();
            pending_assistant.tool_calls = Some(tool_calls.to_vec());

            let not_executed = "[Not executed — tool-iteration budget exhausted. Provide your \
                                final answer now using results already gathered, in the format \
                                the user's message asked for.]";
            let mut batch = Vec::with_capacity(1 + tool_calls.len());
            batch.push(pending_assistant);
            for call in tool_calls {
                batch.push(Message {
                    role: Role::Tool,
                    content: not_executed.to_string(),
                    content_blocks: None,
                    reasoning_content: None,
                    name: None,
                    tool_calls: None,
                    tool_call_id: Some(call.id.clone()),
                    metadata: None,
                });
            }
            context.add_batch(batch);

            info!(
                "handle_tool_calls_with_progress: budget exhausted — requesting final summary round"
            );
            return Box::pin(self.get_completion_with_progress_inner(
                context,
                &mut *collector,
                progress_cb,
                user_id,
                true,
            ))
            .await;
        }

        // Filter out duplicate tool calls before adding assistant message
        // This ensures the tool_call count matches the tool result count,
        // which is required by APIs like DeepSeek that enforce strict pairing.
        let filtered_tool_calls: Vec<ToolCall> = tool_calls
            .iter()
            .take(cfg.max_concurrent_tools)
            .filter(|tc| {
                let tool_name = &tc.function.name;
                let tool_args = &tc.function.arguments;
                if context.is_tool_call_duplicate(tool_name, tool_args) {
                    warn!("Duplicate tool call detected: {} with same args, skipping", tool_name);

                    // Notify about duplicate via progress callback
                    let cb = progress_cb.clone();
                    let name = tool_name.clone();
                    tokio::spawn(async move {
                        (cb)(ProgressEvent::ToolResult {
                            name,
                            result: "[Duplicate tool call skipped - already executed with same \
                                     parameters]"
                                .to_string(),
                            data: None,
                            execution_time_ms: 0,
                        })
                        .await;
                    });

                    false
                } else {
                    true
                }
            })
            .filter(|tc| {
                if !context.is_tool_allowed(&tc.function.name) {
                    warn!(
                        "Tool '{}' is not allowed in this delegation scope, skipping",
                        tc.function.name
                    );

                    // Notify about the disallowed tool via progress callback
                    let cb = progress_cb.clone();
                    let name = tc.function.name.clone();
                    tokio::spawn(async move {
                        (cb)(ProgressEvent::ToolResult {
                            name,
                            result: "[Tool skipped - not allowed in this delegation scope]"
                                .to_string(),
                            data: None,
                            execution_time_ms: 0,
                        })
                        .await;
                    });

                    false
                } else {
                    true
                }
            })
            .cloned()
            .collect();

        if filtered_tool_calls.is_empty() {
            // Every proposed call was a duplicate or is disallowed in this
            // scope. Do NOT end the turn on the (usually content-less) pending
            // assistant message: append it plus one synthetic Tool result per
            // call (1:1 id pairing so prune_if_needed keeps the batch) and run
            // one final no-tool round so the model writes a real user-facing
            // answer instead of returning a blank reply.
            let mut pending_assistant = original_response.message.clone();
            pending_assistant.tool_calls = Some(tool_calls.to_vec());

            let mut batch = Vec::with_capacity(1 + tool_calls.len());
            batch.push(pending_assistant);
            for call in tool_calls {
                batch.push(Message {
                    role: Role::Tool,
                    content: NOT_EXECUTED_NOTE.to_string(),
                    content_blocks: None,
                    reasoning_content: None,
                    name: None,
                    tool_calls: None,
                    tool_call_id: Some(call.id.clone()),
                    metadata: None,
                });
            }
            context.add_batch(batch);

            info!(
                "handle_tool_calls_with_progress: all {} proposed calls were filtered — running final no-tool round",
                tool_calls.len()
            );
            return Box::pin(self.get_completion_with_progress_inner(
                context,
                &mut *collector,
                progress_cb,
                user_id,
                true,
            ))
            .await;
        }

        // Execute tools with progress FIRST, before adding the assistant message.
        // This avoids a critical issue: context.prune_if_needed() removes any
        // assistant message whose tool_call IDs don't yet appear in tool results.
        // By adding the assistant AFTER execution and immediately followed by
        // tool results, the tool_call/tool_result pairing is preserved.
        let tool_context = self
            .build_tool_context(user_id, context.id(), context.delegation().cloned())
            .with_timeout(std::time::Duration::from_secs(120));

        let mut results = Vec::new();

        for tool_call in &filtered_tool_calls {
            let tool_name = tool_call.function.name.clone();
            let tool_args = tool_call.function.arguments.clone();

            // Record this tool call before executing
            context.record_tool_call(&tool_name, &tool_args);

            // Notify tool calling
            (progress_cb)(ProgressEvent::ToolCalling {
                name: tool_name.clone(),
                arguments: tool_args,
            })
            .await;

            debug!("Executing tool: {}", tool_name);

            let _start = std::time::Instant::now();
            let result = self
                .execute_single_tool(tool_call, &tool_context, &progress_cb, context.id())
                .await;

            info!(
                "handle_tool_calls_with_progress: tool={} executed in {:?}",
                tool_name,
                _start.elapsed()
            );

            // Filesystem state changed on success: earlier identical reads/runs
            // may now be re-run legitimately (edit → re-run same test command).
            if !result.is_error.unwrap_or(false) && tool_mutates_filesystem(&tool_name) {
                context.clear_executed_tool_calls();
            }

            let rec = ToolCallRecord {
                name: tool_name.clone(),
                args: tool_call.function.arguments.to_string(),
                result: result.content.clone(),
                success: !result.is_error.unwrap_or(false),
                duration_ms: _start.elapsed().as_millis() as u64,
            };
            collector.record_tool(&rec);
            context.push_tool_call_record(rec);

            results.push(result);
        }

        // Build a map of tool_call_id -> result content for history persistence
        let tool_result_map: std::collections::HashMap<String, String> = results
            .iter()
            .map(|r| (r.tool_call_id.clone(), r.content.clone()))
            .collect();

        // Soft budget reminder, carried on the last result of the batch.
        if let Some(note) = budget_reminder(context) {
            if let Some(last) = results.last_mut() {
                last.content.push_str(&note);
            }
        }

        // NOW add assistant message with ONLY non-duplicate tool calls,
        // immediately followed by tool results as a single atomic batch.
        // This prevents prune_if_needed() from removing the assistant before
        // its tool results are added.
        let mut assistant_msg = original_response.message.clone();
        assistant_msg.tool_calls = Some(filtered_tool_calls.clone());

        let mut batch = Vec::with_capacity(1 + results.len());
        batch.push(assistant_msg);
        for result in &results {
            batch.push(Message {
                role: Role::Tool,
                content: result.content.clone(),
                content_blocks: None,
                reasoning_content: None,
                name: None,
                tool_calls: None,
                tool_call_id: Some(result.tool_call_id.clone()),
                metadata: None,
            });
        }
        context.add_batch(batch);

        // Check execution controller before next iteration
        {
            let ctrl_guard = self.execution_controller.read().await;
            if let Some(ref ctrl) = *ctrl_guard {
                if let Err(reason) = ctrl.check_and_wait().await {
                    return Ok(crate::providers::CompletionResponse {
                        message: Message {
                            role: Role::Assistant,
                            content: format!("Execution halted: {}", reason),
                            content_blocks: None,
                            reasoning_content: None,
                            name: None,
                            tool_calls: None,
                            tool_call_id: None,
                            metadata: None,
                        },
                        usage: None,
                        model: "system".to_string(),
                        finish_reason: Some("cancelled".to_string()),
                    });
                }
            }
        }

        // Get final response with progress (recursive LLM call)
        let recursive_llm_start = std::time::Instant::now();
        info!("handle_tool_calls_with_progress: calling recursive get_completion_with_progress ({} tools executed)", results.len());
        let mut final_response = Box::pin(self.get_completion_with_progress(
            context,
            &mut *collector,
            progress_cb,
            user_id,
        ))
        .await?;
        info!(
            "handle_tool_calls_with_progress: recursive get_completion_with_progress returned in {:?}",
            recursive_llm_start.elapsed()
        );

        // Accumulate token usage from this LLM completion in the tool loop
        if let Some(ref usage) = final_response.usage {
            context.accumulate_turn_token_usage(usage);
        }

        // Preserve tool calls from the original assistant message so that
        // downstream consumers (session_store, etc.) can see what tools were invoked.
        if let Some(ref original_calls) = original_response.message.tool_calls {
            match final_response.message.tool_calls {
                None => final_response.message.tool_calls = Some(original_calls.clone()),
                Some(ref mut existing) => {
                    let mut merged = original_calls.clone();
                    merged.append(existing);
                    final_response.message.tool_calls = Some(merged);
                }
            }
        }

        // Attach execution results to the preserved tool calls so that history
        // replay can show "Done" instead of "Running".
        if let Some(ref mut calls) = final_response.message.tool_calls {
            for call in calls.iter_mut() {
                if call.result.is_none() {
                    if let Some(result_content) = tool_result_map.get(&call.id) {
                        call.result = Some(result_content.clone());
                    }
                }
            }
        }

        Ok(final_response)
    }
}
