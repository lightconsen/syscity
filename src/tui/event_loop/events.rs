//! Routing what the gateway sends into the state.

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use tokio::sync::RwLock;

use crate::tui::error::TuiError;
use crate::tui::gateway_calls::{self as gw, ApprovalDetail};
use crate::tui::resume;
use crate::tui::state::{AppState, AskPrompt, DelegationRow, LiveMode, RunPhase};
use crate::tui::transcript::{LineKind, TranscriptLine};
use crate::tui::ui::blocks;
use crate::tui::ws_client::{ClientEvent, WsClient, WsMessage};

use super::send::start_queued_message;
use super::{STREAM_ASSISTANT, STREAM_THINKING};

/// Route one gateway message.
pub(super) async fn handle_gateway_message(
    message: WsMessage,
    state: &Arc<RwLock<AppState>>,
    ws: &WsClient,
) {
    match message {
        WsMessage::Event(event) => handle_event(event, state, ws).await,
        WsMessage::OrphanResponse(response) => {
            if !response.ok {
                if let Some(err) = response.error {
                    state
                        .write()
                        .await
                        .transcript
                        .push_notice(format!("⚠ {}: {}", err.code, err.message));
                }
            }
        }
        WsMessage::Disconnected => {}
    }
}

/// Apply one server event to the state.
pub(super) async fn handle_event(event: ClientEvent, state: &Arc<RwLock<AppState>>, ws: &WsClient) {
    let Some(payload) = event.payload else {
        return;
    };

    // A connection with no subscriptions receives *every* session's traffic:
    // the gateway reads an empty subscription list as "all", and the window
    // between creating a session and subscribing to it is exactly that. So the
    // client checks too — an event that names a session is ours only if it
    // names *our* session. Events that name none are global on purpose: cron
    // notices, and approvals raised outside any conversation. An approval
    // that names *another* session is not silently dropped either: someone's
    // turn is blocked on it, and in the fail-open window we may be the only
    // client that heard about it.
    if let Some(session_id) = payload["session_id"].as_str() {
        if state.read().await.current_session.as_deref() != Some(session_id) {
            if event.event == "approval.required" {
                let tool = payload["tool_name"].as_str().unwrap_or("tool");
                let mut s = state.write().await;
                s.transcript.push_notice(format!(
                    "⚠ {tool} needs approval in another session — that turn is blocked until \
                     it gets one"
                ));
                s.dirty = true;
            }
            return;
        }
    }

    match event.event.as_str() {
        "chat.delta" => {
            let content = payload["content"].as_str().unwrap_or_default();
            let mut s = state.write().await;
            // Reasoning belongs before the answer it produced. A thinking
            // line that never saw a newline would otherwise stay pending
            // until chat.final and land after the answer.
            s.transcript.seal_stream(STREAM_THINKING);
            s.run_phase = RunPhase::Responding;
            s.transcript
                .push_delta(STREAM_ASSISTANT, LineKind::Assistant, content);
            s.dirty = true;
        }
        "agent.thinking" => {
            let content = payload["content"].as_str().unwrap_or_default();
            let mut s = state.write().await;
            // The live stream gets the same header the history renderer adds,
            // once per turn, the first time reasoning arrives.
            if !s.transcript.has_stream(STREAM_THINKING) {
                s.transcript
                    .push(vec![TranscriptLine::new(LineKind::Reasoning, "thinking:")]);
            }
            s.run_phase = RunPhase::Thinking;
            s.transcript
                .push_delta(STREAM_THINKING, LineKind::Reasoning, content);
            s.dirty = true;
        }
        "tool.calling" => {
            let tool = payload["tool_name"].as_str().unwrap_or("tool").to_string();
            // The gateway names this field `arguments`; reading `args` (as the
            // old TUI did) silently dropped every tool's parameters.
            let args = payload
                .get("arguments")
                .filter(|v| !v.is_null())
                .cloned()
                .or_else(|| payload.get("args").filter(|v| !v.is_null()).cloned());
            let mut s = state.write().await;
            // Same ordering rule as for answer text: reasoning that never saw
            // a newline would otherwise land after the call it motivated.
            s.transcript.seal_stream(STREAM_THINKING);
            s.run_phase = RunPhase::ToolCall(tool.clone());
            s.transcript.push(tool_call_lines(&tool, args.as_ref()));
            // Keep the whole call for `/expand` — the live rows above are
            // truncated. The next call evicts this one, which is fine: an
            // expand targets what just ran.
            s.last_tool_name = Some(tool.clone());
            s.last_tool_args = args.clone();
            s.last_tool_result = None;
            s.dirty = true;
        }
        "tool.result" => {
            let tool = payload["tool_name"].as_str().unwrap_or("tool").to_string();
            let result = payload.get("result").filter(|v| !v.is_null()).cloned();
            let mut s = state.write().await;
            // The tool is done; whatever runs next has not started saying
            // anything yet. Leaving the name up would label the wait with a
            // call that already finished.
            s.run_phase = RunPhase::Waiting;
            s.transcript.push(
                blocks::result_lines(&tool, result.as_ref())
                    .into_iter()
                    .map(|row| TranscriptLine::new(LineKind::ToolResult, row)),
            );
            // The full result is what `/expand` prints. The name is set here
            // too: a result without a matching `tool.calling` (a reconnect
            // mid-call) should still be expandable.
            s.last_tool_name = Some(tool);
            s.last_tool_result = result;
            s.dirty = true;
        }
        "delegation.updated" => {
            let Some(row) = DelegationRow::from_payload(&payload) else {
                return;
            };
            let mut s = state.write().await;
            // A first "running" sighting starts the row's local clock; an
            // update keeps the clock it already has.
            let started = match s.delegation_tasks.iter().find(|t| t.task_id == row.task_id) {
                Some(existing) => existing.started,
                None if row.status == "running" => Some(Instant::now()),
                None => None,
            };
            let row = DelegationRow { started, ..row };
            let position = s
                .delegation_tasks
                .iter()
                .position(|t| t.task_id == row.task_id);
            let terminal = row.is_terminal();
            let task_id = row.task_id.clone();
            if terminal {
                // The notice is the durable record: the live row is about to
                // be retired, and the transcript survives it.
                s.transcript.push_notice(row.graduation_line());
            }
            match position {
                Some(i) => s.delegation_tasks[i] = row,
                None if !terminal => s.delegation_tasks.push(row),
                None => {
                    // A task that went straight to terminal (events lost
                    // before the TUI subscribed): nothing to keep, the
                    // notice above carries it.
                }
            }
            if terminal {
                s.delegation_tasks.retain(|t| t.task_id != task_id);
            }
            s.dirty = true;
        }
        "agent.usage" => {
            let Some(total) = payload["usage"]["total_tokens"].as_u64() else {
                return;
            };
            let mut s = state.write().await;
            // Per-round events already cover every round of the turn —
            // including the final one, which `chat.final`'s own usage also
            // counts. Adding that here would double the total.
            s.run_tokens = Some(s.run_tokens.unwrap_or(0) + total);
            s.dirty = true;
        }
        "chat.final" => {
            let response = payload["response"].as_str().map(str::to_string);
            {
                let mut s = state.write().await;
                // The verbatim answer is what `/copy` puts on the clipboard.
                s.last_assistant_text = response.clone();
                s.transcript.finish_stream(STREAM_THINKING, None);
                s.transcript
                    .finish_stream(STREAM_ASSISTANT, response.as_deref());
                s.end_run();
                s.dirty = true;
            }
            start_queued_message(state, ws).await;
        }
        "chat.error" => {
            let message = payload["message"].as_str().unwrap_or("unknown error");
            {
                let mut s = state.write().await;
                s.transcript.finish_stream(STREAM_ASSISTANT, None);
                s.transcript
                    .push_notice(format!("✘ response failed: {message}"));
                s.end_run();
                s.dirty = true;
            }
            // The turn failed, but the queue is still the user's next move.
            start_queued_message(state, ws).await;
        }
        "session.created" => {
            if let Some(id) = payload["session_id"].as_str() {
                let mut s = state.write().await;
                if s.current_session.is_none() {
                    s.current_session = Some(id.to_string());
                }
                s.dirty = true;
            }
            // The list feeds `/resume`, `/sessions` and the status row. A
            // failed refresh leaves all three showing what the gateway knew
            // last time, which looks exactly like a list that has not changed.
            if let Err(e) = resume::refresh_sessions(state, ws).await {
                state
                    .write()
                    .await
                    .transcript
                    .push_notice(format!("⚠ could not refresh the session list: {e}"));
            }
        }
        "session.renamed" => {
            let id = payload["session_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let name = payload["name"].as_str().unwrap_or_default().to_string();
            let mut s = state.write().await;
            if let Some(entry) = s.sessions.iter_mut().find(|x| x.id == id) {
                entry.name = Some(name);
            }
            s.dirty = true;
        }
        "session.mode_changed" => {
            let mode = payload["mode"]
                .as_str()
                .and_then(crate::tools::PermissionMode::parse);
            if let (Some(id), Some(mode)) = (payload["session_id"].as_str(), mode) {
                let mut s = state.write().await;
                if s.current_session.as_deref() == Some(id) {
                    s.permission_mode = mode;
                    s.transcript.push_notice(format!(
                        "⨿ permission mode: {} (this session)",
                        mode.as_str()
                    ));
                    s.dirty = true;
                }
            }
        }
        "cron.completed" => {
            let name = payload["job_name"].as_str().unwrap_or("cron job");
            let status = payload["status"].as_str().unwrap_or("ok");
            let output = payload["output"].as_str().unwrap_or_default();
            let mut s = state.write().await;
            s.transcript
                .push_notice(format!("⏱ {name} [{status}] {output}"));
            s.dirty = true;
        }
        "approval.required" => {
            let id = payload["approval_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if id.is_empty() {
                return;
            }
            let mut detail = ApprovalDetail {
                id: id.clone(),
                tool_name: payload["tool_name"].as_str().unwrap_or("tool").to_string(),
                requested_by: payload["requested_by"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                risk_level: payload["risk_level"]
                    .as_str()
                    .unwrap_or("Medium")
                    .to_string(),
                message: payload["message"].as_str().unwrap_or_default().to_string(),
                args: None,
            };
            // The event carries no arguments, so ask for them. Without this the
            // prompt would be asking the human to approve a call they cannot
            // see. A resolved-elsewhere approval comes back NOT_FOUND.
            match gw::approvals_get(ws, &id).await {
                Ok(full) => detail = full,
                Err(TuiError::Gateway { code, .. }) if code == "NOT_FOUND" => {
                    state.write().await.transcript.push_notice(format!(
                        "⚠ approval {} was already resolved",
                        detail.tool_name
                    ));
                    return;
                }
                Err(e) => {
                    state
                        .write()
                        .await
                        .set_status(format!("⚠ no details for the approval: {e}"));
                }
            }
            let mut s = state.write().await;
            s.approvals.push_back(detail);
            s.live_mode = LiveMode::Approval;
            s.dirty = true;
        }
        "ask.required" => {
            let mut s = state.write().await;
            s.pending_ask = Some(AskPrompt {
                ask_id: payload["ask_id"].as_str().unwrap_or_default().to_string(),
                question: payload["question"].as_str().unwrap_or_default().to_string(),
                options: payload["options"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                required: payload["required"].as_bool().unwrap_or(true),
                default: payload["default"].as_str().map(str::to_string),
            });
            s.ask_input.clear();
            s.live_mode = LiveMode::Ask;
            s.dirty = true;
        }
        "ask.resolved" => {
            let mut s = state.write().await;
            s.pending_ask = None;
            s.ask_input.clear();
            if s.live_mode == LiveMode::Ask {
                s.live_mode = LiveMode::Composer;
            }
            s.dirty = true;
        }
        _ => {}
    }
}

/// Transcript lines for a tool invocation.
pub(super) fn tool_call_lines(tool: &str, args: Option<&Value>) -> Vec<TranscriptLine> {
    let mut lines = vec![TranscriptLine::new(LineKind::Tool, format!("⚙ {tool}"))];
    if let Some(args) = args {
        // One transcript line per row. Packing the rows into one line loses
        // the newlines at render time (a `\n` has zero cell width and is
        // dropped), so a multi-line call came out as a single run-together
        // row — and again on its way into scrollback.
        lines.extend(
            blocks::args_lines(args, "  ", blocks::TOOL_ARG_LINES_LIVE)
                .into_iter()
                .map(|row| TranscriptLine::new(LineKind::Tool, row)),
        );
    }
    lines
}
