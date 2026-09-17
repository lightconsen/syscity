//! Slash commands.
//!
//! Anything that used to open a popup now prints into the scrollback, which is
//! the whole point of running inline: the output stays where you can read it,
//! scroll it, and copy it. Commands that need the gateway go through
//! [`crate::tui::gateway_calls`].
// INVARIANTS-NONE: command dispatch; state lives in `AppState`.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::RwLock;

use crate::tui::error::TuiError;
use crate::tui::gateway_calls as gw;
use crate::tui::state::{AppState, CommandInfo};
use crate::tui::transcript::{LineKind, TranscriptLine};
use crate::tui::ui::blocks;
use crate::tui::ws_client::WsClient;

/// Commands implemented inside the TUI.
pub const LOCAL_COMMANDS: &[&str] = &[
    "new", "clear", "quit", "exit", "help", "history", "config", "status", "tools", "model",
    "sessions", "resume", "rename", "pin", "agents", "agent", "answer",
];

/// Split a submitted line into `(name, args)`.
pub fn parse_slash_command(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('/')?;
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    };
    if name.is_empty() {
        None
    } else {
        Some((name, args))
    }
}

/// Whether a command is handled locally.
pub fn is_local_command(name: &str) -> bool {
    LOCAL_COMMANDS.contains(&name)
}

/// Handle a slash command.
pub async fn handle_slash_command(
    line: &str,
    state: Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let Some((name, args)) = parse_slash_command(line) else {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ empty command");
        return Ok(());
    };
    if is_local_command(name) {
        handle_local_command(name, args, state, ws).await
    } else {
        execute_remote_command(name, args, state, ws).await
    }
}

/// Commands the TUI implements itself.
async fn handle_local_command(
    name: &str,
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    match name {
        "new" => command_new(args, state, ws).await,
        "clear" => command_clear(state, ws).await,
        "quit" | "exit" => {
            state.write().await.should_quit = true;
            Ok(())
        }
        "help" => command_help(state, ws).await,
        "config" => command_config(args, state, ws).await,
        "status" => command_status(state, ws).await,
        "tools" => command_tools(state, ws).await,
        "model" => command_model(args, state, ws).await,
        "sessions" => command_sessions(state, ws).await,
        "history" => command_history(args, state, ws).await,
        "resume" => crate::tui::resume::command_resume(args, state, ws).await,
        "rename" => command_rename(args, state, ws).await,
        "pin" => command_pin(state, ws).await,
        "agents" => command_agents(state, ws).await,
        "agent" => command_agent(args, state, ws).await,
        "answer" => command_answer(args, state, ws).await,
        _ => {
            state
                .write()
                .await
                .transcript
                .push_notice(format!("⚠ unknown command /{name}"));
            Ok(())
        }
    }
}

/// `/new [agent-id]` — start a conversation, optionally bound to an agent.
async fn command_new(
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let agent = (!args.trim().is_empty()).then(|| args.trim().to_string());
    let session_id = gw::sessions_create(ws, agent.as_deref()).await?;
    let mut s = state.write().await;
    if let Some(old) = s.current_session.take() {
        let _ = gw::sessions_unsubscribe(ws, &old).await;
    }
    gw::sessions_subscribe(ws, &session_id).await?;
    s.current_session = Some(session_id.clone());
    s.current_agent = agent;
    s.transcript.reset();
    s.transcript
        .push_notice(format!("── new session {session_id} ──"));
    Ok(())
}

/// `/clear` — clear the conversation context.
///
/// Scrollback belongs to the terminal and cannot be unprinted; saying so is
/// better than leaving the user to wonder why the old text is still there.
async fn command_clear(state: Arc<RwLock<AppState>>, ws: &mut WsClient) -> Result<(), TuiError> {
    let session = state.read().await.current_session.clone();
    match session {
        Some(id) => {
            gw::sessions_reset(ws, &id).await?;
            let mut s = state.write().await;
            s.transcript.reset();
            s.transcript
                .push_notice("── context cleared (the terminal's own scrollback is untouched) ──");
        }
        None => {
            state
                .write()
                .await
                .transcript
                .push_notice("⚠ no session yet — send a message first");
        }
    }
    Ok(())
}

/// `/help` — print the keybindings and the command catalog.
async fn command_help(state: Arc<RwLock<AppState>>, ws: &mut WsClient) -> Result<(), TuiError> {
    let catalog = gw::commands_list(ws).await.unwrap_or_default();
    let mut s = state.write().await;
    if !catalog.is_empty() {
        s.command_list = catalog;
    }
    let mut lines = vec![TranscriptLine::new(LineKind::Notice, "Keys")];
    for (key, what) in [
        ("Enter", "send · Shift+Enter for a newline"),
        ("Up / Down", "input history (or move within a multiline input)"),
        ("Tab", "complete a /command"),
        ("Esc", "dismiss a prompt · stop a running turn"),
        ("Ctrl+C", "abort the run, or quit when idle"),
        ("Ctrl+R", "resume a session"),
        ("Ctrl+H / Ctrl+E", "this help · configuration"),
        ("Ctrl+Q", "quit"),
    ] {
        lines.push(TranscriptLine::new(LineKind::Notice, format!("  {key:<16} {what}")));
    }
    lines.push(TranscriptLine::new(LineKind::Notice, ""));
    lines.push(TranscriptLine::new(LineKind::Notice, "TUI commands"));
    for cmd in local_command_list() {
        lines.push(TranscriptLine::new(
            LineKind::Notice,
            format!("  /{:<10} {}", cmd.name, cmd.description),
        ));
    }
    if !s.command_list.is_empty() {
        lines.push(TranscriptLine::new(LineKind::Notice, ""));
        lines.push(TranscriptLine::new(LineKind::Notice, "Gateway commands (run with /<name>)"));
        for cmd in &s.command_list {
            lines.push(TranscriptLine::new(
                LineKind::Notice,
                format!("  /{:<10} {}", cmd.name, cmd.description),
            ));
        }
    }
    s.transcript.push(lines);
    Ok(())
}

/// The locally implemented commands, described for `/help`.
pub fn local_command_list() -> Vec<CommandInfo> {
    [
        ("new", "[agent]", "start a new session"),
        ("clear", "", "clear the conversation context"),
        ("resume", "[n|id]", "list sessions, or switch to one"),
        ("sessions", "", "list sessions"),
        ("history", "[n]", "print more of this conversation"),
        ("rename", "<name>", "rename the current session"),
        ("pin", "", "pin or unpin the current session"),
        ("agents", "", "list agents"),
        ("agent", "<id>", "start a session bound to an agent"),
        ("config", "[set <path> <value>]", "show or change configuration"),
        ("status", "", "gateway status"),
        ("tools", "", "list gateway commands"),
        ("model", "<id>", "set the default model"),
        ("answer", "<text>", "answer a pending question"),
        ("help", "", "this help"),
        ("quit", "", "leave the TUI"),
        ("exit", "", "alias of /quit"),
    ]
    .into_iter()
    .map(|(name, usage, description)| CommandInfo {
        key: name.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        usage: usage.to_string(),
        category: "tui".to_string(),
        tier: "essential".to_string(),
        local: true,
        requires_admin: false,
    })
    .collect()
}

/// `/status` — gateway presence and connection details.
async fn command_status(state: Arc<RwLock<AppState>>, ws: &mut WsClient) -> Result<(), TuiError> {
    let value = ws.request("system.presence", None).await?;
    let mut s = state.write().await;
    s.transcript
        .push_notice(format!("gateway: {}", serde_json::to_string(&value).unwrap_or_default()));
    Ok(())
}

/// The most history `/history` will print in one go.
const HISTORY_MAX: usize = 2000;
/// What `/history` prints with no argument.
const HISTORY_DEFAULT: usize = 200;

/// `/history [n]` — print this conversation again, oldest first.
///
/// The scrollback is append-only and top-anchored: there is nowhere to put
/// messages older than what is already printed, so "load more" reprints the
/// window in reading order under a rule rather than splicing it into the
/// middle of the transcript.
async fn command_history(
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let requested = args.trim();
    let limit = if requested.is_empty() {
        HISTORY_DEFAULT
    } else {
        match requested.parse::<usize>() {
            Ok(n) if n > 0 => n.min(HISTORY_MAX),
            _ => {
                state
                    .write()
                    .await
                    .transcript
                    .push_notice(format!("⚠ usage: /history [1-{HISTORY_MAX}]"));
                return Ok(());
            }
        }
    };

    let Some(session) = state.read().await.current_session.clone() else {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ no session yet — send a message first");
        return Ok(());
    };

    let (messages, has_more) = gw::chat_history(ws, &session, limit).await?;
    let mut lines = vec![blocks::rule(&format!(
        "history: {} message(s)",
        messages.len()
    ))];
    if has_more {
        // Older than the window, so it goes above the messages below.
        lines.push(TranscriptLine::new(
            LineKind::Notice,
            format!("… older messages omitted — /history {HISTORY_MAX} asks for more"),
        ));
    }
    lines.extend(blocks::history_lines(&messages));
    state.write().await.transcript.push(lines);
    Ok(())
}

/// `/tools` — the gateway's command catalog.
async fn command_tools(state: Arc<RwLock<AppState>>, ws: &mut WsClient) -> Result<(), TuiError> {
    let catalog = gw::commands_list(ws).await?;
    let count = catalog.len();
    let mut s = state.write().await;
    s.command_list = catalog;
    s.transcript
        .push_notice(format!("{count} gateway commands available — see /help"));
    Ok(())
}

/// `/model <id>` — set the default model.
async fn command_model(
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let model = args.trim();
    if model.is_empty() {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ usage: /model <model-id>");
        return Ok(());
    }
    ws.request("models.set_default", Some(serde_json::json!({ "model_id": model })))
        .await?;
    state
        .write()
        .await
        .transcript
        .push_notice(format!("default model: {model}"));
    Ok(())
}

/// `/sessions` — list the sessions the gateway knows.
async fn command_sessions(state: Arc<RwLock<AppState>>, ws: &mut WsClient) -> Result<(), TuiError> {
    let sessions = crate::tui::resume::refresh_sessions(&state, ws).await?;
    let mut s = state.write().await;
    if sessions.is_empty() {
        s.transcript.push_notice("no sessions yet");
    } else {
        s.transcript
            .push_notice(format!("{} sessions:", sessions.len()));
        for line in crate::tui::resume::session_lines(&sessions) {
            s.transcript.push_notice(line);
        }
    }
    Ok(())
}

/// `/rename <name>` — rename the current session.
async fn command_rename(
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let name = args.trim();
    let session = state.read().await.current_session.clone();
    let Some(id) = session else {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ no session to rename yet");
        return Ok(());
    };
    if name.is_empty() {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ usage: /rename <name>");
        return Ok(());
    }
    gw::sessions_rename(ws, &id, name).await?;
    let mut s = state.write().await;
    if let Some(entry) = s.sessions.iter_mut().find(|x| x.id == id) {
        entry.name = Some(name.to_string());
    }
    s.transcript.push_notice(format!("renamed to \"{name}\""));
    Ok(())
}

/// `/pin` — pin or unpin the current session.
async fn command_pin(state: Arc<RwLock<AppState>>, ws: &mut WsClient) -> Result<(), TuiError> {
    let current = {
        let s = state.read().await;
        s.current_session.clone().map(|id| {
            let pinned = s
                .sessions
                .iter()
                .find(|x| x.id == id)
                .map(|x| x.pinned)
                .unwrap_or(false);
            (id, pinned)
        })
    };
    let Some((id, was_pinned)) = current else {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ no session to pin yet");
        return Ok(());
    };
    let pinned = !was_pinned;
    gw::sessions_set_pinned(ws, &id, pinned).await?;
    let mut s = state.write().await;
    if let Some(entry) = s.sessions.iter_mut().find(|x| x.id == id) {
        entry.pinned = pinned;
    }
    s.transcript.push_notice(if pinned {
        "session pinned"
    } else {
        "session unpinned"
    });
    Ok(())
}

/// `/agents` — list the agents the gateway knows.
async fn command_agents(state: Arc<RwLock<AppState>>, ws: &mut WsClient) -> Result<(), TuiError> {
    let agents = gw::agents_registry(ws).await?;
    let mut s = state.write().await;
    let usable: Vec<_> = agents.iter().filter(|a| a.is_valid).collect();
    if usable.is_empty() {
        s.transcript.push_notice("no agents configured");
    } else {
        s.transcript
            .push_notice(format!("{} agents:", usable.len()));
        for agent in usable {
            s.transcript
                .push_notice(format!("  {} {}  ({})", agent.emoji, agent.display_name, agent.id));
        }
        s.transcript.push_notice("start one with /agent <id>");
    }
    s.agents = agents;
    Ok(())
}

/// `/agent <id>` — start a session bound to an agent.
async fn command_agent(
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let id = args.trim();
    if id.is_empty() {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ usage: /agent <agent-id> — see /agents");
        return Ok(());
    }
    command_new(id, state, ws).await
}

/// `/answer <text>` — answer the pending question.
async fn command_answer(
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let text = args.trim();
    let ask = state.read().await.pending_ask.clone();
    let Some(ask) = ask else {
        state
            .write()
            .await
            .transcript
            .push_notice("no question is waiting for an answer");
        return Ok(());
    };
    if text.is_empty() {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ usage: /answer <text>");
        return Ok(());
    }
    gw::ask_respond(ws, &ask.ask_id, text).await?;
    let mut s = state.write().await;
    s.pending_ask = None;
    s.ask_input.clear();
    s.transcript.push_notice(format!("answered: {text}"));
    Ok(())
}

/// `/config` and `/config set <path> <value>`.
async fn command_config(
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let mut parts = args.splitn(3, char::is_whitespace);
    let verb = parts.next().unwrap_or("");
    if verb != "set" {
        return command_config_show(state, ws).await;
    }
    let path = parts.next().unwrap_or("").trim();
    let raw = parts.next().unwrap_or("").trim();
    if path.is_empty() || raw.is_empty() {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ usage: /config set <path> <value>");
        return Ok(());
    }
    // Values are typed where it is unambiguous: a number stays a number, a
    // bare true/false stays a boolean, everything else is a string.
    let value: Value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
    let revision = state.read().await.config_revision.clone();
    match gw::config_set(ws, path, value.clone(), revision.as_deref()).await {
        Ok(()) => {
            let mut s = state.write().await;
            s.transcript.push_notice(format!(
                "✓ {path} = {}",
                serde_json::to_string(&value).unwrap_or_default()
            ));
            s.config_cache = None;
        }
        Err(e) => {
            state.write().await.transcript.push_notice(format!("✘ {e}"));
        }
    }
    Ok(())
}

/// `/config` — print the effective configuration.
async fn command_config_show(
    state: Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let value = gw::config_get(ws).await?;
    let mut lines = vec![TranscriptLine::new(LineKind::Notice, "configuration")];
    lines.extend(config_lines(&value));
    lines.push(TranscriptLine::new(
        LineKind::Notice,
        "change one with /config set <path> <value>",
    ));
    let mut s = state.write().await;
    s.config_revision = value["revision"].as_str().map(str::to_string);
    s.config_cache = Some(value);
    s.transcript.push(lines);
    Ok(())
}

/// Flatten the configuration payload into readable lines.
pub fn config_lines(value: &Value) -> Vec<TranscriptLine> {
    let mut out = Vec::new();
    for key in ["model", "model_provider"] {
        if let Some(v) = value.get(key).filter(|v| !v.is_null()) {
            out.push(TranscriptLine::new(
                LineKind::Notice,
                format!("  {key} = {}", render_value(v)),
            ));
        }
    }
    if let Some(agent) = value.get("default_agent").and_then(|v| v.as_object()) {
        for (key, v) in agent {
            if key == "system_prompt" {
                continue;
            }
            out.push(TranscriptLine::new(
                LineKind::Notice,
                format!("  default_agent.{key} = {}", render_value(v)),
            ));
        }
    }
    if let Some(overrides) = value.get("agent_overrides").and_then(|v| v.as_object()) {
        for (agent, fields) in overrides {
            if let Some(fields) = fields.as_object() {
                for (key, v) in fields {
                    if key == "system_prompt" {
                        continue;
                    }
                    out.push(TranscriptLine::new(
                        LineKind::Notice,
                        format!("  agent_overrides.{agent}.{key} = {}", render_value(v)),
                    ));
                }
            }
        }
    }
    if let Some(heartbeat) = value.get("heartbeat").and_then(|v| v.as_object()) {
        if let Some(enabled) = heartbeat.get("enabled") {
            out.push(TranscriptLine::new(
                LineKind::Notice,
                format!("  heartbeat.enabled = {}", render_value(enabled)),
            ));
        }
    }
    out
}

/// Render a JSON value compactly, truncating a long one.
fn render_value(value: &Value) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if text.chars().count() > 80 {
        let head: String = text.chars().take(80).collect();
        format!("{head}…")
    } else {
        text
    }
}

/// Forward a command the TUI does not implement to the gateway.
async fn execute_remote_command(
    name: &str,
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let session = state.read().await.current_session.clone();
    let mut params = serde_json::json!({ "command": name, "args": args });
    if let Some(id) = session {
        params["session_id"] = Value::String(id);
    }
    match ws.request("commands.execute", Some(params)).await {
        Ok(payload) => {
            let text = payload["text"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| payload.to_string());
            state.write().await.transcript.push(
                blocks::text_lines(&text)
                    .into_iter()
                    .map(|mut l| {
                        l.kind = LineKind::Notice;
                        l
                    })
                    .collect::<Vec<_>>(),
            );
        }
        Err(e) => {
            state
                .write()
                .await
                .transcript
                .push_notice(format!("✘ /{name}: {e}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_gateway::TestGateway;

    /// A state and client on the test gateway.
    async fn connect(gateway: &TestGateway) -> (Arc<RwLock<AppState>>, WsClient) {
        let auth = crate::tui::auth::AuthConfig::None;
        let url = auth.ws_url("127.0.0.1", gateway.port, None, "tui");
        let (client, _hello) = WsClient::connect(&url, &auth, &["chat"])
            .await
            .expect("connect");
        (Arc::new(RwLock::new(AppState::default())), client)
    }

    /// The output lines a command queued.
    async fn output(state: &Arc<RwLock<AppState>>) -> Vec<String> {
        state
            .write()
            .await
            .transcript
            .take_flushable()
            .into_iter()
            .map(|l| l.text)
            .collect()
    }

    /// `/history` prints the conversation, oldest first, under a rule.
    #[tokio::test]
    async fn history_prints_the_conversation_in_order() {
        let gateway = TestGateway::start().await;
        let (state, mut client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());
        gateway.with_history(
            (0..5)
                .map(|i| {
                    serde_json::json!({
                        "id": format!("msg_{i}"),
                        "role": "user",
                        "content": format!("message {i}"),
                        "timestamp": 1_757_000_000_000_i64 + i * 1000,
                    })
                })
                .collect(),
        );

        command_history("", Arc::clone(&state), &mut client)
            .await
            .expect("history");

        let lines = output(&state).await;
        assert!(lines[0].contains("history: 5 message(s)"), "got {lines:?}");
        let positions: Vec<usize> = (0..5)
            .map(|i| {
                lines
                    .iter()
                    .position(|l| l.ends_with(&format!("message {i}")))
                    .unwrap_or_else(|| panic!("message {i} is missing from {lines:?}"))
            })
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "oldest first: {positions:?}");
    }

    /// An unreadable count is a usage error, not a gateway call.
    #[tokio::test]
    async fn history_rejects_a_count_it_cannot_read() {
        let gateway = TestGateway::start().await;
        let (state, mut client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());

        command_history("banana", Arc::clone(&state), &mut client)
            .await
            .expect("handled");

        let lines = output(&state).await;
        assert!(lines[0].contains("usage: /history"), "got {lines:?}");
        assert!(
            !gateway
                .requests()
                .iter()
                .any(|r| r.method == "chat.history"),
            "nothing was asked of the gateway"
        );
    }

    /// `/history` with no session says so rather than asking about nothing.
    #[tokio::test]
    async fn history_needs_a_session() {
        let gateway = TestGateway::start().await;
        let (state, mut client) = connect(&gateway).await;

        command_history("", Arc::clone(&state), &mut client)
            .await
            .expect("handled");

        let lines = output(&state).await;
        assert!(lines[0].contains("no session"), "got {lines:?}");
    }

    #[test]
    fn parses_commands_with_and_without_arguments() {
        assert_eq!(parse_slash_command("/help"), Some(("help", "")));
        assert_eq!(parse_slash_command("/model gpt-4o"), Some(("model", "gpt-4o")));
        assert_eq!(parse_slash_command("/rename  my  chat "), Some(("rename", "my  chat")));
        assert_eq!(parse_slash_command("not a command"), None);
        assert_eq!(parse_slash_command("/"), None);
    }

    #[test]
    fn local_commands_are_recognised() {
        assert!(is_local_command("resume"));
        assert!(is_local_command("config"));
        assert!(!is_local_command("usage"));
    }

    #[test]
    fn help_lists_every_local_command() {
        let listed: Vec<String> = local_command_list().into_iter().map(|c| c.name).collect();
        for name in LOCAL_COMMANDS {
            assert!(
                listed.contains(&name.to_string()),
                "/{name} is handled but missing from /help"
            );
        }
    }

    #[test]
    fn config_lines_flatten_the_payload() {
        let payload = serde_json::json!({
            "revision": "abc",
            "model": "gpt-4o",
            "model_provider": "openai",
            "default_agent": { "temperature": 0.7, "system_prompt": "very long…" },
            "agent_overrides": { "secretary": { "temperature": 0.3 } },
            "heartbeat": { "enabled": true }
        });
        let text: Vec<String> = config_lines(&payload).into_iter().map(|l| l.text).collect();
        assert!(text.iter().any(|l| l.contains("model = gpt-4o")));
        assert!(text
            .iter()
            .any(|l| l.contains("default_agent.temperature = 0.7")));
        assert!(text
            .iter()
            .any(|l| l.contains("agent_overrides.secretary.temperature = 0.3")));
        assert!(text.iter().any(|l| l.contains("heartbeat.enabled = true")));
        assert!(
            !text.iter().any(|l| l.contains("system_prompt")),
            "the system prompt is too big to dump"
        );
    }

    #[test]
    fn long_values_are_truncated_in_the_config_dump() {
        let long = "x".repeat(200);
        let payload = serde_json::json!({ "model": long });
        let text: Vec<String> = config_lines(&payload).into_iter().map(|l| l.text).collect();
        assert!(text[0].chars().count() < 100);
    }
}
