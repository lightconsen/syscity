# Secret Storage — Requirements Analysis & Design

> Status: implemented (Design as Implemented)
> Audience: syscity contributors and maintainers
> Related modules: config, security, mcp, channels, plugins, secrets

This document has two parts: **requirements analysis** (sensitive-information
inventory, threat model, goals) and **design** (layered storage architecture,
unified abstraction, routing, OAuth lifecycle, migration). It closes with the
**current implementation state** so the design can be checked against what is
actually built.

Core position: **values are stored in 0600 AES-GCM-encrypted files by default;
the OS keyring is an opt-in compile-time feature (`--features keyring`) that
restores keyring-primary routing with the file layer as fallback; config only
stores references.**

---

## 1. Requirements Analysis

### 1.1 Sensitive-Information Inventory

The sensitive information managed by the secret system, enumerated per
subsystem. Every value belongs to a `(namespace, entity, kind)` triple (see
§3.1 `SecretId`).

| Subsystem | Value | SecretId (ns/entity/kind) | Lifecycle | Direction |
|-----------|-------|---------------------------|-----------|-----------|
| LLM | provider API key | `llm/{provider}/api_key` | static | outbound |
| MCP | OAuth refresh token | `mcp-oauth/{server_id}/refresh_token` | rotating | outbound |
| MCP | OAuth access token | `mcp-oauth/{server_id}/access_token` | ephemeral | outbound |
| MCP | env token (user input) | `mcp-env/{server_id}/{key}` | static | outbound |
| Channel | token / app_secret / api_password … | `channel/{channel_id}/{kind}` | static/rotating | outbound |
| Webhook | webhook_secret | `channel/{channel_id}/webhook_secret` | static | inbound |
| Security | `shared_token` | ops-injected (env preferred) | static | inbound |
| Security | OAuth client_secret | `security/oauth-{provider}/client_secret` | static | client |
| Plugin | secret_key (signing) | `plugin/{plugin}/secret_key` | static | signing |

> `[security.rate_limit.shared_secret/device_token/hook_auth]` are rate-limit
> tiers, not secret values; the real secrets are `shared_token` and the tokens
> in the device registry.

### 1.2 Threat Model

syscity primarily runs as a local single-user desktop app (macOS first), but
can also be deployed as a server.

| Threat | Defense |
|--------|---------|
| Another user on the machine reads files | directories `0700` / files `0600`; high-value values may additionally be held in the OS keyring (feature `keyring`) |
| `~` directory is backed up / synced to the cloud | file layer is AES-256-GCM encrypted; with the `keyring` feature, keyring values never touch files |
| Env leak via subprocesses | env is injected only when needed; MCP stdio does not inherit the full environment |
| Log / crash-dump leaks | zeroize everywhere + Debug `[REDACTED]` |
| Path traversal | `sanitize_entity` rejects `..`, `/`, and empty |
| Malicious same-user process | with the `keyring` feature, keyring values never hit files, inherently immune |
| Keyring unavailable (display asleep / headless) | feature `keyring` only: every operation is timeout-bounded → degrade to file fallback; routing recovers automatically on wake |

### 1.3 Goals

1. Store persistent secrets that are **user-entered / third-party-issued** in
   the OS keyring when the `keyring` feature is enabled (opt-in).
2. Default to **0600 encrypted files** — the only backend in default builds,
   and the fallback in environments without a keyring (headless Linux /
   containers / CI).
3. `config.toml` **never stores values, only references**.
4. No aggregation — each entity (provider / server / channel / plugin) gets its
   own entry.
5. Migrations are **atomic and rollback-safe** and never break existing user
   config.
6. A single keyring operation **never hangs forever**; after the display wakes,
   the same daemon **automatically recovers** keyring routing without a restart.

### 1.4 Design Principles

Based on the OWASP Secrets Management Cheat Sheet and desktop OAuth storage
consensus:

1. **config never stores values, only references** (`$VAR` / `SecretRef` /
   store references).
2. **No aggregation**: one entry per entity, so a single leaked plaintext file
   does not expose everything.
3. **OS keyring is an opt-in compile-time feature**: with `--features keyring`
   the OS keychain (macOS Keychain / Windows DPAPI / Linux Secret Service) is
   the preferred backend; default builds use 0600 encrypted files everywhere.
4. **OAuth lifecycle**: access tokens stay **in memory only**; only the refresh
   token is persisted.
5. **PKCE + minimal scope**: a desktop public client does not need a
   client_secret.
6. **Short lifetimes + rotation**: as short as possible; revoke on leak.
7. **Disable/delete cleans up**: removing a service deletes its secret too.
8. **Redact + zeroize everywhere**: Debug output, logs, in-memory values.
9. **Timeout means degrade**: the synchronous keyring API can hang during dark
   wake, so every call is bounded.

---

## 2. Layered Storage Architecture

Five tiers, from "most secure" to "most frequently read/written":

```
┌─────────────────────────────────────────────────────────┐
│ Tier 0  Config reference layer  config.toml              │  ← references only, never values
│         SecretRef("$VAR") / StoreRef{ns,entity,kind}     │
├─────────────────────────────────────────────────────────┤
│ Tier 1  OS keyring (feature `keyring`, opt-in)           │  ← user-entered, persistent, high-value
│         macOS Keychain / Win DPAPI / Linux SecretService │     → keyring crate
├─────────────────────────────────────────────────────────┤
│ Tier 2  File fallback (headless / system-generated)      │  ← AES-256-GCM encrypted
│         ~/.syscity/secrets/{ns}/{entity}.toml  0600/0700 │     → file_store atomic writes
├─────────────────────────────────────────────────────────┤
│ Tier 3  Memory only (access token, runtime-resolved)     │  ← zeroize + TTL
│         MemoryStore                                      │
├─────────────────────────────────────────────────────────┤
│ Tier 4  External injection (env / file / exec)           │  ← ops-injected SecretRef
│         SYSCITY_PROVIDER_{NAME}_KEY etc.                 │
└─────────────────────────────────────────────────────────┘
```

**Tier-selection rules** (judged highest priority first):

1. **Ephemeral / access token** → Tier 3 (memory).
2. **Ops-injected sources** (env/file/exec) configured → Tier 4, not persisted.
3. **System-generated and required headless** → Tier 2 (file).
4. **User-entered / third-party-issued, persistent, high-value** → Tier 1
   (requires the `keyring` feature) when the keyring is available, otherwise
   Tier 2.

---

## 3. Core Abstractions

### 3.1 `SecretId` / `SecretOrigin` / `SecretStore`

The logical identifier and unified backend interface (`src/secrets/store.rs`):

```rust
pub struct SecretId {
    pub namespace: String, // "llm" | "mcp-env" | "mcp-oauth" | "channel" | "security" | "plugin" ...
    pub entity: String,    // provider / server_id / channel id / fixed name
    pub kind: String,      // "api_key" | "refresh_token" | "access_token" | "secret" ...
}

pub enum SecretOrigin { UserEntered, SystemGenerated, OperatorInjected }

#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn get(&self, id: &SecretId) -> crate::Result<Option<String>>;
    async fn set(&self, id: &SecretId, value: &str, origin: SecretOrigin) -> crate::Result<()>;
    async fn delete(&self, id: &SecretId) -> crate::Result<()>;
    async fn has(&self, id: &SecretId) -> bool;
    // Entity-level whole-map ops: get_all / set_all / delete_entity / has_entity (unsupported by default)
}
```

### 3.2 Backends

**KeyringStore (Tier 1)** — `src/secrets/keyring_store.rs`

- Compile-gated: the module and its re-exports (`probe_keyring`, `KeyringStore`)
  are `#[cfg(feature = "keyring")]`, and the crate dependency is optional:
  `keyring = { version = "3", features = ["apple-native", "windows-native", "sync-secret-service"], optional = true }`.
- Mapping: `Entry::new("syscity/{namespace}", "{entity}")`, storing a
  JSON-serialized `kind → value` table (semantically identical to the file
  backend's `[secrets]` table).
- The keyring is a synchronous, blocking API → every method runs on a blocking
  thread via `spawn_blocking`, then is wrapped in `KEYRING_OP_TIMEOUT` (5s;
  100ms in tests); a hang degrades to an error.
- Platform reads/writes go through the `CredentialBackend` trait; tests inject
  an in-memory implementation and never touch a real keychain.

**FileStore (Tier 2, encrypted)** — `src/secrets/file_store.rs`

- Path `~/.syscity/secrets/{namespace}/{entity}.toml`, `[secrets] kind = "value"`.
- Writes: temp file → `set_permissions(0o600)` → atomic rename; directory
  `0o700`; id validated by `sanitize_entity` (rejects `..`, `/`, empty).
- **Encryption**: with a master key, values are stored as
  `base64(nonce[12] || AES-256-GCM ciphertext)`; without a key they are written
  plaintext at 0600. An untagged enum makes the new and legacy formats
  mutually readable.
- **Master key source priority**: OS keyring (`syscity` / `file-master-key`,
  feature `keyring` only) → 0600 `~/.syscity/secrets/.master_key` → plaintext
  fallback; cached in a process-wide `OnceLock`, zeroized on drop. Without the
  feature the keyring source is absent and only the 0600 file is consulted.

**MemoryStore (Tier 3)** — `src/secrets/in_memory.rs`: zeroize memory backend.

**External injection (Tier 4)** — `SecretRef` (`$VAR` / `{env}` / `{file}` /
`{exec}`), resolved by `SecretResolver` with caching and a TTL; values are
zeroized.

### 3.3 Tier Routing

```rust
pub fn route_store(namespace: &str) -> Arc<dyn SecretStore>;   // namespace-level
pub fn choose_store(id: &SecretId) -> Arc<dyn SecretStore>;    // entry-level
pub fn probe_keyring() -> bool;                                // availability probe
```

- `route_store` / `choose_store` consult `keyring_available()`: with the
  `keyring` feature this calls `probe_keyring()`; without the feature
  `keyring_available()` always returns `false`, so routing is **always a plain
  `FileStore`** and the keychain is never touched.
- With the feature, `probe_keyring()` returning `true` makes
  `route_store_with(.., true)` return
  `FallbackStore{ primary: KeyringStore, secondary: FileStore }`; otherwise a
  plain `FileStore`.
- `FallbackStore`: `get` tries primary then secondary; `set` writes the primary
  only; `delete` clears both.
- `choose_store` special-cases `mcp-oauth/access_token` → memory backend
  (short-lived).
- Synchronous probes and master-key reads/writes run inside `with_timeout`
  (detached thread + `recv_timeout`) so a hang never blocks the caller's thread.

### 3.4 Unified Read Points

- `resolve_store_ref(&StoreRef)`: route by reference and read the current value.
- `resolve_secret_or_ref(&StoreRef)` → `SecretValue` (zeroized on drop, Debug
  `[REDACTED]`).
- `resolve_channel_credential(channel, kind, legacy)` / `persist_channel_secrets`:
  store first, plaintext `credentials` map as fallback.
- `resolve_oauth_client_secret(provider, legacy)`: store first, plaintext
  config as fallback.

`StoreRef` is the config-side reference form, serialized as
`{ namespace = "...", entity = "...", kind = "..." }`; `resolve_secret_or_ref`
turns the reference into a value.

---

## 4. Per-Category Design

> "Tier 1" below means *when the `keyring` feature is enabled and the keyring
> is available*; otherwise every persistent value goes to Tier 2 (file).

| Category | Lifecycle | Scope | Storage |
|----------|-----------|-------|---------|
| LLM api key | static | per-provider | Tier 1 (primary) / Tier 2 (fallback); env override wins |
| MCP env token | static | per-server | Tier 1 (primary) / Tier 2 (fallback) |
| MCP OAuth **refresh** | rotating | per-server | Tier 1 (primary) / Tier 2 (fallback) |
| MCP OAuth **access** | ephemeral | per-server | **Tier 3 memory only** |
| Channel token | static/rotating | per-channel | Tier 1 (primary) / Tier 2 (fallback) |
| `webhook_secret` | static | per-channel | Tier 2 / Tier 4 (ops injection) |
| `security.shared_token` | static | global | Tier 4 env preferred, plaintext config backwards-compatible |
| Gateway OAuth `client_secret` | static | global | PKCE means not stored on desktop; if needed, Tier 1 |
| plugin `secret_key` | static | per-plugin | Tier 2 (system-generated) / read from store |

### 4.1 LLM

`model_router` read priority: `SYSCITY_PROVIDER_{NAME}_KEY` (Tier 4) >
`api_keys` > single `api_key`. Each provider has its own keyring entry
(`syscity/llm` / `{provider}`), so routing switches by name and resolves the
matching key. Inline plaintext values remain backwards-compatible.

### 4.2 MCP (OAuth lifecycle)

- **Access token**: in-memory cache only; `refresh_expiring_tokens` renews it
  with the refresh token before expiry; after a restart `handle_get_token`
  re-acquires it from the persisted refresh token.
- **Refresh token**: persisted via `route_store("mcp-oauth")`
  (`set`/`get`/`delete`).
- **Non-sensitive metadata** (`token_url` / `client_id` / `expires_at`): a 0600
  sidecar `~/.syscity/mcp_tokens/{id}.json`, written atomically. **Never
  contains a token**.
- `handle_clear_token`: removes the memory cache + the store entry + the
  sidecar.
- `McpManager::connect` merges `resolved_env` from `route_store("mcp-env")`
  before spawning, guaranteeing "auto-reconnect after restart".

### 4.3 Channel / Webhook

Sensitive channel credential keys (`SENSITIVE_CHANNEL_CREDENTIALS`) are written
to `channel/{channel_id}/{kind}`; reads go through `resolve_channel_credential`,
store first, plaintext config as fallback (backwards-compatible; `secrets
migrate` strips the plaintext).

### 4.4 Security

`shared_token` defaults to env (`SYSCITY_SECURITY_SHARED_TOKEN`) and falls back
to plaintext config without it. An OAuth `client_secret`, if configured, is
written to `security/oauth-{provider}/client_secret`.

### 4.5 Plugin

`secret_key` (signing) can be read via `route_store("plugin").get(...)`; the
plaintext request-body path is kept for backwards compatibility, and on write
the value is stored in the store first.

---

## 5. Migration

| From | To | Strategy | Trigger |
|------|----|----------|---------|
| `mcp_env/*.toml` (`[env]` plaintext) | `route_store("mcp-env")` | delete the old files and directory only after the new location is written; idempotent | daemon startup + `syscity secrets migrate` |
| `mcp_tokens/*.json` plaintext token fields | refresh → `route_store("mcp-oauth")`, access → memory | persist to the store first, then rewrite the sidecar as metadata-only (0600, atomic); any failure leaves the state untouched; idempotent | daemon startup + `syscity secrets migrate` |
| config.toml channel/security/plugin plaintext | store references | **never rewrites user files automatically**; on encountering plaintext → use it + `warn!` guidance; `migrate` strips it | `syscity secrets migrate` |
| existing env `$VAR` | keep | highest priority, untouched | — |

Migrations must be **atomic and rollback-safe**: the old file is removed only
after the new location is written successfully; any failed step leaves the
current state intact.

---

## 6. Current Implementation State

### 6.1 Directory Layout

```
src/secrets/
├── mod.rs           SecretRef / SecretResolver (env/file/exec, Tier 4)
├── store.rs         SecretId / SecretOrigin / SecretStore trait / routing / SecretValue
├── keyring_store.rs Tier 1 backend + availability probe + KeyringHealth
│                     (compiled only with the `keyring` feature)
├── file_store.rs    Tier 2 backend (AES-GCM encryption) + master key + migration
├── mask.rs          Canonical secret masking for all config-describe surfaces
└── in_memory.rs     Tier 3 memory backend (zeroize)
```

`src/mcp/env_store.rs` has been retired; its write logic lives in
`file_store.rs`.

### 6.2 Namespaces

`llm`, `mcp-env`, `mcp-oauth`, `channel`, `webhook`, `security`, `plugin`.

### 6.3 Key Entry Points

- Routing / reads: `route_store` / `choose_store` / `resolve_secret_or_ref` /
  `resolve_store_ref` / `resolve_channel_credential` / `persist_channel_secrets` /
  `resolve_oauth_client_secret`.
- Migration: `migrate_legacy_mcp_env()` (file_store.rs) and
  `migrate_legacy_mcp_tokens()` (mcp/oauth.rs); both are invoked at daemon
  startup in `Gateway::new` and by `syscity secrets migrate`.
- CLI: `syscity secrets list` (names and locations only) / `migrate` /
  `purge {namespace}`.
- OAuth persistence: `persist_refresh_token` / `load_refresh_token` /
  `delete_refresh_token` (mcp-oauth) plus `persist_metadata` (0600 sidecar).
- Masking: `mask_json_value` / `mask_secret` / `is_secret_key` (mask.rs),
  applied by every config-describe surface (§6.6).

### 6.4 Robustness (timeout + auto-recovery)

> Applies only when the `keyring` feature is compiled in; default builds have
> no keyring code at all.

- `KEYRING_OP_TIMEOUT` (5s; 100ms in tests) bounds **every** keyring operation;
  a timeout or failure → `mark_keyring_down()` + a bounded error, so callers
  never block.
- Synchronous paths (probe, master-key read/write) run inside `with_timeout`
  (detached thread + `recv_timeout`); a master-key timeout falls back to the
  `.master_key` file path, so startup is bounded at worst to 5s.
- The `KeyringHealth` state machine replaces a one-shot `OnceLock<bool>`:
  `up` is sticky (zero I/O while confirmed up); `down` is throttled (no
  re-probe within `PROBE_COOLDOWN_SECS = 10`); a failed operation calls
  `mark_down` so the next re-probe is allowed immediately. After the display
  wakes, the next `probe_keyring()` past the cooldown re-probes successfully →
  `route_store` returns `FallbackStore` again and keychain secrets become
  readable **without a restart**.

### 6.5 Subsystem Integration

- `src/model_router/config.rs`: `effective_key()` → `api_keys` /
  `ProviderKey::Ref` resolved via `resolve_store_ref`.
- `src/mcp/manager.rs:204`: `route_store("mcp-env").get_all(server_id)` before
  connect.
- `src/gateway/ws.rs`: mcp.add env writes to `route_store("mcp-env")`; list
  reads `has_entity`.
- `src/gateway/webhooks/`: `resolve_channel_credential(..,
  "webhook_secret", ..)`, called from `feishu.rs`, `generic.rs` and
  `wechatmp.rs`.
- `src/gateway/handlers/plugins.rs:279`: signing key read via
  `route_store("plugin")`.
- `src/mcp/oauth.rs`: access memory-only, refresh persisted, metadata 0600
  sidecar.

### 6.6 Config-Surface Masking & Revision CAS

`mask.rs` is the single secret-masking walker for any `serde_json::Value`
describing gateway configuration. It replaced per-surface ad-hoc masking
(which leaked plaintext on some paths):

- object keys matching `is_secret_key` (the `SENSITIVE_CHANNEL_CREDENTIALS`
  registry, `api_keys`, and a conservative `*_key` / `*_token` / `*_secret` /
  `*_password` suffix heuristic that deliberately does **not** match
  `max_tokens`, `token_url`, `key_path`, `client_id`, …) have their string
  values masked;
- secret *containers* (`keys` / `credentials` / `api_keys`) have every string
  leaf masked regardless of leaf key;
- `env` maps (MCP server env) are matched per key, so `HOST` / `PORT` /
  `BASE_URL` stay readable while `*_KEY` / `*_TOKEN` values are masked.

`mask_secret` renders `head(3)••••tail(4)`; values of ≤6 chars are fully
masked, and empty values stay empty (an unset secret must not look set).

Surfaces going through the walker: REST `get_config_handler`, the
gateway-tool `config.get` / `config.schema.lookup` (whose `hash` stays raw so
`base_hash` optimistic locking is unaffected), and `models.list` (its local
`mask_api_key` folded into the shared `mask_secret`). The local CLI
(`syscity config get`) reads the file directly and is unmasked. WS
`config.get` returns only a SHA-256 `revision` of the config; WS
`config.set` accepts an optional `base_revision` and rejects stale writes
with `REVISION_CONFLICT` — checked twice: fast-fail before the model-router
side effect, then authoritatively under the write lock.

---

## 7. Compatibility & Boundaries

- **Zero breakage**: existing plaintext `config.toml` fields keep working,
  with a `warn!` nudging migration.
- **Env unchanged**: `SYSCITY_PROVIDER_{NAME}_KEY` and
  `SYSCITY_SECURITY_SHARED_TOKEN` are unchanged; the MCP `$VAR` env-ref feature
  is unchanged.
- **Keyring platform variance** (feature `keyring`): solid on macOS;
  unavailable on headless Linux / containers → automatic probe + encrypted
  file fallback + log notice. Under `cfg(test)` the probe always returns
  `false`, so tests never touch a real keychain. Without the feature the
  keychain is never probed at all.
- **Default-build boundary**: a build without `--features keyring` uses the
  0600 file layer exclusively — no keychain access, no password prompts. To
  switch a deployment to keyring-primary, rebuild with `--features keyring`.
- **Master-key boundary** (feature `keyring`): if the keyring master key
  cannot be read during dark-wake startup, a new file master key is generated;
  since it differs from the keyring key of a previously awake process, old
  encrypted files may not decrypt within that process. The blast radius is
  narrow (during dark wake the file secrets are mostly written plaintext at
  0600), and this is a known boundary. Note that a default (file-only) build
  and a `keyring` build derive the master key from different sources — the two
  should not be mixed while encrypted file secrets exist.
- **Rejected alternative**: write-through (writing both keyring and file)
  would guarantee availability but leaves a disk copy of every keychain secret,
  violating the "independent layers" and keychain-only requirements, so it is
  not used.
- **Keyring version**: pinned to v3 (v4 alpha is breaking), optional and
  behind the `keyring` feature; evaluate v4's default features once it
  stabilizes.

---

## 8. Verification

- **Unit tests**: routing matrix (`route_store_with`), the access→memory
  special case, the channel access-persistence special case; FileStore
  roundtrip + permission assertions + atomic writes + AES-GCM encrypt/decrypt;
  KeyringStore through the in-memory backend (including `HangingBackend`
  timeouts and `KeyringHealth` stickiness/throttle/recovery); migration
  idempotence (mcp_env / mcp_tokens); `SecretValue` redaction; masking matrix
  (mask.rs secret keys / containers / env per-key; REST + gateway-tool
  regression tests proving no plaintext leaks).
- **Integration tests**: real OS-keyring roundtrip
  (`tests/secrets_keyring_roundtrip.rs`, `#![cfg(feature = "keyring")]` +
  `#[ignore]`; run with `--features keyring -- --ignored`).
- **Manual verification matrix**:

| Scenario | Expected |
|----------|----------|
| Default build (no `--features keyring`), any OS | all secrets in 0600 encrypted files; no keychain access or prompts |
| `--features keyring`, enable the GitHub preset on macOS | token → saved to Keychain, no password/Touch ID prompt |
| Enable on headless Linux | falls back to 0600 encrypted files, startup log reports the backend |
| Restart the daemon while the display is asleep | no hang; keyring ops degrade bounded-ly |
| After waking (no restart) | MCP/LLM keys readable again without a restart |
| Restart the daemon | MCP auto-reconnects, no re-entry |
| Disable a server | keyring/file entries deleted |
| Back up `~/.syscity` | values in keyring / encrypted files; backup contains no plaintext tokens |
| Multi-LLM switching | one entry per provider; routing picks the key by name |
