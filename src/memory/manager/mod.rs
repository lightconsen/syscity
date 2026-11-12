//! Memory Manager — unified orchestrator for Syscity's memory system
//!
//! Wires together:
//! - UnifiedStore (SQLite + WAL + FTS5)
//! - EmbeddingPipeline (batched async embeddings)
//! - MemoryStore search with embedding support
//!
//! Provides three-tier memory access:
//! 1. Working — in-flight Context (handled by Agent)
//! 2. Episodic — session history via chat_messages table
//! 3. Semantic — extracted facts/prefs via memories table with embeddings

use std::collections::HashMap;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use super::{
    effectiveness::{EffectivenessConfig, EffectivenessTracker},
    events::{MemoryEventBuilder, MemoryEventLog},
    hybrid::hybrid_search_expanded,
    multimodal::{MemoryMultimodalConfig, MultimodalStore},
    personality::PersonalityMemory,
    qmd::{QmdExecutor, QmdScope},
    session_search::SessionSearch,
    tier::{TierAction, TierEvaluator, TierIndex, TierSystemConfig},
    vector::VectorMemoryService,
    ChatHistoryStore, ChatMessage, Memory, MemoryId, MemoryQuery, MemoryStats, MemoryStore,
    TieredStore, UnifiedStore,
};
use crate::providers::{CompletionRequest, Message, Provider};
use crate::rag::context::{select_by_token_budget, ContextWindowConfig};
use crate::rag::hybrid::HybridSearchConfig;
use crate::rag::pipeline::EmbeddingPipelineHandle;

mod manager_builder;
mod manager_compaction;
mod manager_config;
mod manager_effectiveness;
mod observe_retrieve;
mod session_context;

pub use manager_builder::MemoryManagerBuilder;
pub use manager_config::MemoryManagerConfig;
pub use session_context::SessionContext;

// Internal types shared across the manager's sibling modules.
use manager_effectiveness::effectiveness_tracking_id;
use observe_retrieve::{ContextCache, RecentRecall};
/// The MemoryManager orchestrates all memory operations.
pub struct MemoryManager {
    /// Primary memory store (may be tiered or unified).
    store: Arc<dyn MemoryStore>,
    /// Chat history store (always a DatabaseStore for persistence).
    chat_history: Arc<dyn ChatHistoryStore>,
    config: MemoryManagerConfig,
    /// Embedding pipeline handle (optional if pipeline not configured)
    pipeline: Option<EmbeddingPipelineHandle>,
    /// Vector memory service for semantic search (hybrid path)
    vector_service: Option<Arc<VectorMemoryService>>,
    /// FTS5 session search (hybrid path)
    session_search: Option<Arc<SessionSearch>>,
    /// In-memory cache of the last retrieved context (to avoid repeated DB
    /// hits)
    context_cache: RwLock<Option<ContextCache>>,
    /// Event log for memory operations.
    event_log: Option<MemoryEventLog>,
    /// Tier index for memory tier management.
    tier_index: Option<Arc<TierIndex>>,
    /// Effectiveness tracker for recall hit rates.
    effectiveness: Option<Arc<EffectivenessTracker>>,
    /// Multimodal file store.
    multimodal_store: Option<Arc<MultimodalStore>>,
    /// QMD executor for scope-based queries.
    qmd_executor: Option<Arc<QmdExecutor>>,
    /// Recent recalls per session, awaiting hit evaluation.
    /// session_key -> Vec<(recall_id, memory_content)>
    recent_recalls: RwLock<HashMap<String, Vec<RecentRecall>>>,
    /// Last time effectiveness adjustments were applied (rate limiting).
    last_adjustment: RwLock<Option<std::time::Instant>>,
    /// Optional LLM provider for session compaction and fact extraction.
    llm_provider: Option<Arc<dyn Provider>>,
    /// Optional personality memory manager for SOUL.md auto-generation.
    personality_memory: Option<Arc<PersonalityMemory>>,
}

impl std::fmt::Debug for MemoryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryManager")
            .field("store", &"<dyn MemoryStore>")
            .field("chat_history", &"<dyn ChatHistoryStore>")
            .field("config", &self.config)
            .field("pipeline", &self.pipeline.is_some())
            .field("vector_service", &self.vector_service.is_some())
            .field("session_search", &self.session_search.is_some())
            .field("context_cache", &self.context_cache)
            .field("event_log", &self.event_log.is_some())
            .field("tier_index", &self.tier_index.is_some())
            .field("effectiveness", &self.effectiveness.is_some())
            .field("multimodal_store", &self.multimodal_store.is_some())
            .field("qmd_executor", &self.qmd_executor.is_some())
            .field("recent_recalls", &"<HashMap>")
            .field("llm_provider", &self.llm_provider.as_ref().map(|p| p.name()))
            .field("personality_memory", &self.personality_memory.is_some())
            .finish()
    }
}

impl MemoryManager {
    /// Create a new MemoryManager with the given store and chat history.
    ///
    /// When using a unified store, pass the same `Arc` for both arguments.
    /// When using a tiered store, pass the tiered store as `store` and the
    /// short-term backend as `chat_history`.
    pub fn new(
        store: Arc<dyn MemoryStore>,
        chat_history: Arc<dyn ChatHistoryStore>,
        config: MemoryManagerConfig,
    ) -> Self {
        let event_log = config.workspace_dir.as_ref().map(MemoryEventLog::new);
        // Share the TieredStore's index when possible to prevent the manager
        // and store from maintaining two independent indices that silently diverge.
        let tier_index = if config.enable_tiers {
            let store_index = store.as_tiered_store().map(|ts| ts.tier_index().clone());
            Some(store_index.unwrap_or_else(|| Arc::new(TierIndex::new())))
        } else {
            None
        };
        let effectiveness = if config.track_effectiveness {
            Some(Arc::new(EffectivenessTracker::new(EffectivenessConfig::default())))
        } else {
            None
        };
        let multimodal_store = config
            .workspace_dir
            .as_ref()
            .map(|d| Arc::new(MultimodalStore::new(d, MemoryMultimodalConfig::default())));
        let qmd_executor = config
            .workspace_dir
            .as_ref()
            .map(|d| Arc::new(QmdExecutor::new(d)));

        Self {
            store,
            chat_history,
            config,
            pipeline: None,
            vector_service: None,
            session_search: None,
            context_cache: RwLock::new(None),
            event_log,
            tier_index,
            effectiveness,
            multimodal_store,
            qmd_executor,
            recent_recalls: RwLock::new(HashMap::new()),
            last_adjustment: RwLock::new(None),
            llm_provider: None,
            personality_memory: None,
        }
    }

    /// Attach an embedding pipeline for async embedding generation.
    pub fn with_pipeline(mut self, pipeline: EmbeddingPipelineHandle) -> Self {
        self.pipeline = Some(pipeline);
        self
    }

    /// Attach a vector memory service to enable the hybrid search path.
    ///
    /// Hybrid search is active only when *both* `vector_service` and
    /// `session_search` are set.  When active, `retrieve()` calls
    /// [`hybrid_search`] instead of the plain `DatabaseStore` query.
    pub fn with_vector_service(mut self, svc: Arc<VectorMemoryService>) -> Self {
        self.vector_service = Some(svc);
        self
    }

    /// Attach a session search (FTS5) backend to enable the hybrid search path.
    pub fn with_session_search(mut self, ss: Arc<SessionSearch>) -> Self {
        self.session_search = Some(ss);
        self
    }

    /// Attach a QMD executor for scope-based queries.
    pub fn with_qmd_executor(mut self, executor: Arc<QmdExecutor>) -> Self {
        self.qmd_executor = Some(executor);
        self
    }

    /// Attach a multimodal store.
    pub fn with_multimodal_store(mut self, store: Arc<MultimodalStore>) -> Self {
        self.multimodal_store = Some(store);
        self
    }

    /// Attach an effectiveness tracker.
    pub fn with_effectiveness_tracker(mut self, tracker: Arc<EffectivenessTracker>) -> Self {
        self.effectiveness = Some(tracker);
        self
    }

    /// Attach a tier index.
    pub fn with_tier_index(mut self, index: Arc<TierIndex>) -> Self {
        self.tier_index = Some(index);
        self
    }

    /// Attach an LLM provider for session compaction and fact extraction.
    pub fn with_llm_provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.llm_provider = Some(provider);
        self
    }

    /// Attach a personality memory manager for SOUL.md auto-generation.
    pub fn with_personality_memory(mut self, memory: Arc<PersonalityMemory>) -> Self {
        self.personality_memory = Some(memory);
        self
    }

    /// Get the event log (if configured).
    pub fn event_log(&self) -> Option<&MemoryEventLog> {
        self.event_log.as_ref()
    }

    /// Get the tier index (if configured).
    pub fn tier_index(&self) -> Option<Arc<TierIndex>> {
        self.tier_index.clone()
    }

    /// Get the effectiveness tracker (if configured).
    pub fn effectiveness_tracker(&self) -> Option<Arc<EffectivenessTracker>> {
        self.effectiveness.clone()
    }

    /// Get the multimodal store (if configured).
    pub fn multimodal_store(&self) -> Option<Arc<MultimodalStore>> {
        self.multimodal_store.clone()
    }

    /// Get the QMD executor (if configured).
    pub fn qmd_executor(&self) -> Option<Arc<QmdExecutor>> {
        self.qmd_executor.clone()
    }

    /// Get the primary memory store.
    pub fn store(&self) -> Arc<dyn MemoryStore> {
        Arc::clone(&self.store)
    }

    /// Get the chat history store.
    pub fn chat_history(&self) -> Arc<dyn ChatHistoryStore> {
        Arc::clone(&self.chat_history)
    }

    /// Get memory statistics.
    pub async fn stats(&self) -> crate::Result<MemoryStats> {
        self.store.stats().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;

    #[tokio::test]
    async fn test_memory_manager_observe_and_retrieve() {
        let store = Arc::new(UnifiedStore::new_in_memory().await.unwrap());
        let mm = MemoryManager::new(store.clone(), store, MemoryManagerConfig::default());

        // Store some memories
        let id1 = mm
            .observe("user1", "I love sushi", "preference", 0.9)
            .await
            .unwrap();
        let id2 = mm
            .observe("user1", "I work at Google", "fact", 0.7)
            .await
            .unwrap();

        // Verify stored
        assert!(!id1.0.is_empty());
        assert!(!id2.0.is_empty());

        // Retrieve (no embedding, so falls back to text search)
        // Use a simpler query that will match with LIKE '%sushi%'
        let results = mm
            .retrieve("user1", None::<&str>, "sushi", Some(5), None::<&str>)
            .await
            .unwrap();

        // Should find the sushi memory
        assert!(
            results.iter().any(|m| m.content.contains("sushi")),
            "Expected to find sushi memory"
        );
    }

    #[tokio::test]
    async fn test_memory_manager_session_context() {
        let store = Arc::new(UnifiedStore::new_in_memory().await.unwrap());
        let mm = MemoryManager::new(store.clone(), store, MemoryManagerConfig::default());

        // Add messages
        mm.remember_message("user1", "conv1", "user", "Hello!")
            .await
            .unwrap();
        mm.remember_message("user1", "conv1", "assistant", "Hi there!")
            .await
            .unwrap();

        // Add a memory
        mm.observe("user1", "User likes pizza", "preference", 0.8)
            .await
            .unwrap();

        // Get context
        let ctx = mm
            .session_context("user1", "conv1", Some::<&str>("food"), None::<&str>)
            .await
            .unwrap();

        assert_eq!(ctx.messages.len(), 2);
    }

    /// One turn used to run two LLM query expansions of the same text — one
    /// for the global memory search, one for the knowledge-base collection —
    /// at ~2 s each. The expansion is now computed once and handed to both
    /// searches, so a retrieval with a KB collection must still be exactly
    /// one provider call.
    #[tokio::test]
    async fn test_retrieve_expands_the_query_once_for_memory_and_kb() {
        use crate::providers::mock::MockProvider;
        use crate::providers::Provider;
        use crate::rag::config::EmbeddingConfig;
        use crate::rag::embedding::EmbeddingProvider;
        use crate::rag::vector_store::MemoryVectorStore;

        struct FixedEmbeddingProvider;
        #[async_trait]
        impl EmbeddingProvider for FixedEmbeddingProvider {
            fn model_name(&self) -> &str {
                "fixed"
            }
            fn dimension(&self) -> usize {
                4
            }
            async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
                Ok(texts
                    .iter()
                    .map(|t| {
                        let n = t.len() as f32;
                        vec![n, 0.0, 0.0, 0.0]
                    })
                    .collect())
            }
        }

        let provider = Arc::new(MockProvider::new().with_callback(|_| {
            crate::providers::Message::assistant("alternative one\nalternative two")
        }));
        let embedding = Arc::new(FixedEmbeddingProvider) as Arc<dyn EmbeddingProvider>;
        let vectors = Arc::new(MemoryVectorStore::new(4));
        let service =
            VectorMemoryService::new(embedding, vectors.clone(), &EmbeddingConfig::default())
                .with_multi_query(
                    provider.clone() as Arc<dyn Provider>,
                    None,
                    crate::rag::multi_query::MultiQueryConfig {
                        enabled: true,
                        num_variations: 2,
                        ..Default::default()
                    },
                );
        // A populated collection, so the KB branch actually runs.
        service
            .add_to_collection("doc body", None, "kb-default")
            .await
            .unwrap();

        let pool = sqlx::SqlitePool::connect(":memory:").await.unwrap();
        let session_search = Arc::new(SessionSearch::new(pool));
        session_search.initialize().await.unwrap();

        let store = Arc::new(UnifiedStore::new_in_memory().await.unwrap());
        let mm = MemoryManager::new(store.clone(), store, MemoryManagerConfig::default())
            .with_vector_service(Arc::new(service))
            .with_session_search(session_search);

        let _ = mm
            .retrieve("user1", None::<&str>, "what did we decide", Some(5), Some("kb-default"))
            .await
            .unwrap();

        assert_eq!(provider.call_count(), 1, "memory and KB search must share one expansion");
    }

    /// A collection that holds nothing cannot contribute, so the search is
    /// skipped — embedding the query for an empty KB was pure cost.
    #[tokio::test]
    async fn test_retrieve_skips_an_empty_kb_collection() {
        use crate::rag::config::EmbeddingConfig;
        use crate::rag::embedding::EmbeddingProvider;
        use crate::rag::vector_store::MemoryVectorStore;

        struct FixedEmbeddingProvider;
        #[async_trait]
        impl EmbeddingProvider for FixedEmbeddingProvider {
            fn model_name(&self) -> &str {
                "fixed"
            }
            fn dimension(&self) -> usize {
                2
            }
            async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
                Ok(texts.iter().map(|t| vec![t.len() as f32, 0.0]).collect())
            }
        }

        let embedding = Arc::new(FixedEmbeddingProvider) as Arc<dyn EmbeddingProvider>;
        let vectors = Arc::new(MemoryVectorStore::new(2));
        let service = VectorMemoryService::new(embedding, vectors, &EmbeddingConfig::default());

        let pool = sqlx::SqlitePool::connect(":memory:").await.unwrap();
        let session_search = Arc::new(SessionSearch::new(pool));
        session_search.initialize().await.unwrap();

        let store = Arc::new(UnifiedStore::new_in_memory().await.unwrap());
        let mm = MemoryManager::new(store.clone(), store, MemoryManagerConfig::default())
            .with_vector_service(Arc::new(service))
            .with_session_search(session_search);

        let results = mm
            .retrieve("user1", None::<&str>, "anything", Some(5), Some("kb-empty"))
            .await
            .unwrap();
        assert!(
            results.iter().all(|m| m.source != "knowledge_base"),
            "nothing should come from a collection that does not exist"
        );
    }

    #[tokio::test]
    async fn test_session_context_formatting() {
        let ctx = SessionContext {
            messages: vec![],
            memories: vec![
                Memory::new("u1", "Likes coffee", "preference"),
                Memory::new("u1", "Works remotely", "fact"),
            ],
            multimodal_references: vec![],
        };

        let formatted = ctx.format_for_injection();
        assert!(formatted.contains("Relevant Context"));
        assert!(formatted.contains("Likes coffee"));
    }

    #[test]
    fn test_memory_manager_config_default() {
        let config = MemoryManagerConfig::default();
        assert_eq!(config.max_context_memories, 5);
        assert!(config.use_pipeline);
    }

    #[test]
    fn test_context_cache_valid() {
        let cache = ContextCache {
            user_id: "u1".to_string(),
            conversation_id: "c1".to_string(),
            memories: vec![],
            multimodal_references: vec![],
            cached_at: std::time::Instant::now(),
        };
        assert!(cache.is_valid("u1", "c1"));
    }

    #[test]
    fn test_context_cache_invalid_user() {
        let cache = ContextCache {
            user_id: "u1".to_string(),
            conversation_id: "c1".to_string(),
            memories: vec![],
            multimodal_references: vec![],
            cached_at: std::time::Instant::now(),
        };
        assert!(!cache.is_valid("u2", "c1"));
    }

    #[test]
    fn test_context_cache_invalid_conversation() {
        let cache = ContextCache {
            user_id: "u1".to_string(),
            conversation_id: "c1".to_string(),
            memories: vec![],
            multimodal_references: vec![],
            cached_at: std::time::Instant::now(),
        };
        assert!(!cache.is_valid("u1", "c2"));
    }

    #[test]
    fn test_session_context_formatting_empty() {
        let ctx = SessionContext {
            messages: vec![],
            memories: vec![],
            multimodal_references: vec![],
        };
        let formatted = ctx.format_for_injection();
        assert!(!formatted.contains("Relevant Context"));
        assert!(!formatted.contains("Recent Messages"));
    }

    #[test]
    fn test_session_context_formatting_with_many_messages() {
        let mut messages = vec![];
        for i in 0..12 {
            messages.push(ChatMessage::new(
                "c1",
                "u1",
                if i % 2 == 0 { "user" } else { "assistant" },
                format!("msg {}", i),
            ));
        }
        let ctx = SessionContext {
            messages,
            memories: vec![Memory::new("u1", "Likes tea", "preference")],
            multimodal_references: vec![],
        };
        let formatted = ctx.format_for_injection();
        assert!(formatted.contains("Relevant Context"));
        assert!(!formatted.contains("Recent Messages")); // removed — covered by full history
    }

    #[test]
    fn test_memory_manager_builder_default() {
        let builder = MemoryManagerBuilder::default();
        assert!(builder.pipeline.is_none());
        assert!(builder.vector_service.is_none());
        assert!(builder.session_search.is_none());
    }

    #[tokio::test]
    async fn test_memory_manager_debug() {
        let store = Arc::new(UnifiedStore::new_in_memory().await.unwrap());
        let mm = MemoryManager::new(store.clone(), store, MemoryManagerConfig::default());
        let debug = format!("{:?}", mm);
        assert!(debug.contains("MemoryManager"));
    }

    #[tokio::test]
    async fn test_memory_manager_remember_message_and_last_conversation() {
        let store = Arc::new(UnifiedStore::new_in_memory().await.unwrap());
        let mm = MemoryManager::new(store.clone(), store, MemoryManagerConfig::default());

        mm.remember_message("u1", "conv-a", "user", "Hello")
            .await
            .unwrap();
        mm.remember_message("u1", "conv-a", "assistant", "Hi")
            .await
            .unwrap();

        let last = mm.last_conversation("u1").await.unwrap();
        assert_eq!(last, Some("conv-a".to_string()));
    }

    #[tokio::test]
    async fn test_memory_manager_forget() {
        let store = Arc::new(UnifiedStore::new_in_memory().await.unwrap());
        let mm = MemoryManager::new(store.clone(), store, MemoryManagerConfig::default());

        let id = mm
            .observe("u1", "forgettable content", "test", 0.5)
            .await
            .unwrap();
        let deleted = mm.forget(&id).await.unwrap();
        assert!(deleted);

        // Forgetting again should return false
        let deleted_again = mm.forget(&id).await.unwrap();
        assert!(!deleted_again);
    }

    #[tokio::test]
    async fn test_memory_manager_compact_session_short() {
        let store = Arc::new(UnifiedStore::new_in_memory().await.unwrap());
        let mm = MemoryManager::new(store.clone(), store, MemoryManagerConfig::default());

        // Only 3 messages, less than threshold of 10
        for i in 0..3 {
            mm.remember_message("u1", "short-conv", "user", format!("msg {}", i))
                .await
                .unwrap();
        }

        let ids = mm.compact_session("short-conv", None).await.unwrap();
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn test_evaluate_response_hits_marks_hit() {
        use crate::memory::effectiveness::{EffectivenessConfig, EffectivenessTracker};

        let store = Arc::new(UnifiedStore::new_in_memory().await.unwrap());
        let tracker = Arc::new(EffectivenessTracker::new(EffectivenessConfig::default()));
        let mm = MemoryManager::new(store.clone(), store, MemoryManagerConfig::default())
            .with_effectiveness_tracker(tracker.clone());

        let session_key = "u1:conv1";

        // Manually inject a recent recall
        {
            let mut guard = mm.recent_recalls.write().await;
            guard.insert(
                session_key.to_string(),
                vec![RecentRecall {
                    recall_id: "recall-001".to_string(),
                    memory_content: "The user loves hiking in the mountains".to_string(),
                }],
            );
        }

        // Record the recall in the tracker so mark_hit can find it
        tracker
            .record_recall("recall-001", "m1", session_key, "preference", 0.8, 0)
            .await;

        // Response contains the memory content
        mm.evaluate_response_hits(
            session_key,
            "I remember that the user loves hiking in the mountains!",
        )
        .await;

        let stats = tracker.memory_stats("m1").await.unwrap();
        assert_eq!(stats.total_recalls, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.hit_rate, 1.0);

        // recent_recalls should be cleared for this session
        let guard = mm.recent_recalls.read().await;
        assert!(!guard.contains_key(session_key));
    }

    #[tokio::test]
    async fn test_evaluate_response_hits_no_hit() {
        use crate::memory::effectiveness::{EffectivenessConfig, EffectivenessTracker};

        let store = Arc::new(UnifiedStore::new_in_memory().await.unwrap());
        let tracker = Arc::new(EffectivenessTracker::new(EffectivenessConfig::default()));
        let mm = MemoryManager::new(store.clone(), store, MemoryManagerConfig::default())
            .with_effectiveness_tracker(tracker.clone());

        let session_key = "u1:conv2";

        {
            let mut guard = mm.recent_recalls.write().await;
            guard.insert(
                session_key.to_string(),
                vec![RecentRecall {
                    recall_id: "recall-002".to_string(),
                    memory_content: "The user loves hiking in the mountains".to_string(),
                }],
            );
        }

        tracker
            .record_recall("recall-002", "m2", session_key, "preference", 0.8, 0)
            .await;

        // Response does NOT contain the memory content
        mm.evaluate_response_hits(session_key, "That sounds interesting, tell me more.")
            .await;

        let stats = tracker.memory_stats("m2").await.unwrap();
        assert_eq!(stats.total_recalls, 1);
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.hit_rate, 0.0);
    }

    #[tokio::test]
    async fn test_evaluate_response_hits_full_closed_loop() {
        use crate::memory::effectiveness::{
            EffectivenessAction, EffectivenessConfig, EffectivenessTracker,
        };

        let config = EffectivenessConfig {
            auto_adjust: true,
            promotion_threshold: 0.7,
            demotion_threshold: 0.2,
            min_recalls_for_adjustment: 3,
            importance_boost: 0.1,
            importance_penalty: 0.1,
            max_importance: 1.0,
            min_importance: 0.0,
            promote_directly_threshold: 0.9,
            demote_directly_threshold: 0.1,
            max_events_per_memory: 1000,
            max_tracked_memories: 50_000,
        };
        let store = Arc::new(UnifiedStore::new_in_memory().await.unwrap());
        let tracker = Arc::new(EffectivenessTracker::new(config));
        let mm = MemoryManager::new(store.clone(), store, MemoryManagerConfig::default())
            .with_effectiveness_tracker(tracker.clone());

        let session_key = "u1:conv3";

        // Simulate 3 recalls, all hit
        for i in 0..3 {
            let recall_id = format!("recall-high-{}", i);
            {
                let mut guard = mm.recent_recalls.write().await;
                guard
                    .entry(session_key.to_string())
                    .or_default()
                    .push(RecentRecall {
                        recall_id: recall_id.clone(),
                        memory_content: "User prefers dark mode".to_string(),
                    });
            }
            tracker
                .record_recall(&recall_id, "m_high", session_key, "preference", 0.6, 0)
                .await;
        }

        // Response contains the memory content
        mm.evaluate_response_hits(
            session_key,
            "I know that User prefers dark mode, so I'll set that up.",
        )
        .await;

        let stats = tracker.memory_stats("m_high").await.unwrap();
        assert_eq!(stats.total_recalls, 3);
        assert_eq!(stats.hits, 3);
        assert_eq!(stats.hit_rate, 1.0);

        let action = tracker.evaluate("m_high", 0.6).await;
        assert_eq!(action, EffectivenessAction::Boost);

        let new_importance = tracker.apply_action(action, 0.6);
        assert!((new_importance - 0.7).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_apply_effectiveness_adjustments_triggers_tier_migration() {
        use crate::memory::effectiveness::{EffectivenessConfig, EffectivenessTracker};
        use crate::memory::MemoryTier;

        let workspace = tempfile::tempdir().unwrap();
        let config = EffectivenessConfig {
            auto_adjust: true,
            promotion_threshold: 0.7,
            demotion_threshold: 0.2,
            min_recalls_for_adjustment: 3,
            importance_boost: 0.1,
            importance_penalty: 0.1,
            max_importance: 1.0,
            min_importance: 0.0,
            promote_directly_threshold: 0.9,
            demote_directly_threshold: 0.1,
            max_events_per_memory: 1000,
            max_tracked_memories: 50_000,
        };

        let tracker = Arc::new(EffectivenessTracker::new(config));
        let tiered = TieredStore::new(workspace.path().join("memory"))
            .await
            .unwrap()
            .with_effectiveness_config(tracker.config().clone());
        let short_term = tiered.short_term();
        let store = Arc::new(tiered);

        let mut mm_config = MemoryManagerConfig::default();
        mm_config.workspace_dir = Some(workspace.path().to_path_buf());

        let mm = MemoryManager::new(store.clone(), Arc::new(short_term), mm_config)
            .with_effectiveness_tracker(tracker.clone());

        let session_key = "u1:conv-tier";

        // Store a low-importance memory that lands in Working tier.
        let id = mm
            .observe("u1", "Tier migration fact", "fact", 0.1)
            .await
            .unwrap();

        // Register in the store's tier index (observe does this via manager's own
        // index, but the TieredStore also tracks it on store). Verify initial
        // tier.
        let tiered_store = store.as_tiered_store().unwrap();
        assert_eq!(tiered_store.tier_index().get_tier(&id.0), Some(MemoryTier::Working));

        // Simulate 3 recalls, all hit, so hit_rate is 1.0 >=
        // promote_directly_threshold.
        for i in 0..3 {
            let recall_id = format!("recall-tier-{}", i);
            {
                let mut guard = mm.recent_recalls.write().await;
                guard
                    .entry(session_key.to_string())
                    .or_default()
                    .push(RecentRecall {
                        recall_id: recall_id.clone(),
                        memory_content: "Tier migration fact".to_string(),
                    });
            }
            tracker
                .record_recall(&recall_id, &id.0, session_key, "fact", 0.1, 0)
                .await;
        }

        mm.evaluate_response_hits(session_key, "I remember the Tier migration fact.")
            .await;

        let stats = tracker.memory_stats(&id.0).await.unwrap();
        assert_eq!(stats.hit_rate, 1.0);

        // Apply adjustments. The importance boost and effectiveness-driven tier
        // evaluator should promote the memory out of the Working tier.
        mm.apply_effectiveness_adjustments().await;

        let tiered_store = store.as_tiered_store().unwrap();
        let final_tier = tiered_store.tier_index().get_tier(&id.0);
        assert!(
            final_tier != Some(MemoryTier::Working),
            "Memory should have been promoted based on effectiveness, got {:?}",
            final_tier
        );

        let stats = tracker.memory_stats(&id.0).await.unwrap();
        assert_eq!(stats.promotions, 1);
    }

    #[tokio::test]
    async fn test_manager_retrieve_hybrid_tracking_id() {
        use sqlx::sqlite::SqlitePool;

        use crate::memory::effectiveness::{EffectivenessConfig, EffectivenessTracker};
        use crate::memory::session_search::SessionSearch;
        use crate::memory::vector::VectorMemoryService;
        use crate::rag::config::EmbeddingConfig;
        use crate::rag::embedding::EmbeddingProvider;
        use crate::rag::vector_store::{MemoryVectorStore, VectorStore};

        struct FixedEmbeddingProvider;
        #[async_trait]
        impl EmbeddingProvider for FixedEmbeddingProvider {
            fn model_name(&self) -> &str {
                "fixed"
            }
            fn dimension(&self) -> usize {
                2
            }
            async fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
                Ok(texts.iter().map(|t| vec![t.len() as f32, 0.0]).collect())
            }
        }

        let store = Arc::new(UnifiedStore::new_in_memory().await.unwrap());

        // Vector backend: add content so hybrid_search returns a semantic result.
        let provider = Arc::new(FixedEmbeddingProvider) as Arc<dyn EmbeddingProvider>;
        let vector_store = Arc::new(MemoryVectorStore::new(2)) as Arc<dyn VectorStore>;
        let vector_service =
            Arc::new(VectorMemoryService::new(provider, vector_store, &EmbeddingConfig::default()));
        vector_service
            .add_to_collection("I love sushi", None, "default")
            .await
            .unwrap();

        // FTS backend: empty but initialized.
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        let session_search = Arc::new(SessionSearch::new(pool));
        session_search.initialize().await.unwrap();

        let tracker = Arc::new(EffectivenessTracker::new(EffectivenessConfig::default()));

        let mut mm_config = MemoryManagerConfig::default();
        mm_config.hybrid_config.min_score = 0.0;

        let mm = MemoryManager::new(store.clone(), store, mm_config)
            .with_vector_service(vector_service)
            .with_session_search(session_search)
            .with_effectiveness_tracker(tracker.clone());

        let results = mm
            .retrieve("user1", None::<&str>, "sushi", Some(5), None::<&str>)
            .await
            .unwrap();

        assert!(
            results.iter().any(|m| m.content.contains("sushi")),
            "Expected hybrid search to find sushi memory"
        );

        // The synthetic hybrid result should have been tracked by a stable
        // content hash, not its random MemoryId.
        let mem = results
            .iter()
            .find(|m| m.content.contains("sushi"))
            .unwrap();
        let tracking_id = effectiveness_tracking_id(mem, "user1");
        assert_ne!(tracking_id, mem.id.to_string());

        let stats = tracker.memory_stats(&tracking_id).await.unwrap();
        assert_eq!(stats.total_recalls, 1);
    }

    #[tokio::test]
    async fn test_apply_effectiveness_adjustments_handles_storage_error() {
        use crate::memory::effectiveness::{EffectivenessConfig, EffectivenessTracker};

        let tracker = Arc::new(EffectivenessTracker::new(EffectivenessConfig {
            auto_adjust: true,
            promotion_threshold: 0.7,
            demotion_threshold: 0.2,
            min_recalls_for_adjustment: 1,
            importance_boost: 0.1,
            importance_penalty: 0.1,
            max_importance: 1.0,
            min_importance: 0.0,
            promote_directly_threshold: 0.9,
            demote_directly_threshold: 0.1,
            max_events_per_memory: 1000,
            max_tracked_memories: 50_000,
        }));

        let memory = Memory::new("u1", "test fact", "fact").with_importance_score(0.6);
        let memory_id = memory.id.clone();
        let update_calls = Arc::new(AtomicUsize::new(0));

        let chat_history = Arc::new(UnifiedStore::new_in_memory().await.unwrap());
        let store = Arc::new(FailingImportanceStore {
            memory,
            update_calls: update_calls.clone(),
        });

        let mm = MemoryManager::new(store, chat_history, MemoryManagerConfig::default())
            .with_effectiveness_tracker(tracker.clone());

        // Record enough under-performing recalls to trigger an adjustment.
        for i in 0..3 {
            tracker
                .record_recall(&format!("recall-{i}"), &memory_id.0, "u1:conv1", "fact", 0.6, i)
                .await;
        }

        // Should not panic; it should log the storage error and continue.
        mm.apply_effectiveness_adjustments().await;

        assert_eq!(
            update_calls.load(Ordering::SeqCst),
            1,
            "update_importance_score should have been called once"
        );
    }

    struct FailingImportanceStore {
        memory: Memory,
        update_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl MemoryStore for FailingImportanceStore {
        async fn store(&self, _memory: Memory) -> crate::Result<MemoryId> {
            unimplemented!()
        }

        async fn get(&self, id: &MemoryId) -> crate::Result<Option<Memory>> {
            if id.0 == self.memory.id.0 {
                Ok(Some(self.memory.clone()))
            } else {
                Ok(None)
            }
        }

        async fn update(&self, _memory: Memory) -> crate::Result<()> {
            unimplemented!()
        }

        async fn delete(&self, _id: &MemoryId) -> crate::Result<bool> {
            unimplemented!()
        }

        async fn search(&self, _query: MemoryQuery) -> crate::Result<Vec<Memory>> {
            unimplemented!()
        }

        async fn cleanup_expired(&self) -> crate::Result<usize> {
            Ok(0)
        }

        async fn stats(&self) -> crate::Result<MemoryStats> {
            Ok(MemoryStats::default())
        }

        async fn close(&self) -> crate::Result<()> {
            Ok(())
        }

        async fn update_importance_score(
            &self,
            _id: &MemoryId,
            _new_score: f32,
        ) -> crate::Result<Option<Memory>> {
            self.update_calls.fetch_add(1, Ordering::SeqCst);
            Err(crate::error::SyscityError::Storage {
                context: "simulated storage failure".to_string(),
                details: "test".to_string(),
            })
        }
    }
}
