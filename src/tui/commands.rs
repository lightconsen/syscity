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
use crate::tui::osc11;
use crate::tui::state::{AppState, CommandInfo};
use crate::tui::transcript::{LineKind, TranscriptLine};
use crate::tui::ui::blocks;
use crate::tui::ws_client::WsClient;

/// Commands implemented inside the TUI.
pub const LOCAL_COMMANDS: &[&str] = &[
    "new", "clear", "quit", "exit", "help", "history", "config", "status", "tools", "model",
    "sessions", "resume", "rename", "title", "pin", "agents", "agent", "answer", "copy", "retry",
    "expand", "undo", "plan", "save", "import", "theme", "mode",
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
    ws: &WsClient,
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
    ws: &WsClient,
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
        "rename" | "title" => command_rename(args, state, ws).await,
        "undo" => command_undo(args, state, ws).await,
        "plan" => command_plan(state, ws).await,
        "save" => command_save(args, state, ws).await,
        "import" => command_import(args, state, ws).await,
        "pin" => command_pin(state, ws).await,
        "agents" => command_agents(state, ws).await,
        "agent" => command_agent(args, state, ws).await,
        "answer" => command_answer(args, state, ws).await,
        "copy" => {
            // Copy reports through notices, not by returning an error.
            command_copy(state).await;
            Ok(())
        }
        "retry" => crate::tui::event_loop::retry_last_message(&state, ws).await,
        "expand" => {
            command_expand(&state, args).await;
            Ok(())
        }
        "theme" => command_theme(args, &state, ws).await,
        "mode" => command_mode(args, &state, ws).await,
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
    ws: &WsClient,
) -> Result<(), TuiError> {
    let agent = (!args.trim().is_empty()).then(|| args.trim().to_string());
    let session_id = gw::sessions_create(ws, agent.as_deref()).await?;

    // The gateway work happens *before* the lock is taken. Holding `AppState`
    // across a round-trip queues every other writer behind the network, and
    // the moment handlers stop running inside the event loop it stops being a
    // delay and becomes a deadlock.
    let previous = { state.read().await.current_session.clone() };
    if let Some(old) = previous.filter(|old| old != &session_id) {
        // Without this the connection keeps receiving the old session's deltas.
        if let Err(e) = gw::sessions_unsubscribe(ws, &old).await {
            state.write().await.transcript.push_notice(format!(
                "⚠ could not unsubscribe from {old}: {e} — its events may still arrive"
            ));
        }
    }
    gw::sessions_subscribe(ws, &session_id).await?;

    let mut s = state.write().await;
    s.current_session = Some(session_id.clone());
    s.current_agent = agent;
    // Anything queued belongs to the session being left behind.
    let dropped = s.clear_queue();
    s.transcript.reset();
    // A fresh session has no older pages to walk back to.
    s.history_oldest_ms = None;
    s.transcript
        .push_notice(format!("── new session {session_id} ──"));
    if dropped > 0 {
        s.transcript
            .push_notice(format!("⚠ {dropped} queued message(s) dropped"));
    }
    Ok(())
}

/// `/clear` — clear the conversation context.
///
/// Scrollback belongs to the terminal and cannot be unprinted; saying so is
/// better than leaving the user to wonder why the old text is still there.
async fn command_clear(state: Arc<RwLock<AppState>>, ws: &WsClient) -> Result<(), TuiError> {
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
async fn command_help(state: Arc<RwLock<AppState>>, ws: &WsClient) -> Result<(), TuiError> {
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
        ("Ctrl+Y", "copy the last answer"),
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
        ("history", "[n|more]", "print more of this conversation"),
        ("rename", "<name>", "rename the current session"),
        ("title", "<name>", "alias of /rename"),
        ("undo", "[n]", "walk back over the last turn, or several"),
        ("plan", "", "print the plan the agent is working through"),
        ("save", "[md|html|json|text]", "write this conversation to a file"),
        ("import", "<file>", "read a saved conversation into this session"),
        ("pin", "", "pin or unpin the current session"),
        ("agents", "", "list agents"),
        ("agent", "<id>", "start a session bound to an agent"),
        ("config", "[set <path> <value>]", "show or change configuration"),
        ("status", "", "gateway status"),
        ("tools", "", "list gateway commands"),
        ("model", "<id>", "set the default model"),
        ("answer", "<text>", "answer a pending question"),
        ("copy", "", "copy the last answer to the clipboard"),
        ("retry", "", "resend the last prompt"),
        ("expand", "[n]", "print a recent tool call in full (1 = the last)"),
        ("theme", "[dark|light|auto]", "switch the palette, persisted in config"),
        (
            "mode",
            "[default|accept-edits|plan|bypass]",
            "switch this session's permission mode",
        ),
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
async fn command_status(state: Arc<RwLock<AppState>>, ws: &WsClient) -> Result<(), TuiError> {
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

/// `/history [n|more]` — print this conversation again, oldest first.
///
/// The scrollback is append-only and top-anchored: there is nowhere to put
/// messages older than what is already printed, so paging reprints the window
/// in reading order under a rule rather than splicing it into the middle of
/// the transcript. `more` walks backwards from the oldest message shown so
/// far (the gateway's `before` cursor); a number reprints the newest `n`.
async fn command_history(
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
    let requested = args.trim();

    let Some(session) = state.read().await.current_session.clone() else {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ no session yet — send a message first");
        return Ok(());
    };

    if requested == "more" {
        return command_history_more(state, ws, &session).await;
    }

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
                    .push_notice(format!("⚠ usage: /history [1-{HISTORY_MAX}|more]"));
                return Ok(());
            }
        }
    };

    let (messages, has_more) = gw::chat_history(ws, &session, limit, None).await?;
    let mut lines = vec![blocks::rule(&format!(
        "history: {} message(s)",
        messages.len()
    ))];
    if has_more {
        // Older than the window, so it goes above the messages below.
        lines.push(TranscriptLine::new(
            LineKind::Notice,
            "… older messages omitted — /history more pages back".to_string(),
        ));
    }
    let oldest = messages.iter().filter_map(|m| m.timestamp_ms).min();
    lines.extend(blocks::history_lines(&messages));
    let mut s = state.write().await;
    // A wide reprint may reach further back than whatever was shown before.
    s.history_oldest_ms = match (s.history_oldest_ms, oldest) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    s.transcript.push(lines);
    Ok(())
}

/// `/history more` — the page older than the oldest message shown so far.
async fn command_history_more(
    state: Arc<RwLock<AppState>>,
    ws: &WsClient,
    session: &str,
) -> Result<(), TuiError> {
    let oldest = state.read().await.history_oldest_ms;
    let Some(before) = oldest else {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ no history loaded yet — /history prints the newest page first");
        return Ok(());
    };

    let (messages, has_more) = gw::chat_history(ws, session, HISTORY_DEFAULT, Some(before)).await?;
    if messages.is_empty() {
        state
            .write()
            .await
            .transcript
            .push_notice("… no older messages — the beginning of the conversation");
        return Ok(());
    }

    let page_oldest = messages.iter().filter_map(|m| m.timestamp_ms).min();
    let mut lines = vec![blocks::rule(&format!(
        "history: {} older message(s)",
        messages.len()
    ))];
    if has_more {
        lines.push(TranscriptLine::new(
            LineKind::Notice,
            "… still older messages — /history more again".to_string(),
        ));
    }
    lines.extend(blocks::history_lines(&messages));
    let mut s = state.write().await;
    if let Some(ts) = page_oldest {
        s.history_oldest_ms = Some(ts);
    }
    s.transcript.push(lines);
    Ok(())
}

/// `/tools` — the gateway's command catalog.
async fn command_tools(state: Arc<RwLock<AppState>>, ws: &WsClient) -> Result<(), TuiError> {
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
    ws: &WsClient,
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
async fn command_sessions(state: Arc<RwLock<AppState>>, ws: &WsClient) -> Result<(), TuiError> {
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
    ws: &WsClient,
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

/// `/undo [n]` — walk the conversation back over its last turn, or several.
///
/// The turns are removed by the agent, which is what owns the turn log, the redo
/// stack and the transcript store; this asks rather than editing anything
/// locally. What it deliberately does *not* do is erase what is on screen: the
/// terminal's scrollback cannot be unwritten (the same reason `/clear` says so),
/// and a view that quietly disagreed with the agent's context would be worse
/// than one that says out loud which lines no longer count.
async fn command_undo(
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
    let requested = match args.trim() {
        "" => 1,
        other => match other.parse::<u32>() {
            Ok(n) if n > 0 => n,
            _ => {
                state
                    .write()
                    .await
                    .transcript
                    .push_notice(format!("⚠ /undo takes a positive count, not '{other}'"));
                return Ok(());
            }
        },
    };

    let session = state.read().await.current_session.clone();
    let Some(id) = session else {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ no session to undo in yet");
        return Ok(());
    };

    let undone = gw::sessions_undo(ws, &id, requested).await?;
    let mut s = state.write().await;
    if undone == 0 {
        s.transcript
            .push_notice("⚠ nothing to undo — the agent has no turns left to drop");
        return Ok(());
    }
    let plural = if undone == 1 { "turn" } else { "turns" };
    s.transcript.push_notice(format!(
        "── undone {undone} {plural}; the lines above are still shown, but the agent no longer \
         has them ──"
    ));
    Ok(())
}

/// `/plan` — print the plan the agent is working through.
///
/// The plan is the agent's own, made by the planner and advanced through the
/// `update_plan` tool, so this prints what it says rather than anything the TUI
/// tracks. A conversation with no plan says so: that is the ordinary state, and
/// it is not a failure.
async fn command_plan(state: Arc<RwLock<AppState>>, ws: &WsClient) -> Result<(), TuiError> {
    let session = state.read().await.current_session.clone();
    let Some(id) = session else {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ no session yet");
        return Ok(());
    };

    let plan = gw::sessions_plan(ws, &id).await?;
    let mut s = state.write().await;
    let Some(plan) = plan else {
        s.transcript
            .push_notice("no active plan — the agent works turn by turn until it makes one");
        return Ok(());
    };

    let mut lines = vec![TranscriptLine::new(
        LineKind::Notice,
        format!("🎯 plan: {}", plan.goal),
    )];
    for (index, task) in plan.tasks.iter().enumerate() {
        // The marker is the whole point of the view: where the agent is, and
        // what is behind it.
        let marker = match (task.done, task.current) {
            (true, _) => "✔",
            (false, true) => "▶",
            (false, false) => "·",
        };
        lines.push(TranscriptLine::new(
            LineKind::Notice,
            format!("  {marker} {}. {}", index + 1, task.description),
        ));
        if task.current && !task.expected_outcome.is_empty() {
            lines.push(TranscriptLine::new(
                LineKind::Notice,
                format!("      expected: {}", task.expected_outcome),
            ));
        }
    }
    let done = plan.tasks.iter().filter(|t| t.done).count();
    lines.push(TranscriptLine::new(
        LineKind::Notice,
        format!(
            "  {done}/{} done{}",
            plan.tasks.len(),
            if plan.is_complete {
                " — complete"
            } else {
                ""
            }
        ),
    ));
    s.transcript.push(lines);
    Ok(())
}

/// `/save [format]` — write this conversation to a file.
///
/// The gateway writes it, from the session store, because that is the durable
/// record: a conversation being talked to and one untouched since the last start
/// export the same way. Which formats exist is the gateway's answer too — this
/// only checks the spelling so a typo comes back as a sentence rather than as a
/// file with the wrong extension.
async fn command_save(
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
    let format =
        match args.trim() {
            "" => "md",
            other => match other.to_ascii_lowercase().as_str() {
                "md" | "markdown" => "md",
                "html" => "html",
                "json" => "json",
                "text" | "txt" => "text",
                _ => {
                    state.write().await.transcript.push_notice(format!(
                        "⚠ /save writes md, html, json or text — not '{other}'"
                    ));
                    return Ok(());
                }
            },
        };

    let session = state.read().await.current_session.clone();
    let Some(id) = session else {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ no session to save yet");
        return Ok(());
    };

    match gw::sessions_export(ws, &id, format).await {
        Ok(path) if !path.is_empty() => {
            state
                .write()
                .await
                .transcript
                .push_notice(format!("💾 saved as {format}: {path}"));
        }
        // The gateway refused with a reason worth showing verbatim: "this
        // session has no messages" is the common one, and it is not a bug.
        Ok(_) => {
            state
                .write()
                .await
                .transcript
                .push_notice("⚠ the gateway reported no path for the export");
        }
        Err(e) => {
            state
                .write()
                .await
                .transcript
                .push_notice(format!("✘ /save: {e}"));
        }
    }
    Ok(())
}

/// `/import <file>` — read a saved conversation into this session.
///
/// The counterpart of `/save`, and narrower on purpose: only the JSON rendering
/// can be read back, because the others are documents. The gateway names the
/// directory, so what is given here is a file name — one it wrote, or one put
/// where exports land.
async fn command_import(
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
    let file = args.trim();
    if file.is_empty() {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ usage: /import <file> (a .json export from /save)");
        return Ok(());
    }

    let session = state.read().await.current_session.clone();
    let Some(id) = session else {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ no session to import into yet");
        return Ok(());
    };

    match gw::sessions_import(ws, &id, file).await {
        Ok(messages) => {
            let plural = if messages == 1 { "message" } else { "messages" };
            state
                .write()
                .await
                .transcript
                .push_notice(format!("📥 imported {messages} {plural} from {file}"));
        }
        // The gateway's refusals are its own answers — a path instead of a
        // name, a rendering instead of data, an empty file — and each of them
        // is the sentence the user needs.
        Err(e) => {
            state
                .write()
                .await
                .transcript
                .push_notice(format!("✘ /import: {e}"));
        }
    }
    Ok(())
}

/// `/pin` — pin or unpin the current session.
async fn command_pin(state: Arc<RwLock<AppState>>, ws: &WsClient) -> Result<(), TuiError> {
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

/// `/agents` — list the agents the gateway knows, plus this session's
/// delegated tasks as the TUI has learned them from push events.
async fn command_agents(state: Arc<RwLock<AppState>>, ws: &WsClient) -> Result<(), TuiError> {
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
    let tasks = delegation_task_lines(&s.delegation_tasks);
    if tasks.is_empty() {
        s.transcript.push_notice("no delegated tasks running");
    } else {
        for line in tasks {
            s.transcript.push_notice(line);
        }
        s.transcript.push_notice(
            "delegated task rows are learned from live events; finished ones graduate to \
             notices above",
        );
    }
    s.agents = agents;
    Ok(())
}

/// The `/agents` section for delegated tasks: one line per row the TUI is
/// holding, running first in insertion order.
fn delegation_task_lines(rows: &[crate::tui::state::DelegationRow]) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!("delegated tasks ({}):", rows.len())];
    for row in rows {
        lines.push(format!("  {}", row.listing_line()));
    }
    lines
}

/// `/agent <id>` — start a session bound to an agent.
async fn command_agent(
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &WsClient,
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
    ws: &WsClient,
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

/// How much of the last answer `/copy` puts on the clipboard, in bytes.
///
/// Most terminals cap an OSC 52 payload at a few kilobytes; a longer answer
/// is truncated *at a character boundary* before it is encoded.
const COPY_MAX_BYTES: usize = 4096;

/// `/copy` — copy the last finished answer to the terminal's clipboard.
///
/// The clipboard belongs to the terminal, not the gateway, so a copy is local
/// work: it works offline and while a turn runs, and reports through a notice
/// either way.
pub async fn command_copy(state: Arc<RwLock<AppState>>) {
    let Some(text) = state.read().await.last_assistant_text.clone() else {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ no finished answer to copy yet");
        return;
    };
    if text.is_empty() {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ the last answer is empty — nothing to copy");
        return;
    }
    let (payload, truncated) = if text.len() > COPY_MAX_BYTES {
        let mut end = COPY_MAX_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        (&text[..end], true)
    } else {
        (text.as_str(), false)
    };
    let mut out = std::io::stdout();
    let mut s = state.write().await;
    match write_osc52(&mut out, payload) {
        Ok(()) => {
            if truncated {
                s.transcript.push_notice(format!(
                    "✓ copied {} of {} answer bytes (truncated)",
                    payload.len(),
                    text.len()
                ));
            } else {
                s.transcript
                    .push_notice(format!("✓ copied the {} byte answer", payload.len()));
            }
        }
        Err(e) => s
            .transcript
            .push_notice(format!("✘ clipboard write failed: {e}")),
    }
}

/// Emit `text` as an OSC 52 clipboard payload on `out`.
///
/// The escape moves no cursor, so writing it on the event lane between frames
/// is safe — it just must never happen during a render.
pub fn write_osc52(out: &mut impl std::io::Write, text: &str) -> std::io::Result<()> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let encoded = STANDARD.encode(text.as_bytes());
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()
}

/// Ask the terminal to get the user's attention, on `out`.
///
/// A prompt that blocks the turn — an approval, an `ask_user` question — is the
/// one thing in this TUI that needs a person, and it does nothing at all until
/// they look. Without this, a long turn that stops for approval waits in
/// silence on a screen nobody is watching.
///
/// Two channels on purpose: the bell is what every terminal has (if it is
/// turned on), and `OSC 777;notify` is the toast kitty, WezTerm and Ghostty
/// draw. Terminals that do not know the OSC ignore it, so this degrades to just
/// the bell. Like the clipboard escape it moves no cursor, so it is safe on the
/// event lane between frames and must never run during a render.
pub fn write_notify(out: &mut impl std::io::Write, title: &str, body: &str) -> std::io::Result<()> {
    // The OSC parameter is `;`-separated and BEL/ESC terminate it, so neither
    // may appear in the text.
    let clean = |s: &str| s.replace(';', ",").replace(['\x07', '\x1b'], "");
    write!(out, "\x07\x1b]777;notify;{};{}\x07", clean(title), clean(body))?;
    out.flush()
}

/// `/expand [n]` — print a recent tool call in full.
///
/// The live preview truncates a call's arguments and result to a few rows
/// each. The recent calls are kept whole in the state; this reprints one with
/// no cap, so a payload the live view cut off is still inspectable. No
/// argument means the most recent call, which is what `Ctrl+O` sends.
///
/// A *reprint* rather than a fold, because scrollback is append-only — a line
/// written there is never rewritten — so "expanding" something already on
/// screen can only mean printing it again, in full, below.
pub async fn command_expand(state: &Arc<RwLock<AppState>>, args: &str) {
    let nth = match args.trim() {
        "" => 1,
        other => match other.parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                state.write().await.transcript.push_notice(
                    "⚠ /expand takes a count back from the newest (1 = the last call)".to_string(),
                );
                return;
            }
        },
    };

    let mut s = state.write().await;
    let Some(call) = s.recent_tool(nth).cloned() else {
        let held = s.recent_tools.len();
        s.transcript.push_notice(if held == 0 {
            "⚠ no tool call yet — run a turn that calls a tool".to_string()
        } else {
            format!("⚠ only {held} recent tool calls held — /expand 1 is the newest")
        });
        return;
    };
    let name = call.name;
    let args = call.args;
    let result = call.result;
    s.transcript.push_separator();
    // `▾` marks the uncapped reprint, so it reads as complete where the live
    // preview showed `▸` and a `…`.
    s.transcript
        .push(vec![TranscriptLine::new(LineKind::Tool, format!("⚙ {name} ▾"))]);
    if let Some(args) = args {
        // `usize::MAX` rows: nothing truncated, the verbatim arguments.
        for row in blocks::args_lines(&args, "  ", usize::MAX) {
            s.transcript
                .push(vec![TranscriptLine::new(LineKind::Tool, row)]);
        }
    }
    // The result is expanded the same way: first row carries the `↳` marker,
    // mirroring the live preview's prefix.
    if let Some(result) = result {
        let rows = blocks::args_lines(&result, "  ", usize::MAX);
        for (i, row) in rows.iter().enumerate() {
            let row = if i == 0 {
                format!("  ↳ {name}: {}", row.trim_start())
            } else {
                row.clone()
            };
            s.transcript
                .push(vec![TranscriptLine::new(LineKind::ToolResult, row)]);
        }
    }
    s.transcript.push_separator();
}

/// `/config` and `/config set <path> <value>`.
async fn command_config(
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &WsClient,
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
async fn command_config_show(state: Arc<RwLock<AppState>>, ws: &WsClient) -> Result<(), TuiError> {
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

/// `/mode [default|accept-edits|plan|bypass]` — switch this session's
/// permission mode. No argument reports the current mode. The override is
/// ephemeral: it lives on the gateway until restart, which falls back to the
/// configured `[permissions].mode` default. The gateway is the authority —
/// `bypass` without `[permissions].allow_bypass` comes back as an error the
/// notice shows verbatim.
async fn command_mode(
    args: &str,
    state: &Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
    let requested = args.trim();
    if requested.is_empty() {
        let current = state.read().await.permission_mode;
        state.write().await.transcript.push_notice(match current {
            crate::tools::PermissionMode::Default => {
                "mode: default (the gateway config decides; /mode plan switches this session)"
                    .to_string()
            }
            other => format!(
                "mode: {} (this session; restart falls back to the configured default)",
                other.as_str()
            ),
        });
        return Ok(());
    }
    let Some(mode) = crate::tools::PermissionMode::parse(requested) else {
        state
            .write()
            .await
            .transcript
            .push_notice("⚠ /mode is one of default, accept-edits, plan or bypass".to_string());
        return Ok(());
    };
    let session_id = state.read().await.current_session.clone();
    match gw::sessions_set_mode(ws, session_id.as_deref().unwrap_or(""), mode.as_str()).await {
        Ok(()) => {
            {
                let mut s = state.write().await;
                s.permission_mode = mode;
                s.transcript.push_notice(format!(
                    "✔ permission mode: {} (this session; restart falls back to the configured \
                     default)",
                    mode.as_str()
                ));
            }
            Ok(())
        }
        Err(e) => {
            state.write().await.transcript.push_notice(format!("✘ {e}"));
            Ok(())
        }
    }
}

/// `/theme [dark|light|auto]` — switch the palette now and persist it.
///
/// The setting lives in gateway config (`tui.theme`), so the next launch
/// comes up the same way. The flip is immediate: the chosen dark/light
/// palette replaces the active one, degraded to what this terminal can
/// render. A failed write keeps the current theme.
async fn command_theme(
    args: &str,
    state: &Arc<RwLock<AppState>>,
    ws: &WsClient,
) -> Result<(), TuiError> {
    let setting =
        match args.trim() {
            "" | "auto" => crate::gateway::ThemeSetting::Auto,
            "dark" => crate::gateway::ThemeSetting::Dark,
            "light" => crate::gateway::ThemeSetting::Light,
            other => {
                state.write().await.transcript.push_notice(format!(
                    "⚠ /theme is one of dark, light or auto (got \"{other}\")"
                ));
                return Ok(());
            }
        };
    let revision = state.read().await.config_revision.clone();
    // `ThemeSetting` serializes to its own lowercase name.
    let value = serde_json::to_value(setting).unwrap_or_else(|_| serde_json::json!("auto"));
    match gw::config_set(ws, "tui.theme", value, revision.as_deref()).await {
        Ok(()) => {
            let mut s = state.write().await;
            // Same resolution chain the startup uses: `auto` consults the
            // OSC 11 result caught in the setup window, then `$COLORFGBG`.
            let id = osc11::resolve(setting, osc11::env_hint(), s.startup_bg);
            s.active_theme = crate::tui::ui::Theme::from(id).for_mode(osc11::color_mode());
            s.config_cache = None;
            s.transcript.push_notice(format!("✓ theme = {:?}", setting));
        }
        Err(e) => {
            // The old palette stays: nothing was written.
            state.write().await.transcript.push_notice(format!("✘ {e}"));
        }
    }
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
    if let Some(tui) = value.get("tui").and_then(|v| v.as_object()) {
        if let Some(theme) = tui.get("theme").filter(|v| !v.is_null()) {
            out.push(TranscriptLine::new(
                LineKind::Notice,
                format!("  tui.theme = {}", render_value(theme)),
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
    ws: &WsClient,
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
    use std::time::Duration;

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

        command_history("", Arc::clone(&state), &client)
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

    /// `/history more` pages backwards from the oldest message shown, and
    /// stops cleanly at the beginning of the conversation.
    #[tokio::test]
    async fn history_more_walks_backwards_to_the_beginning() {
        let gateway = TestGateway::start().await;
        let (state, mut client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());
        gateway.with_history(
            (0..10)
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

        // `/history 5` loads the newest five and marks the cursor.
        command_history("5", Arc::clone(&state), &client)
            .await
            .expect("first page");
        let _ = output(&state).await;
        assert_eq!(
            state.read().await.history_oldest_ms,
            Some(1_757_000_005_000),
            "the cursor is message 5's timestamp"
        );

        command_history("more", Arc::clone(&state), &client)
            .await
            .expect("older page");
        let lines = output(&state).await;
        assert!(
            lines.iter().any(|l| l.contains("older message(s)")),
            "announced as older: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.ends_with("message 0")), "reached the start: {lines:?}");
        assert_eq!(state.read().await.history_oldest_ms, Some(1_757_000_000_000));

        // The next `more` is past the beginning: a notice, not empty rules.
        command_history("more", Arc::clone(&state), &client)
            .await
            .expect("past the beginning");
        let lines = output(&state).await;
        assert!(
            lines.iter().any(|l| l.contains("no older messages")),
            "the end is said, not blank: {lines:?}"
        );
    }

    /// `more` with nothing loaded yet points at `/history` instead of
    /// inventing a cursor.
    #[tokio::test]
    async fn history_more_without_a_cursor_asks_for_a_first_page() {
        let gateway = TestGateway::start().await;
        let (state, mut client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());

        command_history("more", Arc::clone(&state), &client)
            .await
            .expect("no cursor");
        let lines = output(&state).await;
        assert!(lines.iter().any(|l| l.contains("no history loaded yet")), "got {lines:?}");
    }

    /// `/new` does not hold the state lock across the gateway.
    ///
    /// It used to: `state.write()` was taken, then two requests were awaited
    /// inside the guard. Today that only queues other writers behind a
    /// round-trip. It is a precondition for moving handlers off the event
    /// loop, where the same code would deadlock instead of merely stall.
    #[tokio::test]
    async fn new_session_does_not_hold_the_state_lock() {
        let gateway = TestGateway::start().await;
        let (state, mut client) = connect(&gateway).await;
        state.write().await.current_session = Some("old".to_string());
        // Park `command_new` in the middle of its gateway work.
        gateway.hold("sessions.subscribe");

        let task = tokio::spawn({
            let state = Arc::clone(&state);
            async move { command_new("", state, &mut client).await }
        });
        gateway
            .wait_for("sessions.subscribe", Duration::from_secs(5))
            .await;

        // While that request is in flight, the state must be readable at once.
        let free = tokio::time::timeout(Duration::from_millis(500), state.write()).await;
        assert!(free.is_ok(), "the state lock is held across the gateway call");
        drop(free);

        task.abort();
    }

    /// An unreadable count is a usage error, not a gateway call.
    #[tokio::test]
    async fn history_rejects_a_count_it_cannot_read() {
        let gateway = TestGateway::start().await;
        let (state, mut client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());

        command_history("banana", Arc::clone(&state), &client)
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

        command_history("", Arc::clone(&state), &client)
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
        assert!(is_local_command("copy"));
        assert!(!is_local_command("usage"));
    }

    /// OSC 52 is BEL-terminated base64 of the payload.
    #[test]
    fn osc52_encodes_the_payload_bel_terminated() {
        use base64::Engine;
        let mut out = Vec::new();
        write_osc52(&mut out, "hi").expect("writes");
        let expected =
            format!("\x1b]52;c;{}\x07", base64::engine::general_purpose::STANDARD.encode("hi"));
        assert_eq!(String::from_utf8(out).expect("utf8"), expected);
    }

    /// The notification is a bell for every terminal, plus the OSC 777 toast
    /// the modern ones draw.
    #[test]
    fn notify_rings_and_emits_an_osc_toast() {
        let mut out = Vec::new();
        write_notify(&mut out, "Title", "Body").expect("writes");
        assert_eq!(String::from_utf8(out).expect("utf8"), "\x07\x1b]777;notify;Title;Body\x07");
    }

    /// A `;` in the text would split the OSC parameter and BEL/ESC would end it
    /// early, so both are removed rather than smuggled through.
    #[test]
    fn notify_sanitises_the_parameters() {
        let mut out = Vec::new();
        write_notify(&mut out, "a;b", "c\x07d\x1be").expect("writes");
        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(text, "\x07\x1b]777;notify;a,b;cde\x07");
    }

    /// `/copy` with no finished answer says so instead of clearing the
    /// clipboard (an empty OSC 52 payload is a clipboard *clear*).
    #[tokio::test]
    async fn copy_without_an_answer_refuses() {
        let gateway = TestGateway::start().await;
        let (state, _client) = connect(&gateway).await;

        command_copy(Arc::clone(&state)).await;
        let lines = output(&state).await;
        assert!(lines[0].contains("no finished answer"), "got {lines:?}");
    }

    /// A long answer is copied at a character boundary, announced as cut.
    #[tokio::test]
    async fn copy_truncates_long_answers_at_a_character_boundary() {
        let gateway = TestGateway::start().await;
        let (state, _client) = connect(&gateway).await;
        // 2000 CJK characters: 6000 bytes, past the 4096 cap. 4096 is not a
        // char boundary (4096 % 3 == 1), so this only passes if the cut walks
        // back to one.
        state.write().await.last_assistant_text = Some("中".repeat(2000));

        command_copy(Arc::clone(&state)).await;
        let lines = output(&state).await;
        assert!(lines[0].contains("truncated"), "got {lines:?}");

        // And an empty answer refuses too: a 0-byte payload would clear.
        state.write().await.last_assistant_text = Some(String::new());
        command_copy(Arc::clone(&state)).await;
        let lines = output(&state).await;
        assert!(lines[0].contains("empty"), "got {lines:?}");
    }

    /// `/expand` with no prior tool call says so.
    #[tokio::test]
    async fn expand_without_a_tool_call_refuses() {
        let gateway = TestGateway::start().await;
        let (state, _client) = connect(&gateway).await;

        command_expand(&state, "").await;
        let lines = output(&state).await;
        assert!(lines[0].contains("no tool call yet"), "got {lines:?}");
    }

    /// `/expand` reprints a call whose payload the live preview truncated.
    #[tokio::test]
    async fn expand_prints_the_call_whole() {
        let gateway = TestGateway::start().await;
        let (state, _client) = connect(&gateway).await;
        // Forty keys: far past TOOL_ARG_LINES_LIVE, so the live rows would
        // show the first four and an ellipsis. The expand must not.
        let mut map = serde_json::Map::new();
        for i in 0..40 {
            map.insert(format!("key_{i}"), serde_json::json!(format!("value_{i}")));
        }
        state
            .write()
            .await
            .begin_tool_call("finder", Some(serde_json::Value::Object(map)));
        state
            .write()
            .await
            .finish_tool_call("finder", Some(serde_json::json!({ "found": 3 })));

        command_expand(&state, "").await;
        let lines = output(&state).await;
        assert!(lines.iter().any(|l| l.contains("key_39")), "got {lines:?}");
        let arg_rows = lines.iter().filter(|l| l.contains("\"key_")).count();
        assert!(
            arg_rows > blocks::TOOL_ARG_LINES_LIVE,
            "expected every key in the output, got {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("↳ finder:")),
            "missing the result marker row, got {lines:?}"
        );
    }

    /// `/expand n` reaches back through the ring: the call before the last
    /// one is still addressable.
    #[tokio::test]
    async fn expand_reaches_earlier_calls_by_count() {
        let gateway = TestGateway::start().await;
        let (state, _client) = connect(&gateway).await;
        {
            let mut s = state.write().await;
            s.begin_tool_call("first", Some(serde_json::json!({ "n": 1 })));
            s.finish_tool_call("first", Some(serde_json::json!({ "ok": 1 })));
            s.begin_tool_call("second", Some(serde_json::json!({ "n": 2 })));
            s.finish_tool_call("second", Some(serde_json::json!({ "ok": 2 })));
        }

        command_expand(&state, "2").await;
        let lines = output(&state).await;
        assert!(lines.iter().any(|l| l.contains("⚙ first ▾")), "got {lines:?}");
        assert!(!lines.iter().any(|l| l.contains("second")), "only the one asked for");

        // Out of range is a notice, not a silent reprint of something else.
        command_expand(&state, "9").await;
        let lines = output(&state).await;
        assert!(lines[0].contains("2 recent tool calls held"), "got {lines:?}");

        command_expand(&state, "later").await;
        let lines = output(&state).await;
        assert!(lines[0].contains("takes a count"), "got {lines:?}");
    }

    /// The ring is bounded, and drops the oldest rather than growing.
    #[tokio::test]
    async fn the_recent_tool_ring_is_bounded() {
        let gateway = TestGateway::start().await;
        let (state, _client) = connect(&gateway).await;
        {
            let mut s = state.write().await;
            for i in 0..12 {
                s.begin_tool_call(&format!("call{i}"), None);
            }
            assert_eq!(s.recent_tools.len(), 8, "capped");
            assert_eq!(s.recent_tool(1).map(|c| c.name.as_str()), Some("call11"), "newest");
            assert_eq!(s.recent_tool(8).map(|c| c.name.as_str()), Some("call4"), "oldest held");
            assert!(s.recent_tool(9).is_none(), "and the rest are gone");
        }
    }

    /// A result attaches to the call it belongs to, not to whatever is newest.
    /// A result with no call of its own — a reconnect mid-call — is still kept.
    #[tokio::test]
    async fn results_attach_to_their_own_call() {
        let mut s = AppState::default();
        s.begin_tool_call("a", Some(serde_json::json!({})));
        s.begin_tool_call("b", Some(serde_json::json!({})));
        s.finish_tool_call("a", Some(serde_json::json!({ "for": "a" })));

        assert_eq!(
            s.recent_tool(2).and_then(|c| c.result.clone()),
            Some(serde_json::json!({ "for": "a" }))
        );
        assert_eq!(s.recent_tool(1).and_then(|c| c.result.clone()), None, "b is still running");

        s.finish_tool_call("orphan", Some(serde_json::json!({ "no": "call" })));
        assert_eq!(s.recent_tool(1).map(|c| c.name.as_str()), Some("orphan"));
    }

    /// `/mode` with no argument reports the current mode; a valid argument
    /// switches the session; a junk argument is refused before any wire call.
    #[tokio::test]
    async fn mode_reports_switches_and_refuses() {
        let gateway = TestGateway::start().await;
        let (state, client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());

        // No argument: the current (default) mode, no wire call.
        let _ = command_mode("", &state, &client).await;
        let lines = output(&state).await;
        assert!(lines[0].contains("default"), "got {lines:?}");
        assert!(
            !gateway
                .requests()
                .iter()
                .any(|r| r.method == "sessions.set_mode"),
            "reporting the mode must not touch the wire"
        );

        // A valid argument switches the session.
        let _ = command_mode("plan", &state, &client).await;
        let requests = gateway.requests();
        let set = requests
            .iter()
            .find(|r| r.method == "sessions.set_mode")
            .expect("the switch on the wire");
        assert_eq!(set.params["session_id"], "s1");
        assert_eq!(set.params["mode"], "plan");
        drop(requests);
        let lines = output(&state).await;
        assert!(lines[0].contains("permission mode: plan"), "got {lines:?}");
        assert_eq!(state.read().await.permission_mode, crate::tools::PermissionMode::Plan);

        // A junk argument is refused locally.
        let _ = command_mode("neon", &state, &client).await;
        let lines = output(&state).await;
        assert!(lines[0].contains("one of default"), "got {lines:?}");
        let set_calls = gateway
            .requests()
            .iter()
            .filter(|r| r.method == "sessions.set_mode")
            .count();
        assert_eq!(set_calls, 1, "a refused /mode must not hit the wire again");
    }

    /// `/theme light` writes `tui.theme` and flips the palette now.
    #[tokio::test]
    async fn theme_switch_applies_and_persists() {
        let gateway = TestGateway::start().await;
        let (state, client) = connect(&gateway).await;

        let _ = command_theme("light", &state, &client).await;

        let requests = gateway.requests();
        let set = requests
            .iter()
            .find(|r| r.method == "config.set")
            .expect("a config.set request");
        assert_eq!(set.params["path"], "tui.theme");
        assert_eq!(set.params["value"], "light");
        let mode = osc11::color_mode();
        assert_eq!(
            state.read().await.active_theme,
            crate::tui::ui::Theme::light().for_mode(mode),
            "the palette must switch immediately, degraded to the terminal"
        );
        let lines = output(&state).await;
        assert!(lines[0].contains("theme = Light"), "got {lines:?}");
    }

    /// An unknown value is refused and the current theme stays put.
    #[tokio::test]
    async fn theme_with_an_invalid_name_refuses() {
        let gateway = TestGateway::start().await;
        let (state, client) = connect(&gateway).await;

        command_theme("banana", &state, &client).await;

        let lines = output(&state).await;
        assert!(lines[0].contains("one of dark, light or auto"), "got {lines:?}");
        assert_eq!(
            state.read().await.active_theme,
            crate::tui::ui::Theme::dark(),
            "a refused /theme must not touch the palette"
        );
        assert!(
            !gateway.requests().iter().any(|r| r.method == "config.set"),
            "nothing should have been written"
        );
    }

    /// `/theme auto` consults the same chain the startup used, so the OSC 11
    /// result caught in the setup window drives the palette again.
    #[tokio::test]
    async fn theme_auto_reuses_startup_detection() {
        let gateway = TestGateway::start().await;
        let (state, client) = connect(&gateway).await;
        state.write().await.startup_bg = Some(crate::tui::ui::ThemeId::Light);

        command_theme("auto", &state, &client).await;

        let mode = osc11::color_mode();
        assert_eq!(
            state.read().await.active_theme,
            crate::tui::ui::Theme::light().for_mode(mode),
            "auto must resolve through the startup detection"
        );
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
            "heartbeat": { "enabled": true },
            "tui": { "theme": "light" }
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
        assert!(text.iter().any(|l| l.contains("tui.theme = light")));
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

    /// `/undo` asks the agent to drop the turn and reports how many went —
    /// including when fewer went than were asked for.
    #[tokio::test]
    async fn undo_asks_the_agent_and_reports_what_actually_went() {
        let gateway = TestGateway::start().await;
        gateway.reply(
            "sessions.undo",
            serde_json::json!({ "status": "undone", "undone": 2, "requested": 5 }),
        );
        let (state, client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());

        let _ = command_undo("5", state.clone(), &client).await;

        let requests = gateway.requests();
        let undo = requests
            .iter()
            .find(|r| r.method == "sessions.undo")
            .expect("the undo on the wire");
        assert_eq!(undo.params["session_id"], "s1");
        assert_eq!(undo.params["count"], 5);
        drop(requests);

        // The count reported is the agent's, not the one asked for: saying
        // "undone 5" when two turns existed would be a lie about the context.
        let lines = output(&state).await;
        assert!(lines[0].contains("undone 2 turns"), "got {lines:?}");
    }

    /// Nothing to undo is a notice, not a silent success: the agent answered,
    /// and the answer was "there was nothing there".
    #[tokio::test]
    async fn undo_with_nothing_left_says_so() {
        let gateway = TestGateway::start().await;
        gateway.reply(
            "sessions.undo",
            serde_json::json!({ "status": "undone", "undone": 0, "requested": 1 }),
        );
        let (state, client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());

        let _ = command_undo("", state.clone(), &client).await;

        let lines = output(&state).await;
        assert!(lines[0].contains("nothing to undo"), "got {lines:?}");
    }

    /// A count that is not a count is refused locally, without asking the
    /// agent to undo something nobody asked for.
    #[tokio::test]
    async fn undo_refuses_a_count_it_cannot_read() {
        let gateway = TestGateway::start().await;
        let (state, client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());

        for bad in ["-1", "0", "two", "1.5"] {
            let _ = command_undo(bad, state.clone(), &client).await;
            let lines = output(&state).await;
            assert!(lines[0].contains("positive count"), "{bad}: got {lines:?}");
        }
        assert!(
            !gateway
                .requests()
                .iter()
                .any(|r| r.method == "sessions.undo"),
            "a refused /undo must not reach the wire"
        );
    }

    /// `/title` is `/rename`: same wire call, same effect on the session list.
    /// Driven through the dispatcher, because the alias is what is registered —
    /// a helper the match arm does not reach would test nothing.
    #[tokio::test]
    async fn title_is_rename() {
        let gateway = TestGateway::start().await;
        let (state, client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());

        let _ = handle_slash_command("/title Release triage", state.clone(), &client).await;

        let requests = gateway.requests();
        let rename = requests
            .iter()
            .find(|r| r.method == "sessions.rename")
            .expect("the rename on the wire");
        assert_eq!(rename.params["session_id"], "s1");
        assert_eq!(rename.params["name"], "Release triage");
        drop(requests);
        let lines = output(&state).await;
        assert!(lines[0].contains("Release triage"), "got {lines:?}");
    }

    /// `/plan` prints the plan the agent reports, marking where it is.
    #[tokio::test]
    async fn plan_prints_the_agents_plan() {
        let gateway = TestGateway::start().await;
        gateway.reply(
            "sessions.plan",
            serde_json::json!({
                "session_id": "s1",
                "plan": {
                    "goal": "ship the release",
                    "original_request": "cut 0.4",
                    "is_complete": false,
                    "tasks": [
                        { "description": "bump the version", "expected_outcome": "0.4.0 in Cargo.toml",
                          "current": false, "done": true },
                        { "description": "write the notes", "expected_outcome": "a CHANGELOG section",
                          "current": true, "done": false },
                    ],
                },
            }),
        );
        let (state, client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());

        let _ = command_plan(state.clone(), &client).await;

        let requests = gateway.requests();
        let plan = requests
            .iter()
            .find(|r| r.method == "sessions.plan")
            .expect("the plan on the wire");
        assert_eq!(plan.params["session_id"], "s1");
        drop(requests);

        let lines = output(&state).await;
        let text = lines.join("\n");
        assert!(text.contains("ship the release"), "got {text}");
        assert!(text.contains("✔ 1. bump the version"), "got {text}");
        assert!(text.contains("▶ 2. write the notes"), "got {text}");
        // The expected outcome is shown only for the step being worked on: it is
        // what "done" means now, and a wall of them buries the position.
        assert!(text.contains("expected: a CHANGELOG section"), "got {text}");
        assert!(!text.contains("expected: 0.4.0"), "got {text}");
        assert!(text.contains("1/2 done"), "got {text}");
    }

    /// No plan is the ordinary state of a conversation, not a failure.
    #[tokio::test]
    async fn plan_says_when_there_is_none() {
        let gateway = TestGateway::start().await;
        gateway.reply("sessions.plan", serde_json::json!({ "session_id": "s1", "plan": null }));
        let (state, client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());

        let _ = command_plan(state.clone(), &client).await;

        let lines = output(&state).await;
        assert!(lines[0].contains("no active plan"), "got {lines:?}");
    }

    /// `/save md` asks the gateway to write the conversation and prints where.
    #[tokio::test]
    async fn save_names_the_format_and_reports_the_path() {
        let gateway = TestGateway::start().await;
        gateway.reply(
            "sessions.export",
            serde_json::json!({ "path": "/tmp/transcripts/s1_20261015.md", "format": "md" }),
        );
        let (state, client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());

        let _ = command_save("md", state.clone(), &client).await;

        let requests = gateway.requests();
        let export = requests
            .iter()
            .find(|r| r.method == "sessions.export")
            .expect("the export on the wire");
        assert_eq!(export.params["session_id"], "s1");
        assert_eq!(export.params["format"], "md");
        drop(requests);

        let lines = output(&state).await;
        assert!(lines[0].contains("/tmp/transcripts/s1_20261015.md"), "got {lines:?}");
    }

    /// No argument is the format people want, and a typo is refused before the
    /// gateway is asked to write a file with the wrong extension.
    #[tokio::test]
    async fn save_defaults_to_markdown_and_refuses_a_format_it_does_not_know() {
        let gateway = TestGateway::start().await;
        gateway.reply("sessions.export", serde_json::json!({ "path": "/tmp/x.md" }));
        let (state, client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());

        let _ = command_save("", state.clone(), &client).await;
        let asked = gateway
            .requests()
            .iter()
            .find(|r| r.method == "sessions.export")
            .map(|r| r.params["format"].clone());
        assert_eq!(asked, Some(serde_json::json!("md")));

        let _ = command_save("pdf", state.clone(), &client).await;
        let lines = output(&state).await;
        // The earlier notice is still in the transcript, so this asserts on
        // the line the refused call added rather than on position.
        assert!(lines.iter().any(|l| l.contains("md, html, json or text")), "got {lines:?}");
        let exports = gateway
            .requests()
            .iter()
            .filter(|r| r.method == "sessions.export")
            .count();
        assert_eq!(exports, 1, "a refused /save must not reach the wire");
    }

    /// `/import` sends the file name and reports how much came back.
    #[tokio::test]
    async fn import_sends_the_file_and_reports_the_count() {
        let gateway = TestGateway::start().await;
        gateway.reply(
            "sessions.import",
            serde_json::json!({ "messages": 12, "from": "other-session" }),
        );
        let (state, client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());

        let _ = command_import("triage_20261015.json", state.clone(), &client).await;

        let requests = gateway.requests();
        let import = requests
            .iter()
            .find(|r| r.method == "sessions.import")
            .expect("the import on the wire");
        assert_eq!(import.params["session_id"], "s1");
        assert_eq!(import.params["file"], "triage_20261015.json");
        drop(requests);

        let lines = output(&state).await;
        assert!(lines[0].contains("12 messages"), "got {lines:?}");
    }

    /// The gateway's refusal is the sentence the user needs, so it is shown
    /// rather than replaced with something generic.
    #[tokio::test]
    async fn import_shows_why_the_gateway_refused() {
        let gateway = TestGateway::start().await;
        gateway.reply("sessions.import", serde_json::json!({}));
        gateway.fail_with("sessions.import", "IMPORT_FAILED");
        let (state, client) = connect(&gateway).await;
        state.write().await.current_session = Some("s1".to_string());

        let _ = command_import("notes.md", state.clone(), &client).await;

        let lines = output(&state).await;
        assert!(lines[0].contains("/import"), "got {lines:?}");

        // And no argument is refused without asking the gateway anything.
        let before = gateway.requests().len();
        let _ = command_import("", state.clone(), &client).await;
        let lines = output(&state).await;
        assert!(lines.iter().any(|l| l.contains("usage: /import")), "got {lines:?}");
        assert_eq!(gateway.requests().len(), before, "an empty /import must not hit the wire");
    }
}
