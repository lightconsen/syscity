//! CLI WebSocket helper — calls the gateway's WS RPC surface.
//!
//! The CLI is migrating from REST (`DAEMON_URL/api/v1/...`) to the same WS
//! transport the UI uses. `call` opens a short-lived connection, performs the
//! `connect` handshake, sends one method, and returns the payload.

use serde_json::Value;

use crate::client::DaemonClient;
use crate::error::SyscityError;

/// Fallback endpoint when no configuration can be read.
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 18080;

/// The daemon endpoint the CLI talks to.
///
/// Resolved per call because the port is a runtime choice — the daemon may have
/// been started with `--port` — and most subcommands (`cron`, `skill`, `plugin`,
/// `memory`, `mcp`, …) take no endpoint flag of their own.
///
/// Sources, in order:
///
/// 1. `SYSCITY_SERVER_HOST` / `SYSCITY_SERVER_PORT` (the names the rest of the
///    CLI already honours for the server it talks to).
/// 2. The gateway configuration file (`<root>/config.toml`) — the same file the
///    daemon reads, whose `host`/`port` are *flat* keys. Note this is the
///    gateway's schema, not `config::Config`'s `[server]` section: they are two
///    shapes over one file, and the gateway one is what the daemon binds from.
/// 3. [`DEFAULT_HOST`]/[`DEFAULT_PORT`].
///
/// A config that cannot be read falls back rather than failing the call — the
/// connection error that follows says more than a parse error would.
pub fn endpoint() -> (String, u16) {
    let file = gateway_config_endpoint();
    let host = env_or("SYSCITY_SERVER_HOST")
        .or_else(|| file.as_ref().map(|(h, _)| h.clone()))
        .unwrap_or_else(|| DEFAULT_HOST.to_string());
    let port = env_or("SYSCITY_SERVER_PORT")
        .and_then(|p| p.parse().ok())
        .or_else(|| file.as_ref().map(|(_, p)| *p))
        .unwrap_or(DEFAULT_PORT);
    (host, port)
}

fn env_or(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The flat `host`/`port` of the gateway configuration file, if readable.
fn gateway_config_endpoint() -> Option<(String, u16)> {
    let path = crate::dirs::paths().default_config_file();
    let content = std::fs::read_to_string(path).ok()?;
    let config: crate::gateway::GatewayConfig = toml::from_str(&content).ok()?;
    Some((config.host, config.port))
}

/// Invoke one WS method on the daemon and return the response payload.
pub async fn call(method: &str, params: Value) -> crate::Result<Value> {
    let (host, port) = endpoint();
    let client = DaemonClient::with_ws(&host, port).with_token(gateway_token());
    client.ws_call(method, params).await
}

/// The credential to present to the gateway, if any.
///
/// Sources, in order:
///
/// 1. `SYSCITY_GATEWAY_TOKEN` — the name `docs/protocol.md` has always
///    documented for this, and which nothing read until now.
/// 2. `security.shared_token` from the same `config.toml` the endpoint comes
///    from. The CLI runs on the operator's machine and reads their config
///    already; requiring the token to be exported as well would make every
///    command fail on a token-mode gateway for no gain.
///
/// `auth_mode = "none"` (the local default) needs neither.
fn gateway_token() -> Option<String> {
    if let Some(token) = env_or("SYSCITY_GATEWAY_TOKEN") {
        return Some(token);
    }
    let path = crate::dirs::paths().default_config_file();
    let content = std::fs::read_to_string(path).ok()?;
    let config: crate::gateway::GatewayConfig = toml::from_str(&content).ok()?;
    config
        .security
        .shared_token
        .filter(|token| !token.is_empty())
}

/// Convenience: invoke a WS method and parse the payload into `T`.
#[allow(dead_code)]
pub async fn call_typed<T: serde::de::DeserializeOwned>(
    method: &str,
    params: Value,
) -> crate::Result<T> {
    let payload = call(method, params).await?;
    serde_json::from_value(payload)
        .map_err(|e| SyscityError::Internal(format!("Invalid WS response: {}", e)))
}
