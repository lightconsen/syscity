//! Controller-parameterised turn entry points and single-tool execution.
//! (Split out of the former single-file `agent_engine.rs`; same `impl Agent`.)

use std::sync::Arc;

use crate::channels::{IncomingMessage, OutgoingMessage};
use crate::providers::{ToolCall, ToolResult};
use crate::tools::{ToolContext, ToolExecutionChunk};
use tracing::{error, info};

use super::super::*;

impl Agent {
    /// Process a message in persistent session mode with an execution
    /// controller.
    ///
    /// The controller is attached before processing and detached afterward,
    /// enabling pause/resume/step/cancel during the tool-call loop.
    pub async fn process_message_with_controller(
        &self,
        message: IncomingMessage,
        controller: Arc<ExecutionController>,
        max_iterations: usize,
    ) -> crate::Result<OutgoingMessage> {
        {
            let mut ctrl = self.execution_controller.write().await;
            *ctrl = Some(controller);
            let mut max_iter = self.max_tool_iterations_override.write().await;
            *max_iter = Some(max_iterations);
        }

        let result = self.process_message(message).await;

        {
            let mut ctrl = self.execution_controller.write().await;
            *ctrl = None;
            let mut max_iter = self.max_tool_iterations_override.write().await;
            *max_iter = None;
        }

        result
    }

    /// Steer the running turn, if one is running.
    ///
    /// Returns `false` when no turn holds a controller — the caller then falls
    /// back to queueing the message for the next one. This is what
    /// `QueueMode::Steer` means: keep what the turn has already done and tell
    /// it the new thing, rather than throwing the turn away.
    pub async fn steer(&self, message: impl Into<String>) -> bool {
        let guard = self.execution_controller.read().await;
        match guard.as_ref() {
            Some(controller) => {
                controller.steer(message);
                true
            }
            None => false,
        }
    }

    /// Execute a single tool call, using streaming when the tool advertises
    /// `capabilities.streaming`.
    pub(crate) async fn execute_single_tool(
        &self,
        tool_call: &ToolCall,
        tool_context: &ToolContext,
        progress_cb: &ProgressCallback,
        context_id: &str,
    ) -> ToolResult {
        let tool_name = tool_call.function.name.clone();
        let capabilities = self.tools.get_capabilities(&tool_name);

        if capabilities.streaming {
            self.execute_single_tool_stream(tool_call, tool_context, progress_cb, context_id)
                .await
        } else {
            self.execute_single_tool_buffered(tool_call, tool_context, progress_cb, context_id)
                .await
        }
    }

    /// Buffered execution path for tools that do not support streaming.
    pub(crate) async fn execute_single_tool_buffered(
        &self,
        tool_call: &ToolCall,
        tool_context: &ToolContext,
        progress_cb: &ProgressCallback,
        context_id: &str,
    ) -> ToolResult {
        let tool_name = tool_call.function.name.clone();

        let _start = std::time::Instant::now();
        let result = self
            .tools
            .execute_call(&tool_call.function, tool_context)
            .await;
        let execution_time_ms = _start.elapsed().as_millis() as u64;
        match result {
            Ok(exec_result) => {
                // Reset circuit-breaker on success
                self.tools.reset_failure(&tool_name);
                let tool_data = exec_result.data.clone();
                let tool_result = exec_result.to_tool_result(&tool_call.id);
                let result_str = tool_result.content.clone();

                // Extract artifacts from successful tool results
                self.extract_and_store_artifacts(context_id, &result_str, &tool_name);

                // Notify tool result
                (progress_cb)(ProgressEvent::ToolResult {
                    name: tool_name.clone(),
                    result: result_str.chars().take(200).collect(), // Truncate for display
                    data: tool_data,
                    execution_time_ms,
                })
                .await;

                info!("Tool {} executed successfully", tool_name);
                tool_result
            }
            Err(e) => {
                // Record failure for circuit-breaker
                self.tools.record_failure(&tool_name);
                let error_msg = format!("Tool execution failed: {}", e);

                // Notify tool error
                (progress_cb)(ProgressEvent::ToolResult {
                    name: tool_name.clone(),
                    result: error_msg.clone(),
                    data: None,
                    execution_time_ms,
                })
                .await;

                error!("Tool {} failed: {}", tool_name, e);
                ToolResult::error(&tool_call.id, error_msg)
            }
        }
    }

    /// Streaming execution path for tools that advertise streaming support.
    pub(crate) async fn execute_single_tool_stream(
        &self,
        tool_call: &ToolCall,
        tool_context: &ToolContext,
        progress_cb: &ProgressCallback,
        context_id: &str,
    ) -> ToolResult {
        let tool_name = tool_call.function.name.clone();
        let progress_cb = progress_cb.clone();

        let _start = std::time::Instant::now();
        let result = self
            .tools
            .execute_call_streaming(&tool_call.function, tool_context, |chunk| {
                let progress_cb = progress_cb.clone();
                let tool_name = tool_name.clone();
                async move {
                    let (chunk_text, is_error) = match chunk {
                        ToolExecutionChunk::Output(text) => (text, false),
                        ToolExecutionChunk::Error(text) => (text, true),
                        ToolExecutionChunk::Data(_) | ToolExecutionChunk::Done => return,
                    };
                    (progress_cb)(ProgressEvent::ToolResultDelta {
                        name: tool_name,
                        chunk: chunk_text,
                        is_error,
                    })
                    .await;
                }
            })
            .await;
        let execution_time_ms = _start.elapsed().as_millis() as u64;

        match result {
            Ok(exec_result) => {
                self.tools.reset_failure(&tool_name);
                let tool_data = exec_result.data.clone();
                let tool_result = exec_result.to_tool_result(&tool_call.id);
                let result_str = tool_result.content.clone();

                self.extract_and_store_artifacts(context_id, &result_str, &tool_name);

                (progress_cb)(ProgressEvent::ToolResult {
                    name: tool_name.clone(),
                    result: result_str.chars().take(200).collect(),
                    data: tool_data,
                    execution_time_ms,
                })
                .await;

                info!("Streaming tool {} executed successfully", tool_name);
                tool_result
            }
            Err(e) => {
                self.tools.record_failure(&tool_name);
                let error_msg = format!("Tool execution failed: {}", e);
                (progress_cb)(ProgressEvent::ToolResult {
                    name: tool_name.clone(),
                    result: error_msg.clone(),
                    data: None,
                    execution_time_ms,
                })
                .await;
                error!("Streaming tool {} failed: {}", tool_name, e);
                ToolResult::error(&tool_call.id, error_msg)
            }
        }
    }

    /// Process a message with progress callbacks and an execution controller.
    pub async fn process_message_with_progress_and_controller(
        &self,
        message: IncomingMessage,
        progress_cb: ProgressCallback,
        controller: Arc<ExecutionController>,
        max_iterations: usize,
    ) -> crate::Result<OutgoingMessage> {
        {
            let mut ctrl = self.execution_controller.write().await;
            *ctrl = Some(controller);
            let mut max_iter = self.max_tool_iterations_override.write().await;
            *max_iter = Some(max_iterations);
        }

        let result = self
            .process_message_with_progress(message, progress_cb)
            .await;

        {
            let mut ctrl = self.execution_controller.write().await;
            *ctrl = None;
            let mut max_iter = self.max_tool_iterations_override.write().await;
            *max_iter = None;
        }

        result
    }

    /// Run a message in one-shot mode (no persistence) with an execution
    /// controller.
    ///
    /// The thread context is discarded after execution completes.
    pub async fn run_message_with_controller(
        &self,
        message: IncomingMessage,
        controller: Arc<ExecutionController>,
        max_iterations: usize,
    ) -> crate::Result<OutgoingMessage> {
        let conversation_id = message.conversation_id.0.clone();

        {
            let mut ctrl = self.execution_controller.write().await;
            *ctrl = Some(controller);
            let mut max_iter = self.max_tool_iterations_override.write().await;
            *max_iter = Some(max_iterations);
        }

        let result = self.process_message(message).await;

        {
            let mut ctrl = self.execution_controller.write().await;
            *ctrl = None;
            let mut max_iter = self.max_tool_iterations_override.write().await;
            *max_iter = None;
        }

        // Run mode: discard the thread after execution
        {
            let mut map = self.thread_map.lock().await;
            map.remove(&conversation_id);
        }

        result
    }
}
