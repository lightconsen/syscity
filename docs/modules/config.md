# Config Module

Configuration management, hot reload system, and runtime reconfiguration without restart.

## Architecture

Configuration is split into two layers:

- **`Config`** (`src/config.rs`) — The main application configuration, deserialized from `syscity.toml`. Contains all subsystems as fields.
- **`HotReloadManager`** (`src/config.rs:hot_reload`) — File watcher + event dispatch that detects config changes and triggers per-type handlers.

### Config Fields

| Category | Fields | Reload Support |
|----------|--------|----------------|
| Server | `host`, `port`, `timeout_seconds`, `max_body_size` | Restart required |
| Logging | `level`, `format`, `file`, `stdout`, `rotation` | Hot-reloadable |
| Storage | `storage_type`, `connection`, `database` | Restart required |
| Services | `services` (HashMap of external service configs) | Hot-reloadable |
| Browser | `browser` (bridge, pool, profiles) | Hot-reloadable |
| Memory | `memory` (multimodal, dreaming, tier, effectiveness) | Hot-reloadable |
| Heartbeat | `heartbeat` | Hot-reloadable |
| Computer | `computer` (remote_control, headless, allow_apps) | Hot-reloadable |
| Standing Orders | `standing_orders` | Hot-reloadable |
| Capabilities | `capabilities` (profile, scope, sets) | Hot-reloadable |

### Hot Reload Architecture

```
syscity.toml ──▶ notify debouncer ──▶ mpsc::channel ──▶ HotReloadManager::run()
                  (500ms delay)                            │
                                                          ├── ConfigFileType::Main ──▶ channels add/remove/restart
                                                          ├── ConfigFileType::Agent ──▶ send UpdateConfig to agent
                                                          ├── ConfigFileType::Channel ──▶ reinit single channel
                                                          ├── ConfigFileType::Plugin ──▶ reload_plugin()
                                                          └── ConfigFileType::Gateway ──▶ security/providers/mcp/hot_reload
```

### ConfigFileType

```rust
pub enum ConfigFileType {
    Main,     // syscity.toml — channel lifecycle
    Agent,    // agents/*.toml — per-agent config
    Channel,  // standalone channel config files
    Plugin,   // WASM files or plugin manifests
    Gateway,  // gateway subsection (security, providers, mcp, hot_reload)
    Custom,   // user-defined config files
}
```

### Config Loading Order

1. Default values
2. Config file (`syscity.toml` or specified path)
3. Environment variables (`SYSCITY_*`)

### Secret Resolution

`Config::resolve_secrets()` uses `SecretResolver` to resolve `SecretRef` values in service configurations via environment variables, files, or external executables.

### Secret Masking

`src/secrets/mask.rs` is the single secret-masking walker applied to every
surface that reports configuration back to a client (the gateway tool's
`config.get` / `config.schema.lookup`, REST `get_config_handler`, and
`models.list`'s `api_key_masked`):

- Object keys recognized by `is_secret_key` (trailing `_key` / `_token` /
  `_secret` / `_password`, or the bare `key` / `token` / `secret` /
  `password`, plus the shared channel-credential registry) have their string
  values masked.
- Secret *containers* (`keys` / `credentials` / `api_keys`) have every string
  leaf masked, regardless of the leaf key name.
- `env` maps (MCP server env vars) are matched per key, so identifiers like
  `HOST` / `PORT` stay readable while `*_KEY` / `*_TOKEN` values are masked.
- Masking keeps the first 3 and last 4 characters (`sk-••••abcd`); values of
  6 characters or fewer are fully masked, and empty values stay empty.

### Concurrent Writes (Revision CAS)

The whole `GatewayConfig` has a stable revision: a SHA-256 over the
canonicalized (key-sorted) JSON serialization, so the fingerprint is
independent of `HashMap` iteration order (`gateway::config_revision`).

- WS `config.get` returns the current `revision`; `config.set` accepts an
  optional `base_revision` and rejects stale writes with `REVISION_CONFLICT`
  (the error payload carries `current_revision` / `expected_revision` so the
  client can re-read and retry). The CAS is checked twice: a fast-fail before
  the model-router side effect (a stale `path="model"` write cannot switch
  the router first) and an authoritative check under the write lock.
- The gateway tool's `config.get` reports the same fingerprint as `hash`
  (computed from the *unmasked* config), so `config.apply`'s `base_hash`
  optimistic locking and the WS revision agree on "current config".

## Key Types

```rust
pub struct Config {
    pub schema_version: u32,
    pub app: AppConfig,
    pub server: ServerConfig,
    pub logging: LoggingConfig,
    pub storage: StorageConfig,
    pub services: HashMap<String, ServiceConfig>,
    pub browser: BrowserConfig,
    pub memory: MemoryConfig,
    pub heartbeat: HeartbeatConfig,
    pub computer: ComputerConfig,
    pub standing_orders: StandingOrderConfig,
    pub capabilities: CapabilitiesConfig,
    pub extra: HashMap<String, toml::Value>,
}
```

```rust
pub struct HotReloadManager {
    watched_files: Arc<RwLock<HashMap<PathBuf, WatchedConfig>>>,
    handlers: Arc<RwLock<HashMap<ConfigFileType, Vec<ConfigChangeHandler>>>>,
    change_tx: mpsc::Sender<ConfigChangeEvent>,
    change_rx: Arc<RwLock<mpsc::Receiver<ConfigChangeEvent>>>,
    #[cfg(feature = "hot-reload")]
    watcher: Arc<RwLock<Option<Debouncer<RecommendedWatcher, FileIdMap>>>>,
}
```

```rust
pub struct ConfigChangeEvent {
    pub path: PathBuf,
    pub config_type: ConfigFileType,
    pub change_type: ConfigChangeType,
}
```

## Implemented Features

- Multi-source configuration loading (defaults, TOML file, environment variables)
- Environment variable interpolation in TOML (`$VAR` and `${VAR}`)
- Schema versioning with migration support
- Cross-field validation (ports, log levels, protocols, storage connections)
- File watcher with 500ms debounce for hot reload
- Per-config-type handler registration
- Secret resolution with `SecretRef` (env, file, exec providers)
- Canonical secret masking (`secrets::mask_json_value`) on all config read surfaces
- Config revision CAS (`REVISION_CONFLICT`) for safe concurrent writes
- `computer.allow_apps` — the per-app desktop boundary — is pushed into a shared
  handle on reload and on `config.set`, so it reaches desktop sessions that are
  already running rather than only the ones built after the edit
- `computer.record_video` (default `false`) — capture a video alongside
  `computer.record_start`/`.record_stop`. Read at the start of a recording, not
  hot-reloaded into one already running: the file it writes is named and opened
  up front. Off by default because it starts a screen-capture process and needs
  a permission (macOS Screen Recording) the rest of computer use does not.
- `ReloadableConfig` with broadcast-based change notifications
- Comprehensive unit tests for all config subsystems

