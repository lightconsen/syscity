//! Content-addressed attachment store (CAS) for large binary payloads
//! produced by tools (screenshots today; other blobs later).
//!
//! ## Why
//!
//! Screenshots are megabytes of base64. Inline, they flow through tool
//! results, persist into `session_messages`, and replay into the LLM context
//! on every turn — stored N times with no dedup or integrity guarantee. This
//! module stores each payload ONCE under
//! `~/.syscity/attachments/sha256/<first2>/<rest>` and puts only a compact
//! reference (~100 bytes) into tool results.
//!
//! ## Reference format
//!
//! Tools embed one machine-readable marker line in their text output:
//!
//! ```text
//! {"type":"image_ref","digest":"sha256:<64 hex>","mime":"image/png","size":12345}
//! ```
//!
//! plus a human-readable note so the model knows what the image shows. The
//! marker line is what survives persistence (`session_messages.content` and
//! `tool_calls_json[].result`), which lets the GC sweep find live references
//! with a plain text scan.
//!
//! ## Request-time materialization
//!
//! [`materialize_history`] runs on the cloned request message list (never on
//! persisted history): refs from the CURRENT turn are read back from the
//! store and attached as [`ContentBlock::Image`] blocks on a trailing user
//! message, while refs from older turns degrade to a one-line text
//! placeholder (`[image sha256:abcd1234 …]`). Fresh screenshots matter now;
//! stale ones must not re-balloon the context.
//!
//! Images are carried on a synthetic USER message rather than on the tool
//! message itself because all three provider protocols (OpenAI, Anthropic,
//! Gemini) accept image parts on user messages, while tool-role image parts
//! are inconsistently supported (and rejected by strict OpenAI endpoints).
//!
//! ## Fail-open policy (deliberate)
//!
//! Producers fall back to the old inline-base64 behavior (with a `warn!`)
//! when a CAS write fails: losing a screenshot entirely is worse than storing
//! it inline. Conversely, a missing/corrupt store object at materialization
//! time degrades the ref to its placeholder text — the request still goes
//! out.
//!
//! Backward compatibility: historical rows containing raw
//! `data:image/...;base64` payloads are plain text to every consumer here and
//! stay readable untouched; only marker lines are rewritten.
// INVARIANTS-NONE: CAS objects are immutable files keyed by content digest;
// correctness follows from the digest itself, no cross-object invariant.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::Digest;
use tracing::{debug, warn};

use crate::agent::context::STATE_SNAPSHOT_NAME;
use crate::providers::{ContentBlock, Message, Role};

/// Digest prefix; a full digest is `sha256:<64 lowercase hex chars>`.
const DIGEST_PREFIX: &str = "sha256:";
/// Hex digest length in characters.
const HEX_LEN: usize = 64;
/// `type` value prefix of the machine-readable marker line in tool output.
const REF_MARKER_PREFIX: &str = "{\"type\":\"image_ref\"";
/// Cap on images materialized into a single request, so a pathological turn
/// cannot stuff unbounded image parts into one LLM call.
const MAX_MATERIALIZED_IMAGES: usize = 8;

/// A compact reference to a payload in the attachment store.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachmentRef {
    /// Content digest, e.g. `sha256:ab12cd…` (64 hex chars).
    pub digest: String,
    /// MIME type of the stored payload, e.g. `image/png`.
    pub mime: String,
    /// Payload size in bytes.
    pub size: u64,
}

impl AttachmentRef {
    /// Structured JSON form for tool-result `data` payloads.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "digest": self.digest,
            "mime": self.mime,
            "size": self.size,
        })
    }
}

/// Root directory of the attachment store (`~/.syscity/attachments`).
pub fn attachments_dir() -> PathBuf {
    crate::dirs::attachments_dir()
}

/// Filesystem path of the object for a validated hex digest:
/// `<root>/sha256/<first2>/<remaining62>`.
fn object_path(root: &Path, hex_digest: &str) -> PathBuf {
    root.join("sha256")
        .join(&hex_digest[..2])
        .join(&hex_digest[2..])
}

/// Validate a `sha256:<64 hex>` digest string and return the hex part.
///
/// Rejects anything else (including path separators) so a malformed ref can
/// never escape the store directory.
fn validate_digest(digest: &str) -> io::Result<&str> {
    let hex_part = digest.strip_prefix(DIGEST_PREFIX).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("attachment digest must start with '{DIGEST_PREFIX}'"),
        )
    })?;
    if hex_part.len() != HEX_LEN || !hex_part.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "attachment digest must be 64 lowercase hex chars",
        ));
    }
    Ok(hex_part)
}

/// Write `bytes` to a fresh temp path with owner-only permissions.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

/// Remove a leftover temp file; absence is not an error.
fn cleanup_temp(tmp: &Path) {
    match std::fs::remove_file(tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => debug!("attachment temp cleanup failed for {}: {}", tmp.display(), e),
    }
}

/// The store key of a byte payload: `sha256:<64 lowercase hex chars>`.
///
/// Public so a producer that wants to know whether it already has this object
/// can compute the same key the store would — two spellings of the same bytes
/// would defeat the dedup they both exist for.
pub fn digest_of_bytes(bytes: &[u8]) -> String {
    format!("{DIGEST_PREFIX}{}", hex::encode(sha2::Sha256::digest(bytes)))
}

/// Store `bytes` under their sha256 digest and return the reference.
///
/// Atomic: the payload is written to a unique temp file in the target
/// directory and renamed into place. Content-addressed objects are immutable,
/// so an existing object is reused without rewriting (dedup), and a lost
/// rename race against an identical concurrent write is still success.
pub fn store_bytes(bytes: &[u8], mime: &str) -> io::Result<AttachmentRef> {
    store_bytes_in(&attachments_dir(), bytes, mime)
}

/// [`store_bytes`] against an explicit store root (test seam).
fn store_bytes_in(root: &Path, bytes: &[u8], mime: &str) -> io::Result<AttachmentRef> {
    let digest = digest_of_bytes(bytes);
    let hex_digest = &digest[DIGEST_PREFIX.len()..];
    let path = object_path(root, hex_digest);
    // The temp name is derived before `digest` is moved into the reference.
    let tmp_name =
        format!(".{}.tmp-{}", &hex_digest[2..], &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let aref = AttachmentRef {
        digest,
        mime: mime.to_string(),
        size: bytes.len() as u64,
    };
    if path.exists() {
        return Ok(aref);
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "attachment path has no parent")
    })?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(tmp_name);
    if let Err(e) = write_private(&tmp, bytes) {
        cleanup_temp(&tmp);
        return Err(e);
    }
    match std::fs::rename(&tmp, &path) {
        Ok(()) => Ok(aref),
        Err(e) if path.exists() => {
            // Lost a race against an identical concurrent write; the object
            // is in place, which is all we need.
            cleanup_temp(&tmp);
            debug!("attachment rename raced an existing object: {}", e);
            Ok(aref)
        }
        Err(e) => {
            cleanup_temp(&tmp);
            Err(e)
        }
    }
}

/// Async wrapper around [`store_bytes`] (filesystem I/O off the runtime).
pub async fn store_bytes_async(bytes: Vec<u8>, mime: &str) -> io::Result<AttachmentRef> {
    let mime = mime.to_string();
    tokio::task::spawn_blocking(move || store_bytes(&bytes, &mime))
        .await
        .map_err(io::Error::other)?
}

/// Decode a base64 payload and store it. Convenience for the desktop
/// screenshot path, where the platform adapters already deliver base64.
pub async fn store_base64_image_async(b64: &str, mime: &str) -> io::Result<AttachmentRef> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid base64: {e}")))?;
    store_bytes_async(bytes, mime).await
}

/// Read back the payload for a reference.
pub fn open_ref(aref: &AttachmentRef) -> io::Result<Vec<u8>> {
    open_ref_in(&attachments_dir(), aref)
}

/// [`open_ref`] against an explicit store root (test seam).
fn open_ref_in(root: &Path, aref: &AttachmentRef) -> io::Result<Vec<u8>> {
    let hex_digest = validate_digest(&aref.digest)?;
    std::fs::read(object_path(root, hex_digest))
}

/// Serialize a reference as the single-line machine-readable marker embedded
/// in tool output text.
pub fn render_ref_line(aref: &AttachmentRef) -> String {
    format!(
        "{{\"type\":\"image_ref\",\"digest\":\"{}\",\"mime\":\"{}\",\"size\":{}}}",
        aref.digest, aref.mime, aref.size
    )
}

/// Serde view of a marker line.
#[derive(Deserialize)]
struct RefMarker {
    #[serde(rename = "type")]
    marker_type: String,
    digest: String,
    mime: String,
    size: u64,
}

/// Parse one line as an attachment marker; `None` for any other line.
fn parse_ref_line(line: &str) -> Option<AttachmentRef> {
    let line = line.trim();
    if !line.starts_with(REF_MARKER_PREFIX) {
        return None;
    }
    let marker: RefMarker = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(_) => return None,
    };
    if marker.marker_type != "image_ref" || validate_digest(&marker.digest).is_err() {
        return None;
    }
    Some(AttachmentRef {
        digest: marker.digest,
        mime: marker.mime,
        size: marker.size,
    })
}

/// Extract every attachment reference embedded in `text` (one per marker
/// line), in order.
pub fn refs_in_text(text: &str) -> Vec<AttachmentRef> {
    text.lines().filter_map(parse_ref_line).collect()
}

/// Short human-friendly digest form: `sha256:abcd1234` (first 8 hex chars).
pub fn short_id(digest: &str) -> &str {
    let end = DIGEST_PREFIX.len() + 8;
    if digest.starts_with(DIGEST_PREFIX) && digest.len() >= end {
        &digest[..end]
    } else {
        digest
    }
}

/// Map every marker line in `text` through `f`; non-marker lines pass
/// through unchanged.
fn map_ref_lines(text: &str, f: &dyn Fn(&AttachmentRef) -> String) -> String {
    text.lines()
        .map(|line| match parse_ref_line(line) {
            Some(aref) => f(&aref),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replace every marker line in `text` with a compact placeholder, e.g.
/// `[image sha256:abcd1234 …]`.
pub fn degrade_refs(text: &str) -> String {
    if !text.contains(REF_MARKER_PREFIX) {
        return text.to_string();
    }
    map_ref_lines(text, &|aref| format!("[image {} …]", short_id(&aref.digest)))
}

/// Rewrite attachment refs in a message's text (and inline tool-call result
/// copies) to placeholders.
fn degrade_message_refs(msg: &mut Message) {
    let degraded = degrade_refs(&msg.content);
    if degraded != msg.content {
        msg.content = degraded;
    }
    // Assistant messages can carry an inline copy of the tool result.
    if let Some(ref mut calls) = msg.tool_calls {
        for call in calls.iter_mut() {
            if let Some(ref result) = call.result {
                let degraded = degrade_refs(result);
                if degraded != *result {
                    call.result = Some(degraded);
                }
            }
        }
    }
}

/// Rewrite only the marker line for `target` in `text` to its placeholder.
fn degrade_single_ref(text: &str, target: &AttachmentRef) -> String {
    map_ref_lines(text, &|aref| {
        if aref == target {
            format!("[image {} …]", short_id(&aref.digest))
        } else {
            render_ref_line(aref)
        }
    })
}

/// Materialize attachment references for one outgoing LLM request.
///
/// Operates on a CLONE of the history (see
/// [`crate::agent::context::Context::to_messages`]); persisted rows always
/// keep the compact refs so the GC sweep can find them.
///
/// Policy:
/// - Refs at or before the last real user message (older turns) degrade to
///   `[image sha256:abcd1234 …]` text placeholders.
/// - Refs in tool results AFTER that boundary (current turn) are read back
///   from the store and attached as image blocks on a synthetic user message
///   inserted just before the trailing state snapshot. Unreadable objects
///   degrade to placeholders instead of failing the request.
pub fn materialize_history(messages: &mut Vec<Message>) {
    materialize_history_in(&attachments_dir(), messages);
}

/// [`materialize_history`] against an explicit store root (test seam).
fn materialize_history_in(root: &Path, messages: &mut Vec<Message>) {
    let boundary = messages
        .iter()
        .rposition(|m| m.role == Role::User && m.name.as_deref() != Some(STATE_SNAPSHOT_NAME));
    let Some(boundary) = boundary else {
        for msg in messages.iter_mut() {
            degrade_message_refs(msg);
        }
        return;
    };

    for msg in messages[..=boundary].iter_mut() {
        degrade_message_refs(msg);
    }

    // Current turn: gather refs from tool results, deduped by digest.
    let mut seen: HashSet<String> = HashSet::new();
    let mut images: Vec<ContentBlock> = Vec::new();
    for msg in messages[boundary + 1..].iter_mut() {
        if msg.role != Role::Tool {
            continue;
        }
        for aref in refs_in_text(&msg.content) {
            if !seen.insert(aref.digest.clone()) {
                continue;
            }
            if images.len() >= MAX_MATERIALIZED_IMAGES {
                debug!("attachment materialization capped at {}", MAX_MATERIALIZED_IMAGES);
                break;
            }
            match open_ref_in(root, &aref) {
                Ok(bytes) => {
                    use base64::Engine;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                    images.push(ContentBlock::image_base64(b64, aref.mime.clone()));
                }
                Err(e) => {
                    warn!("attachment {} unavailable at materialization: {}", aref.digest, e);
                    msg.content = degrade_single_ref(&msg.content, &aref);
                }
            }
        }
    }

    if images.is_empty() {
        return;
    }
    let note = if images.len() == 1 {
        "[Screenshot captured by the tool call above.]".to_string()
    } else {
        format!("[{} screenshots captured by the tool calls above.]", images.len())
    };
    let mut blocks = Vec::with_capacity(images.len() + 1);
    blocks.push(ContentBlock::text(note.clone()));
    blocks.extend(images);
    let carrier = Message::user(note).with_content_blocks(blocks);

    // Keep the trailing state-snapshot message last.
    let insert_at = match messages.last() {
        Some(m) if m.name.as_deref() == Some(STATE_SNAPSHOT_NAME) => messages.len() - 1,
        _ => messages.len(),
    };
    messages.insert(insert_at, carrier);
}

/// Scan `text` for `sha256:<64 hex>` digests (any occurrence, not only
/// marker lines) and add them to `out`. Used by the GC sweep to collect the
/// live reference set from persisted rows.
pub fn extract_digests(text: &str, out: &mut HashSet<String>) {
    for (start, _) in text.match_indices(DIGEST_PREFIX) {
        let rest = &text[start + DIGEST_PREFIX.len()..];
        let hex_part: String = rest.chars().take(HEX_LEN).collect();
        if hex_part.len() == HEX_LEN && hex_part.bytes().all(|b| b.is_ascii_hexdigit()) {
            out.insert(format!("{DIGEST_PREFIX}{hex_part}"));
        }
    }
}

/// Outcome of a GC sweep over the attachment store.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GcOutcome {
    /// Number of object files deleted.
    pub removed_files: u64,
    /// Total bytes reclaimed.
    pub removed_bytes: u64,
    /// Number of object files kept (still referenced).
    pub kept_files: u64,
}

/// Delete attachment objects whose digest is not in `referenced`.
///
/// Only files under `<root>/sha256/<xx>/<rest>` whose names form a valid
/// digest are considered objects; orphaned temp files from interrupted writes
/// (`*.tmp-*`) are always removed. Empty prefix directories are cleaned up
/// afterwards. The sweep never fails the caller over a single unreadable
/// entry — those are logged and skipped.
pub fn gc_unreferenced(root: &Path, referenced: &HashSet<String>) -> io::Result<GcOutcome> {
    let mut outcome = GcOutcome::default();
    let algo_dir = root.join("sha256");
    if !algo_dir.is_dir() {
        return Ok(outcome);
    }
    for prefix_entry in std::fs::read_dir(&algo_dir)?.flatten() {
        let prefix_dir = prefix_entry.path();
        if !prefix_dir.is_dir() {
            continue;
        }
        let Some(prefix) = prefix_dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        sweep_prefix_dir(&prefix_dir, prefix, referenced, &mut outcome);
        // Remove the prefix directory when the sweep emptied it. A non-empty
        // remainder is expected (kept objects) and not an error.
        if let Err(e) = std::fs::remove_dir(&prefix_dir) {
            debug!("attachment prefix dir {} kept: {}", prefix_dir.display(), e);
        }
    }
    Ok(outcome)
}

/// Sweep one `<root>/sha256/<xx>/` directory.
fn sweep_prefix_dir(
    prefix_dir: &Path,
    prefix: &str,
    referenced: &HashSet<String>,
    outcome: &mut GcOutcome,
) {
    let entries = match std::fs::read_dir(prefix_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!("attachment GC cannot read {}: {}", prefix_dir.display(), e);
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(rest) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let is_object = prefix.len() == 2
            && rest.len() == HEX_LEN - 2
            && prefix.bytes().all(|b| b.is_ascii_hexdigit())
            && rest.bytes().all(|b| b.is_ascii_hexdigit());
        let digest = format!("{DIGEST_PREFIX}{prefix}{rest}");
        if is_object && referenced.contains(&digest) {
            outcome.kept_files += 1;
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                outcome.removed_files += 1;
                outcome.removed_bytes += size;
            }
            Err(e) => warn!("attachment GC cannot remove {}: {}", path.display(), e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "syscity_attach_test_{}_{}",
            tag,
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store_at(root: &Path, bytes: &[u8], mime: &str) -> AttachmentRef {
        store_bytes_in(root, bytes, mime).unwrap()
    }

    #[test]
    fn store_roundtrip_layout_and_dedup() {
        let root = temp_root("store");
        let aref = store_at(&root, b"png-bytes", "image/png");
        assert!(aref.digest.starts_with(DIGEST_PREFIX));
        assert_eq!(aref.size, 9);
        assert_eq!(aref.mime, "image/png");

        let hex_part = &aref.digest[DIGEST_PREFIX.len()..];
        let path = root
            .join("sha256")
            .join(&hex_part[..2])
            .join(&hex_part[2..]);
        assert!(path.exists(), "object lives at sharded path {}", path.display());

        // Dedup: same bytes → same ref, no second write.
        let aref2 = store_at(&root, b"png-bytes", "image/png");
        assert_eq!(aref, aref2);

        assert_eq!(open_ref_in(&root, &aref).unwrap(), b"png-bytes");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn validate_digest_rejects_bad_input() {
        assert!(validate_digest("sha256:../../etc/passwd").is_err());
        assert!(validate_digest("sha256:abcd").is_err());
        assert!(validate_digest("md5:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_err());
        assert!(validate_digest(&format!("sha256:{}", "ab".repeat(32))).is_ok());
    }

    #[test]
    fn ref_line_roundtrip() {
        let aref = AttachmentRef {
            digest: format!("{DIGEST_PREFIX}{}", "ab".repeat(32)),
            mime: "image/png".to_string(),
            size: 12345,
        };
        let line = render_ref_line(&aref);
        assert!(!line.contains('\n'));
        let refs = refs_in_text(&format!("note before\n{}\nnote after", line));
        assert_eq!(refs, vec![aref]);
    }

    #[test]
    fn refs_in_text_ignores_other_lines() {
        let text = concat!(
            "{\"type\":\"image_ref\",\"digest\":\"sha256:zz\"}\n",
            "plain text\n",
            "{\"type\":\"other\",\"digest\":\"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}"
        );
        assert!(refs_in_text(text).is_empty());
    }

    #[test]
    fn degrade_refs_replaces_marker_lines_only() {
        let aref = AttachmentRef {
            digest: format!("{DIGEST_PREFIX}{}", "cd".repeat(32)),
            mime: "image/png".to_string(),
            size: 10,
        };
        let text = format!("UI tree:\n{}\n(2 windows)", render_ref_line(&aref));
        let degraded = degrade_refs(&text);
        assert!(degraded.contains("[image sha256:cdcdcdcd …]"));
        assert!(!degraded.contains("image_ref"));
        assert!(degraded.contains("UI tree:"));
        // Text without refs passes through untouched.
        assert_eq!(degrade_refs("no refs here"), "no refs here");
    }

    #[test]
    fn short_id_truncates_hex() {
        assert_eq!(short_id("sha256:0123456789abcdef"), "sha256:01234567");
        assert_eq!(short_id("sha256:xyz"), "sha256:xyz");
        assert_eq!(short_id("nodigest"), "nodigest");
    }

    #[test]
    fn extract_digests_finds_embedded_refs() {
        let digest = format!("{DIGEST_PREFIX}{}", "ef".repeat(32));
        let mut out = HashSet::new();
        extract_digests(&format!("prefix {digest} suffix"), &mut out);
        assert!(out.contains(&digest));
        let mut out2 = HashSet::new();
        extract_digests("sha256:short", &mut out2);
        assert!(out2.is_empty());
    }

    #[test]
    fn gc_removes_only_unreferenced() {
        let root = temp_root("gc");
        let referenced_ref = store_at(&root, b"keep", "image/png");
        let orphan_ref = store_at(&root, b"drop", "image/png");
        // Orphaned temp file from an interrupted write.
        let tmp = object_path(&root, &referenced_ref.digest[DIGEST_PREFIX.len()..])
            .parent()
            .unwrap()
            .join(".partial.tmp-deadbeef");
        std::fs::write(&tmp, b"junk").unwrap();

        let mut referenced = HashSet::new();
        referenced.insert(referenced_ref.digest.clone());
        let outcome = gc_unreferenced(&root, &referenced).unwrap();

        assert_eq!(outcome.kept_files, 1);
        assert_eq!(outcome.removed_files, 2); // orphan object + temp file
        assert_eq!(outcome.removed_bytes, 4 + 4);
        let kept_path = object_path(&root, &referenced_ref.digest[DIGEST_PREFIX.len()..]);
        let gone_path = object_path(&root, &orphan_ref.digest[DIGEST_PREFIX.len()..]);
        assert!(kept_path.exists());
        assert!(!gone_path.exists());
        assert!(!tmp.exists());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn gc_missing_root_is_noop() {
        let outcome =
            gc_unreferenced(Path::new("/nonexistent-attach-root"), &HashSet::new()).unwrap();
        assert_eq!(outcome, GcOutcome::default());
    }

    fn tool_msg_with_ref(aref: &AttachmentRef) -> Message {
        Message::tool(format!("Screenshot note\n{}", render_ref_line(aref)), "call-1")
    }

    #[test]
    fn materialize_current_turn_images_and_degrades_old() {
        let root = temp_root("mat");
        let old_ref = store_at(&root, b"old-png", "image/png");
        let new_ref = store_at(&root, b"new-png", "image/png");

        let mut messages = vec![
            Message::user("first request"),
            tool_msg_with_ref(&old_ref),
            Message::assistant("done"),
            Message::user("second request"),
            Message::assistant("calling screenshot tool"),
            tool_msg_with_ref(&new_ref),
        ];

        materialize_history_in(&root, &mut messages);

        // Old turn: degraded to placeholder.
        assert!(messages[1].content.contains("[image sha256:"));
        assert!(!messages[1].content.contains("image_ref"));
        // Current turn: marker line intact in the tool message…
        assert!(messages[5].content.contains("image_ref"));
        // …and an image-carrying user message appended at the end.
        let carrier = messages.last().unwrap();
        assert_eq!(carrier.role, Role::User);
        let blocks = carrier.content_blocks.as_ref().unwrap();
        assert_eq!(blocks.len(), 2); // text note + one image
        match &blocks[1] {
            ContentBlock::Image { base64, mime_type } => {
                assert_eq!(mime_type, "image/png");
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(base64)
                    .unwrap();
                assert_eq!(bytes, b"new-png");
            }
            other => panic!("expected image block, got {:?}", other),
        }

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn materialize_dedups_repeated_digest() {
        let root = temp_root("dedup");
        let aref = store_at(&root, b"same-screen", "image/png");
        let mut messages = vec![
            Message::user("req"),
            tool_msg_with_ref(&aref),
            tool_msg_with_ref(&aref),
        ];
        materialize_history_in(&root, &mut messages);
        let carrier = messages.last().unwrap();
        let blocks = carrier.content_blocks.as_ref().unwrap();
        assert_eq!(blocks.len(), 2); // one text note + ONE deduped image

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn materialize_degrades_missing_objects() {
        let root = temp_root("missing");
        let ghost = AttachmentRef {
            digest: format!("{DIGEST_PREFIX}{}", "00".repeat(32)),
            mime: "image/png".to_string(),
            size: 3,
        };
        let mut messages = vec![Message::user("req"), tool_msg_with_ref(&ghost)];
        materialize_history_in(&root, &mut messages);
        // No carrier message; the unreadable ref degraded in place.
        assert_eq!(messages.len(), 2);
        assert!(messages[1].content.contains("[image sha256:00000000 …]"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn materialize_keeps_state_snapshot_last() {
        let root = temp_root("snap");
        let new_ref = store_at(&root, b"img", "image/png");

        let mut snapshot = Message::user("[state snapshot] Today is 2026-08-23");
        snapshot.name = Some(STATE_SNAPSHOT_NAME.to_string());
        let mut messages = vec![Message::user("req"), tool_msg_with_ref(&new_ref), snapshot];
        materialize_history_in(&root, &mut messages);
        let last = messages.last().unwrap();
        assert_eq!(last.name.as_deref(), Some(STATE_SNAPSHOT_NAME));
        assert_eq!(messages.len(), 4);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn materialize_without_user_turn_degrades_everything() {
        let root = temp_root("nouser");
        let aref = store_at(&root, b"img", "image/png");
        let mut messages = vec![Message::system("sys"), tool_msg_with_ref(&aref)];
        materialize_history_in(&root, &mut messages);
        assert!(messages[1].content.contains("[image sha256:"));
        assert_eq!(messages.len(), 2);

        std::fs::remove_dir_all(&root).unwrap();
    }
}
