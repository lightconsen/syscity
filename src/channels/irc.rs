//! IRC channel.
//!
//! The third connection channel, and the only one that is not a WebSocket: IRC
//! is a plain line protocol over a TCP socket, so this speaks it directly rather
//! than through a library. As with the other two, the agent-facing half is
//! shared — inbound goes through
//! [`dispatch_channel_message_to`](crate::gateway::webhooks::dispatch_channel_message_to).
//!
//! Four things shape it:
//!
//! - **The limit is bytes, not characters.** A line is capped at 510 bytes
//!   *including* the command that carries it, so how much of a reply fits
//!   depends on the length of the name it is addressed to. The budget is
//!   computed per recipient, and cuts land on character boundaries.
//! - **A reply is a `PRIVMSG`, and text is a protocol surface.** A newline in a
//!   reply would start a second IRC command, so carriage returns, newlines and
//!   NULs are stripped from anything sent. That is an injection guard, not
//!   tidying.
//! - **A channel message is addressed, not merely received.** In a channel the
//!   bot is named (`nick:`, `nick,` or `nick `) and the name is stripped; a
//!   message that does not name it is reported as unmentioned rather than
//!   dropped, so `require_mention` — not this channel — decides whether the
//!   agent answers it.
//! - **The nickname is not necessarily the one asked for.** A server that
//!   already has it hands out `nick_`, `nick_1`, … and that confirmed name is
//!   what a channel message has to be addressed to.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::channels::{
    split_message_bytes, Channel, ChannelCapabilities, ChatType, ConversationId, IncomingMessage,
    InputProvenance, MentionState, OutgoingMessage, PlainTextFormatter,
};
use crate::core::models::Id;
use crate::error::SyscityError;

/// The port the reference dials by default, which is IRC's TLS port.
const DEFAULT_PORT: u16 = 6697;

/// The plaintext port. Only reached when TLS is switched off deliberately.
pub const PLAINTEXT_PORT: u16 = 6667;

/// How long registration may take before the connection is given up on.
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(30);

/// The most bytes one IRC line may occupy, including its CRLF.
const MAX_LINE_BYTES: usize = 510;

/// How long to wait between the lines of one reply.
///
/// IRC servers drop a client that talks too fast, and an agent's answer can be
/// several lines.
const FLOOD_DELAY: Duration = Duration::from_millis(300);

/// Backoff between reconnects, indexed by consecutive failures.
const BACKOFF_SECS: [u64; 5] = [2, 5, 10, 30, 60];

/// How many lines may be queued for the socket before a sender is told to slow
/// down.
const OUTBOUND_QUEUE: usize = 128;

/// IRC channel configuration.
#[derive(Clone)]
pub struct IrcConfig {
    /// The server to dial.
    pub server: String,
    /// The port to dial it on.
    pub port: u16,
    /// The nickname to ask for, which the server may amend.
    pub nickname: String,
    /// The channel to join. Without one the channel still works, but only for
    /// direct messages.
    pub channel: String,
    /// Whether to dial with TLS. On by default, as the reference has it.
    pub use_tls: bool,
    /// A password for the server itself, before registration.
    pub server_password: Option<String>,
    /// A NickServ password, sent once registration completes.
    pub nickserv_password: Option<String>,
    /// Where extracted messages go.
    pub message_tx: Option<mpsc::UnboundedSender<IncomingMessage>>,
}

impl std::fmt::Debug for IrcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The passwords are deliberately not printed.
        f.debug_struct("IrcConfig")
            .field("server", &self.server)
            .field("port", &self.port)
            .field("nickname", &self.nickname)
            .field("channel", &self.channel)
            .field("use_tls", &self.use_tls)
            .field("server_password", &self.server_password.is_some())
            .field("nickserv_password", &self.nickserv_password.is_some())
            .finish()
    }
}

impl IrcConfig {
    /// Configuration for one network, joining `channel`.
    pub fn new(
        server: impl Into<String>,
        nickname: impl Into<String>,
        channel: impl Into<String>,
    ) -> Self {
        Self {
            server: server.into(),
            port: DEFAULT_PORT,
            nickname: nickname.into(),
            channel: channel.into(),
            use_tls: true,
            server_password: None,
            nickserv_password: None,
            message_tx: None,
        }
    }

    /// The room left for a message, once the command that carries it is paid
    /// for.
    ///
    /// IRC counts the whole line, so a long channel name leaves less room than a
    /// short nickname does — which is why this is a function of the recipient
    /// rather than a constant.
    fn budget_for(&self, target: &str) -> usize {
        let overhead = format!("PRIVMSG {} :", target).len() + 2; // + CRLF
        MAX_LINE_BYTES.saturating_sub(overhead).max(1)
    }
}

/// One line of IRC, split into its parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrcLine {
    /// `nick!user@host`, or empty when the server sent none.
    pub prefix: String,
    /// The command or numeric.
    pub command: String,
    /// The parameters, with the trailing one's leading `:` removed.
    pub params: Vec<String>,
}

/// Split a raw line into prefix, command and parameters.
///
/// The trailing parameter is the one after ` :` — it may contain spaces, which
/// is why a message's text arrives in one piece.
pub fn parse_line(raw: &str) -> IrcLine {
    let (prefix, rest) = match raw.strip_prefix(':') {
        Some(rest) => match rest.split_once(' ') {
            Some((prefix, rest)) => (prefix.to_string(), rest),
            None => (rest.to_string(), ""),
        },
        None => (String::new(), raw),
    };

    let (head, trailing) = match rest.split_once(" :") {
        Some((head, trailing)) => (head, Some(trailing)),
        None => (rest, None),
    };

    let mut params: Vec<String> = head
        .split_whitespace()
        .skip(1)
        .map(str::to_string)
        .collect();
    if let Some(trailing) = trailing {
        params.push(trailing.to_string());
    }

    IrcLine {
        prefix,
        command: head
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string(),
        params,
    }
}

/// The nickname a prefix belongs to (`nick!user@host`).
pub fn nick_of(prefix: &str) -> &str {
    prefix.split('!').next().unwrap_or(prefix)
}

/// Neutralise the characters that would end a line, or a command.
///
/// A reply containing a newline would otherwise be read by the server as a
/// second IRC command — so this is an injection guard, not tidying.
fn sanitize(text: &str) -> String {
    text.replace(['\r', '\n'], " ").replace('\0', "")
}

/// Remove the mIRC formatting codes a sender may have wrapped their text in.
///
/// They are control characters, so leaving them in hands the agent text with
/// invisible noise. The colour codes are followed by digits that belong to
/// them, and those go too.
fn strip_formatting(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\u{2}' | '\u{1d}' | '\u{1f}' | '\u{16}' | '\u{0f}' => {}
            // Colour: `\x03` then up to two comma-separated numbers.
            '\u{3}' => {
                for _ in 0..2 {
                    let mut digits = 0;
                    while chars.peek().is_some_and(|c| c.is_ascii_digit()) && digits < 2 {
                        chars.next();
                        digits += 1;
                    }
                    if digits == 0 || chars.peek() != Some(&',') {
                        break;
                    }
                    chars.next();
                }
            }
            // Hex colour: `\x04` then six hex digits.
            '\u{4}' => {
                for _ in 0..6 {
                    if chars.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                        chars.next();
                    }
                }
            }
            _ => out.push(c),
        }
    }

    out
}

/// Whether `target` names a channel rather than a person.
///
/// `#` and `&` are the ordinary and local channel prefixes; `+` and `!` are the
/// modeless and safe ones, which behave the same way for this purpose.
pub fn is_channel(target: &str) -> bool {
    target.starts_with(['#', '&', '+', '!'])
}

/// A message extracted from a `PRIVMSG`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrcIncoming {
    /// The sender's nickname.
    pub user_id: String,
    /// The channel it was said in, or the sender when it was said to us.
    pub conversation_id: String,
    /// Whether it was said in a channel.
    pub is_group: bool,
    /// Whether it named the bot.
    pub mentioned: bool,
    /// The text, with a leading `nick:`-style address removed.
    pub text: String,
}

/// Extract a message from a line, or `None` when the line is not one for us.
///
/// The bot's own lines are dropped — answering them would make it talk to
/// itself — as is anything that is not a `PRIVMSG`: joins, parts, mode changes
/// and the numeric replies all arrive on the same connection.
pub fn parse_privmsg(line: &IrcLine, our_nick: &str) -> Option<IrcIncoming> {
    if line.command != "PRIVMSG" || line.params.len() < 2 {
        return None;
    }

    let sender = nick_of(&line.prefix).to_string();
    if sender.is_empty() || sender.eq_ignore_ascii_case(our_nick) {
        return None;
    }

    let mut text = line.params[1].clone();

    // A CTCP ACTION is `/me`, which reads as an emote rather than a message.
    if let Some(action) = text
        .strip_prefix('\u{1}')
        .and_then(|t| t.strip_suffix('\u{1}'))
    {
        let Some(action) = action.strip_prefix("ACTION ") else {
            // Any other CTCP — a version request, a ping — is not a message.
            return None;
        };
        text = format!("* {} {}", sender, action);
    } else if text.starts_with('\u{1}') {
        return None;
    }

    let text = strip_formatting(&text);
    let target = line.params[0].clone();
    let is_group = is_channel(&target);

    // In a channel, the bot has to be named. The name is stripped, but a
    // message that does not carry it is still reported — unmentioned — so that
    // `require_mention` is what decides, rather than this function.
    let (text, mentioned) = if is_group {
        strip_address(&text, our_nick)
    } else {
        (text.trim().to_string(), true)
    };
    if text.is_empty() {
        return None;
    }

    Some(IrcIncoming {
        user_id: sender.clone(),
        conversation_id: if is_group { target } else { sender },
        is_group,
        mentioned,
        text,
    })
}

/// Remove a leading `nick:`, `nick,` or `nick `, and say whether there was one.
fn strip_address(text: &str, our_nick: &str) -> (String, bool) {
    let trimmed = text.trim_start();
    let lowered = trimmed.to_lowercase();
    let ours = our_nick.to_lowercase();

    for suffix in [":", ",", " "] {
        let prefix = format!("{}{}", ours, suffix);
        if lowered.starts_with(&prefix) {
            // An address with nothing after it is not a message, and the empty
            // remainder is what drops it — reported as mentioned so the caller
            // can tell "said nothing" from "not addressed to us".
            return (trimmed[prefix.len()..].trim().to_string(), true);
        }
    }

    (trimmed.trim().to_string(), false)
}

/// A socket, whichever way it was dialled.
///
/// A trait of its own because a trait object cannot name two non-auto traits:
/// the read and write halves have to be one type whether TLS is underneath or
/// not.
trait Socket: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}

impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Socket for T {}

type Stream = Box<dyn Socket>;

/// IRC channel implementation.
pub struct IrcChannel {
    config: IrcConfig,
    running: Arc<AtomicBool>,
    /// Cancelled by `stop()`, so a stop does not wait for the next line.
    shutdown: CancellationToken,
    /// Lines for the socket to write. One channel for both the replies `send`
    /// produces and the `PONG`s the server's keepalive demands, because the
    /// socket has one writer.
    outbound_tx: mpsc::Sender<String>,
    /// Taken by `start`, which is the only thing that owns the socket.
    outbound_rx: tokio::sync::Mutex<Option<mpsc::Receiver<String>>>,
    /// The nickname the server actually gave us, which may not be the one that
    /// was asked for — and is what a channel message has to be addressed to.
    confirmed_nick: Arc<RwLock<String>>,
}

impl std::fmt::Debug for IrcChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrcChannel")
            .field("config", &self.config)
            .field("running", &self.running)
            .finish()
    }
}

impl IrcChannel {
    /// Create a new IRC channel.
    pub fn new(config: IrcConfig) -> Self {
        let (outbound_tx, outbound_rx) = mpsc::channel(OUTBOUND_QUEUE);

        Self {
            confirmed_nick: Arc::new(RwLock::new(config.nickname.clone())),
            config,
            running: Arc::new(AtomicBool::new(false)),
            shutdown: CancellationToken::new(),
            outbound_tx,
            outbound_rx: tokio::sync::Mutex::new(Some(outbound_rx)),
        }
    }

    /// One connection, from the socket to the moment it drops.
    async fn serve(&self, outbound_rx: &mut mpsc::Receiver<String>) -> crate::Result<()> {
        let stream = self.dial().await?;
        let (reader, writer) = tokio::io::split(stream);
        let mut lines = BufReader::new(reader);
        let mut socket = BufWriter::new(writer);

        let mut nick = self.register(&mut lines, &mut socket).await?;
        *self.confirmed_nick.write().await = nick.clone();

        if let Some(password) = &self.config.nickserv_password {
            self.write_line(&mut socket, &format!("PRIVMSG NickServ :IDENTIFY {}", password))
                .await?;
            // NickServ has to have the identification before the join is seen,
            // or a channel that requires it refuses us.
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if !self.config.channel.is_empty() {
            self.write_line(&mut socket, &format!("JOIN {}", self.config.channel))
                .await?;
        }
        info!(
            "IRC channel: on {}:{} as {}, in {}",
            self.config.server, self.config.port, nick, self.config.channel
        );

        let mut buffer = String::new();
        loop {
            // Two branches, and only one of them writes. The keepalive is not a
            // branch of its own: the server pings, and the answer is queued for
            // the write branch, which is what keeps this to one writer.
            tokio::select! {
                _ = self.shutdown.cancelled() => break,

                line = outbound_rx.recv() => {
                    let Some(line) = line else { break };
                    self.write_line(&mut socket, &line).await?;
                }

                read = lines.read_line(&mut buffer) => {
                    let read = read.map_err(|e| self.failed(&format!("the socket errored: {}", e)))?;
                    if read == 0 {
                        info!("IRC channel: the server closed the connection");
                        break;
                    }

                    let raw = std::mem::take(&mut buffer);
                    let line = parse_line(raw.trim_end_matches(['\r', '\n']));

                    match line.command.as_str() {
                        // The keepalive, which has to be answered on the socket
                        // or the server drops us.
                        "PING" => {
                            let token = line.params.first().cloned().unwrap_or_default();
                            self.queue(format!("PONG :{}", token));
                        }
                        "PRIVMSG" => self.handle_privmsg(&line, &nick).await,
                        // The server may take our nickname away and hand us
                        // another; that changes what addresses us.
                        "NICK" if nick_of(&line.prefix).eq_ignore_ascii_case(&nick) => {
                            if let Some(new) = line.params.first() {
                                nick = new.clone();
                                *self.confirmed_nick.write().await = new.clone();
                            }
                        }
                        // ERR_NICKNAMEINUSE, after registration: something took
                        // the name, so a new one is tried.
                        "433" => {
                            // Something took the name after we had it, so a new
                            // one is tried and that becomes what addresses us.
                            nick = next_nick(&nick);
                            self.queue(format!("NICK {}", nick));
                            *self.confirmed_nick.write().await = nick.clone();
                        }
                        _ => {}
                    }
                }
            }
        }

        // The socket is being closed either way; saying goodbye politely is
        // best-effort, and a failure here says nothing worth acting on.
        if let Err(e) = socket.write_all(b"QUIT :shutting down\r\n").await {
            debug!("IRC channel: could not say goodbye: {}", e);
        }
        if let Err(e) = socket.flush().await {
            debug!("IRC channel: could not flush the goodbye: {}", e);
        }
        Ok(())
    }

    /// Open the socket, with TLS unless it was switched off.
    async fn dial(&self) -> crate::Result<Stream> {
        let address = format!("{}:{}", self.config.server, self.config.port);
        let target =
            tokio::time::timeout(REGISTRATION_TIMEOUT, tokio::net::TcpStream::connect(&address))
                .await
                .map_err(|_| self.failed(&format!("connecting to {} timed out", address)))?
                .map_err(|e| self.failed(&format!("connecting to {} failed: {}", address, e)))?;

        if !self.config.use_tls {
            return Ok(Box::new(target));
        }

        let mut roots = tokio_rustls::rustls::RootCertStore::empty();
        let mut loaded = 0usize;
        for cert in rustls_native_certs::load_native_certs().certs {
            // One certificate the platform store cannot load is not a reason to
            // refuse every connection — but an empty store is, and it would
            // otherwise surface as a handshake failure that says nothing about
            // why.
            match roots.add(cert) {
                Ok(()) => loaded += 1,
                Err(e) => debug!("IRC channel: skipping an unusable root certificate: {}", e),
            }
        }
        if loaded == 0 {
            return Err(self.failed("the platform trust store held no usable certificates"));
        }

        let config = tokio_rustls::rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

        let name =
            tokio_rustls::rustls::pki_types::ServerName::try_from(self.config.server.clone())
                .map_err(|e| {
                    self.failed(&format!(
                        "{} is not a name a certificate can cover: {}",
                        self.config.server, e
                    ))
                })?;

        let stream = connector
            .connect(name, target)
            .await
            .map_err(|e| self.failed(&format!("the TLS handshake failed: {}", e)))?;

        Ok(Box::new(stream))
    }

    /// Send `PASS`, `NICK` and `USER`, and wait for the server to welcome us.
    ///
    /// The nickname asked for is not necessarily the one that comes back: a
    /// server whose name is taken amends it, and the server confirms the result
    /// in `001`. What a channel message has to be addressed to is that
    /// confirmed name.
    async fn register(
        &self,
        lines: &mut BufReader<tokio::io::ReadHalf<Stream>>,
        socket: &mut BufWriter<tokio::io::WriteHalf<Stream>>,
    ) -> crate::Result<String> {
        if let Some(password) = &self.config.server_password {
            self.write_line(socket, &format!("PASS {}", password))
                .await?;
        }
        self.write_line(socket, &format!("NICK {}", self.config.nickname))
            .await?;
        self.write_line(socket, &format!("USER {} 0 * :Syscity Agent", self.config.nickname))
            .await?;

        let mut nick = self.config.nickname.clone();
        let mut buffer = String::new();
        let deadline = tokio::time::Instant::now() + REGISTRATION_TIMEOUT;

        loop {
            let read = tokio::time::timeout_at(deadline, lines.read_line(&mut buffer))
                .await
                .map_err(|_| self.failed("the server never welcomed us"))?
                .map_err(|e| {
                    self.failed(&format!("the socket errored while registering: {}", e))
                })?;
            if read == 0 {
                return Err(self.failed("the server closed the connection while registering"));
            }

            let raw = std::mem::take(&mut buffer);
            let line = parse_line(raw.trim_end_matches(['\r', '\n']));

            match line.command.as_str() {
                // RPL_WELCOME: registration is complete, and the first
                // parameter is the nickname the server settled on.
                "001" => {
                    if let Some(confirmed) = line.params.first() {
                        nick = confirmed.clone();
                    }
                    return Ok(nick);
                }
                "PING" => {
                    let token = line.params.first().cloned().unwrap_or_default();
                    self.write_line(socket, &format!("PONG :{}", token)).await?;
                }
                // ERR_NICKNAMEINUSE: try the next spelling and keep waiting.
                "433" => {
                    nick = next_nick(&nick);
                    self.write_line(socket, &format!("NICK {}", nick)).await?;
                }
                _ => {}
            }
        }
    }

    /// Write one line, ending it properly and flushing.
    async fn write_line(
        &self,
        socket: &mut BufWriter<tokio::io::WriteHalf<Stream>>,
        line: &str,
    ) -> crate::Result<()> {
        socket
            .write_all(line.as_bytes())
            .await
            .map_err(|e| self.failed(&format!("the write failed: {}", e)))?;
        socket
            .write_all(b"\r\n")
            .await
            .map_err(|e| self.failed(&format!("the write failed: {}", e)))?;
        socket
            .flush()
            .await
            .map_err(|e| self.failed(&format!("the write failed: {}", e)))
    }

    /// Queue a line for the write branch.
    ///
    /// Never waits: this is called from the read branch, and the receiver it
    /// would be waiting on is owned by the same loop, so waiting here could not
    /// complete. A failure means the queue is already full of lines the socket
    /// has not taken, which is a connection on its way out anyway.
    fn queue(&self, line: String) {
        if let Err(e) = self.outbound_tx.try_send(line) {
            warn!("IRC channel: could not queue a line ({})", e);
        }
    }

    /// Hand one `PRIVMSG` on, if it is one for us.
    async fn handle_privmsg(&self, line: &IrcLine, our_nick: &str) {
        let Some(incoming) = parse_privmsg(line, our_nick) else {
            return;
        };

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

        let message =
            IncomingMessage::new(&incoming.user_id, &incoming.conversation_id, &incoming.text)
                .with_provenance(InputProvenance::ExternalUser {
                    channel: "irc".to_string(),
                    is_direct: !incoming.is_group,
                })
                .with_mention(mention);

        if tx.send(message).is_err() {
            warn!("IRC channel: the inbound bridge is gone; dropping the message");
        }
    }

    /// One error shape for everything the channel can fail at.
    fn failed(&self, reason: &str) -> SyscityError {
        SyscityError::ExternalService {
            source: format!("IRC: {}", reason),
            cause: None,
        }
    }
}

/// The next nickname to try when the one asked for is taken.
///
/// `bot` → `bot_` → `bot_1` → `bot_2`, matching the reference: the first retry
/// adds a bare underscore and later ones count.
fn next_nick(current: &str) -> String {
    // A trailing `_<n>` is the counter, and it is that which increments.
    if let Some((stem, digits)) = current.rsplit_once('_') {
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            let next = digits.parse::<u32>().unwrap_or(0).saturating_add(1);
            return format!("{}_{}", stem, next);
        }
    }

    // The first collision adds a bare underscore; a name that already ends in
    // one gains the counter. Without the second case a name ending in `_` would
    // be retried as itself, for ever.
    if current.ends_with('_') {
        format!("{}1", current)
    } else {
        format!("{}_", current)
    }
}

#[async_trait]
impl Channel for IrcChannel {
    fn name(&self) -> &str {
        "irc"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            chat_types: vec![ChatType::Direct, ChatType::Group],
            // IRC has no formatting at all — the control codes it does have are
            // colour, not structure — so markdown is stripped rather than sent.
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
            return Err(SyscityError::Internal("IRC channel is already running".into()));
        };

        info!("Starting IRC channel...");
        self.running.store(true, Ordering::SeqCst);

        let mut failures = 0usize;
        while !self.shutdown.is_cancelled() {
            let started = Instant::now();
            match self.serve(&mut outbound_rx).await {
                Ok(()) if self.shutdown.is_cancelled() => break,
                Ok(()) => info!("IRC channel: the connection closed; reconnecting"),
                Err(e) => warn!("IRC channel: {}; reconnecting", e),
            }

            if self.shutdown.is_cancelled() {
                break;
            }

            // A connection that lasted is a healthy one: only one that failed
            // straight away counts towards the backoff.
            if started.elapsed() >= Duration::from_secs(30) {
                failures = 0;
            }
            let wait = Duration::from_secs(BACKOFF_SECS[failures.min(BACKOFF_SECS.len() - 1)]);
            failures += 1;
            info!("IRC channel: reconnecting in {}s", wait.as_secs());

            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = tokio::time::sleep(wait) => {}
            }
        }

        self.running.store(false, Ordering::SeqCst);
        info!("IRC channel stopped");
        Ok(())
    }

    async fn stop(&self) -> crate::Result<()> {
        info!("Stopping IRC channel...");
        self.shutdown.cancel();
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        let target = message.conversation_id.0.clone();

        let text = match &message.formatted_content {
            Some(crate::channels::FormattedContent::Markdown(md)) => {
                PlainTextFormatter::strip_markdown(md)
            }
            _ => PlainTextFormatter::strip_markdown(&message.content),
        };

        // The budget belongs to the whole line, so it depends on how long the
        // name being addressed is.
        let budget = self.config.budget_for(&target);
        let segments = split_message_bytes(&text, budget);
        let segments = if segments.is_empty() {
            vec![String::new()]
        } else {
            segments
        };

        for (at, segment) in segments.iter().enumerate() {
            self.outbound_tx
                .send(format!("PRIVMSG {} :{}", target, sanitize(segment)))
                .await
                .map_err(|_| self.failed("the connection is not running"))?;

            // IRC servers drop a client that talks too fast.
            if at + 1 < segments.len() {
                tokio::time::sleep(FLOOD_DELAY).await;
            }
        }

        debug!("IRC message posted to {}", target);
        Ok(Id::new())
    }

    async fn send_typing(&self, _conversation_id: &ConversationId) -> crate::Result<()> {
        // IRC has no typing indicator.
        Ok(())
    }

    async fn edit_message(&self, _message_id: Id, _new_content: String) -> crate::Result<()> {
        Err(SyscityError::Unsupported("IRC messages cannot be edited from here".into()))
    }

    async fn delete_message(&self, _message_id: Id) -> crate::Result<()> {
        Err(SyscityError::Unsupported("IRC messages cannot be deleted from here".into()))
    }

    async fn health_check(&self) -> crate::Result<bool> {
        Ok(self.running.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The nickname and channel these tests pretend to be.
    const NICK: &str = "syscity-bot";
    const CHANNEL: &str = "#chan";

    #[test]
    fn a_line_splits_into_its_parts() {
        let line = parse_line(":alice!u@h PRIVMSG #chan :hello there");
        assert_eq!(line.prefix, "alice!u@h");
        assert_eq!(line.command, "PRIVMSG");
        assert_eq!(line.params, vec!["#chan".to_string(), "hello there".to_string()]);

        // A server line has no prefix, and a numeric has several parameters.
        let numeric = parse_line("001 syscity-bot :Welcome to IRC");
        assert_eq!(numeric.prefix, "");
        assert_eq!(numeric.command, "001");
        assert_eq!(numeric.params, vec!["syscity-bot".to_string(), "Welcome to IRC".to_string()]);

        // A trailing parameter may be empty, and a line may have none.
        assert_eq!(parse_line("PING :").params, vec![String::new()]);
        assert!(parse_line("JOIN").params.is_empty());

        // A colon inside the trailing parameter is not a second split.
        let with_colon = parse_line(":a!b@c PRIVMSG #d :time is 12:30");
        assert_eq!(with_colon.params[1], "time is 12:30");
    }

    #[test]
    fn a_nickname_comes_off_its_prefix() {
        assert_eq!(nick_of("alice!u@h"), "alice");
        assert_eq!(nick_of("alice"), "alice");
        assert_eq!(nick_of(""), "");
    }

    #[test]
    fn a_channel_is_told_from_a_person() {
        for channel in ["#rust", "&local", "+modeless", "!safe"] {
            assert!(is_channel(channel), "{channel}");
        }
        for person in ["alice", "bob_", "a-channel"] {
            assert!(!is_channel(person), "{person}");
        }
    }

    /// In a channel the bot has to be named, and the name comes off — but a
    /// message that does not name it is reported rather than dropped.
    #[test]
    fn a_channel_message_is_addressed_or_reported_unmentioned() {
        let line = parse_line(":alice!u@h PRIVMSG #chan :syscity-bot: hello");
        let parsed = parse_privmsg(&line, NICK).expect("a message");

        assert!(parsed.is_group);
        assert!(parsed.mentioned);
        assert_eq!(parsed.text, "hello");
        assert_eq!(parsed.conversation_id, "#chan");
        assert_eq!(parsed.user_id, "alice");

        // All three spellings a client may use.
        for text in ["syscity-bot, hi", "syscity-bot hi"] {
            let line = parse_line(&format!(":alice!u@h PRIVMSG #chan :{}", text));
            let parsed = parse_privmsg(&line, NICK).expect("a message");
            assert!(parsed.mentioned, "{text}");
            assert_eq!(parsed.text, "hi");
        }

        // Not named: still a message, but not one addressed to us.
        let line = parse_line(":alice!u@h PRIVMSG #chan :hello everyone");
        let parsed = parse_privmsg(&line, NICK).expect("a message");
        assert!(!parsed.mentioned);
        assert_eq!(parsed.text, "hello everyone");

        // The address is only stripped at the front: the name is not a magic
        // word, and a later mention is the user's own text.
        let line = parse_line(":alice!u@h PRIVMSG #chan :ask syscity-bot about it");
        let parsed = parse_privmsg(&line, NICK).expect("a message");
        assert!(!parsed.mentioned);
        assert_eq!(parsed.text, "ask syscity-bot about it");
    }

    /// A private message is addressed to us by definition, and goes back to the
    /// sender rather than to a channel they are not in.
    #[test]
    fn a_private_message_goes_back_to_its_sender() {
        let line = parse_line(&format!(":alice!u@h PRIVMSG {} :a private word", NICK));
        let parsed = parse_privmsg(&line, NICK).expect("a message");

        assert!(!parsed.is_group);
        assert!(parsed.mentioned);
        assert_eq!(parsed.conversation_id, "alice");
        assert_eq!(parsed.text, "a private word");
    }

    /// The bot's own lines come back on the same socket. Answering them would
    /// make it talk to itself.
    #[test]
    fn our_own_message_is_dropped() {
        let line = parse_line(&format!(":{}!u@h PRIVMSG #chan :my own words", NICK));
        assert_eq!(parse_privmsg(&line, NICK), None);

        // Nicknames are case-insensitive on IRC, so the comparison is too.
        let line = parse_line(&format!(":{}!u@h PRIVMSG #chan :my own words", NICK.to_uppercase()));
        assert_eq!(parse_privmsg(&line, NICK), None);
    }

    #[test]
    fn everything_that_is_not_a_message_is_dropped() {
        for raw in [
            ":alice!u@h JOIN #chan",
            ":alice!u@h PART #chan",
            ":alice!u@h QUIT :bye",
            ":irc.test 353 syscity-bot = #chan :alice",
            "PING :token",
            // A CTCP that is not an action is a query, not a message.
            ":alice!u@h PRIVMSG #chan :\u{1}VERSION\u{1}",
            // An address with nothing after it.
            &format!(":alice!u@h PRIVMSG #chan :{}:", NICK),
        ] {
            let line = parse_line(raw);
            assert_eq!(parse_privmsg(&line, NICK), None, "{raw}");
        }
    }

    /// `/me` arrives as a CTCP ACTION, which reads as an emote rather than as a
    /// message with control characters in it.
    #[test]
    fn an_action_reads_as_an_emote() {
        let line = parse_line(":alice!u@h PRIVMSG #chan :\u{1}ACTION waves\u{1}");
        let parsed = parse_privmsg(&line, NICK).expect("a message");

        assert_eq!(parsed.text, "* alice waves");
    }

    /// The formatting codes are invisible on a client, so they are invisible
    /// here too rather than handed to the agent as noise.
    #[test]
    fn formatting_codes_are_removed() {
        assert_eq!(strip_formatting("\u{2}bold\u{2} text"), "bold text");
        // Colour codes carry their digits with them.
        assert_eq!(strip_formatting("\u{3}04red\u{3} plain"), "red plain");
        assert_eq!(strip_formatting("\u{3}04,05both\u{3} plain"), "both plain");
        assert_eq!(strip_formatting("\u{4}ff0000hex\u{4} plain"), "hex plain");
        assert_eq!(strip_formatting("nothing to do"), "nothing to do");

        let line = parse_line(":alice!u@h PRIVMSG #chan :\u{3}04syscity-bot\u{3}: hi");
        let parsed = parse_privmsg(&line, NICK).expect("a message");
        assert_eq!(parsed.text, "hi");
    }

    /// A reply is a protocol line. Text containing a line break would be read
    /// by the server as a second command, so it is neutralised — the words stay
    /// because they are the agent's, but they cannot become a command.
    #[test]
    fn a_reply_cannot_carry_a_new_command() {
        assert_eq!(sanitize("two\r\nlines"), "two  lines");
        assert_eq!(sanitize("with\0a nul"), "witha nul");

        let injected = sanitize("hello\r\nJOIN #evil");
        assert!(!injected.contains('\r'));
        assert!(!injected.contains('\n'));
        assert!(injected.contains("JOIN #evil"), "the text survives as text");
    }

    #[test]
    fn a_taken_nickname_walks_up_the_spellings() {
        assert_eq!(next_nick("bot"), "bot_");
        assert_eq!(next_nick("bot_"), "bot_1");
        assert_eq!(next_nick("bot_1"), "bot_2");
        assert_eq!(next_nick("bot_9"), "bot_10");
        // A name that merely ends in an underscore is not a counter.
        assert_eq!(next_nick("my_bot"), "my_bot_");
    }

    /// The budget belongs to the whole line, so a long channel name leaves less
    /// room than a short nickname does.
    #[test]
    fn the_message_budget_depends_on_who_is_being_addressed() {
        let config = IrcConfig::new("irc.test", NICK, CHANNEL);

        let short = config.budget_for("a");
        let long = config.budget_for("#a-very-long-channel-name");

        assert!(short > long, "a longer name leaves less room");
        // `PRIVMSG <target> :` plus CRLF is what is being paid for.
        assert_eq!(short, MAX_LINE_BYTES - ("PRIVMSG a :".len() + 2));
    }

    /// A reply is split by *bytes*, because IRC counts the whole line — and a
    /// cut still has to land on a character boundary.
    #[test]
    fn a_long_reply_is_split_on_a_byte_budget() {
        let text = "a".repeat(50);
        let pieces = split_message_bytes(&text, 20);
        assert!(pieces.iter().all(|p| p.len() <= 20), "{pieces:?}");
        assert_eq!(pieces.concat(), text);

        // Multi-byte characters are not cut in half.
        let text = "日本語".repeat(10);
        for piece in split_message_bytes(&text, 10) {
            assert!(piece.len() <= 10, "{piece:?} is over budget");
        }
        assert_eq!(split_message_bytes(&text, 10).concat(), text);

        // A budget smaller than one character still makes progress rather than
        // looping forever.
        let pieces = split_message_bytes("日本語", 1);
        assert_eq!(pieces.concat(), "日本語");
        assert_eq!(pieces.len(), 3);
    }

    /// The channel's own `Debug` must not carry the passwords.
    #[test]
    fn the_debug_rendering_does_not_leak_passwords() {
        let mut config = IrcConfig::new("irc.test", NICK, CHANNEL);
        config.server_password = Some("server-secret-value".to_string());
        config.nickserv_password = Some("nickserv-secret-value".to_string());
        let channel = IrcChannel::new(config);

        let rendered = format!("{:?}", channel);
        assert!(!rendered.contains("server-secret-value"), "{rendered}");
        assert!(!rendered.contains("nickserv-secret-value"), "{rendered}");
    }
}
