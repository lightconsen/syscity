//! Memory system for Syscity
//!
//! Provides persistent storage for conversations, messages, and memories
//! with support for semantic search using embeddings.
// INVARIANTS-NONE: tables are written through typed sqlx paths; the cross-session history balance invariant is owned by agent/session_store.

use std::time::SystemTime;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod compressed_store;
pub mod db;
pub mod dreaming;
pub mod effectiveness;
pub mod events;
pub mod flush;
pub mod hybrid;
pub mod in_memory_store;
pub mod manager;
pub mod multimodal;
pub mod onboarding;
pub mod personality;
pub mod qmd;
pub mod query;
pub mod session_search;
pub mod soul;
pub mod sqlite;
pub mod tier;
pub mod tiered_store;
pub mod vector;
pub mod workspace_state;

pub use compressed_store::CompressedJsonlStore;
pub use db::{DatabaseStore, DbStats, QueryBuilder};
pub use in_memory_store::InMemoryStore;
/// Alias for the single canonical SQLite store (WAL + FTS5 + access tracking).
pub type UnifiedStore = DatabaseStore;
pub use crate::rag::hybrid::{
    apply_temporal_decay, mmr_rerank, HybridSearchConfig, HybridSearchResult, MmrConfig,
    TemporalDecayConfig,
};
#[cfg(feature = "pgvector")]
pub use crate::rag::pgvector_store::PgVectorStore;
pub use crate::rag::pipeline::{
    EmbeddingJob, EmbeddingPipeline, EmbeddingPipelineConfig, EmbeddingPipelineHandle,
    PipelineEmbeddingProvider,
};
#[cfg(feature = "sqlite-vec")]
pub use crate::rag::sqlite_vec_store::SqliteVecStore;
pub use crate::rag::{
    ApiEmbeddingProvider, BatchEmbeddingProcessor, CachedEmbeddingProvider, EmbeddedChunk,
    EmbeddingConfig, EmbeddingProvider, LocalGgufEmbeddingProvider, MemoryVectorStore, TextChunker,
    VectorBackend, VectorStore, VectorStoreStats,
};
pub use dreaming::{
    DreamAction, DreamBudget, DreamCheckpoint, DreamConfig, DreamEngine, DreamMetrics, DreamPhase,
    DreamResult, DreamReviewItem, DreamReviewQueue, DreamScheduler, DreamSpeed, DreamThinking,
    KnowledgeEdge, KnowledgeGraph, KnowledgeNode, LlmCallback, ReviewStatus,
};
pub use effectiveness::{
    EffectivenessAction, EffectivenessConfig, EffectivenessStats, EffectivenessTracker, RecallEvent,
};
pub use events::{
    append_memory_event, read_memory_events, DreamPhase as EventDreamPhase, MemoryEvent,
    MemoryEventBuilder, MemoryEventLog, MEMORY_EVENT_LOG_RELATIVE_PATH,
};
pub use flush::{
    check_memory_flush, increment_compaction_count, record_flush_in_state,
    resolve_flush_target_path, FlushReason, MemoryFlushDecision,
};
pub use hybrid::hybrid_search;
pub use manager::{MemoryManager, MemoryManagerBuilder, MemoryManagerConfig, SessionContext};
pub use multimodal::{
    build_multimodal_glob, classify_multimodal_file, FileClassification, MemoryMultimodalConfig,
    MemoryMultimodalModality, MultimodalFileEntry, MultimodalStore, AUDIO_EXTENSIONS,
    DEFAULT_MEMORY_MULTIMODAL_MAX_FILE_BYTES, IMAGE_EXTENSIONS,
};
pub use personality::{MemoryContext, MemoryType, PersonalityMemory};
pub use qmd::{QmdExecutor, QmdQueryResult, QmdScope};
pub use session_search::{SearchResult, SessionSearch, SessionSearchQuery};
pub use soul::{BehaviorConfig, PreferenceConfig, SoulConfig, SoulFile};
pub use sqlite::SqliteMemoryStore;
pub use tier::{
    MemoryTier, TierAction, TierConfig, TierEvaluator, TierIndex, TierSystemConfig, TieredMemory,
    TIER_INDEX_FILE_NAME,
};
pub use tiered_store::TieredStore;
pub use vector::VectorMemoryService;
pub use workspace_state::{WorkspaceManager, WorkspaceState, WORKSPACE_STATE_VERSION};

/// Memory entry type discriminant for type-safe memory categorization
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum MemoryEntryType {
    /// Fact memory (objective information)
    #[default]
    Fact,
    /// Preference memory (user preferences)
    Preference,
    /// Context memory (conversation/session context)
    Context,
    /// User profile memory (persistent user information)
    UserProfile,
    /// Semantic search result memory (synthetic)
    Semantic,
    /// Session search result memory (synthetic)
    Session,
    /// Hybrid search result memory (synthetic)
    Hybrid,
    /// QMD query result memory (synthetic)
    Qmd,
    /// Custom memory type with string identifier
    Custom(String),
}

impl MemoryEntryType {
    /// Convert to string representation
    pub fn as_str(&self) -> &str {
        match self {
            MemoryEntryType::Fact => "fact",
            MemoryEntryType::Preference => "preference",
            MemoryEntryType::Context => "context",
            MemoryEntryType::UserProfile => "user_profile",
            MemoryEntryType::Semantic => "semantic",
            MemoryEntryType::Session => "session",
            MemoryEntryType::Hybrid => "hybrid",
            MemoryEntryType::Qmd => "qmd",
            MemoryEntryType::Custom(s) => s.as_str(),
        }
    }
}

impl From<String> for MemoryEntryType {
    fn from(s: String) -> Self {
        match s.as_str() {
            "fact" => MemoryEntryType::Fact,
            "preference" => MemoryEntryType::Preference,
            "context" => MemoryEntryType::Context,
            "user_profile" => MemoryEntryType::UserProfile,
            "semantic" => MemoryEntryType::Semantic,
            "session" => MemoryEntryType::Session,
            "hybrid" => MemoryEntryType::Hybrid,
            "qmd" => MemoryEntryType::Qmd,
            _ => MemoryEntryType::Custom(s),
        }
    }
}

impl From<&str> for MemoryEntryType {
    fn from(s: &str) -> Self {
        MemoryEntryType::from(s.to_string())
    }
}

impl From<&String> for MemoryEntryType {
    fn from(s: &String) -> Self {
        MemoryEntryType::from(s.as_str())
    }
}

impl std::fmt::Display for MemoryEntryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl From<MemoryEntryType> for String {
    fn from(mem_type: MemoryEntryType) -> Self {
        mem_type.as_str().to_string()
    }
}

/// Serializes `MemoryEntryType` as its canonical lowercase string (e.g.
/// `"fact"`), matching `as_str()`/`From<String>` and the DB column format.
impl Serialize for MemoryEntryType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// Deserializes both the canonical plain-string form (`"fact"`) and the legacy
/// adjacently-tagged form (`{"type":"Fact"}` / `{"type":"Custom","value":"x"}`)
/// written by older builds, so existing archives remain readable.
impl<'de> Deserialize<'de> for MemoryEntryType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum MemoryEntryTypeRepr {
            Str(String),
            Tagged {
                r#type: String,
                #[serde(default)]
                value: Option<serde_json::Value>,
            },
        }
        match MemoryEntryTypeRepr::deserialize(deserializer)? {
            MemoryEntryTypeRepr::Str(s) => Ok(MemoryEntryType::from(s)),
            MemoryEntryTypeRepr::Tagged { r#type, value } => {
                if r#type == "Custom" {
                    let inner = value
                        .and_then(|v| v.as_str().map(|s| s.to_string()))
                        .unwrap_or_else(|| "Custom".to_string());
                    Ok(MemoryEntryType::Custom(inner))
                } else {
                    Ok(MemoryEntryType::from(r#type))
                }
            }
        }
    }
}

impl From<&MemoryEntryType> for String {
    fn from(mem_type: &MemoryEntryType) -> Self {
        mem_type.as_str().to_string()
    }
}

/// Unique identifier for a memory entry
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryId(pub String);

impl MemoryId {
    /// Create a new memory ID
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Generate a new random ID
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl std::fmt::Display for MemoryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn default_importance_score() -> f32 {
    0.5
}

fn default_source() -> String {
    "agent".to_string()
}

fn default_access_count() -> u64 {
    0
}

/// A memory entry stored in the system
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    /// Unique identifier
    pub id: MemoryId,
    /// User ID who owns this memory
    pub user_id: String,
    /// Optional conversation ID
    pub conversation_id: Option<String>,
    /// Memory content
    pub content: String,
    /// Memory entry type (fact, preference, context, etc.)
    pub memory_type: MemoryEntryType,
    /// Optional embedding vector for semantic search
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    /// When the memory was created
    pub created_at: SystemTime,
    /// When the memory was last accessed
    #[serde(default = "SystemTime::now")]
    pub last_accessed: SystemTime,
    /// Number of times this memory has been accessed
    #[serde(default = "default_access_count")]
    pub access_count: u64,
    /// When the memory expires (None = never)
    pub expires_at: Option<SystemTime>,
    /// Additional metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Importance score in [0.0, 1.0]. Default: 0.5
    #[serde(default = "default_importance_score")]
    pub importance_score: f32,
    /// Source that created this memory (e.g., "agent", "user", "compaction")
    #[serde(default = "default_source")]
    pub source: String,
}

impl AsRef<str> for Memory {
    fn as_ref(&self) -> &str {
        &self.content
    }
}

impl Memory {
    /// Create a new memory entry
    pub fn new(
        user_id: impl Into<String>,
        content: impl Into<String>,
        memory_type: impl Into<MemoryEntryType>,
    ) -> Self {
        Self {
            id: MemoryId::generate(),
            user_id: user_id.into(),
            conversation_id: None,
            content: content.into(),
            memory_type: memory_type.into(),
            embedding: None,
            created_at: SystemTime::now(),
            last_accessed: SystemTime::now(),
            access_count: 0,
            expires_at: None,
            metadata: None,
            importance_score: 0.5,
            source: "agent".to_string(),
        }
    }

    /// Set the conversation ID
    pub fn with_conversation(mut self, conversation_id: impl Into<String>) -> Self {
        self.conversation_id = Some(conversation_id.into());
        self
    }

    /// Set the embedding vector
    pub fn with_embedding(mut self, embedding: Vec<f32>) -> Self {
        self.embedding = Some(embedding);
        self
    }

    /// Set expiration time (TTL in seconds)
    pub fn with_ttl(mut self, ttl_seconds: u64) -> Self {
        self.expires_at = Some(SystemTime::now() + std::time::Duration::from_secs(ttl_seconds));
        self
    }

    /// Set metadata
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Set the importance score
    pub fn with_importance_score(mut self, score: f32) -> Self {
        self.importance_score = score;
        self
    }

    /// Set the source label
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = source.into();
        self
    }

    /// Record an access - updates last_accessed and increments access_count
    pub fn record_access(&mut self) {
        self.last_accessed = SystemTime::now();
        self.access_count += 1;
    }

    /// Check if the memory has expired
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .map(|exp| SystemTime::now() > exp)
            .unwrap_or(false)
    }
}

/// Query options for searching memories
#[derive(Debug, Clone, Default)]
pub struct MemoryQuery {
    /// Filter by user ID
    pub user_id: Option<String>,
    /// Filter by conversation ID
    pub conversation_id: Option<String>,
    /// Filter by memory entry type
    pub memory_type: Option<MemoryEntryType>,
    /// Search query for content matching
    pub content_query: Option<String>,
    /// Maximum number of results
    pub limit: usize,
    /// Offset for pagination
    pub offset: usize,
    /// Include expired memories
    pub include_expired: bool,
}

impl MemoryQuery {
    /// Create a new query
    pub fn new() -> Self {
        Self {
            limit: 10,
            ..Default::default()
        }
    }

    /// Filter by user ID
    pub fn for_user(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// Filter by conversation ID.
    ///
    /// Matches memories bound to this conversation **plus unbound ones** —
    /// a memory stored without a conversation is a user-level fact that
    /// belongs to its user rather than to any single conversation, so it is
    /// recallable from all of them. Memories bound to a *different*
    /// conversation (compaction markers) stay scoped to theirs.
    pub fn for_conversation(mut self, conversation_id: impl Into<String>) -> Self {
        self.conversation_id = Some(conversation_id.into());
        self
    }

    /// Filter by memory entry type
    pub fn of_type(mut self, memory_type: impl Into<MemoryEntryType>) -> Self {
        self.memory_type = Some(memory_type.into());
        self
    }

    /// Search by content
    pub fn with_content(mut self, query: impl Into<String>) -> Self {
        self.content_query = Some(query.into());
        self
    }

    /// Set result limit
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Include expired memories
    pub fn include_expired(mut self) -> Self {
        self.include_expired = true;
        self
    }
}

/// Statistics about the memory store
#[derive(Debug, Clone, Default)]
pub struct MemoryStats {
    /// Total number of memories
    pub total_count: usize,
    /// Number of memories per type
    pub count_by_type: std::collections::HashMap<MemoryEntryType, usize>,
    /// Number of expired memories
    pub expired_count: usize,
}

/// Trait for memory storage backends
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Store a new memory
    async fn store(&self, memory: Memory) -> crate::Result<MemoryId>;

    /// Retrieve a memory by ID
    async fn get(&self, id: &MemoryId) -> crate::Result<Option<Memory>>;

    /// Update an existing memory
    async fn update(&self, memory: Memory) -> crate::Result<()>;

    /// Delete a memory by ID
    async fn delete(&self, id: &MemoryId) -> crate::Result<bool>;

    /// Search memories based on query
    async fn search(&self, query: MemoryQuery) -> crate::Result<Vec<Memory>>;

    /// Delete expired memories
    async fn cleanup_expired(&self) -> crate::Result<usize>;

    /// Get statistics
    async fn stats(&self) -> crate::Result<MemoryStats>;

    /// Close the store (clean up resources)
    async fn close(&self) -> crate::Result<()>;

    /// Atomically update a memory's importance score and return the updated
    /// memory.
    ///
    /// The default implementation is **not** atomic: it reads the memory, sets
    /// the new score, and calls [`Self::update`]. Backends that support atomic
    /// read-modify-write should override this to avoid lost updates under
    /// concurrency.
    async fn update_importance_score(
        &self,
        id: &MemoryId,
        new_score: f32,
    ) -> crate::Result<Option<Memory>> {
        let Some(mut memory) = self.get(id).await? else {
            return Ok(None);
        };
        if (memory.importance_score - new_score).abs() < 0.001 {
            return Ok(Some(memory));
        }
        memory.importance_score = new_score;
        self.update(memory.clone()).await?;
        Ok(Some(memory))
    }

    /// Return the concrete store as a [`TieredStore`] if it is one.
    ///
    /// Used by the effectiveness feedback loop to trigger explicit tier
    /// migrations based on recall hit rates.
    fn as_tiered_store(&self) -> Option<&TieredStore> {
        None
    }
}

/// A chat message for conversation history
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Unique identifier
    pub id: String,
    /// Conversation ID
    pub conversation_id: String,
    /// User ID
    pub user_id: String,
    /// Message role (user, assistant, system)
    pub role: String,
    /// Message content
    pub content: String,
    /// When the message was created
    pub created_at: SystemTime,
    /// Optional metadata (e.g., tool calls, tokens used)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl ChatMessage {
    /// Create a new chat message
    pub fn new(
        conversation_id: impl Into<String>,
        user_id: impl Into<String>,
        role: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            conversation_id: conversation_id.into(),
            user_id: user_id.into(),
            role: role.into(),
            content: content.into(),
            created_at: SystemTime::now(),
            metadata: None,
        }
    }

    /// Set metadata
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// A durable compaction record for a conversation.
///
/// When an agent context is compacted (LLM or heuristic), the summary and a
/// boundary anchor are recorded here so that a later restart can replay
/// `[summary] + messages-after-boundary` instead of the full history. The
/// original messages stay in `chat_messages` untouched; this record only
/// remembers where the compaction mask starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationCompaction {
    /// Conversation the record belongs to (one active record per conversation).
    pub conversation_id: String,
    /// Role of the boundary message (`"user"` or `"assistant"`).
    pub boundary_role: String,
    /// Content of the boundary message — the first message the compaction mask
    /// keeps. Used as an anchor to locate the boundary in `chat_messages`.
    pub boundary_content: String,
    /// Summary message content, replayed verbatim in front of the tail.
    pub summary: String,
    /// When the record was created.
    pub created_at: SystemTime,
}

/// Trait for chat history storage
#[async_trait]
pub trait ChatHistoryStore: Send + Sync {
    /// Store a chat message
    async fn store_message(&self, message: ChatMessage) -> crate::Result<()>;

    /// Get chat history for a conversation
    async fn get_conversation_history(
        &self,
        conversation_id: &str,
        limit: usize,
    ) -> crate::Result<Vec<ChatMessage>>;

    /// Get list of conversations for a user
    async fn get_user_conversations(
        &self,
        user_id: &str,
        limit: usize,
    ) -> crate::Result<Vec<String>>;

    /// Delete a conversation and all its messages
    async fn delete_conversation(&self, conversation_id: &str) -> crate::Result<()>;

    /// Get the most recent conversation ID for a user
    async fn get_last_conversation(&self, user_id: &str) -> crate::Result<Option<String>>;

    /// Record the durable boundary of a context compaction.
    ///
    /// The boundary anchor is the first message the mask keeps (everything
    /// before it is replaced by `summary` on rehydration). One active record
    /// per conversation — a later compaction overwrites the previous one.
    async fn record_compaction(
        &self,
        conversation_id: &str,
        boundary_role: &str,
        boundary_content: &str,
        summary: &str,
    ) -> crate::Result<()>;

    /// Load the active compaction record for a conversation, if any.
    async fn get_compaction(
        &self,
        conversation_id: &str,
    ) -> crate::Result<Option<ConversationCompaction>>;

    /// Get conversation history starting at the boundary anchor (the first
    /// message whose `(role, content)` matches, i.e. the last occurrence).
    ///
    /// Returns an empty vec when the anchor cannot be located — callers
    /// should fall back to the full history.
    async fn get_conversation_history_since(
        &self,
        conversation_id: &str,
        boundary_role: &str,
        boundary_content: &str,
        limit: usize,
    ) -> crate::Result<Vec<ChatMessage>>;
}

/// Calculate cosine similarity between two vectors
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot_product / (norm_a * norm_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_id() {
        let id = MemoryId::new("test_id");
        assert_eq!(id.0, "test_id");
    }

    #[test]
    fn test_memory_creation() {
        let memory = Memory::new("user1", "Hello world", "fact")
            .with_conversation("conv1")
            .with_ttl(3600);

        assert_eq!(memory.user_id, "user1");
        assert_eq!(memory.content, "Hello world");
        assert_eq!(memory.memory_type, MemoryEntryType::Fact);
        assert_eq!(memory.conversation_id, Some("conv1".to_string()));
        assert!(memory.expires_at.is_some());
        assert!(!memory.is_expired());
    }

    #[test]
    fn test_memory_query() {
        let query = MemoryQuery::new()
            .for_user("user1")
            .of_type("fact")
            .limit(5);

        assert_eq!(query.user_id, Some("user1".to_string()));
        assert_eq!(query.memory_type, Some(MemoryEntryType::Fact));
        assert_eq!(query.limit, 5);
    }

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let c = vec![1.0, 0.0, 0.0];

        // Orthogonal vectors have 0 similarity
        assert!((cosine_similarity(&a, &b) - 0.0).abs() < 0.001);

        // Same vectors have 1.0 similarity
        assert!((cosine_similarity(&a, &c) - 1.0).abs() < 0.001);

        // Empty vectors return 0
        assert_eq!(cosine_similarity(&[], &[]), 0.0);

        // Different length vectors return 0
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0);
    }
}
