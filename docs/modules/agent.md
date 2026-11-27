# Agent Module

The central orchestrator that handles conversations, manages context, calls tools, and interacts with LLM providers.

## Design

- **`context.rs`** — Builds and maintains the conversation context sent to the LLM. Appends a transient `[state snapshot]` user message (calendar date / weekday / timezone offset) to the tail of every request — it lives only in the request, never in history, so persistence, compaction, and undo are unaffected, and the system-prompt prefix stays byte-stable across threads (provider KV-cache reuse). The exact current time is available via the `time` tool; time is no longer baked into the system prompt.
- **`budget.rs`** / **`disk_budget.rs`** / **`cost_guard.rs`** — Tracks iteration count, token usage, and disk budget to prevent runaway loops.
- **`compressor.rs`** — Compresses conversation history when approaching token limits. `compact_with_llm` and `summarize()` snap the cut so the kept tail never starts on a tool result and a `tool_call` is never orphaned from its results (strict providers reject unbalanced sequences).
- **`session.rs`** / **`session_store.rs`** / **`session_files.rs`** — Persists and restores agent session state. On session load, `repair_orphan_tool_calls()` detects assistant tool calls that never received a persisted result (crash mid-turn) and appends a synthetic `tool` row per orphan, prefixed with the `TOOL_OUTCOME_UNKNOWN` sentinel; the repair is idempotent (synthetic rows are tagged in metadata). A per-request side table (`request_snapshots`) records one write-once debug row per outgoing LLM request — model id, full system prompt, and offered tool schemas — pruned by the same observability sweep as `llm_calls` (`observe.retention_days`).
- **`todo.rs`** — Per-session task list that the agent can read and update. Writes are whole-snapshot replaces (the tool input IS the complete new list; last write wins, no partial-update corner states), and a new user turn automatically clears the previous turn's plan surface (active plan + persisted todo snapshot, memory and disk).
- **`planner.rs`** — Decomposes complex user requests into multi-step plans.
- **`prompt_builder.rs`** — Assembles system prompts with personality and tool descriptions.
- **`subagent_registry.rs`** — Tracks spawned subagents for delegation.
- **`acp.rs`** — Agent Control Protocol integration for pause/resume/step/cancel.
- **`transcript.rs`** — Records full conversation transcripts for replay.
- **`compaction.rs`** — Session memory flush and compaction logic. Compaction is durable: the boundary and summary are recorded in a `conversation_compactions` table (one active record per conversation), so `build_fresh_context` rehydrates `[summary + tail]` after a restart instead of replaying full history (falling back to full history when the boundary anchor cannot be located). Both completion paths compact and retry once when the provider reports a `ContextLength` overflow.
- **`group.rs`** — Multi-agent group session management.
- **`personality.rs`** — Agent personality and agent registry.
- **`turns.rs`** — Thread and turn management for conversation threading.
- **`artifacts.rs`** — Artifact store for generated files and outputs.
- **`heuristics.rs`** — Desktop/complex task detection keyword heuristics (`is_desktop_task` / `is_complex_task`, EN + ZH keyword lists).

## Key Types

```rust
pub struct Agent {
    config: ConfigCell, // runtime-updatable, copy-on-clone
    agent_id: String,
    provider: Arc<dyn Provider>,
    model: Option<String>,
    tools: Arc<ToolRegistry>,
    thread_map: Arc<Mutex<HashMap<String, Thread>>>,
    session_store: Option<Arc<SessionStore>>,
    memory_manager: Option<Arc<MemoryManager>>,
    // ...
}
```

```rust
pub enum ProgressEvent {
    Started,
    ToolCalling { name: String, arguments: String },
    ToolResult { name: String, result: String, data: Option<serde_json::Value>, execution_time_ms: u64 },
    ToolResultDelta { name: String, chunk: String, is_error: bool },
    Generating { content: Option<String> },
    ContentDelta { text: String },
    Completed { response: String },
    Error { message: String },
}
```

## Data Flow

1. `process_message()` receives an `IncomingMessage`
2. `build_fresh_context()` calls `MemoryManager::session_context()` to retrieve memories + multimodal references + recent messages
3. Context is injected into the LLM prompt
4. LLM response is parsed; if it contains tool calls, they are executed via `ToolRegistry`
5. Tool results are fed back to the LLM
6. Final response is formatted and returned

## Implemented Features

- Message processing with progress callbacks
- Context building with memory retrieval and multimodal support
- Iteration and token budget tracking with `CostGuard`
- Conversation compression when approaching token limits, with tool-pair-safe cut points
- Durable compaction: boundary + summary persisted per conversation, rehydrated as `[summary + tail]` on restart, with compact-and-retry-once on provider context-length overflow
- Per-request state snapshot (date/weekday/timezone) appended to every LLM request as a transient user message
- Session persistence and restoration, with crash-recovery repair of orphaned tool calls at load time
- Per-request debug snapshots (model, system prompt, tool schemas) in the `request_snapshots` side table
- Per-session todo list management
- Goal planning with task decomposition
- Subagent spawning and registry tracking
- ACP integration with pause/resume/step/cancel execution control
- Full transcript recording and replay
- Session compaction and memory flush
- Multi-agent group sessions
- Agent personality and template parameter system
- Thread and turn management for conversation threading
- Artifact generation and storage
- Desktop task detection heuristic (`is_desktop_task`)
- Complex task detection heuristic (`is_complex_task`)
- Two-level model binding with dispatch-time resolution: session pin (SQLite, `sessions.set_model`) → per-agent binding (`agent_models` in config) → global default
- Per-agent config overrides applied to running agents (`Agent.config` is a copy-on-clone `RwLock` cell snapshotted once per request build, so updates take effect from the next turn)

