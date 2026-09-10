//! The agent turn loop: reset, process an incoming message, and the
//! progress-reporting variant.
//! (Split out of the former single-file `agent_engine.rs`; same `impl Agent`.)

use std::sync::Arc;

use super::super::agent_cache::{are_tools_cacheable, should_use_cache_llm};
use crate::channels::{IncomingMessage, OutgoingMessage};
use crate::observe::{
    ChannelObservation, ErrorSource, TurnContext, TurnMetricsCollector, TurnMetricsSink,
};
use crate::providers::Message;
use tracing::{debug, error, info, warn};

use super::super::*;

impl Agent {
    /// Begin-of-turn reset for the active plan surfaces.
    ///
    /// The currently effective plan is the todo list written during the most
    /// recent turn; a new user turn automatically clears it so the UI never
    /// shows a stale checklist. Concretely this:
    ///
    /// 1. Drops the conversation's `ActivePlan` (and its persisted snapshot)
    ///    so the prompt no longer injects stale task context, and
    /// 2. Clears the `todo` tool's whole-snapshot state for the conversation.
    ///
    /// Called before any planning runs for the new message, so a plan the
    /// new turn creates is written after this reset and survives it.
    pub(crate) async fn begin_user_turn_reset(&self, conversation_id: &str) {
        let had_plan = {
            let mut plans = self.active_plans.write().await;
            plans.remove(conversation_id).is_some()
        };
        if had_plan {
            // Best-effort removal of the persisted plan snapshot so a daemon
            // restart cannot resurrect the cleared plan.
            if let Some(ref dir) = self.plans_dir {
                let path = dir.join(format!("{}.json", conversation_id));
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        warn!("Failed to remove persisted plan {:?}: {}", path, e);
                    }
                }
            }
            info!("New turn: cleared active plan for conversation {}", conversation_id);
        }

        if let Some(todo_state) = self.tools.todo_state() {
            todo_state.clear_conversation(conversation_id).await;
        }
    }
    pub async fn process_message(
        &self,
        message: IncomingMessage,
    ) -> crate::Result<OutgoingMessage> {
        debug!("Processing message from user: {}", message.user_id);

        let conversation_id = message.conversation_id.0.clone();
        let user_id = message.user_id.0.clone();
        let content = message.content.clone();

        // Composer chips (skills/agents) ride the message metadata; they are
        // appended to the model-facing user message only (never persisted).
        let mentions = crate::channels::mentions_from_extra(&message.metadata.extra);
        let mentions_block = mentions
            .as_ref()
            .and_then(crate::channels::format_mentions_block);
        // Mentions participate in the cache key so identical texts with
        // different chips don't share a cached response.
        let cache_key_content = match &mentions_block {
            Some(block) => format!("{}\u{0}{}", content, block),
            None => content.clone(),
        };

        // ── Prompt-injection guard ────────────────────────────────────────────
        let input_scan = crate::skills::guard::scan_input(&content);
        if !input_scan.passed {
            warn!("Blocked suspicious input from user {}: {:?}", user_id, input_scan.issues);
            return Ok(OutgoingMessage::new(
                crate::channels::ConversationId(conversation_id),
                "I'm unable to process this request as it contains potentially unsafe content. If \
                 you believe this is a mistake, please rephrase your message."
                    .to_string(),
            ));
        }

        // ── New-turn reset ────────────────────────────────────────────────────
        // A new user turn clears the previous turn's active plan/todo list.
        self.begin_user_turn_reset(&conversation_id).await;

        // ── Thread binding check ──────────────────────────────────────────────
        if let Some(ref manager) = self.thread_binding_manager {
            // Check if a binding exists and is still valid
            if manager.is_valid(&conversation_id).await {
                // Record activity on the existing binding
                manager.record_activity(&conversation_id).await;
            } else if manager.get(&conversation_id).await.is_some() {
                // Binding exists but is expired — remove it and warn
                warn!(
                    "Thread binding expired/session {} for conversation {}",
                    conversation_id, conversation_id
                );
                manager.remove(&conversation_id).await;
            }
            // Reap any idle bindings periodically (best-effort)
            let _reaped = manager.reap().await;
        }

        // Decide whether to attempt a response-cache lookup (follow-ups bypass it).
        let should_cache = self.should_attempt_cache(&content).await;

        if should_cache {
            if let Some(cached) = self
                .response_cache
                .get(&user_id, &conversation_id, &cache_key_content)
                .await
            {
                info!("Cache hit for user {} - returning cached response", user_id);

                // Store user message in chat history
                if let Some(ref store) = self.chat_history {
                    use crate::memory::ChatMessage;
                    let chat_msg = ChatMessage::new(&conversation_id, &user_id, "user", &content);
                    if let Err(e) = store.store_message(chat_msg).await {
                        error!("Failed to store user message: {}", e);
                    }
                }

                // Store cached assistant response in chat history
                if let Some(ref store) = self.chat_history {
                    use crate::memory::ChatMessage;
                    let chat_msg =
                        ChatMessage::new(&conversation_id, &user_id, "assistant", &cached.response);
                    if let Err(e) = store.store_message(chat_msg).await {
                        error!("Failed to store assistant message: {}", e);
                    }
                }

                // Return cached response
                return Ok(OutgoingMessage::new(
                    crate::channels::ConversationId(conversation_id),
                    cached.response.clone(),
                ));
            }
        }

        // Store the user message across memory/history/search/transcript
        self.persist_user_message(&conversation_id, &user_id, &content)
            .await;
        // Check if we need task planning
        let needs_planning = self.task_planner.needs_planning(&content).await;

        if needs_planning {
            info!("Complex task detected, creating plan for: {}", conversation_id);

            // Create a plan
            match self.task_planner.create_plan(&content).await {
                Ok(plan) => {
                    let summary = plan.format_summary();
                    info!("Created plan with {} tasks", plan.tasks.len());

                    // Plan turns return before any LLM round, so they produce
                    // no round record. Persist a lightweight turn record whose
                    // whole observable payload is the plan DAG snapshot (closes
                    // the §五 planner blind spot).
                    let mut collector = TurnMetricsCollector::new(TurnContext {
                        session_id: self.session_id.clone(),
                        conversation_id: conversation_id.clone(),
                        agent_id: self.agent_id.clone(),
                        thread_id: format!("thread-{}", conversation_id),
                        turn_index: 0,
                        user_message: content.clone(),
                        enqueued_at: None,
                    })
                    .with_metrics_sink(
                        self.session_store
                            .clone()
                            .map(|s| -> Arc<dyn TurnMetricsSink> { s }),
                    );
                    collector
                        .record_plan_snapshot(crate::observe::record::PlanSnapshot::from(&plan));
                    collector.finish(&summary).await;

                    // Convert to todos
                    let todos = self.task_planner.plan_to_todos(&plan);

                    // Store active plan
                    let active_plan = ActivePlan {
                        plan,
                        todos,
                        completed_tasks: Vec::new(),
                    };

                    let mut plans = self.active_plans.write().await;
                    plans.insert(conversation_id.clone(), active_plan);
                    drop(plans);

                    // Persist the plan if plans_dir is configured
                    if let Some(ref dir) = self.plans_dir {
                        let plans = self.active_plans.read().await;
                        if let Some(active) = plans.get(&conversation_id) {
                            let snapshot = PersistedPlan::from_active(active);
                            let path = dir.join(format!("{}.json", conversation_id));
                            if let Err(e) = snapshot.persist_to(&path).await {
                                warn!("Failed to persist plan: {}", e);
                            }
                        }
                    }

                    // Return the plan to the user
                    return Ok(OutgoingMessage::new(
                        crate::channels::ConversationId(conversation_id),
                        format!("I'll break this down into steps:\n\n{}", summary),
                    ));
                }
                Err(e) => {
                    warn!("Failed to create plan: {}, proceeding without planning", e);
                }
            }
        }

        // ── Per-conversation concurrency guard ──────────────────────────────
        // Prevents reentrant processing: if a second message arrives for the
        // same conversation_id while one is in-flight, it waits here.
        let sem = {
            let mut guards = self.concurrency_guards.lock().await;
            guards
                .entry(conversation_id.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(1)))
                .clone()
        };
        let _permit = match sem.acquire().await {
            Ok(p) => p,
            Err(_) => {
                return Err(crate::error::SyscityError::Internal(
                    "concurrency semaphore closed".into(),
                ));
            }
        };

        // ── Thread take-out (panic-safe via ThreadGuard) ────────────────────
        // ThreadGuard reinserts the thread into thread_map on Drop, preventing
        // thread loss if processing panics between take-out and reinsertion.
        let mut guard = ThreadGuard::take(&self.thread_map, &conversation_id).await;
        if guard.thread.is_none() {
            // First message for this conversation — build initial Context.
            let ctx = self
                .build_fresh_context(&conversation_id, &user_id, &content)
                .await;
            let thread_id = format!("thread-{}", conversation_id);
            // Persist the new thread record (fire-and-forget).
            if let (Some(store), Some(sid)) = (self.session_store.clone(), self.session_id.clone())
            {
                let tid = thread_id.clone();
                let label = conversation_id.clone();
                tokio::spawn(async move {
                    if let Err(e) = store
                        .save_thread(&sid, &tid, &label, chrono::Utc::now().timestamp_millis())
                        .await
                    {
                        warn!("Failed to persist thread {} for session {}: {}", tid, sid, e);
                    }
                });
            }
            guard.thread = Some(Thread::from_context(thread_id, &conversation_id, ctx));
        }
        let thread = guard.get_mut();
        // Safe: from here on, guard.thread is always Some until into_thread().

        // Apply ACP max iteration override for existing threads
        let override_opt = *self.max_tool_iterations_override.read().await;
        if let Some(max_iter) = override_opt {
            thread.context.set_max_tool_iterations(max_iter);
            info!(
                "Applied ACP max iteration override to existing thread: {} for conversation {}",
                max_iter, conversation_id
            );
        }

        // Reset tool tracking and add user message for this turn.
        thread.context.clear_tools_used();
        let model_content = match &mentions_block {
            Some(block) => format!("{}\n\n{}", content, block),
            None => content.clone(),
        };
        thread
            .context
            .add_message(Message::user_named(&user_id, &model_content));

        // Track this turn in the turn log.
        let turn_idx = thread.push_turn(&content);
        thread.turns[turn_idx].start();

        // Check if we're executing an active plan
        let active_plan_check = {
            let plans = self.active_plans.read().await;
            plans.get(&conversation_id).map(|p| {
                (p.plan.progress_percent(), p.plan.current_task().map(|t| t.description.clone()))
            })
        };

        if let Some((progress, Some(current_task))) = active_plan_check {
            info!("Executing plan: {}% - Task: {}", progress, current_task);
        }

        // Get response from LLM (lock NOT held during this await).
        let llm_result = self
            .get_completion(&mut thread.context, &message.user_id.0)
            .await;

        // Complete or interrupt the turn based on result.
        let llm_result = match llm_result {
            Ok(resp) => {
                let asst_text = resp.message.content.clone();
                thread.turns[turn_idx].complete(asst_text.clone());
                // Persist the turn asynchronously (fire-and-forget).
                if let (Some(store), Some(sid)) =
                    (self.session_store.clone(), self.session_id.clone())
                {
                    let tid = thread.id.clone();
                    let user_c = content.clone();
                    let t_idx = turn_idx as i64;
                    tokio::spawn(async move {
                        if let Err(e) = store
                            .append_turn(&sid, &tid, t_idx, &user_c, &asst_text, "complete", None)
                            .await
                        {
                            warn!("Failed to persist turn {} for session {}: {}", t_idx, sid, e);
                        }
                    });
                }
                Ok(resp)
            }
            Err(e) => {
                thread.turns[turn_idx].mark_error();
                Err(e)
            }
        };

        // Collect tools_used BEFORE putting thread back (needed for cache logic below).
        let tools_used_this_turn = thread.context.tools_used().to_vec();

        // ── Put thread back ───────────────────────────────────────────────────
        {
            let mut map = self.thread_map.lock().await;
            map.insert(conversation_id.clone(), guard.into_thread());
        }

        let response = llm_result?;

        // Mark memory hits based on response content
        if let Some(ref mm) = self.memory_manager {
            let session_key = format!("{}:{}", user_id, conversation_id);
            mm.evaluate_response_hits(&session_key, &response.message.content)
                .await;
            // Close the effectiveness feedback loop
            mm.apply_effectiveness_adjustments().await;
        }

        // Store the assistant response across transcript/memory/history/search
        self.persist_assistant_message(&conversation_id, &user_id, &response.message.content)
            .await;
        // Only cache the response if it should be cached
        if should_cache {
            // Check if tools used are cacheable (skip cache for time-sensitive tools)
            if are_tools_cacheable(&tools_used_this_turn) {
                self.response_cache
                    .set(
                        &user_id,
                        &conversation_id,
                        &cache_key_content,
                        response.message.content.clone(),
                        tools_used_this_turn,
                    )
                    .await;
            }
        }

        // ── PII output filtering ─────────────────────────────────────────────
        let filtered_content = if let Some(ref detector) = self.pii_detector {
            match detector.filter_response(&response.message.content) {
                crate::security::FilterResult::Clean(text) => text,
                crate::security::FilterResult::Redacted(text, findings) => {
                    tracing::info!(
                        "Redacted {} PII findings from response for conversation {}",
                        findings.len(),
                        conversation_id
                    );
                    text
                }
                crate::security::FilterResult::Blocked(findings) => {
                    let restricted_count = findings
                        .iter()
                        .filter(|f| {
                            f.classification == crate::security::DataClassification::Restricted
                        })
                        .count();
                    tracing::warn!(
                        "Blocked response containing {} restricted PII items for conversation {}",
                        restricted_count,
                        conversation_id
                    );
                    "⚠️ This response contains sensitive personal information and has been \
                     blocked. Please review the content before sharing."
                        .to_string()
                }
            }
        } else {
            response.message.content.clone()
        };

        // Create outgoing message with usage tracking
        let mut outgoing = OutgoingMessage::new(
            crate::channels::ConversationId(conversation_id),
            filtered_content,
        );
        if let Some(ref usage) = response.usage {
            outgoing.usage = Some(*usage);
        }

        // ── Note: trajectory reflection (retrospect) runs in
        // process_message_with_progress ──

        Ok(outgoing)
    }
    pub async fn process_message_with_progress(
        &self,
        message: IncomingMessage,
        progress_cb: ProgressCallback,
    ) -> crate::Result<OutgoingMessage> {
        debug!("Processing message with progress from user: {}", message.user_id);

        let cfg = self.config_snapshot();

        let conversation_id = message.conversation_id.0.clone();
        let user_id = message.user_id.0.clone();
        let content = message.content.clone();

        // Composer chips (skills/agents) ride the message metadata; they are
        // appended to the model-facing user message only (never persisted).
        let mentions = crate::channels::mentions_from_extra(&message.metadata.extra);
        let mentions_block = mentions
            .as_ref()
            .and_then(crate::channels::format_mentions_block);
        // Mentions participate in the cache key so identical texts with
        // different chips don't share a cached response.
        let cache_key_content = match &mentions_block {
            Some(block) => format!("{}\u{0}{}", content, block),
            None => content.clone(),
        };

        // Notify started
        (progress_cb)(ProgressEvent::Started).await;

        // ── Prompt-injection guard ────────────────────────────────────────────
        let input_scan = crate::skills::guard::scan_input(&content);
        if !input_scan.passed {
            warn!("Blocked suspicious input from user {}: {:?}", user_id, input_scan.issues);
            let rejection = "I'm unable to process this request as it contains potentially unsafe \
                             content. If you believe this is a mistake, please rephrase your \
                             message."
                .to_string();
            // Guard rejection happens before a turn collector exists, so the
            // turn_id is empty (no persisted turn to vote on).
            (progress_cb)(ProgressEvent::Completed {
                response: rejection.clone(),
                turn_id: String::new(),
                usage: None,
            })
            .await;
            return Ok(OutgoingMessage::new(
                crate::channels::ConversationId(conversation_id),
                rejection,
            ));
        }

        // ── New-turn reset ────────────────────────────────────────────────────
        // A new user turn clears the previous turn's active plan/todo list.
        self.begin_user_turn_reset(&conversation_id).await;

        // Decide whether to attempt a response-cache lookup (follow-ups bypass it).
        let should_cache = self.should_attempt_cache(&content).await;

        if should_cache {
            if let Some(cached) = self
                .response_cache
                .get(&user_id, &conversation_id, &cache_key_content)
                .await
            {
                info!("Cache hit for user {} - returning cached response", user_id);

                // Notify cache hit
                (progress_cb)(ProgressEvent::ToolCalling {
                    name: "cache".to_string(),
                    arguments: "{\"hit\": true}".to_string(),
                })
                .await;

                // Store user message in chat history
                if let Some(ref store) = self.chat_history {
                    use crate::memory::ChatMessage;
                    let chat_msg = ChatMessage::new(&conversation_id, &user_id, "user", &content);
                    if let Err(e) = store.store_message(chat_msg).await {
                        error!("Failed to store user message: {}", e);
                    }
                }

                // Store cached assistant response in chat history
                if let Some(ref store) = self.chat_history {
                    use crate::memory::ChatMessage;
                    let chat_msg =
                        ChatMessage::new(&conversation_id, &user_id, "assistant", &cached.response);
                    if let Err(e) = store.store_message(chat_msg).await {
                        error!("Failed to store assistant message: {}", e);
                    }
                }

                // Record the cache hit as a completed turn with no LLM rounds so
                // cache-hit rate / cost statistics stay accurate. The collector
                // must be created BEFORE emitting Completed so the cache-hit
                // turn has a stable turn_id for feedback.vote.
                let mut cache_collector = TurnMetricsCollector::new(TurnContext {
                    session_id: self.session_id.clone(),
                    conversation_id: conversation_id.clone(),
                    agent_id: self.agent_id.clone(),
                    thread_id: format!("thread-{}", conversation_id),
                    turn_index: 0,
                    user_message: content.clone(),
                    enqueued_at: Some(message.metadata.timestamp),
                })
                .with_metrics_sink(
                    self.session_store
                        .clone()
                        .map(|s| -> Arc<dyn TurnMetricsSink> { s }),
                );
                let cache_turn_id = cache_collector.turn_id().to_string();

                // Notify completed with cached response
                (progress_cb)(ProgressEvent::Completed {
                    response: cached.response.clone(),
                    turn_id: cache_turn_id.clone(),
                    usage: None,
                })
                .await;
                cache_collector.mark_cache_hit();
                cache_collector.finish(&cached.response).await;

                // Online risk scan for the cache-hit turn (no tools ran).
                // Cache hits perform no compaction, so there are no
                // compression-quality risks to carry.
                self.scan_turn_for_badcase(
                    &content,
                    &cached.response,
                    0,
                    &cache_turn_id,
                    &conversation_id,
                    Vec::new(),
                );

                // Return cached response
                return Ok(OutgoingMessage::new(
                    crate::channels::ConversationId(conversation_id),
                    cached.response.clone(),
                ));
            }
        }

        // Store the user message across memory/history/search/transcript
        self.persist_user_message(&conversation_id, &user_id, &content)
            .await;
        // ── Per-conversation concurrency guard ──────────────────────────────
        let sem = {
            let mut guards = self.concurrency_guards.lock().await;
            guards
                .entry(conversation_id.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(1)))
                .clone()
        };
        let _permit = match sem.acquire().await {
            Ok(p) => p,
            Err(_) => {
                return Err(crate::error::SyscityError::Internal(
                    "concurrency semaphore closed".into(),
                ));
            }
        };

        // ── Thread take-out (panic-safe via ThreadGuard) ────────────────────
        let mut guard = ThreadGuard::take(&self.thread_map, &conversation_id).await;
        if guard.thread.is_none() {
            let ctx = self
                .build_fresh_context(&conversation_id, &user_id, &content)
                .await;
            let thread_id = format!("thread-{}", conversation_id);
            if let (Some(store), Some(sid)) = (self.session_store.clone(), self.session_id.clone())
            {
                let tid = thread_id.clone();
                let label = conversation_id.clone();
                tokio::spawn(async move {
                    if let Err(e) = store
                        .save_thread(&sid, &tid, &label, chrono::Utc::now().timestamp_millis())
                        .await
                    {
                        warn!("Failed to persist thread {} for session {}: {}", tid, sid, e);
                    }
                });
            }
            guard.thread = Some(Thread::from_context(thread_id, &conversation_id, ctx));
        }
        let thread = guard.get_mut();

        // Apply delegation scope from message metadata (delegated child agents).
        // Applied after the thread is available so both freshly built and resumed
        // delegation conversations get the scope before any LLM call.
        if let Some(scope_value) = message
            .metadata
            .extra
            .get(crate::delegation::DELEGATION_SCOPE_KEY)
        {
            match serde_json::from_value::<crate::delegation::DelegationScope>(scope_value.clone())
            {
                Ok(scope) => {
                    thread.context.set_delegation(Some(scope.clone()));
                    if let Some(max_iter) = scope.max_iterations {
                        thread.context.set_max_tool_iterations(max_iter);
                    }
                    info!(
                        delegation_root = %scope.root_id,
                        delegation_task = %scope.task_id,
                        depth = scope.depth,
                        "Applied delegation scope to conversation {}",
                        conversation_id
                    );
                }
                Err(e) => {
                    warn!(
                        "Failed to parse delegation scope for conversation {}: {}",
                        conversation_id, e
                    );
                }
            }
        }

        // Apply ACP max iteration override for existing threads
        let override_opt = *self.max_tool_iterations_override.read().await;
        if let Some(max_iter) = override_opt {
            thread.context.set_max_tool_iterations(max_iter);
            info!(
                "Applied ACP max iteration override to existing thread: {} for conversation {}",
                max_iter, conversation_id
            );
        }

        // Reset tool tracking and add user message for this turn.
        thread.context.clear_tools_used();
        let model_content = match &mentions_block {
            Some(block) => format!("{}\n\n{}", content, block),
            None => content.clone(),
        };
        thread
            .context
            .add_message(Message::user_named(&user_id, &model_content));

        // Track this turn.
        let turn_idx = thread.push_turn(&content);
        thread.turns[turn_idx].start();

        // Per-turn observability collector (persisted on finish/fail/abort).
        let mut collector = TurnMetricsCollector::new(TurnContext {
            session_id: self.session_id.clone(),
            conversation_id: conversation_id.clone(),
            agent_id: self.agent_id.clone(),
            thread_id: thread.id.clone(),
            turn_index: turn_idx,
            user_message: content.clone(),
            enqueued_at: Some(message.metadata.timestamp),
        })
        .with_metrics_sink(
            self.session_store
                .clone()
                .map(|s| -> Arc<dyn TurnMetricsSink> { s }),
        )
        .with_min_retention_ratio(self.compression_quality.min_retention_ratio);

        // Capture the stable turn id BEFORE the collector is consumed by
        // finish()/fail()/abort(); it is threaded into the Completed event.
        let turn_id = collector.turn_id().to_string();

        // Drain the inbound channel-layer observation (debounce/enrich/route)
        // attached by the gateway dispatch layer, if the message carried one.
        if let Some(obs) = message
            .metadata
            .extra
            .get("channel_observation")
            .and_then(|v| serde_json::from_value::<ChannelObservation>(v.clone()).ok())
        {
            collector.record_channel(obs);
        }

        // Start-of-turn timestamp for online sampling latency. Captured before
        // the LLM round so `latency_ms` reflects generation time.
        let sampling_started = std::time::Instant::now();

        // Get response from LLM with progress (lock NOT held).
        let llm_result = self
            .get_completion_with_progress(
                &mut thread.context,
                &mut collector,
                progress_cb.clone(),
                &message.user_id.0,
            )
            .await;

        // Compression-quality risk signals (§三). The compaction observations
        // are recorded during the LLM loop, so capture them BEFORE the collector
        // is consumed by finish()/abort()/fail() below. Gated by
        // `compression_quality.enabled` so default configs collect nothing.
        let compression_risks = if self.compression_quality.enabled {
            collector.compression_risks()
        } else {
            Vec::new()
        };

        // Sampling metadata snapshots, filled inside the `Ok(resp)` arm below
        // BEFORE the collector is consumed by finish()/abort(). Declared up
        // here so they remain in scope at the `sample_turn` call site after the
        // match block.
        let mut sampling_model = String::new();
        let mut sampling_cache_hit = false;
        let mut sampling_usage = None;

        // Complete or interrupt the turn.
        let llm_result = match llm_result {
            Ok(resp) => {
                let asst_text = resp.message.content.clone();
                thread.turns[turn_idx].complete(asst_text.clone());
                if let (Some(store), Some(sid)) =
                    (self.session_store.clone(), self.session_id.clone())
                {
                    let tid = thread.id.clone();
                    let user_c = content.clone();
                    let t_idx = turn_idx as i64;
                    let asst_text_spawn = asst_text.clone();
                    let turn_id_spawn = turn_id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = store
                            .append_turn(
                                &sid,
                                &tid,
                                t_idx,
                                &user_c,
                                &asst_text_spawn,
                                "complete",
                                Some(&turn_id_spawn),
                            )
                            .await
                        {
                            warn!("Failed to persist turn {} for session {}: {}", t_idx, sid, e);
                        }
                    });
                }
                // Snapshot sampling metadata BEFORE the collector is consumed
                // by finish(): the model and cache-hit flag only live inside
                // the collector, and usage lives on the response.
                sampling_model = if collector.model().is_empty() {
                    resp.model.clone()
                } else {
                    collector.model().to_string()
                };
                sampling_cache_hit = collector.cache_hit();
                sampling_usage = resp.usage;

                // Controller checkpoints return Ok with finish_reason="cancelled";
                // treat that as an abort, not a successful completion.
                if resp.finish_reason.as_deref() == Some("cancelled") {
                    collector.abort().await;
                } else {
                    collector.finish(&asst_text).await;
                }
                Ok(resp)
            }
            Err(e) => {
                thread.turns[turn_idx].mark_error();
                collector.fail(ErrorSource::Llm, &e.to_string()).await;
                Err(e)
            }
        };

        // Transfer accumulated tool call records and token usage from context to this
        // turn
        let tool_records = thread.context.take_tool_call_records();
        if !tool_records.is_empty() {
            thread.turns[turn_idx].tool_calls = tool_records;
        }
        thread.turns[turn_idx].token_usage = thread.context.take_turn_token_usage();

        let tools_used_this_turn = thread.context.tools_used().to_vec();

        // Snapshot turns for retrospect engine before moving the thread.
        let retrospect_turns: Vec<crate::agent::turns::Turn> = thread.turns.clone();
        let retrospect_turn_count = thread.turn_count();

        // ── Put thread back ───────────────────────────────────────────────────
        {
            let mut map = self.thread_map.lock().await;
            map.insert(conversation_id.clone(), guard.into_thread());
        }

        let response = llm_result?;

        // Store the assistant response across transcript/memory/history/search
        self.persist_assistant_message(&conversation_id, &user_id, &response.message.content)
            .await;

        // Only cache the response if it should be cached
        let tool_call_count = tools_used_this_turn.len();
        if should_cache && are_tools_cacheable(&tools_used_this_turn) {
            self.response_cache
                .set(
                    &user_id,
                    &conversation_id,
                    &cache_key_content,
                    response.message.content.clone(),
                    tools_used_this_turn,
                )
                .await;
        }

        // Notify completed
        let response_content = response.message.content.clone();
        (progress_cb)(ProgressEvent::Completed {
            response: response_content.clone(),
            turn_id: turn_id.clone(),
            usage: response.usage,
        })
        .await;

        // ── Online risk scan: auto-collect badcases into the pending pool ──
        self.scan_turn_for_badcase(
            &content,
            &response_content,
            tool_call_count,
            &turn_id,
            &conversation_id,
            compression_risks,
        );

        // ── Production turn sampling: persist a sampled subset of turns ──
        // Fire-and-forget, mirroring `scan_turn_for_badcase`. Guard-rejection
        // turns (empty turn_id) are skipped inside `sample_turn`.
        self.sample_turn(
            &content,
            &response_content,
            tool_call_count,
            &turn_id,
            &conversation_id,
            sampling_model,
            sampling_cache_hit,
            sampling_usage
                .as_ref()
                .map(|u| u.total_tokens as u64)
                .unwrap_or(0),
            sampling_started.elapsed().as_millis() as u64,
        );

        // Create outgoing message with full metadata
        let mut outgoing = OutgoingMessage::new(
            crate::channels::ConversationId(conversation_id),
            response_content,
        );
        if let Some(ref reasoning) = response.message.reasoning_content {
            if !reasoning.is_empty() {
                outgoing.reasoning_content = Some(reasoning.clone());
            }
        }
        if let Some(ref calls) = response.message.tool_calls {
            if !calls.is_empty() {
                outgoing.tool_calls = Some(calls.clone());
            }
        }
        outgoing.usage = response.usage;

        // ── Retrospect: background trajectory reflection ─────────────────
        if let Some(ref engine) = self.retrospect_engine {
            let counter = self
                .retrospect_counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            let interval = engine.config.interval as u64;
            let min_turns = engine.config.min_turns as u64;

            if counter >= min_turns && counter.is_multiple_of(interval) {
                let engine = engine.clone();
                let mm = self.memory_manager.clone();
                let uid = user_id.clone();
                let criteria = cfg
                    .reflection_config
                    .as_ref()
                    .map(|rc| rc.criteria.clone())
                    .unwrap_or_default();
                let turns = retrospect_turns.clone();
                let total = retrospect_turn_count;

                tokio::spawn(async move {
                    match engine.retrospect(&turns, total, &criteria).await {
                        Ok(retrospect_result) => {
                            info!(
                                "Retrospect trajectory reflection at turn {}: {}",
                                retrospect_result.turn_count, retrospect_result.observation
                            );

                            // Compute dynamic importance from critique scores
                            let importance =
                                crate::agent::reflection::critic::compute_retrospect_importance(
                                    &retrospect_result.critique,
                                );

                            // Build metadata from critique for richer memory browsing
                            let metadata = serde_json::json!({
                                "turn_count": retrospect_result.turn_count,
                                "dimension_scores": retrospect_result.critique.dimension_scores,
                                "weaknesses": retrospect_result.critique.weaknesses,
                                "suggested_improvements": retrospect_result.critique.suggested_improvements,
                            });

                            // Write observation to memory.
                            if let Some(ref mm) = mm {
                                if let Err(e) = mm
                                    .observe_with_metadata(
                                        &uid,
                                        retrospect_result.observation,
                                        "interaction_pattern",
                                        importance,
                                        metadata,
                                    )
                                    .await
                                {
                                    warn!("Failed to persist interaction pattern: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Retrospect trajectory reflection failed: {}", e);
                        }
                    }
                });
            }
        }

        Ok(outgoing)
    }

    /// Attach the 在线质量监控（§八）config to this agent.
    /// Persist the user message across all side channels (episodic memory,
    /// chat history, session-search index, transcript/disk budget) best-effort.
    /// Shared by `process_message` and `process_message_with_progress`.
    async fn persist_user_message(&self, conversation_id: &str, user_id: &str, content: &str) {
        // Persist user message via MemoryManager (episodic memory)
        if let Some(ref mm) = self.memory_manager {
            if let Err(e) = mm
                .remember_message(user_id, conversation_id, "user", content)
                .await
            {
                warn!("MemoryManager: failed to store user message: {}", e);
            }
        }

        // Store user message in chat history and index for search
        let message_id = uuid::Uuid::new_v4().to_string();
        if let Some(ref store) = self.chat_history {
            use crate::memory::ChatMessage;
            let chat_msg = ChatMessage::new(conversation_id, user_id, "user", content);
            // Clone message_id before moving chat_msg
            let msg_id = chat_msg.id.clone();
            if let Err(e) = store.store_message(chat_msg).await {
                error!("Failed to store user message: {}", e);
            }
            if let Some(ref search) = self.session_search {
                if let Err(e) = search
                    .index_message(&msg_id, conversation_id, user_id, content, "user")
                    .await
                {
                    error!("Failed to index user message for search: {}", e);
                }
            }
        } else if let Some(ref search) = self.session_search {
            if let Err(e) = search
                .index_message(&message_id, conversation_id, user_id, content, "user")
                .await
            {
                error!("Failed to index user message for search: {}", e);
            }
        }

        // Record user message in transcript
        if let Some(ref transcript_store) = self.transcript_store {
            transcript_store.append(
                conversation_id,
                "agent",
                user_id,
                conversation_id,
                TranscriptMessage::new("user", content),
            );
            // Track transcript size in disk budget
            if let Some(ref budget) = self.disk_budget {
                if let Err(e) = budget.track_item(
                    conversation_id,
                    format!("transcript-user-{}", message_id),
                    BudgetCategory::Transcript,
                    content.len(),
                ) {
                    warn!("Failed to track user transcript in disk budget: {}", e);
                }
            }
        }
    }

    /// Persist the assistant response across all side channels (transcript,
    /// episodic memory, chat history, session-search index) best-effort.
    /// Shared by `process_message` and `process_message_with_progress`.
    async fn persist_assistant_message(&self, conversation_id: &str, user_id: &str, content: &str) {
        let assistant_message_id = uuid::Uuid::new_v4().to_string();

        // Record assistant message in transcript
        if let Some(ref transcript_store) = self.transcript_store {
            transcript_store.append(
                conversation_id,
                "agent",
                user_id,
                conversation_id,
                TranscriptMessage::new("assistant", content),
            );
            // Track transcript size in disk budget
            if let Some(ref budget) = self.disk_budget {
                if let Err(e) = budget.track_item(
                    conversation_id,
                    format!("transcript-assistant-{}", assistant_message_id),
                    BudgetCategory::Transcript,
                    content.len(),
                ) {
                    warn!("Failed to track assistant transcript in disk budget: {}", e);
                }
            }
        }

        // Persist assistant response via MemoryManager (episodic memory)
        if let Some(ref mm) = self.memory_manager {
            if let Err(e) = mm
                .remember_message(user_id, conversation_id, "assistant", content)
                .await
            {
                warn!("MemoryManager: failed to store assistant message: {}", e);
            }
        }

        if let Some(ref store) = self.chat_history {
            use crate::memory::ChatMessage;
            let chat_msg = ChatMessage::new(conversation_id, user_id, "assistant", content);
            let msg_id = chat_msg.id.clone();
            if let Err(e) = store.store_message(chat_msg).await {
                error!("Failed to store assistant message: {}", e);
            }
            // Index for session search
            if let Some(ref search) = self.session_search {
                if let Err(e) = search
                    .index_message(&msg_id, conversation_id, user_id, content, "assistant")
                    .await
                {
                    error!("Failed to index assistant message for search: {}", e);
                }
            }
        } else if let Some(ref search) = self.session_search {
            // Even if chat history is not enabled, index for search
            if let Err(e) = search
                .index_message(
                    &assistant_message_id,
                    conversation_id,
                    user_id,
                    content,
                    "assistant",
                )
                .await
            {
                error!("Failed to index assistant message for search: {}", e);
            }
        }
    }

    /// Whether this user message is a short follow-up that should bypass the
    /// response cache (references prior context).
    fn is_follow_up(content: &str) -> bool {
        content.len() < 50
            && (content.contains("it")
                || content.contains("that")
                || content.contains("this")
                || content.contains("上面的")
                || content.contains("这个")
                || content.contains("那个"))
    }

    /// Decide whether to attempt a response-cache lookup for this turn.
    async fn should_attempt_cache(&self, content: &str) -> bool {
        !Self::is_follow_up(content)
            && should_use_cache_llm(&self.provider, content, self.model.clone()).await
    }

    ///
    /// Snapshot from `GatewayConfig.eval.online_monitoring` at spawn time; the
    /// judge trigger in [`scan_turn_for_badcase`] reads it without holding any
    /// lock across an await.
    pub fn with_online_monitoring(
        mut self,
        online_monitoring: crate::gateway::config::OnlineMonitoringConfig,
    ) -> Self {
        self.online_monitoring = online_monitoring;
        self
    }
}
