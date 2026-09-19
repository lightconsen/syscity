//! Every gateway call the TUI makes, in one place.
//!
//! Each method lives here exactly once, with the payload shapes the gateway
//! actually serves — verified against the handlers, not assumed. The previous
//! TUI scattered these across `event_loop.rs` and `commands.rs` and drifted:
//! `sessions.list` was read as a bare array (it is `{sessions:[…]}`),
//! `chat.history` likewise, `chat.abort` was called without its required
//! `session_id`, `tool.calling` was read as `args` (it is `arguments`), and
//! approvals were answered on a method (`approval.respond`) that does not
//! exist. Concentrating the calls and testing the parsers against literal
//! payloads is what keeps that from happening again.
// INVARIANTS-NONE: presentation-layer gateway client; owns no persistent state.

use serde_json::{json, Value};

use crate::tui::error::TuiError;
use crate::tui::ws_client::WsClient;

/// One session as the gateway lists it (`sessions.list`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionInfo {
    /// Session id (`session_id` on the wire).
    pub id: String,
    /// Human label; the gateway derives one from the first activity.
    pub name: Option<String>,
    /// Agent the session is bound to, if any.
    pub agent_id: Option<String>,
    /// RFC3339 timestamp of the last activity.
    pub last_activity: Option<String>,
    /// Number of messages in the session.
    pub message_count: u64,
    /// Whether the session is pinned.
    pub pinned: bool,
}

/// One message from `chat.history`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryMessage {
    /// Message id.
    pub id: String,
    /// `user` / `assistant` / `system` / `tool`.
    pub role: String,
    /// Message text.
    pub content: String,
    /// Provider reasoning, when the model emitted any.
    pub reasoning: Option<String>,
    /// Raw `tool_calls` payload, when present.
    pub tool_calls: Option<Value>,
    /// Creation time in milliseconds since the epoch.
    pub timestamp_ms: Option<i64>,
}

/// One agent from `agents.registry`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentInfo {
    /// Agent id.
    pub id: String,
    /// Human name.
    pub display_name: String,
    /// Signature emoji.
    pub emoji: String,
    /// Whether the personality is valid (invalid ones are not usable).
    pub is_valid: bool,
}

/// A pending tool approval (`approvals.get`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApprovalDetail {
    /// Approval id.
    pub id: String,
    /// Tool awaiting the decision.
    pub tool_name: String,
    /// Who asked for it.
    pub requested_by: String,
    /// Risk classification, as reported by the gateway.
    pub risk_level: String,
    /// Human-readable explanation.
    pub message: String,
    /// Tool arguments — the thing the human actually needs to see.
    pub args: Option<Value>,
}

/// The result of a `chat.send`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatSendResult {
    /// Session the gateway routed the message to. The TUI adopts this rather
    /// than inventing its own id.
    pub session_id: String,
    /// Agent that will answer.
    pub agent_id: Option<String>,
}

// ── Parsers ────────────────────────────────────────────────────────────────
//
// Split out from the calls so they can be tested against the real payload
// literals without a gateway.

/// Parse a `sessions.list` payload.
pub fn parse_sessions(value: &Value) -> Vec<SessionInfo> {
    value["sessions"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let id = row["session_id"].as_str()?.to_string();
                    Some(SessionInfo {
                        id,
                        name: row["name"].as_str().map(str::to_string),
                        agent_id: row["agent_id"].as_str().map(str::to_string),
                        last_activity: row["last_activity"].as_str().map(str::to_string),
                        message_count: row["message_count"].as_u64().unwrap_or(0),
                        pinned: row["pinned"].as_bool().unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a `chat.history` payload into messages plus the `has_more` flag.
pub fn parse_history(value: &Value) -> (Vec<HistoryMessage>, bool) {
    let messages = value["messages"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| HistoryMessage {
                    id: row["id"].as_str().unwrap_or_default().to_string(),
                    role: row["role"].as_str().unwrap_or("assistant").to_string(),
                    content: row["content"].as_str().unwrap_or_default().to_string(),
                    reasoning: row["reasoning_content"].as_str().map(str::to_string),
                    tool_calls: row.get("tool_calls").filter(|v| !v.is_null()).cloned(),
                    timestamp_ms: row["timestamp"].as_i64(),
                })
                .collect()
        })
        .unwrap_or_default();
    let has_more = value["has_more"].as_bool().unwrap_or(false);
    (messages, has_more)
}

/// Parse an `agents.registry` payload.
pub fn parse_agents(value: &Value) -> Vec<AgentInfo> {
    value["agents"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    Some(AgentInfo {
                        id: row["id"].as_str()?.to_string(),
                        display_name: row["display_name"].as_str().unwrap_or_default().to_string(),
                        emoji: row["emoji"].as_str().unwrap_or("🤖").to_string(),
                        is_valid: row["is_valid"].as_bool().unwrap_or(true),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse an `approvals.get` payload.
pub fn parse_approval(value: &Value) -> ApprovalDetail {
    ApprovalDetail {
        id: value["id"].as_str().unwrap_or_default().to_string(),
        tool_name: value["tool_name"].as_str().unwrap_or_default().to_string(),
        requested_by: value["requested_by"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        risk_level: value["risk_level"].as_str().unwrap_or("Medium").to_string(),
        message: value["message"].as_str().unwrap_or_default().to_string(),
        args: value.get("args").filter(|v| !v.is_null()).cloned(),
    }
}

/// Parse a `commands.list` payload.
pub fn parse_commands(value: &Value) -> Vec<crate::tui::state::CommandInfo> {
    value["commands"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| crate::tui::state::CommandInfo {
                    key: row["key"].as_str().unwrap_or_default().to_string(),
                    name: row["name"].as_str().unwrap_or_default().to_string(),
                    description: row["description"].as_str().unwrap_or_default().to_string(),
                    usage: row["args"].as_str().unwrap_or_default().to_string(),
                    category: row["category"].as_str().unwrap_or_default().to_string(),
                    tier: row["tier"].as_str().unwrap_or_default().to_string(),
                    local: row["local"].as_bool().unwrap_or(false),
                    requires_admin: row["requires_admin"].as_bool().unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── Calls ──────────────────────────────────────────────────────────────────

/// List sessions.
pub async fn sessions_list(ws: &WsClient) -> Result<Vec<SessionInfo>, TuiError> {
    let value = ws.request("sessions.list", None).await?;
    Ok(parse_sessions(&value))
}

/// Create a session, optionally bound to an agent, returning its id.
pub async fn sessions_create(ws: &WsClient, agent_id: Option<&str>) -> Result<String, TuiError> {
    let params = match agent_id {
        Some(id) => json!({ "agent_id": id }),
        None => json!({}),
    };
    let value = ws.request("sessions.create", Some(params)).await?;
    Ok(value["session_id"].as_str().unwrap_or_default().to_string())
}

/// Subscribe to a session's events.
///
/// The parameter is a **list** (`session_ids`), not a single id — sending
/// `session_id` is rejected as an invalid request.
///
/// A connection with no subscriptions receives *every* session's deltas, so
/// switching sessions must also unsubscribe from the previous one.
pub async fn sessions_subscribe(ws: &WsClient, session_id: &str) -> Result<(), TuiError> {
    ws.request("sessions.subscribe", Some(json!({ "session_ids": [session_id] })))
        .await
        .map(|_| ())
}

/// Stop receiving a session's events.
pub async fn sessions_unsubscribe(ws: &WsClient, session_id: &str) -> Result<(), TuiError> {
    ws.request("sessions.unsubscribe", Some(json!({ "session_ids": [session_id] })))
        .await
        .map(|_| ())
}

/// Rename a session.
pub async fn sessions_rename(ws: &WsClient, id: &str, name: &str) -> Result<(), TuiError> {
    ws.request("sessions.rename", Some(json!({ "session_id": id, "name": name })))
        .await
        .map(|_| ())
}

/// Pin or unpin a session.
pub async fn sessions_set_pinned(ws: &WsClient, id: &str, pinned: bool) -> Result<(), TuiError> {
    ws.request("sessions.set_pinned", Some(json!({ "session_id": id, "pinned": pinned })))
        .await
        .map(|_| ())
}

/// Clear a session's context (this is what `/clear` means).
pub async fn sessions_reset(ws: &WsClient, id: &str) -> Result<(), TuiError> {
    ws.request("sessions.reset", Some(json!({ "session_id": id })))
        .await
        .map(|_| ())
}

/// Load a session's message history, oldest first, up to `limit` messages.
///
/// `before` is the gateway's pagination cursor (a timestamp in milliseconds):
/// only messages strictly older than it come back. `None` asks for the newest
/// page. The second return value is the gateway's `has_more`: true whenever
/// the window came back full, which is a page-size heuristic rather than a
/// count of what is left.
pub async fn chat_history(
    ws: &WsClient,
    session_id: &str,
    limit: usize,
    before: Option<i64>,
) -> Result<(Vec<HistoryMessage>, bool), TuiError> {
    let mut params = json!({ "session_id": session_id, "limit": limit });
    if let Some(before) = before {
        params["before"] = json!(before);
    }
    let value = ws.request("chat.history", Some(params)).await?;
    Ok(parse_history(&value))
}

/// Send a message, returning the session the gateway routed it to.
pub async fn chat_send(
    ws: &WsClient,
    session_id: &str,
    message: &str,
) -> Result<ChatSendResult, TuiError> {
    let value = ws
        .request("chat.send", Some(json!({ "message": message, "session_id": session_id })))
        .await?;
    Ok(ChatSendResult {
        session_id: value["session_id"]
            .as_str()
            .unwrap_or(session_id)
            .to_string(),
        agent_id: value["agent_id"].as_str().map(str::to_string),
    })
}

/// Abort the in-flight turn for a session.
pub async fn chat_abort(ws: &WsClient, session_id: &str) -> Result<(), TuiError> {
    ws.request("chat.abort", Some(json!({ "session_id": session_id })))
        .await
        .map(|_| ())
}

/// Look up a pending approval — the event that announces one carries no
/// arguments, so this is how the prompt learns what it is asking about.
pub async fn approvals_get(ws: &WsClient, id: &str) -> Result<ApprovalDetail, TuiError> {
    let value = ws
        .request("approvals.get", Some(json!({ "id": id })))
        .await?;
    Ok(parse_approval(&value))
}

/// Answer a pending approval.
pub async fn approvals_decide(
    ws: &WsClient,
    id: &str,
    approve: bool,
    reason: Option<&str>,
) -> Result<(), TuiError> {
    let (method, params) = if approve {
        ("approvals.approve", json!({ "id": id }))
    } else {
        (
            "approvals.deny",
            json!({ "id": id, "reason": reason.unwrap_or("Denied by user") }),
        )
    };
    ws.request(method, Some(params)).await.map(|_| ())
}

/// Answer an `ask_user` question.
pub async fn ask_respond(ws: &WsClient, ask_id: &str, response: &str) -> Result<(), TuiError> {
    ws.request("ask.respond", Some(json!({ "ask_id": ask_id, "response": response })))
        .await
        .map(|_| ())
}

/// Read the whole configuration (`config.get` takes no parameters).
pub async fn config_get(ws: &WsClient) -> Result<Value, TuiError> {
    ws.request("config.get", None).await
}

/// Write one configuration value.
pub async fn config_set(
    ws: &WsClient,
    path: &str,
    value: Value,
    base_revision: Option<&str>,
) -> Result<(), TuiError> {
    let mut params = json!({ "path": path, "value": value });
    if let Some(rev) = base_revision {
        params["base_revision"] = json!(rev);
    }
    ws.request("config.set", Some(params)).await.map(|_| ())
}

/// List agents.
pub async fn agents_registry(ws: &WsClient) -> Result<Vec<AgentInfo>, TuiError> {
    let value = ws.request("agents.registry", None).await?;
    Ok(parse_agents(&value))
}

/// List the gateway's command catalog.
pub async fn commands_list(ws: &WsClient) -> Result<Vec<crate::tui::state::CommandInfo>, TuiError> {
    let value = ws
        .request("commands.list", Some(json!({ "tier": "power" })))
        .await?;
    Ok(parse_commands(&value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_gateway::TestGateway;

    /// A client talking to the test gateway.
    async fn connect(gateway: &TestGateway) -> WsClient {
        let auth = crate::tui::auth::AuthConfig::None;
        let url = auth.ws_url("127.0.0.1", gateway.port, None, "tui");
        let (client, _hello) = WsClient::connect(&url, &auth, &["chat"])
            .await
            .expect("connect");
        client
    }

    /// `n` scripted messages, oldest first, one second apart.
    fn scripted(n: usize) -> Vec<Value> {
        (0..n)
            .map(|i| {
                json!({
                    "id": format!("msg_{i}"),
                    "role": if i % 2 == 0 { "user" } else { "assistant" },
                    "content": format!("message {i}"),
                    "timestamp": 1_757_000_000_000_i64 + (i as i64 * 1000),
                })
            })
            .collect()
    }

    /// The payloads below are copied from the gateway handlers, so a shape
    /// change on either side shows up as a failing test rather than as an
    /// empty list in the UI.
    #[test]
    fn sessions_payload_uses_session_id() {
        let payload = json!({
            "sessions": [
                {
                    "session_id": "tui:anonymous",
                    "name": "New Session",
                    "agent_id": "secretary",
                    "channel": "tui",
                    "message_count": 4,
                    "last_activity": "2026-09-15T12:00:00+00:00",
                    "is_active": true,
                    "pinned": true,
                    "model": null,
                    "created_at": "2026-09-15T11:00:00+00:00"
                }
            ]
        });
        let sessions = parse_sessions(&payload);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "tui:anonymous");
        assert_eq!(sessions[0].name.as_deref(), Some("New Session"));
        assert_eq!(sessions[0].agent_id.as_deref(), Some("secretary"));
        assert_eq!(sessions[0].message_count, 4);
        assert!(sessions[0].pinned);
    }

    #[test]
    fn sessions_payload_without_rows_is_empty() {
        assert!(parse_sessions(&json!({})).is_empty());
        assert!(parse_sessions(&json!({ "sessions": null })).is_empty());
    }

    #[test]
    fn history_payload_is_wrapped_and_uses_reasoning_content() {
        let payload = json!({
            "session_id": "s1",
            "messages": [
                {
                    "id": "msg_1",
                    "role": "user",
                    "content": "hi",
                    "reasoning_content": null,
                    "tool_calls": null,
                    "timestamp": 1757000000000_i64,
                    "turn_id": null
                },
                {
                    "id": "msg_2",
                    "role": "assistant",
                    "content": "hello",
                    "reasoning_content": "thinking…",
                    "tool_calls": [{ "id": "call_1", "function": { "name": "file_read" } }],
                    "timestamp": 1757000001000_i64,
                    "turn_id": "t1"
                }
            ],
            "has_more": true
        });
        let (messages, has_more) = parse_history(&payload);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].reasoning, None);
        assert_eq!(messages[1].reasoning.as_deref(), Some("thinking…"));
        assert!(messages[1].tool_calls.is_some());
        assert_eq!(messages[1].timestamp_ms, Some(1757000001000));
        assert!(has_more);
    }

    #[test]
    fn approval_payload_carries_the_arguments() {
        let payload = json!({
            "id": "ap_1",
            "tool_name": "file_write",
            "args": { "path": "/tmp/x" },
            "requested_at": "2026-09-15T12:00:00Z",
            "requested_by": "secretary",
            "risk_level": "High",
            "approval_level": "Single",
            "message": "write a file outside the workspace",
            "age_seconds": 3
        });
        let detail = parse_approval(&payload);
        assert_eq!(detail.id, "ap_1");
        assert_eq!(detail.tool_name, "file_write");
        assert_eq!(detail.risk_level, "High");
        assert_eq!(detail.args.as_ref().and_then(|a| a["path"].as_str()), Some("/tmp/x"));
    }

    #[test]
    fn commands_payload_is_an_object_with_a_commands_array() {
        let payload = json!({
            "commands": [
                {
                    "key": "status",
                    "name": "status",
                    "description": "Show gateway status",
                    "args": "",
                    "category": "status",
                    "tier": "essential",
                    "local": false,
                    "requires_admin": false
                }
            ]
        });
        let commands = parse_commands(&payload);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "status");
        assert!(!commands[0].local);
    }

    /// A window longer than one turn comes back whole, in reading order.
    ///
    /// `switch_to` asked for 100 and printed them, and nothing could ask for
    /// more — so anything past a hundred messages was unreadable from the TUI.
    #[tokio::test]
    async fn a_history_window_is_the_newest_n_in_reading_order() {
        let gateway = TestGateway::start().await;
        let mut client = connect(&gateway).await;
        gateway.with_history(scripted(500));

        let (messages, has_more) = chat_history(&mut client, "s1", 300, None)
            .await
            .expect("history");

        assert_eq!(messages.len(), 300);
        assert!(has_more, "a full window may have older messages behind it");
        let contents: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
        let expected: Vec<String> = (200..500).map(|i| format!("message {i}")).collect();
        assert_eq!(contents, expected, "reading order, oldest first");
    }

    /// Asking for more than there is returns everything and says so.
    #[tokio::test]
    async fn a_history_window_short_of_the_limit_reports_the_end() {
        let gateway = TestGateway::start().await;
        let mut client = connect(&gateway).await;
        gateway.with_history(scripted(30));

        let (messages, has_more) = chat_history(&mut client, "s1", 2000, None)
            .await
            .expect("history");

        assert_eq!(messages.len(), 30);
        assert!(!has_more, "the whole conversation fits");
        assert_eq!(messages[0].content, "message 0");
        assert_eq!(messages[29].content, "message 29");
    }

    /// Nothing stored is not an error, and not an endless loop.
    #[tokio::test]
    async fn an_empty_history_terminates() {
        let gateway = TestGateway::start().await;
        let mut client = connect(&gateway).await;

        let (messages, has_more) = chat_history(&mut client, "s1", 500, None)
            .await
            .expect("history");

        assert!(messages.is_empty());
        assert!(!has_more);
    }

    /// The `before` cursor walks backwards: a page asked for with it contains
    /// only strictly-older messages, and nothing from the first page repeats.
    #[tokio::test]
    async fn a_before_cursor_pages_backwards() {
        let gateway = TestGateway::start().await;
        let mut client = connect(&gateway).await;
        gateway.with_history(scripted(500));

        let (first, more) = chat_history(&mut client, "s1", 100, None)
            .await
            .expect("first page");
        assert!(more);
        let cursor = first[0].timestamp_ms.expect("the page's oldest timestamp");

        let (second, _) = chat_history(&mut client, "s1", 100, Some(cursor))
            .await
            .expect("second page");

        assert_eq!(second.len(), 100);
        assert_eq!(second[0].content, "message 300", "the page before 400..500");
        let newest_in_second = second[99].timestamp_ms.expect("timestamp");
        assert!(newest_in_second < cursor, "strictly older, no overlap");
    }

    #[test]
    fn agents_payload_defaults_a_missing_emoji() {
        let payload = json!({
            "agents": [
                { "id": "default", "display_name": "Default", "is_valid": true },
                { "id": "ghost", "display_name": "Ghost", "emoji": "👻", "is_valid": false }
            ]
        });
        let agents = parse_agents(&payload);
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].emoji, "🤖");
        assert_eq!(agents[1].emoji, "👻");
        assert!(!agents[1].is_valid);
    }
}
