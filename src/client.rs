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
        }
    }

    /// Create a new client connecting to the unified gateway port
    pub fn with_ws(host: &str, port: u16) -> Self {
        Self {
            client: Client::new(),
            base_url: format!("http://{}:{}", host, port),
            ws_url: format!("ws://{}:{}/ws", host, port),
        }
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

        let (ws_stream, _) = connect_async(&self.ws_url).await.map_err(|e| {
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

        // Connect handshake.
        send_frame(&mut write, "conn", "connect", &json!({ "protocol_version": 1 })).await?;
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
