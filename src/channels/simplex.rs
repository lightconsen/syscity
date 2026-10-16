//! SimpleX Chat channel.
//!
//! The fourth connection channel, and the one with the least protocol of its
//! own: it speaks to a local `simplex-chat` daemon over a WebSocket, and what
//! travels on that socket is SimpleX's *own* command language wrapped in JSON —
//! a command string out, events in. As with the others, the agent-facing half is
//! shared: inbound goes through
//! [`dispatch_channel_message_to`](crate::gateway::webhooks::dispatch_channel_message_to).
//!
//! Four things shape it:
//!
//! - **There is no handshake and no subscription.** Connect and events flow. A
//!   contact asking to connect arrives as an event of its own and has to be
//!   answered with a command, or nobody can ever talk to the bot.
//! - **A command is a string, not a structure.** The payload is
//!   `{"corrId": …, "cmd": "/_send @<id> json […]"}` — SimpleX's CLI syntax, with
//!   the message JSON-encoded *inside* the command string.
//! - **Addressing is by id, never by name.** `@<contactId>` and `#<groupId>`;
//!   the display-name forms silently drop when the name does not resolve, so
//!   they are not used.
//! - **Silence is normal.** The daemon is legitimately application-quiet for
//!   long stretches, so idle time is not evidence of a dead socket — the
//!   keepalive is the WebSocket's own ping, and a connection is only rebuilt
//!   when the socket actually closes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::channels::{
    split_message, Channel, ChannelCapabilities, ChatType, ConversationId, IncomingMessage,
    InputProvenance, MentionState, OutgoingMessage, PlainTextFormatter,
};
use crate::core::models::Id;
use crate::error::SyscityError;

/// Where the daemon listens by default.
pub const DEFAULT_WS_URL: &str = "ws://127.0.0.1:5225";

/// SimpleX has no hard message limit; this is a sanity bound, as the reference
/// has it.
pub const MAX_SIMPLEX_CHARS: usize = 8_000;

/// How long the socket may be quiet before it is pinged.
///
/// The daemon is application-silent for long stretches, so this is the
/// WebSocket's own ping rather than anything SimpleX-defined.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);

/// How many of this process's own correlation ids are remembered, so the
/// daemon's echoes of them are not mistaken for events.
const CORR_ID_MEMORY: usize = 200;

/// Backoff between reconnects, doubling from the first to the last.
const BACKOFF_SECS: [u64; 6] = [2, 4, 8, 16, 32, 60];

/// The `chatInfo.type` of a conversation.
const CHAT_DIRECT: &str = "direct";
const CHAT_GROUP: &str = "group";

/// The content type that carries something a person said.
const CONTENT_RECEIVED: &str = "rcvMsgContent";

/// The directions that mean the message is *ours*.
const DIRECTIONS_SENT: &[&str] = &["directSnd", "groupSnd"];

/// The message types whose `text` field is worth reading.
const TEXT_BEARING: &[&str] = &["text", "file", "image", "voice", "link", "video"];

/// SimpleX channel configuration.
#[derive(Clone)]
pub struct SimplexConfig {
    /// The daemon's WebSocket endpoint.
    pub ws_url: String,
    /// Whether to accept contact requests as they arrive.
    ///
    /// On by default, as the reference has it — but a bot that accepts everyone
    /// is only reasonable because the gateway's own access control runs after
    /// this, on every message.
    pub auto_accept: bool,
    /// Where extracted messages go.
    pub message_tx: Option<mpsc::UnboundedSender<IncomingMessage>>,
}

impl std::fmt::Debug for SimplexConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimplexConfig")
            .field("ws_url", &self.ws_url)
            .field("auto_accept", &self.auto_accept)
            .field("message_tx", &self.message_tx.is_some())
            .finish()
    }
}

impl SimplexConfig {
    /// Configuration for a daemon on the given endpoint.
    pub fn new(ws_url: impl Into<String>) -> Self {
        Self {
            ws_url: ws_url.into(),
            auto_accept: true,
            message_tx: None,
        }
    }
}

/// A message extracted from a chat item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimplexIncoming {
    /// The sender: their contact id, or their member id in a group.
    pub user_id: String,
    /// Where a reply goes — a contact id, or `group:<id>`.
    pub conversation_id: String,
    /// Whether it was said in a group.
    pub is_group: bool,
    /// The text.
    pub text: String,
}

/// What an event asks of this channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimplexEvent {
    /// One or more messages to hand on.
    Messages(Vec<SimplexIncoming>),
    /// Somebody asking to connect, which has to be answered or they never can.
    AcceptContact {
        /// The request the daemon wants answered.
        request_id: String,
    },
    /// Anything this channel does not act on.
    Ignored,
}

/// Read a string field, tolerating the daemon sending an id as a number.
fn field<'a>(value: &'a serde_json::Value, path: &[&str]) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for key in path {
        current = current.get(key)?;
    }
    if current.is_null() {
        None
    } else {
        Some(current)
    }
}

/// A protocol identifier, which the daemon may send as a number or a string.
fn identifier(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// A string field at `path`, or empty.
fn text_at(value: &serde_json::Value, path: &[&str]) -> String {
    field(value, path).map(identifier).unwrap_or_default()
}

/// Read an event from the daemon.
///
/// The envelope is usually `{"corrId": …, "resp": {…}}`, but some daemon
/// versions put the response fields at the top level, so both are read.
pub fn parse_event(raw: &str) -> crate::Result<SimplexEvent> {
    let event: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| SyscityError::Validation(format!("Invalid SimpleX event: {}", e)))?;

    // The response is nested under `resp` when the daemon sends one, and at the
    // top level when it does not.
    let body = field(&event, &["resp"]).unwrap_or(&event);
    let kind = text_at(&event, &["type"]);
    let kind = if kind.is_empty() {
        text_at(body, &["type"])
    } else {
        kind
    };

    match kind.as_str() {
        "contactRequest" => {
            let request =
                field(&event, &["contactRequest"]).or_else(|| field(body, &["contactRequest"]));
            let Some(request) = request else {
                return Ok(SimplexEvent::Ignored);
            };
            Ok(SimplexEvent::AcceptContact {
                request_id: text_at(request, &["contactRequestId"]),
            })
        }
        // Both spellings exist across daemon versions; one carries a list and
        // one a single item.
        "newChatItems" | "newChatItem" => {
            let items = field(&event, &["chatItems"])
                .or_else(|| field(body, &["chatItems"]))
                .or_else(|| field(&event, &["chatItem"]))
                .or_else(|| field(body, &["chatItem"]));

            let messages = match items {
                Some(serde_json::Value::Array(items)) => {
                    items.iter().filter_map(message_from_item).collect()
                }
                Some(item) => message_from_item(item).into_iter().collect(),
                None => Vec::new(),
            };

            if messages.is_empty() {
                return Ok(SimplexEvent::Ignored);
            }
            Ok(SimplexEvent::Messages(messages))
        }
        _ => Ok(SimplexEvent::Ignored),
    }
}

/// Pull a message out of one chat item, or `None` when it is not one for us.
fn message_from_item(item: &serde_json::Value) -> Option<SimplexIncoming> {
    // Our own messages come back on the same socket. Answering them would make
    // the bot talk to itself.
    let direction = text_at(item, &["chatItem", "chatDir", "type"]);
    if DIRECTIONS_SENT.contains(&direction.as_str()) {
        return None;
    }

    // Anything that is not something a person sent — a membership change, a
    // deleted item — carries no text worth having.
    if text_at(item, &["chatItem", "content", "type"]) != CONTENT_RECEIVED {
        return None;
    }

    let kind = text_at(item, &["chatItem", "content", "msgContent", "type"]);
    if !TEXT_BEARING.contains(&kind.as_str()) {
        return None;
    }
    let text = text_at(item, &["chatItem", "content", "msgContent", "text"])
        .trim()
        .to_string();
    if text.is_empty() {
        return None;
    }

    let (user_id, conversation_id, is_group) = match text_at(item, &["chatInfo", "type"]).as_str() {
        CHAT_DIRECT => {
            let id = text_at(item, &["chatInfo", "contact", "contactId"]);
            if id.is_empty() {
                return None;
            }
            (id.clone(), id, false)
        }
        CHAT_GROUP => {
            let group_id = text_at(item, &["chatInfo", "groupInfo", "groupId"]);
            if group_id.is_empty() {
                return None;
            }
            // In a group the sender is the member, not the conversation.
            let member = text_at(item, &["chatItem", "chatDir", "groupMember", "memberId"]);
            if member.is_empty() {
                return None;
            }
            (member, format!("group:{}", group_id), true)
        }
        _ => return None,
    };

    Some(SimplexIncoming {
        user_id,
        conversation_id,
        is_group,
        text,
    })
}

/// The command that sends text to a conversation.
///
/// A command *string*, not a structure: SimpleX's own syntax, with the message
/// JSON encoded inside it. Addressing is by id — the display-name forms resolve
/// a name at the daemon and silently drop when it does not, which is not
/// something a reply should be able to do.
pub fn send_command(conversation_id: &str, text: &str) -> String {
    let target = match conversation_id.strip_prefix("group:") {
        Some(group_id) => format!("#{}", group_id),
        None => format!("@{}", conversation_id),
    };
    let items = serde_json::json!([{ "msgContent": { "type": "text", "text": text } }]);
    format!("/_send {} json {}", target, items)
}

/// SimpleX channel implementation.
pub struct SimplexChannel {
    config: SimplexConfig,
    running: Arc<AtomicBool>,
    /// Cancelled by `stop()`, so a stop does not wait for the next event.
    shutdown: CancellationToken,
    /// Commands for the socket to write, from `send` and from the event loop
    /// itself. One queue, because the socket has one writer.
    outbound_tx: mpsc::Sender<String>,
    /// Taken by `start`, which is the only thing that owns the socket.
    outbound_rx: tokio::sync::Mutex<Option<mpsc::Receiver<String>>>,
    /// `corrId`s this process minted, so its own command echoes are not read
    /// back as events.
    own_corr_ids: Arc<RwLock<Vec<String>>>,
}

impl std::fmt::Debug for SimplexChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimplexChannel")
            .field("config", &self.config)
            .field("running", &self.running)
            .finish()
    }
}

impl SimplexChannel {
    /// Create a new SimpleX channel.
    pub fn new(config: SimplexConfig) -> Self {
        let (outbound_tx, outbound_rx) = mpsc::channel(64);

        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            shutdown: CancellationToken::new(),
            outbound_tx,
            outbound_rx: tokio::sync::Mutex::new(Some(outbound_rx)),
            own_corr_ids: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// One connection, from the socket to the moment it drops.
    async fn serve(&self, outbound_rx: &mut mpsc::Receiver<String>) -> crate::Result<()> {
        let (socket, _) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async(&self.config.ws_url),
        )
        .await
        .map_err(|_| self.failed("the connection timed out"))?
        .map_err(|e| self.failed(&format!("the connection failed: {}", e)))?;

        let (mut write, mut read) = socket.split();
        info!("SimpleX channel: connected to {}", self.config.ws_url);

        loop {
            // Two branches, and only one of them writes. The keepalive shares
            // the read branch rather than being a third, because two branches
            // that wrote would not borrow — and the read branch here writes
            // nothing at all, since answering an event means queueing a command
            // for the write branch.
            tokio::select! {
                _ = self.shutdown.cancelled() => break,

                command = outbound_rx.recv() => {
                    let Some(command) = command else { break };
                    write
                        .send(Message::text(command))
                        .await
                        .map_err(|e| self.failed(&format!("the write failed: {}", e)))?;
                }

                incoming = tokio::time::timeout(KEEPALIVE_INTERVAL, read.next()) => {
                    match incoming {
                        Ok(Some(Ok(Message::Text(text)))) => self.handle_event(&text).await,
                        Ok(Some(Ok(Message::Close(_)))) => {
                            info!("SimpleX channel: the daemon closed the connection");
                            break;
                        }
                        Ok(Some(Ok(_))) => {}
                        Ok(Some(Err(e))) => {
                            return Err(self.failed(&format!("the socket errored: {}", e)));
                        }
                        Ok(None) => break,
                        Err(_) => {
                            // The daemon is application-silent by design, so this
                            // says nothing about whether it is alive — that is
                            // what the WebSocket's own ping is for.
                            write.send(Message::Ping(Vec::new())).await.map_err(|e| {
                                self.failed(&format!("the keepalive failed: {}", e))
                            })?;
                        }
                    }
                }
            }
        }

        if let Err(e) = write.send(Message::Close(None)).await {
            debug!("SimpleX channel: could not close the socket cleanly: {}", e);
        }
        Ok(())
    }

    /// Act on one event from the daemon.
    async fn handle_event(&self, raw: &str) {
        // A command this process sent comes back as an echo, and is not an
        // event. The daemon stamps our own `corrId` on it.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
            if let Some(corr_id) = value.get("corrId").and_then(|c| c.as_str()) {
                let own = self.own_corr_ids.write().await;
                if own.iter().any(|id| id == corr_id) {
                    return;
                }
            }
        }

        let event = match parse_event(raw) {
            Ok(event) => event,
            Err(e) => {
                warn!("SimpleX channel: {}", e);
                return;
            }
        };

        match event {
            SimplexEvent::AcceptContact { request_id } => {
                if !self.config.auto_accept {
                    debug!("SimpleX channel: leaving a contact request alone");
                    return;
                }
                if request_id.is_empty() {
                    return;
                }
                info!("SimpleX channel: accepting a contact request");
                let frame = self.envelope(format!("/accept {}", request_id)).await;
                self.queue(frame);
            }
            SimplexEvent::Messages(messages) => {
                for incoming in messages {
                    self.deliver(incoming);
                }
            }
            SimplexEvent::Ignored => {}
        }
    }

    /// Hand one message to the inbound seam.
    fn deliver(&self, incoming: SimplexIncoming) {
        // SimpleX has no mention: in a group there is no way for a sender to
        // address the bot, so a group message is reported as unmentioned. That
        // makes `require_mention` mean "say nothing in groups", which is the
        // only reading of it this platform supports.
        let mention = if incoming.is_group {
            MentionState::NotMentioned
        } else {
            MentionState::DirectMessage
        };

        let Some(tx) = &self.config.message_tx else {
            return;
        };

        let message =
            IncomingMessage::new(&incoming.user_id, &incoming.conversation_id, &incoming.text)
                .with_provenance(InputProvenance::ExternalUser {
                    channel: "simplex".to_string(),
                    is_direct: !incoming.is_group,
                })
                .with_mention(mention);

        if tx.send(message).is_err() {
            warn!("SimpleX channel: the inbound bridge is gone; dropping the message");
        }
    }

    /// Wrap a command in the envelope every frame on this socket takes, and
    /// remember its correlation id so the daemon's echo of it is not read back
    /// as an event.
    async fn envelope(&self, command: String) -> String {
        let corr_id = format!("syscity-{}", uuid::Uuid::new_v4().simple());

        let mut own = self.own_corr_ids.write().await;
        if own.len() >= CORR_ID_MEMORY {
            own.remove(0);
        }
        own.push(corr_id.clone());

        serde_json::json!({ "corrId": corr_id, "cmd": command }).to_string()
    }

    /// Queue a frame for the write branch.
    fn queue(&self, frame: String) {
        if let Err(e) = self.outbound_tx.try_send(frame) {
            warn!("SimpleX channel: could not queue a command ({})", e);
        }
    }

    /// One error shape for everything the channel can fail at.
    fn failed(&self, reason: &str) -> SyscityError {
        SyscityError::ExternalService {
            source: format!("SimpleX: {}", reason),
            cause: None,
        }
    }
}

#[async_trait]
impl Channel for SimplexChannel {
    fn name(&self) -> &str {
        "simplex"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            chat_types: vec![ChatType::Direct, ChatType::Group],
            // SimpleX renders a little markdown; the plain-text reduction is
            // what the agent's markdown becomes safely.
            supports_formatting: false,
            supports_attachments: false,
            supports_images: false,
            supports_threads: false,
            supports_typing: false,
            supports_buttons: false,
            supports_commands: true,
            supports_reactions: false,
            supports_edit: false,
            supports_unsend: false,
            supports_effects: false,
        }
    }

    async fn start(&self) -> crate::Result<()> {
        let Some(mut outbound_rx) = self.outbound_rx.lock().await.take() else {
            return Err(SyscityError::Internal("SimpleX channel is already running".into()));
        };

        info!("Starting SimpleX channel...");
        self.running.store(true, Ordering::SeqCst);

        let mut failures = 0usize;
        while !self.shutdown.is_cancelled() {
            let started = Instant::now();
            match self.serve(&mut outbound_rx).await {
                Ok(()) if self.shutdown.is_cancelled() => break,
                Ok(()) => info!("SimpleX channel: the connection closed; reconnecting"),
                Err(e) => warn!("SimpleX channel: {}; reconnecting", e),
            }

            if self.shutdown.is_cancelled() {
                break;
            }

            // A connection that lasted is a healthy one: only one that failed
            // straight away counts towards the backoff.
            if started.elapsed() >= KEEPALIVE_INTERVAL {
                failures = 0;
            }
            let wait = Duration::from_secs(BACKOFF_SECS[failures.min(BACKOFF_SECS.len() - 1)]);
            failures += 1;
            info!("SimpleX channel: reconnecting in {}s", wait.as_secs());

            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = tokio::time::sleep(wait) => {}
            }
        }

        self.running.store(false, Ordering::SeqCst);
        info!("SimpleX channel stopped");
        Ok(())
    }

    async fn stop(&self) -> crate::Result<()> {
        info!("Stopping SimpleX channel...");
        self.shutdown.cancel();
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        let conversation_id = message.conversation_id.0.clone();

        let text = match &message.formatted_content {
            Some(crate::channels::FormattedContent::Markdown(md)) => {
                PlainTextFormatter::strip_markdown(md)
            }
            _ => PlainTextFormatter::strip_markdown(&message.content),
        };

        // SimpleX has no hard limit; this is a sanity bound rather than the
        // platform's, so a long answer still arrives as consecutive messages.
        for segment in split_message(&text, MAX_SIMPLEX_CHARS) {
            let frame = self
                .envelope(send_command(&conversation_id, &segment))
                .await;

            self.outbound_tx
                .send(frame)
                .await
                .map_err(|_| self.failed("the connection is not running"))?;
        }

        debug!("SimpleX message posted to {}", conversation_id);
        Ok(Id::new())
    }

    async fn send_typing(&self, _conversation_id: &ConversationId) -> crate::Result<()> {
        // SimpleX has no typing indicator for a bot.
        Ok(())
    }

    async fn edit_message(&self, _message_id: Id, _new_content: String) -> crate::Result<()> {
        Err(SyscityError::Unsupported("SimpleX messages cannot be edited from here".into()))
    }

    async fn delete_message(&self, _message_id: Id) -> crate::Result<()> {
        Err(SyscityError::Unsupported("SimpleX messages cannot be deleted from here".into()))
    }

    async fn health_check(&self) -> crate::Result<bool> {
        Ok(self.running.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, body: serde_json::Value) -> String {
        serde_json::json!({
            "corrId": "daemon-1",
            "resp": { "type": kind, "chatItems": [body] },
        })
        .to_string()
    }

    fn item(
        chat: serde_json::Value,
        direction: &str,
        content: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "chatInfo": chat,
            "chatItem": { "chatDir": { "type": direction }, "content": content },
        })
    }

    fn received(kind: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "rcvMsgContent",
            "msgContent": { "type": kind, "text": text },
        })
    }

    fn direct() -> serde_json::Value {
        serde_json::json!({
            "type": "direct",
            "contact": { "contactId": 42, "profile": { "displayName": "Alice" } },
        })
    }

    #[test]
    fn a_direct_message_parses_out_of_the_envelope() {
        let raw =
            event("newChatItems", item(direct(), "directRcv", received("text", "hello there")));

        let SimplexEvent::Messages(messages) = parse_event(&raw).expect("parses") else {
            panic!("expected a message");
        };
        assert_eq!(
            messages,
            vec![SimplexIncoming {
                user_id: "42".to_string(),
                conversation_id: "42".to_string(),
                is_group: false,
                text: "hello there".to_string(),
            }]
        );
    }

    /// In a group the sender is the member and the conversation is the group,
    /// so the two ids are different.
    #[test]
    fn a_group_message_separates_its_sender_from_its_conversation() {
        let group = serde_json::json!({
            "type": "group",
            "groupInfo": { "groupId": 7, "localDisplayName": "team" },
        });
        let chat_item = serde_json::json!({
            "chatInfo": group,
            "chatItem": {
                "chatDir": { "type": "groupRcv", "groupMember": { "memberId": 99 } },
                "content": received("text", "hello everyone"),
            },
        });
        let raw = serde_json::json!({
            "corrId": "daemon-1",
            "resp": { "type": "newChatItems", "chatItems": [chat_item] },
        })
        .to_string();

        let SimplexEvent::Messages(messages) = parse_event(&raw).expect("parses") else {
            panic!("expected a message");
        };
        assert_eq!(messages[0].user_id, "99");
        assert_eq!(messages[0].conversation_id, "group:7");
        assert!(messages[0].is_group);
    }

    /// Our own messages come back on the same socket.
    #[test]
    fn our_own_message_is_dropped() {
        for direction in DIRECTIONS_SENT {
            let raw =
                event("newChatItems", item(direct(), direction, received("text", "my own words")));
            assert_eq!(parse_event(&raw).expect("parses"), SimplexEvent::Ignored, "{direction}");
        }
    }

    #[test]
    fn everything_that_is_not_a_message_is_ignored() {
        // Content that is not something a person sent.
        for content in [
            serde_json::json!({ "type": "chatMember", "member": {} }),
            received("unknownKind", "hello"),
        ] {
            let raw = event("newChatItems", item(direct(), "directRcv", content));
            assert_eq!(parse_event(&raw).expect("parses"), SimplexEvent::Ignored);
        }

        // Empty text, and a conversation the channel cannot address.
        let raw = event("newChatItems", item(direct(), "directRcv", received("text", "   ")));
        assert_eq!(parse_event(&raw).expect("parses"), SimplexEvent::Ignored);

        let no_contact = serde_json::json!({ "type": "direct", "contact": {} });
        let raw = event("newChatItems", item(no_contact, "directRcv", received("text", "hi")));
        assert_eq!(parse_event(&raw).expect("parses"), SimplexEvent::Ignored);

        // Events this channel does not act on.
        for kind in [
            "rcvFileDescrReady",
            "rcvFileComplete",
            "chatItemUpdated",
            "unknown",
        ] {
            let raw = serde_json::json!({ "corrId": "d", "resp": { "type": kind } }).to_string();
            assert_eq!(parse_event(&raw).expect("parses"), SimplexEvent::Ignored, "{kind}");
        }
    }

    /// The daemon does not always nest the response, so both spellings are read.
    #[test]
    fn an_event_without_the_resp_wrapper_is_still_read() {
        let raw = serde_json::json!({
            "type": "newChatItems",
            "chatItems": [item(direct(), "directRcv", received("text", "hello"))],
        })
        .to_string();

        let SimplexEvent::Messages(messages) = parse_event(&raw).expect("parses") else {
            panic!("expected a message");
        };
        assert_eq!(messages[0].text, "hello");
    }

    /// The singular spelling carries one item rather than a list.
    #[test]
    fn a_single_chat_item_is_read_as_well_as_a_list() {
        let raw = serde_json::json!({
            "corrId": "d",
            "resp": {
                "type": "newChatItem",
                "chatItem": item(direct(), "directRcv", received("text", "one")),
            },
        })
        .to_string();

        let SimplexEvent::Messages(messages) = parse_event(&raw).expect("parses") else {
            panic!("expected a message");
        };
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "one");
    }

    #[test]
    fn a_contact_request_asks_to_be_accepted() {
        let raw = serde_json::json!({
            "corrId": "daemon-1",
            "resp": {
                "type": "contactRequest",
                "contactRequest": { "contactRequestId": "req-1", "contactId": 55 },
            },
        })
        .to_string();

        assert_eq!(
            parse_event(&raw).expect("parses"),
            SimplexEvent::AcceptContact {
                request_id: "req-1".to_string()
            }
        );
    }

    #[test]
    fn an_event_that_is_not_json_is_an_error() {
        assert!(parse_event("not json").is_err());
        // An empty frame is what a stray newline looks like on this socket.
        assert!(parse_event("").is_err());
    }

    /// The command is SimpleX's own syntax, and the message is JSON inside it.
    #[test]
    fn a_send_command_addresses_by_id() {
        let command = send_command("42", "hello");
        assert!(command.starts_with("/_send @42 json "), "{command}");
        assert!(command.contains(r#""type":"text""#), "{command}");

        let group = send_command("group:7", "hello");
        assert!(group.starts_with("/_send #7 json "), "{group}");
    }

    /// A message containing quotes would break the command if it were not
    /// encoded — and a hostile one could otherwise forge the JSON around it.
    #[test]
    fn a_message_is_escaped_inside_the_command() {
        let command = send_command("42", r#"he said "hi" and }{"injected":true}"#);

        let items = command.trim_start_matches("/_send @42 json ");
        let parsed: serde_json::Value = serde_json::from_str(items).expect("valid JSON");
        assert_eq!(
            parsed[0]["msgContent"]["text"], r#"he said "hi" and }{"injected":true}"#,
            "the message stays a value, and cannot become structure"
        );
    }

    /// The channel's own `Debug` carries nothing secret, but it should not
    /// carry a token either — there is none to leak, and this pins that.
    #[test]
    fn the_debug_rendering_shows_where_it_connects() {
        let channel = SimplexChannel::new(SimplexConfig::new("ws://127.0.0.1:9999"));
        let rendered = format!("{:?}", channel);
        assert!(rendered.contains("9999"), "{rendered}");
    }
}
