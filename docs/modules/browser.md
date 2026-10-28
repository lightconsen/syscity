# Browser Module

Browser automation module for Syscity, providing web interaction capabilities for the AI assistant.

## Design

All browser functionality is gated behind the `browser` Cargo feature.

- **`pool.rs`** — `BrowserPool`: Persistent browser instance caching with idle eviction
  - `BrowserInstance` — Managed browser instance with lifecycle tracking
  - `PageHandle` — Reference to a page within a browser instance, plus the
    conversation that opened it
- **`aria_snapshot.rs`** — ARIA Snapshot: LLM-friendly accessible tree with ref markers
  - `AriaSnapshot` — Serializable accessibility tree
  - `act_by_ref()` — Execute actions by ARIA reference markers
- **`profile.rs`** — Profile Management: Multiple browser configs (headless/headed, Chrome MCP)
  - `BrowserProfile` — Named profile with viewport, headless, and driver settings
  - `BrowserPoolConfig` — Idle timeout and cleanup interval settings
  - `BrowserDriver` — `Managed` (chromiumoxide launches Chrome) or
    `ChromeMcp { cdp_url }` (attach to a Chrome that is already running)
- **`bridge.rs`** — Bridge Server: HTTP API decoupling for browser operations
  - `BrowserBridge` — Local HTTP server exposing browser operations (off by
    default; enable with `[browser] bridge_enabled`)
- **`bridge_client.rs`** — Bridge Client: HTTP client for the bridge server
  - `BridgeClient` — Async client for bridge operations (reads
    `BROWSER_BRIDGE_URL`; not currently called by the gateway — in-process
    calls go straight to the pool)
- **`navigation_guard.rs`** — SSRF Guard: Navigation security with allow/deny lists
  - `NavigationPolicy` — Policy for allowed/disallowed URLs
  - `assert_navigation_allowed()` — Pre-flight navigation check
- **`sandbox.rs`** — Docker-isolated browser sandbox. Implemented but not yet
  wired to any call site; running everything behind `docker` is what the P3
  item was for, and nothing calls it today.

## Key Types

```rust
pub struct BrowserPool {
    instances: Arc<RwLock<HashMap<String, Arc<BrowserInstance>>>>,
    config: BrowserPoolConfig,
    profiles: Arc<RwLock<HashMap<String, BrowserProfile>>>,
}

pub struct BrowserInstance {
    pub browser: Arc<chromiumoxide::Browser>,
    pub pages: RwLock<HashMap<String, PageHandle>>,
    pub profile: BrowserProfile,
    pub last_used: RwLock<Instant>,
}

pub struct PageHandle {
    pub target_id: String,
    pub page: Arc<chromiumoxide::Page>,
    pub last_used: Instant,
    /// The conversation that opened this page; `None` for non-conversation
    /// callers (the PDF renderer, the remote bridge).
    pub owner: Option<String>,
}

pub struct BrowserProfile {
    pub name: String,
    pub headless: bool,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub driver: BrowserDriver,
}

pub enum BrowserDriver {
    Managed,
    ChromeMcp { cdp_url: String },
}

pub struct AriaSnapshot {
    pub nodes: Vec<AriaNodeLine>,
}

pub enum ActKind {
    Click,
    Fill { value: String },
    Press { key: String },
}
```

## Data Flow

```
Agent Request  (ToolContext.identity.conversation_id = owner)
    │
    ▼
BrowserTool::execute → execute_actions
    │
    ├── pool present (for_gateway / for_eval)
    │       │
    │       ▼
    │   BrowserPool::get_or_create(profile)
    │       │
    │       ▼
    │   BrowserInstance::most_recent_page_of(owner)
    │       │  — this conversation's page, or a new one opened as its own
    │       ▼
    │   execute_single_action(...)  ──▶ Page interaction
    │       │
    │       ├── Snapshot ──▶ AriaSnapshot (LLM-readable tree with ref markers)
    │       └── Act { ref_id } ──▶ act_by_ref() ──▶ Page interaction
    │
    └── no pool (BrowserTool::new)
            │
            ▼
        launch a Chrome per call, run, let it die

Bridge (when enabled) and PDF export share the pool's instances but address
pages by explicit target id, so they use the unrestricted `*_of(None)` forms.
```

## Implemented Features

- Browser instance pooling with idle eviction
- **Page reuse across tool calls**: a pooled instance hands the next tool call
  the page the previous call left behind (with a one-line liveness probe that
  replaces a page closed or crashed since), so a Navigate in one call and a
  Type in the next operate on the same tab. Previously every call opened a
  fresh `about:blank`, which made cross-call sessions impossible.
- **Conversation-scoped pages**: the gateway builds one tool registry and
  shares it with every conversation, so the pool is shared too. Each page
  records the conversation that opened it; `most_recent_page_of`,
  `list_pages_of` and `find_page_by_title_of` return that conversation's pages
  and nothing else, so one conversation's tab never becomes another's. Pages
  it did not open belong to another conversation or to the PDF renderer and
  the bridge — those address their pages by explicit target id and never
  list, so the unrestricted `*_of(None)` forms are scoped to them.
- **Pool wired into both registries**: `BrowserTool::for_gateway` (the
  gateway's registry, sized from `[browser]`) and `BrowserTool::for_eval` (a
  standalone eval run). `BrowserTool::new()` keeps the pool-less per-call
  behavior for tests and one-off use.
- ARIA snapshot generation for LLM-friendly page representation
- Action execution by reference markers (click, fill, press)
- Multiple browser profiles (headless/headed, viewport sizes)
- Bridge server for HTTP-based browser decoupling (opt-in)
- Navigation guard with SSRF protection
- Compile-time feature gating (`browser` feature)

