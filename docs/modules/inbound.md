# Inbound Module

Inbound pipeline for processing all user messages before they reach the agent.

## Design

The inbound pipeline replaces the direct "Channel -> Gateway -> Agent" path with a layered processing pipeline:

```
Channel Extension -> Debounce -> Media Understanding -> Dispatch
-> Queue Mode Resolve -> Agent Router -> Agent
```

- **`debounce.rs`** — `InboundDebouncer` with configurable delay for batching rapid messages
- **`media.rs`** — `MediaUnderstandingPipeline` for processing attachments (images, audio)
- **`dispatch.rs`** — `AutoReplyDispatch` with send policy and plugin-owned binding checks
- **`queue.rs`** — `QueueModeResolver` for determining queue mode (sync/async/batch)
- **`router.rs`** — `AgentRouter` with binding store for agent selection. Its
  global default is the switchable one: `agents.default` calls
  `AgentRouter::set_default_agent`, so a switch changes where unrouted messages
  go on the next turn. Already-bound sessions keep their agent.

### Pipeline Stages

| Stage | File | Purpose |
|-------|------|---------|
| 0 | Identity validation | Pre-debounce sender verification |
| 1 | Debounce | Batch rapid messages from the same sender |
| 2 | Media | Process attachments (OCR, transcription, classification) |
| 3 | Dispatch | Apply send policy, check plugin bindings |
| 4 | Envelope | Track session context |
| 5 | Queue | Resolve queue mode |
| 6 | Router | Select target agent and workspace |

## Key Types

```rust
pub struct RoutedMessage {
    pub incoming: IncomingMessage,
    pub agent_id: String,
    pub workspace_id: Option<String>,
    pub queue_mode: QueueMode,
    pub suppress_delivery: bool,
    pub media_results: Option<MediaUnderstandingResult>,
}

pub trait InboundPipeline: Send + Sync {
    async fn process(&self, message: IncomingMessage) -> Option<RoutedMessage>;
    async fn flush(&self, key: &str) -> Vec<RoutedMessage>;
}

pub struct DefaultInboundPipeline {
    debouncer: Arc<InboundDebouncer>,
    media_pipeline: MediaUnderstandingPipeline,
    dispatch: AutoReplyDispatch,
    queue_resolver: QueueModeResolver,
    router: AgentRouter,
    routed_tx: mpsc::Sender<RoutedMessage>,
}

pub enum QueueMode {
    Sync,       // Process immediately
    Async,      // Queue for later processing
    Batch,      // Batch with other messages
}

pub struct AgentRouter {
    binding_store: Arc<dyn BindingStore>,
    config: AgentRouterConfig,
}
```

## Data Flow

```
IncomingMessage
    │
    ▼
IdentityValidator (warn-only)
    │
    ▼
InboundDebouncer
    │
    ├──▶ Absorbed (batched) ──▶ flush() ──▶ process_stages()
    └──▶ Passed through ──▶ process_stages()
                                │
                                ├──▶ MediaUnderstandingPipeline
                                ├──▶ AutoReplyDispatch
                                ├──▶ SessionEnvelopeManager
                                ├──▶ QueueModeResolver
                                └──▶ AgentRouter
                                        │
                                        ▼
                                    RoutedMessage ──▶ routed_tx
```

## Implemented Features

- Layered inbound message processing pipeline
- Message debouncing with configurable delay
- Media attachment processing pipeline
- Send policy dispatch with suppression support
- Session envelope tracking
- Queue mode resolution (sync/async/batch)
- Agent routing with binding store (in-memory and SQLite backends)
- Identity validation pre-checks
- Plugin-owned binding support
- Workspace hint routing
- Flush support for shutdown and explicit flush requests

