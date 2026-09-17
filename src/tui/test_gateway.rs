//! A minimal in-process gateway for line-mode tests.
//!
//! Line mode talks to a real socket, so testing it needs one. Booting the real
//! gateway would drag in storage, providers and a port; instead this speaks
//! just enough of the protocol — it completes the handshake, answers the
//! handful of methods line mode calls, and hands the test both the frames the
//! client sent and a way to push events back at it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot};
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::Message;

/// One request the client sent.
#[derive(Debug, Clone)]
pub struct Received {
    /// Method name.
    pub method: String,
    /// Parameters, verbatim.
    pub params: Value,
}

/// What every connection to the same stand-in gateway shares.
///
/// Separate from [`TestGateway`] because the serving tasks outlive any one
/// connection: a client that reconnects gets a new socket onto the same
/// gateway, with the same scripted history and the same refusals.
#[derive(Clone)]
struct Shared {
    requests: Arc<Mutex<Vec<Received>>>,
    /// Events go to whichever connection is live. A broadcast rather than a
    /// queue: connections come and go, and a receiver held for one
    /// connection's lifetime would lock out the next one — which is exactly
    /// what a reconnect test does.
    events: broadcast::Sender<Message>,
    close: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    silent: Arc<std::sync::atomic::AtomicBool>,
    failures: Arc<Mutex<HashMap<String, String>>>,
    upgrade_headers: Arc<Mutex<Vec<(String, String)>>>,
    history: Arc<Mutex<Vec<Value>>>,
    held: Arc<Mutex<Vec<String>>>,
}

/// A stand-in gateway, listening on a loopback port.
pub struct TestGateway {
    /// The port the client should connect to.
    pub port: u16,
    shared: Shared,
    /// Push events at whichever connection is live.
    events: broadcast::Sender<Message>,
}

impl TestGateway {
    /// Bind a port and start serving clients.
    ///
    /// Connections are served one after another, so a test can drop the socket
    /// and watch the client come back to the same gateway.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
        let port = listener.local_addr().expect("local addr").port();
        let (event_tx, _) = broadcast::channel::<Message>(64);
        let shared = Shared {
            requests: Arc::new(Mutex::new(Vec::new())),
            events: event_tx.clone(),
            close: Arc::new(Mutex::new(None)),
            silent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failures: Arc::new(Mutex::new(HashMap::new())),
            upgrade_headers: Arc::new(Mutex::new(Vec::new())),
            history: Arc::new(Mutex::new(Vec::new())),
            held: Arc::new(Mutex::new(Vec::new())),
        };

        let serving = shared.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (close_tx, close_rx) = oneshot::channel::<()>();
                *serving.close.lock().expect("close slot") = Some(close_tx);
                tokio::spawn(serve(stream, serving.clone(), close_rx));
            }
        });

        Self { port, shared, events: event_tx }
    }
    /// Take `method`: accept it, record it, and never answer it.
    ///
    /// For a test that needs the client parked mid-call — the difference
    /// between "slow" and "never" is what makes a lock held across a request
    /// observable instead of a race.
    pub fn hold(&self, method: &str) {
        self.shared
            .held
            .lock()
            .expect("held methods")
            .push(method.to_string());
    }

    /// Serve these messages from `chat.history`, oldest first.
    pub fn with_history(&self, messages: Vec<Value>) {
        *self.shared.history.lock().expect("history") = messages;
    }

    /// One header of the WebSocket upgrade request, lower-cased.
    pub fn upgrade_header(&self, name: &str) -> Option<String> {
        self.shared
            .upgrade_headers
            .lock()
            .expect("upgrade headers")
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    }

    /// Record requests from here on, but never answer them.
    pub fn go_silent(&self) {
        self.shared
            .silent
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Make `method` answer with an error carrying `code`.
    pub fn fail_with(&self, method: &str, code: &str) {
        self.shared
            .failures
            .lock()
            .expect("failure table")
            .insert(method.to_string(), code.to_string());
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<Received> {
        self.shared.requests.lock().expect("request log").clone()
    }

    /// Wait for the first request named `method`, and return its parameters.
    ///
    /// Panics on timeout: a test that waits for a frame the client never sent
    /// has already failed, and saying which frame makes that readable.
    pub async fn wait_for(&self, method: &str, timeout: Duration) -> Value {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let found = {
                let requests = self.shared.requests.lock().expect("request log");
                requests
                    .iter()
                    .find(|r| r.method == method)
                    .map(|r| r.params.clone())
            };
            if let Some(params) = found {
                return params;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the client never sent {method}; it sent {:?}",
                self.requests()
                    .into_iter()
                    .map(|r| r.method)
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Push a server event at the client.
    pub fn push_event(&self, event: &str, payload: Value) {
        let frame = json!({ "type": "event", "event": event, "payload": payload, "seq": 1 });
        // Nobody listening is not an error: a test may push before the client
        // has connected, or between connections.
        let _ = self.events.send(Message::Text(frame.to_string()));
    }

    /// Drop the connection, the way a gateway restart would.
    pub async fn close(&self) {
        if let Some(tx) = self.shared.close.lock().expect("close slot").take() {
            let _ = tx.send(());
        }
        // Give the serving task a moment to drop the socket.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Serve one connection until the test closes it or the client leaves.
async fn serve(stream: tokio::net::TcpStream, shared: Shared, mut close_rx: oneshot::Receiver<()>) {
    let record_headers = {
        let sink = Arc::clone(&shared.upgrade_headers);
        move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
            let mut sink = sink.lock().expect("upgrade headers");
            for (name, value) in req.headers() {
                sink.push((
                    name.as_str().to_lowercase(),
                    value.to_str().unwrap_or_default().to_string(),
                ));
            }
            Ok(resp)
        }
    };
    let Ok(mut socket) = accept_hdr_async(stream, record_headers).await else {
        return;
    };

    let mut events = shared.events.subscribe();
    loop {
        tokio::select! {
            _ = &mut close_rx => break,
            Ok(message) = events.recv() => {
                if socket.send(message).await.is_err() {
                    break;
                }
            }
            incoming = socket.next() => {
                let Some(Ok(Message::Text(text))) = incoming else {
                    break;
                };
                let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let method = frame["method"].as_str().unwrap_or_default().to_string();
                let params = frame["params"].clone();
                shared
                    .requests
                    .lock()
                    .expect("request log")
                    .push(Received {
                        method: method.clone(),
                        params: params.clone(),
                    });
                let held = shared.held.lock().expect("held methods").contains(&method);
                if shared.silent.load(std::sync::atomic::Ordering::SeqCst) || held {
                    // Recorded, deliberately unanswered: a request left in
                    // flight.
                    continue;
                }
                let failure = shared
                    .failures
                    .lock()
                    .expect("failure table")
                    .get(&method)
                    .cloned();
                let reply = match failure {
                    Some(code) => json!({
                        "type": "res",
                        "id": frame["id"].clone(),
                        "ok": false,
                        "error": { "code": code, "message": format!("{method} refused") },
                    }),
                    None => json!({
                        "type": "res",
                        "id": frame["id"].clone(),
                        "ok": true,
                        "payload": payload_for(&method, &params, &shared.history),
                    }),
                };
                if socket.send(Message::Text(reply.to_string())).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// The history reply, following the same contract as the real store: the
/// newest `limit` messages strictly older than `before`, in reading order.
fn history_page(stored: &Arc<Mutex<Vec<Value>>>, params: &Value, limit: usize) -> Value {
    let before = params["before"].as_i64();
    let mut eligible: Vec<Value> = stored
        .lock()
        .expect("history")
        .iter()
        .filter(|m| before.is_none_or(|b| m["timestamp"].as_i64().unwrap_or(0) < b))
        .cloned()
        .collect();
    let has_more = eligible.len() > limit;
    if has_more {
        let excess = eligible.len() - limit;
        eligible.drain(..excess);
    }
    json!({
        "session_id": params["session_id"].clone(),
        "messages": eligible,
        "has_more": has_more,
    })
}

/// The reply for a method — only the shapes the client parses.
fn payload_for(method: &str, params: &Value, stored: &Arc<Mutex<Vec<Value>>>) -> Value {
    match method {
        "chat.history" => {
            history_page(stored, params, params["limit"].as_u64().unwrap_or(100) as usize)
        }
        // The announcement carries no arguments, so the client asks for them;
        // echo the id back so a decision goes to the approval it was shown for.
        "approvals.get" => json!({
            "id": params["id"],
            "tool_name": "file_write",
            "requested_by": "secretary",
            "risk_level": "High",
            "message": "writes outside the workspace",
            "args": { "path": "/etc/hosts" },
        }),
        _ => canned(method),
    }
}

/// The reply for a method — only the shapes the client parses.
fn canned(method: &str) -> Value {
    match method {
        "connect" => json!({
            "protocol_version": 1,
            "session_key": "test",
            "features": [],
            "scopes_granted": ["chat", "read", "write"],
            "server": { "version": "test", "conn_id": "conn-1" },
        }),
        "sessions.create" => json!({ "session_id": "s1" }),
        "sessions.list" => json!({ "sessions": [] }),
        "chat.send" => json!({ "session_id": "s1" }),
        "chat.history" => json!({ "messages": [], "has_more": false }),
        "commands.list" => json!({ "commands": [] }),
        _ => json!({}),
    }
}
