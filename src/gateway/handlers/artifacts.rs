//! Artifact serving handler
//!
//! Serves `write_report` documents for the document preview feature. Reports
//! live under each producing agent's own workspace (`<workspace>/artifacts/`);
//! reports written before that move stay in the legacy `~/.syscity/artifacts/`
//! directory and remain reachable via their old URLs.
//!
//! URL shapes (the route is a wildcard):
//! - `/api/v1/artifacts/@<agent_id>/<file>` — an agent's workspace artifacts
//!   (`@default` = the shared default workspace).
//! - `/api/v1/artifacts/@<agent_id>/<root>/<task>/<file>` — delegation-scoped
//!   report inside that agent's workspace.
//! - `/api/v1/artifacts/[<root>/<task>/]<file>` — legacy global artifacts dir.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
};

use crate::dirs::SyscityPaths;
use crate::gateway::GatewayState;

#[derive(Debug, serde::Deserialize)]
pub struct ArtifactExportQuery {
    /// Export conversion target (currently only "pptx" for slides canvases).
    to: Option<String>,
}

/// Resolve a canvas `<img src>` to bytes, relative to the artifact's own
/// directory. Anything escaping that directory is skipped (the converter
/// drops the image rather than failing the export).
struct ArtifactImageResolver {
    base: std::path::PathBuf,
}

impl crate::office::slides::ImageResolver for ArtifactImageResolver {
    fn resolve(&self, src: &str) -> Option<(Vec<u8>, String)> {
        if src.starts_with('/') || src.contains("..") || src.starts_with("http") {
            return None;
        }
        let path = self.base.join(src);
        let canon = path.canonicalize().ok()?;
        if !canon.starts_with(&self.base) {
            return None;
        }
        let bytes = std::fs::read(&canon).ok()?;
        let format = canon
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("png")
            .trim_start_matches('.')
            .to_string();
        Some((bytes, format))
    }
}

/// Convert an authored-HTML artifact to a real Office file download.
///
/// `target` is one of "pptx" | "docx" | "xlsx". The conversion is CPU-bound
/// and synchronous (HTML parse + OOXML zip), so it runs off the async runtime.
async fn export_document(
    file_path: std::path::PathBuf,
    filename: String,
    target: &str,
) -> axum::response::Response {
    let html = match tokio::fs::read_to_string(&file_path).await {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "Document not found".to_string(),
            )
                .into_response();
        }
    };

    let base = file_path.parent().map(|p| p.to_path_buf());
    let title = filename.clone();
    let target_owned = target.to_string();
    let result = tokio::task::spawn_blocking(move || match target_owned.as_str() {
        "pptx" => {
            let resolver = ArtifactImageResolver { base: base.unwrap_or_default() };
            crate::office::slides::canvas_html_to_pptx(&html, &title, &resolver)
        }
        "docx" => {
            let resolver = ArtifactImageResolver {
                base: base.clone().unwrap_or_default(),
            };
            crate::office::docx::flow_html_to_docx(&html, &resolver)
        }
        "xlsx" => crate::office::xlsx::tables_html_to_xlsx(&html),
        _ => Err(format!("unsupported export target '{target_owned}'")),
    })
    .await;

    match result {
        Ok(Ok(bytes)) => {
            let stem = filename.trim_end_matches(".html").trim_end_matches(".md");
            let download_name = format!("{stem}.{target}");
            let content_type = match target {
                "pptx" => {
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                }
                "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                _ => "application/octet-stream",
            };
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, content_type.to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{download_name}\""),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        Ok(Err(e)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("Conversion failed: {e}"),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("Conversion panicked: {e}"),
        )
            .into_response(),
    }
}

/// Resolve a request path to its on-disk location, honoring the `@owner`
/// prefix for agent-workspace artifacts and falling back to the legacy global
/// directory. Returns `None` for unsafe paths.
fn resolve_artifact_path(paths: &SyscityPaths, path: &str) -> Option<std::path::PathBuf> {
    // Path traversal protection: reject absolute paths, ".." / "." / empty
    // segments, backslashes, and percent-encoded variants, so the joined path
    // can never escape its root.
    let lower = path.to_lowercase();
    if path.starts_with('/')
        || lower.contains("%2e")
        || lower.contains("%2f")
        || path.contains('\\')
        || path
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return None;
    }

    let (root, rel) = if let Some(rest) = path.strip_prefix('@') {
        let (owner, rel) = rest.split_once('/')?;
        // The owner segment must be an agent id — the same charset the
        // write side allows (alphanumeric, '-', '_').
        if owner.is_empty()
            || !owner
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return None;
        }
        let ws = if owner == "default" {
            paths.workspace_data_dir()
        } else {
            paths.agent_workspace_dir(owner)
        };
        (ws.join("artifacts"), rel)
    } else {
        (paths.artifacts_dir(), path)
    };

    Some(root.join(std::path::Path::new(rel)))
}

/// GET /api/v1/artifacts/*path  (+ `?to=pptx` for on-demand conversion)
///
/// Reads a document from the resolved artifacts location and returns it with
/// the appropriate Content-Type (text/markdown for .md, text/html for .html).
/// With `?to=pptx|docx|xlsx` the document must be authored HTML and the
/// response is the converted Office file download instead of the source.
pub async fn artifact_handler(
    State(state): State<Arc<GatewayState>>,
    Path(path): Path<String>,
    Query(query): Query<ArtifactExportQuery>,
) -> impl IntoResponse {
    let Some(file_path) = resolve_artifact_path(&state.paths, &path) else {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "Access denied".to_string(),
        )
            .into_response();
    };

    // Note: we intentionally skip canonicalize() here because on macOS it
    // fails on Unicode filenames due to NFC/NFD normalization differences
    // between the HTTP-decoded path and the filesystem-stored path.
    // The write_report tool already validates paths server-side.

    if let Some(to) = &query.to {
        if matches!(to.as_str(), "pptx" | "docx" | "xlsx") {
            let filename = path
                .rsplit('/')
                .next()
                .unwrap_or("document.html")
                .to_string();
            return export_document(file_path, filename, to).await;
        }
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("Unsupported export target '{to}' (supported: pptx, docx, xlsx)"),
        )
            .into_response();
    }

    match tokio::fs::read_to_string(&file_path).await {
        Ok(content) => {
            let mime = if path.ends_with(".html") {
                "text/html; charset=utf-8"
            } else {
                "text/markdown; charset=utf-8"
            };
            (StatusCode::OK, [(header::CONTENT_TYPE, mime)], content).into_response()
        }
        Err(_) => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "Document not found".to_string(),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;

    /// A state whose `paths` root is a temp dir, so these tests never read or
    /// write the real `~/.syscity`.
    async fn state() -> Arc<GatewayState> {
        Arc::new(make_test_state(GatewayConfig::default()).await)
    }

    fn query(to: Option<&str>) -> Query<ArtifactExportQuery> {
        Query(ArtifactExportQuery { to: to.map(str::to_string) })
    }

    async fn get(
        state: &Arc<GatewayState>,
        path: &str,
        to: Option<&str>,
    ) -> axum::response::Response {
        artifact_handler(State(state.clone()), Path(path.to_string()), query(to))
            .await
            .into_response()
    }

    #[tokio::test]
    async fn test_traversal_paths_rejected() {
        let state = state().await;
        for bad in [
            "..",
            "../etc/passwd",
            "/etc/passwd",
            "a/../b.md",
            "a//b.md",
            "%2e%2e/x",
            "%2fetc%2fpasswd",
            "a\\..\\b.md",
            "./a.md",
        ] {
            let resp = get(&state, bad, None).await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "should reject {:?}", bad);
        }
    }

    #[tokio::test]
    async fn test_nested_tree_path_served() {
        // A tree-scoped artifact is stored under artifacts/<root>/<task>/<file>
        // and served from the matching URL.
        let state = state().await;
        let dir = state.paths.artifacts_dir().join("root-t").join("task-t");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("r.md"), "# hi from tree")
            .await
            .unwrap();

        let resp = get(&state, "root-t/task-t/r.md", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("# hi from tree"));
    }

    #[tokio::test]
    async fn test_agent_workspace_path_served() {
        // Agent-workspace artifacts are addressed with an `@<owner>` prefix
        // and served from that workspace's artifacts dir.
        let state = state().await;
        let dir = state.paths.workspace_data_dir().join("artifacts");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("ws-test.md"), "# hi from workspace")
            .await
            .unwrap();

        let resp = get(&state, "@default/ws-test.md", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("# hi from workspace"));
    }

    #[tokio::test]
    async fn test_owner_segment_validated() {
        let state = state().await;
        for bad in ["@../x.md", "@a.b/x.md", "@default"] {
            let resp = get(&state, bad, None).await;
            assert_ne!(resp.status(), StatusCode::OK, "should not serve {:?}", bad);
        }
    }

    #[tokio::test]
    async fn test_slides_export_returns_pptx() {
        let state = state().await;
        let dir = state.paths.artifacts_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(
            dir.join("export-deck.html"),
            r#"<div class="slide"><div style="position:absolute;left:80px;top:60px;width:1120px">标题页</div></div>"#,
        )
        .await
        .unwrap();

        let resp = get(&state, "export-deck.html", Some("pptx")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(ct.contains("presentationml"), "content-type: {ct}");
        let body = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        // A pptx is a zip package.
        assert_eq!(&body[..2], b"PK");
    }

    #[tokio::test]
    async fn test_docx_and_xlsx_export() {
        let state = state().await;
        let dir = state.paths.artifacts_dir();
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("export-doc.html"), "<h1>标题</h1><p>正文</p>")
            .await
            .unwrap();
        tokio::fs::write(
            dir.join("export-sheet.html"),
            "<table><thead><tr><th>A</th></tr></thead><tbody><tr><td>1</td></tr></tbody></table>",
        )
        .await
        .unwrap();

        let docx = get(&state, "export-doc.html", Some("docx")).await;
        assert_eq!(docx.status(), StatusCode::OK);
        let ct = docx
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(ct.contains("wordprocessingml"), "content-type: {ct}");
        let body = axum::body::to_bytes(docx.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(&body[..2], b"PK");

        let xlsx = get(&state, "export-sheet.html", Some("xlsx")).await;
        assert_eq!(xlsx.status(), StatusCode::OK);
        let ct = xlsx
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(ct.contains("spreadsheetml"), "content-type: {ct}");
        let body = axum::body::to_bytes(xlsx.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(&body[..2], b"PK");
    }

    #[tokio::test]
    async fn test_export_rejects_unknown_target() {
        let state = state().await;
        let resp = get(&state, "x.md", Some("pdf")).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
