//! skills.list / skills.install handlers.

use super::*;
pub(super) async fn handle_skills_list(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let skills = {
        let sm = state.tools.skills_manager.read().await;
        sm.list_skills().await
    };
    let entries: Vec<_> = skills
        .into_iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "description": s.description,
                "version": s.version,
                "author": s.author,
                "triggers": s.triggers.iter().map(|t| {
                    serde_json::json!({
                        "type": format!("{:?}", t.trigger_type).to_lowercase(),
                        "pattern": t.pattern,
                    })
                }).collect::<Vec<_>>(),
                "depends_on": s.depends_on,
                "provides": s.provides,
                "chain": s.chain,
            })
        })
        .collect();
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "skills": entries,
            "count": entries.len(),
        }),
    )
}

pub(super) async fn handle_skills_install(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Debug, Deserialize)]
    struct InstallPayload {
        name: String,
        #[serde(default)]
        content: Option<String>,
        #[serde(default, rename = "zip_base64")]
        zip_base64: Option<String>,
    }
    let payload: InstallPayload = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let name = payload.name.trim();
    if name.is_empty() {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "Skill name is required");
    }

    let skills_dir = state.paths.skills_dir();
    let skill_dir = skills_dir.join(name);
    if let Err(e) = tokio::fs::create_dir_all(&skill_dir).await {
        return WsResponse::err(
            &req.id,
            "INTERNAL_ERROR",
            format!("Failed to create skill directory: {}", e),
        );
    }

    if let Some(zip_base64) = payload.zip_base64 {
        // Decode base64 ZIP
        let zip_bytes = match base64::engine::general_purpose::STANDARD.decode(&zip_base64) {
            Ok(b) => b,
            Err(e) => {
                return WsResponse::err(
                    &req.id,
                    "INVALID_CONTENT",
                    format!("Invalid base64: {}", e),
                )
            }
        };

        // Guard against oversized ZIPs before blocking the thread pool.
        const MAX_ZIP_BYTES: usize = 64 * 1024 * 1024;
        const MAX_ZIP_ENTRIES: usize = 10_000;
        const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;
        if zip_bytes.len() > MAX_ZIP_BYTES {
            return WsResponse::err(
                &req.id,
                "INVALID_CONTENT",
                format!("ZIP exceeds maximum size of {} MB", MAX_ZIP_BYTES / (1024 * 1024)),
            );
        }

        let skill_dir_clone = skill_dir.clone();
        // Extract ZIP synchronously (ZipFile is not Send)
        #[allow(clippy::type_complexity)]
        let extract_task: tokio::task::JoinHandle<
            Result<Vec<(std::path::PathBuf, Vec<u8>)>, String>,
        > = tokio::task::spawn_blocking(move || {
            let cursor = std::io::Cursor::new(zip_bytes);
            let mut archive = match zip::ZipArchive::new(cursor) {
                Ok(a) => a,
                Err(e) => return Err(format!("Invalid ZIP: {}", e)),
            };

            if archive.len() > MAX_ZIP_ENTRIES {
                return Err(format!("ZIP contains too many entries (max {})", MAX_ZIP_ENTRIES));
            }

            let mut files: Vec<(std::path::PathBuf, Vec<u8>)> = Vec::new();
            let mut total_uncompressed: usize = 0;
            for i in 0..archive.len() {
                let mut file = match archive.by_index(i) {
                    Ok(f) => f,
                    Err(e) => return Err(format!("ZIP read error: {}", e)),
                };
                let outpath = match file.enclosed_name() {
                    Some(p) => skill_dir_clone.join(p),
                    None => continue,
                };
                if !file.is_dir() {
                    let size = file.size() as usize;
                    if size > MAX_ENTRY_BYTES {
                        return Err(format!(
                            "ZIP entry '{}' exceeds maximum size of {} MB",
                            outpath.display(),
                            MAX_ENTRY_BYTES / (1024 * 1024)
                        ));
                    }
                    total_uncompressed = total_uncompressed.saturating_add(size);
                    if total_uncompressed > MAX_ZIP_BYTES {
                        return Err(format!(
                            "ZIP total uncompressed size exceeds {} MB",
                            MAX_ZIP_BYTES / (1024 * 1024)
                        ));
                    }
                    let mut contents = Vec::with_capacity(size);
                    if let Err(e) = std::io::Read::read_to_end(&mut file, &mut contents) {
                        return Err(format!("Failed to read ZIP entry: {}", e));
                    }
                    files.push((outpath, contents));
                }
            }
            Ok(files)
        });

        let files: Vec<(std::path::PathBuf, Vec<u8>)> =
            match tokio::time::timeout(std::time::Duration::from_secs(30), extract_task).await {
                Ok(Ok(Ok(f))) => f,
                Ok(Ok(Err(msg))) => return WsResponse::err(&req.id, "INVALID_CONTENT", msg),
                Ok(Err(_)) => {
                    return WsResponse::err(
                        &req.id,
                        "INTERNAL_ERROR",
                        "ZIP extraction task was cancelled".to_string(),
                    )
                }
                Err(_) => {
                    return WsResponse::err(
                        &req.id,
                        "INVALID_CONTENT",
                        "ZIP extraction timed out".to_string(),
                    )
                }
            };

        // Write extracted files
        for (outpath, contents) in files {
            if let Some(parent) = outpath.parent() {
                if !parent.exists() {
                    if let Err(e) = tokio::fs::create_dir_all(parent).await {
                        return WsResponse::err(
                            &req.id,
                            "INTERNAL_ERROR",
                            format!("Failed to create directory: {}", e),
                        );
                    }
                }
            }
            if let Err(e) = tokio::fs::write(&outpath, &contents).await {
                return WsResponse::err(
                    &req.id,
                    "INTERNAL_ERROR",
                    format!("Failed to write file: {}", e),
                );
            }
        }

        // Validate SKILL.md exists and is valid
        let skill_md_path = skill_dir.join("SKILL.md");
        if !skill_md_path.exists() {
            if let Err(e) = tokio::fs::remove_dir_all(&skill_dir).await {
                warn!("Failed to remove skill dir {}: {}", skill_dir.display(), e);
            }
            return WsResponse::err(
                &req.id,
                "INVALID_CONTENT",
                "ZIP must contain SKILL.md at the root",
            );
        }

        let skill_md_content = match tokio::fs::read_to_string(&skill_md_path).await {
            Ok(c) => c,
            Err(e) => {
                if let Err(rm_err) = tokio::fs::remove_dir_all(&skill_dir).await {
                    warn!("Failed to remove skill dir {}: {}", skill_dir.display(), rm_err);
                }
                return WsResponse::err(
                    &req.id,
                    "INVALID_CONTENT",
                    format!("Failed to read SKILL.md: {}", e),
                );
            }
        };

        if let Err(e) = crate::skills::parse_skill_md(&skill_md_content) {
            if let Err(rm_err) = tokio::fs::remove_dir_all(&skill_dir).await {
                warn!("Failed to remove skill dir {}: {}", skill_dir.display(), rm_err);
            }
            return WsResponse::err(&req.id, "INVALID_CONTENT", format!("Invalid SKILL.md: {}", e));
        }
    } else if let Some(content) = payload.content {
        // Legacy single-file install
        if let Err(e) = crate::skills::parse_skill_md(&content) {
            return WsResponse::err(
                &req.id,
                "INVALID_CONTENT",
                format!("Invalid skill markdown: {}", e),
            );
        }
        let skill_path = skill_dir.join("SKILL.md");
        if let Err(e) = tokio::fs::write(&skill_path, &content).await {
            return WsResponse::err(
                &req.id,
                "INTERNAL_ERROR",
                format!("Failed to write skill file: {}", e),
            );
        }
    } else {
        return WsResponse::err(
            &req.id,
            "INVALID_PARAMS",
            "Either content or zip_base64 is required",
        );
    }

    // Reload skills
    {
        let sm = state.tools.skills_manager.write().await;
        if let Err(e) = sm.load_all().await {
            return WsResponse::err(
                &req.id,
                "INTERNAL_ERROR",
                format!("Failed to reload skills: {}", e),
            );
        }
    }

    WsResponse::ok(&req.id, serde_json::json!({ "status": "installed", "name": name }))
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
    async fn skills_list_empty_ok() {
        let state = state().await;
        let resp = handle_skills_list(&req("r1", None), &state).await;
        assert!(resp.ok);
        let payload = resp.payload.as_ref().unwrap();
        assert_eq!(payload["count"], 0);
        assert!(payload["skills"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn skills_install_missing_params_errors() {
        let state = state().await;
        let resp = handle_skills_install(&req("r1", None), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_REQUEST");
    }

    #[tokio::test]
    async fn skills_install_empty_name_errors() {
        let state = state().await;
        let params = Some(serde_json::json!({ "name": "", "content": "x" }));
        let resp = handle_skills_install(&req("r1", params), &state).await;
        assert!(!resp.ok);
        assert_eq!(resp.error.as_ref().unwrap().code, "INVALID_PARAMS");
    }
}
