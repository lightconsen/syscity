//! WeCom (企业微信) AI Bot channel, over its WebSocket gateway.
//!
//! Unlike every channel before it, WeCom has no webhook. The gateway opens a
//! long-lived WebSocket to `wss://openws.work.weixin.qq.com`, authenticates by
//! sending a first frame, and both receives messages and sends replies over
//! that one socket. So the *transport* half is new here and the agent-facing
//! half is not: inbound goes through
//! [`dispatch_channel_message_to`](crate::gateway::webhooks::dispatch_channel_message_to),
//! the same tail the webhook channels use.
//!
//! Four things shape it:
//!
//! - **One connection per bot, and no resume.** A second connection kicks the
//!   first, and a reconnect is a fresh subscribe rather than a resumption, so
//!   the loop is "connect, subscribe, serve, and start over when it drops".
//! - **A reply is a frame that echoes the request it answers.** An inbound
//!   message carries a `req_id`, and replying means sending `aibot_respond_msg`
//!   with that same `req_id` — which is what puts the answer in the thread the
//!   question came from. The window on a `req_id` closes, so the last one seen
//!   per chat is remembered and a closed window falls back to the other path.
//! - **A group can only be answered in reply mode.** An AI Bot cannot start a
//!   conversation in a group, so a reply with no live `req_id` is refused there
//!   rather than sent into the void.
//! - **The connection task is the socket's only owner.** `send` does not touch
//!   the socket; it hands a frame to the task. That is what keeps a reconnect
//!   from racing a write, and it is why the task is registered in the gateway's
//!   `TaskRegistry` rather than left in a bare `tokio::spawn`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::channels::{
    split_message, Channel, ChannelCapabilities, ChatType, ConversationId, IncomingMessage,
    InputProvenance, MentionState, OutgoingMessage, PlainTextFormatter,
};
use crate::core::models::Id;
use crate::error::SyscityError;
use crate::gateway::webhooks::ReplayGuard;

/// Where the AI Bot gateway lives.
const DEFAULT_WS_URL: &str = "wss://openws.work.weixin.qq.com";

// The command names, as they appear on the wire.
const CMD_SUBSCRIBE: &str = "aibot_subscribe";
const CMD_PING: &str = "ping";
const CMD_RESPOND: &str = "aibot_respond_msg";
const CMD_SEND: &str = "aibot_send_msg";
const CMD_CALLBACK: &str = "aibot_msg_callback";
const CMD_LEGACY_CALLBACK: &str = "aibot_callback";
const CMD_EVENT_CALLBACK: &str = "aibot_event_callback";

/// The commands that carry a message *in*.
///
/// Anything else arriving with a request id we are waiting on is that request's
/// acknowledgement — the acknowledgement's own `cmd` is not documented, so it
/// is recognised by what it is *not*.
const INBOUND_COMMANDS: &[&str] = &[CMD_CALLBACK, CMD_LEGACY_CALLBACK, CMD_EVENT_CALLBACK];

/// How long a connection has to authenticate before it is given up on.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a message may take to be acknowledged before it is assumed
/// delivered. WeCom renders the message before it acknowledges it, so treating
/// a missing acknowledgement as a failure would send it twice.
const ACK_TIMEOUT: Duration = Duration::from_secs(15);

/// The protocol's own keepalive, sent once the socket has been quiet this long.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Backoff between reconnects, indexed by consecutive failures.
const BACKOFF_SECS: [u64; 5] = [2, 5, 10, 30, 60];

/// The most characters one WeCom message carries.
pub const MAX_WECOM_CHARS: usize = 4000;

/// The window on a request id has closed; the reply has to go out the other way.
const ERR_REQ_EXPIRED: i64 = 846604;

/// The socket's halves, named once so the helpers can take them directly.
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type SocketWrite = futures::stream::SplitSink<Socket, Message>;
type SocketRead = futures::stream::SplitStream<Socket>;

/// WeCom AI Bot channel configuration.
#[derive(Clone)]
pub struct WeComConfig {
    /// The AI Bot's id, from the admin console.
    pub bot_id: String,
    /// Its secret.
    pub secret: String,
    /// The gateway to connect to. Configurable because a sovereign deployment
    /// publishes its own, and because a test needs somewhere to point.
    pub ws_url: String,
    /// Where extracted messages go.
    pub message_tx: Option<mpsc::UnboundedSender<IncomingMessage>>,
}

impl std::fmt::Debug for WeComConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The secret is deliberately not printed.
        f.debug_struct("WeComConfig")
            .field("bot_id", &self.bot_id)
            .field("ws_url", &self.ws_url)
            .field("message_tx", &self.message_tx.is_some())
            .finish()
    }
}

impl WeComConfig {
    /// Configuration for one bot, against the public gateway.
    pub fn new(bot_id: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            bot_id: bot_id.into(),
            secret: secret.into(),
            ws_url: DEFAULT_WS_URL.to_string(),
            message_tx: None,
        }
    }
}

/// A frame, in the shape every frame on this socket takes.
fn frame(cmd: &str, req_id: &str, body: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "cmd": cmd,
        "headers": { "req_id": req_id },
        "body": body,
    })
}

/// A fresh request id, named after the command it belongs to.
fn new_req_id(cmd: &str) -> String {
    format!("{}-{}", cmd, uuid::Uuid::new_v4().simple())
}

/// The request id a frame carries, if it carries one.
fn req_id_of(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("headers")?
        .get("req_id")?
        .as_str()
        .map(str::to_string)
        .filter(|id| !id.is_empty())
}

/// The server's answer to a frame we sent.
#[derive(Debug, Clone)]
struct Ack {
    errcode: i64,
    errmsg: String,
}

/// Read an acknowledgement out of a frame.
///
/// The reference implementation reads `errcode` from the top level in some
/// places and from `body` in others, and its own tests disagree about which —
/// so both are accepted, and a frame carrying neither is treated as a
/// success rather than as a failure to parse.
fn ack_from(payload: &serde_json::Value) -> Ack {
    fn read(value: &serde_json::Value) -> Option<(i64, String)> {
        let errcode = value.get("errcode")?.as_i64()?;
        let errmsg = value
            .get("errmsg")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        Some((errcode, errmsg))
    }

    let (errcode, errmsg) = read(payload)
        .or_else(|| payload.get("body").and_then(read))
        .unwrap_or((0, String::new()));

    Ack { errcode, errmsg }
}

/// Whether a frame is the platform saying another connection took this bot's
/// place.
fn is_disconnect_event(payload: &serde_json::Value) -> bool {
    payload
        .get("body")
        .and_then(|b| b.get("event_type"))
        .and_then(|e| e.as_str())
        == Some("disconnected_event")
}

/// A message extracted from an inbound callback frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeComIncoming {
    /// The platform's id for this message — the dedup key.
    pub msg_id: String,
    /// The frame's request id, which a reply has to echo.
    pub req_id: String,
    /// The conversation to answer in.
    pub chat_id: String,
    /// Whether that conversation is a group.
    pub is_group: bool,
    /// Whether the message named the bot. Only meaningful in a group, where a
    /// message that did not is one the gateway may be configured to ignore.
    pub mentioned: bool,
    /// The sender.
    pub user_id: String,
    /// The message text, with a leading group mention removed.
    pub text: String,
}

/// Remove the leading `@Bot ` a group message carries, and say whether there
/// was one.
///
/// Only the leading one: WeCom renders a mention as plain text, so an `@`
/// further in is something the user typed. Whether the mention was there is
/// worth keeping — it is what tells the gateway this message was addressed to
/// the bot rather than merely visible to it.
fn strip_mention(text: &str) -> (String, bool) {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix('@') else {
        return (trimmed.to_string(), false);
    };

    let after = rest.trim_start_matches(|c: char| !c.is_whitespace());
    // A bare `@` is the user's own text, not a mention of anybody.
    if after.is_empty() || after.len() == rest.len() {
        return (trimmed.to_string(), false);
    }

    (after.trim_start().to_string(), true)
}

/// Extract a message from an inbound frame, or `None` when it is not one.
///
/// WeCom also posts lifecycle events and acknowledgements to the same socket,
/// and a message that is not text — an image, a file, a voice note — has no
/// content this channel can act on. Everything but a text message from a person
/// is dropped here.
pub fn parse_callback(raw: &str) -> crate::Result<Option<WeComIncoming>> {
    #[derive(Deserialize, Default)]
    struct Headers {
        #[serde(default)]
        req_id: Option<String>,
    }

    #[derive(Deserialize)]
    struct From {
        #[serde(default)]
        userid: Option<String>,
    }

    #[derive(Deserialize)]
    struct Text {
        #[serde(default)]
        content: Option<String>,
    }

    #[derive(Deserialize)]
    struct Body {
        #[serde(default)]
        msgid: Option<String>,
        #[serde(default)]
        chatid: Option<String>,
        #[serde(default)]
        chattype: Option<String>,
        #[serde(default)]
        from: Option<From>,
        #[serde(default)]
        msgtype: Option<String>,
        #[serde(default)]
        text: Option<Text>,
    }

    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        cmd: Option<String>,
        #[serde(default)]
        headers: Headers,
        #[serde(default)]
        body: Option<Body>,
    }

    let envelope: Envelope = match serde_json::from_str(raw) {
        Ok(envelope) => envelope,
        Err(_) => {
            // WeCom has been seen putting raw control characters inside JSON
            // strings, which strict parsing rejects. They are not what the user
            // wrote, so the retry drops them rather than the message.
            let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
            serde_json::from_str(&cleaned)
                .map_err(|e| SyscityError::Validation(format!("Invalid WeCom frame: {}", e)))?
        }
    };

    if !envelope
        .cmd
        .as_deref()
        .is_some_and(|cmd| INBOUND_COMMANDS.contains(&cmd))
    {
        return Ok(None);
    }

    // The event callback is a lifecycle notice — a kick, a subscription change
    // — not a message.
    if envelope.cmd.as_deref() == Some(CMD_EVENT_CALLBACK) {
        return Ok(None);
    }

    let Some(body) = envelope.body else {
        return Ok(None);
    };

    let req_id = envelope.headers.req_id.unwrap_or_default();
    let msg_id = body
        .msgid
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| req_id.clone());
    if msg_id.is_empty() {
        return Ok(None);
    }

    let Some(user_id) = body.from.and_then(|f| f.userid).filter(|id| !id.is_empty()) else {
        return Ok(None);
    };

    if body.msgtype.as_deref() != Some("text") {
        return Ok(None);
    }
    let Some(content) = body.text.and_then(|t| t.content) else {
        return Ok(None);
    };

    let is_group = body
        .chattype
        .as_deref()
        .is_some_and(|kind| kind.eq_ignore_ascii_case("group"));

    // A group is its own conversation; a direct message is addressed by the
    // person, since that is all the platform sends.
    let chat_id = body
        .chatid
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| user_id.clone());

    let (text, mentioned) = if is_group {
        strip_mention(&content)
    } else {
        (content.trim().to_string(), true)
    };
    if text.is_empty() {
        return Ok(None);
    }

    Ok(Some(WeComIncoming {
        msg_id,
        req_id,
        chat_id,
        is_group,
        mentioned,
        user_id,
        text,
    }))
}

/// A frame for the connection task to write.
struct Outbound {
    payload: serde_json::Value,
    /// The request id the server will acknowledge it with.
    req_id: String,
    /// Where the acknowledgement goes.
    ack: Option<oneshot::Sender<Ack>>,
}

/// How a chat is answered, learned from the messages that arrive.
#[derive(Debug, Clone)]
struct ChatTarget {
    /// The request id of the most recent message, while its reply window is
    /// open. `None` once the server has said that window closed.
    req_id: Option<String>,
    /// A group, where an AI Bot cannot start a conversation.
    is_group: bool,
}

/// Why a message could not be delivered.
enum Delivery {
    /// The reply window on that request id had closed.
    ReqExpired,
    /// Anything else, already rendered.
    Failed(SyscityError),
}

/// WeCom AI Bot channel implementation.
pub struct WeComChannel {
    config: WeComConfig,
    running: Arc<AtomicBool>,
    /// Cancelled by `stop()`. The connection loop watches it, so a stop does
    /// not wait for the next frame to arrive before taking effect.
    shutdown: CancellationToken,
    /// Stable for the life of the process: the platform uses it to recognise
    /// the same client coming back rather than a second one.
    device_id: String,
    outbound_tx: mpsc::UnboundedSender<Outbound>,
    /// Taken by `start`, which is the only thing that owns the socket.
    outbound_rx: tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<Outbound>>>,
    /// chat id to how it is answered, learned from the messages that arrive.
    chats: Arc<RwLock<HashMap<String, ChatTarget>>>,
    /// The platform redelivers, and answering twice is worse than dropping.
    seen: Arc<ReplayGuard>,
}

impl std::fmt::Debug for WeComChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeComChannel")
            .field("config", &self.config)
            .field("running", &self.running)
            .finish()
    }
}

impl WeComChannel {
    /// Create a new WeCom channel.
    pub fn new(config: WeComConfig) -> Self {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();

        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            shutdown: CancellationToken::new(),
            device_id: uuid::Uuid::new_v4().simple().to_string(),
            outbound_tx,
            outbound_rx: tokio::sync::Mutex::new(Some(outbound_rx)),
            chats: Arc::new(RwLock::new(HashMap::new())),
            seen: Arc::new(ReplayGuard::default()),
        }
    }

    /// One connection, from the subscribe frame to the moment it drops.
    async fn serve(
        &self,
        outbound_rx: &mut mpsc::UnboundedReceiver<Outbound>,
    ) -> crate::Result<()> {
        let (socket, _) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(&self.config.ws_url),
        )
        .await
        .map_err(|_| self.failed("the connection timed out"))?
        .map_err(|e| self.failed(&format!("the connection failed: {}", e)))?;

        let (mut write, mut read) = socket.split();
        self.subscribe(&mut write, &mut read).await?;
        info!("WeCom channel: subscribed as bot {}", self.config.bot_id);

        // Acknowledgements we are waiting on. This lives here rather than on the
        // channel so that a dropped connection fails its pending sends instead
        // of leaving them to time out.
        let mut pending: HashMap<String, oneshot::Sender<Ack>> = HashMap::new();

        loop {
            tokio::select! {
                // `stop()`, or the gateway's shutdown.
                _ = self.shutdown.cancelled() => break,

                // Anything the server sent.
                incoming = read.next() => {
                    let Some(incoming) = incoming else { break };
                    let message = incoming.map_err(|e| {
                        self.failed(&format!("the socket errored: {}", e))
                    })?;

                    match message {
                        Message::Text(text) => self.handle_frame(&text, &mut pending).await,
                        Message::Close(_) => {
                            info!("WeCom channel: the server closed the connection");
                            break;
                        }
                        _ => {}
                    }
                }

                // A frame to write — or, when nothing has been written for a
                // while, the keepalive. One branch for both, because the socket
                // has one writer.
                egress = tokio::time::timeout(HEARTBEAT_INTERVAL, outbound_rx.recv()) => {
                    match egress {
                        Ok(Some(outbound)) => {
                            write.send(Message::text(outbound.payload.to_string())).await
                                .map_err(|e| self.failed(&format!("the write failed: {}", e)))?;
                            if let Some(ack) = outbound.ack {
                                pending.insert(outbound.req_id, ack);
                            }
                        }
                        // Every sender is gone; nothing can be sent any more.
                        Ok(None) => break,
                        Err(_) => {
                            let req_id = new_req_id(CMD_PING);
                            let ping = frame(CMD_PING, &req_id, serde_json::json!({}));
                            write.send(Message::text(ping.to_string())).await
                                .map_err(|e| self.failed(&format!("the keepalive failed: {}", e)))?;
                        }
                    }
                }
            }
        }

        // Pending acknowledgements drop here, which is what tells their callers
        // the connection went away.
        // Closing politely is best-effort: the connection is ending either
        // way, and a failure here says nothing worth acting on.
        if let Err(e) = write.send(Message::Close(None)).await {
            debug!("WeCom channel: could not close the socket cleanly: {}", e);
        }
        Ok(())
    }

    /// Authenticate the connection and wait for the server to accept it.
    ///
    /// The credential is the *first frame*, not a header or a query parameter,
    /// so nothing can be sent until this has been answered — and a wrong secret
    /// is reported here rather than on the first message.
    async fn subscribe(&self, write: &mut SocketWrite, read: &mut SocketRead) -> crate::Result<()> {
        let req_id = new_req_id(CMD_SUBSCRIBE);
        let body = serde_json::json!({
            "bot_id": self.config.bot_id,
            "secret": self.config.secret,
            // Stable across reconnects, so the platform sees the same client
            // coming back rather than a second one.
            "device_id": self.device_id,
        });

        write
            .send(Message::text(frame(CMD_SUBSCRIBE, &req_id, body).to_string()))
            .await
            .map_err(|e| self.failed(&format!("the subscribe could not be sent: {}", e)))?;

        let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
        loop {
            let next = tokio::time::timeout_at(deadline, read.next())
                .await
                .map_err(|_| self.failed("the subscribe was never acknowledged"))?;

            let Some(next) = next else {
                return Err(self.failed("the socket closed during authentication"));
            };
            let message = next.map_err(|e| self.failed(&format!("the socket errored: {}", e)))?;

            let Message::Text(text) = message else {
                continue;
            };
            let Ok(payload) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };

            // The server's own keepalive arrives while we wait.
            if payload.get("cmd").and_then(|c| c.as_str()) == Some(CMD_PING) {
                continue;
            }
            if req_id_of(&payload).as_deref() != Some(req_id.as_str()) {
                continue;
            }

            let ack = ack_from(&payload);
            if ack.errcode != 0 {
                return Err(self.failed(&format!(
                    "the subscription was refused: {} (errcode {})",
                    ack.errmsg, ack.errcode
                )));
            }

            return Ok(());
        }
    }

    /// Deal with one frame from the server: an acknowledgement, or a message.
    async fn handle_frame(&self, raw: &str, pending: &mut HashMap<String, oneshot::Sender<Ack>>) {
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(raw) else {
            warn!("WeCom channel: ignoring a frame that is not JSON");
            return;
        };

        let cmd = payload
            .get("cmd")
            .and_then(|c| c.as_str())
            .unwrap_or_default();

        if !INBOUND_COMMANDS.contains(&cmd) {
            // A frame echoing a request we are waiting on is that request's
            // answer.
            if let Some(ack) = req_id_of(&payload).and_then(|id| pending.remove(&id)) {
                // The caller may have given up on the acknowledgement already;
                // that is not worth reporting.
                let _ = ack.send(ack_from(&payload));
                return;
            }

            // Another connection took this bot's place. The platform allows one
            // socket per bot, so reconnecting would only kick the newcomer back
            // and start a fight over the connection; the channel stands down
            // instead, and stays down until the gateway is restarted.
            if cmd == CMD_EVENT_CALLBACK && is_disconnect_event(&payload) {
                warn!("WeCom channel: another connection took over this bot; not reconnecting");
                self.shutdown.cancel();
            }
            return;
        }

        let incoming = match parse_callback(raw) {
            Ok(Some(incoming)) => incoming,
            Ok(None) => return,
            Err(e) => {
                warn!("WeCom channel: {}", e);
                return;
            }
        };

        // The platform redelivers; answering the same message twice is worse
        // than dropping it.
        if !self.seen.first_sighting(incoming.msg_id.as_bytes()) {
            debug!("WeCom channel: duplicate delivery {} ignored", incoming.msg_id);
            return;
        }

        // Remember how to answer this chat *before* the message goes anywhere:
        // the reply may be sent long after this frame, and the request id is the
        // only way to put it in the right thread.
        self.chats.write().await.insert(
            incoming.chat_id.clone(),
            ChatTarget {
                req_id: Some(incoming.req_id.clone()).filter(|id| !id.is_empty()),
                is_group: incoming.is_group,
            },
        );

        let mention = if !incoming.is_group {
            MentionState::DirectMessage
        } else if incoming.mentioned {
            MentionState::Mentioned
        } else {
            MentionState::NotMentioned
        };

        let Some(tx) = &self.config.message_tx else {
            return;
        };

        let message = IncomingMessage::new(&incoming.user_id, &incoming.chat_id, &incoming.text)
            .with_provenance(InputProvenance::ExternalUser {
                channel: "wecom".to_string(),
                is_direct: !incoming.is_group,
            })
            .with_mention(mention);

        if tx.send(message).is_err() {
            warn!("WeCom channel: the inbound bridge is gone; dropping the message");
        }
    }

    /// Answer a message in the thread it came from.
    async fn respond(&self, req_id: &str, text: &str) -> Result<(), Delivery> {
        // The reply echoes the *inbound* frame's request id; that is what puts
        // it in the thread the question came from.
        let payload = frame(
            CMD_RESPOND,
            req_id,
            serde_json::json!({ "msgtype": "markdown", "markdown": { "content": text } }),
        );
        self.deliver_frame(payload, req_id).await
    }

    /// Start a message rather than answer one — which only a direct message can
    /// be.
    async fn proactive(&self, chat_id: &str, is_group: bool, text: &str) -> crate::Result<()> {
        if is_group {
            return Err(SyscityError::Unsupported(
                "an AI Bot cannot start a conversation in a WeCom group, and the reply window \
                 on this one has closed"
                    .into(),
            ));
        }

        let req_id = new_req_id(CMD_SEND);
        let payload = frame(
            CMD_SEND,
            &req_id,
            serde_json::json!({
                "chatid": chat_id,
                "msgtype": "markdown",
                "markdown": { "content": text },
            }),
        );

        self.deliver_frame(payload, &req_id)
            .await
            .map_err(|e| match e {
                Delivery::Failed(e) => e,
                Delivery::ReqExpired => self.failed("a proactive send was refused"),
            })
    }

    /// Send one message into a chat, preferring the reply path.
    async fn deliver(&self, chat_id: &str, target: &ChatTarget, text: &str) -> crate::Result<()> {
        if let Some(req_id) = &target.req_id {
            match self.respond(req_id, text).await {
                Ok(()) => return Ok(()),
                Err(Delivery::ReqExpired) => {
                    // The window on that request has closed. Forget it, so the
                    // rest of this reply — and the next one — goes out the other
                    // way rather than failing the same way.
                    warn!("WeCom channel: the reply window for {} closed; falling back", chat_id);
                    if let Some(entry) = self.chats.write().await.get_mut(chat_id) {
                        entry.req_id = None;
                    }
                }
                Err(Delivery::Failed(e)) => return Err(e),
            }
        }

        self.proactive(chat_id, target.is_group, text).await
    }

    /// Hand a frame to the connection task and wait for the server's answer.
    async fn deliver_frame(
        &self,
        payload: serde_json::Value,
        req_id: &str,
    ) -> Result<(), Delivery> {
        let (ack_tx, ack_rx) = oneshot::channel();

        self.outbound_tx
            .send(Outbound {
                payload,
                req_id: req_id.to_string(),
                ack: Some(ack_tx),
            })
            .map_err(|_| Delivery::Failed(self.failed("the connection is not running")))?;

        match tokio::time::timeout(ACK_TIMEOUT, ack_rx).await {
            Ok(Ok(ack)) if ack.errcode == 0 => Ok(()),
            Ok(Ok(ack)) if ack.errcode == ERR_REQ_EXPIRED => Err(Delivery::ReqExpired),
            Ok(Ok(ack)) => Err(Delivery::Failed(self.failed(&format!(
                "the message was refused: {} (errcode {})",
                ack.errmsg, ack.errcode
            )))),
            Ok(Err(_)) => Err(Delivery::Failed(
                self.failed("the connection dropped before the message was acknowledged"),
            )),
            // WeCom renders the message before it acknowledges it, so an
            // acknowledgement that never comes is not a failed delivery — and
            // treating it as one would send the reply twice.
            Err(_) => {
                debug!(
                    "WeCom channel: no acknowledgement within {}s; assuming delivered",
                    ACK_TIMEOUT.as_secs()
                );
                Ok(())
            }
        }
    }

    /// One error shape for everything the channel can fail at.
    fn failed(&self, reason: &str) -> SyscityError {
        SyscityError::ExternalService {
            source: format!("WeCom: {}", reason),
            cause: None,
        }
    }
}

#[async_trait]
impl Channel for WeComChannel {
    fn name(&self) -> &str {
        "wecom"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            chat_types: vec![ChatType::Direct, ChatType::Group],
            // WeCom renders a subset of markdown that excludes headings and
            // tables, so markdown sent literally would half-render; the
            // plain-text reduction is what the agent's markdown becomes safely.
            supports_formatting: false,
            supports_attachments: false,
            supports_images: false,
            supports_threads: false,
            // The typing bubble is the first frame of a streamed reply, and this
            // channel does not stream.
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
            return Err(SyscityError::Internal("WeCom channel is already running".into()));
        };

        info!("Starting WeCom channel...");
        self.running.store(true, Ordering::SeqCst);

        let mut failures = 0usize;
        while !self.shutdown.is_cancelled() {
            let started = Instant::now();
            match self.serve(&mut outbound_rx).await {
                Ok(()) if self.shutdown.is_cancelled() => break,
                Ok(()) => info!("WeCom channel: the connection closed; reconnecting"),
                Err(e) => warn!("WeCom channel: {}; reconnecting", e),
            }

            if self.shutdown.is_cancelled() {
                break;
            }

            // A connection that lasted is a healthy one: only one that failed
            // straight away counts towards the backoff, so a long-lived session
            // that drops does not come back to a minute-long wait.
            if started.elapsed() >= HEARTBEAT_INTERVAL {
                failures = 0;
            }
            let wait = Duration::from_secs(BACKOFF_SECS[failures.min(BACKOFF_SECS.len() - 1)]);
            failures += 1;
            info!("WeCom channel: reconnecting in {}s", wait.as_secs());

            // Waiting out the backoff, but not through a stop.
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = tokio::time::sleep(wait) => {}
            }
        }

        self.running.store(false, Ordering::SeqCst);
        info!("WeCom channel stopped");
        Ok(())
    }

    async fn stop(&self) -> crate::Result<()> {
        info!("Stopping WeCom channel...");
        self.shutdown.cancel();
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        let chat_id = message.conversation_id.0.clone();

        let text = match &message.formatted_content {
            Some(crate::channels::FormattedContent::Markdown(md)) => {
                PlainTextFormatter::strip_markdown(md)
            }
            _ => PlainTextFormatter::strip_markdown(&message.content),
        };

        // WeCom caps a message at 4000 characters, so a longer answer arrives as
        // consecutive messages rather than being refused outright.
        for segment in split_message(&text, MAX_WECOM_CHARS) {
            // Re-read per segment: a reply window that closed on the first one
            // has to change how the second is sent.
            let target = self
                .chats
                .read()
                .await
                .get(&chat_id)
                .cloned()
                .ok_or_else(|| {
                    SyscityError::Validation(format!(
                        "WeCom can only reply to a conversation it has heard from, and {} has \
                         sent nothing",
                        chat_id
                    ))
                })?;

            self.deliver(&chat_id, &target, &segment).await?;
        }

        debug!("WeCom message posted to {}", chat_id);
        Ok(Id::new())
    }

    async fn send_typing(&self, _conversation_id: &ConversationId) -> crate::Result<()> {
        // The typing bubble on this platform is the first frame of a streamed
        // reply, and this channel does not stream.
        Ok(())
    }

    async fn edit_message(&self, _message_id: Id, _new_content: String) -> crate::Result<()> {
        Err(SyscityError::Unsupported("WeCom messages cannot be edited from here".into()))
    }

    async fn delete_message(&self, _message_id: Id) -> crate::Result<()> {
        Err(SyscityError::Unsupported("WeCom messages cannot be deleted from here".into()))
    }

    async fn health_check(&self) -> crate::Result<bool> {
        Ok(self.running.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callback(cmd: &str, body: serde_json::Value) -> String {
        serde_json::json!({
            "cmd": cmd,
            "headers": { "req_id": "req-1" },
            "body": body,
        })
        .to_string()
    }

    fn message(body: serde_json::Value) -> String {
        callback(CMD_CALLBACK, body)
    }

    fn text_message(content: &str, chattype: &str, chatid: &str) -> String {
        message(serde_json::json!({
            "msgid": "msg-1",
            "chatid": chatid,
            "chattype": chattype,
            "from": { "userid": "user-1" },
            "msgtype": "text",
            "text": { "content": content },
        }))
    }

    #[test]
    fn a_direct_message_parses_with_its_sender_its_chat_and_its_request_id() {
        let parsed = parse_callback(&text_message("hello there", "single", "chat-1"))
            .expect("the frame parses")
            .expect("it is a message");

        assert_eq!(
            parsed,
            WeComIncoming {
                msg_id: "msg-1".to_string(),
                req_id: "req-1".to_string(),
                chat_id: "chat-1".to_string(),
                is_group: false,
                mentioned: true,
                user_id: "user-1".to_string(),
                text: "hello there".to_string(),
            }
        );
    }

    /// A direct message is addressed by the person, because that is all the
    /// platform sends; a group is its own conversation.
    #[test]
    fn a_direct_message_with_no_chat_id_is_addressed_to_its_sender() {
        let parsed = parse_callback(&message(serde_json::json!({
            "msgid": "msg-1",
            "from": { "userid": "user-1" },
            "msgtype": "text",
            "text": { "content": "hello" },
        })))
        .expect("the frame parses")
        .expect("it is a message");

        assert_eq!(parsed.chat_id, "user-1");
    }

    /// The leading mention goes, the fact that it was there stays, and an `@`
    /// the user typed later in the message is left alone.
    #[test]
    fn a_group_mention_is_stripped_and_its_presence_kept() {
        let parsed = parse_callback(&text_message("@Bot can you check this", "group", "group-1"))
            .expect("the frame parses")
            .expect("it is a message");

        assert!(parsed.is_group);
        assert!(parsed.mentioned);
        assert_eq!(parsed.text, "can you check this");

        let unmentioned = parse_callback(&text_message("can you check this", "group", "group-1"))
            .expect("the frame parses")
            .expect("it is a message");
        assert!(!unmentioned.mentioned, "nothing was addressed to the bot");
        assert_eq!(unmentioned.text, "can you check this");

        let mid_sentence = parse_callback(&text_message("ask @Alice about it", "group", "group-1"))
            .expect("the frame parses")
            .expect("it is a message");
        assert_eq!(
            mid_sentence.text, "ask @Alice about it",
            "only a leading mention is the platform's"
        );
    }

    /// A bare `@` is the user's own text, not a mention of anybody — and
    /// treating it as one would empty the message.
    #[test]
    fn a_bare_at_sign_is_not_a_mention() {
        assert_eq!(strip_mention("@"), ("@".to_string(), false));
        assert_eq!(strip_mention("@  "), ("@".to_string(), false));
        assert_eq!(strip_mention("@Bot"), ("@Bot".to_string(), false));
        assert_eq!(strip_mention("  @Bot  hi  "), ("hi".to_string(), true));
        assert_eq!(strip_mention("hi"), ("hi".to_string(), false));
    }

    /// Only a text message from a person becomes a turn. WeCom posts lifecycle
    /// events, acknowledgements and other media to the same socket.
    #[test]
    fn everything_that_is_not_a_text_message_is_dropped() {
        // The other frame types on this socket.
        assert_eq!(
            parse_callback(&callback(CMD_EVENT_CALLBACK, serde_json::json!({ "event_type": "x" })))
                .expect("parses"),
            None
        );
        assert_eq!(
            parse_callback(&callback("aibot_respond_msg", serde_json::json!({ "errcode": 0 })))
                .expect("parses"),
            None
        );

        // Media, which this channel does not carry.
        for msgtype in ["image", "file", "voice", "appmsg", "mixed"] {
            let frame = message(serde_json::json!({
                "msgid": "msg-1",
                "from": { "userid": "user-1" },
                "msgtype": msgtype,
            }));
            assert_eq!(
                parse_callback(&frame).expect("parses"),
                None,
                "{msgtype} has no text this channel can act on"
            );
        }

        // Empty text, and no sender to attribute it to.
        assert_eq!(parse_callback(&text_message("   ", "single", "chat-1")).expect("parses"), None);
        assert_eq!(
            parse_callback(&message(serde_json::json!({
                "msgid": "msg-1",
                "msgtype": "text",
                "text": { "content": "hello" },
            })))
            .expect("parses"),
            None
        );
    }

    /// A message with no `msgid` still has one identity worth deduping on.
    #[test]
    fn a_message_with_no_id_falls_back_to_its_request_id() {
        let parsed = parse_callback(&message(serde_json::json!({
            "from": { "userid": "user-1" },
            "msgtype": "text",
            "text": { "content": "hello" },
        })))
        .expect("the frame parses")
        .expect("it is a message");

        assert_eq!(parsed.msg_id, "req-1");
    }

    #[test]
    fn a_frame_that_is_not_json_is_an_error() {
        assert!(parse_callback("not json").is_err());
    }

    /// WeCom has been seen putting raw control characters inside JSON strings,
    /// which strict parsing rejects. They are not what the user wrote, so the
    /// message is recovered rather than dropped.
    #[test]
    fn a_frame_with_raw_control_characters_is_recovered() {
        let frame =
            text_message("hello", "single", "chat-1").replace("\"hello\"", "\"hel\u{1}lo\"");

        assert!(
            serde_json::from_str::<serde_json::Value>(&frame).is_err(),
            "the frame really is unparseable as it stands"
        );

        let parsed = parse_callback(&frame)
            .expect("the frame is recovered")
            .expect("it is a message");
        assert_eq!(parsed.text, "hello");
    }

    /// The reference reads `errcode` from the top level in some places and from
    /// the body in others, and its own tests disagree, so both are accepted.
    #[test]
    fn an_acknowledgement_is_read_from_either_place() {
        let top = serde_json::json!({ "errcode": 846604, "errmsg": "expired" });
        assert_eq!(ack_from(&top).errcode, ERR_REQ_EXPIRED);

        let nested = serde_json::json!({ "body": { "errcode": 846604, "errmsg": "expired" } });
        assert_eq!(ack_from(&nested).errcode, ERR_REQ_EXPIRED);

        // A frame carrying neither is not a failure to parse.
        let silent = serde_json::json!({ "headers": { "req_id": "req-1" } });
        assert_eq!(ack_from(&silent).errcode, 0);
    }

    #[test]
    fn a_disconnect_event_is_recognised() {
        assert!(is_disconnect_event(&serde_json::json!({
            "body": { "event_type": "disconnected_event" }
        })));
        assert!(!is_disconnect_event(&serde_json::json!({
            "body": { "event_type": "something_else" }
        })));
        assert!(!is_disconnect_event(&serde_json::json!({ "errcode": 0 })));
    }

    #[test]
    fn a_request_id_is_named_after_its_command_and_is_unique() {
        let first = new_req_id(CMD_SUBSCRIBE);
        let second = new_req_id(CMD_SUBSCRIBE);

        assert!(first.starts_with("aibot_subscribe-"), "{first}");
        assert_ne!(first, second);
        assert_eq!(
            req_id_of(&serde_json::json!({ "headers": { "req_id": first } })).as_deref(),
            Some(first.as_str())
        );
        assert_eq!(req_id_of(&serde_json::json!({ "headers": {} })), None);
        assert_eq!(req_id_of(&serde_json::json!({ "headers": { "req_id": "" } })), None);
    }

    /// The channel's own `Debug` must not carry the bot's secret.
    #[test]
    fn the_debug_rendering_does_not_leak_the_secret() {
        let channel = WeComChannel::new(WeComConfig::new("bot-1", "super-secret-value"));

        let rendered = format!("{:?}", channel);
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
    }

    /// A chat the channel has never heard from cannot be answered: an AI Bot
    /// cannot start a conversation in a group, and this channel does not guess
    /// at direct ones.
    #[tokio::test]
    async fn sending_to_an_unknown_chat_is_refused() {
        let channel = WeComChannel::new(WeComConfig::new("bot-1", "secret"));

        let error = channel
            .send(OutgoingMessage::new(ConversationId::new("never-seen"), "hello"))
            .await
            .expect_err("a chat that has sent nothing cannot be answered");

        assert!(error.to_string().contains("never-seen"), "{error}");
    }
}
