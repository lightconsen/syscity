# Gateway Module

The control plane for Syscity, managing channels, agents, and the HTTP/WebSocket API.

## Design

- **`Gateway`** — Main struct that owns:
  - `GatewayState` — shared state (memory manager, channel registry, agent pool, tool registry, plugin manager)
  - Axum router for HTTP API + WebSocket
  - Channel lifecycle management
- **`GatewayState`** — `Arc`-shared state with `RwLock` fields for dynamic components
- **`GatewayConfig`** — Comprehensive runtime configuration with all subsystem fields
- **Auth** (`auth.rs`) — JWT-based authentication, API key validation
- **Rate Limiting** (`rate_limit.rs`) — Token bucket rate limiter per client
- **Webhooks** (`webhooks/`) — Incoming webhook handlers, one module per provider
- **Middleware** (`middleware.rs`) — CORS, auth, logging, trusted proxy auth
- **Protocol** (`protocol.rs`) — ACP protocol handlers
- **Commands** (`commands.rs`) — Gateway control commands
- **WebSocket** (`ws/`) — Real-time bidirectional message streaming; per-topic method modules include `config_ws.rs`, `models.rs`, `sessions.rs`, `tasks.rs`, `skills_ws.rs`, `logs.rs`, `workspace.rs` (agent workspace browser), and `ask.rs` (`ask.respond`)
- **Send Policy** (`send_policy.rs`) — Message send policy enforcement
- **Hooks** (`hooks.rs`) — Gateway-level hooks system
- **Command Provider** (`command_provider.rs`) — Command resolution and provisioning
- **Handlers** (`handlers/`) — REST API handlers for health, device pairing, artifact serving (`/api/v1/artifacts/*path`), admin, etc.

### Module Layout

`gateway/mod.rs` was split into focused submodules (2026-06-20) to keep the
control-plane core readable, and two of those submodules — `lifecycle/` and
`config/` — grew into directory modules of their own (2026-09-22). The entry
point (`mod.rs`) now holds `GatewayState` access checks and the `Gateway`
struct shell; behavior lives in:

- **`lifecycle/`** — `start_gateway` / `stop_gateway` / `build_router` free
  functions (startup sequence, graceful shutdown, Axum router assembly), in
  `start.rs`, `shutdown.rs` and `router.rs`, with the helpers they share
  (agent spawning, the quality-gate check, MCP tool registration) in
  `helpers.rs`.
- **`dispatch.rs`** — inbound message entry worker and routed message dispatch.
- **`hot_reload.rs`** — config-change handlers for Main / Agent / Channel /
  Plugin / Gateway file types.
- **`init/`** — subsystem constructors: `channels.rs`,
  `storage.rs`, `agents.rs`, `pipelines.rs`, `security.rs`,
  `services.rs`, `tools.rs`.
- **`runtime.rs`** — runtime event/command types (`BufferedMessage`,
  `AgentHandle`, `AgentCommand`, `AgentQuery`, `GatewayEvent`, `AgentStatus`).
- **`agent_spawn.rs`** — `spawn_agent_inner` and adapter wiring.
- **`config/`** — the `GatewayConfig` tree: `mod.rs` holds `GatewayConfig`
  itself and the `*Config` structs are grouped by area across `memory.rs`,
  `misc.rs`, `optimizer.rs`, `plugins.rs`, `security.rs` and `tui.rs`.
- **`state.rs`** / **`types.rs`** / **`watchdog.rs`** — shared state,
  request/response DTOs, repair/watchdog logic.

### Startup Flow

1. Load configuration
2. Initialize `MemoryManager` (tiered or unified based on config)
3. Initialize `ToolRegistry` with built-in + MCP tools
4. Initialize `ChannelRegistry` with configured channels
5. Start channels (`init_channels()`)
6. Start `DreamScheduler` (if tiered memory is enabled)
7. Start HTTP server (Axum)

### GatewayConfig Fields

| Category | Fields |
|----------|--------|
| Network | `host`, `port` |
| Agent | `default_agent` |
| Channels | `channels` (HashMap) |
| Memory | `vector_memory` |
| Plugins | `plugins` |
| Hot Reload | `hot_reload` |
| ACP | `acp` |
| Cron | `cron` |
| Heartbeat | `heartbeat` |
| Security | `security` |
| Storage | `storage` |
| Providers | `providers` (HashMap) |
| Model | `model`, `model_provider` |
| MCP | `mcp` |
| Cost | `cost_guard` |
| Workspace | `workspace_dir`, `workspace_only` |
| Browser | `browser` |
| Computer | `computer` |
| Dreaming | `dreaming` |
| Standing Orders | `standing_orders` |
| Capabilities | `capabilities` |

## Key Types

```rust
pub struct Gateway {
    state: Arc<GatewayState>,
    config: GatewayConfig,
    listener: Option<TcpListener>,
}

pub struct GatewayState {
    pub memory_manager: RwLock<Option<MemoryManager>>,
    pub channel_registry: RwLock<ChannelRegistry>,
    pub tool_registry: Arc<ToolRegistry>,
    pub plugin_manager: RwLock<PluginManager>,
    // ...
}
```

```rust
pub struct GatewayConfig {
    pub host: String,
    pub port: u16,
    pub default_agent: AgentConfig,
    pub channels: HashMap<String, ChannelConfig>,
    pub vector_memory: VectorMemoryConfig,
    pub plugins: PluginConfig,
    pub hot_reload: HotReloadConfig,
    pub acp: AcpConfig,
    pub cron: CronConfig,
    pub heartbeat: HeartbeatConfig,
    pub security: SecurityConfig,
    pub storage: StorageConfig,
    pub providers: HashMap<String, ProviderConfig>,
    pub model: String,
    pub model_provider: String,
    pub mcp: McpSettings,
    pub cost_guard: CostGuardConfig,
    pub workspace_dir: Option<PathBuf>,
    pub workspace_only: bool,
    pub browser: BrowserConfig,
    pub computer: ComputerConfig,
    pub dreaming: MemoryDreamingConfig,
    pub standing_orders: StandingOrderConfig,
    pub capabilities: CapabilitiesConfig,
}
```

## Implemented Features

- Axum-based HTTP API with REST endpoints
- WebSocket for real-time bidirectional streaming
- Multi-channel lifecycle management
- Agent pool with spawn and lifecycle control
- Tool registry integration with built-in and MCP tools
- Plugin manager integration
- Memory manager initialization (tiered or unified)
- Dream scheduler startup
- JWT and API key authentication
- Token bucket rate limiting
- CORS and trusted proxy middleware
- Webhook handlers for external integrations
- ACP protocol handlers for subagent control
- Send policy enforcement
- Gateway-level hooks system
- Command provider for dynamic command resolution
- Health check and admin handlers
- Admin endpoints for provider switching and status
- Online self-update endpoints (`/api/v1/update`, `/api/v1/update/status`, `/api/v1/update/progress`)
- Cost guard configuration
- Workspace boundary enforcement
- Config snapshot and diff for change tracking
- Agent workspace file browsing over WebSocket (`workspace.list` / `workspace.read`) with path-traversal protection
- Artifact serving from per-agent workspaces (`/api/v1/artifacts/@<owner>/...`, `@default` = shared workspace) with fallback to the legacy `~/.syscity/artifacts/` directory
- Secret masking on all config read surfaces (WS `config.get`, gateway tool `config.get` / `config.schema.lookup`, REST config handler) via `secrets::mask_json_value`
- Config revision CAS: `config.get` returns a SHA-256 `revision`; `config.set` accepts `base_revision` and rejects stale writes with `REVISION_CONFLICT`
- Human-in-the-loop `ask_user` flow: `ask.required` / `ask.resolved` events forwarded onto the WS bus, answered via `ask.respond`

