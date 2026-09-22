//! Security, authentication, scopes and rate-limiting configuration.

use super::*;
/// Credential source precedence for tokens, API keys, and passwords.
///
/// Controls which source wins when both environment variables and the
/// configuration file supply the same credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CredentialPrecedence {
    /// Environment variables take precedence over the config file.
    #[default]
    EnvFirst,
    /// The config file takes precedence over environment variables.
    ConfigFirst,
}

/// Security configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Enable security features (auth, rate limiting, security headers)
    pub enabled: bool,
    /// Require authentication for API access
    pub auth_required: bool,
    /// Require pairing for new users
    pub pairing_required: bool,
    /// Authentication mode
    #[serde(default)]
    pub auth_mode: crate::gateway::protocol::AuthMode,
    /// Shared secret token for simple authentication
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared_token: Option<String>,

    // ── Scope entitlements ───────────────────────────────────────────────
    //
    // What each credential is *worth*. A connection's granted scopes are the
    // intersection of these with whatever the client asks for, so a client can
    // narrow its access but never widen it (`protocol::resolve_scopes`). Keep
    // the sets here rather than in code: granting `admin` to an unauthenticated
    // local client has to be a visible decision in the config, not a default
    // buried in a match arm.
    /// Scopes granted to anonymous clients when `auth_mode = "none"`.
    ///
    /// Deliberately excludes `admin`: the local-only mode still runs a runtime
    /// that executes shell commands and drives the desktop, so "reachable on
    /// loopback" is not a reason to hand over the control plane.
    #[serde(default = "default_local_scopes")]
    pub local_scopes: Vec<String>,
    /// Scopes granted to a client presenting `shared_token`.
    #[serde(default = "default_shared_token_scopes")]
    pub shared_token_scopes: Vec<String>,
    /// Scopes granted to a paired device (`auth_mode = "device"`).
    ///
    /// Named per-deployment rather than per-device; a device-scoped entitlement
    /// recorded at pairing time is the natural follow-up.
    #[serde(default = "default_device_scopes")]
    pub device_scopes: Vec<String>,
    /// Browser origins allowed to open a WebSocket to `/ws`, in addition to the
    /// gateway's own origins and the desktop (Tauri) ones.
    ///
    /// A browser always sends `Origin` on a WebSocket upgrade; a non-browser
    /// client (CLI, TUI) sends none. Only Origins listed here — or the built-ins
    /// — are accepted, which is what stops a web page the user visits from
    /// driving the local runtime.
    #[serde(default)]
    pub allowed_ws_origins: Vec<String>,
    /// Allow `auth_mode = "none"` on a non-loopback bind.
    ///
    /// Off by default: an unauthenticated listener on a reachable interface is
    /// almost never what someone means, and the failure is silent otherwise.
    #[serde(default)]
    pub allow_non_loopback_without_auth: bool,

    /// Cap on simultaneous WebSocket connections to `/ws`.
    ///
    /// Better to refuse a new connection than to let one more socket claim a
    /// slot forever. Before this existed there was no bound at all.
    #[serde(default = "default_max_ws_connections")]
    pub max_ws_connections: usize,

    /// Rate limiting configuration
    pub rate_limit: RateLimitConfig,
    /// Enable security headers
    pub security_headers: bool,

    /// CORS configuration
    #[serde(default)]
    pub cors: crate::gateway::auth::CorsConfig,
    /// CSP configuration
    #[serde(default)]
    pub csp: crate::gateway::auth::CspConfig,
    /// Mention gating configuration
    #[serde(default)]
    pub mention_gating: crate::security::mention_gate::MentionGatingConfig,
    /// Allowed Tailscale tailnets (empty = any tailnet allowed when
    /// auth_mode=tailscale)
    #[serde(default)]
    pub allowed_tailnets: Vec<String>,
    /// Trusted proxy IPs for X-Forwarded-For header resolution
    #[serde(default)]
    pub trusted_proxies: Vec<std::net::IpAddr>,
    /// Tailscale whois cache TTL in seconds (default 300)
    #[serde(default = "default_tailscale_ttl")]
    pub tailscale_auth_ttl_secs: u64,
    /// Trusted proxy authentication configuration.
    #[serde(default)]
    pub trusted_proxy: crate::security::trusted_proxy::TrustedProxyConfig,
    /// Credential source precedence for tokens, API keys, and passwords.
    #[serde(default)]
    pub credential_precedence: CredentialPrecedence,

    /// Deny outbound network to fenced command tools (`shell`, `code_exec`,
    /// `process`) instead of only fencing their writes.
    ///
    /// Off by default: `curl`, `git fetch`, package installs and every other
    /// networked command are ordinary work, and turning the network off by
    /// default would break them rather than protect anything the user did
    /// not ask to protect. When on, the kernel fence grows a network clause
    /// per platform — Seatbelt `(deny network*)` on macOS, a seccomp socket
    /// filter on Linux, and no AppContainer network capability SIDs on
    /// Windows. Nothing about this is per-call: it is a posture for every
    /// fenced command in the deployment.
    #[serde(default)]
    pub fence_network: bool,
}

fn default_tailscale_ttl() -> u64 {
    300
}

/// Rate limiting configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
// A partial table in the config file must fill the rest from the defaults
// rather than fail the whole parse — the outer `#[serde(default)]` only
// covers *absent* fields, not a present-but-incomplete nested table.
#[serde(default)]
pub struct RateLimitConfig {
    /// Enable rate limiting
    pub enabled: bool,
    /// Maximum requests per window (legacy token bucket)
    pub capacity: u32,
    /// Refill rate (tokens per second) (legacy token bucket)
    pub refill_rate: f64,
    /// Use multi-tier sliding window rate limiting instead of token bucket
    #[serde(default)]
    pub multi_tier: bool,
    /// Global tier: overall API rate limit
    #[serde(default)]
    pub global: TierConfig,
    /// Per-authenticated-user rate limit
    #[serde(default)]
    pub per_user: TierConfig,
    /// Per-IP rate limit (for anonymous requests)
    #[serde(default)]
    pub per_ip: TierConfig,
    /// Per-endpoint rate limit
    #[serde(default)]
    pub per_endpoint: TierConfig,
    /// Shared-secret authentication scope rate limit.
    #[serde(default)]
    pub shared_secret: TierConfig,
    /// Device-token authentication scope rate limit.
    #[serde(default)]
    pub device_token: TierConfig,
    /// Webhook/hook authentication scope rate limit.
    #[serde(default)]
    pub hook_auth: TierConfig,
    /// Control-plane write operation rate limit.
    #[serde(default)]
    pub control_plane_write: TierConfig,
    /// Lockout configuration for repeated failures.
    #[serde(default)]
    pub lockout: crate::security::sliding_window::LockoutConfig,
    /// Skip rate limiting for loopback addresses.
    #[serde(default)]
    pub loopback_exempt: bool,
}

/// Single tier configuration for multi-tier rate limiting
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierConfig {
    /// Enable this tier
    pub enabled: bool,
    /// Maximum requests per window
    pub capacity: u32,
    /// Window size in seconds
    pub window_secs: u64,
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            capacity: 100,
            window_secs: 60,
        }
    }
}

/// Scopes an anonymous local client gets when `auth_mode = "none"`.
///
/// Chatting, reading and the write-side it needs (sessions, config, files) —
/// but not `admin`, which is what made the old behaviour a privilege
/// escalation rather than a convenience.
pub fn default_local_scopes() -> Vec<String> {
    vec![
        crate::gateway::protocol::SCOPE_CHAT.to_string(),
        crate::gateway::protocol::SCOPE_READ.to_string(),
        crate::gateway::protocol::SCOPE_WRITE.to_string(),
        // `pairing` grants no method of its own; it decides which connections
        // receive `device.pair.requested` — the event carrying a pairing code.
        // A local client is the operator, so it keeps seeing them; narrowing
        // this away is how an operator hides pairing codes from a client.
        crate::gateway::protocol::SCOPE_PAIRING.to_string(),
    ]
}

/// Scopes a client presenting `shared_token` gets: the same read-mostly pair a
/// client gets by default.
pub fn default_shared_token_scopes() -> Vec<String> {
    vec![
        crate::gateway::protocol::SCOPE_CHAT.to_string(),
        crate::gateway::protocol::SCOPE_READ.to_string(),
        // See `default_local_scopes`: without `pairing` a token-authenticated
        // client would stop receiving device pairing codes, which is a UX
        // regression rather than a hardening win.
        crate::gateway::protocol::SCOPE_PAIRING.to_string(),
    ]
}

/// Scopes a paired device gets — the mobile and desktop apps need the write
/// side to open sessions and drive tools.
pub fn default_device_scopes() -> Vec<String> {
    default_local_scopes()
}

/// Default cap on simultaneous WebSocket connections.
pub fn default_max_ws_connections() -> usize {
    256
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auth_required: false,
            pairing_required: false,
            auth_mode: crate::gateway::protocol::AuthMode::None,
            shared_token: None,
            local_scopes: default_local_scopes(),
            shared_token_scopes: default_shared_token_scopes(),
            device_scopes: default_device_scopes(),
            allowed_ws_origins: Vec::new(),
            allow_non_loopback_without_auth: false,
            max_ws_connections: default_max_ws_connections(),
            rate_limit: RateLimitConfig::default(),
            security_headers: true,
            cors: crate::gateway::auth::CorsConfig::default(),
            csp: crate::gateway::auth::CspConfig::default(),
            mention_gating: crate::security::mention_gate::MentionGatingConfig::default(),
            allowed_tailnets: Vec::new(),
            trusted_proxies: Vec::new(),
            tailscale_auth_ttl_secs: 300,
            trusted_proxy: crate::security::trusted_proxy::TrustedProxyConfig::default(),
            credential_precedence: CredentialPrecedence::default(),
            fence_network: false,
        }
    }
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            capacity: 100,
            refill_rate: 10.0,
            multi_tier: true,
            global: TierConfig {
                enabled: true,
                capacity: 1000,
                window_secs: 60,
            },
            per_user: TierConfig {
                enabled: true,
                capacity: 100,
                window_secs: 60,
            },
            per_ip: TierConfig {
                enabled: true,
                capacity: 30,
                window_secs: 60,
            },
            per_endpoint: TierConfig {
                enabled: false,
                capacity: 50,
                window_secs: 60,
            },
            shared_secret: TierConfig {
                enabled: true,
                capacity: 200,
                window_secs: 60,
            },
            device_token: TierConfig {
                enabled: true,
                capacity: 60,
                window_secs: 60,
            },
            hook_auth: TierConfig {
                enabled: true,
                capacity: 300,
                window_secs: 60,
            },
            control_plane_write: TierConfig {
                enabled: true,
                capacity: 20,
                window_secs: 60,
            },
            lockout: crate::security::sliding_window::LockoutConfig::default(),
            loopback_exempt: true,
        }
    }
}
