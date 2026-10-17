# Providers Module

LLM provider abstractions with a protocol-driven architecture: 3 protocol implementations (OpenAI, Anthropic, Gemini) serve all vendors via configurable presets.

## Architecture

```
Vendor (preset, e.g. "kimi") ──► Protocol (OpenAI / Anthropic / Gemini) ──► Provider impl
       │
       ├── builtin presets define defaults (base_url, model, auth, features)
       ├── users can override any default
       └── "custom" type for fully-manual configuration
```

Key insight: **vendor ≠ protocol**. A single vendor (e.g. Kimi) may expose multiple protocol endpoints (OpenAI-compatible and Anthropic-compatible).

### Core Types

| Type | File | Purpose |
|------|------|---------|
| `Protocol` | `mod.rs` | Enum: `OpenAi`, `Anthropic`, `Gemini` |
| `AuthMethod` | `mod.rs` | Authentication strategy (Bearer, ApiKeyHeader, GoogleApiKey, None, CustomHeader) |
| `ProtocolVariant` | `mod.rs` | A vendor's specific protocol + defaults |
| `ProviderDefinition` | `mod.rs` | A vendor with one or more protocol variants |
| `ProviderInstanceConfig` | `mod.rs` | Fully-resolved runtime config passed to protocol providers |

### Files

```
src/providers/
├── mod.rs                  # Provider trait, types, shared models
├── openai.rs               # OpenAI Chat Completions protocol (config-driven)
├── anthropic.rs            # Anthropic Messages API protocol (config-driven)
├── gemini.rs               # Google Gemini generateContent protocol
├── fallback.rs             # Multi-provider fallback chain
├── mock.rs                 # Programmable test provider
├── preset.rs               # TOML loader + hand-rolled fallback
├── presets.toml            # Builtin vendor definitions (embedded via include_str!)
├── resolver.rs             # Config → provider dispatch
├── stream_wrappers.rs      # Composable stream processing wrappers
└── sdk.rs                  # Plugin SDK types
```

### Protocol Providers (3 implementations)

| Protocol | File | SSE Format | Auth |
|----------|------|------------|------|
| OpenAI | `openai.rs` | `data: {delta: {content}}` + `[DONE]` | Bearer token |
| Anthropic | `anthropic.rs` | `content_block_delta`, `message_stop` events | `x-api-key` |
| Gemini | `gemini.rs` | Newline-delimited JSON (non-SSE) | `x-goog-api-key` |

All three accept `ProviderInstanceConfig`, making them fully config-driven. Vendors like Moonshot/Minimax are variants of `OpenAiProvider` with different stream families, not separate provider files.

### Builtin Presets (`presets.toml` + `preset.rs`)

Vendor definitions live in `presets.toml`, embedded at compile time via
`include_str!` and parsed once, on the first `builtin_providers()` call. **This
is the only vendor registry**: adding a vendor is one TOML table — display name,
its suggested models, and one table per protocol endpoint — plus a row in the
table below. Nothing else reads a vendor list of its own; the config-facing
`model_router::provider_presets()` is a view over this one, adding the legacy
`ProviderType` and the synthetic `custom` entry.

A vendor's `models` list is what the pickers show and what a provider with no
model list of its own is backfilled from, and its **first entry must be the
variant's `default_model`** — the two are read interchangeably, so a test holds
them together. If the embedded TOML ever fails to parse, `preset.rs` logs the
error and falls back to a minimal hand-rolled set (OpenAI + Anthropic) so the
gateway still boots.

| Preset | Display name | Default base URL | Auth | Default model |
|--------|--------------|------------------|------|---------------|
| `openai` | OpenAI | `api.openai.com/v1` | Bearer | `gpt-5.4-mini` |
| `deepseek` | DeepSeek | `api.deepseek.com/v1` | Bearer | `deepseek-v4-flash` |
| `ollama` | Ollama | `localhost:11434/v1` | None | `qwen3` |
| `qwen` | Qwen | `dashscope.aliyuncs.com/compatible-mode/v1` | Bearer | `qwen3.7-plus` |
| `kimi` | Moonshot (Kimi) | `api.moonshot.cn/v1` | Bearer | `kimi-k2.6` |
| `anthropic` | Anthropic | `api.anthropic.com` | `x-api-key` | `claude-sonnet-5` |
| `openrouter` | OpenRouter | `openrouter.ai/api/v1` | Bearer | `anthropic/claude-sonnet-5.5` |

Only the OpenAI protocol is listed above; `kimi` is the dual-protocol vendor
(its Anthropic-compatible endpoint is `api.moonshot.cn/anthropic`, selected with
`protocol = "anthropic"`).

**These seven are the whole list by design.** `presets.toml` had grown to 35 in a
breadth pass; it is now seven, because a preset buys *convenience*, not
capability: any OpenAI-compatible endpoint was always reachable through
`base_url`, and every removed row was another fact that could rot. A vendor added
back is one TOML table plus a resolver test. `models_endpoint` may be an absolute
URL — no shipped preset uses that today, but the field still supports it.


### Resolver (`resolver.rs`)

- `resolve_provider()` — Quick resolution: preset name + optional overrides → `Arc<dyn Provider>`
- `resolve_from_config()` — Full resolution with all overrides (protocol, base_url, model, max_context, vision/tools support, stream_family, auth_method)
- Automatically selects the correct protocol variant (e.g. Kimi's Anthropic endpoint when `protocol = "anthropic"`)
- Custom providers fall through to manual configuration
- The runtime entry point is `ModelRouter::create_provider(name, config)`: a
  provider whose **key** names a preset resolves that preset's variant, with the
  config's own base URL and model on top. `config.provider_type` decides only for
  a key that is not a preset (a display-name spelling like `DeepSeek`), and
  otherwise chooses which of a dual-protocol vendor's endpoints to use.

## Design

- **`Provider` trait** — Core abstraction:
  - `complete()` — non-streaming request
  - `stream()` — streaming response (`CompletionStream`)
  - `supports_tools()`, `max_context()`, `health_check()`
  - `stream_family()` — provider-specific stream wrapper selection
- **`ProviderRegistry`** — Holds named provider instances
- **`CompletionRequest`** — Unified request with messages, tools, temperature, model, vision/tools/reasoning flags, fallback models
- **`CompletionChunk`** — Streaming delta with content, reasoning_content, tool_calls, usage

### Implementations

| Provider | File | Notes |
|----------|------|-------|
| OpenAI | `openai.rs` | Chat Completions API + streaming + tool calling + retry logic + config-driven |
| Anthropic | `anthropic.rs` | Messages API + streaming + thinking support + config-driven |
| Gemini | `gemini.rs` | Google Gemini via `generateContent` API + function calling + streaming |
| Fallback | `fallback.rs` | Chains multiple providers with failover logic |
| Mock | `mock.rs` | Programmable test provider with sequence/callback modes |

All vendor-specific variants (Ollama, Moonshot, MiniMax, DeepSeek, Qwen) are now handled as `OpenAiProvider` instances configured via `resolver.rs` with different base URLs, models, and stream families. No separate files needed.

### Stream Wrappers

`stream_wrappers.rs` defines `ProviderStreamFamily` for provider-specific payload adaptations:
- Generic (default)
- OpenAI Responses Defaults
- OpenAiReasoning (o1/o3 series)
- Anthropic Thinking
- Google Thinking
- Moonshot
- Minimax
- OpenRouter

## Key Types

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn default_model(&self) -> &str;
    fn supports_tools(&self) -> bool;
    fn max_context(&self) -> usize;
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse>;
    async fn stream(&self, request: CompletionRequest) -> Result<CompletionStream>;
    async fn health_check(&self) -> Result<bool>;
    fn stream_family(&self) -> ProviderStreamFamily;
}

/// Supported API protocols (only 3 implementations)
pub enum Protocol { OpenAi, Anthropic, Gemini }

/// Authentication method for an API endpoint
pub enum AuthMethod { Bearer, ApiKeyHeader, GoogleApiKey, None, CustomHeader { name: String } }

/// A protocol variant within a vendor definition
pub struct ProtocolVariant {
    pub protocol: Protocol,
    pub default_base_url: String,
    pub default_model: String,
    pub auth_method: AuthMethod,
    /// List-models path relative to `default_base_url`, or an absolute URL when
    /// the list does not live under it (`None` = derive from the protocol).
    pub models_endpoint: Option<String>,
    pub default_max_context: usize,
    pub default_supports_vision: bool,
    pub default_supports_tools: bool,
    pub default_stream_family: ProviderStreamFamily,
}

/// A vendor: its identity, its suggested models, and one variant per endpoint
pub struct ProviderDefinition {
    pub name: String,
    pub display_name: String,
    /// Suggested model IDs, the default model first
    pub models: Vec<String>,
    pub variants: Vec<ProtocolVariant>,
}

/// Resolved runtime config for a protocol provider
pub struct ProviderInstanceConfig {
    pub protocol: Protocol,
    pub auth_method: AuthMethod,
    pub api_key: Option<String>,
    pub base_url: String,
    pub model: String,
    pub max_context: usize,
    pub supports_vision: bool,
    pub supports_tools: bool,
    pub stream_family: ProviderStreamFamily,
}
```

## Implemented Features

- Protocol-driven architecture — 3 protocol implementations serve all vendors via `resolver.rs` + `preset.rs`
- Vendor ≠ Protocol separation — Kimi supports both OpenAI and Anthropic protocols via `ProtocolVariant` selection
- Config-driven providers — `OpenAiProvider::from_config()`, `AnthropicProvider::from_config()`, `GeminiProvider::from_config()`
- Redundant files eliminated — `ollama.rs`, `moonshot.rs`, `minimax.rs` replaced by resolver-based dispatch
- New OpenAI-compatible vendor = one TOML table in `presets.toml` + nothing else
- Full backward compatibility — existing `ProviderType` enum mapped to preset names
- Auth profile store with SQLite persistence (`AuthProfileStore`) — key state metadata only (failure counts, cooldown, status); raw keys remain in config
- API key rotation with `AuthProfile` / `AuthProfileManager` — automatic failover between multiple keys per provider
- OAuth 2.0 + PKCE initial authorization flow (`pkce.rs`, `oauth_flow.rs`, `oauth_callback.rs`) + CLI `syscity provider auth` command
- OAuth 2.0 device authorization (RFC 8628) for providers that declare a `device_auth_url` — no browser needed on the gateway's host, and the flow advances through `providers.auth_status` polling rather than a callback (`provider_oauth.rs`)
- Differentiated cooldown — `FailureClass::default_backoff_secs()` used per failure type (RateLimit=60s, AuthTemporary=5s, etc.), never below config minimum
- Usage tracking (`ProviderUsageTracker`) — per-provider token consumption, cost estimation, time windows (today/this_hour/this_month), budget enforcement
- Model catalog (`ModelCatalog`) — dynamic registry with `ModelCatalogEntry`, suppression list; exposed via OpenAI-compatible `/v1/models` and WS `models.list`
- Doctor diagnostic system (`syscity doctor`) — provider health, auth status, circuit state, deprecation warnings, migration hints, plugin extension point
- Credential state machine (`AuthProfile`) — Active→Cooldown→Disabled lifecycle with key rotation, SQLite persistence
- OAuth token auto-refresh (`refresh_if_needed()`) — called at the start of every `complete()` and `stream()` in both `openai.rs` and `anthropic.rs` providers
- Model suppression (`ModelCatalog::suppress/unsuppress/is_suppressed`) — fully wired in `list()`, `find_by_capability()`, `find_by_provider()`; auto-suppress on `ModelNotFound` errors
- Plugin-extensible providers (`PluginProvider` + `PluginProviderRegistry`) — WASM-backed providers registered through `PluginManager::register_plugin_providers()`
- Smart routing (`resolve_alias_with_capabilities()`) — capability-based model selection (vision, tools, reasoning) in `model_router/mod.rs`
- Remote usage fetch (`UsageFetcher` trait) — `OpenAiUsageFetcher` hits OpenAI billing API, `LocalBudgetFetcher` reads config budget
- Stream family wrappers (`ProviderStreamFamily`) — OpenAi, OpenAiReasoning, Anthropic, AnthropicThinking, GoogleThinking, Moonshot, Minimax, OpenRouter, Generic

