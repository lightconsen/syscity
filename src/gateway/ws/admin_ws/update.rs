//! WS admin handlers: update.

use std::sync::Arc;
use std::time::Duration;

use super::super::{WsRequest, WsResponse};
use crate::gateway::state::{GatewayState, UpdateStatusCache};

// ── Update ──────────────────────────────────────────────────────────────

/// How long a release check is reused before GitHub is asked again.
const STATUS_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// How long a *failed* check is reused. Short enough that a transient network
/// blip does not hide a real release for six hours, long enough that the UI's
/// periodic poll does not ask GitHub again every few minutes while offline.
const STATUS_FAILURE_TTL: Duration = Duration::from_secs(60);

/// A check must never block the WS dispatch loop indefinitely.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// `update.status` — current/latest release info.
///
/// This is the only way the UI learns whether updates are usable: it renders
/// the buttons disabled unless `enabled` is true. It therefore has to report
/// the configuration, not just whatever the cache holds — the cache starts
/// empty and is only filled by a check, so reading it alone answered
/// `{enabled: false}` forever and greyed out "Check & Update" even though
/// `[update].enabled` was true.
///
/// A check is *never* awaited here. The WS dispatcher handles one frame at a
/// time per connection (`ws/core.rs`), so waiting on GitHub would stall every
/// later `chat.send` on that connection for as long as the request takes —
/// and this is polled at app startup. The reply carries the cached answer (or
/// "up to date" before the first one lands) and a background refresh follows;
/// the UI picks the result up on its next poll, or immediately when it asked
/// with `params.refresh = true`.
pub(crate) async fn handle_update_status(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    let enabled = state.config.read().await.update.enabled;
    let embedded = state.embedded;

    let force = req
        .params
        .as_ref()
        .and_then(|p| p.get("refresh"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let info = match cached_info(state).await {
        // Fresh, and the caller does not want a re-check: answer from cache.
        Some((info, fresh)) if fresh && !force => info,
        // Stale, missing, or explicitly refreshed: answer with what we have
        // and refresh behind the reply.
        Some((info, _)) => {
            spawn_check(state);
            info
        }
        None => {
            spawn_check(state);
            crate::update::UpdateInfo::up_to_date(crate::VERSION)
        }
    };

    let checking = state
        .update
        .checking
        .load(std::sync::atomic::Ordering::Acquire);
    WsResponse::ok(&req.id, status_payload(enabled, embedded, checking, info))
}

/// The cached release info and whether it is still fresh.
async fn cached_info(state: &Arc<GatewayState>) -> Option<(crate::update::UpdateInfo, bool)> {
    state
        .update
        .status_cache
        .read()
        .await
        .as_ref()
        .map(|c| (c.info.clone(), c.checked_at.elapsed() < c.ttl))
}

/// Start a release check in the background, unless one is already running.
fn spawn_check(state: &Arc<GatewayState>) {
    use std::sync::atomic::Ordering;
    if state.update.checking.swap(true, Ordering::AcqRel) {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let _ = refresh_cache(&state).await;
        state.update.checking.store(false, Ordering::Release);
    });
}

/// The `update.status` payload. `enabled` comes from the configuration and is
/// what the UI gates both buttons on, so it is never derived from whether a
/// check happened to succeed.
fn status_payload(
    enabled: bool,
    embedded: bool,
    checking: bool,
    info: crate::update::UpdateInfo,
) -> serde_json::Value {
    serde_json::json!({
        "enabled": enabled,
        "current": info.current,
        "latest": info.latest,
        "update_available": info.update_available,
        "embedded": embedded,
        "checking": checking,
    })
}

/// Query GitHub and store the answer (a failure included, with a shorter TTL).
///
/// Returns the info now in the cache. Offline, that is the last good answer —
/// never "an update is available", which the caller could not act on.
async fn refresh_cache(state: &Arc<GatewayState>) -> crate::update::UpdateInfo {
    state
        .update
        .checks_total
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // A hung request must not hold the connection's dispatcher: the check is
    // its own task, and the timeout bounds how long that task lives.
    let client = reqwest::Client::builder()
        .timeout(CHECK_TIMEOUT)
        .build()
        .unwrap_or_default();
    match crate::update::github::check_latest(&client, crate::VERSION).await {
        Ok(info) => {
            *state.update.status_cache.write().await = Some(UpdateStatusCache {
                info: info.clone(),
                checked_at: std::time::Instant::now(),
                ttl: STATUS_TTL,
            });
            info
        }
        Err(e) => {
            tracing::warn!("Update status check failed: {e}");
            let previous = state
                .update
                .status_cache
                .read()
                .await
                .as_ref()
                .map(|c| c.info.clone())
                .unwrap_or_else(|| crate::update::UpdateInfo::up_to_date(crate::VERSION));
            *state.update.status_cache.write().await = Some(UpdateStatusCache {
                info: previous.clone(),
                checked_at: std::time::Instant::now(),
                ttl: STATUS_FAILURE_TTL,
            });
            previous
        }
    }
}

/// `update.progress` — current update phase/percent (polled).
pub(crate) async fn handle_update_progress(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    let progress = state.update.progress.read().await;
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "phase": progress.phase,
            "percent": progress.percent,
            "error": progress.error,
            "current": progress.current,
            "latest": progress.latest,
        }),
    )
}

/// `update.trigger` — start the self-update flow (same checks as
/// `POST /api/v1/update`). Rejected when embedded (desktop uses the Tauri
/// updater instead) or disabled in config.
pub(crate) async fn handle_update_trigger(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    use crate::gateway::handlers::update::{run_update_task, set_progress};
    use crate::gateway::state::{UpdatePhase, UpdateProgress};

    if state.embedded {
        return WsResponse::err(
            &req.id,
            "CONFLICT",
            "This syscity instance is embedded in the desktop app; use the desktop updater instead.",
        );
    }
    if !state.config.read().await.update.enabled {
        return WsResponse::err(
            &req.id,
            "FORBIDDEN",
            "Online updates are disabled in the configuration.",
        );
    }
    {
        let progress = state.update.progress.read().await;
        let busy = matches!(
            progress.phase,
            UpdatePhase::Checking
                | UpdatePhase::Downloading
                | UpdatePhase::Verifying
                | UpdatePhase::Applying
                | UpdatePhase::Restarting
        );
        if busy {
            return WsResponse::err(&req.id, "CONFLICT", "An update is already in progress.");
        }
    }

    *state.update.progress.write().await = UpdateProgress::idle(crate::VERSION);
    set_progress(state, UpdatePhase::Checking, 5, None).await;

    let host = state.config.read().await.host.clone();
    let port = state.config.read().await.port;
    let task_state = state.clone();
    let shutdown_token = state.shutdown_token.clone();
    let handle = tokio::spawn(async move {
        run_update_task(task_state, shutdown_token, host, port).await;
    });
    state
        .task_registry
        .insert_join("update:apply", handle)
        .await;

    WsResponse::ok(&req.id, serde_json::json!({ "status": "started" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::UpdateInfo;

    #[test]
    fn enabled_reflects_the_configuration_not_the_cache() {
        // The regression: the cache starts empty, and the handler used to
        // answer `{enabled: false}` whenever it was. The UI disables both
        // buttons on `enabled`, so a configured-to-on gateway showed
        // "Online updates are disabled in the gateway configuration."
        let off = status_payload(false, false, false, UpdateInfo::up_to_date(crate::VERSION));
        assert_eq!(off["enabled"], false);
        assert_eq!(off["update_available"], false);
        assert_eq!(off["current"], crate::VERSION);

        let on = status_payload(
            true,
            false,
            true,
            UpdateInfo {
                current: "0.3.19".into(),
                latest: "0.3.20".into(),
                update_available: true,
            },
        );
        assert_eq!(on["enabled"], true);
        assert_eq!(on["current"], "0.3.19");
        assert_eq!(on["latest"], "0.3.20");
        assert_eq!(on["update_available"], true);
        // The check runs off the request path, so the reply has to say so:
        // the UI polls again until this clears.
        assert_eq!(on["checking"], true);
    }

    #[test]
    fn embedded_is_reported_so_the_ui_can_explain_the_tauri_path() {
        let payload = status_payload(true, true, false, UpdateInfo::up_to_date(crate::VERSION));
        assert_eq!(payload["embedded"], true);
        assert_eq!(payload["checking"], false);
    }

    #[test]
    fn a_failed_check_is_cached_briefly_and_a_good_one_for_hours() {
        // The TTL is what keeps an offline UI from re-asking GitHub on every
        // poll, and what keeps a transient failure from hiding a release all
        // day. Pin the relationship rather than the numbers.
        assert!(STATUS_FAILURE_TTL < STATUS_TTL);
        assert!(
            STATUS_FAILURE_TTL <= Duration::from_secs(5 * 60),
            "a failure cached longer than a few minutes would outlive the blip it recorded"
        );
        assert!(CHECK_TIMEOUT < STATUS_FAILURE_TTL);
    }
}
