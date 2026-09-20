# Browser Module

Browser automation module for Syscity, providing web interaction capabilities for the AI assistant.

## Design

All browser functionality is gated behind the `browser` Cargo feature.

- **`pool.rs`** — `BrowserPool`: Persistent browser instance caching with idle eviction
  - `BrowserInstance` — Managed browser instance with lifecycle tracking
  - `PageHandle` — Reference to a page within a browser instance
- **`aria_snapshot.rs`** — ARIA Snapshot: LLM-friendly accessible tree with ref markers
  - `AriaSnapshot` — Serializable accessibility tree
  - `act_by_ref()` — Execute actions by ARIA reference markers
- **`profile.rs`** — Profile Management: Multiple browser configs (headless/headed, Chrome MCP)
  - `BrowserProfile` — Named profile with viewport, headless, and driver settings
  - `BrowserPoolConfig` — Idle timeout and cleanup interval settings
  - `BrowserDriver` — Driver selection (Chrome, Chromium, etc.)
- **`bridge.rs`** — Bridge Server: HTTP API decoupling for browser operations
  - `BrowserBridge` — Local HTTP server exposing browser operations
- **`bridge_client.rs`** — Bridge Client: HTTP client for the bridge server
  - `BridgeClient` — Async client for bridge operations
- **`navigation_guard.rs`** — SSRF Guard: Navigation security with allow/deny lists
  - `NavigationPolicy` — Policy for allowed/disallowed URLs
  - `assert_navigation_allowed()` — Pre-flight navigation check
- **`sandbox.rs`** — Docker-isolated browser sandbox (P3)

## Key Types

```rust
pub struct BrowserPool {
    instances: HashMap<String, BrowserInstance>,
    config: BrowserPoolConfig,
}

pub struct BrowserInstance {
    profile: BrowserProfile,
    pages: HashMap<String, PageHandle>,
    last_used: Instant,
}

pub struct BrowserProfile {
    pub name: String,
    pub headless: bool,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub driver: BrowserDriver,
}

pub enum BrowserDriver {
    Chrome,
    Chromium,
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
Agent Request
    │
    ▼
BrowserPool::get_or_create_instance()
    │
    ├──▶ BrowserInstance (cached or new)
    │       │
    │       ▼
    │   aria_snapshot() ──▶ AriaSnapshot (LLM-readable tree)
    │       │
    │       ▼
    │   act_by_ref(ref, action) ──▶ Page interaction
    │
    └──▶ BridgeClient (if bridge mode)
            │
            ▼
        BrowserBridge HTTP API
```

## Implemented Features

- Browser instance pooling with idle eviction
- **Page reuse across tool calls**: a pooled instance hands the next tool call
  the page the previous call left behind (with a one-line liveness probe that
  replaces a page closed or crashed since), so a Navigate in one call and a
  Type in the next operate on the same tab. Previously every call opened a
  fresh `about:blank`, which made cross-call sessions impossible.
- ARIA snapshot generation for LLM-friendly page representation
- Action execution by reference markers (click, fill, press)
- Multiple browser profiles (headless/headed, viewport sizes)
- Bridge server for HTTP-based browser decoupling
- Bridge client for remote browser control
- Navigation guard with SSRF protection
- Docker sandbox support for isolated browser execution
- Compile-time feature gating (`browser` feature)

