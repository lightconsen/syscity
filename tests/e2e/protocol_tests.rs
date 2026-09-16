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
