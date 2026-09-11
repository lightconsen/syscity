//! kb.* WS handlers — local Knowledge Base management.
//!
//! Exposes the engine's `KnowledgeBaseManager` (rag/ingestion) over WS for the
//! built-in UI. Collections are per-agent (`kb-{agent_id}`) — the same
//! collections agents retrieve from implicitly via memory context, so a
//! document uploaded here is immediately retrievable by that agent.
//!
//! Ingestion requires an API embedding provider (`[vector_memory]`
//! `provider = "openai"` + key); when it isn't usable, `kb.collections`
//! reports `configured: false` with a human-readable reason and the write
//! methods fail with `KB_NOT_CONFIGURED`.

use std::sync::Arc;

use super::*;
use crate::gateway::init::services::ensure_kb_manager;
use crate::rag::ingestion::{KnowledgeSource, SourceType};

/// Upload cap for `kb.ingest` / `cloud.kb.upload`: 32 MiB of raw bytes. The
/// WS frame envelope is 64 MiB and base64 inflates by ~4/3, so this leaves
/// comfortable headroom.
pub(crate) const MAX_KB_UPLOAD_BYTES: usize = 32 * 1024 * 1024;

/// `kb.collections` — list per-agent collections with summary stats.
///
/// Returns `configured: false` (HTTP-OK, not an error) when the embedding
/// provider is unusable — that is the default install state and the UI shows
/// a setup hint instead.
pub(super) async fn handle_kb_collections(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let cfg = state.config.read().await.clone();
    match ensure_kb_manager(&cfg, state).await {
        Err(reason) => WsResponse::ok(
            &req.id,
            serde_json::json!({ "configured": false, "reason": reason, "collections": [] }),
        ),
        Ok(manager) => match manager.list_collections().await {
            Ok(collections) => WsResponse::ok(
                &req.id,
                serde_json::json!({
                    "configured": true,
                    "reason": null,
                    "collections": collections,
                }),
            ),
            Err(e) => WsResponse::err(&req.id, "INTERNAL_ERROR", e.to_string()),
        },
    }
}

/// `kb.docs` — list the ingestion records of one collection. An unknown
/// collection is an empty list, not an error.
pub(super) async fn handle_kb_docs(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct DocsParams {
        collection: String,
    }
    let p: DocsParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let cfg = state.config.read().await.clone();
    let manager = match ensure_kb_manager(&cfg, state).await {
        Ok(m) => m,
        Err(reason) => return WsResponse::err(&req.id, "KB_NOT_CONFIGURED", reason),
    };
    match manager.list(Some(&p.collection), None).await {
        Ok(docs) => {
            WsResponse::ok(&req.id, serde_json::json!({ "collection": p.collection, "docs": docs }))
        }
        Err(e) => WsResponse::err(&req.id, "INTERNAL_ERROR", e.to_string()),
    }
}

/// `kb.ingest` — upload one document (base64) into an agent's collection.
///
/// The bytes land at a stable path (`{agent_dir}/kb-uploads/{filename}`) so
/// `source_id` stays meaningful across re-uploads and checksum dedupe works;
/// any previous version of the document is deleted first (clean replace).
pub(super) async fn handle_kb_ingest(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct IngestParams {
        agent_id: String,
        filename: String,
        content_base64: String,
    }
    let p: IngestParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let agent_id = p.agent_id.trim();
    if agent_id.is_empty() {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "agent_id is required");
    }

    // Sanitize the filename to its final component (reject paths / traversal).
    let filename = match std::path::Path::new(p.filename.trim()).file_name() {
        Some(name) => name.to_string_lossy().to_string(),
        None => {
            return WsResponse::err(&req.id, "INVALID_PARAMS", "filename is required");
        }
    };

    // Pure input validation before any filesystem/manager work.
    let bytes = match base64::engine::general_purpose::STANDARD.decode(&p.content_base64) {
        Ok(b) => b,
        Err(e) => {
            return WsResponse::err(&req.id, "INVALID_CONTENT", format!("Invalid base64: {e}"))
        }
    };
    if bytes.len() > MAX_KB_UPLOAD_BYTES {
        return WsResponse::err(
            &req.id,
            "INVALID_CONTENT",
            format!("File exceeds maximum size of {} MB", MAX_KB_UPLOAD_BYTES / (1024 * 1024)),
        );
    }

    let agent_dir = state.paths.agent_dir(agent_id);
    if !agent_dir.is_dir() {
        return WsResponse::err(&req.id, "INVALID_PARAMS", format!("Unknown agent '{agent_id}'"));
    }

    let cfg = state.config.read().await.clone();
    let manager = match ensure_kb_manager(&cfg, state).await {
        Ok(m) => m,
        Err(reason) => return WsResponse::err(&req.id, "KB_NOT_CONFIGURED", reason),
    };

    let uploads_dir = agent_dir.join("kb-uploads");
    if let Err(e) = tokio::fs::create_dir_all(&uploads_dir).await {
        return WsResponse::err(
            &req.id,
            "INTERNAL_ERROR",
            format!("Failed to create upload directory: {e}"),
        );
    }
    let file_path = uploads_dir.join(&filename);
    if let Err(e) = tokio::fs::write(&file_path, &bytes).await {
        return WsResponse::err(&req.id, "INTERNAL_ERROR", format!("Failed to write upload: {e}"));
    }

    let collection = crate::rag::ingestion::KnowledgeBaseManager::collection_name(agent_id);
    let doc_id = file_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| filename.clone());

    // Clean replace: drop any previous version so shrunk chunk counts never
    // leave orphaned vectors behind.
    if let Err(e) = manager.delete(&collection, Some(&doc_id)).await {
        warn!("KB: pre-ingest delete of '{doc_id}' failed: {e}");
    }

    // `collection` must stay None on the source — a set value would override
    // the collection parameter inside `ingest_source`.
    let source = KnowledgeSource {
        id: None,
        name: filename.clone(),
        source_type: SourceType::File {
            path: format!("kb-uploads/{filename}"),
        },
        pattern: None,
        collection: None,
        chunk_strategy: None,
    };
    let report = manager
        .ingest_source(&source, &collection, &agent_dir, true)
        .await;

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "collection": collection,
            "doc_id": doc_id,
            "report": report,
        }),
    )
}

/// `kb.doc_content` — read one document's source file for UI preview.
///
/// Resolves the document's `source_id` from the ingestion tracker and returns
/// the file content (text) capped at 256 KiB, or a `binary: true` marker for
/// non-text formats (pdf/docx/xlsx) which the UI cannot render inline.
/// URL sources have no local bytes → `NOT_SUPPORTED`.
pub(super) async fn handle_kb_doc_content(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct DocContentParams {
        collection: String,
        doc_id: String,
    }
    let p: DocContentParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if !p.collection.starts_with("kb-") || p.collection == "kb-" {
        return WsResponse::err(
            &req.id,
            "INVALID_PARAMS",
            "collection must be a per-agent collection (kb-{agent_id})",
        );
    }
    if p.doc_id.trim().is_empty() {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "doc_id is required");
    }

    let cfg = state.config.read().await.clone();
    let manager = match ensure_kb_manager(&cfg, state).await {
        Ok(m) => m,
        Err(reason) => return WsResponse::err(&req.id, "KB_NOT_CONFIGURED", reason),
    };

    let source_id = match manager.list(Some(&p.collection), None).await {
        Ok(records) => match records.into_iter().find(|r| r.doc_id == p.doc_id) {
            Some(r) => r.source_id,
            None => {
                return WsResponse::err(
                    &req.id,
                    "NOT_FOUND",
                    format!("No document '{}' in {}", p.doc_id, p.collection),
                )
            }
        },
        Err(e) => return WsResponse::err(&req.id, "INTERNAL_ERROR", e.to_string()),
    };

    if is_url_source(&source_id) {
        return WsResponse::err(
            &req.id,
            "NOT_SUPPORTED",
            "URL sources have no local bytes to preview",
        );
    }

    const MAX_READ_BYTES: usize = 256 * 1024;
    let file = std::path::PathBuf::from(&source_id);
    let meta = match tokio::fs::metadata(&file).await {
        Ok(m) => m,
        Err(e) => return WsResponse::err(&req.id, "NOT_FOUND", format!("stat failed: {e}")),
    };
    if meta.is_dir() {
        return WsResponse::err(&req.id, "INVALID_REQUEST", "Source path is a directory");
    }

    let handle = match tokio::fs::File::open(&file).await {
        Ok(f) => f,
        Err(e) => return WsResponse::err(&req.id, "READ_FAILED", format!("open failed: {e}")),
    };
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    if let Err(e) = handle
        .take((MAX_READ_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .await
    {
        return WsResponse::err(&req.id, "READ_FAILED", format!("read failed: {e}"));
    }
    let truncated = buf.len() > MAX_READ_BYTES;
    if truncated {
        buf.truncate(MAX_READ_BYTES);
    }
    let size = meta.len();

    let binary = buf.contains(&0) || String::from_utf8(buf.clone()).is_err();
    if binary {
        return WsResponse::ok(
            &req.id,
            serde_json::json!({
                "doc_id": p.doc_id,
                "size": size,
                "truncated": false,
                "binary": true,
            }),
        );
    }
    // UTF-8 validity was checked above; this cannot fail.
    let content = String::from_utf8_lossy(&buf).into_owned();
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "doc_id": p.doc_id,
            "size": size,
            "truncated": truncated,
            "binary": false,
            "content": content,
        }),
    )
}

/// True when a KB source_id points at a fetched URL rather than a local file.
fn is_url_source(source_id: &str) -> bool {
    source_id.starts_with("http://") || source_id.starts_with("https://")
}

/// `kb.delete_doc` — delete one document (vectors + tracker + uploaded file).
pub(super) async fn handle_kb_delete_doc(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct DeleteDocParams {
        collection: String,
        doc_id: String,
    }
    let p: DeleteDocParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if !p.collection.starts_with("kb-") || p.collection == "kb-" {
        return WsResponse::err(
            &req.id,
            "INVALID_PARAMS",
            "collection must be a per-agent collection (kb-{agent_id})",
        );
    }
    if p.doc_id.trim().is_empty() {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "doc_id is required");
    }

    let cfg = state.config.read().await.clone();
    let manager = match ensure_kb_manager(&cfg, state).await {
        Ok(m) => m,
        Err(reason) => return WsResponse::err(&req.id, "KB_NOT_CONFIGURED", reason),
    };

    // Remember the source path first so UI-uploaded files can be removed too
    // (prevents resurrection on the next agent re-ingest).
    let source_id = match manager.list(Some(&p.collection), None).await {
        Ok(records) => records
            .into_iter()
            .find(|r| r.doc_id == p.doc_id)
            .map(|r| r.source_id),
        Err(e) => return WsResponse::err(&req.id, "INTERNAL_ERROR", e.to_string()),
    };

    let report = match manager.delete(&p.collection, Some(&p.doc_id)).await {
        Ok(r) => r,
        Err(e) => return WsResponse::err(&req.id, "INTERNAL_ERROR", e.to_string()),
    };

    // Best-effort file cleanup: only files inside a `kb-uploads` dir are
    // UI-managed; kb.toml-managed sources are re-ingested by the watcher.
    if let Some(src) = &source_id {
        let src_path = std::path::Path::new(src);
        if src_path.components().any(|c| c.as_os_str() == "kb-uploads") {
            if let Some(name) = src_path.file_name() {
                let agent_id = p.collection.trim_start_matches("kb-");
                let upload = state
                    .paths
                    .agent_dir(agent_id)
                    .join("kb-uploads")
                    .join(name);
                match tokio::fs::remove_file(&upload).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        warn!("KB: failed to remove upload file {}: {e}", upload.display())
                    }
                }
            }
        }
    }

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "collection": p.collection,
            "doc_id": p.doc_id,
            "chunks_deleted": report.chunks_deleted,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;

    fn req(id: &str, params: Option<serde_json::Value>) -> WsRequest {
        WsRequest {
            frame_type: "req".into(),
            id: id.into(),
            method: "x".into(),
            params,
        }
    }

    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    #[tokio::test]
    async fn collections_unconfigured_reports_flag_not_error() {
        let state = state().await;
        let resp = handle_kb_collections(&req("r1", Some(serde_json::json!({}))), &state).await;
        assert!(resp.ok, "expected ok: {:?}", resp.error);
        let payload = resp.payload.unwrap();
        assert_eq!(payload["configured"], false);
        assert!(payload["reason"].is_string());
        assert!(payload["collections"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ingest_unknown_agent_is_invalid_params() {
        let state = state().await;
        let params = serde_json::json!({
            "agent_id": "no-such-agent",
            "filename": "a.md",
            "content_base64": base64::engine::general_purpose::STANDARD.encode("hi"),
        });
        let resp = handle_kb_ingest(&req("r1", Some(params)), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn ingest_bad_base64_is_invalid_content() {
        let state = state().await;
        let params = serde_json::json!({
            "agent_id": "x",
            "filename": "a.md",
            "content_base64": "!!!not-base64!!!",
        });
        let resp = handle_kb_ingest(&req("r1", Some(params)), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_CONTENT");
    }

    #[tokio::test]
    async fn delete_doc_rejects_non_kb_collection() {
        let state = state().await;
        let params = serde_json::json!({ "collection": "other", "doc_id": "a" });
        let resp = handle_kb_delete_doc(&req("r1", Some(params)), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn doc_content_rejects_non_kb_collection() {
        let state = state().await;
        let params = serde_json::json!({ "collection": "other", "doc_id": "a" });
        let resp = handle_kb_doc_content(&req("r1", Some(params)), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn doc_content_rejects_empty_doc_id() {
        let state = state().await;
        let params = serde_json::json!({ "collection": "kb-default", "doc_id": "  " });
        let resp = handle_kb_doc_content(&req("r1", Some(params)), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn ensure_kb_manager_unconfigured_gives_reason() {
        // Default config has no usable embedding provider — the manager must
        // not build, with a human-readable reason (no fs/network touched).
        let state = state().await;
        let cfg = GatewayConfig::default();
        let err = match ensure_kb_manager(&cfg, &state).await {
            Err(e) => e,
            Ok(_) => panic!("expected ensure_kb_manager to fail on default config"),
        };
        assert!(
            err.contains("embedding_api_key") || err.contains("provider"),
            "unexpected reason: {err}"
        );
    }
}
