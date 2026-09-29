//! WebChat Channel Implementation
//!
//! This module implements the Channel trait for a browser-based web chat
//! interface. It provides a WebSocket endpoint for real-time messaging and
//! serves a lightweight HTML/JS frontend.
//!
//! Features:
//! - WebSocket for bidirectional real-time messaging
//! - Auto-generated session IDs per browser tab
//! - Markdown rendering on the frontend
//! - Typing indicators
//! - Message history per session

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};

use crate::channels::{
    Channel, ChannelCapabilities, ChatType, ConversationId, FormattedContent, IncomingMessage,
    OutgoingMessage,
};
use crate::core::models::Id;
use crate::security::pairing::{DmPolicy, PairingStore, RequestAccessResult};

/// Default WebSocket port for WebChat
const DEFAULT_WEBCHAT_PORT: u16 = 8081;

/// WebChat channel configuration
#[derive(Debug, Clone)]
pub struct WebchatConfig {
    /// HTTP server bind address
    pub bind_addr: String,
    /// Optional allowed session IDs (empty = allow all)
    pub allowed_sessions: Vec<String>,
    /// Message handler for incoming messages
    pub message_tx: Option<mpsc::UnboundedSender<IncomingMessage>>,
    /// Custom HTML page title
    pub page_title: String,
}

impl WebchatConfig {
    /// Create new config with default bind address
    pub fn new() -> Self {
        Self {
            bind_addr: format!("0.0.0.0:{}", DEFAULT_WEBCHAT_PORT),
            allowed_sessions: Vec::new(),
            message_tx: None,
            page_title: "Syscity Chat".to_string(),
        }
    }

    /// Set custom bind address
    pub fn with_bind_addr(mut self, addr: impl Into<String>) -> Self {
        self.bind_addr = addr.into();
        self
    }

    /// Set allowed sessions
    pub fn allow_sessions(mut self, sessions: Vec<String>) -> Self {
        self.allowed_sessions = sessions;
        self
    }

    /// Set message handler
    pub fn with_message_handler(mut self, tx: mpsc::UnboundedSender<IncomingMessage>) -> Self {
        self.message_tx = Some(tx);
        self
    }

    /// Set page title
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.page_title = title.into();
        self
    }
}

impl Default for WebchatConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// WebSocket message protocol
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WebchatMessage {
    /// Client sends a chat message
    Chat { session_id: String, content: String },
    /// Server sends a response
    Response {
        session_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        html_content: Option<String>,
    },
    /// Typing indicator
    Typing { session_id: String, is_typing: bool },
    /// System notification
    System { session_id: String, message: String },
    /// Client ready / handshake
    Ready { session_id: String },
}

/// Active WebSocket connection info
#[derive(Debug)]
struct WebchatConnection {
    sender: mpsc::UnboundedSender<String>,
}

/// WebChat channel implementation
pub struct WebchatChannel {
    config: WebchatConfig,
    running: Arc<std::sync::atomic::AtomicBool>,
    /// Active connections by session ID
    connections: Arc<RwLock<HashMap<String, WebchatConnection>>>,
    /// Track message IDs for edit/delete
    message_map: Arc<RwLock<HashMap<String, String>>>,
    /// Pairing store
    pairing_store: Arc<RwLock<Option<Arc<PairingStore>>>>,
    /// DM policy
    dm_policy: Arc<RwLock<DmPolicy>>,
    /// Allowlist
    allow_from: Arc<RwLock<Vec<String>>>,
    /// Message sender for routing to agent
    message_tx: Arc<RwLock<Option<mpsc::UnboundedSender<IncomingMessage>>>>,
    /// Server handle for shutdown
    server_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl std::fmt::Debug for WebchatChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebchatChannel")
            .field("config", &self.config)
            .field("running", &self.running)
            .finish()
    }
}

impl WebchatChannel {
    /// Create a new WebChat channel
    pub fn new(config: WebchatConfig) -> Self {
        Self {
            config,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            connections: Arc::new(RwLock::new(HashMap::new())),
            message_map: Arc::new(RwLock::new(HashMap::new())),
            pairing_store: Arc::new(RwLock::new(None)),
            dm_policy: Arc::new(RwLock::new(DmPolicy::Open)),
            allow_from: Arc::new(RwLock::new(Vec::new())),
            message_tx: Arc::new(RwLock::new(None)),
            server_handle: Arc::new(RwLock::new(None)),
        }
    }

    /// Set pairing store
    pub async fn set_pairing_store(&self, store: Arc<PairingStore>) {
        let mut s = self.pairing_store.write().await;
        *s = Some(store);
    }

    /// Set DM policy
    pub async fn set_dm_policy(&self, policy: DmPolicy) {
        let mut p = self.dm_policy.write().await;
        *p = policy;
    }

    /// Set allowlist
    pub async fn set_allow_from(&self, sessions: Vec<String>) {
        let mut a = self.allow_from.write().await;
        *a = sessions;
    }

    /// Set message sender
    pub async fn set_message_sender(&self, sender: mpsc::UnboundedSender<IncomingMessage>) {
        let mut tx = self.message_tx.write().await;
        *tx = Some(sender);
    }

    /// Check if session is authorized
    pub async fn check_access(&self, session_id: &str) -> (bool, Option<String>) {
        let policy = *self.dm_policy.read().await;
        match policy {
            DmPolicy::Open => (true, None),
            DmPolicy::Allowlist => {
                let allow_from = self.allow_from.read().await;
                if allow_from.contains(&session_id.to_string()) {
                    (true, None)
                } else {
                    (false, Some("Session not authorized.".to_string()))
                }
            }
            DmPolicy::Pairing => {
                let store_guard = self.pairing_store.read().await;
                if let Some(store) = store_guard.as_ref() {
                    match store
                        .request_access("webchat", session_id, Some(session_id))
                        .await
                    {
                        Ok(RequestAccessResult::AlreadyAuthorized) => (true, None),
                        Ok(RequestAccessResult::AlreadyPending { code, .. }) => (
                            false,
                            Some(format!("Access pending approval. Pairing code: `{}`", code)),
                        ),
                        Ok(RequestAccessResult::NewRequest { code }) => {
                            (false, Some(format!("Access requested. Pairing code: `{}`", code)))
                        }
                        Ok(RequestAccessResult::RateLimited { .. }) => {
                            (false, Some("Too many requests.".to_string()))
                        }
                        Err(_) => (false, Some("Access check error.".to_string())),
                    }
                } else {
                    (false, Some("Access control not configured.".to_string()))
                }
            }
        }
    }

    /// Send a message to a specific WebSocket session
    pub async fn send_to_session(&self, session_id: &str, content: &str) -> crate::Result<()> {
        let connections = self.connections.read().await;
        if let Some(conn) = connections.get(session_id) {
            let msg = WebchatMessage::Response {
                session_id: session_id.to_string(),
                content: content.to_string(),
                html_content: None,
            };
            let json = serde_json::to_string(&msg).unwrap_or_default();
            if conn.sender.send(json).is_err() {
                warn!("WebChat message send failed: receiver closed");
            }
            Ok(())
        } else {
            Err(crate::error::SyscityError::NotFound {
                resource: format!("WebChat session {} not connected", session_id),
            })
        }
    }

    /// Broadcast a message to all connected sessions
    pub async fn broadcast(&self, content: &str) -> crate::Result<()> {
        let connections = self.connections.read().await;
        for (session_id, conn) in connections.iter() {
            let msg = WebchatMessage::Response {
                session_id: session_id.clone(),
                content: content.to_string(),
                html_content: None,
            };
            let json = serde_json::to_string(&msg).unwrap_or_default();
            if conn.sender.send(json).is_err() {
                warn!("WebChat broadcast send failed: receiver closed");
            }
        }
        Ok(())
    }

    /// Get the HTML frontend page
    fn chat_html(&self) -> String {
        let title = &self.config.page_title;
        format!(
            r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>{title}</title>
    <style>
        * {{ margin: 0; padding: 0; box-sizing: border-box; }}
        body {{
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
            background: #1a1a2e;
            color: #eee;
            height: 100vh;
            display: flex;
            flex-direction: column;
        }}
        .header {{
            background: #16213e;
            padding: 1rem;
            border-bottom: 1px solid #0f3460;
        }}
        .header h1 {{ font-size: 1.2rem; color: #e94560; }}
        .chat-container {{
            flex: 1;
            overflow-y: auto;
            padding: 1rem;
            display: flex;
            flex-direction: column;
            gap: 0.5rem;
        }}
        .message {{
            max-width: 80%;
            padding: 0.75rem 1rem;
            border-radius: 1rem;
            word-wrap: break-word;
        }}
        .message.user {{
            align-self: flex-end;
            background: #e94560;
            color: white;
            border-bottom-right-radius: 0.25rem;
        }}
        .message.bot {{
            align-self: flex-start;
            background: #16213e;
            border: 1px solid #0f3460;
            border-bottom-left-radius: 0.25rem;
        }}
        .message.system {{
            align-self: center;
            background: #0f3460;
            font-size: 0.85rem;
            color: #aaa;
        }}
        .input-container {{
            display: flex;
            padding: 1rem;
            gap: 0.5rem;
            background: #16213e;
            border-top: 1px solid #0f3460;
        }}
        .input-container input {{
            flex: 1;
            padding: 0.75rem 1rem;
            border: 1px solid #0f3460;
            border-radius: 1.5rem;
            background: #1a1a2e;
            color: #eee;
            font-size: 1rem;
        }}
        .input-container input:focus {{
            outline: none;
            border-color: #e94560;
        }}
        .input-container button {{
            padding: 0.75rem 1.5rem;
            border: none;
            border-radius: 1.5rem;
            background: #e94560;
            color: white;
            font-size: 1rem;
            cursor: pointer;
        }}
        .input-container button:hover {{
            background: #c73e54;
        }}
        .typing {{
            align-self: flex-start;
            color: #888;
            font-style: italic;
            font-size: 0.9rem;
            padding: 0.5rem 1rem;
        }}
        code {{
            background: rgba(0,0,0,0.3);
            padding: 0.2rem 0.4rem;
            border-radius: 0.25rem;
            font-family: 'Courier New', monospace;
        }}
        pre {{
            background: rgba(0,0,0,0.3);
            padding: 0.75rem;
            border-radius: 0.5rem;
            overflow-x: auto;
            margin: 0.5rem 0;
        }}
        pre code {{ padding: 0; background: none; }}
    </style>
</head>
<body>
    <div class="header">
        <h1>{title}</h1>
    </div>
    <div class="chat-container" id="chat">
        <div class="message system">Connected. Type a message to start chatting.</div>
    </div>
    <div class="typing" id="typing" style="display:none">Bot is typing...</div>
    <div class="input-container">
        <input type="text" id="input" placeholder="Type a message..." autocomplete="off">
        <button onclick="send()">Send</button>
    </div>
    <script>
        const sessionId = localStorage.getItem('syscity_session') || crypto.randomUUID();
        localStorage.setItem('syscity_session', sessionId);
        const chat = document.getElementById('chat');
        const input = document.getElementById('input');
        const typing = document.getElementById('typing');
        let ws;

        function connect() {{
            const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
            ws = new WebSocket(`${{protocol}}//${{window.location.host}}/ws`);
            ws.onopen = () => {{
                ws.send(JSON.stringify({{type:'ready',session_id:sessionId}}));
            }};
            ws.onmessage = (e) => {{
                const msg = JSON.parse(e.data);
                if (msg.type === 'response') addMessage(msg.content, 'bot');
                else if (msg.type === 'typing') typing.style.display = msg.is_typing ? 'block' : 'none';
                else if (msg.type === 'system') addMessage(msg.message, 'system');
            }};
            ws.onclose = () => setTimeout(connect, 3000);
        }}

        function addMessage(text, cls) {{
            const div = document.createElement('div');
            div.className = 'message ' + cls;
            div.innerHTML = markdownToHtml(text);
            chat.appendChild(div);
            chat.scrollTop = chat.scrollHeight;
        }}

        function markdownToHtml(md) {{
            return md
                .replace(/\*\*(.+?)\*\*/g, '<b>$1</b>')
                .replace(/\*(.+?)\*/g, '<i>$1</i>')
                .replace(/`(.+?)`/g, '<code>$1</code>')
                .replace(/```(\w+)?\n([\s\S]+?)```/g, '<pre><code>$2</code></pre>')
                .replace(/\n/g, '<br>');
        }}

        function send() {{
            const text = input.value.trim();
            if (!text || !ws || ws.readyState !== WebSocket.OPEN) return;
            addMessage(text, 'user');
            ws.send(JSON.stringify({{type:'chat',session_id:sessionId,content:text}}));
            input.value = '';
        }}

        input.addEventListener('keypress', (e) => {{ if (e.key === 'Enter') send(); }});
        connect();
    </script>
</body>
</html>
"#
        )
    }
}

#[async_trait]
impl Channel for WebchatChannel {
    fn name(&self) -> &str {
        "webchat"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            chat_types: vec![ChatType::Direct],
            supports_formatting: true,
            supports_attachments: false, // Could be added with file upload
            supports_images: true,       // Inline images via markdown
            supports_threads: false,
            supports_typing: true,
            supports_buttons: false,
            supports_commands: false,
            supports_reactions: false,
            supports_edit: false,
            supports_unsend: false,
            supports_effects: false,
        }
    }

    async fn start(&self) -> crate::Result<()> {
        info!("Starting WebChat channel on {}", self.config.bind_addr);

        self.running
            .store(true, std::sync::atomic::Ordering::SeqCst);

        // Build axum router
        let html = self.chat_html();
        let connections = self.connections.clone();
        let message_tx = self.message_tx.clone();
        let running = self.running.clone();

        let app = axum::Router::new()
            .route(
                "/",
                axum::routing::get(move || {
                    let html = html.clone();
                    async move { axum::response::Html(html) }
                }),
            )
            .route(
                "/ws",
                axum::routing::get(move |ws: axum::extract::WebSocketUpgrade| {
                    let connections = connections.clone();
                    let message_tx = message_tx.clone();
                    let running = running.clone();
                    async move {
                        ws.on_upgrade(move |socket| {
                            handle_websocket(socket, connections, message_tx, running)
                        })
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind(&self.config.bind_addr)
            .await
            .map_err(crate::error::SyscityError::Io)?;

        let server = axum::serve(listener, app);

        let handle = tokio::spawn(async move {
            let _ = server.await;
        });

        {
            let mut h = self.server_handle.write().await;
            *h = Some(handle);
        }

        info!("WebChat channel started at http://{}", self.config.bind_addr);
        Ok(())
    }

    async fn stop(&self) -> crate::Result<()> {
        info!("Stopping WebChat channel...");
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);

        // Close all connections
        let mut conns = self.connections.write().await;
        conns.clear();

        // Abort server
        if let Some(handle) = self.server_handle.write().await.take() {
            handle.abort();
        }

        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        let session_id = &message.conversation_id.0;
        let content = match &message.formatted_content {
            Some(FormattedContent::Markdown(md)) => md.clone(),
            Some(FormattedContent::Html(html)) => html.clone(),
            _ => message.content,
        };

        self.send_to_session(session_id, &content).await?;

        let msg_id = Id::new();
        let mut map = self.message_map.write().await;
        map.insert(msg_id.to_string(), session_id.to_string());

        Ok(msg_id)
    }

    async fn send_typing(&self, conversation_id: &ConversationId) -> crate::Result<()> {
        let session_id = &conversation_id.0;
        let connections = self.connections.read().await;
        if let Some(conn) = connections.get(session_id) {
            let msg = WebchatMessage::Typing {
                session_id: session_id.to_string(),
                is_typing: true,
            };
            let json = serde_json::to_string(&msg).unwrap_or_default();
            if conn.sender.send(json).is_err() {
                warn!("WebChat typing send failed: receiver closed");
            }
        }
        Ok(())
    }

    async fn edit_message(&self, _message_id: Id, _new_content: String) -> crate::Result<()> {
        Err(crate::error::SyscityError::Internal("WebChat edit not implemented".to_string()))
    }

    async fn delete_message(&self, _message_id: Id) -> crate::Result<()> {
        Err(crate::error::SyscityError::Internal(
            "WebChat delete not implemented".to_string(),
        ))
    }

    async fn health_check(&self) -> crate::Result<bool> {
        Ok(self.running.load(std::sync::atomic::Ordering::SeqCst))
    }
}

/// Handle a WebSocket connection
async fn handle_websocket(
    mut socket: axum::extract::ws::WebSocket,
    connections: Arc<RwLock<HashMap<String, WebchatConnection>>>,
    message_tx: Arc<RwLock<Option<mpsc::UnboundedSender<IncomingMessage>>>>,
    _running: Arc<std::sync::atomic::AtomicBool>,
) {
    use axum::extract::ws::Message;

    let session_id = uuid::Uuid::new_v4().to_string();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<String>();

    // Register this connection for outbound messages
    {
        let mut conns = connections.write().await;
        conns.insert(session_id.clone(), WebchatConnection { sender: outbound_tx });
    }

    // Send a Ready message to acknowledge the connection
    if let Ok(json) =
        serde_json::to_string(&WebchatMessage::Ready { session_id: session_id.clone() })
    {
        if socket.send(Message::Text(json)).await.is_err() {
            warn!("WebChat: failed to send Ready message to session {}", session_id);
        }
    }

    // Main loop: handle incoming WS messages and outbound channel messages
    loop {
        tokio::select! {
            // Outbound: forward messages from the channel to this WebSocket
            msg = outbound_rx.recv() => {
                match msg {
                    Some(text) => {
                        if socket.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            // Inbound: read messages from WebSocket and route to agent
            ws_msg = socket.recv() => {
                match ws_msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(WebchatMessage::Chat { content, .. }) =
                            serde_json::from_str(&text)
                        {
                            if !content.trim().is_empty() {
                                let tx_guard = message_tx.read().await;
                                if let Some(ref tx) = *tx_guard {
                                    let msg = IncomingMessage::new(
                                        &session_id,
                                        &session_id,
                                        content,
                                    )
                                    .with_provenance(
                                        crate::channels::InputProvenance::ExternalUser {
                                            channel: "webchat".to_string(),
                                            is_direct: true,
                                        },
                                    );
                                    if tx.send(msg).is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        warn!("WebChat WS recv error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    // Clean up on disconnect
    let mut conns = connections.write().await;
    conns.remove(&session_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_webchat_config() {
        let config = WebchatConfig::new()
            .with_bind_addr("127.0.0.1:9000")
            .with_title("Test Chat")
            .allow_sessions(vec!["session1".to_string()]);

        assert_eq!(config.bind_addr, "127.0.0.1:9000");
        assert_eq!(config.page_title, "Test Chat");
        assert_eq!(config.allowed_sessions.len(), 1);
    }

    #[test]
    fn test_webchat_config_default() {
        let config = WebchatConfig::default();
        assert_eq!(config.bind_addr, format!("0.0.0.0:{}", DEFAULT_WEBCHAT_PORT));
        assert_eq!(config.page_title, "Syscity Chat");
    }

    #[test]
    fn test_webchat_capabilities() {
        let config = WebchatConfig::new();
        let channel = WebchatChannel::new(config);
        let caps = channel.capabilities();
        assert!(caps.supports_formatting);
        assert!(caps.supports_typing);
        assert!(caps.supports_images);
        assert!(!caps.supports_edit);
    }

    #[test]
    fn test_chat_html_contains_title() {
        let config = WebchatConfig::new().with_title("My Bot");
        let channel = WebchatChannel::new(config);
        let html = channel.chat_html();
        assert!(html.contains("My Bot"));
        assert!(html.contains("WebSocket"));
        assert!(html.contains("chat-container"));
    }

    #[test]
    fn test_webchat_message_serde() {
        let msg = WebchatMessage::Chat {
            session_id: "s1".to_string(),
            content: "hello".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"chat\""));
        assert!(json.contains("\"session_id\":\"s1\""));
    }
}
