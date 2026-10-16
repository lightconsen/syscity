//! IRC channel end-to-end tests, against a fake server.
//!
//! IRC is a plain line protocol over a TCP socket, so the peer here is a real
//! TCP server that speaks it back — which is a stricter test than a mock would
//! be, because the channel's own registration, line framing, keepalive and
//! reconnect code is what is being exercised.
//!
//! TLS is switched off for every test: a certificate for `127.0.0.1` is not
//! something a test can have, and the TLS path is a thin wrapper over the same
//! framing.

#![cfg(feature = "irc")]

use std::sync::Arc;
use std::time::Duration;

use syscity::channels::irc::{IrcChannel, IrcConfig};
use syscity::channels::{Channel, ConversationId, IncomingMessage, MentionState, OutgoingMessage};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// How long a test waits for something the channel should do on its own.
const PATIENCE: Duration = Duration::from_secs(10);

/// The nickname the tests ask for, and the channel they join.
const NICK: &str = "syscity-bot";
const CHANNEL: &str = "#test";

/// What the fake server saw, as it arrived.
type Seen = mpsc::UnboundedSender<String>;

/// One accepted connection, with everything a scripted server needs.
struct Peer {
    reader: BufReader<tokio::io::ReadHalf<TcpStream>>,
    writer: BufWriter<tokio::io::WriteHalf<TcpStream>>,
}

impl Peer {
    /// The next line the channel sent, or fail rather than hang.
    async fn next_line(&mut self) -> String {
        let mut line = String::new();
        let read = timeout(PATIENCE, self.reader.read_line(&mut line))
            .await
            .expect("the channel should have sent something")
            .expect("the socket should be readable");
        assert!(read > 0, "the channel closed the connection unexpectedly");
        line.trim_end_matches(['\r', '\n']).to_string()
    }

    /// Assert that the next line is `expected`, and return it.
    async fn expect(&mut self, expected: &str) -> String {
        let line = self.next_line().await;
        assert_eq!(line, expected);
        line
    }

    /// Send a line to the channel.
    async fn send(&mut self, line: &str) {
        self.writer
            .write_all(format!("{}\r\n", line).as_bytes())
            .await
            .expect("the write succeeds");
        self.writer.flush().await.expect("the flush succeeds");
    }

    /// Complete registration the way a server does, confirming `nick`.
    async fn welcome(&mut self, nick: &str) {
        self.expect(&format!("NICK {}", nick)).await;
        self.expect(&format!("USER {} 0 * :Syscity Agent", nick))
            .await;
        self.send(&format!(":irc.test 001 {} :Welcome", nick)).await;
    }
}

/// Start a fake IRC server. `serve` runs once per accepted connection.
fn fake_server<F, Fut>(serve: F) -> (String, u16, mpsc::UnboundedReceiver<String>)
where
    F: Fn(Peer, Seen) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a test port");
    let addr = listener.local_addr().expect("read the test port");
    listener
        .set_nonblocking(true)
        .expect("a non-blocking listener");
    let listener = TcpListener::from_std(listener).expect("adopt the listener");

    let (seen_tx, seen_rx) = mpsc::unbounded_channel();
    let serve = Arc::new(serve);

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen_tx.clone();
            let serve = serve.clone();
            tokio::spawn(async move {
                let (reader, writer) = tokio::io::split(stream);
                serve(
                    Peer {
                        reader: BufReader::new(reader),
                        writer: BufWriter::new(writer),
                    },
                    seen,
                )
                .await;
            });
        }
    });

    ("127.0.0.1".to_string(), addr.port(), seen_rx)
}

/// A channel pointed at the fake server, with its inbound seam returned.
fn channel_for(
    server: String,
    port: u16,
) -> (Arc<IrcChannel>, mpsc::UnboundedReceiver<IncomingMessage>) {
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
    let mut config = IrcConfig::new(server, NICK, CHANNEL);
    config.port = port;
    // A test cannot hold a certificate for 127.0.0.1.
    config.use_tls = false;
    config.message_tx = Some(inbound_tx);

    (Arc::new(IrcChannel::new(config)), inbound_rx)
}

/// Start the channel's connection loop.
fn start(channel: &Arc<IrcChannel>) -> tokio::task::JoinHandle<syscity::Result<()>> {
    let channel = channel.clone();
    tokio::spawn(async move { channel.start().await })
}

/// Wait for the channel to have stopped, or fail rather than hang.
async fn stopped(running: tokio::task::JoinHandle<syscity::Result<()>>) {
    timeout(PATIENCE, running)
        .await
        .expect("start() must return once stopped, rather than leaking the task")
        .expect("the task joins")
        .expect("the channel ran cleanly");
}

/// Wait for a message on the inbound seam, or fail rather than hang.
async fn inbound(rx: &mut mpsc::UnboundedReceiver<IncomingMessage>) -> IncomingMessage {
    timeout(PATIENCE, rx.recv())
        .await
        .expect("a message should have reached the inbound seam")
        .expect("the seam should still be open")
}

/// Registration, the join, a channel message addressed to the bot, and a reply
/// going back out — the whole loop, over a real socket.
#[tokio::test]
async fn a_message_round_trips_over_the_socket() {
    let (server, port, _seen) = fake_server(|mut peer, _seen| async move {
        peer.welcome(NICK).await;
        peer.expect(&format!("JOIN {}", CHANNEL)).await;

        peer.send(&format!(":alice!u@h PRIVMSG {} :{}: hello there", CHANNEL, NICK))
            .await;

        // The reply is a PRIVMSG to the channel, with the address stripped.
        let reply = peer.next_line().await;
        assert_eq!(reply, format!("PRIVMSG {} :the answer", CHANNEL));

        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let (channel, mut inbound_rx) = channel_for(server, port);
    let running = start(&channel);

    let message = inbound(&mut inbound_rx).await;
    assert_eq!(message.user_id.0, "alice");
    assert_eq!(message.conversation_id.0, CHANNEL);
    assert_eq!(message.content, "hello there", "the address is stripped");
    assert_eq!(message.mention, MentionState::Mentioned);

    channel
        .send(OutgoingMessage::new(ConversationId::new(CHANNEL), "the answer"))
        .await
        .expect("the reply is delivered");

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// A channel message that does not name the bot is still reported — as
/// unmentioned — so `require_mention`, rather than this channel, decides
/// whether the agent answers it. The reference drops these outright; reporting
/// them is what makes the setting mean something.
#[tokio::test]
async fn a_channel_message_that_does_not_name_the_bot_is_reported_as_unmentioned() {
    let (server, port, _seen) = fake_server(|mut peer, _seen| async move {
        peer.welcome(NICK).await;
        peer.expect(&format!("JOIN {}", CHANNEL)).await;
        peer.send(&format!(":bob!u@h PRIVMSG {} :just talking to myself", CHANNEL))
            .await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let (channel, mut inbound_rx) = channel_for(server, port);
    let running = start(&channel);

    let message = inbound(&mut inbound_rx).await;
    assert_eq!(message.content, "just talking to myself");
    assert_eq!(message.mention, MentionState::NotMentioned);
    assert_eq!(
        message.provenance,
        syscity::channels::InputProvenance::ExternalUser {
            channel: "irc".to_string(),
            is_direct: false,
        }
    );

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// A private message goes to the sender, not to a channel, and is addressed to
/// the bot by definition.
#[tokio::test]
async fn a_private_message_is_addressed_to_its_sender() {
    let (server, port, _seen) = fake_server(|mut peer, _seen| async move {
        peer.welcome(NICK).await;
        peer.expect(&format!("JOIN {}", CHANNEL)).await;
        peer.send(&format!(":carol!u@h PRIVMSG {} :a private word", NICK))
            .await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let (channel, mut inbound_rx) = channel_for(server, port);
    let running = start(&channel);

    let message = inbound(&mut inbound_rx).await;
    assert_eq!(message.conversation_id.0, "carol", "the reply goes to the sender");
    assert_eq!(message.mention, MentionState::DirectMessage);

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// The server's keepalive has to be answered, or it drops the connection.
#[tokio::test]
async fn a_server_ping_is_answered() {
    let (server, port, _seen) = fake_server(|mut peer, _seen| async move {
        peer.welcome(NICK).await;
        peer.expect(&format!("JOIN {}", CHANNEL)).await;
        peer.send("PING :token-123").await;
        peer.expect("PONG :token-123").await;
    });

    let (channel, _inbound_rx) = channel_for(server, port);
    let running = start(&channel);

    // The peer asserts the PONG itself; give it time to arrive, then stop.
    tokio::time::sleep(Duration::from_secs(1)).await;
    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// The nickname asked for is not always the one handed back: a server whose
/// name is taken amends it, and it is the *confirmed* name that a channel
/// message has to be addressed to.
#[tokio::test]
async fn a_taken_nickname_is_retried_and_the_confirmed_one_addresses_us() {
    let (server, port, _seen) = fake_server(|mut peer, _seen| async move {
        peer.expect(&format!("NICK {}", NICK)).await;
        peer.expect(&format!("USER {} 0 * :Syscity Agent", NICK))
            .await;
        peer.send(":irc.test 433 * syscity-bot :Nickname is already in use")
            .await;

        // The retry, and then the name the server settles on.
        let retry = peer.next_line().await;
        assert_eq!(retry, "NICK syscity-bot_");
        peer.send(":irc.test 001 syscity-bot_ :Welcome").await;
        peer.expect(&format!("JOIN {}", CHANNEL)).await;

        // Addressed to the *confirmed* name, which is not the one configured.
        peer.send(&format!(":dave!u@h PRIVMSG {} :syscity-bot_: are you there", CHANNEL))
            .await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let (channel, mut inbound_rx) = channel_for(server, port);
    let running = start(&channel);

    let message = inbound(&mut inbound_rx).await;
    assert_eq!(message.content, "are you there");
    assert_eq!(message.mention, MentionState::Mentioned);

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// A reply is a protocol line, so text containing a line break would be read by
/// the server as a second command. It has to be neutralised, not sent.
#[tokio::test]
async fn a_reply_cannot_inject_a_second_irc_command() {
    let (server, port, _seen) = fake_server(|mut peer, _seen| async move {
        peer.welcome(NICK).await;
        peer.expect(&format!("JOIN {}", CHANNEL)).await;

        // The reply carries a newline and a whole second command inside it.
        let reply = peer.next_line().await;
        assert!(
            reply.starts_with(&format!("PRIVMSG {} :", CHANNEL)),
            "one line, and it is a PRIVMSG: {reply}"
        );
        assert!(!reply.contains('\r'), "{reply}");
        assert!(
            !reply.contains("JOIN #evil"),
            "the injected command must not survive as a command: {reply}"
        );
        assert!(
            reply.contains("JOIN #evil"),
            "but the words are the agent's, so they stay as text: {reply}"
        );

        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let (channel, _inbound_rx) = channel_for(server, port);
    let running = start(&channel);

    tokio::time::sleep(Duration::from_secs(1)).await;
    channel
        .send(OutgoingMessage::new(ConversationId::new(CHANNEL), "here you go\r\nJOIN #evil"))
        .await
        .expect("the reply is delivered");

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// A dropped connection is rebuilt, and registration happens again on its own.
#[tokio::test]
async fn a_dropped_connection_is_rebuilt() {
    let (server, port, mut seen) = fake_server(|mut peer, seen| async move {
        peer.welcome(NICK).await;
        let _ = seen.send("registered".to_string());
        // Hang up on the channel.
        drop(peer.writer);
        drop(peer.reader);
    });

    let (channel, _inbound_rx) = channel_for(server, port);
    let running = start(&channel);

    assert_eq!(
        timeout(PATIENCE, seen.recv())
            .await
            .expect("a first connection")
            .unwrap(),
        "registered"
    );
    assert_eq!(
        timeout(PATIENCE, seen.recv())
            .await
            .expect("a second connection, on its own")
            .unwrap(),
        "registered",
        "the channel reconnects and registers again without being asked"
    );

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}
