//! WS admin handlers: skills.

use std::sync::Arc;

use tracing::info;

use super::super::{WsRequest, WsResponse};
use crate::gateway::GatewayState;

// ── Skills ──────────────────────────────────────────────────────────────

/// `skills.get` — one skill (`{ name }`).
pub(crate) async fn handle_skills_get(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let name = match super::required_str_param(req, "name") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let sm = state.tools.skills_manager.read().await;
    match sm.get_skill(&name).await {
        Some(skill) => WsResponse::ok(&req.id, serde_json::to_value(&skill).unwrap_or_default()),
        None => WsResponse::err(&req.id, "NOT_FOUND", "skill not found"),
    }
}

/// `skills.enable` / `skills.disable` — `{ id, enabled }`.
pub(crate) async fn handle_skills_set_enabled(
    req: &WsRequest,
    state: &Arc<GatewayState>,
    enabled: bool,
) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let mut sm = state.tools.skills_manager.write().await;
    match sm.set_skill_enabled(&id, enabled).await {
        Ok(()) => WsResponse::ok(&req.id, serde_json::json!({ "success": true, "id": id })),
        Err(e) => WsResponse::err(&req.id, "NOT_FOUND", e.to_string()),
    }
}

/// `skills.uninstall` — remove a skill (`{ name }`).
pub(crate) async fn handle_skills_uninstall(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let name = match super::required_str_param(req, "name") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let sm = state.tools.skills_manager.read().await;
    match sm.uninstall_skill(&name).await {
        Ok(_) => WsResponse::ok(&req.id, serde_json::json!({ "success": true })),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
    }
}

/// `skills.run` — activate a skill (`{ id }`).
pub(crate) async fn handle_skills_run(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let sm = state.tools.skills_manager.read().await;
    match sm.activate_skill(&id).await {
        Ok(_) => WsResponse::ok(&req.id, serde_json::json!({ "success": true, "id": id })),
        Err(e) => WsResponse::err(&req.id, "NOT_FOUND", e.to_string()),
    }
}

// ── Install lifecycle (pin / rollback / provenance) ──────────────────────

/// `skills.install_source` — install a skill from a git URL or local path
/// (`{ source, name? }`).
///
/// The daemon fetches, checks and installs, rather than the CLI doing it
/// client-side: the guard runs *before* anything lands (the old path wrote the
/// bytes first and let the filesystem watcher refuse them afterwards), the
/// install is recorded, and the running daemon reloads without waiting on a
/// watcher event.
///
/// `https://` and local paths only. A plain `http://` URL is refused, and so is
/// any URL resolving to a private address — the daemon fetching a caller-chosen
/// URL is an SSRF surface, and "install a skill from" is not a reason to reach
/// the operator's internal network.
pub(crate) async fn handle_skills_install_source(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(serde::Deserialize)]
    struct Params {
        source: String,
        name: Option<String>,
    }
    let p: Params = match super::super::parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    let name = p.name.clone().unwrap_or_else(|| {
        p.source
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&p.source)
            .trim_end_matches(".git")
            .to_string()
    });
    if name.is_empty() {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "could not derive a skill name");
    }

    let is_url = p.source.starts_with("http://")
        || p.source.starts_with("https://")
        || p.source.starts_with("git@");

    // A staging directory beside the skills dir: the fetch lands here, the
    // guard reads it, and only then does anything become a skill.
    let staging_root = crate::dirs::skills_dir().join(".staging");
    if let Err(e) = tokio::fs::create_dir_all(&staging_root).await {
        return WsResponse::err(&req.id, "INTERNAL", format!("cannot create staging dir: {e}"));
    }
    let staging = staging_root.join(&name);
    let _ = tokio::fs::remove_dir_all(&staging).await;

    let fetched: crate::Result<()> = if is_url {
        if let Err(e) = guard_remote_source(&p.source).await {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return WsResponse::err(&req.id, "INVALID_PARAMS", e.to_string());
        }
        info!("Cloning skill '{name}' from {}", p.source);
        match tokio::process::Command::new("git")
            .args(["clone", "--depth=1", &p.source, &staging.to_string_lossy()])
            .status()
            .await
        {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(crate::error::SyscityError::ExternalService {
                source: format!("git clone of '{}' exited {status}", p.source),
                cause: None,
            }),
            Err(e) => Err(crate::error::SyscityError::ExternalService {
                source: format!("could not run git: {e}"),
                cause: None,
            }),
        }
    } else {
        let src = std::path::Path::new(&p.source);
        if !src.exists() {
            return WsResponse::err(
                &req.id,
                "NOT_FOUND",
                format!("source path does not exist: {}", p.source),
            );
        }
        crate::plugins::installer::copy_dir_all(src, &staging).await
    };
    if let Err(e) = fetched {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return WsResponse::err(&req.id, "BAD_GATEWAY", format!("fetch failed: {e}"));
    }

    // Guard before install: a package that cannot load must not land.
    if let Err(e) = crate::skills::SkillManager::precheck_skill_package(&staging).await {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return WsResponse::err(&req.id, "INVALID_PARAMS", format!("refused: {e}"));
    }

    let sm = state.tools.skills_manager.read().await;
    let user_dir = sm.user_dir().to_path_buf();
    if let Err(e) = sm.install_from_directory(&staging, &name).await {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return WsResponse::err(&req.id, "INTERNAL", e.to_string());
    }
    drop(sm);

    // Provenance, so `skills.versions` can say where this came from.
    let installed_dir = user_dir.join(&name);
    let kind = if is_url { "git" } else { "local" };
    let _ = crate::skills::install_state::save(
        &installed_dir,
        &crate::skills::install_state::InstallState {
            version: None,
            source: Some(format!("{kind}:{}", p.source)),
            installed_at: Some(chrono::Utc::now()),
            ..Default::default()
        },
    )
    .await;

    let _ = tokio::fs::remove_dir_all(&staging).await;

    // Load it now; the install is not finished while the skill is inert.
    let reload = {
        let sm = state.tools.skills_manager.read().await;
        sm.reload().await
    };
    match reload {
        Ok(count) => WsResponse::ok(
            &req.id,
            serde_json::json!({ "success": true, "id": name, "skills_loaded": count }),
        ),
        Err(e) => WsResponse::err(&req.id, "INTERNAL", e.to_string()),
    }
}

/// Refuse a remote source the daemon should not fetch.
async fn guard_remote_source(source: &str) -> crate::Result<()> {
    let url = reqwest::Url::parse(source).map_err(|e| {
        crate::error::SyscityError::Validation(format!("'{source}' is not a URL: {e}"))
    })?;
    if url.scheme() != "https" {
        return Err(crate::error::SyscityError::Validation(format!(
            "refusing '{}': only https URLs are fetched",
            url.scheme()
        )));
    }
    let host = url
        .host_str()
        .ok_or_else(|| crate::error::SyscityError::Validation("URL has no host".to_string()))?;
    // A literal IP is checked directly; a name has to be resolved first, since
    // `https://127.0.0.1.nip.io/` is not a private IP as written.
    use std::net::ToSocketAddrs;
    let addrs: Vec<std::net::SocketAddr> = format!("{host}:443")
        .to_socket_addrs()
        .map_err(|e| {
            crate::error::SyscityError::Validation(format!("cannot resolve '{host}': {e}"))
        })?
        .collect();
    if addrs.is_empty() {
        return Err(crate::error::SyscityError::Validation(format!(
            "'{host}' resolved to no addresses"
        )));
    }
    if let Some(private) = addrs
        .iter()
        .find(|a| crate::browser::navigation_guard::is_private_ip(a.ip()))
    {
        return Err(crate::error::SyscityError::Validation(format!(
            "refusing '{host}': resolves to a private address ({})",
            private.ip()
        )));
    }
    Ok(())
}
/// `skills.versions` — a skill's install record and rollback history (`{ id }`).
pub(crate) async fn handle_skills_versions(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let id = match super::required_str_param(req, "id") {
        Ok(v) => v,
        Err(res) => return res,
    };
    let sm = state.tools.skills_manager.read().await;
    match sm.skill_install_state(&id).await {
        Ok((record, history)) => WsResponse::ok(
            &req.id,
            serde_json::json!({
                "id": id,
                "version": record.version,
                "source": record.source,
                "sha256": record.sha256,
                "pinned": record.pinned,
                "installed_at": record.installed_at,
                "usage": record.usage,
                "history": history,
            }),
        ),
        Err(e) => WsResponse::err(&req.id, "NOT_FOUND", e.to_string()),
    }
}

/// `skills.pin` — hold or release a skill's version (`{ id, pinned }`).
pub(crate) async fn handle_skills_pin(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(serde::Deserialize)]
    struct Params {
        id: String,
        #[serde(default = "default_true")]
        pinned: bool,
    }
    fn default_true() -> bool {
        true
    }
    let p: Params = match super::super::parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let sm = state.tools.skills_manager.read().await;
    match sm.pin_skill(&p.id, p.pinned).await {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({ "success": true, "id": p.id, "pinned": p.pinned }),
        ),
        Err(e) => WsResponse::err(&req.id, "NOT_FOUND", e.to_string()),
    }
}

/// `skills.rollback` — restore a version kept under `.history/` (`{ id, version }`).
pub(crate) async fn handle_skills_rollback(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(serde::Deserialize)]
    struct Params {
        id: String,
        version: String,
    }
    let p: Params = match super::super::parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    let sm = state.tools.skills_manager.read().await;
    match sm.rollback_skill(&p.id, &p.version).await {
        Ok(()) => WsResponse::ok(
            &req.id,
            serde_json::json!({ "success": true, "id": p.id, "version": p.version }),
        ),
        Err(e) => WsResponse::err(&req.id, "NOT_FOUND", e.to_string()),
    }
}

#[cfg(test)]
mod source_guard_tests {
    use super::guard_remote_source;

    #[tokio::test]
    async fn refuses_a_non_https_scheme() {
        let err = guard_remote_source("http://github.com/x/y.git")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("only https"), "{err}");
    }

    #[tokio::test]
    async fn refuses_a_loopback_literal() {
        // The daemon fetching a caller-chosen URL is an SSRF surface; a skill
        // install is not a reason to reach the operator's own machine.
        for source in [
            "https://127.0.0.1/x/y.git",
            "https://10.0.0.5/x/y.git",
            "https://192.168.1.10/x/y.git",
            "https://[::1]/x/y.git",
        ] {
            let err = guard_remote_source(source).await.unwrap_err();
            assert!(
                format!("{err}").contains("private address"),
                "{source} was not refused as private: {err}"
            );
        }
    }

    #[tokio::test]
    async fn refuses_a_name_that_resolves_to_loopback() {
        // Not a literal IP, so the check has to resolve first.
        let err = guard_remote_source("https://localhost/x/y.git")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("private address"), "{err}");
    }
}
