//! DingTalk (钉钉) channel, over Stream Mode.
//!
//! Like WeCom, and unlike every channel before batch 6, there is no webhook
//! route: the gateway opens a long-lived connection and the agent-facing half
//! goes through
//! [`dispatch_channel_message_to`](crate::gateway::webhooks::dispatch_channel_message_to),
//! the same tail as everything else. Three things make DingTalk's transport its
//! own shape:
//!
//! - **A socket is not opened until the platform hands one over.** Stream Mode
//!   begins with an *HTTP* call that returns a `wss://` endpoint and a
//!   one-shot ticket, and only then is the socket opened — with the ticket as a
//!   query parameter.
//! - **Every delivery has to be acknowledged, or it comes back.** The
//!   acknowledgement goes out on the same socket, carrying the inbound message
//!   id, and it must not wait for an agent turn — so inbound is acked first and
//!   handed on after.
//! - **A reply does not go over the socket at all.** It is an HTTP POST to a
//!   per-conversation URL that *arrived inside the message*, which expires. So
//!   that URL is attacker-influenced input that gets a request sent to it, and
//!   it is held to an allowlist before anything is sent — the same guard Teams
//!   needs for its service URL.
//!
//! The protocol here was read out of the published `dingtalk-stream` SDK rather
//! than guessed; the plan file records where from.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
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
use crate::gateway::webhooks::ReplayGuard;

/// Where the gateway API lives. Overridable, which is how a test points the
/// whole handshake at a local server.
const DEFAULT_OPENAPI_ENDPOINT: &str = "https://api.dingtalk.com";

/// The call that hands out a socket and a ticket.
const OPEN_CONNECTION_PATH: &str = "/v1.0/gateway/connections/open";

/// The one topic this channel subscribes to: a person's message to the bot.
const TOPIC_BOT_MESSAGE: &str = "/v1.0/im/bot/messages/get";

/// The envelope says `SYSTEM`, `EVENT` or `CALLBACK`.
const TYPE_CALLBACK: &str = "CALLBACK";
const TYPE_SYSTEM: &str = "SYSTEM";

/// The `SYSTEM` topic that means the socket is being taken away.
const TOPIC_DISCONNECT: &str = "disconnect";

/// The hosts a reply may be posted to.
///
/// `sessionWebhook` arrives inside the message, so it is a value the sender
/// influences and the gateway sends a request to — matched exactly, the way
/// Teams matches its connector hosts, rather than by suffix.
const ALLOWED_WEBHOOK_HOSTS: &[&str] = &["api.dingtalk.com", "oapi.dingtalk.com"];

/// How long the handshake, the socket, and a reply may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the socket may be quiet before it is pinged.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);

/// How long before a session webhook really expires it is treated as expired.
///
/// The webhook is the only way to answer, and a reply that arrives a minute
/// before the deadline but lands after it has failed for no good reason.
const WEBHOOK_SAFETY_MARGIN: Duration = Duration::from_secs(300);

/// Backoff between reconnects, indexed by consecutive failures.
const BACKOFF_SECS: [u64; 5] = [2, 5, 10, 30, 60];

/// The most characters one DingTalk message carries.
pub const MAX_DINGTALK_CHARS: usize = 20_000;

/// The acknowledgement code for a delivery that was accepted.
const ACK_OK: i64 = 200;

/// DingTalk Stream Mode channel configuration.
#[derive(Clone)]
pub struct DingTalkConfig {
    /// The app's Client ID, which is also the robot's code.
    pub client_id: String,
    /// Its Client Secret.
    pub client_secret: String,
    /// Where the gateway API lives. Configurable because a sovereign
    /// deployment publishes its own, and because a test needs somewhere to
    /// point.
    pub openapi_endpoint: String,
    /// Where extracted messages go.
    pub message_tx: Option<mpsc::UnboundedSender<IncomingMessage>>,
}

impl std::fmt::Debug for DingTalkConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The secret is deliberately not printed.
        f.debug_struct("DingTalkConfig")
            .field("client_id", &self.client_id)
            .field("openapi_endpoint", &self.openapi_endpoint)
            .field("message_tx", &self.message_tx.is_some())
            .finish()
    }
}

impl DingTalkConfig {
    /// Configuration for one app, against the public gateway.
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            openapi_endpoint: DEFAULT_OPENAPI_ENDPOINT.to_string(),
            message_tx: None,
        }
    }
}

/// Now, in the milliseconds-since-epoch the platform timestamps with.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The host's outbound-facing address.
///
/// The handshake asks for it. The UDP socket is never sent on — connecting it
/// only makes the kernel pick a route, which is what reveals the address.
fn local_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("8.8.8.8:80")?;
            socket.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "0.0.0.0".to_string())
}

/// The reply address a message carried, and when it stops working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWebhook {
    /// Where a reply is posted.
    pub url: String,
    /// When it expires, in milliseconds since the epoch. Zero means the
    /// platform said nothing, which is read as "no deadline given".
    pub expires_at_ms: u64,
}

impl SessionWebhook {
    /// Whether this has expired, or is close enough that a reply might not
    /// land.
    fn is_spent(&self) -> bool {
        self.expires_at_ms > 0
            && now_ms() + WEBHOOK_SAFETY_MARGIN.as_millis() as u64 >= self.expires_at_ms
    }
}

/// A reply URL, rebuilt in a canonical form, or `None` when it is not one this
/// channel will post to.
pub fn validate_webhook_url(raw: &str) -> Option<String> {
    let parsed = url::Url::parse(raw).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }

    let host = parsed.host_str()?;
    if !ALLOWED_WEBHOOK_HOSTS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(host))
    {
        return None;
    }

    // A session webhook is scheme, host and a path on the default port — and
    // nothing else, *except* the query, which is not decoration: the session it
    // authorises the reply with travels as `?session=…`, so refusing a query
    // would refuse every genuine reply URL. That is worth stating because it is
    // exactly the rule Teams' connector guard uses, and copying it here was the
    // first thing this function did.
    if parsed.port().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }

    Some(raw.to_string())
}

/// A message extracted from a DingTalk callback frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DingTalkIncoming {
    /// The platform's id for this message — the dedup key, and what the
    /// acknowledgement has to echo.
    pub message_id: String,
    /// The conversation to answer in.
    pub conversation_id: String,
    /// Whether that conversation is a group.
    pub is_group: bool,
    /// Whether the message named the bot.
    pub mentioned: bool,
    /// The sender, preferring their staff id — it is stable across the
    /// organisation, where the plain sender id may not be.
    pub user_id: String,
    /// The message text.
    pub text: String,
    /// Where to reply, when the message carried somewhere to reply to.
    pub reply_to: Option<SessionWebhook>,
}

/// Extract a message from a Stream Mode frame, or `None` when it is not one.
///
/// The envelope carries a `data` field that is *itself a JSON string*, which is
/// the part worth being careful about: a frame that parses once is not yet
/// parsed. Everything that is not a text message addressed to this bot —
/// lifecycle notices, the card topic, the other message types — is dropped.
pub fn parse_callback(raw: &str) -> crate::Result<Option<DingTalkIncoming>> {
    #[derive(Deserialize, Default)]
    struct Headers {
        #[serde(rename = "messageId", default)]
        message_id: Option<String>,
        #[serde(default)]
        topic: Option<String>,
    }

    #[derive(Deserialize)]
    struct Envelope {
        #[serde(rename = "type", default)]
        kind: Option<String>,
        #[serde(default)]
        headers: Headers,
        /// A JSON document, as a string.
        #[serde(default)]
        data: Option<String>,
    }

    #[derive(Deserialize, Default)]
    struct TextContent {
        #[serde(default)]
        content: Option<String>,
    }

    #[derive(Deserialize, Default)]
    struct BotMessage {
        #[serde(rename = "msgId", default)]
        msg_id: Option<String>,
        #[serde(rename = "conversationId", default)]
        conversation_id: Option<String>,
        #[serde(rename = "conversationType", default)]
        conversation_type: Option<String>,
        #[serde(rename = "senderId", default)]
        sender_id: Option<String>,
        #[serde(rename = "senderStaffId", default)]
        sender_staff_id: Option<String>,
        #[serde(rename = "isInAtList", default)]
        is_in_at_list: Option<bool>,
        #[serde(rename = "msgtype", default)]
        msg_type: Option<String>,
        #[serde(default)]
        text: Option<TextContent>,
        #[serde(rename = "sessionWebhook", default)]
        session_webhook: Option<String>,
        #[serde(rename = "sessionWebhookExpiredTime", default)]
        session_webhook_expires: Option<u64>,
    }

    let envelope: Envelope = serde_json::from_str(raw)
        .map_err(|e| SyscityError::Validation(format!("Invalid DingTalk frame: {}", e)))?;

    if envelope.kind.as_deref() != Some(TYPE_CALLBACK) {
        return Ok(None);
    }
    if envelope.headers.topic.as_deref() != Some(TOPIC_BOT_MESSAGE) {
        return Ok(None);
    }

    let Some(data) = envelope.data else {
        return Ok(None);
    };
    let Ok(message) = serde_json::from_str::<BotMessage>(&data) else {
        warn!("DingTalk: the frame's data is not a message");
        return Ok(None);
    };

    // The frame's header id is the fallback, and is what the acknowledgement
    // echoes; the message's own id is what dedup uses.
    let message_id = message
        .msg_id
        .or(envelope.headers.message_id)
        .filter(|id| !id.is_empty());
    let Some(message_id) = message_id else {
        return Ok(None);
    };

    if message.msg_type.as_deref() != Some("text") {
        return Ok(None);
    }
    let Some(text) = message.text.and_then(|t| t.content) else {
        return Ok(None);
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        return Ok(None);
    }

    let Some(conversation_id) = message.conversation_id.filter(|id| !id.is_empty()) else {
        return Ok(None);
    };

    let user_id = message
        .sender_staff_id
        .filter(|id| !id.is_empty())
        .or(message.sender_id)
        .filter(|id| !id.is_empty());
    let Some(user_id) = user_id else {
        return Ok(None);
    };

    // `"2"` is a group; everything else the platform sends is a one-to-one
    // conversation, which is how both reference implementations read it.
    let is_group = message.conversation_type.as_deref() == Some("2");

    // The reply URL is checked here rather than at send time, so a message that
    // arrived with somewhere unusable to reply to is still a turn — it simply
    // has no address, which the send path reports.
    let reply_to = message.session_webhook.and_then(|url| {
        if validate_webhook_url(&url).is_none() {
            warn!("DingTalk: ignoring a reply URL that is not a session webhook");
            return None;
        }
        Some(SessionWebhook {
            url,
            expires_at_ms: message.session_webhook_expires.unwrap_or(0),
        })
    });

    Ok(Some(DingTalkIncoming {
        message_id,
        conversation_id,
        is_group,
        // A one-to-one conversation is addressed to the bot by definition; in a
        // group the platform says so structurally rather than in the text.
        mentioned: !is_group || message.is_in_at_list.unwrap_or(false),
        user_id,
        text,
        reply_to,
    }))
}

/// The acknowledgement a delivery has to be answered with.
fn acknowledgement(message_id: &str) -> String {
    serde_json::json!({
        "code": ACK_OK,
        "headers": { "messageId": message_id, "contentType": "application/json" },
        "message": "",
        // The reference serialises this field as a *string*, not an object.
        "data": serde_json::json!({ "response": "OK" }).to_string(),
    })
    .to_string()
}

/// What the handshake hands back.
#[derive(Debug, Deserialize)]
struct OpenConnection {
    /// The socket to open.
    endpoint: String,
    /// The one-shot credential for it.
    ticket: String,
}

/// DingTalk Stream Mode channel implementation.
pub struct DingTalkChannel {
    config: DingTalkConfig,
    running: Arc<AtomicBool>,
    /// Cancelled by `stop()`, so a stop does not wait for the next frame.
    shutdown: CancellationToken,
    http: reqwest::Client,
    /// conversation id to where a reply goes, learned from the messages that
    /// arrive — each carrying its own, expiring, URL.
    replies: Arc<RwLock<HashMap<String, SessionWebhook>>>,
    /// The platform redelivers anything it was not acknowledged for.
    seen: Arc<ReplayGuard>,
}

impl std::fmt::Debug for DingTalkChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DingTalkChannel")
            .field("config", &self.config)
            .field("running", &self.running)
            .finish()
    }
}

impl DingTalkChannel {
    /// Create a new DingTalk channel.
    pub fn new(config: DingTalkConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REPLY_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            shutdown: CancellationToken::new(),
            http,
            replies: Arc::new(RwLock::new(HashMap::new())),
            seen: Arc::new(ReplayGuard::default()),
        }
    }

    /// One connection, from the handshake to the moment the socket drops.
    async fn serve(&self) -> crate::Result<()> {
        let connection = self.open_connection().await?;

        // The ticket is a query parameter, not a header and not a frame.
        //
        // An endpoint with no path has to gain the root first: `wss://host` with
        // a query appended makes the client build a request line with no target
        // at all, which the peer rejects as a malformed request — an error that
        // says nothing about the ticket it is really about.
        let base = connection.endpoint.trim_end_matches('/');
        let base = if base
            .split("://")
            .nth(1)
            .is_some_and(|authority_and_path| authority_and_path.contains('/'))
        {
            base.to_string()
        } else {
            format!("{}/", base)
        };
        let uri = format!("{}?ticket={}", base, urlencoding::encode(&connection.ticket));

        let (socket, _) =
            tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(&uri))
                .await
                .map_err(|_| self.failed("the socket timed out"))?
                .map_err(|e| self.failed(&format!("the socket failed: {}", e)))?;

        let (mut write, mut read) = socket.split();
        info!("DingTalk channel: connected (client {})", self.config.client_id);

        loop {
            // Two branches, and only one of them writes.
            //
            // The stop has to be its own branch rather than a check after the
            // read: a check would only be reached once a frame arrived, so
            // `stop()` would wait out the whole keepalive interval — which is
            // the flaw this channel is meant not to repeat. The read and the
            // keepalive share the other branch because the socket has one
            // writer, and two branches that both wrote would not borrow.
            tokio::select! {
                _ = self.shutdown.cancelled() => break,

                next = tokio::time::timeout(KEEPALIVE_INTERVAL, read.next()) => {
                    match next {
                        Ok(Some(Ok(Message::Text(text)))) => {
                            if let Some(ack) = self.handle_frame(&text).await {
                                write.send(Message::text(ack)).await.map_err(|e| {
                                    self.failed(&format!("the acknowledgement failed: {}", e))
                                })?;
                            }
                        }
                        Ok(Some(Ok(Message::Close(_)))) => {
                            info!("DingTalk channel: the server closed the connection");
                            break;
                        }
                        Ok(Some(Ok(_))) => {}
                        Ok(Some(Err(e))) => {
                            return Err(self.failed(&format!("the socket errored: {}", e)));
                        }
                        Ok(None) => break,
                        // Nothing has arrived for a while. The keepalive is the
                        // WebSocket's own ping, which is what the reference
                        // sends; the platform answering it is what proves the
                        // socket is still there.
                        Err(_) => {
                            write.send(Message::Ping(Vec::new())).await.map_err(|e| {
                                self.failed(&format!("the keepalive failed: {}", e))
                            })?;
                        }
                    }
                }
            }
        }

        let _ = write.send(Message::Close(None)).await;
        Ok(())
    }

    /// Ask the platform for a socket, and get the ticket for it.
    ///
    /// Stream Mode is not reachable without this: the endpoint is not a
    /// constant and the ticket is one-shot, so every reconnect starts here, and
    /// it is also the only place a bad credential is reported.
    async fn open_connection(&self) -> crate::Result<OpenConnection> {
        let body = serde_json::json!({
            "clientId": self.config.client_id,
            "clientSecret": self.config.client_secret,
            "subscriptions": [{ "type": "CALLBACK", "topic": TOPIC_BOT_MESSAGE }],
            "ua": "syscity/1.0",
            "localIp": local_ip(),
        });

        let response = self
            .http
            .post(format!("{}{}", self.config.openapi_endpoint, OPEN_CONNECTION_PATH))
            .header("Accept", "application/json")
            .json(&body)
            .timeout(HANDSHAKE_TIMEOUT)
            .send()
            .await
            .map_err(|e| self.failed(&format!("the handshake failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            // The body names the cause — a bad app key, a robot that was never
            // enabled — which is the difference between a typo and a missing
            // capability.
            let detail = response.text().await.unwrap_or_default();
            return Err(self.failed(&format!("the handshake returned {}: {}", status, detail)));
        }

        response
            .json::<OpenConnection>()
            .await
            .map_err(|e| self.failed(&format!("the handshake answered with no socket: {}", e)))
    }

    /// Deal with one frame, returning the acknowledgement it wants — if any.
    async fn handle_frame(&self, raw: &str) -> Option<String> {
        let Ok(envelope) = serde_json::from_str::<serde_json::Value>(raw) else {
            warn!("DingTalk channel: ignoring a frame that is not JSON");
            return None;
        };

        let kind = envelope
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or_default();
        let topic = envelope
            .get("headers")
            .and_then(|h| h.get("topic"))
            .and_then(|t| t.as_str())
            .unwrap_or_default();

        // The platform taking the socket away. There is nothing to acknowledge;
        // the loop reconnects.
        if kind == TYPE_SYSTEM {
            if topic == TOPIC_DISCONNECT {
                info!("DingTalk channel: the platform asked for the connection to close");
            }
            return None;
        }

        let incoming = match parse_callback(raw) {
            Ok(Some(incoming)) => incoming,
            Ok(None) => return None,
            Err(e) => {
                warn!("DingTalk channel: {}", e);
                return None;
            }
        };

        // Acknowledge before anything else happens. The acknowledgement must
        // not wait for an agent turn — the reference notes that blocking here
        // stalls the connection's own keepalive — and an unacknowledged
        // delivery is redelivered, which is why dedup exists below.
        let ack = acknowledgement(&incoming.message_id);

        if self.shutdown.is_cancelled() {
            return Some(ack);
        }

        // The platform redelivers; answering the same message twice is worse
        // than dropping it.
        if !self.seen.first_sighting(incoming.message_id.as_bytes()) {
            debug!("DingTalk channel: duplicate delivery {} ignored", incoming.message_id);
            return Some(ack);
        }

        // Remember where a reply goes *before* the message moves on: the URL is
        // per message and expires, so the most recent one is the one to use.
        if let Some(webhook) = &incoming.reply_to {
            self.replies
                .write()
                .await
                .insert(incoming.conversation_id.clone(), webhook.clone());
        }

        let mention = if !incoming.is_group {
            MentionState::DirectMessage
        } else if incoming.mentioned {
            MentionState::Mentioned
        } else {
            MentionState::NotMentioned
        };

        if let Some(tx) = &self.config.message_tx {
            let message =
                IncomingMessage::new(&incoming.user_id, &incoming.conversation_id, &incoming.text)
                    .with_provenance(InputProvenance::ExternalUser {
                        channel: "dingtalk".to_string(),
                        is_direct: !incoming.is_group,
                    })
                    .with_mention(mention);

            if tx.send(message).is_err() {
                warn!("DingTalk channel: the inbound bridge is gone; dropping the message");
            }
        }

        Some(ack)
    }

    /// One error shape for everything the channel can fail at.
    fn failed(&self, reason: &str) -> SyscityError {
        SyscityError::ExternalService {
            source: format!("DingTalk: {}", reason),
            cause: None,
        }
    }
}

#[async_trait]
impl Channel for DingTalkChannel {
    fn name(&self) -> &str {
        "dingtalk"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            chat_types: vec![ChatType::Direct, ChatType::Group],
            // DingTalk renders a subset of markdown that excludes headings and
            // tables, so markdown sent literally would half-render; the
            // plain-text reduction is what the agent's markdown becomes safely.
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
        info!("Starting DingTalk channel...");
        self.running.store(true, Ordering::SeqCst);

        let mut failures = 0usize;
        while !self.shutdown.is_cancelled() {
            let started = Instant::now();
            match self.serve().await {
                Ok(()) if self.shutdown.is_cancelled() => break,
                Ok(()) => info!("DingTalk channel: the connection closed; reconnecting"),
                Err(e) => warn!("DingTalk channel: {}; reconnecting", e),
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
            info!("DingTalk channel: reconnecting in {}s", wait.as_secs());

            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = tokio::time::sleep(wait) => {}
            }
        }

        self.running.store(false, Ordering::SeqCst);
        info!("DingTalk channel stopped");
        Ok(())
    }

    async fn stop(&self) -> crate::Result<()> {
        info!("Stopping DingTalk channel...");
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

        // The reply address is the one the most recent message carried, and it
        // expires — a bot can only answer a conversation it has heard from
        // recently, which is this platform's own limit rather than this
        // channel's.
        let webhook = {
            let replies = self.replies.read().await;
            replies.get(&conversation_id).cloned()
        };
        let Some(webhook) = webhook else {
            return Err(SyscityError::Validation(format!(
                "DingTalk can only reply to a conversation it has heard from, and {} has sent \
                 nothing",
                conversation_id
            )));
        };

        if webhook.is_spent() {
            self.replies.write().await.remove(&conversation_id);
            return Err(
                self.failed(&format!("the reply window for {} has closed", conversation_id))
            );
        }

        // Re-checked on the way out rather than trusted from the way in: this is
        // where a request is actually sent to a URL that came in on a message.
        let Some(url) = validate_webhook_url(&webhook.url) else {
            return Err(self.failed(&format!("refusing to reply to {}", webhook.url)));
        };

        // DingTalk caps a message at 20000 characters, so a longer answer
        // arrives as consecutive messages rather than being refused outright.
        for segment in split_message(&text, MAX_DINGTALK_CHARS) {
            self.post_markdown(&url, &segment).await?;
        }

        debug!("DingTalk message posted to {}", conversation_id);
        Ok(Id::new())
    }

    async fn send_typing(&self, _conversation_id: &ConversationId) -> crate::Result<()> {
        // DingTalk has no typing indicator for a bot.
        Ok(())
    }

    async fn edit_message(&self, _message_id: Id, _new_content: String) -> crate::Result<()> {
        Err(SyscityError::Unsupported("DingTalk messages cannot be edited from here".into()))
    }

    async fn delete_message(&self, _message_id: Id) -> crate::Result<()> {
        Err(SyscityError::Unsupported(
            "DingTalk messages cannot be deleted from here".into(),
        ))
    }

    async fn health_check(&self) -> crate::Result<bool> {
        Ok(self.running.load(Ordering::SeqCst))
    }
}

impl DingTalkChannel {
    /// Post one markdown message to a session webhook.
    async fn post_markdown(&self, url: &str, text: &str) -> crate::Result<()> {
        let response = self
            .http
            .post(url)
            .json(&serde_json::json!({
                "msgtype": "markdown",
                "markdown": { "title": "Syscity", "text": text },
            }))
            .send()
            .await
            .map_err(|e| self.failed(&format!("the reply failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let detail = response.text().await.unwrap_or_default();
            return Err(self.failed(&format!("the reply returned {}: {}", status, detail)));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT: &str = "cid-1";

    fn frame(kind: &str, topic: &str, message_id: &str, data: serde_json::Value) -> String {
        serde_json::json!({
            "specVersion": "1.0",
            "type": kind,
            "headers": { "messageId": message_id, "topic": topic, "contentType": "application/json" },
            "data": data.to_string(),
        })
        .to_string()
    }

    fn bot_message(body: serde_json::Value) -> String {
        frame(TYPE_CALLBACK, TOPIC_BOT_MESSAGE, "hdr-1", body)
    }

    fn text_message(content: &str, conversation_type: &str) -> String {
        bot_message(serde_json::json!({
            "msgId": "msg-1",
            "conversationId": CHAT,
            "conversationType": conversation_type,
            "senderId": "sender-1",
            "senderStaffId": "staff-1",
            "msgtype": "text",
            "text": { "content": content },
            "sessionWebhook": "https://api.dingtalk.com/robot/send?session=x",
            "sessionWebhookExpiredTime": now_ms() + 3_600_000,
        }))
    }

    #[test]
    fn a_direct_message_parses_out_of_a_doubly_encoded_frame() {
        let parsed = parse_callback(&text_message("hello there", "1"))
            .expect("the frame parses")
            .expect("it is a message");

        assert_eq!(parsed.message_id, "msg-1");
        assert_eq!(parsed.conversation_id, CHAT);
        assert_eq!(parsed.user_id, "staff-1", "the stable staff id is preferred");
        assert_eq!(parsed.text, "hello there");
        assert!(!parsed.is_group);
        assert!(parsed.mentioned, "a one-to-one conversation is addressed to the bot");
        assert!(parsed.reply_to.is_some(), "the message carried somewhere to reply to");
    }

    #[test]
    fn a_sender_without_a_staff_id_falls_back_to_their_sender_id() {
        let body = serde_json::json!({
            "msgId": "msg-1",
            "conversationId": CHAT,
            "conversationType": "1",
            "senderId": "sender-1",
            "msgtype": "text",
            "text": { "content": "hi" },
        });

        let parsed = parse_callback(&bot_message(body))
            .expect("parses")
            .expect("it is a message");
        assert_eq!(parsed.user_id, "sender-1");
        assert_eq!(parsed.reply_to, None, "no reply URL was carried");
    }

    /// The mention is a structured field, not something to scrape out of the
    /// text — and whether it is there decides whether a group message is a turn
    /// at all.
    #[test]
    fn a_group_message_reports_its_mention_structurally() {
        let mentioned = parse_callback(&bot_message(serde_json::json!({
            "msgId": "msg-1",
            "conversationId": CHAT,
            "conversationType": "2",
            "senderId": "sender-1",
            "msgtype": "text",
            "text": { "content": "@Bot can you check this" },
            "isInAtList": true,
        })))
        .expect("parses")
        .expect("it is a message");

        assert!(mentioned.is_group);
        assert!(mentioned.mentioned);
        assert_eq!(
            mentioned.text, "@Bot can you check this",
            "the text is left alone; the platform reports the mention separately"
        );

        let unmentioned = parse_callback(&bot_message(serde_json::json!({
            "msgId": "msg-2",
            "conversationId": CHAT,
            "conversationType": "2",
            "senderId": "sender-1",
            "msgtype": "text",
            "text": { "content": "just talking" },
            "isInAtList": false,
        })))
        .expect("parses")
        .expect("it is a message");

        assert!(!unmentioned.mentioned);
    }

    #[test]
    fn everything_that_is_not_a_text_message_is_dropped() {
        // The other frame types and topics on the same socket.
        assert_eq!(
            parse_callback(&frame(TYPE_SYSTEM, TOPIC_DISCONNECT, "hdr-1", serde_json::json!({})))
                .expect("parses"),
            None
        );
        assert_eq!(
            parse_callback(&frame(
                TYPE_CALLBACK,
                "/v1.0/card/instances/callback",
                "hdr-1",
                serde_json::json!({})
            ))
            .expect("parses"),
            None
        );

        // The other message types.
        for msg_type in ["picture", "richText", "audio", "file", "video", "card"] {
            let body = serde_json::json!({
                "msgId": "msg-1",
                "conversationId": CHAT,
                "conversationType": "1",
                "senderId": "sender-1",
                "msgtype": msg_type,
            });
            assert_eq!(
                parse_callback(&bot_message(body)).expect("parses"),
                None,
                "{msg_type} has no text this channel can act on"
            );
        }

        // Empty text, and no sender to attribute it to.
        assert_eq!(parse_callback(&text_message("   ", "1")).expect("parses"), None);
        assert_eq!(
            parse_callback(&bot_message(serde_json::json!({
                "msgId": "msg-1",
                "conversationId": CHAT,
                "msgtype": "text",
                "text": { "content": "hi" },
            })))
            .expect("parses"),
            None
        );
    }

    /// A message whose `data` is not a message is dropped rather than treated
    /// as an empty one.
    #[test]
    fn a_frame_whose_data_is_not_a_message_is_dropped() {
        assert_eq!(
            parse_callback(&frame(
                TYPE_CALLBACK,
                TOPIC_BOT_MESSAGE,
                "hdr-1",
                serde_json::json!([])
            ))
            .expect("parses"),
            None
        );
        assert!(parse_callback("not json").is_err());
    }

    #[test]
    fn a_message_with_no_id_falls_back_to_its_frame_header() {
        let body = serde_json::json!({
            "conversationId": CHAT,
            "conversationType": "1",
            "senderId": "sender-1",
            "msgtype": "text",
            "text": { "content": "hi" },
        });

        let parsed = parse_callback(&bot_message(body))
            .expect("parses")
            .expect("it is a message");
        assert_eq!(parsed.message_id, "hdr-1");
    }

    /// The acknowledgement has to echo the delivery's id, or the platform
    /// treats it as unacknowledged and sends the message again.
    #[test]
    fn the_acknowledgement_echoes_the_message_id() {
        let ack: serde_json::Value =
            serde_json::from_str(&acknowledgement("msg-1")).expect("the ack is JSON");

        assert_eq!(ack["code"], ACK_OK);
        assert_eq!(ack["headers"]["messageId"], "msg-1");
        assert_eq!(ack["headers"]["contentType"], "application/json");
        assert!(
            ack["data"].is_string(),
            "the reference serialises this field as a string, not an object"
        );
    }

    /// The reply URL arrives inside the message, so it is a value the sender
    /// influences and the gateway now sends a request to.
    #[test]
    fn a_reply_url_is_held_to_the_allowlist() {
        for allowed in [
            "https://api.dingtalk.com/robot/send?session=x",
            "https://oapi.dingtalk.com/robot/sendBySession?session=y",
        ] {
            assert!(validate_webhook_url(allowed).is_some(), "{allowed}");
        }

        for refused in [
            "https://evil.example/robot/send",
            "https://api.dingtalk.com.evil.example/robot/send",
            "https://evil-api.dingtalk.com/robot/send",
            "http://api.dingtalk.com/robot/send",
            "https://api.dingtalk.com:8443/robot/send",
            "https://user:pass@api.dingtalk.com/robot/send",
            "https://api.dingtalk.com/robot/send#fragment",
            "not a url",
            "",
        ] {
            assert_eq!(validate_webhook_url(refused), None, "{refused} must be refused");
        }
    }

    /// The URL is compared exactly, so a suffix rule cannot be abused — the
    /// same property Teams' connector allowlist has.
    #[test]
    fn a_reply_url_that_is_not_a_session_webhook_is_discarded_at_the_door() {
        let body = serde_json::json!({
            "msgId": "msg-1",
            "conversationId": CHAT,
            "conversationType": "1",
            "senderId": "sender-1",
            "msgtype": "text",
            "text": { "content": "hi" },
            "sessionWebhook": "https://evil.example/robot/send",
        });

        let parsed = parse_callback(&bot_message(body))
            .expect("parses")
            .expect("it is still a message");
        assert_eq!(
            parsed.reply_to, None,
            "a message with nowhere safe to reply to is still a turn"
        );
    }

    #[test]
    fn a_session_webhook_knows_when_it_has_spent_itself() {
        let live = SessionWebhook {
            url: "https://api.dingtalk.com/robot/send?session=x".to_string(),
            expires_at_ms: now_ms() + 3_600_000,
        };
        assert!(!live.is_spent());

        // Inside the safety margin, so a reply might not land.
        let closing = SessionWebhook {
            expires_at_ms: now_ms() + 60_000,
            ..live.clone()
        };
        assert!(closing.is_spent());

        // The platform saying nothing is not a deadline.
        let undated = SessionWebhook { expires_at_ms: 0, ..live };
        assert!(!undated.is_spent());
    }

    /// The channel's own `Debug` must not carry the app secret.
    #[test]
    fn the_debug_rendering_does_not_leak_the_secret() {
        let channel = DingTalkChannel::new(DingTalkConfig::new("client-1", "super-secret-value"));

        let rendered = format!("{:?}", channel);
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
    }

    /// A conversation the channel has never heard from has nowhere to be
    /// answered.
    #[tokio::test]
    async fn sending_to_an_unknown_conversation_is_refused() {
        let channel = DingTalkChannel::new(DingTalkConfig::new("client-1", "secret"));

        let error = channel
            .send(OutgoingMessage::new(ConversationId::new("never-seen"), "hello"))
            .await
            .expect_err("a conversation that has sent nothing cannot be answered");

        assert!(error.to_string().contains("never-seen"), "{error}");
    }
}
