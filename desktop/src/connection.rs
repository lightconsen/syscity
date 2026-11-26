//! Desktop client connection settings: local (embedded/reused gateway) vs
//! remote (a gateway running on another host, authenticated with a token).
//!
//! Stored in `~/.syscity/client.toml`, separate from `config.toml` because the
//! two answer different questions: `config.toml` is the *gateway's* settings
//! and is only read when this app runs or reuses a gateway; `client.toml` is
//! *this client's* choice of gateway, and is read on every launch.
//!
//! There is deliberately no `agent_id` field. A token that is an agent access
//! token (`[security.agent_tokens.<label>]`) carries its agent inside it, and
//! the gateway answers which one in the `connect` handshake (`bound_agent`).
//! Storing the agent here as well would let the two disagree, and the one the
//! user typed is the one with no authority.

use serde::{Deserialize, Serialize};

/// Which gateway this desktop app should use.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionMode {
    /// Run / reuse a local gateway (loopback).
    #[default]
    Local,
    /// Connect to a gateway on another host.
    Remote,
}

/// Client connection settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectionConfig {
    pub mode: ConnectionMode,
    /// Remote gateway host (ignored in Local mode).
    pub host: String,
    /// Remote gateway port (ignored in Local mode).
    pub port: u16,
    /// Talk to the remote gateway over TLS (`https://` / `wss://`).
    ///
    /// A gateway reachable from outside a LAN is normally behind a reverse
    /// proxy that terminates TLS; the plaintext path is for a local or
    /// tailnet-only gateway. Ignored in Local mode.
    #[serde(default)]
    pub tls: bool,
    /// Remote gateway credential (ignored in Local mode).
    ///
    /// Either the gateway's `security.shared_token` or one of its
    /// `[security.agent_tokens.<label>]` entries. The gateway decides which,
    /// and an agent token narrows this client to that one agent — no client
    /// setting can widen that.
    pub token: Option<String>,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            mode: ConnectionMode::Local,
            host: "127.0.0.1".to_string(),
            port: 18080,
            tls: false,
            token: None,
        }
    }
}

impl ConnectionConfig {
    /// The URL scheme for HTTP requests to this gateway.
    ///
    /// The frontend derives the WebSocket URL from this one by swapping the
    /// scheme (`http` → `ws`), so there is deliberately no second accessor —
    /// two places that both decide a scheme is how they drift apart.
    pub fn http_scheme(&self) -> &'static str {
        if self.tls {
            "https"
        } else {
            "http"
        }
    }
}

/// Path to the connection settings file.
pub fn connection_file() -> std::path::PathBuf {
    syscity::dirs::config_dir().join("client.toml")
}

/// Load connection settings, falling back to Local defaults.
pub fn load_connection() -> ConnectionConfig {
    let path = connection_file();
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(config) = toml::from_str::<ConnectionConfig>(&content) {
            return config;
        }
    }
    ConnectionConfig::default()
}

/// Persist connection settings.
///
/// The token is written as-is, like `config.toml`'s own credentials: the file
/// is the user's, in the user's home directory, with the platform's usual
/// permissions. A keychain would be better and is not what this file is.
pub fn save_connection(config: &ConnectionConfig) -> Result<(), String> {
    let path = connection_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = toml::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(&path, content).map_err(|e| e.to_string())
}

/// The remote gateway base URL (`https://host:port` or `http://host:port`).
pub fn remote_base(config: &ConnectionConfig) -> String {
    format!("{}://{}:{}", config.http_scheme(), config.host, config.port)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An existing `client.toml` — written before `tls` existed — must still
    /// parse, and default to plaintext rather than fail the whole file.
    #[test]
    fn an_old_client_toml_still_parses() {
        let parsed: ConnectionConfig = toml::from_str(
            r#"
mode = "remote"
host = "gateway.example"
port = 443
token = "secret"
"#,
        )
        .expect("a client.toml without `tls` must deserialize");
        assert_eq!(parsed.mode, ConnectionMode::Remote);
        assert_eq!(parsed.host, "gateway.example");
        assert!(!parsed.tls, "tls defaults off, so an old file keeps working");
        assert_eq!(parsed.token.as_deref(), Some("secret"));
    }

    /// The scheme is the whole point of the field: `remote_base` has to produce
    /// what the WebView (and the CSP) can actually connect to.
    #[test]
    fn tls_selects_the_scheme() {
        let plain = ConnectionConfig {
            mode: ConnectionMode::Remote,
            host: "192.168.1.10".into(),
            port: 18080,
            tls: false,
            token: None,
        };
        assert_eq!(remote_base(&plain), "http://192.168.1.10:18080");

        let secure = ConnectionConfig { tls: true, ..plain.clone() };
        assert_eq!(remote_base(&secure), "https://192.168.1.10:18080");

        // The frontend derives `wss` by swapping the scheme, so both spellings
        // have to round-trip through the serialized form as well.
        let serialized = toml::to_string(&secure).expect("serialize");
        let reparsed: ConnectionConfig = toml::from_str(&serialized).expect("reparse");
        assert_eq!(reparsed, secure);
    }
}
