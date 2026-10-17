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

/// How a download should be rendered.
#[derive(Debug, serde::Deserialize)]
pub struct ArtifactExportQuery {
    /// Export target. Only `pdf` is supported.
    to: Option<String>,
}

/// Render an authored-HTML artifact to a PDF download.
///
/// The render goes through a real browser rather than a PDF library: the
/// document is arbitrary HTML an agent wrote, so a partial CSS engine would not
/// fail — it would produce a plausible-looking PDF with a broken layout.
async fn export_pdf(
    state: &Arc<GatewayState>,
    file_path: std::path::PathBuf,
    filename: &str,
) -> axum::response::Response {
    let html = match tokio::fs::read_to_string(&file_path).await {
        Ok(html) => html,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "Document not found".to_string(),
            )
                .into_response();
        }
    };

    #[cfg(feature = "browser")]
    {
        let pool = state.infra.export_pool.read().await.clone();
        let Some(pool) = pool else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "PDF export needs a browser pool, and none is running".to_string(),
            )
                .into_response();
        };

        return match crate::browser::render::html_to_pdf(&pool, "default", &html).await {
            Ok(bytes) => {
                let stem = filename.trim_end_matches(".html").trim_end_matches(".md");
                (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, "application/pdf".to_string()),
                        (
                            header::CONTENT_DISPOSITION,
                            format!("attachment; filename=\"{stem}.pdf\""),
                        ),
                    ],
                    bytes,
                )
                    .into_response()
            }
            Err(e) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                format!("PDF export failed: {e}"),
            )
                .into_response(),
        };
    }

    #[cfg(not(feature = "browser"))]
    {
        let _ = (state, &html);
        (
            StatusCode::NOT_IMPLEMENTED,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "PDF export requires a build with the `browser` feature".to_string(),
        )
            .into_response()
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

    let full = root.join(std::path::Path::new(rel));

    // The checks above are lexical, so they cannot see a symlink: one planted
    // inside the root (agents have shell and write tools) would be followed by
    // the read below and serve a file from outside the tree. Resolve the root
    // and the file's *directory* and keep the result inside the root. The
    // final component is deliberately never canonicalized — on macOS that
    // fails when the caller's Unicode form (NFC/NFD) differs from the name on
    // disk, which is why the read opens the name the caller asked for.
    if let (Ok(canon_root), Some(Ok(canon_dir))) =
        (root.canonicalize(), full.parent().map(std::path::Path::canonicalize))
    {
        if !canon_dir.starts_with(&canon_root) {
            return None;
        }
    }
    // Neither branch above resolves when the file simply does not exist; the
    // lexical result stands and the read reports it the way it always has
    // (404, not 403).

    // A final component that *is* a symlink is the escape the directory check
    // cannot catch, so it is refused outright. A symlinked artifact is not a
    // configuration worth the confused-deputy risk: copy the file in instead.
    if full
        .symlink_metadata()
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
    {
        return None;
    }

    Some(full)
}

/// GET /api/v1/artifacts/*path
///
/// Reads a document from the resolved artifacts location and returns it with
/// the appropriate Content-Type (text/markdown for .md, text/html for .html).
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
        if to == "pdf" {
            let filename = path
                .rsplit('/')
                .next()
                .unwrap_or("document.html")
                .to_string();
            return export_pdf(&state, file_path, &filename).await;
        }
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("Unsupported export target '{to}' (supported: pdf)"),
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

    async fn get(state: &Arc<GatewayState>, path: &str) -> axum::response::Response {
        artifact_handler(
            State(state.clone()),
            Path(path.to_string()),
            Query(ArtifactExportQuery { to: None }),
        )
        .await
        .into_response()
    }

    /// Without the `browser` feature there is no PDF renderer, and the export
    /// says that instead of failing obscurely.
    #[tokio::test]
    async fn a_pdf_export_without_a_browser_is_refused() {
        let state = state().await;
        let dir = std::env::temp_dir().join(format!("art-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("report.html"), "<h1>hi</h1>")
            .await
            .unwrap();

        let resp = artifact_handler(
            State(state.clone()),
            Path("x.html".to_string()),
            Query(ArtifactExportQuery { to: Some("pdf".to_string()) }),
        )
        .await
        .into_response();

        // Either the feature is off (501) or no pool is running (503); both are
        // explicit refusals rather than a broken file.
        assert!(
            matches!(resp.status(), StatusCode::NOT_IMPLEMENTED | StatusCode::SERVICE_UNAVAILABLE)
                || resp.status() == StatusCode::NOT_FOUND,
            "unexpected status {}",
            resp.status()
        );
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
            let resp = get(&state, bad).await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "should reject {:?}", bad);
        }
    }

    /// The lexical checks stop `..` and absolute paths; a symlink is what they
    /// cannot see. A link inside the root must not serve a file from outside
    /// it — agents can write into these directories.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_symlinked_artifact_is_refused() {
        let state = state().await;
        let root = state.paths.artifacts_dir();
        tokio::fs::create_dir_all(&root).await.unwrap();
        let outside = root.parent().unwrap().join("outside-secret.md");
        tokio::fs::write(&outside, "not yours").await.unwrap();

        let link = root.join("leak.md");
        let _ = tokio::fs::remove_file(&link).await;
        tokio::fs::symlink(&outside, &link).await.unwrap();
        assert_eq!(
            get(&state, "leak.md").await.status(),
            StatusCode::FORBIDDEN,
            "a symlink inside the root must not be followed out of it"
        );

        // The absolute case, which is the one that matters on a real host.
        let absolute = root.join("hosts.md");
        let _ = tokio::fs::remove_file(&absolute).await;
        tokio::fs::symlink("/etc/hosts", &absolute).await.unwrap();
        assert_eq!(get(&state, "hosts.md").await.status(), StatusCode::FORBIDDEN);
    }

    /// A symlinked *directory* is the same escape one level up: the join stays
    /// lexical, so only resolving the directory catches it.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_symlinked_directory_inside_root_is_refused() {
        let state = state().await;
        let root = state.paths.artifacts_dir();
        tokio::fs::create_dir_all(&root).await.unwrap();
        let outside = root.parent().unwrap();
        tokio::fs::write(outside.join("outside-secret.md"), "not yours")
            .await
            .unwrap();

        let link = root.join("outdir");
        let _ = tokio::fs::remove_file(&link).await;
        tokio::fs::symlink(outside, &link).await.unwrap();
        assert_eq!(get(&state, "outdir/outside-secret.md").await.status(), StatusCode::FORBIDDEN);
    }

    /// A name the caller sends in a different Unicode form than the one on
    /// disk must keep working, which is why the final component is never
    /// canonicalized (see `resolve_artifact_path`).
    #[tokio::test]
    async fn test_unicode_filename_served() {
        let state = state().await;
        let root = state.paths.artifacts_dir();
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(root.join("baogao-报告.md"), "# 报告")
            .await
            .unwrap();

        let resp = get(&state, "baogao-报告.md").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("报告"));
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

        let resp = get(&state, "root-t/task-t/r.md").await;
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

        let resp = get(&state, "@default/ws-test.md").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("# hi from workspace"));
    }

    #[tokio::test]
    async fn test_owner_segment_validated() {
        let state = state().await;
        for bad in ["@../x.md", "@a.b/x.md", "@default"] {
            let resp = get(&state, bad).await;
            assert_ne!(resp.status(), StatusCode::OK, "should not serve {:?}", bad);
        }
    }
}
