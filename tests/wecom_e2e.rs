//! WeCom channel end-to-end tests, against a fake peer.
//!
//! WeCom is the first channel with no webhook route: the gateway opens a
//! WebSocket and the same socket carries messages both ways. So the signed-
//! webhook shape every other channel's e2e uses does not apply here, and what
//! needs proving is the *transport* — that the channel authenticates, that a
//! message it receives reaches the inbound seam, that a reply goes back out
//! addressed to the message it answers, and that a dropped connection is
//! rebuilt rather than abandoned.
//!
//! The peer below is a real WebSocket server on a real socket, so the channel's
//! own connect, subscribe, framing and reconnect code is what is under test.

#![cfg(feature = "wecom")]

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use syscity::channels::wecom::{WeComChannel, WeComConfig};
use syscity::channels::{Channel, ConversationId, IncomingMessage, MentionState, OutgoingMessage};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::WebSocketStream;

/// How long a test waits for something the channel should do on its own.
const PATIENCE: Duration = Duration::from_secs(15);

/// A frame the peer received, decoded.
type Frame = serde_json::Value;

/// One accepted connection, as the scripted peer sees it.
type Peer = WebSocketStream<tokio::net::TcpStream>;

/// The frames the peer has seen, as they arrive.
type Seen = mpsc::UnboundedSender<Frame>;

/// Start a fake WeCom gateway, and return the URL to point the channel at plus
/// a receiver of every frame the peer sees.
///
/// `serve` is called once per accepted connection, so a test can script one
/// exchange — or hang up, and watch the channel come back.
fn fake_gateway<F, Fut>(serve: F) -> (String, mpsc::UnboundedReceiver<Frame>)
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
            // Each connection is served on its own task, so one that ends does
            // not stop the listener accepting the next.
            tokio::spawn(async move {
                if let Ok(socket) = tokio_tungstenite::accept_async(stream).await {
                    serve(socket, seen).await;
                }
            });
        }
    });

    (format!("ws://{}", addr), seen_rx)
}

/// A channel pointed at `url`, with its inbound seam returned alongside.
fn channel_for(url: String) -> (Arc<WeComChannel>, mpsc::UnboundedReceiver<IncomingMessage>) {
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
    let mut config = WeComConfig::new("bot-1", "the-secret");
    config.ws_url = url;
    config.message_tx = Some(inbound_tx);
    (Arc::new(WeComChannel::new(config)), inbound_rx)
}

/// Start the channel's connection loop, and hand back its task so a test can
/// prove the task ends rather than leaking.
fn start(channel: &Arc<WeComChannel>) -> tokio::task::JoinHandle<syscity::Result<()>> {
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

/// The next frame the peer sent, or fail rather than hang.
async fn next_frame(socket: &mut Peer) -> Frame {
    let message = tokio::time::timeout(PATIENCE, socket.next())
        .await
        .expect("the peer should have sent something")
        .expect("the socket should be open")
        .expect("the frame should be readable");
    serde_json::from_str(message.to_text().expect("a text frame")).expect("a JSON frame")
}

/// The next frame the peer saw, or fail rather than hang.
async fn seen(rx: &mut mpsc::UnboundedReceiver<Frame>) -> Frame {
    tokio::time::timeout(PATIENCE, rx.recv())
        .await
        .expect("the channel should have sent something")
        .expect("the peer should still be listening")
}

/// Wait for a message on the inbound seam, or fail rather than hang.
async fn inbound(rx: &mut mpsc::UnboundedReceiver<IncomingMessage>) -> IncomingMessage {
    tokio::time::timeout(PATIENCE, rx.recv())
        .await
        .expect("a message should have reached the inbound seam")
        .expect("the seam should still be open")
}

/// Answer the subscribe frame every connection begins with.
///
/// Asserts the shape while it is here, because this is the one frame the
/// channel must get right for anything else to happen.
async fn subscribe(socket: &mut Peer, seen: &Seen) {
    let frame = next_frame(socket).await;
    assert_eq!(frame["cmd"], "aibot_subscribe", "a connection authenticates first");
    assert_eq!(frame["body"]["bot_id"], "bot-1");
    assert_eq!(frame["body"]["secret"], "the-secret");
    assert!(
        frame["body"]["device_id"]
            .as_str()
            .is_some_and(|d| !d.is_empty()),
        "the subscribe carries a device id"
    );

    let req_id = frame["headers"]["req_id"].as_str().expect("a request id");
    socket
        .send(Message::text(ack(req_id)))
        .await
        .expect("the acknowledgement is written");
    let _ = seen.send(frame);
}

/// The `aibot_msg_callback` a person's message arrives as.
fn callback(req_id: &str, msg_id: &str, chat: &str, user: &str, text: &str, group: bool) -> String {
    serde_json::json!({
        "cmd": "aibot_msg_callback",
        "headers": { "req_id": req_id },
        "body": {
            "msgid": msg_id,
            "chatid": chat,
            "chattype": if group { "group" } else { "single" },
            "from": { "userid": user },
            "msgtype": "text",
            "text": { "content": text },
        },
    })
    .to_string()
}

/// The acknowledgement the server sends for a frame it accepted.
fn ack(req_id: &str) -> String {
    serde_json::json!({ "errcode": 0, "errmsg": "ok", "headers": { "req_id": req_id } }).to_string()
}

/// A message arrives over the socket, reaches the inbound seam, and the reply
/// goes back out on the request id that carried it — which is what puts the
/// answer in the thread the question came from.
#[tokio::test]
async fn a_message_round_trips_over_the_socket() {
    let (url, mut frames) = fake_gateway(|mut socket, seen| async move {
        subscribe(&mut socket, &seen).await;

        socket
            .send(Message::text(callback(
                "req-inbound-1",
                "msg-1",
                "chat-1",
                "user-1",
                "hello there",
                false,
            )))
            .await
            .expect("the message is written");

        // The reply echoes the *inbound* request id.
        let reply = next_frame(&mut socket).await;
        let _ = seen.send(reply.clone());
        assert_eq!(reply["cmd"], "aibot_respond_msg");
        assert_eq!(reply["headers"]["req_id"], "req-inbound-1");
        assert_eq!(reply["body"]["msgtype"], "markdown");
        assert_eq!(reply["body"]["markdown"]["content"], "the answer");

        socket
            .send(Message::text(ack("req-inbound-1")))
            .await
            .expect("the acknowledgement is written");

        // Hold the connection open until the test is finished with it.
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let (channel, mut inbound_rx) = channel_for(url);
    let running = start(&channel);

    let message = inbound(&mut inbound_rx).await;
    assert_eq!(message.user_id.0, "user-1");
    assert_eq!(message.conversation_id.0, "chat-1");
    assert_eq!(message.content, "hello there");
    assert_eq!(message.mention, MentionState::DirectMessage);

    // The reply is addressed to the chat, which is what `session_channels`
    // hands the outbound path.
    channel
        .send(OutgoingMessage::new(ConversationId::new("chat-1"), "the answer"))
        .await
        .expect("the reply is delivered");

    // The subscribe, then the reply the peer echoed back to us.
    assert_eq!(seen(&mut frames).await["cmd"], "aibot_subscribe");
    assert_eq!(seen(&mut frames).await["headers"]["req_id"], "req-inbound-1");

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// A group message is reported as a group, and a leading mention is stripped
/// without losing the fact that there was one — the two facts the inbound stage
/// reads to decide whether to answer at all.
#[tokio::test]
async fn a_group_message_keeps_its_shape_and_loses_its_mention() {
    let (url, _frames) = fake_gateway(|mut socket, seen| async move {
        subscribe(&mut socket, &seen).await;
        socket
            .send(Message::text(callback(
                "req-1",
                "msg-1",
                "group-1",
                "user-1",
                "@Bot can you check this",
                true,
            )))
            .await
            .expect("the message is written");
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let (channel, mut inbound_rx) = channel_for(url);
    let running = start(&channel);

    let message = inbound(&mut inbound_rx).await;
    assert_eq!(message.conversation_id.0, "group-1");
    assert_eq!(message.content, "can you check this", "the mention is stripped");
    assert_eq!(
        message.mention,
        MentionState::Mentioned,
        "and the fact that it was there is kept"
    );
    assert_eq!(
        message.provenance,
        syscity::channels::InputProvenance::ExternalUser {
            channel: "wecom".to_string(),
            is_direct: false,
        },
        "a group must not be reported as a direct message, or its bot answers everything in it"
    );

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// The platform redelivers, and answering the same message twice is worse than
/// dropping it.
#[tokio::test]
async fn a_redelivered_message_is_dropped() {
    let (url, _frames) = fake_gateway(|mut socket, seen| async move {
        subscribe(&mut socket, &seen).await;
        let body = callback("req-1", "msg-1", "chat-1", "user-1", "hello", false);
        socket
            .send(Message::text(body.clone()))
            .await
            .expect("written");
        socket
            .send(Message::text(body))
            .await
            .expect("written again");
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let (channel, mut inbound_rx) = channel_for(url);
    let running = start(&channel);

    assert_eq!(inbound(&mut inbound_rx).await.content, "hello");
    assert!(
        tokio::time::timeout(Duration::from_millis(750), inbound_rx.recv())
            .await
            .is_err(),
        "the second copy of the same message must not become a second turn"
    );

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// A dropped connection is rebuilt rather than abandoned: the gateway
/// reconnects and authenticates again, with nobody asking it to.
#[tokio::test]
async fn a_dropped_connection_is_rebuilt() {
    let (url, mut frames) = fake_gateway(|mut socket, seen| async move {
        subscribe(&mut socket, &seen).await;
        // Hang up on the channel. The peer's next connection is served by the
        // same script, so a second subscribe is the channel coming back.
        let _ = socket.close(None).await;
    });

    let (channel, _inbound_rx) = channel_for(url);
    let running = start(&channel);

    assert_eq!(seen(&mut frames).await["cmd"], "aibot_subscribe", "the first connection");
    assert_eq!(
        seen(&mut frames).await["cmd"],
        "aibot_subscribe",
        "and the channel reconnects and authenticates again on its own"
    );

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// A reply window that has closed falls back to the proactive path for a direct
/// message — and is refused outright in a group, where an AI Bot cannot start a
/// conversation.
#[tokio::test]
async fn a_closed_reply_window_falls_back_or_is_refused() {
    let (url, mut frames) = fake_gateway(|mut socket, seen| async move {
        subscribe(&mut socket, &seen).await;

        socket
            .send(Message::text(callback("req-1", "msg-1", "dm-1", "user-1", "hi", false)))
            .await
            .expect("the direct message is written");
        socket
            .send(Message::text(callback("req-2", "msg-2", "group-1", "user-1", "@Bot hi", true)))
            .await
            .expect("the group message is written");

        // Both replies are refused, because both windows have closed. The
        // direct message has somewhere to go instead; the group does not, and
        // nothing further should arrive for it.
        for req_id in ["req-1", "req-2"] {
            let reply = next_frame(&mut socket).await;
            assert_eq!(reply["headers"]["req_id"], req_id);
            socket
                .send(Message::text(
                    serde_json::json!({
                        "errcode": 846604,
                        "errmsg": "req_id window expired",
                        "headers": { "req_id": req_id },
                    })
                    .to_string(),
                ))
                .await
                .expect("the refusal is written");

            if req_id == "req-1" {
                let fallback = next_frame(&mut socket).await;
                let _ = seen.send(fallback.clone());
                assert_eq!(fallback["cmd"], "aibot_send_msg");
                assert_eq!(fallback["body"]["chatid"], "dm-1");
                assert_eq!(fallback["body"]["markdown"]["content"], "the answer");

                socket
                    .send(Message::text(ack(fallback["headers"]["req_id"]
                        .as_str()
                        .expect("a request id"))))
                    .await
                    .expect("the acknowledgement is written");
            }
        }

        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let (channel, mut inbound_rx) = channel_for(url);
    let running = start(&channel);

    assert_eq!(inbound(&mut inbound_rx).await.conversation_id.0, "dm-1");
    assert_eq!(inbound(&mut inbound_rx).await.conversation_id.0, "group-1");

    // A direct message: the window has closed, so the reply starts a message
    // instead of answering one.
    channel
        .send(OutgoingMessage::new(ConversationId::new("dm-1"), "the answer"))
        .await
        .expect("a direct message falls back to the proactive path");

    // A group: there is no such fallback, and saying so is the honest answer.
    let refused = channel
        .send(OutgoingMessage::new(ConversationId::new("group-1"), "the answer"))
        .await;
    assert!(
        refused.is_err(),
        "an AI Bot cannot start a conversation in a group, so this must not silently succeed"
    );

    assert_eq!(seen(&mut frames).await["cmd"], "aibot_subscribe");
    assert_eq!(seen(&mut frames).await["cmd"], "aibot_send_msg");

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}
