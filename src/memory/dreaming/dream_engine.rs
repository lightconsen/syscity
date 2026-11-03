//! Dream engine orchestration: light/deep/REM phase execution.

use super::*;

/// The dreaming engine orchestrates background memory consolidation.
pub struct DreamEngine {
    pub(super) config: DreamConfig,
    tier_config: TierSystemConfig,
    /// Last checkpoint for recovery.
    checkpoint: RwLock<DreamCheckpoint>,
    /// Knowledge graph built during REM dreams.
    knowledge_graph: RwLock<KnowledgeGraph>,
    /// Event log for recording promotions and dream completions.
    event_log: Option<MemoryEventLog>,
    /// Workspace directory for knowledge graph persistence.
    workspace_dir: Option<std::path::PathBuf>,
    /// Optional review queue for human-in-the-loop mode.
    /// When set, dream actions are enqueued instead of applied directly.
    review_queue: Option<Arc<DreamReviewQueue>>,
    /// Observability counters for dream activity.
    metrics: Arc<DreamMetrics>,
}

/// Helper to estimate LLM token count from text.
///
/// This is a fast heuristic approximation (characters / 4) when provider
/// token usage metadata is unavailable.
pub(super) fn estimate_tokens(text: &str) -> u32 {
    (text.chars().count() as u32 / 4).max(1)
}

/// Capture current system memory usage in megabytes.
///
/// `None` on a platform `sysinfo` cannot read (notably iOS), where the dream
/// engine simply runs without the memory trace.
fn current_memory_mb() -> Option<f64> {
    let mut sys = System::new_with_specifics(
        RefreshKind::nothing().with_memory(sysinfo::MemoryRefreshKind::nothing()),
    )
    .ok()?;
    sys.refresh_memory();
    Some(sys.used_memory() as f64 / 1024.0 / 1024.0)
}

/// Timing and token-tracking context for a single dream phase.
struct DreamPhaseContext {
    start: Instant,
    input_tokens: u32,
    output_tokens: u32,
}

impl DreamPhaseContext {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    fn track_prompt(&mut self, prompt: &str) {
        self.input_tokens += estimate_tokens(prompt);
    }

    fn track_response(&mut self, response: &str) {
        self.output_tokens += estimate_tokens(response);
    }

    fn duration_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

impl DreamEngine {
    /// Create a new dream engine.
    pub fn new(config: DreamConfig, tier_config: TierSystemConfig) -> Self {
        Self {
            config,
            tier_config,
            checkpoint: RwLock::new(DreamCheckpoint::default()),
            knowledge_graph: RwLock::new(KnowledgeGraph::default()),
            event_log: None,
            workspace_dir: None,
            review_queue: None,
            metrics: Arc::new(DreamMetrics::default()),
        }
    }

    /// Attach an event log.
    pub fn with_event_log(mut self, log: MemoryEventLog) -> Self {
        self.event_log = Some(log);
        self
    }

    /// Attach a review queue for human-in-the-loop dream mode.
    ///
    /// When set, dream actions are enqueued for review instead of
    /// applied directly to the memory store.
    pub fn with_review_queue(mut self, queue: Arc<DreamReviewQueue>) -> Self {
        self.review_queue = Some(queue);
        self
    }

    /// Attach a workspace directory for knowledge graph persistence.
    pub fn with_workspace_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.workspace_dir = Some(dir);
        self
    }

    /// Attach shared observability metrics.
    pub fn with_metrics(mut self, metrics: Arc<DreamMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Get the current metrics.
    pub fn metrics(&self) -> Arc<DreamMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Initialize the engine: load persisted knowledge graph if available.
    pub async fn initialize(&self) {
        if let Some(ref dir) = self.workspace_dir {
            let kg_path = dir.join("memory/.dreams/knowledge_graph.json");
            if kg_path.exists() {
                match KnowledgeGraph::load_from_disk(&kg_path).await {
                    Ok(kg) => {
                        *self.knowledge_graph.write().await = kg;
                        info!("Loaded persisted knowledge graph from {:?}", kg_path);
                    }
                    Err(e) => warn!("Failed to load knowledge graph: {}", e),
                }
            }
        }
    }

    /// Persist the current knowledge graph to disk.
    async fn save_knowledge_graph(&self) {
        if let Some(ref dir) = self.workspace_dir {
            let kg_path = dir.join("memory/.dreams/knowledge_graph.json");
            let graph = self.knowledge_graph.read().await.clone();
            if let Err(e) = graph.save_to_disk(&kg_path).await {
                warn!("Failed to save knowledge graph: {}", e);
            } else {
                debug!("Saved knowledge graph to {:?}", kg_path);
            }
        }
    }

    /// Run a Light Dream: deduplication, expiry cleanup, basic tier
    /// maintenance.
    ///
    /// - Remove expired memories
    /// - Deduplicate by embedding similarity > threshold
    /// - Promote/demote based on tier rules
    pub async fn run_light(
        &self,
        store: &dyn MemoryStore,
        tier_index: &TierIndex,
        cancel: CancelSignal,
    ) -> crate::Result<DreamResult> {
        let started_at = SystemTime::now();
        let dream_id = format!("dream-light-{}", uuid::Uuid::new_v4());
        info!("Starting Light Dream: {}", dream_id);

        let ctx = DreamPhaseContext::new();
        let baseline_memory_mb = current_memory_mb();

        let mut removed = 0;
        let mut promoted = 0;
        let mut demoted = 0;
        let mut processed = 0;
        let mut errors = Vec::new();
        let mut cancelled = false;

        // Check for cancellation before starting
        if *cancel.borrow() {
            cancelled = true;
        }

        // Fetch all memories
        let memories = if !cancelled {
            store
                .search(MemoryQuery::new().limit(self.config.max_memories_per_cycle))
                .await?
        } else {
            Vec::new()
        };
        info!("Light Dream: processing {} memories", memories.len());

        let evaluator = TierEvaluator::new(self.tier_config.clone());

        // Deduplication: use LSH banding to find candidate pairs in near-linear
        // time, then confirm with exact cosine similarity. Memories without
        // embeddings fall back to prefix-hash bucketing.
        let mut removed_ids: HashSet<MemoryId> = HashSet::new();
        if !cancelled {
            let dedup_threshold = self.config.dedup_similarity_threshold;
            let candidate_pairs = lsh_dedup::build_dedup_candidate_pairs(&memories);
            for (i, j) in candidate_pairs {
                // Check cancellation periodically
                if *cancel.borrow() {
                    cancelled = true;
                    break;
                }
                let mem_i = &memories[i];
                let mem_j = &memories[j];
                if removed_ids.contains(&mem_i.id) || removed_ids.contains(&mem_j.id) {
                    continue;
                }
                let similar = match (&mem_i.embedding, &mem_j.embedding) {
                    (Some(emb_i), Some(emb_j)) => cosine_similarity(emb_i, emb_j) > dedup_threshold,
                    _ => {
                        let key_i = mem_i
                            .content
                            .to_lowercase()
                            .chars()
                            .take(50)
                            .collect::<String>();
                        let key_j = mem_j
                            .content
                            .to_lowercase()
                            .chars()
                            .take(50)
                            .collect::<String>();
                        key_i == key_j
                    }
                };
                if similar {
                    if mem_i.importance_score >= mem_j.importance_score {
                        removed_ids.insert(mem_j.id.clone());
                    } else {
                        removed_ids.insert(mem_i.id.clone());
                    }
                }
            }
        }

        // Delete duplicates if not cancelled
        if !cancelled {
            for id in &removed_ids {
                // Check cancellation periodically
                if *cancel.borrow() {
                    cancelled = true;
                    break;
                }
                if let Err(e) = store.delete(id).await {
                    errors.push(format!("Failed to delete duplicate {}: {}", id, e));
                } else {
                    removed += 1;
                    tier_index.remove(&id.to_string());
                }
            }
        }

        // Tier maintenance if not cancelled
        if !cancelled {
            // Tier maintenance — also moves data between backends via as_tiered_store().
            // Skip memories that were deduplicated and removed.
            let tiered_store = store.as_tiered_store();
            for mem in &memories {
                // Check cancellation periodically
                if *cancel.borrow() {
                    cancelled = true;
                    break;
                }
                if removed_ids.contains(&mem.id) {
                    continue;
                }
                processed += 1;
                if let Some(tiered) = tier_index.get(&mem.id.to_string()) {
                    let old_tier = tiered.tier;
                    match evaluator.evaluate(mem, &tiered, None) {
                        TierAction::Promote(new_tier) => {
                            // Actually move data between backends when a tiered store is available.
                            if let Some(ts) = tiered_store {
                                if let Err(e) = ts.migrate_memory(mem, new_tier).await {
                                    errors.push(format!(
                                        "Failed to migrate memory {}: {}",
                                        mem.id, e
                                    ));
                                    continue;
                                }
                                // Keep the outer tier_index in sync with the actual data location.
                                tier_index.update_tier(&mem.id.to_string(), new_tier);
                            } else {
                                // Fallback: update index only (data stays in original backend).
                                tier_index.update_tier(&mem.id.to_string(), new_tier);
                            }
                            promoted += 1;
                            if let Some(ref event_log) = self.event_log {
                                let event = MemoryEventBuilder::new().promotion(
                                    format!("{}:dream", mem.user_id),
                                    format!("promote-{}", uuid::Uuid::new_v4()),
                                    old_tier.label(),
                                    new_tier.label(),
                                    "Dream tier evaluation",
                                );
                                if let Err(e) = event_log.append(&event).await {
                                    warn!("Failed to append promotion event: {}", e);
                                }
                            }
                        }
                        TierAction::Demote(new_tier) => {
                            // Actually move data between backends when a tiered store is available.
                            if let Some(ts) = tiered_store {
                                if let Err(e) = ts.migrate_memory(mem, new_tier).await {
                                    errors.push(format!(
                                        "Failed to migrate memory {}: {}",
                                        mem.id, e
                                    ));
                                    continue;
                                }
                                // Keep the outer tier_index in sync with the actual data location.
                                tier_index.update_tier(&mem.id.to_string(), new_tier);
                            } else {
                                // Fallback: update index only (data stays in original backend).
                                tier_index.update_tier(&mem.id.to_string(), new_tier);
                            }
                            demoted += 1;
                            if let Some(ref event_log) = self.event_log {
                                let event = MemoryEventBuilder::new().promotion(
                                    format!("{}:dream", mem.user_id),
                                    format!("demote-{}", uuid::Uuid::new_v4()),
                                    old_tier.label(),
                                    new_tier.label(),
                                    "Dream tier evaluation",
                                );
                                if let Err(e) = event_log.append(&event).await {
                                    warn!("Failed to append promotion event: {}", e);
                                }
                            }
                        }
                        TierAction::Evict => {
                            if let Err(e) = store.delete(&mem.id).await {
                                errors.push(format!("Failed to evict {}: {}", mem.id, e));
                            } else {
                                removed += 1;
                                tier_index.remove(&mem.id.to_string());
                            }
                        }
                        TierAction::Keep => {}
                    }
                }
            }
        }

        let finished_at = SystemTime::now();
        let peak_memory_mb = current_memory_mb().map(|peak| {
            if let Some(baseline) = baseline_memory_mb {
                peak.max(baseline)
            } else {
                peak
            }
        });
        let result = DreamResult {
            dream_id,
            phase: DreamPhase::Light,
            started_at,
            finished_at,
            duration_ms: ctx.duration_ms(),
            memories_processed: processed,
            memories_created: 0,
            memories_removed: removed,
            memories_promoted: promoted,
            memories_demoted: demoted,
            peak_memory_mb,
            llm_tokens_input: ctx.input_tokens,
            llm_tokens_output: ctx.output_tokens,
            summary: if cancelled {
                format!(
                    "Light Dream: cancelled after processing {} memories, removed {} \
                     duplicates/expired, promoted {}, demoted {}",
                    processed, removed, promoted, demoted
                )
            } else {
                format!(
                    "Light Dream: processed {} memories, removed {} duplicates/expired, promoted \
                     {}, demoted {}",
                    processed, removed, promoted, demoted
                )
            },
            errors,
            cancelled,
        };

        *self.checkpoint.write().await = DreamCheckpoint {
            last_memory_id: None,
            phase: Some(DreamPhase::Light),
            timestamp: finished_at,
        };

        info!("Light Dream complete: {}", result.summary);
        Ok(result)
    }

    /// Run a Deep Dream: topic clustering, summary generation, cross-session
    /// linking.
    ///
    /// - Cluster memories by embedding similarity
    /// - Generate summary memories for dense clusters
    /// - Link related memories across sessions
    pub async fn run_deep(
        &self,
        store: &dyn MemoryStore,
        tier_index: &TierIndex,
        cancel: CancelSignal,
    ) -> crate::Result<DreamResult> {
        let started_at = SystemTime::now();
        let dream_id = format!("dream-deep-{}", uuid::Uuid::new_v4());
        info!("Starting Deep Dream: {}", dream_id);

        let ctx = DreamPhaseContext::new();
        let baseline_memory_mb = current_memory_mb();

        let mut created = 0;
        let mut errors = Vec::new();
        let mut cancelled = false;

        // Check for cancellation before starting
        if *cancel.borrow() {
            cancelled = true;
        }

        let memories = if !cancelled {
            store
                .search(MemoryQuery::new().limit(self.config.max_memories_per_cycle))
                .await?
        } else {
            Vec::new()
        };
        info!("Deep Dream: processing {} memories", memories.len());

        let mut clusters: HashMap<String, Vec<&Memory>> = HashMap::new();

        // Agglomerative clustering by embedding cosine similarity if not cancelled
        if !cancelled {
            // Memories with embeddings are clustered using cosine similarity;
            // memories without embeddings fall back to word-based grouping.
            let (with_embeddings, without_embeddings): (Vec<&Memory>, Vec<&Memory>) =
                memories.iter().partition(|m| m.embedding.is_some());

            // Cluster memories with embeddings using single-pass threshold merging
            // via union-find if not cancelled
            if !with_embeddings.is_empty() && !cancelled {
                let n = with_embeddings.len();
                let embeddings: Vec<&Vec<f32>> = with_embeddings
                    .iter()
                    .filter_map(|m| m.embedding.as_ref())
                    .collect();
                let merge_threshold = 0.7;

                // Union-find over memory indices.
                let mut parent: Vec<usize> = (0..n).collect();
                fn find(parent: &mut [usize], mut x: usize) -> usize {
                    while parent[x] != x {
                        parent[x] = parent[parent[x]]; // path compression
                        x = parent[x];
                    }
                    x
                }

                for i in 0..n {
                    // Check cancellation periodically
                    if *cancel.borrow() {
                        cancelled = true;
                        break;
                    }
                    for j in (i + 1)..n {
                        if embeddings[i].len() != embeddings[j].len() {
                            continue;
                        }
                        let sim = cosine_similarity(embeddings[i], embeddings[j]);
                        if sim > merge_threshold {
                            let ri = find(&mut parent, i);
                            let rj = find(&mut parent, j);
                            if ri != rj {
                                parent[ri] = rj;
                            }
                        }
                    }
                }

                if !cancelled {
                    // Group memories by their union-find root.
                    for i in 0..n {
                        let root = find(&mut parent, i);
                        let key = with_embeddings[root].id.0.clone();
                        clusters.entry(key).or_default().push(with_embeddings[i]);
                    }
                }
            }

            if !cancelled {
                // Fall back to word-based clustering for memories without embeddings
                for mem in &without_embeddings {
                    // Check cancellation periodically
                    if *cancel.borrow() {
                        cancelled = true;
                        break;
                    }
                    let words: Vec<String> = mem
                        .content
                        .split_whitespace()
                        .filter(|w| w.len() > 4)
                        .map(|w| {
                            w.to_lowercase()
                                .trim_matches(|c: char| !c.is_alphanumeric())
                                .to_string()
                        })
                        .filter(|w| !w.is_empty())
                        .collect();

                    for word in words.iter().take(3) {
                        clusters.entry(word.clone()).or_default().push(mem);
                    }
                }
            }
        }

        // Generate summary memories for clusters with >= 3 members if not cancelled
        if !cancelled {
            for (topic, cluster) in clusters {
                // Check cancellation periodically
                if *cancel.borrow() {
                    cancelled = true;
                    break;
                }
                let mut unique_memories: Vec<&Memory> = cluster;
                unique_memories.sort_by_key(|m| &m.id.0);
                unique_memories.dedup_by(|a, b| a.id.0 == b.id.0);
                if unique_memories.len() >= 3 {
                    let summaries: Vec<String> = unique_memories
                        .iter()
                        .map(|m| m.content.chars().take(80).collect::<String>())
                        .collect();

                    let summary_content = format!("Topic '{}': {}", topic, summaries.join("; "));

                    let user_id = match unique_memories.first() {
                        Some(m) => m.user_id.clone(),
                        None => continue,
                    };
                    let summary = Memory::new(user_id, summary_content, "dream_summary")
                        .with_importance_score(0.7)
                        .with_source("dream_deep")
                        .with_metadata(serde_json::json!({
                            "dream_phase": "deep",
                            "topic": topic,
                            "source_memory_count": unique_memories.len(),
                        }));

                    match store.store(summary).await {
                        Ok(id) => {
                            tier_index.insert(id.to_string(), MemoryTier::LongTerm);
                            created += 1;
                        }
                        Err(e) => {
                            errors.push(format!("Failed to store summary: {}", e));
                        }
                    }
                }
            }
        }

        let finished_at = SystemTime::now();
        let peak_memory_mb = current_memory_mb().map(|peak| {
            if let Some(baseline) = baseline_memory_mb {
                peak.max(baseline)
            } else {
                peak
            }
        });
        let processed = memories.len();
        let result = DreamResult {
            dream_id,
            phase: DreamPhase::Deep,
            started_at,
            finished_at,
            duration_ms: ctx.duration_ms(),
            memories_processed: processed as u32,
            memories_created: created,
            memories_removed: 0,
            memories_promoted: 0,
            memories_demoted: 0,
            peak_memory_mb,
            llm_tokens_input: ctx.input_tokens,
            llm_tokens_output: ctx.output_tokens,
            summary: if cancelled {
                format!(
                    "Deep Dream: cancelled after processing {} memories, created {} topic \
                     summaries",
                    processed, created
                )
            } else {
                format!(
                    "Deep Dream: processed {} memories, created {} topic summaries",
                    processed, created
                )
            },
            errors,
            cancelled,
        };

        *self.checkpoint.write().await = DreamCheckpoint {
            last_memory_id: None,
            phase: Some(DreamPhase::Deep),
            timestamp: finished_at,
        };

        info!("Deep Dream complete: {}", result.summary);
        Ok(result)
    }

    /// Run a REM Dream: cross-session pattern discovery, knowledge graph
    /// update.
    ///
    /// - Extract entities and relationships via LLM-based NER (when callback
    ///   provided)
    /// - Update knowledge graph
    /// - Detect recurring patterns across sessions
    pub async fn run_rem(
        &self,
        store: &dyn MemoryStore,
        _tier_index: &TierIndex,
        llm_callback: Option<&LlmCallback>,
        cancel: CancelSignal,
    ) -> crate::Result<DreamResult> {
        let started_at = SystemTime::now();
        let dream_id = format!("dream-rem-{}", uuid::Uuid::new_v4());
        info!("Starting REM Dream: {}", dream_id);

        let mut ctx = DreamPhaseContext::new();
        let baseline_memory_mb = current_memory_mb();

        let mut created = 0;
        let mut errors = Vec::new();
        let mut cancelled = false;

        // Check for cancellation before starting
        if *cancel.borrow() {
            cancelled = true;
        }

        let memories = if !cancelled {
            store
                .search(MemoryQuery::new().limit(self.config.max_memories_per_cycle))
                .await?
        } else {
            Vec::new()
        };
        let processed = memories.len();
        info!("REM Dream: processing {} memories", memories.len());

        let (mut nodes, mut edges) = (Vec::new(), Vec::new());

        // LLM-based entity extraction when callback is available and not cancelled
        if !cancelled {
            if let Some(llm) = llm_callback {
                // Check for cancellation before LLM call
                if *cancel.borrow() {
                    cancelled = true;
                } else {
                    // Build combined content for NER
                    let combined_content: Vec<String> =
                        memories.iter().map(|m| m.content.clone()).collect();
                    let content_for_prompt = combined_content.join("\n---\n");

                    let prompt = format!(
                        "Extract entities (people, places, organizations, concepts) and their \
                         relationships from the following memory content. Each memory is \
                         separated by '---'.\n\nReturn ONLY a JSON object with this schema:\n{{\n  \
                         \"entities\": [{{\"label\": \"name\", \"type\": \
                         \"person|place|organization|concept\", \"confidence\": 0.9}}],\n  \
                         \"relationships\": [{{\"from\": \"entity_label\", \"to\": \
                         \"entity_label\", \"relation\": \"verb_phrase\", \"confidence\": \
                         0.8}}]\n}}\n\nMemory content:\n{}\n\nJSON:",
                        content_for_prompt.chars().take(8000).collect::<String>()
                    );

                    ctx.track_prompt(&prompt);
                    let response = llm(prompt).await;
                    ctx.track_response(&response);

                    // Check for cancellation after LLM call
                    if *cancel.borrow() {
                        cancelled = true;
                    } else {
                        // Parse JSON response
                        #[derive(Deserialize)]
                        struct NerResponse {
                            entities: Vec<NerEntity>,
                            #[serde(default)]
                            relationships: Vec<NerRelationship>,
                        }
                        #[derive(Deserialize)]
                        struct NerEntity {
                            label: String,
                            #[serde(rename = "type")]
                            entity_type: String,
                            #[serde(default = "default_confidence")]
                            confidence: f32,
                        }
                        #[derive(Deserialize)]
                        struct NerRelationship {
                            from: String,
                            to: String,
                            relation: String,
                            #[serde(default = "default_confidence")]
                            confidence: f32,
                        }
                        fn default_confidence() -> f32 {
                            0.5
                        }

                        match serde_json::from_str::<NerResponse>(&response) {
                            Ok(parsed) => {
                                for entity in &parsed.entities {
                                    // Check for cancellation periodically
                                    if *cancel.borrow() {
                                        cancelled = true;
                                        break;
                                    }
                                    let pattern = entity.label.to_lowercase();
                                    let memory_ids: Vec<String> = memories
                                        .iter()
                                        .filter(|m| m.content.to_lowercase().contains(&pattern))
                                        .map(|m| m.id.to_string())
                                        .collect();
                                    if !memory_ids.is_empty() {
                                        nodes.push(KnowledgeNode {
                                            label: entity.label.clone(),
                                            node_type: entity.entity_type.clone(),
                                            memory_ids,
                                            confidence: entity.confidence,
                                        });
                                    }
                                }

                                if !cancelled {
                                    for rel in parsed.relationships {
                                        edges.push(KnowledgeEdge {
                                            from: rel.from,
                                            to: rel.to,
                                            relation: rel.relation,
                                            confidence: rel.confidence,
                                        });
                                    }

                                    // Fall back to co-occurrence edges if LLM didn't provide
                                    // relationships
                                    if edges.is_empty() {
                                        for (i, n1) in nodes.iter().enumerate() {
                                            for n2 in nodes.iter().skip(i + 1) {
                                                let shared: Vec<_> = n1
                                                    .memory_ids
                                                    .iter()
                                                    .filter(|id| n2.memory_ids.contains(id))
                                                    .collect();
                                                if !shared.is_empty() {
                                                    edges.push(KnowledgeEdge {
                                                        from: n1.label.clone(),
                                                        to: n2.label.clone(),
                                                        relation: "co_occurs".to_string(),
                                                        confidence: (shared.len() as f32
                                                            / n1.memory_ids
                                                                .len()
                                                                .max(n2.memory_ids.len())
                                                                as f32)
                                                            .min(1.0),
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                errors.push(format!("Failed to parse LLM NER response: {}", e));
                                debug!(
                                    "LLM NER response (first 500 chars): {}",
                                    response.chars().take(500).collect::<String>()
                                );
                                // Fall back to heuristic extraction
                                if !cancelled {
                                    (nodes, edges) =
                                        knowledge_graph::extract_entities_heuristic(&memories);
                                }
                            }
                        }
                    }
                }
            } else if !cancelled {
                // No LLM callback: use heuristic extraction
                (nodes, edges) = knowledge_graph::extract_entities_heuristic(&memories);
            }
        }

        let node_count = nodes.len();
        let edge_count = edges.len();

        // Store knowledge graph — merge, don't replace, so previous cycles'
        // entities and relationships are preserved. Cap size to prevent
        // unbounded memory growth across REM cycles.
        if !cancelled {
            {
                let mut graph = self.knowledge_graph.write().await;
                let existing_labels: std::collections::HashSet<String> =
                    graph.nodes.iter().map(|n| n.label.clone()).collect();
                for node in nodes {
                    // Check for cancellation periodically
                    if *cancel.borrow() {
                        cancelled = true;
                        break;
                    }
                    if !existing_labels.contains(&node.label) {
                        graph.nodes.push(node);
                    }
                }
                if !cancelled {
                    graph.edges.extend(edges);
                    graph.cap_size();
                }
            }
            if !cancelled {
                self.save_knowledge_graph().await;
            }
        }

        // Create pattern memory from the graph if not cancelled
        if !cancelled && node_count > 0 {
            let pattern_mem = Memory::new(
                "system",
                format!(
                    "REM Dream discovered {} entities and {} relationships",
                    node_count, edge_count
                ),
                "dream_pattern",
            )
            .with_importance_score(0.8)
            .with_source("dream_rem")
            .with_metadata(serde_json::json!({
                "dream_phase": "rem",
                "entity_count": node_count,
                "relation_count": edge_count,
            }));

            match store.store(pattern_mem).await {
                Ok(_id) => {
                    created += 1;
                }
                Err(e) => {
                    errors.push(format!("Failed to store pattern memory: {}", e));
                }
            }
        }

        let finished_at = SystemTime::now();
        let peak_memory_mb = current_memory_mb().map(|peak| {
            if let Some(baseline) = baseline_memory_mb {
                peak.max(baseline)
            } else {
                peak
            }
        });
        let result = DreamResult {
            dream_id,
            phase: DreamPhase::Rem,
            started_at,
            finished_at,
            duration_ms: ctx.duration_ms(),
            memories_processed: processed as u32,
            memories_created: created,
            memories_removed: 0,
            memories_promoted: 0,
            memories_demoted: 0,
            peak_memory_mb,
            llm_tokens_input: ctx.input_tokens,
            llm_tokens_output: ctx.output_tokens,
            summary: if cancelled {
                format!(
                    "REM Dream: cancelled after processing {} memories, discovered {} entities, \
                     {} relations, created {} patterns",
                    processed, node_count, edge_count, created
                )
            } else {
                format!(
                    "REM Dream: processed {} memories, discovered {} entities, {} relations, \
                     created {} patterns",
                    processed, node_count, edge_count, created
                )
            },
            errors,
            cancelled,
        };

        *self.checkpoint.write().await = DreamCheckpoint {
            last_memory_id: None,
            phase: Some(DreamPhase::Rem),
            timestamp: finished_at,
        };

        info!("REM Dream complete: {}", result.summary);
        Ok(result)
    }

    /// Convert local DreamPhase to event-log DreamPhase.
    fn to_event_phase(phase: DreamPhase) -> crate::memory::events::DreamPhase {
        match phase {
            DreamPhase::Light => crate::memory::events::DreamPhase::Light,
            DreamPhase::Deep => crate::memory::events::DreamPhase::Deep,
            DreamPhase::Rem => crate::memory::events::DreamPhase::Rem,
        }
    }

    /// Run a full dream cycle: Light -> Deep -> (optional REM).
    pub async fn run_full_cycle(
        &self,
        store: &dyn MemoryStore,
        tier_index: &TierIndex,
        include_rem: bool,
        llm_callback: Option<&LlmCallback>,
        cancel: CancelSignal,
    ) -> crate::Result<Vec<DreamResult>> {
        if !self.config.enabled {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();

        // Check for cancellation before starting
        if *cancel.borrow() {
            return Ok(results);
        }

        // Light dream always runs
        match self.run_light(store, tier_index, cancel.clone()).await {
            Ok(r) => {
                self.metrics.record(&r, false);
                if let Some(ref event_log) = self.event_log {
                    let event = MemoryEventBuilder::new().dream(
                        &r.dream_id,
                        Self::to_event_phase(r.phase),
                        &r.summary,
                        r.memories_processed,
                        r.memories_created,
                    );
                    if let Err(e) = event_log.append(&event).await {
                        warn!("Failed to append dream event: {}", e);
                    }
                }
                results.push(r);
            }
            Err(e) => {
                self.metrics.dreams_total.fetch_add(1, Ordering::Relaxed);
                self.metrics.dreams_failed.fetch_add(1, Ordering::Relaxed);
                warn!("Light dream failed: {}", e);
            }
        }

        // Check for cancellation before deep dream
        if *cancel.borrow() {
            return Ok(results);
        }

        // Deep dream runs on balanced/slow or if enough memories
        match self.run_deep(store, tier_index, cancel.clone()).await {
            Ok(r) => {
                self.metrics.record(&r, false);
                if let Some(ref event_log) = self.event_log {
                    let event = MemoryEventBuilder::new().dream(
                        &r.dream_id,
                        Self::to_event_phase(r.phase),
                        &r.summary,
                        r.memories_processed,
                        r.memories_created,
                    );
                    if let Err(e) = event_log.append(&event).await {
                        warn!("Failed to append dream event: {}", e);
                    }
                }
                results.push(r);
            }
            Err(e) => {
                self.metrics.dreams_total.fetch_add(1, Ordering::Relaxed);
                self.metrics.dreams_failed.fetch_add(1, Ordering::Relaxed);
                warn!("Deep dream failed: {}", e);
            }
        }

        // REM dream runs only if requested and budget allows and not cancelled
        if include_rem && self.config.budget == DreamBudget::Expensive && !*cancel.borrow() {
            match self
                .run_rem(store, tier_index, llm_callback, cancel.clone())
                .await
            {
                Ok(r) => {
                    self.metrics.record(&r, false);
                    if let Some(ref event_log) = self.event_log {
                        let event = MemoryEventBuilder::new().dream(
                            &r.dream_id,
                            Self::to_event_phase(r.phase),
                            &r.summary,
                            r.memories_processed,
                            r.memories_created,
                        );
                        if let Err(e) = event_log.append(&event).await {
                            warn!("Failed to append dream event: {}", e);
                        }
                    }
                    results.push(r);
                }
                Err(e) => {
                    self.metrics.dreams_total.fetch_add(1, Ordering::Relaxed);
                    self.metrics.dreams_failed.fetch_add(1, Ordering::Relaxed);
                    warn!("REM dream failed: {}", e);
                }
            }
        }

        Ok(results)
    }

    /// Get the current knowledge graph.
    pub async fn knowledge_graph(&self) -> KnowledgeGraph {
        self.knowledge_graph.read().await.clone()
    }

    /// Get the last checkpoint.
    pub async fn checkpoint(&self) -> DreamCheckpoint {
        self.checkpoint.read().await.clone()
    }
}

// ── Dream Review Queue
