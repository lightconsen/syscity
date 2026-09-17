//! Protocol-level behaviour: what the gateway answers when a client misbehaves.

use super::*;

/// A frame the gateway cannot parse must still get an answer.
///
/// It used to be logged and dropped: the client had sent a request id and was
/// left waiting out its own timeout, unable to tell a bad frame from a slow
/// gateway. The frame is the problem, not the connection, so the connection
/// stays open.
#[tokio::test]
#[serial]
async fn malformed_frames_are_answered_and_keep_the_connection() {
    let port = free_port();
    start_test_gateway(port, false).await;
    let mut client = FrontendSimulator::connect(port).await;

    // Not JSON at all — nothing to correlate the answer with.
    let resp = client
        .send_raw_and_read_response("{ this is not json")
        .await;
    assert_eq!(resp["ok"], false, "a malformed frame must be answered: {resp}");
    assert_eq!(resp["error"]["code"], "INVALID_JSON");

    // Valid JSON, wrong shape: the id is readable, so it must come back.
    let resp = client
        .send_raw_and_read_response(r#"{"type":"req","id":"correlate-me","nope":1}"#)
        .await;
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["id"], "correlate-me", "the error must carry the request id");
    assert_eq!(resp["error"]["code"], "INVALID_JSON");

    // And the session survived both.
    let health = client.request("health", json!(null)).await;
    assert_eq!(health["ok"], true, "the connection must still work: {health}");
}

/// A connection that floods is refused, and one that keeps going is hung up.
///
/// The REST rate limiter sees one request per WebSocket connection, so it
/// cannot bound what a client does *inside* one; the per-frame bucket is what
/// does. Frames are sent in bursts (a round trip per frame would refill the
/// bucket faster than the test spends it, and prove nothing), and the refusals
/// are answered rather than dropped — a client over its budget is still waiting
/// on ids.
#[tokio::test]
#[serial]
async fn a_flood_of_requests_is_refused() {
    let port = free_port();
    start_test_gateway(port, false).await;
    let mut client = FrontendSimulator::connect(port).await;

    // Burst 1200, plus a little: enough to spend it and collect the refusals.
    let total = 1_250;
    let batch = 250;
    let mut refusals = 0;
    let mut closed = false;
    for sent in (0..total).step_by(batch) {
        for i in sent..(sent + batch).min(total) {
            client
                .send_raw_frame(&format!(r#"{{"type":"req","id":"flood-{i}","method":"ping"}}"#))
                .await;
        }
        for _ in sent..(sent + batch).min(total) {
            match client.read_response().await {
                Some(resp) => {
                    if resp["error"]["code"] == "RATE_LIMITED" {
                        refusals += 1;
                    }
                }
                None => {
                    closed = true;
                    break;
                }
            }
        }
        if closed {
            break;
        }
    }

    assert!(
        refusals > 0,
        "1250 requests through a 1200-frame burst must be refused at some point"
    );
    assert!(closed, "a client that keeps sending after the refusals is disconnected");
}
