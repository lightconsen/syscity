//! Agent workspace file browsing: workspace.list / workspace.read.
//!
//! Read-only access to an agent's workspace directory for the WebUI file
//! browser. Client paths are always relative to the resolved workspace root
//! and validated against traversal (segment checks + canonicalized prefix).

use std::path::{Path, PathBuf};

use super::*;
use tokio::io::AsyncReadExt;

/// Max bytes returned by `workspace.read` before truncation.
const MAX_READ_BYTES: usize = 256 * 1024;
/// Max entries returned per directory by `workspace.list`.
const MAX_ENTRIES: usize = 1000;

#[derive(Debug, Deserialize)]
struct ListParams {
    agent_id: Option<String>,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReadParams {
    agent_id: Option<String>,
    path: String,
}

/// Resolve the effective workspace root for `agent_id`.
///
/// Missing/empty/"default" resolves the default agent's workspace (shared
/// `~/.syscity/workspace` unless a `workspace_dir` is configured). Named
/// agents resolve via their live config when spawned, else personality +
/// persisted overrides. Returns None when the agent is unknown.
async fn resolve_workspace_root(
    state: &Arc<GatewayState>,
    agent_id: Option<&str>,
) -> Option<PathBuf> {
    match agent_id.filter(|s| !s.is_empty()) {
        None | Some("default") => {
            let config = state.config.read().await;
            let mut cfg = config.default_agent.clone();
            cfg.agent_id = Some("default".to_string());
            if cfg.workspace_dir.is_none() {
                // Mirror the ACP builder: an explicit global workspace_dir
                // applies to the default agent too.
                cfg.workspace_dir = config
                    .workspace_dir
                    .as_ref()
                    .map(crate::dirs::resolve_tilde);
            }
            Some(cfg.resolve_workspace_dir(&state.paths))
        }
        Some(id) => {
            {
                let agents = state.agents.agents.read().await;
                if let Some(handle) = agents.get(id) {
                    return Some(handle.config.resolve_workspace_dir(&state.paths));
                }
            }
            let personality = {
                let registry = state.agents.registry.read().await;
                registry.get(id).cloned()
            };
            match personality {
                Some(p) => {
                    let mut cfg = p.to_agent_config();
                    // `to_agent_config` leaves agent_id/workspace_dir None;
                    // stamp the id or resolution would fall back to the
                    // shared default workspace.
                    cfg.agent_id = Some(id.to_string());
                    {
                        let config = state.config.read().await;
                        config.apply_agent_overrides(id, &mut cfg);
                    }
                    Some(cfg.resolve_workspace_dir(&state.paths))
                }
                None => None,
            }
        }
    }
}

/// Pure segment-level validation of a client-supplied relative path.
///
/// Rejects absolute paths, backslashes, and empty / `.` / `..` segments, so
/// the joined path cannot escape the workspace root syntactically.
fn validate_rel_path(rel: &str) -> bool {
    !rel.starts_with('/')
        && !rel.contains('\\')
        && !rel
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
}

/// Join `rel` onto `root` after validation, then verify the canonicalized
/// result stays under the canonicalized root (symlink escape guard).
/// Returns None when the path is invalid, escapes, or does not exist.
async fn resolve_within(root: &Path, rel: &str) -> Option<PathBuf> {
    if !rel.is_empty() && !validate_rel_path(rel) {
        return None;
    }
    let canon_root = tokio::fs::canonicalize(root).await.ok()?;
    let target = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let canon = tokio::fs::canonicalize(&target).await.ok()?;
    if canon.starts_with(&canon_root) {
        Some(canon)
    } else {
        None
    }
}

pub(super) async fn handle_workspace_list(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let params: ListParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let root = match resolve_workspace_root(state, params.agent_id.as_deref()).await {
        Some(r) => r,
        None => return error_agent_not_found(&req.id),
    };
    let rel = params.path.unwrap_or_default();
    if !rel.is_empty() && !validate_rel_path(&rel) {
        return WsResponse::err(&req.id, "PATH_FORBIDDEN", "Path escapes workspace root");
    }

    // A missing workspace dir is an empty listing, not an error.
    let dir = match resolve_within(&root, &rel).await {
        Some(d) => d,
        None if rel.is_empty() || tokio::fs::metadata(&root).await.is_err() => {
            return WsResponse::ok(
                &req.id,
                serde_json::json!({
                    "root": root.display().to_string(),
                    "path": rel,
                    "entries": [],
                }),
            );
        }
        None => {
            return WsResponse::err(&req.id, "NOT_FOUND", "Directory not found");
        }
    };
    let meta = match tokio::fs::metadata(&dir).await {
        Ok(m) => m,
        Err(e) => return WsResponse::err(&req.id, "READ_FAILED", format!("stat failed: {e}")),
    };
    if !meta.is_dir() {
        return WsResponse::err(&req.id, "INVALID_REQUEST", "Path is not a directory");
    }

    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) => return WsResponse::err(&req.id, "READ_FAILED", format!("read_dir failed: {e}")),
    };
    let mut entries: Vec<serde_json::Value> = Vec::new();
    loop {
        match rd.next_entry().await {
            Ok(Some(entry)) => {
                let name = entry.file_name().to_string_lossy().to_string();
                let child_rel = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                let meta = entry.metadata().await.ok();
                let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                entries.push(serde_json::json!({
                    "name": name,
                    "path": child_rel,
                    "kind": if is_dir { "dir" } else { "file" },
                    "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
                    "modified": meta
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs()),
                }));
                if entries.len() >= MAX_ENTRIES {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                return WsResponse::err(&req.id, "READ_FAILED", format!("read_dir failed: {e}"));
            }
        }
    }
    // Directories first, then files, each sorted by name (case-insensitive).
    entries.sort_by(|a, b| {
        let a_dir = a["kind"] == "dir";
        let b_dir = b["kind"] == "dir";
        b_dir.cmp(&a_dir).then_with(|| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "root": root.display().to_string(),
            "path": rel,
            "entries": entries,
        }),
    )
}

pub(super) async fn handle_workspace_read(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let params: ReadParams = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let root = match resolve_workspace_root(state, params.agent_id.as_deref()).await {
        Some(r) => r,
        None => return error_agent_not_found(&req.id),
    };
    if params.path.is_empty() || !validate_rel_path(&params.path) {
        return WsResponse::err(&req.id, "PATH_FORBIDDEN", "Path escapes workspace root");
    }
    let file = match resolve_within(&root, &params.path).await {
        Some(f) => f,
        None => return WsResponse::err(&req.id, "NOT_FOUND", "File not found"),
    };
    let meta = match tokio::fs::metadata(&file).await {
        Ok(m) => m,
        Err(e) => return WsResponse::err(&req.id, "READ_FAILED", format!("stat failed: {e}")),
    };
    if meta.is_dir() {
        return WsResponse::err(&req.id, "INVALID_REQUEST", "Path is a directory");
    }
    let size = meta.len();

    let file_handle = match tokio::fs::File::open(&file).await {
        Ok(f) => f,
        Err(e) => return WsResponse::err(&req.id, "READ_FAILED", format!("open failed: {e}")),
    };
    let mut buf = Vec::new();
    if let Err(e) = file_handle
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

    // Binary sniff: NUL byte or invalid UTF-8 → report as binary, no content.
    if buf.contains(&0) {
        return WsResponse::ok(
            &req.id,
            serde_json::json!({
                "path": params.path,
                "size": size,
                "truncated": false,
                "binary": true,
            }),
        );
    }
    match String::from_utf8(buf) {
        Ok(content) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "path": params.path,
                "size": size,
                "truncated": truncated,
                "binary": false,
                "content": content,
            }),
        ),
        Err(_) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "path": params.path,
                "size": size,
                "truncated": false,
                "binary": true,
            }),
        ),
    }
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

    /// Point the default agent's workspace at a fresh temp dir and return it.
    async fn state_with_tmp_workspace() -> (Arc<GatewayState>, PathBuf) {
        let state = state().await;
        let dir = std::env::temp_dir().join(format!("syscity_ws_test_{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        {
            let mut guard = state.config.write().await;
            let config = Arc::make_mut(&mut guard);
            config.default_agent.workspace_dir = Some(dir.clone());
        }
        (state, dir)
    }

    #[test]
    fn validate_rel_path_rejects_traversal() {
        for bad in [
            "..",
            "../etc/passwd",
            "/etc/passwd",
            "a/../b.md",
            "a//b.md",
            "a\\..\\b.md",
            "./a.md",
            "a/./b",
        ] {
            assert!(!validate_rel_path(bad), "should reject {bad:?}");
        }
        for good in ["a.md", "sub/b.txt", "a/b/c", ".hidden", "sub dir/x.md"] {
            assert!(validate_rel_path(good), "should accept {good:?}");
        }
    }

    #[tokio::test]
    async fn resolve_root_default_agent_uses_shared_workspace() {
        let state = state().await;
        let root = resolve_workspace_root(&state, None).await.unwrap();
        assert_eq!(root, state.paths.workspace_data_dir());
        let root = resolve_workspace_root(&state, Some("default"))
            .await
            .unwrap();
        assert_eq!(root, state.paths.workspace_data_dir());
        let root = resolve_workspace_root(&state, Some("")).await.unwrap();
        assert_eq!(root, state.paths.workspace_data_dir());
    }

    #[tokio::test]
    async fn resolve_root_named_agent_uses_own_workspace() {
        let state = state().await;
        let personality = crate::agent::AgentPersonality {
            id: "alice".into(),
            identity: "Alice".into(),
            is_valid: true,
            ..Default::default()
        };
        state
            .agents
            .registry
            .write()
            .await
            .insert_for_test(personality);

        let root = resolve_workspace_root(&state, Some("alice")).await.unwrap();
        assert_eq!(root, state.paths.agent_workspace_dir("alice"));

        let missing = resolve_workspace_root(&state, Some("ghost")).await;
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn list_returns_dirs_first_and_entries() {
        let (state, dir) = state_with_tmp_workspace().await;
        tokio::fs::write(dir.join("a.md"), "# hello").await.unwrap();
        tokio::fs::create_dir_all(dir.join("sub")).await.unwrap();
        tokio::fs::write(dir.join("sub").join("b.txt"), "world")
            .await
            .unwrap();

        let resp = handle_workspace_list(&req("r1", Some(serde_json::json!({}))), &state).await;
        assert!(resp.ok);
        let payload = resp.payload.unwrap();
        let entries = payload["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["name"], "sub");
        assert_eq!(entries[0]["kind"], "dir");
        assert_eq!(entries[1]["name"], "a.md");
        assert_eq!(entries[1]["kind"], "file");

        let resp =
            handle_workspace_list(&req("r2", Some(serde_json::json!({ "path": "sub" }))), &state)
                .await;
        assert!(resp.ok);
        let entries = resp.payload.unwrap()["entries"].as_array().unwrap().clone();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["path"], "sub/b.txt");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn list_missing_workspace_returns_empty() {
        let state = state().await;
        {
            let mut guard = state.config.write().await;
            let config = Arc::make_mut(&mut guard);
            config.default_agent.workspace_dir = Some(PathBuf::from("/nonexistent/ws/xyz"));
        }
        let resp = handle_workspace_list(&req("r1", Some(serde_json::json!({}))), &state).await;
        assert!(resp.ok);
        assert!(resp.payload.unwrap()["entries"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn list_rejects_traversal() {
        let (state, dir) = state_with_tmp_workspace().await;
        for bad in ["../escape", "/etc/passwd", "a/../b"] {
            let resp =
                handle_workspace_list(&req("r1", Some(serde_json::json!({ "path": bad }))), &state)
                    .await;
            assert!(!resp.ok, "should reject {bad:?}");
            assert_eq!(resp.error.as_ref().unwrap().code, "PATH_FORBIDDEN");
        }
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn read_returns_content_and_rejects_dir() {
        let (state, dir) = state_with_tmp_workspace().await;
        tokio::fs::write(dir.join("a.md"), "# hello").await.unwrap();

        let resp =
            handle_workspace_read(&req("r1", Some(serde_json::json!({ "path": "a.md" }))), &state)
                .await;
        assert!(resp.ok);
        let payload = resp.payload.unwrap();
        assert_eq!(payload["content"], "# hello");
        assert_eq!(payload["binary"], false);
        assert_eq!(payload["truncated"], false);

        let resp =
            handle_workspace_read(&req("r2", Some(serde_json::json!({ "path": "." }))), &state)
                .await;
        assert!(!resp.ok); // "." fails segment validation

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn read_truncates_large_and_flags_binary() {
        let (state, dir) = state_with_tmp_workspace().await;
        let big = "x".repeat(MAX_READ_BYTES + 100);
        tokio::fs::write(dir.join("big.txt"), big).await.unwrap();
        tokio::fs::write(dir.join("bin.dat"), b"a\0b")
            .await
            .unwrap();

        let resp = handle_workspace_read(
            &req("r1", Some(serde_json::json!({ "path": "big.txt" }))),
            &state,
        )
        .await;
        assert!(resp.ok);
        let payload = resp.payload.unwrap();
        assert_eq!(payload["truncated"], true);
        assert_eq!(payload["content"].as_str().unwrap().len(), MAX_READ_BYTES);

        let resp = handle_workspace_read(
            &req("r2", Some(serde_json::json!({ "path": "bin.dat" }))),
            &state,
        )
        .await;
        assert!(resp.ok);
        let payload = resp.payload.unwrap();
        assert_eq!(payload["binary"], true);
        assert!(payload.get("content").is_none());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
