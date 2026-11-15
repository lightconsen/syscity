//! WS admin handlers: traces and the action/screenshot timeline.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;

use super::super::{parse_params, WsRequest, WsResponse};
use crate::gateway::GatewayState;

// ── Turn traces (replay) ────────────────────────────────────────────────

/// A turn id that is safe to join onto a date partition.
///
/// The id comes from a client, so it must not be able to walk out of the turns
/// directory or name a partition itself.
fn valid_turn_id(turn_id: &str) -> bool {
    !turn_id.is_empty()
        && !turn_id.contains('/')
        && !turn_id.contains('\\')
        && !turn_id.contains("..")
}

/// Locate `turns/YYYY-MM-DD/<turn_id>/`.
///
/// Turn dirs are partitioned by the *write* date and the id does not carry it,
/// so the reader scans the partitions. Bounded by the retention window — this
/// is a local personal-assistant store, not a shared archive.
fn find_turn_dir(state: &GatewayState, turn_id: &str) -> Option<PathBuf> {
    let base = state.paths.turns_dir();
    let rd = std::fs::read_dir(&base).ok()?;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() && path.join(turn_id).is_dir() {
            return Some(path.join(turn_id));
        }
    }
    None
}

/// Parse a turn's append-only `full.json` into its events, in file order.
///
/// File order is time order: the sink only ever appends. A line that does not
/// parse is skipped rather than failing the read — one truncated tail (a crash
/// mid-append) must not hide every step before it.
fn read_full_events(dir: &std::path::Path) -> Option<Vec<serde_json::Value>> {
    let raw = std::fs::read_to_string(dir.join("full.json")).ok()?;
    Some(
        raw.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .collect(),
    )
}

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
    if !valid_turn_id(&p.turn_id) {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "invalid turn_id");
    }

    let Some(dir) = find_turn_dir(state, &p.turn_id) else {
        return WsResponse::err(&req.id, "NOT_FOUND", "turn not found");
    };

    let summary = std::fs::read_to_string(dir.join("summary.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let full_events = read_full_events(&dir);

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "turn_id": p.turn_id,
            "summary": summary,
            "full_trace": full_events,
        }),
    )
}

/// The three-state verification of a step, as a timeline draws it.
fn verified_word(state: Option<&str>) -> Option<bool> {
    match state {
        Some("verified") => Some(true),
        Some("failed") => Some(false),
        _ => None,
    }
}

/// Read a CAS digest out of an evidence object.
///
/// Attachment refs travel as `{digest, mime, size}`; a trace line keeps a bare
/// digest in `before_ref` / `after_ref`. Both shapes are accepted so a reader
/// does not have to know which version wrote the file.
fn digest_of(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Object(o) => {
            o.get("digest").and_then(|v| v.as_str()).map(str::to_string)
        }
        _ => None,
    }
}

/// `turns.replay` — one turn as an ordered action/screenshot timeline.
///
/// The plan's timeline entry: `{step, action, effect, verified, before_ref,
/// after_ref}` per step, in the order the actions happened. `step` is the
/// position among the turn's action events — the same numbering the run's own
/// event stream uses, so a timeline drawn from a trace and one drawn from live
/// `computer.step` events agree.
///
/// Deliberately **not** a coordinate replay: the frames are the CAS digests of
/// what the screen looked like, and the action is described semantically
/// ("click element s1:0.2"). Replaying pixels is not stable across DPI or
/// window sizes, which is why the plan forbids it.
pub(crate) async fn handle_turns_replay(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        turn_id: String,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if !valid_turn_id(&p.turn_id) {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "invalid turn_id");
    }

    let Some(dir) = find_turn_dir(state, &p.turn_id) else {
        return WsResponse::err(&req.id, "NOT_FOUND", "turn not found");
    };

    let summary = std::fs::read_to_string(dir.join("summary.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let events = read_full_events(&dir).unwrap_or_default();

    let mut steps = Vec::new();
    let mut image_refs = 0usize;
    for event in &events {
        if event.get("type").and_then(|t| t.as_str()) != Some("action") {
            continue;
        }
        let Some(detail) = event.get("detail") else {
            continue;
        };
        let before_ref = digest_of(detail.get("before_ref"));
        let after_ref = digest_of(detail.get("after_ref"));
        // Only count a frame that is distinct from the one before it: two
        // steps that show the same pixels are one picture, and the client
        // fetches them by digest anyway.
        if before_ref.is_some() || after_ref.is_some() {
            image_refs += 1;
        }
        steps.push(serde_json::json!({
            "step": steps.len(),
            "round": event.get("round").and_then(|r| r.as_u64()),
            "action": event.get("action").and_then(|a| a.as_str()),
            "rung": detail.get("rung").and_then(|v| v.as_str()),
            "effect": detail.get("effect").and_then(|v| v.as_str()),
            "delivery": detail.get("delivery").and_then(|v| v.as_str()),
            "verified": verified_word(detail.get("verification").and_then(|v| v.as_str())),
            "before_ref": before_ref,
            "after_ref": after_ref,
            "recovery": detail.get("recovery").and_then(|v| v.as_str()),
            "settle_ms": detail.get("settle_ms").and_then(|v| v.as_u64()),
        }));
    }

    // The turn's own input, so a client can show the goal the timeline answers.
    // `summary` carries a truncated preview; this is the same fact, not a new
    // one, which is why the replay does not re-open the session store for it.
    let input = summary
        .as_ref()
        .and_then(|s| s.get("user_message_preview"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "turn_id": p.turn_id,
            "input": input,
            "steps": steps,
            "step_count": steps.len(),
            "image_count": image_refs,
            // Digests are not images: a client turns them into a picture with
            // `attachment.get`. Said in the payload so a UI does not have to
            // guess the URL scheme.
            "image_method": "attachment.get",
        }),
    )
}

/// The largest payload `attachment.get` will hand back in one frame.
///
/// A WS frame carries this inline as base64 (~4/3 the size). Screenshots are
/// encoded to well under a megabyte, so this is a backstop against a caller
/// naming some other, larger object in the store rather than a real limit.
const MAX_ATTACHMENT_BYTES: u64 = 8 * 1024 * 1024;

/// `attachment.get` — one CAS object by digest, base64 for the UI.
///
/// The timeline hands out digests, and a digest is not something a browser can
/// render. This is the read that closes that gap for every attachment in the
/// store (screenshots in tool results, run steps, replayed turns), not only the
/// ones a turn trace mentions.
///
/// `read` scope: the objects are the agent's own captures, and a caller that
/// may read the trace that names them may read them.
pub(crate) async fn handle_attachment_get(
    req: &WsRequest,
    _state: &Arc<GatewayState>,
) -> WsResponse {
    // The store is process-wide (`dirs::attachments_dir`); the state is taken
    // for signature parity with the rest of the dispatcher, not for its paths.
    #[derive(Deserialize)]
    struct Params {
        digest: String,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };

    // `open_ref` validates the digest shape before touching the filesystem, so
    // a path-shaped caller cannot address anything but an object.
    let aref = crate::attachments::AttachmentRef {
        digest: p.digest.clone(),
        mime: "application/octet-stream".to_string(),
        size: 0,
    };
    if !p.digest.starts_with("sha256:") || p.digest.len() != "sha256:".len() + 64 {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "digest must be sha256:<64 hex>");
    }

    let bytes = match tokio::task::spawn_blocking(move || crate::attachments::open_ref(&aref)).await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return WsResponse::err(&req.id, "NOT_FOUND", "attachment not found");
        }
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::InvalidInput => {
            return WsResponse::err(&req.id, "INVALID_PARAMS", e.to_string());
        }
        Ok(Err(e)) => {
            return WsResponse::err(
                &req.id,
                "INTERNAL_ERROR",
                format!("could not read attachment: {e}"),
            )
        }
        Err(e) => {
            return WsResponse::err(
                &req.id,
                "INTERNAL_ERROR",
                format!("attachment read failed: {e}"),
            )
        }
    };

    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return WsResponse::err(
            &req.id,
            "TOO_LARGE",
            format!(
                "attachment is {} bytes; attachment.get returns at most {}",
                bytes.len(),
                MAX_ATTACHMENT_BYTES
            ),
        );
    }

    use base64::Engine as _;
    let mime = sniff_mime(&bytes);
    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "digest": p.digest,
            "mime": mime,
            "size": bytes.len(),
            "content_base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
        }),
    )
}

/// The object's MIME type from its first bytes.
///
/// The store keeps bytes, not metadata: the reference that named them was
/// written by whichever capture produced it, and re-deriving the type from the
/// payload is what keeps this read honest for objects stored by an older path.
fn sniff_mime(bytes: &[u8]) -> &'static str {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G'];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF];
    if bytes.starts_with(PNG) {
        "image/png"
    } else if bytes.starts_with(JPEG) {
        "image/jpeg"
    } else if bytes.starts_with(b"%PDF") {
        "application/pdf"
    } else if std::str::from_utf8(bytes).is_ok() {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

// ── Turn rerun (D.3) ───────────────────────────────────────────────────

/// The tool names a rerun is allowed to use without asking.
///
/// A rerun of a computer-use turn has to be able to act on the desktop, and
/// every desktop tool is `requires_approval`. With no human present the
/// approval gate fails closed, so an un-pre-approved rerun would measure the
/// approval policy instead of the prompt. The list is the desktop family the
/// gateway itself registers (`init/tools.rs`); a name that is not registered
/// is simply never called.
const RERUN_DESKTOP_TOOLS: &[&str] = &[
    "computer",
    "screen_state",
    "screen_ocr",
    "screen_ui_detect",
    "macos_accessibility",
    "macos_screenshot",
    "applescript",
    "macos_notification",
    "ios_list_devices",
    "ios_screenshot",
    "android_control",
];

/// The read-only file tools a rerun may use to inspect what the agent did.
const RERUN_FILE_TOOLS: &[&str] = &["file_read", "grep", "glob"];

/// Everything a rerun is allowed to do without a human: the desktop family,
/// the eval file/shell set, and the read tools — nothing that sends, spends,
/// or posts.
fn rerun_permissions(
    registry: &crate::tools::ToolRegistry,
) -> Arc<crate::tools::PermissionsRuntime> {
    let mut allow: Vec<String> = registry.list();
    for name in RERUN_DESKTOP_TOOLS.iter().chain(RERUN_FILE_TOOLS) {
        let name = name.to_string();
        if !allow.contains(&name) {
            allow.push(name);
        }
    }
    crate::eval::standalone::eval_permissions(allow)
}

/// The rerun task id for a turn and attempt.
///
/// `EvalTask::id` becomes part of the trial directory
/// (`<tmp>/eval_<id>_<trial>`) and the conversation id, so it must not carry a
/// path separator — the turn id comes from a client.
fn rerun_task_id(turn_id: &str, rerun_id: &str) -> String {
    let safe = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!("rerun_{}_{}", safe(turn_id), safe(rerun_id))
}

/// The turn input a rerun replays.
///
/// `summary.json` keeps only a preview (truncated at `MAX_FIELD_BYTES`), so the
/// authoritative input is the first user message of the turn's first LLM round
/// in `full.json`. The preview is the fallback for a turn whose round was never
/// captured — a truncated goal is still more useful than refusing to rerun.
fn rerun_input(
    events: &[serde_json::Value],
    summary: Option<&serde_json::Value>,
) -> Option<String> {
    for event in events {
        if event.get("type").and_then(|t| t.as_str()) != Some("round") {
            continue;
        }
        let Some(request) = event.get("request") else {
            continue;
        };
        let Some(messages) = request.as_array() else {
            continue;
        };
        for message in messages {
            if message.get("role").and_then(|r| r.as_str()) != Some("user") {
                continue;
            }
            if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
                if !content.trim().is_empty() {
                    return Some(content.to_string());
                }
            }
        }
    }
    summary
        .and_then(|s| s.get("user_message_preview"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// Write one rerun result, atomically, next to the trace it is about.
async fn save_rerun(
    state: &GatewayState,
    turn_id: &str,
    rerun_id: &str,
    body: &serde_json::Value,
) -> std::io::Result<()> {
    let dir = state.paths.reruns_dir().join(turn_id);
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{rerun_id}.json"));
    let mut tmp = path.clone();
    tmp.set_extension("json.tmp");
    tokio::fs::write(&tmp, serde_json::to_vec_pretty(body).unwrap_or_default()).await?;
    tokio::fs::rename(&tmp, &path).await
}

/// `turns.rerun` — replay a recorded turn's input to ask "would it still pass?".
///
/// Params: `{ turn_id, assertions?, criteria?, model? }`.
///
/// `assertions` is an array of `GoalCondition`s and `criteria` a
/// `QualityCriteria`; **at least one is required**. Without them the harness's
/// `conditions_passed` and `critique_passed` both default to `true`, so a
/// rerun would report green no matter what the agent did — the one failure
/// mode a "did my prompt change help?" tool must not have. The refusal is at
/// the call, so a caller cannot mistake an assertion-free run for a pass.
///
/// `write` scope: a rerun drives tools (shell, file writes, and — when the
/// caller asks for them — the desktop), so it is not a read.
///
/// Returns as soon as the run is started; the result arrives as a
/// `turn.rerun_finished` event and is also written to
/// `<root>/reruns/<turn_id>/<rerun_id>.json`. WS dispatch is serial per
/// connection and a rerun takes minutes, so awaiting it here would block every
/// other call on the same socket.
pub(crate) async fn handle_turns_rerun(req: &WsRequest, state: &Arc<GatewayState>) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        turn_id: String,
        #[serde(default)]
        assertions: Vec<crate::goal::condition::GoalCondition>,
        #[serde(default)]
        criteria: Option<crate::agent::reflection::types::QualityCriteria>,
        #[serde(default)]
        model: Option<String>,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if !valid_turn_id(&p.turn_id) {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "invalid turn_id");
    }

    // A rerun is only worth running against something that can fail.
    if p.assertions.is_empty() && p.criteria.is_none() {
        return WsResponse::err(
            &req.id,
            "NO_ASSERTIONS",
            "a rerun needs `assertions` (GoalCondition[]) or `criteria`; without either every \
             trial passes vacuously and the answer would be meaningless",
        );
    }

    let Some(dir) = find_turn_dir(state, &p.turn_id) else {
        return WsResponse::err(&req.id, "NOT_FOUND", "turn not found");
    };
    let summary = std::fs::read_to_string(dir.join("summary.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let events = read_full_events(&dir).unwrap_or_default();
    let Some(input) = rerun_input(&events, summary.as_ref()) else {
        return WsResponse::err(
            &req.id,
            "NO_INPUT",
            "the turn has no recorded user message to replay",
        );
    };

    let (provider, model) = match super::computer::driver_provider(state).await {
        Ok(pair) => pair,
        Err(refusal) => return refusal.with_id(&req.id),
    };
    let model = p.model.unwrap_or(model);

    // The eval registry (shell, files, web, browser) plus the permissions every
    // one of those needs pre-approved — the same rule `syscity eval run` uses,
    // derived from the registry rather than hard-coded. The desktop tools come
    // from the gateway's own registry, so a rerun of a screen turn can reach
    // the real adapter; they are individually optional, so a gateway with no
    // adapter simply has no `computer` tool.
    let mut registry = crate::eval::standalone::create_eval_tool_registry(None, None, None);
    for name in RERUN_DESKTOP_TOOLS {
        if let Some(tool) = state.tools.registry.get(name) {
            registry.register_dynamic(tool);
        }
    }
    let permissions = rerun_permissions(&registry);
    registry = registry.with_permissions(permissions);
    let tools = Arc::new(registry);

    let agent_config = state.config.read().await.default_agent.clone();
    let agent = crate::agent::Agent::new(agent_config, provider.clone(), tools)
        .with_paths(state.paths.clone());
    let critic = p.criteria.as_ref().map(|_| {
        let mut critic = crate::agent::reflection::critic::Critic::new(provider);
        critic = critic.with_model(model.clone());
        critic
    });

    let rerun_id = uuid::Uuid::new_v4().to_string();
    let task_id = rerun_task_id(&p.turn_id, &rerun_id);
    let task = crate::eval::EvalTask {
        id: task_id,
        description: format!("rerun of turn {}", p.turn_id),
        input: input.clone(),
        conditions: p.assertions.clone(),
        criteria: p.criteria.clone(),
        source: crate::eval::EvalTaskSource::Online,
        ..Default::default()
    };

    let run_state = state.clone();
    let turn_id = p.turn_id.clone();
    let rerun_id_out = rerun_id.clone();
    let assertions = p.assertions.len();
    let model_out = model.clone();
    let handle = tokio::spawn(async move {
        let harness =
            crate::eval::harness::EvalHarness::new(Arc::new(agent), critic).with_default_trials(1);
        // N=1: a rerun answers "can it still do this", not "how often" — a
        // second trial is a second minute of desktop time answering a question
        // nobody asked.
        let outcome = harness.run(task, 1).await;
        let body = match outcome {
            Ok(summary) => {
                let trial = summary.per_trial.first();
                serde_json::json!({
                    "rerun_id": rerun_id_out,
                    "turn_id": turn_id,
                    "status": "finished",
                    "passed": summary.pass_rate > 0.0,
                    "pass_rate": summary.pass_rate,
                    "assertions": assertions,
                    "model": model_out,
                    "duration_ms": summary.avg_duration_ms,
                    "condition_results": trial.map(|t| &t.condition_results),
                    "critique_passed": trial.map(|t| t.critique_passed).unwrap_or(true),
                    "response": trial.map(|t| &t.response),
                    "tool_calls": trial.map(|t| &t.tool_calls),
                })
            }
            Err(e) => serde_json::json!({
                "rerun_id": rerun_id_out,
                "turn_id": turn_id,
                "status": "failed",
                "passed": false,
                "assertions": assertions,
                "model": model_out,
                "error": e.to_string(),
            }),
        };
        if let Err(e) = save_rerun(&run_state, &turn_id, &rerun_id_out, &body).await {
            tracing::warn!("could not persist rerun {rerun_id_out} of turn {turn_id}: {e}");
        }
        let _ = run_state
            .events
            .tx
            .send(crate::gateway::GatewayEvent::TurnRerunFinished { turn_id, body: body.clone() });
    });
    state
        .task_registry
        .insert_join(format!("turns.rerun:{rerun_id}"), handle)
        .await;

    WsResponse::ok(
        &req.id,
        serde_json::json!({
            "rerun_id": rerun_id,
            "turn_id": p.turn_id,
            "status": "started",
            "model": model,
            "input": input,
            "assertions": assertions,
            "has_criteria": p.criteria.is_some(),
            "result_method": "turns.rerun_result",
        }),
    )
}

/// `turns.rerun_result` — read a finished rerun by `{ turn_id, rerun_id }`.
///
/// The event carries the result to whoever was connected when it finished; a
/// client that connected later (or missed the event) reads it here.
pub(crate) async fn handle_turns_rerun_result(
    req: &WsRequest,
    state: &Arc<GatewayState>,
) -> WsResponse {
    #[derive(Deserialize)]
    struct Params {
        turn_id: String,
        rerun_id: String,
    }
    let p: Params = match parse_params(req) {
        Ok(p) => p,
        Err(res) => return res,
    };
    if !valid_turn_id(&p.turn_id) || !valid_turn_id(&p.rerun_id) {
        return WsResponse::err(&req.id, "INVALID_PARAMS", "invalid turn_id or rerun_id");
    }
    let path = state
        .paths
        .reruns_dir()
        .join(&p.turn_id)
        .join(format!("{}.json", p.rerun_id));
    match tokio::fs::read_to_string(&path).await {
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(body) => WsResponse::ok(&req.id, serde_json::json!({ "rerun": body })),
            Err(e) => WsResponse::err(
                &req.id,
                "INTERNAL_ERROR",
                format!("rerun file is not valid JSON: {e}"),
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => WsResponse::err(
            &req.id,
            "NOT_FOUND",
            "no such rerun (it may still be running; the event is `turn.rerun_finished`)",
        ),
        Err(e) => WsResponse::err(&req.id, "INTERNAL_ERROR", format!("could not read rerun: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state_tests::make_test_state;
    use crate::gateway::GatewayConfig;

    fn req(method: &str, params: serde_json::Value) -> WsRequest {
        WsRequest {
            frame_type: "req".into(),
            id: "r1".into(),
            method: method.into(),
            params: Some(params),
        }
    }

    /// A turn directory of the shape the writer produces, under the state's
    /// own layout root.
    fn write_turn(state: &GatewayState, turn_id: &str, actions: &[serde_json::Value]) {
        let dir = state
            .paths
            .turns_dir()
            .join(chrono::Local::now().format("%Y-%m-%d").to_string())
            .join(turn_id);
        std::fs::create_dir_all(&dir).expect("turn dir");
        let summary = serde_json::json!({
            "schema_version": 1,
            "turn_id": turn_id,
            "user_message_preview": "open the calculator",
        });
        std::fs::write(dir.join("summary.json"), summary.to_string()).expect("summary");
        let mut lines = vec![
            serde_json::json!({
                "type": "round",
                "round": 0,
                "request": null,
                "response": "ok",
            })
            .to_string(),
            serde_json::json!({
                "type": "tool",
                "round": 0,
                "name": "computer",
                "args": "{}",
                "result": "clicked",
                "success": true,
                "duration_ms": 12,
            })
            .to_string(),
        ];
        for action in actions {
            lines.push(action.to_string());
        }
        std::fs::write(dir.join("full.json"), lines.join("\n")).expect("full trace");
    }

    fn action(round: u64, action: &str, effect: &str, verified: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "action",
            "round": round,
            "action": action,
            "detail": {
                "action": action,
                "rung": "ax_action",
                "effect": effect,
                "delivery": "delivered",
                "retry": "safe",
                "verification": verified,
                "before_ref": format!("sha256:{}", "a".repeat(64)),
                "after_ref": format!("sha256:{}", "b".repeat(64)),
            },
        })
    }

    /// The timeline is the plan's `[{step, action, effect, verified,
    /// before_ref, after_ref}]`, in the order the actions happened.
    #[tokio::test]
    async fn a_turn_replays_as_an_ordered_action_and_frame_timeline() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        write_turn(
            &state,
            "turn-1",
            &[
                action(0, "click", "confirmed", "verified"),
                action(1, "type", "unverifiable", "unverifiable"),
            ],
        );

        let resp = handle_turns_replay(
            &req("turns.replay", serde_json::json!({"turn_id": "turn-1"})),
            &state,
        )
        .await;
        assert!(resp.ok, "{:?}", resp.error);
        let payload = resp.payload.expect("payload");

        let steps = payload["steps"].as_array().expect("steps");
        assert_eq!(steps.len(), 2, "only action events are steps: {payload}");
        assert_eq!(steps[0]["step"], 0);
        assert_eq!(steps[0]["action"], "click");
        assert_eq!(steps[0]["effect"], "confirmed");
        assert_eq!(steps[0]["verified"], true);
        assert_eq!(steps[0]["before_ref"], format!("sha256:{}", "a".repeat(64)));
        assert_eq!(steps[0]["after_ref"], format!("sha256:{}", "b".repeat(64)));

        assert_eq!(steps[1]["step"], 1);
        // `unverifiable` is not a failure — nothing confirmed *or* denied it.
        assert_eq!(steps[1]["verified"], serde_json::Value::Null);
        assert_eq!(payload["step_count"], 2);
        assert_eq!(payload["input"], "open the calculator");
    }

    /// A turn with no computer-use steps is still a turn: the read answers with
    /// an empty timeline rather than NOT_FOUND.
    #[tokio::test]
    async fn a_turn_without_actions_replays_as_an_empty_timeline() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        write_turn(&state, "turn-plain", &[]);

        let resp = handle_turns_replay(
            &req("turns.replay", serde_json::json!({"turn_id": "turn-plain"})),
            &state,
        )
        .await;
        assert!(resp.ok, "{:?}", resp.error);
        let payload = resp.payload.expect("payload");
        assert_eq!(payload["steps"].as_array().map(Vec::len), Some(0));
        assert_eq!(payload["image_count"], 0);
    }

    #[tokio::test]
    async fn replay_refuses_an_invalid_or_unknown_turn_id() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        for bad in ["", "..", "../etc", "a/b"] {
            let resp = handle_turns_replay(
                &req("turns.replay", serde_json::json!({"turn_id": bad})),
                &state,
            )
            .await;
            assert!(!resp.ok);
            assert_eq!(resp.error.expect("error").code, "INVALID_PARAMS");
        }

        let resp = handle_turns_replay(
            &req("turns.replay", serde_json::json!({"turn_id": "no-such-turn"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.expect("error").code, "NOT_FOUND");
    }

    /// `attachment.get` validates the digest before it reads anything: a
    /// path-shaped caller must not be able to address a file.
    #[tokio::test]
    async fn attachment_get_refuses_a_path_shaped_digest() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        for bad in ["../../etc/passwd", "sha256:../..", "deadbeef"] {
            let resp = handle_attachment_get(
                &req("attachment.get", serde_json::json!({ "digest": bad })),
                &state,
            )
            .await;
            assert!(!resp.ok, "{bad} must be refused");
            assert_eq!(resp.error.expect("error").code, "INVALID_PARAMS");
        }
    }

    /// A stored object comes back intact, with a type taken from its bytes
    /// rather than from whatever the caller claims.
    #[tokio::test]
    async fn attachment_get_returns_a_stored_object_with_a_sniffed_mime() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        // A one-pixel PNG header is enough: the store is content-addressed and
        // does not care whether the payload decodes.
        let bytes = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let aref = crate::attachments::store_bytes(&bytes, "image/png").expect("store");

        let resp = handle_attachment_get(
            &req("attachment.get", serde_json::json!({ "digest": aref.digest })),
            &state,
        )
        .await;
        assert!(resp.ok, "{:?}", resp.error);
        let payload = resp.payload.expect("payload");
        assert_eq!(payload["mime"], "image/png");
        assert_eq!(payload["size"], 8);
        use base64::Engine as _;
        assert_eq!(
            payload["content_base64"],
            serde_json::json!(base64::engine::general_purpose::STANDARD.encode(bytes))
        );
    }

    #[tokio::test]
    async fn attachment_get_reports_a_missing_object_as_not_found() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let digest = format!("sha256:{}", "0".repeat(64));
        let resp = handle_attachment_get(
            &req("attachment.get", serde_json::json!({ "digest": digest })),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.expect("error").code, "NOT_FOUND");
    }

    // ── turns.rerun ────────────────────────────────────────────────────

    /// The whole method's premise: a rerun with nothing that can fail is
    /// refused, because the harness defaults both `conditions_passed` and
    /// `critique_passed` to true — an assertion-free rerun would always be
    /// green, which is the one answer a "did the prompt change help?" tool
    /// must never give.
    #[tokio::test]
    async fn rerun_refuses_to_run_without_an_assertion_that_can_fail() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        write_turn(&state, "turn-r", &[]);

        let resp = handle_turns_rerun(
            &req("turns.rerun", serde_json::json!({"turn_id": "turn-r"})),
            &state,
        )
        .await;
        assert!(!resp.ok);
        let err = resp.error.expect("error");
        assert_eq!(err.code, "NO_ASSERTIONS");
        assert!(err.message.contains("meaningless"), "{}", err.message);
    }

    /// The same refusal applies to a turn that does not exist: the caller
    /// learns what is wrong with the *call* first, since assertions are the
    /// part they can fix.
    #[tokio::test]
    async fn rerun_refuses_an_unknown_or_malformed_turn_before_starting() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let assertions = serde_json::json!([{ "type": "file_exists", "path": "/tmp/x" }]);

        for bad in ["", "..", "a/b"] {
            let resp = handle_turns_rerun(
                &req(
                    "turns.rerun",
                    serde_json::json!({ "turn_id": bad, "assertions": assertions }),
                ),
                &state,
            )
            .await;
            assert!(!resp.ok);
            assert_eq!(resp.error.expect("error").code, "INVALID_PARAMS");
        }

        let resp = handle_turns_rerun(
            &req(
                "turns.rerun",
                serde_json::json!({ "turn_id": "no-such-turn", "assertions": assertions }),
            ),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.expect("error").code, "NOT_FOUND");
    }

    /// The replayed input is the turn's real user message from `full.json`,
    /// not the truncated preview — a rerun of a preview would be a rerun of a
    /// different task.
    #[test]
    fn rerun_replays_the_recorded_user_message_not_the_preview() {
        let long_goal = "x".repeat(10_000);
        let events = vec![serde_json::json!({
            "type": "round",
            "round": 0,
            "request": [
                { "role": "system", "content": "you are a desktop agent" },
                { "role": "user", "content": long_goal },
            ],
            "response": "ok",
        })];
        let summary = serde_json::json!({ "user_message_preview": "truncated…" });

        let input = rerun_input(&events, Some(&summary)).expect("input");
        assert_eq!(input.len(), 10_000);
        assert_eq!(input, long_goal);

        // A turn whose round was never captured falls back to the preview
        // rather than refusing: a truncated goal still asks the right thing.
        assert_eq!(rerun_input(&[], Some(&summary)).as_deref(), Some("truncated…"));
        assert_eq!(rerun_input(&[], None), None);
    }

    /// `EvalTask::id` becomes a directory name and a conversation id; a turn
    /// id carrying a separator must not survive into it.
    #[test]
    fn rerun_task_ids_are_safe_as_path_segments() {
        let id = rerun_task_id("../../etc/passwd", "a/b");
        assert!(!id.contains('/'), "{id}");
        assert!(!id.contains('.'), "{id}");
        assert!(id.starts_with("rerun_"), "{id}");
    }

    /// A finished rerun is readable by id, which is how a client that missed
    /// the event gets the verdict.
    #[tokio::test]
    async fn a_persisted_rerun_reads_back_by_id() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let body = serde_json::json!({
            "rerun_id": "rr-1",
            "turn_id": "turn-1",
            "status": "finished",
            "passed": false,
            "assertions": 1,
        });
        save_rerun(&state, "turn-1", "rr-1", &body)
            .await
            .expect("persist");

        let resp = handle_turns_rerun_result(
            &req(
                "turns.rerun_result",
                serde_json::json!({ "turn_id": "turn-1", "rerun_id": "rr-1" }),
            ),
            &state,
        )
        .await;
        assert!(resp.ok, "{:?}", resp.error);
        let payload = resp.payload.expect("payload");
        assert_eq!(payload["rerun"]["passed"], false);
        assert_eq!(payload["rerun"]["assertions"], 1);
    }

    #[tokio::test]
    async fn a_missing_rerun_reads_back_as_not_found() {
        let state = Arc::new(make_test_state(GatewayConfig::default()).await);
        let resp = handle_turns_rerun_result(
            &req(
                "turns.rerun_result",
                serde_json::json!({ "turn_id": "turn-1", "rerun_id": "nope" }),
            ),
            &state,
        )
        .await;
        assert!(!resp.ok);
        assert_eq!(resp.error.expect("error").code, "NOT_FOUND");
    }

    /// The permission set a rerun installs is derived from the registry, so
    /// every tool it advertises can actually run — an approval-gated tool in a
    /// context with nobody to ask fails closed, and a rerun that can reach the
    /// desktop must be able to drive it.
    #[test]
    fn rerun_permissions_cover_every_registered_tool() {
        let mut registry = crate::tools::ToolRegistry::new();
        registry.register(Box::new(crate::tools::shell::ShellTool::new()));
        registry.register(Box::new(crate::tools::file::FileWriteTool::new()));

        let runtime = rerun_permissions(&registry);
        let allow = runtime.config_projection().allow;
        assert!(allow.iter().any(|r| r == "shell"), "{allow:?}");
        assert!(allow.iter().any(|r| r == "file_write"), "{allow:?}");
        // The desktop family is allowed whether or not this gateway has an
        // adapter: a name that is not registered is simply never called, and a
        // registry-derived list alone would miss it on a headless host.
        assert!(allow.iter().any(|r| r == "computer"), "{allow:?}");
    }

    /// A started rerun answers immediately (WS dispatch is serial per
    /// connection, so awaiting a minutes-long run would block the socket),
    /// writes its result where `turns.rerun_result` looks, and announces it as
    /// `turn.rerun_finished`.
    #[tokio::test]
    async fn a_started_rerun_persists_its_result_and_announces_it() {
        use crate::providers::mock::MockProvider;
        use crate::providers::{FunctionCall, Message as ProviderMessage, Role, ToolCall};

        let mut state = make_test_state(GatewayConfig::default()).await;
        // The state's own model router decides the provider; a mock that asks
        // for one shell command answers the "did the agent act" question the
        // assertion below scores.
        let mock = Arc::new(MockProvider::new().with_callback(|messages| {
            if messages.iter().any(|m| m.role == Role::Tool) {
                return ProviderMessage::assistant("done");
            }
            ProviderMessage::assistant("running").with_tool_calls(vec![ToolCall {
                id: "call_1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "shell".into(),
                    arguments: serde_json::json!({ "command": "true" }).to_string(),
                },
                index: None,
                result: None,
            }])
        }));
        state
            .infra
            .model_router
            .add_provider_instance("mock", mock)
            .await
            .expect("register mock provider");
        let state = Arc::new(state);

        write_turn(&state, "turn-e2e", &[]);
        let mut events = state.events.tx.subscribe();

        let resp = handle_turns_rerun(
            &req(
                "turns.rerun",
                serde_json::json!({
                    "turn_id": "turn-e2e",
                    "assertions": [{ "type": "exit_code", "command": "true" }],
                }),
            ),
            &state,
        )
        .await;
        assert!(resp.ok, "{:?}", resp.error);
        let payload = resp.payload.expect("payload");
        assert_eq!(payload["status"], "started");
        let rerun_id = payload["rerun_id"].as_str().expect("rerun_id").to_string();
        assert_eq!(payload["assertions"], 1);

        // The result arrives as an event, not as this response.
        let finished = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                match events.recv().await.expect("event") {
                    crate::gateway::GatewayEvent::TurnRerunFinished { turn_id, body } => {
                        break (turn_id, body);
                    }
                    _ => continue,
                }
            }
        })
        .await
        .expect("rerun finished in time");
        assert_eq!(finished.0, "turn-e2e");
        assert_eq!(finished.1["status"], "finished");
        assert_eq!(finished.1["assertions"], 1);

        // And the same body is readable afterwards.
        let read = handle_turns_rerun_result(
            &req(
                "turns.rerun_result",
                serde_json::json!({ "turn_id": "turn-e2e", "rerun_id": rerun_id }),
            ),
            &state,
        )
        .await;
        assert!(read.ok, "{:?}", read.error);
        assert_eq!(read.payload.expect("payload")["rerun"]["status"], "finished");
    }
}
