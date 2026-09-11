//! cloud.kb.docs / push / pull — local KB ↔ cloud backup sync.
//!
//! The cloud is backup storage only: the local collections (`kb-{agent_id}`)
//! stay the single truth source and the only retrieval path. `push` uploads
//! UI-managed documents whose full-file SHA-256 differs from the cloud copy;
//! `pull` restores a cloud backup into an agent's collection (download →
//! verify sha → write → clean-replace ingest).

use std::sync::Arc;

use super::super::{WsRequest, WsResponse};
#[cfg(feature = "cloud")]
use super::cloud_kb_client;
#[cfg(not(feature = "cloud"))]
use super::cloud_unavailable;
use crate::gateway::GatewayState;

#[cfg(feature = "cloud")]
use std::collections::HashMap;

#[cfg(feature = "cloud")]
use serde::Deserialize;

#[cfg(feature = "cloud")]
use super::super::parse_params;

#[cfg(feature = "cloud")]
use crate::cloud::client::CloudClient;

#[cfg(feature = "cloud")]
use crate::gateway::init::services::ensure_kb_manager;

#[cfg(feature = "cloud")]
use crate::rag::ingestion::{compute_checksum, detect_mime, KnowledgeSource, SourceType};

/// Cloud-side per-document cap (`MAX_DOC_BYTES` in syscity-cloud kb.ts).
#[cfg(feature = "cloud")]
const MAX_SYNC_DOC_BYTES: usize = 50 * 1024 * 1024;

/// `cloud.kb.pull` params: `{collection}` or `{cloud_kb_id, agent_id}`.
#[cfg(feature = "cloud")]
#[derive(Deserialize)]
struct PullParams {
    collection: Option<String>,
    cloud_kb_id: Option<String>,
    agent_id: Option<String>,
}

/// Resolve the pull target from its two param forms (pure — no fs/network).
/// Returns `(collection, agent_id, explicit_cloud_kb_id)`.
#[cfg(feature = "cloud")]
fn target_of(p: &PullParams) -> Result<(String, String, Option<String>), String> {
    if let Some(coll) = p
        .collection
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !valid_collection(coll) {
            return Err("collection must be a per-agent collection (kb-{agent_id})".into());
        }
        let agent = coll.trim_start_matches("kb-").to_string();
        Ok((coll.to_string(), agent, p.cloud_kb_id.clone()))
    } else {
        let kb_id = p.cloud_kb_id.as_deref().map(str::trim).unwrap_or("");
        let agent = p.agent_id.as_deref().map(str::trim).unwrap_or("");
        if kb_id.is_empty() || agent.is_empty() {
            return Err("provide {collection} or {cloud_kb_id, agent_id}".into());
        }
        Ok((format!("kb-{agent}"), agent.to_string(), Some(kb_id.to_string())))
    }
}

/// `cloud.kb.docs` — list the documents stored in a cloud KB backup.
pub(crate) async fn handle_cloud_kb_docs(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[cfg(feature = "cloud")]
    #[derive(Deserialize)]
    struct DocsParams {
        kb_id: String,
    }
    #[cfg(feature = "cloud")]
    {
        let p: DocsParams = match parse_params(req) {
            Ok(p) => p,
            Err(res) => return res,
        };
        let kb_id = p.kb_id.trim();
        if kb_id.is_empty() {
            return WsResponse::err(&req.id, "INVALID_PARAMS", "kb_id is required");
        }
        let client = match cloud_kb_client(req, state).await {
            Ok(c) => c,
            Err(res) => return res,
        };
        match client.kb_list_documents(kb_id).await {
            Ok(docs) => {
                WsResponse::ok(&req.id, serde_json::json!({ "kb_id": kb_id, "documents": docs }))
            }
            Err(e) => WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
        }
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = state;
        cloud_unavailable(req)
    }
}

/// `cloud.kb.push` — back up one local collection to the cloud.
///
/// Uploads every tracker-recorded document whose bytes live under
/// `{agent_dir}/kb-uploads/` (UI-managed uploads) and whose full-file SHA-256
/// differs from the cloud copy. URL sources and kb.toml-managed files are
/// skipped — the watcher owns those, and pushing them could fight the
/// tracker's doc_id.
pub(crate) async fn handle_cloud_kb_push(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[cfg(feature = "cloud")]
    #[derive(Deserialize)]
    struct PushParams {
        collection: String,
    }
    #[cfg(feature = "cloud")]
    {
        let p: PushParams = match parse_params(req) {
            Ok(p) => p,
            Err(res) => return res,
        };
        let collection = p.collection.trim().to_string();
        if !valid_collection(&collection) {
            return WsResponse::err(
                &req.id,
                "INVALID_PARAMS",
                "collection must be a per-agent collection (kb-{agent_id})",
            );
        }
        let agent_id = collection.trim_start_matches("kb-").to_string();
        let uploads_dir = state.paths.agent_dir(&agent_id).join("kb-uploads");

        let client = match cloud_kb_client(req, state).await {
            Ok(c) => c,
            Err(res) => return res,
        };
        let cfg = state.config.read().await.clone();
        let manager = match ensure_kb_manager(&cfg, state).await {
            Ok(m) => m,
            Err(reason) => return WsResponse::err(&req.id, "KB_NOT_CONFIGURED", reason),
        };
        let records = match manager.list(Some(&collection), None).await {
            Ok(r) => r,
            Err(e) => return WsResponse::err(&req.id, "INTERNAL_ERROR", e.to_string()),
        };

        // Find or create the cloud KB by name (= collection name).
        let (cloud_kb_id, cloud_kb_name) = match find_or_create_kb(&client, &collection).await {
            Ok(v) => v,
            Err(e) => return WsResponse::err(&req.id, "BAD_GATEWAY", e),
        };
        let cloud_docs = match client.kb_list_documents(&cloud_kb_id).await {
            Ok(d) => d,
            Err(e) => return WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
        };
        let cloud_sha: HashMap<String, String> = cloud_docs
            .into_iter()
            .filter(|d| d.status == "stored")
            .filter_map(|d| d.sha256.map(|s| (d.filename, s)))
            .collect();

        let mut pushed: usize = 0;
        let mut unchanged: usize = 0;
        let mut skipped_url: usize = 0;
        let mut skipped_external: usize = 0;
        let mut too_large: usize = 0;
        let mut failed: usize = 0;
        let mut errors: Vec<String> = Vec::new();

        for rec in &records {
            let source = std::path::Path::new(&rec.source_id);
            if rec.source_id.starts_with("http://") || rec.source_id.starts_with("https://") {
                skipped_url += 1;
                continue;
            }
            if !source.starts_with(&uploads_dir) {
                skipped_external += 1;
                continue;
            }
            let filename = match source.file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None => {
                    failed += 1;
                    errors.push(format!("{}: source path has no filename", rec.doc_id));
                    continue;
                }
            };
            let bytes = match tokio::fs::read(source).await {
                Ok(b) => b,
                Err(e) => {
                    failed += 1;
                    errors.push(format!("{filename}: read failed: {e}"));
                    continue;
                }
            };
            if bytes.len() > MAX_SYNC_DOC_BYTES {
                too_large += 1;
                continue;
            }
            let sha = sha256_hex(&bytes);
            if cloud_sha.get(&filename).map(String::as_str) == Some(sha.as_str()) {
                unchanged += 1;
                continue;
            }
            match client
                .kb_upload(&cloud_kb_id, &filename, &bytes, detect_mime(source))
                .await
            {
                Ok(_) => pushed += 1,
                Err(e) => {
                    failed += 1;
                    errors.push(format!("{filename}: {e}"));
                }
            }
        }

        WsResponse::ok(
            &req.id,
            serde_json::json!({
                "collection": collection,
                "cloud_kb_id": cloud_kb_id,
                "cloud_kb_name": cloud_kb_name,
                "total": records.len(),
                "pushed": pushed,
                "unchanged": unchanged,
                "skipped_url": skipped_url,
                "skipped_external": skipped_external,
                "too_large": too_large,
                "failed": failed,
                "errors": errors,
            }),
        )
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = state;
        cloud_unavailable(req)
    }
}

/// `cloud.kb.pull` — restore a cloud backup into a local agent collection.
///
/// `{collection}` picks the backup whose cloud KB name equals the collection
/// name; `{cloud_kb_id, agent_id}` names them explicitly (the collection is
/// then derived as `kb-{agent_id}`). Missing or changed files are downloaded
/// and re-ingested so the agent can immediately retrieve them.
pub(crate) async fn handle_cloud_kb_pull(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[cfg(feature = "cloud")]
    {
        let p: PullParams = match parse_params(req) {
            Ok(p) => p,
            Err(res) => return res,
        };

        // Target: `{collection}` or `{cloud_kb_id, agent_id}`.
        let (collection, agent_id, explicit_kb_id) = match target_of(&p) {
            Ok(t) => t,
            Err(msg) => return WsResponse::err(&req.id, "INVALID_PARAMS", msg),
        };

        let client = match cloud_kb_client(req, state).await {
            Ok(c) => c,
            Err(res) => return res,
        };

        let agent_dir = state.paths.agent_dir(&agent_id);
        if !agent_dir.is_dir() {
            return WsResponse::err(
                &req.id,
                "INVALID_PARAMS",
                format!("Unknown agent '{agent_id}'"),
            );
        }
        let uploads_dir = agent_dir.join("kb-uploads");

        let cfg = state.config.read().await.clone();
        let manager = match ensure_kb_manager(&cfg, state).await {
            Ok(m) => m,
            Err(reason) => return WsResponse::err(&req.id, "KB_NOT_CONFIGURED", reason),
        };

        let cloud_kb_id = match explicit_kb_id {
            Some(id) => id,
            None => {
                // Find the backup by name (= collection name).
                let kbs = match client.list_kbs().await {
                    Ok(v) => v,
                    Err(e) => return WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
                };
                match find_kb_by_name(&kbs, &collection) {
                    Some(id) => id,
                    None => {
                        return WsResponse::err(
                            &req.id,
                            "NOT_FOUND",
                            format!("No cloud backup named '{collection}'"),
                        )
                    }
                }
            }
        };

        let cloud_docs = match client.kb_list_documents(&cloud_kb_id).await {
            Ok(d) => d,
            Err(e) => return WsResponse::err(&req.id, "BAD_GATEWAY", e.to_string()),
        };

        // Tracker checksums by doc_id — decides whether a (re-)ingest is
        // needed after the file bytes are current.
        let tracked: HashMap<String, Option<String>> =
            match manager.list(Some(&collection), None).await {
                Ok(records) => records
                    .into_iter()
                    .map(|r| (r.doc_id, r.checksum))
                    .collect(),
                Err(e) => return WsResponse::err(&req.id, "INTERNAL_ERROR", e.to_string()),
            };

        let mut pulled: usize = 0;
        let mut unchanged: usize = 0;
        let mut failed: usize = 0;
        let mut errors: Vec<String> = Vec::new();

        for doc in &cloud_docs {
            let filename = match std::path::Path::new(&doc.filename).file_name() {
                Some(n) => n.to_string_lossy().to_string(),
                None => {
                    failed += 1;
                    errors.push(format!("{}: invalid filename '{}'", doc.id, doc.filename));
                    continue;
                }
            };
            if doc.status != "stored" {
                failed += 1;
                errors.push(format!("{filename}: cloud status '{}', not stored", doc.status));
                continue;
            }
            let cloud_sha = match doc.sha256.as_deref() {
                Some(s) => s,
                None => {
                    failed += 1;
                    errors.push(format!("{filename}: missing sha256"));
                    continue;
                }
            };

            // Make the local file match the cloud copy (download when missing
            // or changed, verifying the sha afterwards).
            let local = uploads_dir.join(&filename);
            let mut bytes = tokio::fs::read(&local).await.unwrap_or_default();
            if sha256_hex(&bytes) != cloud_sha {
                let dl = match client.kb_download_document(&cloud_kb_id, &doc.id).await {
                    Ok(d) => d,
                    Err(e) => {
                        failed += 1;
                        errors.push(format!("{filename}: download failed: {e}"));
                        continue;
                    }
                };
                if sha256_hex(&dl.bytes) != cloud_sha {
                    failed += 1;
                    errors.push(format!("{filename}: sha256 mismatch after download"));
                    continue;
                }
                if let Err(e) = tokio::fs::create_dir_all(&uploads_dir).await {
                    failed += 1;
                    errors.push(format!("{filename}: mkdir failed: {e}"));
                    continue;
                }
                if let Err(e) = tokio::fs::write(&local, &dl.bytes).await {
                    failed += 1;
                    errors.push(format!("{filename}: write failed: {e}"));
                    continue;
                }
                bytes = dl.bytes;
            }

            // Re-ingest when the tracker doesn't match the file bytes (clean
            // replace: drop old chunks first so shrunk docs leave no orphans).
            let doc_id = std::path::Path::new(&filename)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| filename.clone());
            let local_checksum = compute_checksum(&bytes);
            if tracked.get(&doc_id).cloned().flatten().as_deref() == Some(local_checksum.as_str()) {
                unchanged += 1;
                continue;
            }
            if let Err(e) = manager.delete(&collection, Some(&doc_id)).await {
                tracing::warn!("KB pull: pre-ingest delete of '{doc_id}' failed: {e}");
            }
            // `collection` stays None — a set value would override the
            // collection parameter inside `ingest_source`.
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
            if report.docs_indexed > 0 {
                pulled += 1;
            } else {
                failed += 1;
                errors.push(format!("{filename}: {}", report.errors.join("; ")));
            }
        }

        WsResponse::ok(
            &req.id,
            serde_json::json!({
                "collection": collection,
                "agent_id": agent_id,
                "cloud_kb_id": cloud_kb_id,
                "total": cloud_docs.len(),
                "pulled": pulled,
                "unchanged": unchanged,
                "failed": failed,
                "errors": errors,
            }),
        )
    }
    #[cfg(not(feature = "cloud"))]
    {
        let _ = state;
        cloud_unavailable(req)
    }
}

/// A collection param must name a per-agent collection (`kb-{agent_id}`).
#[cfg(feature = "cloud")]
fn valid_collection(collection: &str) -> bool {
    collection.starts_with("kb-") && collection.len() > 3
}

/// Full-file SHA-256 (hex). Unlike the tracker's `compute_checksum` (first
/// 4 KiB + length), this matches the cloud's `sha256HexBuf` exactly.
#[cfg(feature = "cloud")]
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Find a cloud KB id by exact name in a `cloud.kb.list` payload.
#[cfg(feature = "cloud")]
fn find_kb_by_name(list_kbs_value: &serde_json::Value, name: &str) -> Option<String> {
    list_kbs_value
        .get("knowledge_bases")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .find(|kb| kb.get("name").and_then(|n| n.as_str()) == Some(name))
        .and_then(|kb| kb.get("id").and_then(|i| i.as_str()).map(String::from))
}

/// Find a cloud KB by name or create it (`push` find-or-create).
#[cfg(feature = "cloud")]
async fn find_or_create_kb(client: &CloudClient, name: &str) -> Result<(String, String), String> {
    let kbs = client.list_kbs().await.map_err(|e| e.to_string())?;
    if let Some(id) = find_kb_by_name(&kbs, name) {
        return Ok((id, name.to_string()));
    }
    let created = client.kb_create(name).await.map_err(|e| e.to_string())?;
    let id = created
        .get("id")
        .and_then(|i| i.as_str())
        .ok_or_else(|| "cloud kb_create returned no id".to_string())?
        .to_string();
    Ok((id, name.to_string()))
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
            method: "cloud.kb.sync".into(),
            params,
        }
    }

    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    /// With cloud disabled (default config), all three sync methods must
    /// report UNAUTHORIZED — hermetically, with no network or fs writes.
    #[tokio::test]
    async fn sync_methods_unauthorized_when_cloud_disabled() {
        let state = state().await;
        let cases: Vec<(&str, Option<serde_json::Value>)> = vec![
            ("cloud.kb.docs", Some(serde_json::json!({ "kb_id": "kb1" }))),
            ("cloud.kb.push", Some(serde_json::json!({ "collection": "kb-agent1" }))),
            (
                "cloud.kb.pull",
                Some(serde_json::json!({ "cloud_kb_id": "kb1", "agent_id": "agent1" })),
            ),
        ];
        for (method, params) in cases {
            let resp = match method {
                "cloud.kb.docs" => handle_cloud_kb_docs(&req("r1", params), &state).await,
                "cloud.kb.push" => handle_cloud_kb_push(&req("r2", params), &state).await,
                _ => handle_cloud_kb_pull(&req("r3", params), &state).await,
            };
            assert!(!resp.ok, "{method} unexpectedly succeeded");
            assert_eq!(
                resp.error.as_ref().unwrap().code,
                "UNAUTHORIZED",
                "{method} wrong error code"
            );
        }
    }

    /// Param validation order (params before the cloud gate) only exists in
    /// cloud builds; without the feature every method short-circuits to
    /// cloud_unavailable (UNAUTHORIZED) — covered by
    /// `sync_methods_unauthorized_when_cloud_disabled` above.
    #[cfg(feature = "cloud")]
    #[tokio::test]
    async fn push_rejects_non_kb_collection() {
        let state = state().await;
        for collection in ["other", "kb-", ""] {
            let resp = handle_cloud_kb_push(
                &req("r1", Some(serde_json::json!({ "collection": collection }))),
                &state,
            )
            .await;
            assert!(!resp.ok, "collection '{collection}' unexpectedly ok");
            assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
        }
    }

    #[cfg(feature = "cloud")]
    #[tokio::test]
    async fn push_requires_collection_param() {
        let state = state().await;
        // Missing field → the params deserializer rejects it as malformed.
        let resp = handle_cloud_kb_push(&req("r1", Some(serde_json::json!({}))), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_REQUEST");
    }

    #[cfg(feature = "cloud")]
    #[tokio::test]
    async fn pull_requires_collection_or_explicit_ids() {
        let state = state().await;
        for params in [
            serde_json::json!({}),
            serde_json::json!({ "cloud_kb_id": "kb1" }),
            serde_json::json!({ "agent_id": "agent1" }),
            serde_json::json!({ "collection": "" }),
        ] {
            let resp = handle_cloud_kb_pull(&req("r1", Some(params.clone())), &state).await;
            assert!(!resp.ok, "params {params} unexpectedly ok");
            assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
        }
    }

    #[cfg(feature = "cloud")]
    #[tokio::test]
    async fn pull_rejects_non_kb_collection() {
        let state = state().await;
        let resp = handle_cloud_kb_pull(
            &req("r1", Some(serde_json::json!({ "collection": "shared" }))),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }
}
