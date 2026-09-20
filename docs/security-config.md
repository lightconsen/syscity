# Security Configuration

This guide covers how to configure Syscity's security: authentication modes,
rate limiting, CORS/CSP headers, device pairing, and trusted proxies.

All settings live under the `[security]` table in `~/.syscity/syscity.toml`.
The implementation lives in `src/gateway/config.rs` (`SecurityConfig`) and
`src/security/`. For the module-level design, see
[modules/security.md](modules/security.md).

> **Defaults are open for local development.** Out of the box `auth_required =
> false` and `auth_mode = "none"`, so anyone who can reach the port can connect.
> Before exposing Syscity beyond `127.0.0.1`, enable authentication.

## Quick reference

```toml
[security]
enabled = true            # master switch: auth + rate limiting + security headers
auth_required = true      # reject unauthenticated HTTP/WS upgrades
auth_mode = "token"       # none | token | device | tailscale
shared_token = "…"        # required for auth_mode = "token"
pairing_required = false  # require device pairing for new users
security_headers = true   # emit CSP / security headers
```

Apply with the CLI (then `syscity reload`):

```bash
syscity config set security.enabled=true
syscity config set security.auth_required=true
syscity config set security.auth_mode=token
syscity reload
```

> **Secrets are masked on remote read surfaces.** Anything that returns
> config content to a client — REST `GET /api/v1/config`, the agent-facing
> `config.get` / `config.schema.lookup` gateway tools, `models.list` — runs
> through the canonical masking walker (`src/secrets/mask.rs`), so
> `shared_token`, provider `api_key`s, and channel `credentials` come back as
> `abc••••wxyz`. The local CLI (`syscity config get`) reads the file directly
> and is unmasked. Over WebSocket, `config.get` returns only a SHA-256
> `revision`; `config.set` accepts an optional `base_revision` and rejects
> stale writes with `REVISION_CONFLICT` (compare-and-swap).

## Deployment model: one principal per gateway

The gateway serves **one principal per process**. Sessions are not isolated
between connections: any client that can talk to `/ws` and hold `chat`/`read`
can list every session, read its history, and act on it by id. There is no
owner column and no per-object authorization, by design — the supported
deployments are a single local user, or one gateway instance per tenant (see
`docs/scale.md`), and a check that is dead code in every supported deployment
would imply an isolation guarantee nothing exercises.

Consequences to plan around:

- Do **not** serve several people from one instance. Run one gateway (or one
  container) per tenant, with its own `SYSCITY_HOME`.
- Treat `/ws` reachability as the security boundary. The `auth_mode` guards,
  the Origin/Host check on the upgrade, and the connection cap all exist to
  keep that boundary meaningful — see `docs/modules/tui.md` for the client side.
- If multi-user sharing is ever needed, it is a design change (owner in the
  session schema, per-handler authorization, a two-principal test matrix), not
  a configuration flag.

## Authentication modes

`auth_mode` (enum in `src/gateway/protocol.rs`) selects the strategy:

| Mode | Value | Use case |
|------|-------|----------|
| **None** | `"none"` | Local dev only — anonymous access (default) |
| **Token** | `"token"` | A single shared secret for trusted clients |
| **Device** | `"device"` | Per-device pairing with approval |
| **Tailscale** | `"tailscale"` | Identity from the Tailscale tailnet |

`auth_required` gates the HTTP/WebSocket upgrade middleware. When `auth_mode`
is not `none`, the connection is rejected with **401** unless a valid credential
is presented.

### Token mode

The simplest authenticated setup. Define a shared secret:

```toml
[security]
enabled = true
auth_required = true
auth_mode = "token"
shared_token = "<generated below>"
```

Generate a strong secret instead of hand-picking one:

```bash
syscity config set security.shared_token="$(openssl rand -hex 32)"
```

Clients present the token in any of these ways (any one suffices):

- **Bearer header:** `Authorization: Bearer <token>`
- **Upgrade ticket** (preferred for browser WebSocket, where headers can't be
  set): `POST /api/v1/ws-ticket` with the token as a Bearer header returns
  `{"ticket": "...", "expires_in": 30}`, and the connection then uses
  `ws://host:18080/ws?ticket=<ticket>`. A ticket is single-use and short-lived,
  so a URL that leaks — devtools, a proxy log, a copy-paste — is worthless
  within seconds. What the ticket is worth is what the minting credential was
  worth; minting never widens access.
- **Query parameter** (kept for a client that cannot fetch a ticket first):
  `ws://host:18080/ws?token=<token>`. The token therefore still appears in URLs
  for those clients — prefer the ticket — and the gateway redacts credential
  parameters from its own log lines (`gateway/middleware.rs::redact_uri`).

  The web client reaches for this fallback only when the gateway has no
  exchange at all (a 404/405/501 from `/api/v1/ws-ticket`). A *refused*
  exchange — a rejected credential, a network fault — refuses the connection
  and retries on the normal backoff instead, because falling back would put
  the token in a URL at exactly the moment it is already being declined.
- **ACP `connect` handshake:** `params.auth.token = "<token>"`

The TUI client passes it directly:

```bash
syscity tui --host <host> --port 18080 --token <token>
```

Notes:
- A connection authenticated by `shared_token` gets `user_id = "shared"` and the
  default scopes (`chat`, `read`). All shared-token clients share one identity —
  use device or Tailscale mode if you need to distinguish users.
- Token mode **also** accepts already-issued session tokens (e.g. from OAuth
  login), validated via the session store.

### Device mode

Each device must be paired and approved before it can connect.

```toml
[security]
enabled = true
auth_required = true
auth_mode = "device"
pairing_required = true
```

Pairing workflow (CLI):

```bash
syscity device list              # list pending / paired devices
syscity device approve <code>    # approve a pairing code
syscity device reject <code>
syscity device revoke <id>
syscity device qr <code>         # render a QR / setup URL for the device
```

Pairing codes are 8-character unambiguous strings and can be delivered as a QR
code or a `syscity://pair/{code}` URI (`src/security/device_pairing.rs`).

### Tailscale mode

Derives identity from the Tailscale tailnet via `tailscale whois` (cached).

```toml
[security]
enabled = true
auth_required = true
auth_mode = "tailscale"
allowed_tailnets = ["example.ts.net"]   # empty = any tailnet allowed
tailscale_auth_ttl_secs = 300           # whois cache TTL
```

Requires the host to be on a tailnet.

## Scopes

Sessions carry scopes that gate WebSocket RPC methods
(`src/gateway/protocol.rs`):

| Scope | Grants |
|-------|--------|
| `chat` | `chat.send`, `chat.abort` |
| `read` | read-only queries, `acp.list/status/tree` |
| `write` | task mutation, config writes |
| `acp` | sub-agent execution |
| `pairing` | receives `device.pair.requested` — the event carrying a pairing code. It gates no RPC method of its own. |
| `admin` | admin/control-plane operations |

Granted scopes come from the credential, never from the request: a client may
ask for *less* than it is entitled to, never more (`resolve_scopes`). The
defaults are `chat`+`read`+`write`+`pairing` for anonymous local clients
(`local_scopes`) and `chat`+`read`+`pairing` for a `shared_token`
(`shared_token_scopes`) — all configurable. Narrowing a client's scopes is also
how pairing codes are kept away from it. Admin commands require `admin`.

## Rate limiting

`[security.rate_limit]` supports a legacy token bucket and a multi-tier
sliding-window limiter.

```toml
[security.rate_limit]
enabled = true
multi_tier = true       # use the sliding-window tiers below
loopback_exempt = true  # skip limits for 127.0.0.1 / ::1

# Each tier: enabled / capacity (requests) / window_secs
[security.rate_limit.global]
enabled = true
capacity = 100
window_secs = 60

[security.rate_limit.per_user]
capacity = 100
window_secs = 60

[security.rate_limit.per_ip]
capacity = 100
window_secs = 60
```

Available tiers: `global`, `per_user`, `per_ip`, `per_endpoint`,
`shared_secret`, `device_token`, `hook_auth`, `control_plane_write`. A
`[security.rate_limit.lockout]` section configures lockout after repeated
failures. If `multi_tier = false`, the legacy `capacity` / `refill_rate`
token-bucket fields apply instead.

## Security headers (CORS / CSP)

Enabled by `security_headers = true`.

```toml
[security.cors]
enabled = true
allowed_origins = ["*"]          # tighten to your UI origin in production
allow_credentials = false        # must stay false while allowed_origins is "*"
max_age_secs = 3600

[security.csp]
enabled = true
use_nonce = true                 # nonce inline scripts
# policy = "default-src 'self'; …"  # override the default policy if needed
```

`allowed_origins = ["*"]` and `allow_credentials = true` are refused together,
and the gateway **refuses to start** on the pair rather than quietly dropping
one half of it. A wildcard origin with credentials tells every browser that any
site may make credentialed requests, which turns "allow anything" from a
convenience into a cross-origin read. To send credentials across origins, list
the origins you trust instead of the wildcard:

```toml
[security.cors]
allowed_origins = ["https://your-ui.example"]
allow_credentials = true
```

Nothing in Syscity asks for `allow_credentials`: cookies are not a credential
here — a client presents a bearer token or a single-use WebSocket upgrade
ticket — and the flag only decides whether a *browser* is permitted to attach
cookies it may already hold.

The default CSP restricts scripts/styles to `'self'`, allows `ws:`/`wss:` for
the WebSocket connection, and sets `frame-ancestors 'none'`. In production,
narrow `cors.allowed_origins` to your actual UI origin rather than `*`.

## Trusted proxy

When Syscity runs behind a reverse proxy, configure trusted-proxy handling so
client IPs and forwarded identities are honored only from known proxies.

```toml
trusted_proxies = ["10.0.0.1"]   # IPs allowed to set X-Forwarded-For

[security.trusted_proxy]
# IP whitelist/CIDR, required headers, user extraction, allowUsers whitelist
```

Never trust forwarding headers from arbitrary clients — only list proxies you
control.

## Credentials from the environment

Secrets can come from environment variables instead of the config file. The
`credential_precedence` setting controls who wins:

```toml
[security]
credential_precedence = "env_first"   # env_first | config_first
```

- `env_first` — the env var always overrides the config value.
- `config_first` — env var is used only when the config value is empty.

Relevant variables (applied in `src/daemon.rs`):

| Variable | Sets |
|----------|------|
| `SYSCITY_SECURITY_SHARED_TOKEN` | `security.shared_token` |
| `SYSCITY_API_KEY` + `SYSCITY_BASE_URL` | LLM provider credentials |

For other secrets, config values support `SecretRef` resolution via `env`,
`file`, and `exec` providers (`src/security/`).

## Recommended production baseline

```toml
[security]
enabled = true
auth_required = true
auth_mode = "token"            # or "device" / "tailscale"
shared_token = "…"             # via SYSCITY_SECURITY_SHARED_TOKEN in prod
security_headers = true
credential_precedence = "env_first"

[security.rate_limit]
enabled = true
multi_tier = true
loopback_exempt = true

[security.cors]
allowed_origins = ["https://your-ui.example.com"]
```

Then keep the secret out of the file:

```bash
export SYSCITY_SECURITY_SHARED_TOKEN="$(openssl rand -hex 32)"
syscity start
```

## See also

- [Getting Started](getting-started.md)
- [Security module design](modules/security.md)
- [Gateway](modules/gateway.md)
- [Protocol](protocol.md)
