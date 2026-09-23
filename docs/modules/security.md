# Security Module

Input validation, access control, authentication, and sandboxing.

## Design

- **Auth** (`auth.rs`) — JWT token generation/validation, API key management, session management with scopes
- **Pairing** (`pairing.rs`) — Device pairing and DM policy
- **Device Pairing** (`device_pairing.rs`) — Challenge-based device pairing with QR codes and setup URLs
- **Allowlist** (`allowlist.rs`) — Multi-dimensional allowlist matching with compiled cache
- **Tailscale Auth** (`tailscale.rs`) — Tailscale whois verification with caching
- **Trusted Proxy** (`trusted_proxy.rs`) — IP whitelist/CIDR, required headers, user extraction
- **Runtime Audit** (`runtime_audit.rs`) — Audit logging for auth and security events
- **Content Filter** (`content_filter.rs`) — Secret scanning and PII detection in outputs
- **Sliding Window** (`sliding_window.rs`) — Rate limit tracking with lockout

### Validators (in `tools/mod.rs`)

- `NameValidator` — Tool name length, character set, prefix rules
- `SchemaValidator` — JSON Schema structure (type: object, properties required)
- `SecurityValidator` — Path traversal and command injection detection

## Key Types

```rust
pub struct AuthManager {
    users: Arc<RwLock<HashMap<UserId, User>>>,
    sessions: Arc<RwLock<HashMap<String, Session>>>,
    pairing_required: bool,
    audit_log: Option<Arc<dyn AuditLogger>>,
}

pub struct User {
    pub id: UserId,
    pub name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub is_admin: bool,
    pub scopes: Vec<String>,
    pub metadata: HashMap<String, String>,
}

pub struct Session {
    pub token: String,
    pub user_id: UserId,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub device_fingerprint: Option<String>,
    pub scopes: Vec<String>,
}
```

## Implemented Features

- Auth mode ambiguity detection — fails fast when both `shared_token` and OAuth are configured but `auth_mode` is not set
- Tailscale authentication — `TailscaleAuthenticator` with whois verification via CLI, caching, and tailnet-based authorization
- Full allowlist system with multi-dimensional matching — 10 match sources (Id, Username, Name, Tag, E164, PrefixedId, PrefixedUser, PrefixedName, Slug, Localpart), compiled `HashSet` cache, wildcard `*` support, account-scoped storage with file locking
- SecretRef resolution system — three providers (`env`, `file`, `exec`) with `SecretResolver::resolve()`
- Multi-scope rate limiting — `MultiTierRateLimiter` includes `global`/`per_user`/`per_ip`/`per_endpoint`/`shared_secret`/`device_token`/`hook_auth`/`control_plane_write` tiers, lockout tracking, attempt serialization, and loopback exemption
- Device pairing challenge — `DevicePairingStore` generates 8-character unambiguous codes, enforces a max pending limit, produces QR codes and `syscity://pair/{code}` URIs, and supports base64url setup URLs
- Secret resolution cache — `SecretResolver` keeps per-provider payload caches (env, file, exec) and a per-reference result cache with TTL, plus `refresh()` and `refresh_reference()` for manual invalidation
- Audit logging for auth events — `AuditEventType` includes `Login`, `Logout`, and `TokenValidation`; `AuthManager` emits events via an attached `AuditLogger` on `create_session`, `validate_session`, and `revoke_session`
- Trusted proxy authentication — IP whitelist/CIDR, required headers, user extraction, allowUsers whitelist, audit logging
- Credential precedence — `env_first` vs `config_first` for token/password sources. `CredentialPrecedence` enum applied in `daemon.rs:apply_env_security_overrides()` and `apply_env_provider_overrides()`
- Secret scanning in tool outputs — `ContentFilter` with `SecretScanner` and `PiiDetector` wired into `ToolRegistry` during `Gateway::new()`
- Kernel write fences for workspace-only command tools — `shell` / `process` / `code_exec` attach a `WriteFence` (workspace root + allowed paths) to the `ProcessRequest` when `workspace_only` is set (`src/tools/process_runner.rs`), turning the parent-side path check into a kernel-enforced deny of all file writes outside the workspace: macOS Seatbelt (argv wrapped behind `/usr/bin/sandbox-exec`, plus process-group kill so the forked sandboxed grandchild can't outlive a timeout), Windows AppContainer + Job object (`src/tools/win_appcontainer.rs` — Low-integrity token, DACL grant + mandatory Low label on the workspace, `KILL_ON_JOB_CLOSE` whole-tree termination), and on Linux three stacked layers: **Landlock** (in-place `restrict_self` via `pre_exec`, write rights only — reads/exec/network stay unrestricted), a **seccomp escape-vector deny list** installed on every fenced run (`ptrace`, `process_vm_readv/writev`, `bpf`, `keyctl`, module/mount/namespace syscalls → EACCES; `clone3` → ENOSYS so libc falls back to a flag-filterable `clone`; per-architecture syscall tables verified against `libc::SYS_*` by a contract test, and the program logic simulated kernel-style on any host), and an optional **namespace view** (`[security] fence_namespaces`, default `auto`: unprivileged user namespace + private mount namespace, root re-bound and marked read-only recursively, `/tmp` covered with a fresh tmpfs, working trees plus `/dev`/`/run`/`/proc` re-bound read-write on top — `2>/dev/null` and dbus/systemd/docker IPC keep working; no PID namespace, since `pre_exec` runs once between fork and exec and cannot place the command itself into one, so same-uid `/proc` entries stay visible while their memory does not). All layers fail closed — `ProcessError::Sandbox` — when the platform sandbox is unavailable, except the namespace view, whose `auto` posture degrades with a warning (`require` fails, `off` skips) because a hardened sysctl or container is an environment fact, not an attack
- Canonical secret masking — `src/secrets/mask.rs` (`mask_json_value` / `mask_secret`) is the single walker behind every config-describe surface (REST config handler, the `config.get` / `config.schema.lookup` gateway tools, `models.list`), so provider keys, channel credentials, and `security.shared_token` are never returned in plaintext; WS `config.set` supports `base_revision` compare-and-swap (`REVISION_CONFLICT` on stale writes)
- JWT session management with scope-based access control
- User registration and lookup with admin flag support

