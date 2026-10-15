//! Lifecycle operations: start/self-repair, thread management, undo/redo,
//! shutdown, health, and artifact extraction.

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::providers::Message;
use crate::tools::ToolRegistry;

use super::agent_cache::{RE_CODE_BLOCK, RE_URL};
use super::planner::ActivePlanView;
use super::*;

impl Agent {
    /// Start the agent (for background processing if needed)
    pub async fn start(&self) -> crate::Result<()> {
        info!("Starting agent");
        // Agent is mostly stateless, but this could be used for background tasks
        Ok(())
    }

    /// Spawn a background self-repair task.
    ///
    /// Every `check_interval` the task:
    /// 1. Evicts contexts that have been inactive longer than
    ///    `stale_threshold`.
    /// 2. Logs and reports any tools that are currently circuit-broken.
    ///
    /// The task runs until the `Agent` is dropped.
    pub fn start_self_repair_loop(
        &self,
        check_interval: Duration,
        stale_threshold: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let thread_map = Arc::clone(&self.thread_map);
        let tools = Arc::clone(&self.tools);

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(check_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                ticker.tick().await;

                // ── 1. Evict stale threads ────────────────────────────────────
                let stale_ids: Vec<String> = {
                    let guard = thread_map.lock().await;
                    guard
                        .iter()
                        .filter(|(_, t)| t.context.is_stale(stale_threshold))
                        .map(|(id, _)| id.clone())
                        .collect()
                };

                if !stale_ids.is_empty() {
                    let mut guard = thread_map.lock().await;
                    for id in &stale_ids {
                        guard.remove(id);
                        warn!(
                            conversation_id = id.as_str(),
                            "Self-repair: evicted stale context (inactive >{:?})", stale_threshold
                        );
                    }
                }

                // ── 2. Report degraded tools ──────────────────────────────────
                let degraded = tools.degraded_tools();
                if !degraded.is_empty() {
                    warn!(
                        tools = ?degraded,
                        "Self-repair: {} tool(s) are circuit-broken",
                        degraded.len()
                    );
                }
            }
        })
    }

    /// Return a summary of all active threads:
    /// `(thread_id, label, turn_count, conversation_id)`.
    pub async fn thread_summaries(&self) -> Vec<(String, String, usize, String)> {
        let map = self.thread_map.lock().await;
        map.iter()
            .map(|(conv_id, t)| (t.id.clone(), t.label.clone(), t.turns.len(), conv_id.clone()))
            .collect()
    }

    /// Return turn details for a conversation, identified by its
    /// `conversation_id` (the `thread_map` key).
    ///
    /// Each element is `(index, state_str, user_preview, asst_preview)`.
    /// Returns `None` if no thread exists for that conversation.
    pub async fn thread_turns_for(
        &self,
        conv_id: &str,
    ) -> Option<Vec<(usize, String, String, String)>> {
        let map = self.thread_map.lock().await;
        map.get(conv_id).map(|t| {
            t.turns
                .iter()
                .map(|turn| {
                    let state = format!("{:?}", turn.state).to_lowercase();
                    let user_preview: String = turn.user_message.chars().take(80).collect();
                    let asst_preview: String = turn.assistant_response.chars().take(80).collect();
                    (turn.index, state, user_preview, asst_preview)
                })
                .collect()
        })
    }

    /// Return context assembly info for a conversation.
    ///
    /// Returns `(message_count, token_count, max_tokens, system_prompt_len,
    /// tool_iterations)` or `None` if the thread is not found.
    pub async fn context_info(&self, conv_id: &str) -> Option<(usize, usize, usize, usize, usize)> {
        let map = self.thread_map.lock().await;
        map.get(conv_id).map(|t| {
            (
                t.context.message_count(),
                t.context.token_count(),
                t.context.max_context_tokens(),
                t.context.system_prompt().len(),
                t.context.tool_iterations(),
            )
        })
    }

    /// Compact the context for a conversation using the Summarize strategy.
    ///
    /// Returns `(before_message_count, after_message_count)` or `None` if the
    /// thread is not found or no compaction was needed.
    pub async fn compact_context(&self, conv_id: &str) -> Option<(usize, usize)> {
        let mut map = self.thread_map.lock().await;
        map.get_mut(conv_id).map(|thread| {
            let messages = thread.context.to_messages();
            let before = messages.len();
            let target = thread.context.max_context_tokens() / 2;
            let compressor =
                ContextCompressor::new(target).with_strategy(CompressionStrategy::Summarize);
            let compressed = compressor.compress(&messages);
            let after = compressed.len();
            if after < before {
                thread.context.replace_messages(compressed);
            }
            (before, after)
        })
    }

    /// Branch a conversation: copy what it has so far under a new id, and
    /// persist the copy.
    ///
    /// The new branch is a *live* conversation — it has the parent's turns and
    /// message window, so continuing in it continues this conversation rather
    /// than starting a blank one — and it is written to the session store, so it
    /// survives a restart and appears in the session list like any other.
    ///
    /// Persistence is awaited rather than spawned: the point of the call is to
    /// create something durable, and reporting success before the rows exist
    /// would make a failed write look like a branch. The parent is not touched.
    ///
    /// Returns the number of turns copied.
    pub async fn fork_conversation(&self, from: &str, to: &str) -> crate::Result<usize> {
        let thread_id = format!("thread-{to}");

        // The parent, when this agent has it in memory; otherwise a context of
        // the kind a fresh conversation gets, since building one is the agent's
        // job (system prompt, tools, memories) and both are this agent's
        // conversations.
        let parent = {
            let map = self.thread_map.lock().await;
            map.get(from)
                .map(|thread| thread.fork(thread_id.clone(), to))
        };
        let forked = match parent {
            Some(forked) => forked,
            None => {
                let context = self.build_fresh_context(to, "branch", "").await;
                Thread::from_context(thread_id.clone(), to, context)
            }
        };

        if let (Some(store), Some(session_id)) =
            (self.session_store.as_ref(), self.session_id.as_deref())
        {
            store
                .save_thread(
                    session_id,
                    &thread_id,
                    &forked.label,
                    chrono::Utc::now().timestamp_millis(),
                )
                .await?;
            for turn in &forked.turns {
                store
                    .append_turn(
                        session_id,
                        &thread_id,
                        turn.index as i64,
                        &turn.user_message,
                        &turn.assistant_response,
                        turn_state_name(turn.state),
                        None,
                    )
                    .await?;
            }
        }

        let turns = forked.turns.len();
        self.thread_map.lock().await.insert(to.to_string(), forked);
        info!("Forked conversation {} into {} ({} turns)", from, to, turns);
        Ok(turns)
    }

    /// Whether a conversation has a plan in flight, and how far along it is.
    ///
    /// The plan itself is the agent's own bookkeeping — the model drives it
    /// through the `update_plan` tool — so this is the read side: what a client
    /// shows when someone asks what the agent thinks it is doing.
    pub async fn active_plan(&self, conversation_id: &str) -> Option<ActivePlanView> {
        let plans = self.active_plans.read().await;
        plans.get(conversation_id).map(ActivePlanView::from_active)
    }

    /// Undo the last turn for a conversation.
    ///
    /// Moves the most recent `Turn` from the turn log to the redo stack and
    /// strips the corresponding messages from the context window. If a
    /// `SessionStore` is attached the turn rows are also hard-deleted from
    /// SQLite (fire-and-forget).
    ///
    /// Returns `true` if a turn was undone, `false` if the thread was empty or
    /// not found.
    pub async fn undo_last_turn(&self, conversation_id: &str) -> bool {
        let mut map = self.thread_map.lock().await;
        if let Some(thread) = map.get_mut(conversation_id) {
            let last_idx = thread.turns.len().saturating_sub(1) as i64;
            let undone = thread.undo_last_turn();
            if undone {
                if let (Some(store), Some(sid)) =
                    (self.session_store.clone(), self.session_id.clone())
                {
                    let tid = thread.id.clone();
                    tokio::spawn(async move {
                        // Counted: shutdown waits for this write before closing storage.
                        let _owed = crate::agent::writes::pending().guard();
                        if let Err(e) = store.delete_turn(&sid, &tid, last_idx).await {
                            warn!("Failed to delete turn {} for session {}: {}", last_idx, sid, e);
                        }
                    });
                }
            }
            undone
        } else {
            false
        }
    }

    /// Redo the most recently undone turn for a conversation.
    ///
    /// Restores the turn from the redo stack back to the turn log and
    /// re-inserts its messages into the context window. Note: persistence
    /// is not supported for redo (the turn was deleted from SQLite on undo).
    ///
    /// Returns `true` if a turn was redone, `false` if the redo stack was empty
    /// or the thread was not found.
    pub async fn redo_last_turn(&self, conversation_id: &str) -> bool {
        let mut map = self.thread_map.lock().await;
        if let Some(thread) = map.get_mut(conversation_id) {
            thread.redo_last_turn()
        } else {
            false
        }
    }

    /// Returns `true` if the conversation can undo a turn.
    pub async fn can_undo(&self, conversation_id: &str) -> bool {
        let map = self.thread_map.lock().await;
        map.get(conversation_id)
            .map(|t| t.can_undo())
            .unwrap_or(false)
    }

    /// Returns `true` if the conversation can redo a turn.
    pub async fn can_redo(&self, conversation_id: &str) -> bool {
        let map = self.thread_map.lock().await;
        map.get(conversation_id)
            .map(|t| t.can_redo())
            .unwrap_or(false)
    }

    /// Restore threads from the `SessionStore` for the current `session_id`.
    ///
    /// This rebuilds each persisted `Thread` (system prompt + accumulated
    /// history) so conversation continuity survives a restart.  Call once
    /// during agent startup, after `with_session_store` has been configured.
    pub async fn restore_threads(&self) -> crate::Result<()> {
        let store = self
            .session_store
            .as_ref()
            .ok_or_else(|| crate::error::SyscityError::Internal("no session store".into()))?;
        let sid = self
            .session_id
            .as_deref()
            .ok_or_else(|| crate::error::SyscityError::Internal("no session id".into()))?;

        let thread_rows = store.load_threads_for_session(sid).await?;

        // Crash-recovery balance fix: a mid-turn crash can leave an assistant
        // tool call without a result row. Append a synthetic
        // TOOL_OUTCOME_UNKNOWN result per orphan before replaying, so the
        // derived history is balanced and the outcome is treated as unknown.
        // Idempotent — safe to run on every load.
        match store.repair_orphan_tool_calls(sid).await {
            Ok(0) => {}
            Ok(n) => info!("Balanced {} orphaned tool call(s) in session {}", n, sid),
            Err(e) => warn!("Failed to repair orphaned tool calls in session {}: {}", sid, e),
        }

        let mut map = self.thread_map.lock().await;

        for (tid, label, _created_ms, turns) in thread_rows {
            // Build a fresh context (system prompt, token limits) — history
            // is replayed via push_turn / complete below.
            let ctx = self.build_fresh_context(&tid, "restore", "").await;
            let mut thread = Thread::from_context(&tid, &label, ctx);
            for (_idx, user_msg, asst_msg, _state) in turns {
                let i = thread.push_turn(&user_msg);
                thread.context.add_message(Message::user(&user_msg));
                thread.turns[i].complete(asst_msg.clone());
                thread.context.add_message(Message::assistant(&asst_msg));
            }
            // Thread is keyed by conversation_id; the thread_id is "thread-{conv_id}".
            let conv_id = tid.trim_start_matches("thread-").to_string();
            map.insert(conv_id, thread);
        }

        info!("Restored {} thread(s) from session {}", map.len(), sid);
        Ok(())
    }

    /// Close a conversation and trigger compaction if eligible.
    ///
    /// Compaction is triggered when the session has accumulated more than 50
    /// turns OR is older than 7 days. Compaction extracts key facts from the
    /// conversation history into semantic memories via the MemoryManager.
    ///
    /// The thread is removed from `thread_map` regardless of compaction.
    /// Also flushes transcript and cleans up session files.
    pub async fn close_conversation(&self, conversation_id: &str) {
        const MAX_TURNS_BEFORE_COMPACT: usize = 50;
        const MAX_AGE_DAYS: u64 = 7;

        // Acquire the concurrency guard to prevent concurrent processing
        // while we remove the thread. Also cleans up the guard afterward.
        let semaphore = {
            let mut guards = self.concurrency_guards.lock().await;
            guards
                .entry(conversation_id.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(1)))
                .clone()
        };
        // Acquire the per-conversation semaphore to wait for any in-flight
        // process_message to complete before we remove the thread.
        let _permit = match semaphore.acquire().await {
            Ok(p) => p,
            Err(_) => {
                warn!("close_conversation: semaphore closed for {}", conversation_id);
                return;
            }
        };

        // Remove the concurrency guard entry (cleanup leak)
        {
            let mut guards = self.concurrency_guards.lock().await;
            guards.remove(conversation_id);
        }

        // Remove the thread from the map
        let thread_opt = {
            let mut map = self.thread_map.lock().await;
            map.remove(conversation_id)
        };

        let thread = match thread_opt {
            Some(t) => t,
            None => return, // Nothing to close
        };

        // Flush transcript to disk
        if let Some(ref transcript_store) = self.transcript_store {
            let store = transcript_store.clone();
            if let Err(e) = store.flush(conversation_id).await {
                warn!("Failed to flush transcript for {}: {}", conversation_id, e);
            } else {
                info!("Flushed transcript for {}", conversation_id);
            }
        }

        // Cleanup session files
        if let Some(ref file_manager) = self.session_file_manager {
            if let Err(e) = file_manager.cleanup_session(conversation_id).await {
                warn!("Failed to cleanup session files for {}: {}", conversation_id, e);
            } else {
                info!("Cleaned up session files for {}", conversation_id);
            }
        }

        // Clear disk budget tracking for this session
        if let Some(ref budget) = self.disk_budget {
            budget.clear_session(conversation_id);
        }

        // Remove the active plan for this conversation to prevent memory leak
        {
            let mut plans = self.active_plans.write().await;
            plans.remove(conversation_id);
        }

        // Determine if compaction is needed
        let age_secs = thread.created_at.elapsed().unwrap_or_default().as_secs();
        let too_old = age_secs > MAX_AGE_DAYS * 86_400;
        let too_long = thread.turn_count() > MAX_TURNS_BEFORE_COMPACT;

        if too_old || too_long {
            if let Some(mm) = self.memory_manager.clone() {
                let conv_id = conversation_id.to_string();
                tokio::spawn(async move {
                    // Counted: shutdown waits for this write before closing storage.
                    let _owed = crate::agent::writes::pending().guard();
                    match mm.compact_session(&conv_id, None).await {
                        Ok(ids) => {
                            info!("Session {} compacted: {} facts extracted", conv_id, ids.len());
                        }
                        Err(e) => {
                            warn!("Session compaction failed for {}: {}", conv_id, e);
                        }
                    }
                });
            }
        } else {
            debug!(
                "Session {} closed without compaction ({} turns, {} days old)",
                conversation_id,
                thread.turn_count(),
                age_secs / 86_400
            );
        }
    }

    /// Shutdown the agent, compacting all active sessions.
    pub async fn shutdown(&self) -> crate::Result<()> {
        info!("Shutting down agent");

        // Compact all open sessions before shutting down
        let conversation_ids: Vec<String> = {
            let map = self.thread_map.lock().await;
            map.keys().cloned().collect()
        };
        for conv_id in conversation_ids {
            self.close_conversation(&conv_id).await;
        }

        if let Some(tx) = self.shutdown_tx.write().await.take() {
            if tx.send(()).await.is_err() {
                debug!("Agent shutdown: receiver already dropped");
            }
        }
        Ok(())
    }

    /// Get agent health status
    pub async fn health_check(&self) -> crate::Result<bool> {
        self.provider.health_check().await
    }

    /// Get the tool registry
    pub fn get_tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Extract artifacts (code blocks, links) from tool result content
    /// and store them in the artifact store.
    pub(super) fn extract_and_store_artifacts(
        &self,
        session_id: &str,
        content: &str,
        tool_name: &str,
    ) {
        let Some(ref artifact_store) = self.artifact_store else {
            return;
        };

        // Extract code blocks: ```language\ncode\n```
        for (idx, cap) in RE_CODE_BLOCK.captures_iter(content).enumerate() {
            let language = cap.get(1).map(|m| m.as_str()).unwrap_or("text");
            let code = cap.get(2).map(|m| m.as_str()).unwrap_or("");
            if code.len() < 20 {
                continue; // Skip trivial snippets
            }
            let artifact = Artifact::code(
                format!("{}-code-{}", tool_name, idx),
                session_id,
                format!("Code from {} ({})", tool_name, language),
                language,
                code,
            );
            let size = artifact.size_bytes;
            artifact_store.add(artifact);
            // Track in disk budget
            if let Some(ref budget) = self.disk_budget {
                if let Err(e) = budget.track_item(
                    session_id,
                    format!("artifact-{}-code-{}", tool_name, idx),
                    BudgetCategory::Artifact,
                    size,
                ) {
                    warn!("Failed to track code artifact in disk budget: {}", e);
                }
            }
        }

        // Extract URLs/links
        for (idx, cap) in RE_URL.captures_iter(content).enumerate() {
            let url = cap.get(0).map(|m| m.as_str()).unwrap_or("");
            if url.len() < 10 {
                continue;
            }
            let artifact = Artifact::link(
                format!("{}-link-{}", tool_name, idx),
                session_id,
                format!("Link from {}", tool_name),
                url,
            );
            let size = artifact.size_bytes;
            artifact_store.add(artifact);
            if let Some(ref budget) = self.disk_budget {
                if let Err(e) = budget.track_item(
                    session_id,
                    format!("artifact-{}-link-{}", tool_name, idx),
                    BudgetCategory::Artifact,
                    size,
                ) {
                    warn!("Failed to track link artifact in disk budget: {}", e);
                }
            }
        }
    }
}

/// A turn's state as the store records it.
///
/// The same words the engine writes: `session_messages.turn_state` is read back
/// with a `COALESCE(..., 'complete')`, so an unrecognised spelling would show up
/// as a finished turn.
fn turn_state_name(state: crate::agent::turns::TurnState) -> &'static str {
    match state {
        crate::agent::turns::TurnState::Pending => "pending",
        crate::agent::turns::TurnState::Running => "running",
        crate::agent::turns::TurnState::Complete => "complete",
        crate::agent::turns::TurnState::Interrupted => "interrupted",
        crate::agent::turns::TurnState::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An agent with a store, the way the gateway builds one.
    async fn agent_with_store() -> (Arc<Agent>, Arc<crate::agent::session_store::SessionStore>) {
        let pool = sqlx::SqlitePool::connect(":memory:").await.expect("pool");
        let store = Arc::new(
            crate::agent::session_store::SessionStore::from_pool(pool)
                .await
                .expect("store"),
        );
        // The session row, because the thread rows carry a foreign key to it —
        // the same order the gateway creates them in.
        store
            .save_session(
                "session-1",
                &crate::agent::session_store::SessionMetadata::new("session-1", "a", "ws", "u"),
                "{}",
            )
            .await
            .expect("session row");
        let agent = AgentBuilder::new()
            .config(crate::agent::AgentConfig::default())
            .provider(Arc::new(crate::providers::mock::MockProvider::new()))
            .tools(Arc::new(ToolRegistry::new()))
            .build()
            .expect("agent")
            .with_session_store(store.clone(), "session-1");
        (Arc::new(agent), store)
    }

    /// Give a conversation two turns, the way a finished exchange leaves it.
    async fn add_turns(agent: &Agent, conversation_id: &str) {
        let mut map = agent.thread_map.lock().await;
        let context = agent.build_fresh_context(conversation_id, "u", "").await;
        let mut thread =
            Thread::from_context(format!("thread-{conversation_id}"), conversation_id, context);
        for (user, assistant) in [("first", "one"), ("second", "two")] {
            let index = thread.push_turn(user);
            thread.turns[index].complete(assistant);
            thread.context.add_message(Message::user(user));
            thread.context.add_message(Message::assistant(assistant));
        }
        map.insert(conversation_id.to_string(), thread);
    }

    /// The branch is the conversation so far, and it is written down: that
    /// second half is what makes it something to come back to rather than
    /// something to lose on the next restart.
    #[tokio::test]
    async fn forking_a_conversation_copies_it_and_persists_the_copy() {
        let (agent, store) = agent_with_store().await;
        add_turns(&agent, "a").await;

        let turns = agent.fork_conversation("a", "b").await.expect("fork");
        assert_eq!(turns, 2);

        let map = agent.thread_map.lock().await;
        let branch = map.get("b").expect("the branch is a live conversation");
        assert_eq!(branch.turn_count(), 2);
        assert_eq!(branch.turns[1].assistant_response, "two");
        assert_eq!(branch.context.id(), "thread-b");
        assert_eq!(map.get("a").expect("parent").turn_count(), 2);
        drop(map);

        // And it is in the store, turn for turn, so `chat.history` and the
        // session list see the same conversation the agent has.
        let rows = store
            .load_threads_for_session("session-1")
            .await
            .expect("threads");
        let branch_row = rows
            .iter()
            .find(|(id, ..)| id == "thread-b")
            .expect("the branch thread was saved");
        assert_eq!(branch_row.3.len(), 2);
        assert_eq!(branch_row.3[0].1, "first");
        assert_eq!(branch_row.3[1].2, "two");
    }

    /// Branching a conversation the agent has no thread for branches what it
    /// has — nothing — rather than failing: a session resumed from the store is
    /// exactly that case.
    #[tokio::test]
    async fn forking_an_unknown_conversation_starts_an_empty_branch() {
        let (agent, _store) = agent_with_store().await;

        let turns = agent
            .fork_conversation("never-seen", "branch")
            .await
            .expect("fork");
        assert_eq!(turns, 0);
        assert!(agent.thread_map.lock().await.contains_key("branch"));
    }
}
