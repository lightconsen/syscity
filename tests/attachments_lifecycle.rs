//! Attachment `image_ref` lifecycle.
//!
//! This is the mechanism that keeps screenshots out of the model's context:
//! tool output carries a JSON marker, older turns are degraded to text
//! placeholders, and only the images *after* the last user turn are
//! materialized back into image content blocks. It is wired into the
//! computer/screen-state tools, and no test above unit level asserted the
//! marker format or the materialization result — the screenshot tests only
//! checked that some string was produced.
//!
//! The store resolves through `dirs::paths()`, so `install_test_root()` must
//! run first (the same requirement every test in `tests/common` documents).

mod common;

use syscity::attachments;
use syscity::providers::{ContentBlock, Message, Role};

fn tool_message(content: String) -> Message {
    Message {
        role: Role::Tool,
        content,
        ..Message::user("")
    }
}

#[test]
fn store_marker_round_trip() {
    common::install_test_root();

    // Distinct bytes per test: the store is content-addressed and shares one
    // root across this binary, so equal payloads would dedup into one ref.
    let payload = b"attachments-lifecycle-round-trip-bytes";
    let aref = attachments::store_bytes(payload, "image/png").expect("store");

    assert!(aref.digest.starts_with("sha256:"), "digest: {}", aref.digest);
    assert_eq!(aref.size, payload.len() as u64);

    // The marker is a single-line JSON object with a `type` field — distinct
    // from `AttachmentRef::to_json()`, which has three keys and no `type`.
    let line = attachments::render_ref_line(&aref);
    let parsed: serde_json::Value = serde_json::from_str(&line).expect("marker is JSON");
    assert_eq!(parsed["type"], "image_ref");
    assert_eq!(parsed["digest"], aref.digest);
    assert_eq!(parsed["mime"], "image/png");
    assert_eq!(parsed["size"], payload.len());

    // Text carrying the marker parses back to the same ref, and reads the
    // original bytes.
    assert_eq!(attachments::refs_in_text(&line), vec![aref.clone()]);
    assert_eq!(attachments::open_ref(&aref).expect("read back"), payload);
}

#[test]
fn materialization_attaches_recent_images_and_degrades_old_ones() {
    common::install_test_root();

    let payload = b"attachments-lifecycle-materialize-bytes";
    let aref = attachments::store_bytes(payload, "image/png").expect("store");
    let marker = attachments::render_ref_line(&aref);

    // A tool message carrying the marker *after* the last user turn, plus an
    // older tool message from a previous turn that must be degraded.
    let mut history = vec![
        tool_message(format!("old screenshot\n{marker}")),
        Message::user("look at the monitor"),
        tool_message(format!("fresh screenshot\n{marker}")),
    ];

    attachments::materialize_history(&mut history);

    // The old turn is degraded to a placeholder: no marker survives there.
    assert!(
        history[0].content.contains("[image sha256:"),
        "the stale ref must degrade to a placeholder: {}",
        history[0].content
    );
    assert!(!history[0].content.contains("image_ref"), "the stale marker must not remain");

    // A carrier user message with the image block was appended.
    let carrier = history.last().expect("carrier appended");
    assert_eq!(carrier.role, Role::User, "the carrier is a user message");
    let blocks = carrier
        .content_blocks
        .as_ref()
        .expect("carrier carries content blocks");
    assert!(
        matches!(blocks.first(), Some(ContentBlock::Text { .. })),
        "the carrier leads with a text note, got {blocks:?}"
    );
    let image = blocks
        .iter()
        .find_map(|b| match b {
            ContentBlock::Image { base64, mime_type } => Some((base64, mime_type)),
            _ => None,
        })
        .expect("an image block for the fresh screenshot");
    assert_eq!(image.1, "image/png");
    assert!(!image.0.contains("data:"), "the block carries raw base64, not a data URL");
    assert!(!image.0.is_empty(), "the image payload is not empty");
}

#[test]
fn history_without_refs_is_left_alone() {
    common::install_test_root();

    let mut history = vec![
        Message::user("just text"),
        tool_message("a plain tool result".to_string()),
    ];
    let before: Vec<String> = history.iter().map(|m| m.content.clone()).collect();

    attachments::materialize_history(&mut history);

    let after: Vec<String> = history.iter().map(|m| m.content.clone()).collect();
    assert_eq!(before, after, "no refs means no rewrite and no carrier message");
    assert!(
        history.iter().all(|m| m.content_blocks.is_none()),
        "nothing should have gained content blocks"
    );
}
