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

    /// Refuse a remote setting this client could never connect with.
    ///
    /// Checked before the file is written, not when the connection is opened:
    /// a saved-but-impossible config is invisible until the next launch, and
    /// in remote mode that launch is a spinner with no way into Settings —
    /// remote mode never falls back to a local gateway. Failing here keeps
    /// the previous, working config on disk instead.
    ///
    /// `Local` needs nothing: host / port / token are ignored in that mode.
    pub fn validate(&self) -> Result<(), String> {
        if self.mode != ConnectionMode::Remote {
            return Ok(());
        }
        if self.host.trim().is_empty() {
            return Err("Remote mode needs a host — enter the gateway's address".to_string());
        }
        // `remote_base` formats `scheme://host:port`, so a scheme pasted into
        // the host field would produce `http://http://…`. Which scheme to use
        // is the `tls` flag's job, so say that rather than silently accepting
        // a value that cannot work.
        if self.host.contains("://") {
            return Err("Enter the host on its own (no http:// or https://) and use TLS for \
                 https / wss"
                .to_string());
        }
        if self.port == 0 {
            return Err("Remote mode needs a port between 1 and 65535".to_string());
        }
        Ok(())
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
///
/// Validation happens here rather than at the call site so that every writer
/// is covered, and it happens *before* the write so a rejected config leaves
/// the previous one in place.
pub fn save_connection(config: &ConnectionConfig) -> Result<(), String> {
    save_connection_to(&connection_file(), config)
}

/// `save_connection` against an explicit path, so the refusal path can be
/// tested without writing to (or reading from) the real `~/.syscity`.
fn save_connection_to(path: &std::path::Path, config: &ConnectionConfig) -> Result<(), String> {
    config.validate()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = toml::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(path, content).map_err(|e| e.to_string())
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

    fn remote(host: &str, port: u16) -> ConnectionConfig {
        ConnectionConfig {
            mode: ConnectionMode::Remote,
            host: host.into(),
            port,
            tls: false,
            token: None,
        }
    }

    /// A remote config that cannot work must be refused at save time. These are
    /// the three ways a person gets one: an empty host left behind by a click on
    /// "Remote", a pasted URL in the host field, and a port cleared in a number
    /// input (which arrives as 0).
    ///
    /// Refusing here is not cosmetic: remote mode has no local fallback, so a
    /// saved impossible config means the next launch parks on the startup
    /// screen with no route into Settings.
    #[test]
    fn an_impossible_remote_config_is_refused() {
        assert!(remote("192.168.1.10", 18080).validate().is_ok(), "the good case");

        let empty = remote("", 18080).validate();
        assert!(empty.is_err(), "an empty host must be refused");
        assert!(empty.unwrap_err().contains("host"));

        let pasted = remote("http://gateway.example", 443).validate();
        assert!(pasted.is_err(), "a pasted URL must be refused");
        assert!(
            pasted.unwrap_err().contains("TLS"),
            "the message must name the control that replaces the scheme"
        );

        assert!(remote("gateway.example", 0).validate().is_err(), "port 0 must be refused");
    }

    /// Local mode ignores host / port / token, so a half-filled form there is
    /// not an error — refusing it would block a save that is perfectly valid.
    #[test]
    fn local_mode_validates_whatever_the_remote_fields_say() {
        let config = ConnectionConfig {
            mode: ConnectionMode::Local,
            host: String::new(),
            port: 0,
            tls: false,
            token: None,
        };
        assert!(config.validate().is_ok());
    }

    /// A refused save must not have written anything: the previous config is
    /// the one that still works.
    #[test]
    fn a_refused_save_leaves_the_previous_file_alone() {
        let dir = tempfile_dir();
        let path = dir.join("client.toml");

        let good = remote("192.168.1.10", 18080);
        save_connection_to(&path, &good).expect("the good config saves");
        let before = std::fs::read_to_string(&path).expect("read back");

        let err = save_connection_to(&path, &remote("", 18080))
            .expect_err("an impossible remote config must be refused");
        assert!(err.contains("host"), "the reason must name the field: {err}");

        let after = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(before, after, "a refused config must not reach the disk");
    }

    /// The happy path, so the test above cannot pass by never writing at all.
    #[test]
    fn a_valid_remote_config_round_trips_through_the_file() {
        let dir = tempfile_dir();
        let path = dir.join("client.toml");
        let config = ConnectionConfig {
            tls: true,
            token: Some("agent-token".into()),
            ..remote("gateway.example", 443)
        };
        save_connection_to(&path, &config).expect("save");

        let content = std::fs::read_to_string(&path).expect("read back");
        let parsed: ConnectionConfig = toml::from_str(&content).expect("reparse");
        assert_eq!(parsed, config);
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "syscity-connection-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }
}
