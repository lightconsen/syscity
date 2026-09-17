//! WebSocket client for the TUI.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream,
};
use uuid::Uuid;

use crate::tui::auth::AuthConfig;
use crate::tui::error::TuiError;
use crate::VERSION;

const PROTOCOL_VERSION: u32 = 1;

/// Client-side request frame.
#[derive(Debug, Clone, Serialize)]
struct ClientRequest {
    #[serde(rename = "type")]
    frame_type: &'static str,
    id: String,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

/// Client-side response frame.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientResponse {
    #[serde(rename = "type")]
    _frame_type: String,
    id: String,
    pub ok: bool,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    pub error: Option<ClientError>,
}

/// Client-side event frame.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientEvent {
    #[serde(rename = "type")]
    _frame_type: String,
    pub event: String,
    #[serde(default)]
    pub payload: Option<Value>,
    #[serde(default)]
    #[allow(dead_code)]
    pub seq: Option<u64>,
}

/// Error shape in a response frame.
#[derive(Debug, Clone, Deserialize)]
pub struct ClientError {
    pub code: String,
    pub message: String,
}

/// Payload of the hello-ok response.
#[derive(Debug, Clone, Deserialize)]
pub struct HelloOkPayload {
    #[allow(dead_code)]
    pub protocol_version: u32,
    #[allow(dead_code)]
    pub session_key: String,
    pub features: Vec<String>,
    pub scopes_granted: Vec<String>,
    pub server: ServerInfo,
}

/// Server info in hello-ok.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerInfo {
    pub version: String,
    #[allow(dead_code)]
    pub conn_id: String,
}

/// Message type exposed by the WebSocket client stream.
#[derive(Debug, Clone)]
pub enum WsMessage {
    /// Server event.
    Event(ClientEvent),
    /// Response without a pending waiter.
    OrphanResponse(ClientResponse),
    /// The socket closed. Sent exactly once, when the driver exits — without
    /// it the event stream would simply stop producing and the UI would sit
    /// there looking alive.
    Disconnected,
}

/// How long a request may wait for its response before the caller gives up.
///
/// Without this a wedged gateway leaves the caller awaiting forever. The
/// caller is a task rather than the event loop, so a wedged gateway no longer
/// freezes the TUI — but a command that never returns still holds its own
/// place in the queue, so the bound is what keeps that from being permanent.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Shared state tracking pending request/response waiters.
type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<ClientResponse>>>>;

/// WebSocket client connected to a Syscity gateway.
///
/// Every method takes `&self`, so one client can serve the event loop and the
/// handlers it spawns at the same time. That is what lets a command run off
/// the loop without the loop losing its connection: requests are independent
/// (each carries its own id and its own waiter), and the socket is fed by a
/// channel, so the only thing that needs a lock is the receive side.
pub struct WsClient {
    /// Channel of incoming messages (events + orphan responses).
    ///
    /// Behind a mutex purely so `next` can take `&self`; only the loop reads
    /// it, so it is never contended.
    event_rx: AsyncMutex<mpsc::UnboundedReceiver<WsMessage>>,
    /// Sender half for outgoing messages.
    write_tx: mpsc::UnboundedSender<Message>,
    /// Pending response waiters.
    pending: PendingMap,
    /// Stop signal for background tasks.
    _stop_tx: mpsc::Sender<()>,
}

impl WsClient {
    /// Connect to `url`, perform the `connect` handshake, and return the client
    /// plus the `hello-ok` payload.
    ///
    /// Deliberately subscribes to nothing: the gateway seeds the connection's
    /// subscriptions from the `session_id` in the upgrade URL, and later
    /// switches go through `gateway_calls::sessions_subscribe`. A second,
    /// private subscribe here would be a second source of truth for the same
    /// thing — and was, with the wrong parameter name, silently rejected.
    pub async fn connect(
        url: &str,
        auth: &AuthConfig,
        scopes: &[&str],
    ) -> Result<(Self, HelloOkPayload), TuiError> {
        let (ws_stream, _response) = connect_async(ws_request(url, auth)?)
            .await
            .map_err(|e| TuiError::WebSocket(e.to_string()))?;

        let (write_tx, write_rx) = mpsc::unbounded_channel::<Message>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<WsMessage>();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (stop_tx, stop_rx) = mpsc::channel::<()>(1);

        tokio::spawn(ws_driver(ws_stream, write_rx, event_tx, Arc::clone(&pending), stop_rx));

        let client = Self {
            event_rx: AsyncMutex::new(event_rx),
            write_tx,
            pending,
            _stop_tx: stop_tx,
        };

        let params = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "client": {
                "id": "tui",
                "version": VERSION,
            },
            "scopes": scopes,
        });

        let response = client.request("connect", Some(params)).await?;

        let payload: HelloOkPayload = serde_json::from_value(response)?;

        Ok((client, payload))
    }

    /// Send a request and await the matching response.
    ///
    /// `&self`, not `&mut self`: concurrent callers are the point — each
    /// request carries its own id and its own waiter, and the socket is fed by
    /// a channel, so nothing here races.
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, TuiError> {
        let id = format!("tui_{}", Uuid::new_v4());
        let request = ClientRequest {
            frame_type: "req",
            id: id.clone(),
            method: method.to_string(),
            params,
        };

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.insert(id.clone(), tx);
        }

        let text = serde_json::to_string(&request)?;
        self.write_tx
            .send(Message::Text(text))
            .map_err(|_| TuiError::WebSocket("send channel closed".to_string()))?;

        let response = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => response,
            // The driver drained the waiters on its way out, which is how a
            // request learns the connection is gone.
            Ok(Err(_)) => {
                return Err(TuiError::WebSocket(
                    "the gateway connection was lost before it answered".to_string(),
                ))
            }
            Err(_) => {
                // Drop the waiter so a late response cannot resolve a request
                // the caller has already abandoned.
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                return Err(TuiError::Timeout(method.to_string()));
            }
        };

        if response.ok {
            Ok(response.payload.unwrap_or(Value::Null))
        } else {
            let err = response.error.unwrap_or(ClientError {
                code: "UNKNOWN".to_string(),
                message: "unknown error".to_string(),
            });
            Err(TuiError::Gateway {
                code: err.code,
                message: err.message,
            })
        }
    }

    /// Send a fire-and-forget text message.
    #[allow(dead_code)]
    pub fn send_text(&self, text: String) -> Result<(), TuiError> {
        self.write_tx
            .send(Message::Text(text))
            .map_err(|_| TuiError::WebSocket("send channel closed".to_string()))
    }

    /// Close the connection gracefully.
    #[allow(dead_code)]
    pub fn close(&self) -> Result<(), TuiError> {
        self.write_tx
            .send(Message::Close(None))
            .map_err(|_| TuiError::WebSocket("send channel closed".to_string()))
    }

    /// Request the gateway abort the current run.
    #[allow(dead_code)]
    pub async fn abort(&mut self) -> Result<Value, TuiError> {
        self.request("chat.abort", None).await
    }

    /// Receive the next message, if any.
    pub async fn next(&self) -> Option<WsMessage> {
        self.event_rx.lock().await.recv().await
    }
}

/// The upgrade request for `url`, presenting the token as a Bearer credential.
///
/// A browser cannot set headers on a WebSocket upgrade, which is why the
/// gateway also accepts `?token=`. A Rust client can, and this is one — so the
/// credential does not have to sit in a URL that reaches the access log, the
/// process list and any proxy in between. It is also what stops the TUI
/// tripping the gateway's "token in the upgrade URL" warning.
fn ws_request(
    url: &str,
    auth: &AuthConfig,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, TuiError> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    // `into_client_request` is what `connect_async(&url)` uses internally, and
    // it adds the handshake headers a hand-built request would be missing.
    let mut request = url
        .into_client_request()
        .map_err(|e| TuiError::WebSocket(format!("bad websocket url: {e}")))?;
    if let Some(bearer) = auth.bearer() {
        let value = bearer
            .parse()
            .map_err(|e| TuiError::Auth(format!("invalid token header: {e}")))?;
        request.headers_mut().insert("authorization", value);
    }
    Ok(request)
}

/// Combined read/write WebSocket driver.
async fn ws_driver(
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut write_rx: mpsc::UnboundedReceiver<Message>,
    event_tx: mpsc::UnboundedSender<WsMessage>,
    pending: PendingMap,
    mut stop_rx: mpsc::Receiver<()>,
) {
    let (mut write, mut read) = ws_stream.split();

    loop {
        tokio::select! {
            biased;

            _ = stop_rx.recv() => {
                let _ = write.send(Message::Close(None)).await;
                break;
            }

            Some(msg) = write_rx.recv() => {
                if write.send(msg).await.is_err() {
                    break;
                }
            }

            Some(item) = read.next() => {
                match item {
                    Ok(Message::Text(text)) => {
                        handle_text(&text, &event_tx, &pending);
                    }
                    Ok(Message::Close(_)) | Ok(Message::Frame(_)) => break,
                    Ok(Message::Ping(data)) => {
                        if write.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Pong(_)) => {}
                    Ok(Message::Binary(_)) => {}
                    Err(_) => break,
                }
            }

            else => break,
        }
    }

    // Unblock every in-flight request. The driver is the only thing that can
    // ever resolve one and it is about to stop, so a waiter left in the map
    // sits out its full timeout and is then told the request *timed out* —
    // which is the wrong story, and 15 seconds of a frozen UI to tell it.
    pending.lock().unwrap_or_else(|e| e.into_inner()).clear();

    // Tell the UI the stream is over, so it can show the disconnect and start
    // reconnecting instead of spinning on a silent channel.
    let _ = event_tx.send(WsMessage::Disconnected);
}

/// Parse an incoming text frame and route it to waiters or events.
fn handle_text(text: &str, event_tx: &mpsc::UnboundedSender<WsMessage>, pending: &PendingMap) {
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };

    let frame_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match frame_type {
        "res" => {
            let response: ClientResponse = match serde_json::from_value(value) {
                Ok(r) => r,
                Err(_) => return,
            };

            let waiter = {
                let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
                pending.remove(&response.id)
            };

            if let Some(tx) = waiter {
                let _ = tx.send(response);
            } else {
                let _ = event_tx.send(WsMessage::OrphanResponse(response));
            }
        }
        "event" => {
            let event: ClientEvent = match serde_json::from_value(value) {
                Ok(e) => e,
                Err(_) => return,
            };
            let _ = event_tx.send(WsMessage::Event(event));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_gateway::TestGateway;

    /// A client on the test gateway.
    async fn connect(gateway: &TestGateway) -> WsClient {
        let auth = AuthConfig::None;
        let url = auth.ws_url("127.0.0.1", gateway.port, None, "tui");
        let (client, _hello) = WsClient::connect(&url, &auth, &["chat"])
            .await
            .expect("connect");
        client
    }

    /// Connecting subscribes to nothing of its own.
    ///
    /// The gateway seeds the connection's subscriptions from the `session_id`
    /// in the upgrade URL, and session switches go through
    /// `gateway_calls::sessions_subscribe`. A private subscribe here is a
    /// second source of truth for the same thing — and was, with a bare
    /// `session_id` where the method wants `session_ids`, so every connect
    /// that carried a session id also sent a request the gateway rejected as
    /// invalid. Nothing surfaced the rejection: the reply went to `.ok()`.
    /// The credential travels in a header, not in the URL.
    ///
    /// The TUI used to connect with `?token=…`, which puts the shared secret
    /// in the gateway's access log, in the process list of every hop, and in
    /// any proxy in between. The CLI has always sent a Bearer header; the TUI
    /// can do the same and the gateway accepts both.
    #[tokio::test]
    async fn the_credential_travels_in_a_header() {
        let gateway = TestGateway::start().await;
        let auth = AuthConfig::Token { token: "s3cret".to_string() };
        let url = auth.ws_url("127.0.0.1", gateway.port, None, "tui");
        assert!(!url.contains("s3cret"), "not in the URL: {url}");

        let (_client, hello) = WsClient::connect(&url, &auth, &["chat"])
            .await
            .expect("connect");

        assert_eq!(hello.server.version, "test");
        assert_eq!(gateway.upgrade_header("authorization").as_deref(), Some("Bearer s3cret"));
        // And the handshake's own headers survive, or the gateway refuses the
        // upgrade outright.
        assert!(gateway.upgrade_header("sec-websocket-key").is_some());
    }

    #[tokio::test]
    async fn connecting_subscribes_to_nothing() {
        let gateway = TestGateway::start().await;
        let auth = AuthConfig::None;
        let url = auth.ws_url("127.0.0.1", gateway.port, Some("s1"), "tui");

        let (_client, hello) = WsClient::connect(&url, &auth, &["chat"])
            .await
            .expect("connect");
        assert_eq!(hello.server.version, "test");

        let methods: Vec<String> = gateway.requests().into_iter().map(|r| r.method).collect();
        assert_eq!(methods, vec!["connect"], "the handshake is the only request a connect makes");
    }

    /// A dropped connection fails in-flight requests at once, and says why.
    ///
    /// The driver is the only thing that can resolve a request and it exits
    /// with the socket. Without draining the waiter map, each caller sat out
    /// the full request timeout and was then told the request had *timed out*
    /// — a frozen UI, and the wrong diagnosis for the user to act on.
    #[tokio::test]
    async fn a_lost_connection_fails_in_flight_requests_at_once() {
        let gateway = TestGateway::start().await;
        let auth = AuthConfig::None;
        let url = auth.ws_url("127.0.0.1", gateway.port, None, "tui");
        let (mut client, _) = WsClient::connect(&url, &auth, &["chat"])
            .await
            .expect("connect");

        // A request the gateway accepts and then never answers.
        gateway.go_silent();
        let waiting = tokio::spawn(async move { client.request("system.presence", None).await });
        gateway
            .wait_for("system.presence", std::time::Duration::from_secs(5))
            .await;

        let started = std::time::Instant::now();
        gateway.close().await;
        let err = waiting
            .await
            .expect("join")
            .expect_err("nothing can answer");
        let elapsed = started.elapsed();

        assert!(matches!(err, TuiError::WebSocket(_)), "the connection is what failed: {err:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "it must not sit out the {}s request timeout: took {elapsed:?}",
            REQUEST_TIMEOUT.as_secs()
        );
    }

    /// One client serves concurrent callers.
    ///
    /// `request` took `&mut self`, which is what forced every call to happen
    /// inside the event loop: the client could not be in two places at once,
    /// so a command holding it froze the loop. Each request carries its own id
    /// and its own waiter and the socket is fed by a channel, so it can.
    #[tokio::test]
    async fn the_client_serves_concurrent_requests() {
        let gateway = TestGateway::start().await;
        let client = Arc::new(connect(&gateway).await);

        // Park one request on the wire...
        gateway.hold("sessions.list");
        let parked = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.request("sessions.list", None).await })
        };
        gateway
            .wait_for("sessions.list", std::time::Duration::from_secs(5))
            .await;

        // ...and a second still goes out and comes back behind it.
        let answered = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.request("agents.registry", None),
        )
        .await;
        assert!(
            answered.is_ok(),
            "the second request queued behind the first instead of running alongside it"
        );

        parked.abort();
    }

    #[test]
    fn request_serializes() {
        let req = ClientRequest {
            frame_type: "req",
            id: "id".to_string(),
            method: "chat.send".to_string(),
            params: Some(serde_json::json!({ "message": "hi" })),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"type\":\"req\""));
        assert!(s.contains("\"method\":\"chat.send\""));
    }

    #[test]
    fn response_deserializes() {
        let text = r#"{"type":"res","id":"id","ok":true,"payload":{"key":"val"}}"#;
        let resp: ClientResponse = serde_json::from_str(text).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.id, "id");
    }

    #[test]
    fn event_deserializes() {
        let text = r#"{"type":"event","event":"chat.delta","payload":{"content":"hi"},"seq":1}"#;
        let evt: ClientEvent = serde_json::from_str(text).unwrap();
        assert_eq!(evt.event, "chat.delta");
    }
}
