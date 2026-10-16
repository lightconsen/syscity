//! SimpleX channel end-to-end tests, against a fake daemon.
//!
//! The peer here is a real WebSocket server standing in for `simplex-chat`: it
//! sends events the way the daemon does and reads the command strings the
//! channel writes back. So the channel's own connect, event handling, command
//! framing and reconnect code is what is under test.
//!
//! The daemon's own command *language* (`/_send`, `/accept`) is SimpleX's, and a
//! fake peer can only check the channel speaks it — not that SimpleX agrees.

#![cfg(feature = "simplex")]

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use syscity::channels::simplex::{SimplexChannel, SimplexConfig};
use syscity::channels::{Channel, ConversationId, IncomingMessage, MentionState, OutgoingMessage};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::WebSocketStream;

/// How long a test waits for something the channel should do on its own.
const PATIENCE: Duration = Duration::from_secs(10);

/// One accepted connection, as the scripted daemon sees it.
type Peer = WebSocketStream<tokio::net::TcpStream>;

/// What the daemon saw, as it arrived.
type Seen = mpsc::UnboundedSender<String>;

/// Start a fake daemon. `serve` runs once per accepted connection.
fn fake_daemon<F, Fut>(serve: F) -> (String, mpsc::UnboundedReceiver<String>)
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
                if let Ok(socket) = tokio_tungstenite::accept_async(stream).await {
                    serve(socket, seen).await;
                }
            });
        }
    });

    (format!("ws://{}", addr), seen_rx)
}

/// A channel pointed at the fake daemon, with its inbound seam returned.
fn channel_for(url: String) -> (Arc<SimplexChannel>, mpsc::UnboundedReceiver<IncomingMessage>) {
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
    let mut config = SimplexConfig::new(url);
    config.message_tx = Some(inbound_tx);
    (Arc::new(SimplexChannel::new(config)), inbound_rx)
}

/// Wait for something the daemon saw.
async fn received(rx: &mut mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(PATIENCE, rx.recv())
        .await
        .expect("the channel should have sent something")
        .expect("the daemon should still be listening")
}

/// Start the channel's connection loop.
fn start(channel: &Arc<SimplexChannel>) -> tokio::task::JoinHandle<syscity::Result<()>> {
    let channel = channel.clone();
    tokio::spawn(async move { channel.start().await })
}

/// Wait for the channel to have stopped, or fail rather than hang.
async fn stopped(running: tokio::task::JoinHandle<syscity::Result<()>>) {
    tokio::time::timeout(PATIENCE, running)
        .await
        .expect("start() must return once stopped, rather than leaking the task")
        .expect("the task joins")
        .expect("the channel ran cleanly");
}

/// The next frame the daemon received, decoded.
async fn next_command(socket: &mut Peer) -> serde_json::Value {
    let message = tokio::time::timeout(PATIENCE, socket.next())
        .await
        .expect("the channel should have sent something")
        .expect("the socket should be open")
        .expect("the frame should be readable");
    serde_json::from_str(message.to_text().expect("a text frame")).expect("a JSON frame")
}

/// Wait for a message on the inbound seam, or fail rather than hang.
async fn inbound(rx: &mut mpsc::UnboundedReceiver<IncomingMessage>) -> IncomingMessage {
    tokio::time::timeout(PATIENCE, rx.recv())
        .await
        .expect("a message should have reached the inbound seam")
        .expect("the seam should still be open")
}

/// The `newChatItems` event a direct message arrives as.
fn direct_message(contact_id: u64, text: &str) -> String {
    serde_json::json!({
        "corrId": "daemon-1",
        "resp": {
            "type": "newChatItems",
            "chatItems": [{
                "chatInfo": {
                    "type": "direct",
                    "contact": { "contactId": contact_id, "profile": { "displayName": "Alice" } },
                },
                "chatItem": {
                    "chatDir": { "type": "directRcv" },
                    "content": {
                        "type": "rcvMsgContent",
                        "msgContent": { "type": "text", "text": text },
                    },
                },
            }],
        },
    })
    .to_string()
}

/// A message arrives, reaches the inbound seam, and a reply goes back as the
/// command the daemon expects.
#[tokio::test]
async fn a_message_round_trips_through_the_daemon() {
    let (url, mut seen) = fake_daemon(|mut socket, seen| async move {
        socket
            .send(Message::text(direct_message(42, "hello there")))
            .await
            .expect("the event is written");

        let command = next_command(&mut socket).await;
        let _ = seen.send(command.to_string());
        let sent = command["cmd"].as_str().unwrap_or_default();
        assert!(sent.starts_with("/_send @42 json "), "addressed by id: {sent}");
        let items: serde_json::Value =
            serde_json::from_str(sent.trim_start_matches("/_send @42 json "))
                .expect("the items are JSON");
        assert_eq!(
            items[0]["msgContent"]["text"], "the answer",
            "the message is JSON-encoded inside the command: {sent}"
        );
        assert_eq!(items[0]["msgContent"]["type"], "text");
        assert!(
            command["corrId"].as_str().is_some_and(|c| !c.is_empty()),
            "every command carries a correlation id"
        );

        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let (channel, mut inbound_rx) = channel_for(url);
    let running = start(&channel);

    let message = inbound(&mut inbound_rx).await;
    assert_eq!(message.user_id.0, "42");
    assert_eq!(message.conversation_id.0, "42");
    assert_eq!(message.content, "hello there");
    assert_eq!(message.mention, MentionState::DirectMessage);

    channel
        .send(OutgoingMessage::new(ConversationId::new("42"), "the answer"))
        .await
        .expect("the reply is delivered");

    // The command the daemon saw, echoed back through the channel's own filter:
    // the reply is the only thing written, and its echo is not a message.
    let command = received(&mut seen).await;
    assert!(command.contains("/_send @42 json"), "{command}");

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// A group message is addressed to the group, and reported as unmentioned —
/// SimpleX has no way for a sender to address the bot in a group, so that is
/// the only truthful value.
#[tokio::test]
async fn a_group_message_is_addressed_to_the_group() {
    let (url, _seen) = fake_daemon(|mut socket, _seen| async move {
        let event = serde_json::json!({
            "corrId": "daemon-1",
            "resp": {
                "type": "newChatItems",
                "chatItems": [{
                    "chatInfo": { "type": "group", "groupInfo": { "groupId": 7, "localDisplayName": "team" } },
                    "chatItem": {
                        "chatDir": { "type": "groupRcv", "groupMember": { "memberId": 99 } },
                        "content": {
                            "type": "rcvMsgContent",
                            "msgContent": { "type": "text", "text": "hello everyone" },
                        },
                    },
                }],
            },
        });
        socket
            .send(Message::text(event.to_string()))
            .await
            .expect("written");
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let (channel, mut inbound_rx) = channel_for(url);
    let running = start(&channel);

    let message = inbound(&mut inbound_rx).await;
    assert_eq!(message.user_id.0, "99", "the sender is the member, not the group");
    assert_eq!(message.conversation_id.0, "group:7");
    assert_eq!(message.mention, MentionState::NotMentioned);
    assert_eq!(
        message.provenance,
        syscity::channels::InputProvenance::ExternalUser {
            channel: "simplex".to_string(),
            is_direct: false,
        }
    );

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// The bot's own messages come back on the same socket, and answering them
/// would make it talk to itself.
#[tokio::test]
async fn our_own_message_is_ignored() {
    let (url, _seen) = fake_daemon(|mut socket, _seen| async move {
        let ours = serde_json::json!({
            "corrId": "daemon-1",
            "resp": {
                "type": "newChatItems",
                "chatItems": [{
                    "chatInfo": { "type": "direct", "contact": { "contactId": 42 } },
                    "chatItem": {
                        "chatDir": { "type": "directSnd" },
                        "content": {
                            "type": "rcvMsgContent",
                            "msgContent": { "type": "text", "text": "my own reply" },
                        },
                    },
                }],
            },
        });
        socket
            .send(Message::text(ours.to_string()))
            .await
            .expect("written");

        // Then a real one, to prove the channel is still working.
        socket
            .send(Message::text(direct_message(42, "and a real message")))
            .await
            .expect("written");
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let (channel, mut inbound_rx) = channel_for(url);
    let running = start(&channel);

    let message = inbound(&mut inbound_rx).await;
    assert_eq!(
        message.content, "and a real message",
        "our own message must not have been the one delivered"
    );

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// A contact request has to be answered or nobody can ever reach the bot.
#[tokio::test]
async fn a_contact_request_is_accepted() {
    let (url, mut seen) = fake_daemon(|mut socket, seen| async move {
        let request = serde_json::json!({
            "corrId": "daemon-1",
            "resp": {
                "type": "contactRequest",
                "contactRequest": { "contactRequestId": "req-1", "contactId": 55 },
            },
        });
        socket
            .send(Message::text(request.to_string()))
            .await
            .expect("written");

        let command = next_command(&mut socket).await;
        let _ = seen.send(command.to_string());
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let (channel, _inbound_rx) = channel_for(url);
    let running = start(&channel);

    let command = received(&mut seen).await;
    assert!(
        command.contains("/accept req-1"),
        "the request is answered with the command the daemon expects: {command}"
    );

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// A daemon that drops the socket is reconnected to, on its own.
#[tokio::test]
async fn a_dropped_connection_is_rebuilt() {
    let (url, mut seen) = fake_daemon(|mut socket, seen| async move {
        let _ = seen.send("connected".to_string());
        let _ = socket.close(None).await;
    });

    let (channel, _inbound_rx) = channel_for(url);
    let running = start(&channel);

    assert_eq!(
        tokio::time::timeout(PATIENCE, seen.recv())
            .await
            .expect("a first connection")
            .unwrap(),
        "connected"
    );
    assert_eq!(
        tokio::time::timeout(PATIENCE, seen.recv())
            .await
            .expect("a second connection, on its own")
            .unwrap(),
        "connected"
    );

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}
