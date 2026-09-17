//! Lifecycle operations: start/self-repair, thread management, undo/redo,
//! shutdown, health, and artifact extraction.

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::providers::Message;
use crate::tools::ToolRegistry;

use super::agent_cache::{RE_CODE_BLOCK, RE_URL};
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
