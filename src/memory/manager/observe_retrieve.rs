//! Observe/retrieve paths plus the recall and context caches.

use super::*;

/// Maximum number of recent recalls to track per session before LRU eviction.
const MAX_RECENT_RECALLS_PER_SESSION: usize = 100;

/// Maximum number of distinct sessions to track in recent_recalls before
/// evicting the least recently accessed session.  Prevents unbounded
/// HashMap growth when sessions accumulate without calling
/// `evaluate_response_hits`.
const MAX_RECENT_RECALL_SESSIONS: usize = 1000;

/// Tracks a recent recall so it can be evaluated for a hit after the LLM
/// responds.
#[derive(Debug, Clone)]
pub(super) struct RecentRecall {
    pub(super) recall_id: String,
    pub(super) memory_content: String,
}

/// Cached context to avoid repeated DB hits within a short window.
#[derive(Debug, Clone)]
pub(super) struct ContextCache {
    pub(super) user_id: String,
    pub(super) conversation_id: String,
    pub(super) memories: Vec<Memory>,
    pub(super) multimodal_references: Vec<String>,
    pub(super) cached_at: std::time::Instant,
}

impl ContextCache {
    /// TTL for the context cache
    const TTL_MS: u64 = 5000;

    pub(super) fn is_valid(&self, user_id: &str, conversation_id: &str) -> bool {
        self.user_id == user_id
            && self.conversation_id == conversation_id
            && self.cached_at.elapsed().as_millis() < Self::TTL_MS as u128
    }
}

impl MemoryManager {
    // =============================================================================
    // Core API: observe, retrieve, session_context
    // =============================================================================

    /// Observe a fact and store it with an embedding.
    ///
    /// This is the primary write path for semantic memories.  The embedding
    /// is generated via the pipeline if configured, otherwise skipped (to be
    /// filled later by background job).
    pub async fn observe(
        &self,
        user_id: impl Into<String>,
        content: impl Into<String>,
        memory_type: impl Into<String>,
        importance: f32,
    ) -> crate::Result<MemoryId> {
        let user_id = user_id.into();
        let content = content.into();
        let memory_type = memory_type.into();

        debug!(
            "Observing memory: user={} type={} importance={}",
            user_id, memory_type, importance
        );

        // Generate embedding via pipeline if available
        let embedding = if let Some(ref pipeline) = self.pipeline {
            match pipeline.embed(&content).await {
                Ok(emb) => Some(emb),
                Err(e) => {
                    warn!("Embedding failed, storing without embedding: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let memory = Memory::new(&user_id, content, memory_type)
            .with_importance_score(importance)
            .with_source("agent");

        let memory = if let Some(emb) = embedding {
            memory.with_embedding(emb)
        } else {
            memory
        };

        let id = self.store.store(memory.clone()).await?;

        // Invalidate context cache (fix 2.4).
        self.invalidate_cache().await;

        // Register in tier index if enabled
        if let Some(ref tier_index) = self.tier_index {
            if let Some(tiered_store) = self.store.as_tiered_store() {
                // Use the tiered store's evaluator for consistency
                let entry_tier = tiered_store.evaluator().entry_tier(importance, 0);
                tier_index.insert(id.to_string(), entry_tier);
            } else {
                let entry_tier =
                    TierEvaluator::new(TierSystemConfig::default()).entry_tier(importance, 0);
                tier_index.insert(id.to_string(), entry_tier);
            }
        }

        info!("Memory observed: {}", id);
        Ok(id)
    }

    /// Observe a fact with associated metadata and store it with an embedding.
    ///
    /// Like [`observe`](Self::observe) but accepts an additional `metadata`
    /// payload (e.g. dimension scores, weaknesses, suggestions from a
    /// retrospect critique) that is stored alongside the memory.
    pub async fn observe_with_metadata(
        &self,
        user_id: impl Into<String>,
        content: impl Into<String>,
        memory_type: impl Into<String>,
        importance: f32,
        metadata: serde_json::Value,
    ) -> crate::Result<MemoryId> {
        let user_id = user_id.into();
        let content = content.into();
        let memory_type = memory_type.into();

        debug!(
            "Observing memory with metadata: user={} type={} importance={}",
            user_id, memory_type, importance
        );

        let embedding = if let Some(ref pipeline) = self.pipeline {
            match pipeline.embed(&content).await {
                Ok(emb) => Some(emb),
                Err(e) => {
                    warn!("Embedding failed, storing without embedding: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let memory = Memory::new(&user_id, content, memory_type)
            .with_importance_score(importance)
            .with_source("agent")
            .with_metadata(metadata);

        let memory = if let Some(emb) = embedding {
            memory.with_embedding(emb)
        } else {
            memory
        };

        let id = self.store.store(memory.clone()).await?;

        self.invalidate_cache().await;

        if let Some(ref tier_index) = self.tier_index {
            if let Some(tiered_store) = self.store.as_tiered_store() {
                let entry_tier = tiered_store.evaluator().entry_tier(importance, 0);
                tier_index.insert(id.to_string(), entry_tier);
            } else {
                let entry_tier =
                    TierEvaluator::new(TierSystemConfig::default()).entry_tier(importance, 0);
                tier_index.insert(id.to_string(), entry_tier);
            }
        }

        info!("Memory observed with metadata: {}", id);
        Ok(id)
    }

    /// Retrieve relevant memories for a query.
    ///
    /// **Hybrid path** (when both `vector_service` and `session_search` are
    /// attached): runs [`hybrid_search`] concurrently over the vector store and
    /// the FTS5 session index, merges with weighted scoring, applies MMR
    /// re-ranking, and converts results to [`Memory`] values.
    ///
    /// **Fallback path** (when either backend is absent):
    /// 1. Embed the query via pipeline (if available)
    /// 2. Search `DatabaseStore` with embedding similarity or LIKE text search
    pub async fn retrieve(
        &self,
        user_id: impl AsRef<str>,
        conversation_id: Option<&str>,
        query: impl Into<String>,
        limit: Option<usize>,
        kb_collection: Option<&str>,
    ) -> crate::Result<Vec<Memory>> {
        let user_id = user_id.as_ref();
        let query_text = query.into();
        let limit = limit.unwrap_or(self.config.max_context_memories);

        debug!(
            "Retrieving memories: user={} query_len={} hybrid={} fallback=keyword",
            user_id,
            query_text.len(),
            self.vector_service.is_some() && self.session_search.is_some(),
        );

        let mut memories: Vec<Memory>;

        // ── Hybrid path ───────────────────────────────────────────────────────
        if let (Some(ref vs), Some(ref ss)) = (&self.vector_service, &self.session_search) {
            let mut cfg = self.config.hybrid_config.clone();
            cfg.max_results = limit;

            // Expand once per retrieval, not once per search: the global
            // hybrid search and the knowledge-base search below both use these
            // sub-queries. Two LLM expansion calls per turn was ~4 s of the
            // latency on a first message.
            let sub_queries = vs.expand(&query_text).await;

            let hybrid_results = hybrid_search_expanded(
                &query_text,
                &sub_queries,
                user_id,
                conversation_id.unwrap_or(""),
                vs,
                ss,
                &cfg,
            )
            .await;

            memories = hybrid_results
                .into_iter()
                .map(|r| {
                    Memory::new(user_id, r.content, r.memory_type.clone())
                        .with_importance_score(r.score)
                        .with_source(&r.source)
                        .with_metadata(serde_json::json!({
                            "citation": r.citation,
                            "hybrid_source": r.source,
                        }))
                })
                .collect();

            // ── KB collection search (additive, shares the expansion) ───────
            if let Some(kb_coll) = kb_collection.filter(|c| !c.is_empty()) {
                // An empty (or absent) knowledge base cannot contribute; skip
                // the embedding and the RRF merge rather than pay for them.
                let populated = vs
                    .collection_len(kb_coll)
                    .await
                    .map(|len| len > 0)
                    .unwrap_or(true);
                if populated {
                    match vs
                        .search_collection_expanded(&sub_queries, limit, kb_coll, 0.0)
                        .await
                    {
                        Ok(kb_results) => {
                            for r in kb_results {
                                // Skip duplicates against main results
                                if memories.iter().any(|m| m.content == r.content) {
                                    continue;
                                }
                                memories.push(
                                    Memory::new(user_id, r.content, "kb")
                                        .with_importance_score(r.score)
                                        .with_source("knowledge_base")
                                        .with_metadata(serde_json::json!({
                                            "collection": kb_coll,
                                        })),
                                );
                            }
                            // Re-sort by importance score descending
                            memories.sort_by(|a, b| {
                                b.importance_score
                                    .partial_cmp(&a.importance_score)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            });
                        }
                        Err(e) => {
                            warn!("KB collection search failed for '{}': {}", kb_coll, e);
                        }
                    }
                }
            }
        } else {
            // ── Fallback: keyword search via the primary store ────────────────────
            // DatabaseStore no longer handles embedding-based search; semantic
            // recall requires the hybrid vector+FTS path configured above.
            let mut mq = MemoryQuery::new()
                .for_user(user_id)
                .with_content(&query_text)
                .limit(limit);

            if let Some(conv_id) = conversation_id {
                mq = mq.for_conversation(conv_id);
            }

            memories = self.store.search(mq).await?;
        }

        // ── QMD search path ───────────────────────────────────────────────────
        if let Some(ref qmd) = self.qmd_executor {
            let scope = QmdScope::default().with_key_prefix(format!("{}:", user_id));
            match qmd.query(&query_text, Some(&scope)).await {
                Ok(qmd_results) => {
                    let existing: std::collections::HashSet<String> =
                        memories.iter().map(|m| m.content.clone()).collect();
                    for qr in qmd_results.into_iter().take(limit) {
                        if let Some(body) = qr.body.or(qr.snippet) {
                            if existing.contains(&body) {
                                continue;
                            }
                            let score = qr.score.unwrap_or(0.5) as f32;
                            memories.push(
                                Memory::new(user_id, body, "qmd")
                                    .with_importance_score(score)
                                    .with_source("qmd"),
                            );
                        }
                    }
                    memories.sort_by(|a, b| {
                        b.importance_score
                            .partial_cmp(&a.importance_score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    memories.truncate(limit);
                }
                Err(e) => {
                    // "qmd not installed" is a common, benign case — log at debug!
                    // level. Genuine query failures still surface as warn!.
                    let msg = e.to_string();
                    if msg.contains("qmd binary not available") {
                        tracing::debug!("QMD skipped: {msg}");
                    } else {
                        warn!("QMD query failed: {msg}");
                    }
                }
            }
        }

        // Update cache
        if let Some(conv_id) = conversation_id {
            let cache = ContextCache {
                user_id: user_id.to_string(),
                conversation_id: conv_id.to_string(),
                memories: memories.clone(),
                multimodal_references: vec![],
                cached_at: std::time::Instant::now(),
            };
            *self.context_cache.write().await = Some(cache);
        }

        // Log recall events and track effectiveness
        let session_key = conversation_id
            .map(|c| format!("{}:{}", user_id, c))
            .unwrap_or_else(|| user_id.to_string());

        for (rank, mem) in memories.iter().enumerate() {
            let tracking_id = effectiveness_tracking_id(mem, user_id);

            // Record access in tier index
            if let Some(ref tier_index) = self.tier_index {
                tier_index.record_access(&tracking_id);
            }

            // Track effectiveness
            if let Some(ref effectiveness) = self.effectiveness {
                let recall_id = format!("recall-{}", uuid::Uuid::new_v4());
                effectiveness
                    .record_recall(
                        recall_id.clone(),
                        &tracking_id,
                        &session_key,
                        mem.memory_type.as_str(),
                        mem.importance_score,
                        rank,
                    )
                    .await;

                // Store recall for later hit evaluation
                let mut recent_guard = self.recent_recalls.write().await;
                let recalls = recent_guard.entry(session_key.clone()).or_default();
                recalls.push(RecentRecall {
                    recall_id,
                    memory_content: mem.content.clone(),
                });
                // Enforce per-session bound to prevent unbounded growth
                if recalls.len() > MAX_RECENT_RECALLS_PER_SESSION {
                    recalls.remove(0);
                }
                // Enforce total-session bound to prevent unbounded HashMap growth.
                // Evict the oldest-inserted session when the cap is exceeded.
                if recent_guard.len() > MAX_RECENT_RECALL_SESSIONS {
                    // Remove the session key that first appears in iteration order.
                    // HashMap iteration is deterministic within a single-threaded
                    // context, so this is a stable (if arbitrary) victim selection.
                    if let Some(victim) = recent_guard.keys().next().cloned() {
                        if victim != session_key {
                            recent_guard.remove(&victim);
                        } else {
                            // Don't evict the session we just inserted into;
                            // pick the next key instead.
                            let mut keys: Vec<_> = recent_guard.keys().cloned().collect();
                            keys.sort();
                            if let Some(victim) = keys.into_iter().find(|k| k != &session_key) {
                                recent_guard.remove(&victim);
                            }
                        }
                    }
                }
            }

            // Log recall event
            if let Some(ref event_log) = self.event_log {
                let event = MemoryEventBuilder::new().recall(
                    &session_key,
                    format!("recall-{}", uuid::Uuid::new_v4()),
                    &mem.memory_type,
                    mem.content.chars().take(100).collect::<String>(),
                );
                if let Err(e) = event_log.append(&event).await {
                    warn!("Failed to append recall event: {}", e);
                }
            }
        }

        Ok(memories)
    }

    /// Retrieve session context for a conversation.
    ///
    /// Returns:
    /// 1. Recent chat messages (episodic)
    /// 2. Relevant semantic memories (via retrieve)
    pub async fn session_context(
        &self,
        user_id: impl AsRef<str>,
        conversation_id: impl AsRef<str>,
        query: Option<impl Into<String>>,
        kb_collection: Option<&str>,
    ) -> crate::Result<SessionContext> {
        let user_id = user_id.as_ref();
        let conversation_id = conversation_id.as_ref();

        // Check cache first
        {
            let guard = self.context_cache.read().await;
            if let Some(ref cache) = *guard {
                if cache.is_valid(user_id, conversation_id) {
                    return Ok(SessionContext {
                        messages: self
                            .chat_history
                            .get_conversation_history(conversation_id, 50)
                            .await?,
                        memories: cache.memories.clone(),
                        multimodal_references: cache.multimodal_references.clone(),
                    });
                }
            }
        }

        // Episodic: recent messages
        let messages = self
            .chat_history
            .get_conversation_history(conversation_id, 50)
            .await?;

        // Semantic: relevant memories
        let memories = if let Some(q) = query {
            self.retrieve(
                user_id,
                Some(conversation_id),
                q,
                Some(self.config.max_context_memories),
                kb_collection,
            )
            .await?
        } else {
            // No query, fetch recent high-importance memories
            let mq = MemoryQuery::new()
                .for_user(user_id)
                .for_conversation(conversation_id)
                .limit(self.config.max_context_memories);
            self.store.search(mq).await?
        };

        // ── Context-window-aware memory budget ─────────────────────────────
        let memories = if let Some(ref cw_config) = self.config.context_window {
            let current_tokens: usize = messages
                .iter()
                .map(|m| crate::rag::context::estimate_tokens(&m.content))
                .sum();
            select_by_token_budget(memories, cw_config, current_tokens)
        } else {
            memories
        };

        // Multimodal: scan for image/audio files in workspace
        let mut multimodal_references = Vec::new();
        if let Some(ref mm_store) = self.multimodal_store {
            use crate::memory::multimodal::MemoryMultimodalModality;
            for modality in [
                MemoryMultimodalModality::Image,
                MemoryMultimodalModality::Audio,
            ] {
                let files = mm_store.scan_modality(modality).await;
                for entry in files.into_iter().take(5) {
                    let label = entry
                        .label
                        .unwrap_or_else(|| format!("{} file: {}", modality, entry.filename));
                    multimodal_references.push(format!("[{}]", label));
                }
            }
        }

        // Update cache
        let ctx = SessionContext {
            messages,
            memories,
            multimodal_references,
        };
        {
            let cache = ContextCache {
                user_id: user_id.to_string(),
                conversation_id: conversation_id.to_string(),
                memories: ctx.memories.clone(),
                multimodal_references: ctx.multimodal_references.clone(),
                cached_at: std::time::Instant::now(),
            };
            *self.context_cache.write().await = Some(cache);
        }

        Ok(ctx)
    }

    /// Remember a user message (store in chat history).
    pub async fn remember_message(
        &self,
        user_id: impl Into<String>,
        conversation_id: impl Into<String>,
        role: impl Into<String>,
        content: impl Into<String>,
    ) -> crate::Result<()> {
        let user_id = user_id.into();
        let conversation_id = conversation_id.into();
        let role = role.into();
        let content = content.into();
        let msg = ChatMessage::new(&conversation_id, &user_id, &role, &content);
        let msg_id = msg.id.clone();
        self.chat_history.store_message(msg).await?;

        // Index in session search FTS5 if available (fix 2.3).
        if let Some(ref ss) = self.session_search {
            if let Err(e) = ss
                .index_message(&msg_id, &conversation_id, &user_id, &content, &role)
                .await
            {
                warn!("Failed to index message in session search: {e}");
            }
        }

        // Invalidate context cache so the next session_context
        // picks up the new message (fix 2.4).
        self.invalidate_cache().await;

        Ok(())
    }

    /// Get the last conversation ID for a user.
    pub async fn last_conversation(
        &self,
        user_id: impl AsRef<str>,
    ) -> crate::Result<Option<String>> {
        self.chat_history
            .get_last_conversation(user_id.as_ref())
            .await
    }

    /// Forget (delete) a memory by ID.
    pub async fn forget(&self, id: &MemoryId) -> crate::Result<bool> {
        let deleted = self.store.delete(id).await?;
        // Clean up tier index to prevent stale entries from accumulating.
        if let Some(ref tier_index) = self.tier_index {
            tier_index.remove(&id.to_string());
        }
        // Invalidate context cache (fix 2.4).
        self.invalidate_cache().await;
        Ok(deleted)
    }
}
