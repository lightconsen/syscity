//! Authentication configuration for the TUI WebSocket connection.

use crate::gateway::protocol::AuthMode;

/// Auth mode and credentials for the TUI client.
#[derive(Debug, Clone)]
pub enum AuthConfig {
    /// No authentication (development / local desktop mode).
    None,
    /// Shared secret token passed as a query parameter.
    Token {
        /// The shared token.
        token: String,
    },
}

impl AuthConfig {
    /// Build an `AuthConfig` from an optional token.
    pub fn from_token(token: Option<&str>) -> Self {
        match token {
            Some(t) if !t.is_empty() => Self::Token { token: t.to_string() },
            _ => Self::None,
        }
    }

    /// The `Authorization` header value, when there is a credential.
    ///
    /// The credential belongs here rather than in the URL: a WebSocket
    /// upgrade can carry headers when the client can set them, and this one
    /// can. A URL reaches the gateway's access log, the process list and every
    /// proxy in between.
    pub fn bearer(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Token { token } => Some(format!("Bearer {token}")),
        }
    }

    /// Build the WebSocket URL.
    ///
    /// Deliberately carries no credential — see [`AuthConfig::bearer`]. The
    /// gateway also accepts `?token=`, because a browser cannot set headers on
    /// an upgrade, but nothing here needs that.
    pub fn ws_url(&self, host: &str, port: u16, session_id: Option<&str>, client: &str) -> String {
        let mut url = format!("ws://{}:{}/ws", host, port);
        let mut first = true;

        let mut append = |key: &str, value: &str| {
            let sep = if first {
                first = false;
                "?"
            } else {
                "&"
            };
            url.push_str(sep);
            url.push_str(key);
            url.push('=');
            url.push_str(&urlencoding::encode(value));
        };

        if let Some(sid) = session_id {
            append("session_id", sid);
        }
        if !client.is_empty() {
            append("client", client);
        }

        url
    }

    /// Build the HTTP base URL.
    #[allow(dead_code)]
    pub fn http_url(&self, host: &str, port: u16) -> String {
        format!("http://{}:{}", host, port)
    }

    /// Return the configured auth mode for protocol handshake.
    #[allow(dead_code)]
    pub fn auth_mode(&self) -> AuthMode {
        match self {
            Self::None => AuthMode::None,
            Self::Token { .. } => AuthMode::Token,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_without_auth() {
        let auth = AuthConfig::None;
        assert_eq!(
            auth.ws_url("127.0.0.1", 18080, None, "tui"),
            "ws://127.0.0.1:18080/ws?client=tui"
        );
    }

    /// The URL never carries the credential, whatever else it carries.
    #[test]
    fn ws_url_keeps_the_token_out_of_the_url() {
        let auth = AuthConfig::Token {
            token: "secret token".to_string(),
        };
        let url = auth.ws_url("127.0.0.1", 18080, Some("sess-1"), "tui");
        assert_eq!(url, "ws://127.0.0.1:18080/ws?session_id=sess-1&client=tui");
        assert!(!url.contains("secret"), "a URL is not a place for a credential");
    }

    #[test]
    fn the_token_becomes_a_bearer_header() {
        assert_eq!(
            AuthConfig::Token { token: "s3cret".to_string() }
                .bearer()
                .as_deref(),
            Some("Bearer s3cret")
        );
        assert_eq!(AuthConfig::None.bearer(), None);
    }
}
