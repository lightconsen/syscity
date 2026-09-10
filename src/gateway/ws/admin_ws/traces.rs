//! WS admin handlers: traces.

use std::sync::Arc;

use serde::Deserialize;

use super::super::{parse_params, WsRequest, WsResponse};
use crate::gateway::GatewayState;

// ── Turn traces (replay) ────────────────────────────────────────────────

/// `traces.get` — replay a recorded agent turn (`{ turn_id }`). Returns the
/// turn summary + full event list. Formerly `GET /api/traces/:turn_id`.
pub(crate) async fn handle_traces_get(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        turn_id: String,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if p.turn_id.is_empty()
        || p.turn_id.contains('/')
        || p.turn_id.contains('\\')
        || p.turn_id.contains("..")
    {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "invalid turn_id");
    }

    // Turn dirs are `turns/YYYY-MM-DD/<turn_id>/`; the date isn't in the id, so
    // scan the date partitions (bounded — a local personal-assistant store).
    let base = state.paths.turns_dir();
    let mut turn_dir = None;
    if let Ok(rd) = std::fs::read_dir(&base) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join(&p.turn_id).is_dir() {
                turn_dir = Some(path.join(&p.turn_id));
                break;
            }
        }
    }
    let Some(dir) = turn_dir else {
        return WsResponse::err(&req.id, "NOT_FOUND", "turn not found");
    };

    let summary = std::fs::read_to_string(dir.join("summary.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let full_events = std::fs::read_to_string(dir.join("full.json"))
        .ok()
        .map(|s| {
            s.lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .collect::<Vec<_>>()
        });

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "turn_id": p.turn_id,
            "summary": summary,
            "full_trace": full_events,
        }),
    )
}
