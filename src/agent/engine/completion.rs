//! LLM completion calls, model resolution, context compaction, and request snapshots.
//! (Split out of the former single-file `agent_engine.rs`; same `impl Agent`.)

use tokio_stream::StreamExt;

use crate::observe::TurnMetricsCollector;
use crate::providers::{CompletionRequest, Message, Role, ToolCall};
use tracing::{debug, info, warn};

use super::super::*;

/// Injected once when an LLM round ends with neither text nor tool calls —
/// e.g. a reasoning-only stream that exhausted its output budget, or a
/// provider that silently stops. Pushes the model to actually answer instead
/// of completing the turn with a blank reply.
const EMPTY_REPLY_NUDGE: &str = "你的上一条回复内容为空（没有生成任何文字，也没有调用工具）。请直接继续：要么基于已有信息给出最终回答，要么执行下一步操作，不要返回空内容。\n\
Your previous reply was empty (no text and no tool calls were produced). Continue directly: give your final answer based on what you already have, or take the next concrete step — do not reply with empty content.";

impl Agent {
    /// Resolve the effective model id for a conversation, using the same
    /// precedence as the send path: per-session binding > temporary override >
    /// agent default > provider default.
    pub(crate) async fn resolve_model_id(&self, conversation_id: &str) -> String {
        let session_model = self
            .session_models
            .read()
            .await
            .get(conversation_id)
            .cloned();
        let override_model = self.model_override.read().await.clone();
        session_model
            .or(override_model)
            .or(self.model.clone())
            .unwrap_or_else(|| self.provider.default_model().to_string())
    }

    /// Persist a compact snapshot of one outgoing LLM request — resolved model
    /// id, system prompt, and the tool names/schemas offered — into the
    /// `request_snapshots` side table, for post-hoc debugging of turns where
    /// the model behaved oddly. Fire-and-forget: insert failures are logged,
    /// never propagated. Only the first send attempt of a request is
    /// snapshotted (a compaction retry changes the messages, not the header).
    pub(crate) fn persist_request_snapshot(
        &self,
        context: &Context,
        model_id: &str,
        tools: &[crate::providers::ToolDefinition],
    ) {
        let Some(store) = self.session_store.clone() else {
            return;
        };
        // Copy everything the snapshot needs into owned data before spawning:
        // `RequestSnapshot` borrows, and borrowed locals cannot escape into a
        // `'static` task.
        let session_id = self.session_id.clone();
        let conversation_id = context.id().to_string();
        let agent_id = self.agent_id.clone();
        let model = model_id.to_string();
        let system_prompt = context.system_prompt().to_string();
        let tools_json = super::super::session_store::compact_tools_json(tools);
        tokio::spawn(async move {
            let snapshot = super::super::session_store::RequestSnapshot {
                session_id: session_id.as_deref(),
                conversation_id: Some(&conversation_id),
                agent_id: Some(&agent_id),
                model: &model,
                system_prompt: &system_prompt,
                tools_json: &tools_json,
            };
            if let Err(e) = store.save_request_snapshot(&snapshot).await {
                warn!("Failed to save request snapshot: {}", e);
            }
        });
    }

    /// Get a completion from the LLM, handling tool calls.
    ///
    /// Thin wrapper over [`Agent::get_completion_inner`] with `final_round = false`.
    pub(crate) async fn get_completion(
        &self,
        context: &mut Context,
        user_id: &str,
    ) -> crate::Result<crate::providers::CompletionResponse> {
        // An LLM round can end with neither text nor tool calls (e.g. a
        // reasoning-only stream that exhausted its output budget, or the
        // provider silently stopping). That must not complete the turn as a
        // blank reply — nudge once toward a real answer, then surface
        // whatever comes back (honest even if still empty).
        let mut nudged_empty = false;
        loop {
            let response = self.get_completion_inner(context, user_id, false).await?;
            let has_output = !response.message.content.trim().is_empty()
                || response
                    .message
                    .tool_calls
                    .as_ref()
                    .is_some_and(|c| !c.is_empty());
            if has_output || nudged_empty {
                return Ok(response);
            }
            nudged_empty = true;
            warn!(
                "LLM returned an empty reply (no text, no tool calls); nudging once toward a response"
            );
            context.add_message(Message {
                role: Role::User,
                content: EMPTY_REPLY_NUDGE.to_string(),
                content_blocks: None,
                reasoning_content: None,
                name: None,
                tool_calls: None,
                tool_call_id: None,
                metadata: None,
            });
        }
    }

    /// Get a completion from the LLM, handling tool calls.
    ///
    /// When `final_round == true` the model is NOT offered tools and residual
    /// tool calls are discarded, so it can only write a text answer — used to
    /// close a turn with a real summary after the tool budget is exhausted.
    pub(crate) async fn get_completion_inner(
        &self,
        context: &mut Context,
        user_id: &str,
        final_round: bool,
    ) -> crate::Result<crate::providers::CompletionResponse> {
        let cfg = self.config_snapshot();
        // If the context is over-budget, reduce it before sending (persisting
        // the compaction mask so a restart can rehydrate the same boundary).
        // No observability collector exists on this (non-progress) path, so
        // compression events here are not recorded.
        self.compact_context_if_needed(context, &cfg, None).await;

        // Attachment refs in tool results: current turn materializes to
        // images, older turns degrade to placeholders (request clone only).
        let mut messages = context.to_messages();
        crate::attachments::materialize_history(&mut messages);

        // Get available tools (skipped on the final round so the model can no
        // longer request more tools — it must write its closing summary).
        let tools: Vec<crate::providers::ToolDefinition> = if final_round {
            Vec::new()
        } else {
            let tool_context =
                self.build_tool_context(user_id, context.id(), context.delegation().cloned());
            let tool_defs = self.tools.get_available(&tool_context);
            let has_tools = !tool_defs.is_empty();
            // Convert FunctionDefinition to ToolDefinition. Kept owned so the
            // request can be re-armed after a compaction retry below.
            if has_tools && self.provider.supports_tools() {
                tool_defs
                    .into_iter()
                    .map(|f| crate::providers::ToolDefinition {
                        tool_type: "function".to_string(),
                        function: f,
                    })
                    .collect()
            } else {
                Vec::new()
            }
        };

        let extra = self.extra_params.read().await.clone();
        let mut request = CompletionRequest {
            model: self.model.clone(),
            messages,
            temperature: Some(cfg.temperature),
            max_tokens: Some(cfg.max_tokens),
            stream: false,
            extra,
            ..Default::default()
        };
        self.patch_request_for_reasoning(&mut request);
        if !tools.is_empty() {
            request.tools = Some(tools.clone());
        }

        // Check live cost guard before calling provider
        if let Some(ref guard) = self.cost_guard {
            if guard.is_exceeded() {
                return Err(crate::error::SyscityError::Validation(
                    "Budget limit exceeded — refusing provider call. Adjust daily_limit_cents or \
                     hourly_action_limit in config."
                        .to_string(),
                ));
            }
        }

        // Snapshot what is about to be sent (one row per LLM request). The
        // routing ref may carry a `cloud/` qualifier; store the bare model id.
        let model_id = self.resolve_model_id(context.id()).await;
        let snapshot_model = crate::model_router::parse_model_ref(&model_id).1;
        self.persist_request_snapshot(context, snapshot_model, &tools);

        // Get completion — use model router when available for key rotation /
        // fallback. If the provider rejects the request as over its context
        // window, compact the context and retry once instead of failing.
        let mut retried = false;
        let response = loop {
            let outcome = if let Some(ref router) = self.model_router {
                let req_tools = request.tools.take();
                match router
                    .complete_with_route(&model_id, request.messages, req_tools)
                    .await
                {
                    Ok((resp, rec)) => {
                        // Non-progress path has no turn collector; surface the
                        // route decision at debug level for replay/debugging.
                        debug!(
                            "[observe] route record: {}",
                            serde_json::to_string(&rec).unwrap_or_default()
                        );
                        Ok(resp)
                    }
                    Err(e) => Err(e),
                }
            } else {
                self.provider.complete(request.clone()).await
            };

            match outcome {
                Ok(r) => break r,
                Err(e)
                    if !retried
                        && crate::model_router::FailureClass::from_error(&e, None)
                            == crate::model_router::FailureClass::ContextLength =>
                {
                    retried = true;
                    info!(
                        "[compaction] provider rejected context as too long — compacting and \
                         retrying once"
                    );
                    self.compact_context_forced(context, &cfg, None).await;
                    let mut messages = context.to_messages();
                    crate::attachments::materialize_history(&mut messages);
                    request.messages = messages;
                    if !tools.is_empty() {
                        request.tools = Some(tools.clone());
                    }
                }
                Err(e) => return Err(e),
            }
        };

        // Record token usage in cost guard
        if let Some(ref guard) = self.cost_guard {
            if let Some(ref usage) = response.usage {
                guard.record_usage(
                    usage.prompt_tokens as u64,
                    usage.completion_tokens as u64,
                    response.model.as_str(),
                );
            }
        }

        // Handle tool calls if present — unless this is the final round, in
        // which case residual tool calls are discarded so the agent cannot loop
        // back into the tool-iteration guard.
        if !final_round {
            if let Some(tool_calls) = &response.message.tool_calls {
                if !tool_calls.is_empty() {
                    debug!("Processing {} tool calls", tool_calls.len());
                    return self
                        .handle_tool_calls(context, &response, tool_calls, user_id)
                        .await;
                }
            }
        }

        // Add assistant message to context (final round never carries dangling
        // tool calls into history).
        let mut final_message = response.message.clone();
        if final_round {
            final_message.tool_calls = None;
        }
        context.add_message(final_message);

        Ok(response)
    }

    /// If `context` is over-budget, compact it and persist a durable
    /// compaction record so a later restart can rehydrate `[summary] + tail`
    /// instead of replaying the full history.
    pub(crate) async fn compact_context_if_needed(
        &self,
        context: &mut Context,
        cfg: &crate::agent::AgentConfig,
        collector: Option<&mut TurnMetricsCollector>,
    ) {
        // If the context is over-budget, try to reduce it before sending.
        if context.needs_pruning() {
            self.compact_context_forced(context, cfg, collector).await;
        }
    }

    /// Force a compaction regardless of the local token budget, then persist
    /// the boundary. Used by the overflow retry: when the provider rejects the
    /// request as over its real context window, `needs_pruning()` may still be
    /// false (our estimate is larger than the model's limit), so compaction
    /// must not be gated on it.
    pub(crate) async fn compact_context_forced(
        &self,
        context: &mut Context,
        cfg: &crate::agent::AgentConfig,
        collector: Option<&mut TurnMetricsCollector>,
    ) {
        let tokens_before = context.token_count();
        if let Some(ref compaction_model) = cfg.compaction_model {
            // LLM-assisted compaction: produce a high-quality summary.
            let compressor =
                crate::agent::compressor::ContextCompressor::new(cfg.max_context_tokens);
            let history = context.history().to_vec();
            let compacted = compressor
                .compact_with_llm(&history, &self.provider, Some(compaction_model.as_str()), 2, 6)
                .await;
            context.replace_messages(compacted);
            if let Some(c) = collector {
                c.record_compression(tokens_before, context.token_count(), "llm_summary");
            }
        } else {
            // Fallback: drop middle messages and insert a placeholder summary.
            // This keeps the context coherent without an extra LLM call.
            context.summarize();
            if let Some(c) = collector {
                c.record_compression(tokens_before, context.token_count(), "heuristic_summary");
            }
        }
        self.record_compaction_boundary(context).await;
    }

    /// Persist the current compaction mask.
    ///
    /// The boundary anchor is the first message after the summary that is safe
    /// to replay from persistence: a user message or an assistant message
    /// without tool calls. Tool results and tool-calling assistant turns are
    /// not stored in `chat_messages`, so anchoring on them would leave a broken
    /// pair on rehydration.
    pub(crate) async fn record_compaction_boundary(&self, context: &Context) {
        let Some(summary_idx) = context
            .history()
            .iter()
            .position(|m| m.name.as_deref() == Some("compaction_summary"))
        else {
            return;
        };
        let summary = context.history()[summary_idx].content.clone();
        let Some(boundary) = context.history()[summary_idx + 1..].iter().find(|m| {
            m.role != crate::providers::Role::Tool
                && !(m.role == crate::providers::Role::Assistant && m.tool_calls.is_some())
        }) else {
            return;
        };
        let Some(ref store) = self.chat_history else {
            return;
        };
        if let Err(e) = store
            .record_compaction(
                context.id(),
                &boundary.role.to_string(),
                &boundary.content,
                &summary,
            )
            .await
        {
            warn!("[compaction] failed to persist compaction boundary: {}", e);
        }
    }

    /// Get a completion from the LLM with progress callbacks.
    ///
    /// Thin wrapper over [`Agent::get_completion_with_progress_inner`] with
    /// `final_round = false` — ordinary rounds may request tools.
    pub(crate) async fn get_completion_with_progress(
        &self,
        context: &mut Context,
        collector: &mut TurnMetricsCollector,
        progress_cb: ProgressCallback,
        user_id: &str,
    ) -> crate::Result<crate::providers::CompletionResponse> {
        // Same empty-reply guard as [`Agent::get_completion`]: a round that
        // yields neither text nor tool calls must not complete the turn blank.
        let mut nudged_empty = false;
        loop {
            let response = self
                .get_completion_with_progress_inner(
                    context,
                    collector,
                    progress_cb.clone(),
                    user_id,
                    false,
                )
                .await?;
            let has_output = !response.message.content.trim().is_empty()
                || response
                    .message
                    .tool_calls
                    .as_ref()
                    .is_some_and(|c| !c.is_empty());
            if has_output || nudged_empty {
                return Ok(response);
            }
            nudged_empty = true;
            warn!(
                "LLM round returned an empty reply (no text, no tool calls); nudging once toward a response"
            );
            context.add_message(Message {
                role: Role::User,
                content: EMPTY_REPLY_NUDGE.to_string(),
                content_blocks: None,
                reasoning_content: None,
                name: None,
                tool_calls: None,
                tool_call_id: None,
                metadata: None,
            });
        }
    }

    /// Get a completion from the LLM with progress callbacks.
    ///
    /// When `final_round == true` the model is NOT offered any tools and any
    /// residual tool calls in its reply are discarded — it can only write a
    /// text answer. Used after the tool-iteration budget is exhausted so the
    /// agent always closes with a real user-facing summary instead of a canned
    /// "reached the maximum number of tool calls" message.
    pub(crate) async fn get_completion_with_progress_inner(
        &self,
        context: &mut Context,
        collector: &mut TurnMetricsCollector,
        progress_cb: ProgressCallback,
        user_id: &str,
        final_round: bool,
    ) -> crate::Result<crate::providers::CompletionResponse> {
        let cfg = self.config_snapshot();
        // Attachment refs in tool results: current turn materializes to
        // images, older turns degrade to placeholders (request clone only).
        let mut messages = context.to_messages();
        crate::attachments::materialize_history(&mut messages);
        let user_msg_count = messages.iter().filter(|m| m.role == Role::User).count();
        let assistant_msg_count = messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        let tool_msg_count = messages.iter().filter(|m| m.role == Role::Tool).count();
        info!(
            "get_completion_with_progress: entry — msgs={} (user={}, asst={}, tool={})",
            messages.len(),
            user_msg_count,
            assistant_msg_count,
            tool_msg_count
        );

        // Get available tools (skipped on the final round so the model can no
        // longer request more tools — it must write its closing summary).
        let tools: Vec<crate::providers::ToolDefinition> = if final_round {
            Vec::new()
        } else {
            let tool_context =
                self.build_tool_context(user_id, context.id(), context.delegation().cloned());
            let tool_defs = self.tools.get_available(&tool_context);
            let has_tools = !tool_defs.is_empty();
            // Convert FunctionDefinition to ToolDefinition. Kept owned so the
            // request can be re-armed after a compaction retry below.
            if has_tools && self.provider.supports_tools() {
                tool_defs
                    .into_iter()
                    .map(|f| crate::providers::ToolDefinition {
                        tool_type: "function".to_string(),
                        function: f,
                    })
                    .collect()
            } else {
                Vec::new()
            }
        };

        let extra = self.extra_params.read().await.clone();
        let mut request = CompletionRequest {
            model: self.model.clone(),
            messages,
            temperature: Some(cfg.temperature),
            max_tokens: Some(cfg.max_tokens),
            stream: true,
            extra,
            ..Default::default()
        };
        self.patch_request_for_reasoning(&mut request);
        if !tools.is_empty() {
            request.tools = Some(tools.clone());
        }

        // Check live cost guard before calling provider
        if let Some(ref guard) = self.cost_guard {
            if guard.is_exceeded() {
                return Err(crate::error::SyscityError::Validation(
                    "Budget limit exceeded — refusing provider call. Adjust daily_limit_cents or \
                     hourly_action_limit in config."
                        .to_string(),
                ));
            }
        }

        // Notify generating (starting)
        (progress_cb)(ProgressEvent::Generating { content: None }).await;

        // Snapshot what is about to be sent (one row per LLM request). The
        // routing ref may carry a `cloud/` qualifier; store/emit the bare id.
        let model_id = self.resolve_model_id(context.id()).await;
        let bare_model = crate::model_router::parse_model_ref(&model_id)
            .1
            .to_string();
        self.persist_request_snapshot(context, &bare_model, &tools);

        // Snapshot the full request messages (untruncated) for the observability
        // full-trace input. Captured before the stream-setup loop below because
        // the model-router branch moves `request.messages`.
        let input_json = serde_json::to_string(&request.messages).map_err(|e| {
            crate::error::SyscityError::Internal(format!(
                "Failed to serialize request messages: {}",
                e
            ))
        })?;

        // Get streaming completion — use model router when available. If the
        // provider rejects the request as over its context window at stream
        // setup (before any bytes are emitted), compact and retry once.
        let mut retried = false;
        let (raw_stream, family, round_model, round_provider, route_record) = loop {
            let setup = if let Some(ref router) = self.model_router {
                let req_tools = request.tools.take();
                let stream = router
                    .stream_with_route(&model_id, request.messages, req_tools)
                    .await;
                let provider = router
                    .provider_for_model(&model_id)
                    .await
                    .unwrap_or_else(|| "router".to_string());
                // Capture the route decision from the successful setup attempt.
                let route_record = stream.as_ref().ok().map(|(_, rec)| rec.clone());
                let stream = stream.map(|(s, _)| s);
                // When using model router, fall back to Generic stream family
                (
                    stream,
                    crate::providers::stream_wrappers::ProviderStreamFamily::Generic,
                    bare_model.clone(),
                    provider,
                    route_record,
                )
            } else {
                let round_model = request
                    .model
                    .clone()
                    .unwrap_or_else(|| self.provider.default_model().to_string());
                let round_provider = self.provider.name().to_string();
                (
                    self.provider.stream(request.clone()).await,
                    self.provider.stream_family(),
                    round_model,
                    round_provider,
                    None,
                )
            };

            match setup {
                (Ok(stream), family, round_model, round_provider, route_record) => {
                    break (stream, family, round_model, round_provider, route_record);
                }
                (Err(e), _, _, _, _)
                    if !retried
                        && crate::model_router::FailureClass::from_error(&e, None)
                            == crate::model_router::FailureClass::ContextLength =>
                {
                    retried = true;
                    info!(
                        "[compaction] provider rejected streaming context as too long — \
                         compacting and retrying once"
                    );
                    self.compact_context_forced(context, &cfg, Some(&mut *collector))
                        .await;
                    let mut messages = context.to_messages();
                    crate::attachments::materialize_history(&mut messages);
                    request.messages = messages;
                    if !tools.is_empty() {
                        request.tools = Some(tools.clone());
                    }
                }
                (Err(e), _, _, _, _) => return Err(e),
            }
        };
        let registry = crate::providers::stream_wrappers::StreamFamilyRegistry::default();
        let mut stream = registry.apply(family, raw_stream);

        // Begin an observability round for this LLM call.
        collector.begin_round(&round_provider, &round_model, Some(input_json));
        // Persist the route decision on the turn record.
        if let Some(rec) = route_record {
            collector.record_route(rec);
        }

        let mut accumulated_text = String::new();
        let mut accumulated_reasoning = String::new();
        let mut accumulated_tool_calls: Vec<ToolCall> = Vec::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<crate::providers::Usage> = None;

        while let Some(chunk) = stream.next().await {
            // Emit reasoning delta
            if let Some(ref reasoning_delta) = chunk.reasoning_content {
                if !reasoning_delta.is_empty() {
                    collector.round_first_token();
                    collector.push_reasoning_delta(reasoning_delta);
                    accumulated_reasoning.push_str(reasoning_delta);
                    (progress_cb)(ProgressEvent::Generating {
                        content: Some(reasoning_delta.clone()),
                    })
                    .await;
                }
            }

            // Emit text delta
            if let Some(ref text_delta) = chunk.content {
                if !text_delta.is_empty() {
                    collector.round_first_token();
                    collector.push_text_delta(text_delta);
                    accumulated_text.push_str(text_delta);
                    (progress_cb)(ProgressEvent::ContentDelta { text: text_delta.clone() }).await;
                }
            }

            // Accumulate tool calls from stream
            if let Some(ref calls) = chunk.tool_calls {
                for call in calls {
                    // Merge partial tool calls by index (streaming deltas use index as key)
                    let key = call.index.unwrap_or(0);
                    if let Some(existing) = accumulated_tool_calls
                        .iter_mut()
                        .find(|c| c.index == Some(key) || (c.index.is_none() && c.id == call.id))
                    {
                        // Fill in id/type/name from first chunk if they were empty.
                        // Note: some providers (DeepSeek) send the function name
                        // in every delta chunk; unconditionally appending with
                        // push_str would duplicate it (file_readfile_read).
                        if existing.id.is_empty() && !call.id.is_empty() {
                            existing.id = call.id.clone();
                        }
                        if existing.call_type.is_empty() && !call.call_type.is_empty() {
                            existing.call_type = call.call_type.clone();
                        }
                        if existing.function.name.is_empty() && !call.function.name.is_empty() {
                            existing.function.name = call.function.name.clone();
                        }
                        // Some providers (DeepSeek) send the complete JSON
                        // arguments in every delta chunk. If the incoming
                        // chunk looks like a complete JSON value (starts
                        // with `{`/`[` and ends with `}`/`]`), replace the
                        // existing arguments rather than appending.
                        let incoming = &call.function.arguments;
                        let is_complete_json =
                            incoming.starts_with(['{', '[']) && incoming.ends_with(['}', ']']);
                        if is_complete_json && !existing.function.arguments.is_empty() {
                            existing.function.arguments = incoming.clone();
                        } else {
                            existing.function.arguments.push_str(incoming);
                        }
                    } else {
                        accumulated_tool_calls.push(call.clone());
                    }
                }
            }

            if chunk.is_done {
                finish_reason = Some("stop".to_string());
                usage = chunk.usage;
                break;
            }
        }

        // Close the observability round with usage and an accurate finish reason.
        let round_finish_reason = if accumulated_tool_calls.is_empty() {
            "stop".to_string()
        } else {
            "tool_calls".to_string()
        };
        collector.end_round(usage.as_ref(), Some(round_finish_reason));

        // Build the final message
        let final_message = Message {
            role: Role::Assistant,
            content: accumulated_text.clone(),
            content_blocks: None,
            reasoning_content: if accumulated_reasoning.is_empty() {
                None
            } else {
                Some(accumulated_reasoning.clone())
            },
            name: None,
            tool_calls: if accumulated_tool_calls.is_empty() {
                None
            } else {
                Some(accumulated_tool_calls.clone())
            },
            tool_call_id: None,
            metadata: None,
        };

        let response = crate::providers::CompletionResponse {
            message: final_message,
            usage,
            model: self
                .model
                .clone()
                .unwrap_or_else(|| self.provider.default_model().to_string()),
            finish_reason,
        };

        // Record token usage in cost guard (approximate from accumulated text if no
        // usage provided)
        if let Some(ref guard) = self.cost_guard {
            let prompt_tokens = context
                .to_messages()
                .iter()
                .map(|m| m.content.len() / 4)
                .sum::<usize>() as u64;
            let completion_tokens =
                (accumulated_text.len() + accumulated_reasoning.len()) as u64 / 4;
            guard.record_usage(prompt_tokens, completion_tokens, response.model.as_str());
        }

        // Handle tool calls if present — unless this is the final round, in
        // which case residual tool calls are discarded so the agent cannot loop
        // back into the tool-iteration guard.
        if !final_round {
            if let Some(ref tool_calls) = response.message.tool_calls {
                if !tool_calls.is_empty() {
                    debug!("Processing {} tool calls with progress", tool_calls.len());
                    return self
                        .handle_tool_calls_with_progress(
                            context,
                            &mut *collector,
                            &response,
                            tool_calls,
                            progress_cb,
                            user_id,
                        )
                        .await;
                }
            }
        }

        // Add assistant message to context (final round never carries dangling
        // tool calls into history).
        let mut final_message = response.message.clone();
        if final_round {
            final_message.tool_calls = None;
        }
        context.add_message(final_message);

        // Accumulate token usage for non-tool-call responses
        if let Some(ref usage) = response.usage {
            context.accumulate_turn_token_usage(usage);
        }

        Ok(response)
    }
}
