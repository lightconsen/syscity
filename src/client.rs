//! API Client for connecting to Syscity daemon
//!
//! Provides a client for CLI/web commands to connect to the running daemon.
//! The gateway is WS-native: the management surface is driven over the WS RPC
//! protocol via [`DaemonClient::ws_call`]; the only HTTP the client uses is the
//! `/health` liveness probe.

// INVARIANTS-NONE: CLI/daemon client; owns no mutable runtime state
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

/// Daemon API client
#[derive(Clone)]
pub struct DaemonClient {
    client: Client,
    base_url: String,
    ws_url: String,
    /// Credential for a gateway that authenticates (`security.auth_mode`).
    ///
    /// Without one the CLI could only talk to `auth_mode = "none"` gateways:
    /// `ws_call` opened the socket with no credential at all, so a token-mode
    /// gateway refused the upgrade and every CLI command failed with a generic
    /// connect error.
    token: Option<String>,
}

/// Health response
#[derive(Debug, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub agent: String,
}

impl DaemonClient {
    /// Create a new client
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            client: Client::new(),
            base_url: format!("http://{}:{}", host, port),
            ws_url: format!("ws://{}:{}/chat/stream", host, port),
            token: None,
        }
    }

    /// Create a new client connecting to the unified gateway port
    pub fn with_ws(host: &str, port: u16) -> Self {
        Self {
            client: Client::new(),
            base_url: format!("http://{}:{}", host, port),
            ws_url: format!("ws://{}:{}/ws", host, port),
            token: None,
        }
    }

    /// Present `token` as this client's credential.
    ///
    /// `None` means the gateway is expected not to require one (`auth_mode`
    /// `none`, the local default).
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }

    /// Check if daemon is running and has AI agent
    pub async fn health(&self) -> crate::Result<HealthResponse> {
        let url = format!("{}/health", self.base_url);
        let response = self.client.get(&url).send().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("Failed to connect: {}", e))
        })?;

        let health: HealthResponse = response.json().await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("Invalid response: {}", e))
        })?;

        Ok(health)
    }

    /// Generic WebSocket RPC call: connects, performs the `connect` handshake,
    /// sends `method` with `params`, and returns the response payload.
    ///
    /// The gateway's WS protocol expects the `connect` frame first
    /// (`{protocol_version: 1}`), then a request frame; the response is a
    /// `WsResponse` with `ok`/`payload`/`error`.
    pub async fn ws_call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> crate::Result<serde_json::Value> {
        use serde_json::json;

        let request = ws_request(&self.ws_url, self.token.as_deref())?;
        let (ws_stream, _) = connect_async(request).await.map_err(|e| {
            crate::error::SyscityError::Internal(format!("WebSocket connect failed: {}", e))
        })?;
        let (mut write, mut read) = ws_stream.split();

        async fn send_frame<W>(
            write: &mut W,
            id: &str,
            method: &str,
            params: &serde_json::Value,
        ) -> crate::Result<()>
        where
            W: futures::Sink<Message> + Unpin,
            W::Error: std::fmt::Display,
        {
            let frame = json!({ "type": "req", "id": id, "method": method, "params": params });
            let text = serde_json::to_string(&frame)
                .map_err(|e| crate::error::SyscityError::Internal(format!("JSON error: {}", e)))?;
            futures::SinkExt::send(write, Message::Text(text))
                .await
                .map_err(|e| {
                    crate::error::SyscityError::Internal(format!("WebSocket send failed: {}", e))
                })
        }

        async fn read_resp<R>(read: &mut R) -> crate::Result<serde_json::Value>
        where
            R: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
                + Unpin,
        {
            if let Some(msg) = futures::StreamExt::next(read).await {
                match msg {
                    Ok(Message::Text(text)) => Ok(serde_json::from_str::<serde_json::Value>(&text)
                        .map_err(|e| {
                            crate::error::SyscityError::Internal(format!("Invalid response: {}", e))
                        })?),
                    Ok(Message::Close(_)) => {
                        Err(crate::error::SyscityError::Internal("WebSocket closed".to_string()))
                    }
                    Err(e) => {
                        Err(crate::error::SyscityError::Internal(format!("WebSocket error: {}", e)))
                    }
                    _ => Err(crate::error::SyscityError::Internal(
                        "Unexpected message type".to_string(),
                    )),
                }
            } else {
                Err(crate::error::SyscityError::Internal("No response received".to_string()))
            }
        }

        // Connect handshake. The token goes in the frame as well as the
        // upgrade header: the two are checked by different layers (the upgrade
        // middleware and the handshake), and a gateway that only trusts one of
        // them should still accept the CLI.
        send_frame(&mut write, "conn", "connect", &connect_params(self.token.as_deref())).await?;
        loop {
            let resp = read_resp(&mut read).await?;
            if resp["id"].as_str() == Some("conn") {
                break;
            }
        }

        // Send the actual method and read its response.
        send_frame(&mut write, "req", method, &params).await?;
        loop {
            let resp = read_resp(&mut read).await?;
            if resp["id"].as_str() != Some("req") {
                continue;
            }
            if resp["ok"].as_bool().unwrap_or(false) {
                return Ok(resp
                    .get("payload")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null));
            }
            let msg = resp
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("gateway error");
            return Err(crate::error::SyscityError::Internal(msg.to_string()));
        }
    }

    /// Check if daemon is available
    pub async fn is_available(&self) -> bool {
        self.health().await.is_ok()
    }

    /// Get default client using standard daemon address
    pub fn default_client() -> Self {
        Self::with_ws("127.0.0.1", 18080)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_client_new() {
        let client = DaemonClient::new("127.0.0.1", 18080);
        assert_eq!(client.base_url, "http://127.0.0.1:18080");
        assert_eq!(client.ws_url, "ws://127.0.0.1:18080/chat/stream");
    }

    #[test]
    fn test_daemon_client_with_ws() {
        let client = DaemonClient::with_ws("127.0.0.1", 18080);
        assert_eq!(client.base_url, "http://127.0.0.1:18080");
        assert_eq!(client.ws_url, "ws://127.0.0.1:18080/ws");
    }

    #[test]
    fn test_daemon_client_default_client() {
        let client = DaemonClient::default_client();
        assert_eq!(client.base_url, "http://127.0.0.1:18080");
        assert_eq!(client.ws_url, "ws://127.0.0.1:18080/ws");
    }

    #[test]
    fn test_health_response_deserialize() {
        let json = r#"{"status":"ok","agent":"ready"}"#;
        let health: HealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(health.status, "ok");
        assert_eq!(health.agent, "ready");
    }
}

/// Check if daemon is running, returning helpful error if not
pub async fn check_daemon() -> crate::Result<DaemonClient> {
    let client = DaemonClient::default_client();

    match client.health().await {
        Ok(health) => {
            if health.agent == "ready" {
                Ok(client)
            } else {
                Err(crate::error::SyscityError::Internal(
                    "Daemon is running but AI agent is not configured.\nSet SYSCITY_BASE_URL and \
                     SYSCITY_API_KEY, then restart daemon."
                        .to_string(),
                ))
            }
        }
        Err(_) => Err(crate::error::SyscityError::Internal(
            "Daemon is not running.\nStart it with: syscity start".to_string(),
        )),
    }
}

/// The upgrade request for `url`, presenting `token` as a Bearer credential
/// when there is one.
///
/// A browser cannot set headers on a WebSocket upgrade; a Rust client can, and
/// the CLI is one — so the credential never has to go in the URL here.
fn ws_request(
    url: &str,
    token: Option<&str>,
) -> crate::Result<tokio_tungstenite::tungstenite::handshake::client::Request> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    // `into_client_request` is what `connect_async(&url)` uses internally: it
    // adds the handshake headers (`sec-websocket-key` and friends) that a
    // hand-built request does not have.
    let mut request = url.into_client_request().map_err(|e| {
        crate::error::SyscityError::Internal(format!("Failed to build WS request: {e}"))
    })?;
    if let Some(token) = token {
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {token}").parse().map_err(|e| {
                crate::error::SyscityError::Internal(format!("Invalid token header: {e}"))
            })?,
        );
    }
    Ok(request)
}

/// The `connect` frame's params, with the credential when there is one.
fn connect_params(token: Option<&str>) -> serde_json::Value {
    match token {
        Some(token) => serde_json::json!({ "protocol_version": 1, "auth": { "token": token } }),
        None => serde_json::json!({ "protocol_version": 1 }),
    }
}

#[cfg(test)]
mod token_tests {
    use super::*;

    #[test]
    fn the_upgrade_carries_the_token_when_there_is_one() {
        let request = ws_request("ws://127.0.0.1:18080/ws", Some("s3cret")).unwrap();
        assert_eq!(request.headers().get("authorization").unwrap(), "Bearer s3cret");
        // The upgrade's own headers must survive: a request built by hand
        // without `into_client_request` loses `sec-websocket-key`, and the
        // gateway refuses it with a protocol error.
        assert!(
            request.headers().contains_key("sec-websocket-key"),
            "the handshake headers come with the request"
        );
        assert!(
            !request.uri().to_string().contains("s3cret"),
            "the credential belongs in the header, not the URL"
        );

        let anonymous = ws_request("ws://127.0.0.1:18080/ws", None).unwrap();
        assert!(anonymous.headers().get("Authorization").is_none());
    }

    #[test]
    fn the_handshake_repeats_the_token_when_there_is_one() {
        assert_eq!(
            connect_params(Some("s3cret")),
            serde_json::json!({ "protocol_version": 1, "auth": { "token": "s3cret" } })
        );
        assert_eq!(connect_params(None), serde_json::json!({ "protocol_version": 1 }));
    }
}
