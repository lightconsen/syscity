# Memory Module

Persistent storage for conversations, messages, and memories with semantic search, tiered routing, and background dreaming.

## Design

### Storage Traits

```rust
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn store(&self, memory: Memory) -> Result<MemoryId>;
    async fn get(&self, id: &MemoryId) -> Result<Option<Memory>>;
    async fn update(&self, memory: Memory) -> Result<()>;
    async fn delete(&self, id: &MemoryId) -> Result<bool>;
    async fn search(&self, query: MemoryQuery) -> Result<Vec<Memory>>;
    async fn cleanup_expired(&self) -> Result<usize>;
    async fn stats(&self) -> Result<MemoryStats>;
    async fn close(&self) -> Result<()>;
    fn as_tiered_store(&self) -> Option<&TieredStore>;
}

#[async_trait]
pub trait ChatHistoryStore: Send + Sync {
    async fn store_message(&self, message: ChatMessage) -> Result<()>;
    async fn get_conversation_history(&self, conversation_id: &str, limit: usize) -> Result<Vec<ChatMessage>>;
    async fn get_user_conversations(&self, user_id: &str, limit: usize) -> Result<Vec<String>>;
    async fn delete_conversation(&self, conversation_id: &str) -> Result<()>;
    async fn get_last_conversation(&self, user_id: &str) -> Result<Option<String>>;
}
```

### Backends

| Backend | Tier | Storage |
|---------|------|---------|
| `InMemoryStore` | Working | Ephemeral HashMap |
| `DatabaseStore` | ShortTerm, LongTerm | SQLite (sqlx) with WAL + FTS5 |
| `CompressedJsonlStore` | Archival | Gzip-compressed JSON Lines |

### Tiered Store

`TieredStore` routes each memory to its tier-specific backend based on `TierEvaluator::entry_tier()`. A `TierIndex` tracks which tier holds each memory ID for fast lookups.

- **Promotion**: Working → ShortTerm → LongTerm → Archival
- **Demotion**: Archival → LongTerm → ShortTerm → Working
- **Eviction**: When TTL expires or tier is disabled

### Memory Manager

`MemoryManager` is the high-level facade:
- `store: Arc<dyn MemoryStore>` — tiered or unified
- `chat_history: Arc<dyn ChatHistoryStore>` — always SQLite-backed
- `session_context()` — builds `SessionContext` with recent messages, retrieved memories, and multimodal references
- `retrieve()` — semantic search with optional embedding; also queries QMD executor (if available) and merges results

### Dreaming

`DreamScheduler` runs background cycles on a cron schedule:
- **Light Dream** — deduplication (embedding cosine similarity > threshold, fallback to text hash), tag cleanup, expired cleanup. Records `PromotionApplied` events on tier changes.
- **Deep Dream** — topic clustering, summary generation
- **REM Dream** — cross-session association, pattern discovery, knowledge graph update. Graph is persisted to `memory/.dreams/knowledge_graph.json`.

Started in `Gateway::start()` with `event_log` and `workspace_dir` wired in.

### Event System

`MemoryEventLog` writes JSONL to `memory/.dreams/events.jsonl`:
- `RecallRecorded` — when memories are recalled into session context
- `PromotionApplied` — when memories are promoted/demoted between tiers
- `CompactCompleted` — when a session is compacted into semantic memories
- `DreamCompleted` — when a dream phase finishes

### Subsystems

- **Multimodal** (`multimodal.rs`) — File classification (image/audio), glob scanning, path management
- **Events** (`events.rs`) — JSONL event log for recall, promotion, compact, and dream events
- **QMD** (`qmd.rs`) — Query Markdown/Document CLI wrapper with `QmdScope` (channel, chat_type, key_prefix, allow/deny) access control; wired into `retrieve()`
- **Vector** (`vector.rs`) — Embedding providers (API, cached, local GGUF), text chunking, vector stores
- **Hybrid Search** (`hybrid.rs`) — Combines semantic + keyword with MMR rerank and temporal decay
- **Effectiveness** (`effectiveness.rs`) — Tracks recall hit rates to tune memory importance
- **Session Search** (`session_search.rs`) — Search across conversation history
- **Personality** (`personality.rs`) — Conversation pattern analysis and SOUL.md auto-generation
- **Personality memory** (`personality.rs`, `PersonalityMemory`) — the agent's markdown identity and memory files. Two axes of isolation: identity files (`SOUL.md`, `IDENTITY.md`, `BOOTSTRAP.md`, `AGENTS.md`, `TOOLS.md`, `HEARTBEAT.md`) belong to the agent and stay shared; personal files (`USER.md`, `MEMORY.md`, and the dated `memory/*.md` fragments) belong to one human and live under `users/<component>/`. A user-scoped instance reads its own copy or nothing — there is no fallback to the shared file, because silencing one person by handing them another's would be a leak with a friendly face. Dated fragments are the exception: they *merge*, with a personal fragment shadowing the shared one of the same name. `shared` / `anonymous` / `ws-user` mean "this deployment has no per-user identity", so they keep the flat layout an upgrade must not move files out from under.
- **Soul** (`soul.rs`) — Soul/personality file management with `SoulConfig`
- **Workspace State** (`workspace_state.rs`) — Workspace-level state persistence
- **Flush** (`flush.rs`) — Memory flush decision logic for compaction
- **Pipeline** (`pipeline.rs`) — Embedding pipeline with background job processing
- **Local Embeddings** (`local_embeddings.rs`) — Local GGUF embedding model support (behind `local-embeddings` feature)
- **PgVector** (`pgvector_store.rs`) — PostgreSQL pgvector backend (behind `pgvector` feature)
- **SQLite-Vec** (`sqlite_vec_store.rs`) — SQLite vector extension backend (enabled by default via the `sqlite-vec` feature)

## Implemented Features

- Tiered memory store with promotion/demotion/eviction
- SQLite-backed chat history with WAL + FTS5
- Semantic search with embedding providers (API, cached, local GGUF)
- Hybrid search with MMR rerank and temporal decay
- Background dreaming scheduler (Light/Deep/REM phases)
- Knowledge graph persistence for cross-session associations
- Memory event logging (JSONL) for operational visibility
- Effectiveness tracking with closed-loop feedback into tier evaluation
- QMD integration for document-aware retrieval
- Multimodal file classification and storage
- Session search across conversation history
- Personality analysis and SOUL.md auto-generation
- Per-user personality memory: identity files shared, `USER.md` / `MEMORY.md` / dated fragments partitioned by principal under `users/`
- Dream review queue with human approval/rejection
- Dream observability dashboard with metrics and Prometheus export
- Memory export/import for migration (JSON/JSONL)
- Local embedding model support (GGUF)
- Multiple vector backends (SQLite-vec default, PostgreSQL pgvector, in-memory)
- Workspace state persistence
- Embedding pipeline with background job processing

