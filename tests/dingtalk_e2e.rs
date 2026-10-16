//! DingTalk channel end-to-end tests, against a fake peer.
//!
//! Stream Mode has two halves a test has to stand up: the **HTTP handshake**
//! that hands out a socket and a one-shot ticket, and the **socket** itself.
//! Both are real here — `wiremock` for the first, a real `TcpListener` serving
//! WebSocket upgrades for the second — so the channel's own handshake, ticket
//! encoding, framing, acknowledgement and reconnect code is what is under test.
//!
//! The reply path is deliberately *not* covered here. A reply is an HTTP POST to
//! a per-message URL that arrives inside the inbound frame, and that URL must be
//! on `api.dingtalk.com` — which a hermetic test cannot be. The guard that makes
//! that safe, the expiry, and the refusal to answer an unknown conversation are
//! unit-tested in the module instead.

#![cfg(feature = "dingtalk")]

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use syscity::channels::dingtalk::{DingTalkChannel, DingTalkConfig};
use syscity::channels::{Channel, IncomingMessage, MentionState};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::WebSocketStream;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// How long a test waits for something the channel should do on its own.
const PATIENCE: Duration = Duration::from_secs(15);

/// The one topic this channel subscribes to.
const TOPIC: &str = "/v1.0/im/bot/messages/get";

/// One accepted connection, as the scripted peer sees it.
type Peer = WebSocketStream<tokio::net::TcpStream>;

/// The frames the peer has seen, as they arrive.
type Seen = mpsc::UnboundedSender<serde_json::Value>;

/// Start a fake peer: a real WebSocket server, and the `wss://`-shaped endpoint
/// the handshake below hands to the channel.
fn fake_socket<F, Fut>(serve: F) -> (String, mpsc::UnboundedReceiver<serde_json::Value>)
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
    let endpoint = format!("ws://{}", addr);

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

    (endpoint, seen_rx)
}

/// A channel pointed at both fakes, with its inbound seam returned alongside.
fn channel_for(
    handshake: &MockServer,
) -> (Arc<DingTalkChannel>, mpsc::UnboundedReceiver<IncomingMessage>) {
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
    let mut config = DingTalkConfig::new("client-1", "the-secret");
    config.openapi_endpoint = handshake.uri();
    config.message_tx = Some(inbound_tx);

    (Arc::new(DingTalkChannel::new(config)), inbound_rx)
}

/// Start the channel's connection loop.
fn start(channel: &Arc<DingTalkChannel>) -> tokio::task::JoinHandle<syscity::Result<()>> {
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
async fn next_frame(socket: &mut Peer) -> serde_json::Value {
    let message = tokio::time::timeout(PATIENCE, socket.next())
        .await
        .expect("the peer should have sent something")
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

/// The `CALLBACK` frame a person's message arrives as — note that `data` is a
/// JSON *string*, which is the part worth getting right.
fn callback(
    message_id: &str,
    chat: &str,
    conversation_type: &str,
    text: &str,
    mentioned: bool,
) -> String {
    let data = serde_json::json!({
        "msgId": message_id,
        "conversationId": chat,
        "conversationType": conversation_type,
        "senderId": "sender-1",
        "senderStaffId": "staff-1",
        // The mention is a structured field, not something in the text.
        "isInAtList": mentioned,
        "msgtype": "text",
        "text": { "content": text },
    });

    serde_json::json!({
        "specVersion": "1.0",
        "type": "CALLBACK",
        "headers": { "messageId": message_id, "topic": TOPIC, "contentType": "application/json" },
        "data": data.to_string(),
    })
    .to_string()
}

/// The handshake, the ticket, the delivery, the acknowledgement and the seam —
/// the whole transport, in one exchange.
#[tokio::test]
async fn a_message_round_trips_through_the_handshake_and_the_socket() {
    let (endpoint, mut seen) = fake_socket(|mut socket, seen| async move {
        socket
            .send(Message::text(callback("msg-1", "cid-1", "1", "hello there", false)))
            .await
            .expect("the message is written");

        // Every delivery must be acknowledged, echoing its id, or the platform
        // sends it again.
        let ack = next_frame(&mut socket).await;
        let _ = seen.send(ack.clone());
        assert_eq!(ack["code"], 200);
        assert_eq!(ack["headers"]["messageId"], "msg-1");
        assert!(ack["data"].is_string(), "the reference sends this field as a string");

        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    // The handshake answers with the socket the peer is actually listening on.
    let handshake = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/gateway/connections/open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "endpoint": endpoint,
            "ticket": "ticket-1",
        })))
        .mount(&handshake)
        .await;

    let (channel, mut inbound_rx) = channel_for(&handshake);
    let running = start(&channel);

    let message = inbound(&mut inbound_rx).await;
    assert_eq!(message.user_id.0, "staff-1");
    assert_eq!(message.conversation_id.0, "cid-1");
    assert_eq!(message.content, "hello there");
    assert_eq!(message.mention, MentionState::DirectMessage);

    let ack = tokio::time::timeout(PATIENCE, seen.recv())
        .await
        .expect("the acknowledgement should arrive")
        .expect("the peer should still be listening");
    assert_eq!(ack["headers"]["messageId"], "msg-1");

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// A group message is reported as a group, and its mention comes from the
/// platform's own field rather than being scraped out of the text.
#[tokio::test]
async fn a_group_message_keeps_its_shape_and_its_mention() {
    let (endpoint, _seen) = fake_socket(|mut socket, seen| async move {
        let _ = seen.send(serde_json::json!({ "ready": true }));
        socket
            .send(Message::text(callback("msg-1", "group-1", "2", "@Bot check this", true)))
            .await
            .expect("the message is written");
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let handshake = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/gateway/connections/open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "endpoint": endpoint,
            "ticket": "ticket-1",
        })))
        .mount(&handshake)
        .await;

    let (channel, mut inbound_rx) = channel_for(&handshake);
    let running = start(&channel);

    let message = inbound(&mut inbound_rx).await;
    assert_eq!(message.conversation_id.0, "group-1");
    assert_eq!(
        message.content, "@Bot check this",
        "DingTalk reports the mention structurally, so the text is left alone"
    );
    assert_eq!(message.mention, MentionState::Mentioned);
    assert_eq!(
        message.provenance,
        syscity::channels::InputProvenance::ExternalUser {
            channel: "dingtalk".to_string(),
            is_direct: false,
        },
        "a group must not be reported as a direct message"
    );

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// A delivery is acknowledged before it becomes a turn, and a redelivery of the
/// same message is acknowledged again without becoming a second turn.
#[tokio::test]
async fn a_redelivery_is_acknowledged_but_not_repeated() {
    let (endpoint, mut seen) = fake_socket(|mut socket, seen| async move {
        let body = callback("msg-1", "cid-1", "1", "hello", false);
        socket
            .send(Message::text(body.clone()))
            .await
            .expect("written");
        let first = next_frame(&mut socket).await;
        let _ = seen.send(first);

        // The platform sends it again; it must be acknowledged again.
        socket
            .send(Message::text(body))
            .await
            .expect("written again");
        let second = next_frame(&mut socket).await;
        let _ = seen.send(second);

        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    let handshake = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/gateway/connections/open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "endpoint": endpoint,
            "ticket": "ticket-1",
        })))
        .mount(&handshake)
        .await;

    let (channel, mut inbound_rx) = channel_for(&handshake);
    let running = start(&channel);

    assert_eq!(inbound(&mut inbound_rx).await.content, "hello");
    assert!(
        tokio::time::timeout(Duration::from_millis(750), inbound_rx.recv())
            .await
            .is_err(),
        "the redelivery must not become a second turn"
    );

    // Both acknowledgements arrive, because the platform has to be told it can
    // stop retrying.
    for _ in 0..2 {
        let ack = tokio::time::timeout(PATIENCE, seen.recv())
            .await
            .expect("each delivery is acknowledged")
            .expect("the peer should still be listening");
        assert_eq!(ack["headers"]["messageId"], "msg-1");
    }

    channel.stop().await.expect("the channel stops");
    stopped(running).await;
}

/// A socket that drops is rebuilt from a fresh handshake — the ticket is
/// one-shot, so reconnecting has to start there.
#[tokio::test]
async fn a_dropped_socket_is_rebuilt_from_a_fresh_handshake() {
    let (endpoint, _seen) = fake_socket(|mut socket, seen| async move {
        let _ = seen.send(serde_json::json!({ "connected": true }));
        // Hang up on the channel.
        let _ = socket.close(None).await;
    });

    let handshake = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1.0/gateway/connections/open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "endpoint": endpoint,
            "ticket": "ticket-1",
        })))
        // .expect(2) is the assertion: a second handshake means the channel came
        // back on its own rather than giving up.
        .expect(2)
        .mount(&handshake)
        .await;

    let (channel, _inbound_rx) = channel_for(&handshake);
    let running = start(&channel);

    // Waiting for the backoff (2s) plus the retry is what the mock's
    // expectation above verifies; drop the channel only once that has had time.
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(channel.health_check().await.expect("health is readable"));

    channel.stop().await.expect("the channel stops");
    stopped(running).await;

    handshake.verify().await;
}
