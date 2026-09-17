//! Session resolution: which conversation to open, and how to switch.
//!
//! Without a session sidebar, "which conversation am I in" is answered by
//! `--continue` / `--resume` at launch and `/resume` in-session. The list is
//! printed into the scrollback like any other command output.
// INVARIANTS-NONE: session selection helpers; state lives in `AppState`.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::tui::app::SessionChoice;
use crate::tui::error::TuiError;
use crate::tui::gateway_calls::{self as gw, SessionInfo};
use crate::tui::state::AppState;
use crate::tui::transcript::LineKind;
use crate::tui::ui::blocks;
use crate::tui::ws_client::WsClient;

/// Refresh the session list held in state and return it.
pub async fn refresh_sessions(
    state: &Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<Vec<SessionInfo>, TuiError> {
    let sessions = gw::sessions_list(ws).await?;
    state.write().await.sessions = sessions.clone();
    Ok(sessions)
}

/// What the startup flags resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupSession {
    /// Start a fresh conversation.
    Fresh,
    /// Open this session.
    Use(String),
    /// `--resume` with no id: print the list and let the user choose with
    /// `/resume <n>`.
    ListAndWait,
}

/// Resolve the startup flags into a session to open.
pub async fn resolve_startup_session(
    choice: &SessionChoice,
    state: &Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<StartupSession, TuiError> {
    match choice {
        SessionChoice::New => Ok(StartupSession::Fresh),
        SessionChoice::Resume(id) if id.is_empty() => Ok(StartupSession::ListAndWait),
        SessionChoice::Resume(id) => Ok(StartupSession::Use(id.clone())),
        SessionChoice::Continue => {
            let sessions = refresh_sessions(state, ws).await?;
            Ok(match most_recent(&sessions) {
                Some(session) => StartupSession::Use(session.id.clone()),
                None => StartupSession::Fresh,
            })
        }
    }
}

/// The most recently active session.
pub fn most_recent(sessions: &[SessionInfo]) -> Option<&SessionInfo> {
    sessions
        .iter()
        // The timestamp is RFC3339, which sorts correctly as a string.
        .max_by(|a, b| a.last_activity.cmp(&b.last_activity))
}

/// Render the session list as printable lines.
pub fn session_lines(sessions: &[SessionInfo]) -> Vec<String> {
    sessions
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let name = s.name.as_deref().unwrap_or("(unnamed)");
            let agent = s.agent_id.as_deref().unwrap_or("-");
            let when = s.last_activity.as_deref().unwrap_or("-");
            let pin = if s.pinned { "📌 " } else { "" };
            format!("  {:>2}. {pin}{name}  [{agent}]  {} msgs  {when}", idx + 1, s.message_count)
        })
        .collect()
}

/// Resolve a `/resume` argument — by list position or by id.
pub fn pick_session<'a>(sessions: &'a [SessionInfo], arg: &str) -> Option<&'a SessionInfo> {
    if arg.is_empty() {
        return None;
    }
    if let Ok(idx) = arg.parse::<usize>() {
        return sessions.get(idx.checked_sub(1)?);
    }
    sessions.iter().find(|s| s.id == arg)
}

/// Switch the TUI to `id`: unsubscribe, subscribe, and print that session's
/// history below a rule.
pub async fn switch_to(
    id: &str,
    state: &Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let previous = { state.read().await.current_session.clone() };
    if let Some(previous) = previous.filter(|p| p != id) {
        // Without this the connection keeps receiving the old session's deltas.
        if let Err(e) = gw::sessions_unsubscribe(ws, &previous).await {
            state.write().await.transcript.push_notice(format!(
                "⚠ could not unsubscribe from {previous}: {e} — its events may still arrive"
            ));
        }
    }
    gw::sessions_subscribe(ws, id).await?;

    {
        let mut s = state.write().await;
        s.transcript.reset();
        // A queued message was written for the session being left.
        s.clear_queue();
        s.current_session = Some(id.to_string());
        s.current_agent = s
            .sessions
            .iter()
            .find(|x| x.id == id)
            .and_then(|x| x.agent_id.clone());
        s.transcript
            .push(vec![blocks::rule(&format!("resumed {id}"))]);
    }

    match gw::chat_history(ws, id, 100).await {
        Ok((messages, has_more)) => {
            let mut lines = Vec::new();
            if has_more {
                lines.push(crate::tui::transcript::TranscriptLine::new(
                    LineKind::Notice,
                    "… older messages omitted — /history prints more",
                ));
            }
            lines.extend(blocks::history_lines(&messages));
            state.write().await.transcript.push(lines);
        }
        Err(e) => {
            state
                .write()
                .await
                .transcript
                .push_notice(format!("⚠ could not load history: {e}"));
        }
    }
    Ok(())
}

/// `/resume` — list sessions, or switch to the one named.
pub async fn command_resume(
    args: &str,
    state: Arc<RwLock<AppState>>,
    ws: &mut WsClient,
) -> Result<(), TuiError> {
    let sessions = refresh_sessions(&state, ws).await?;
    let arg = args.trim();

    if arg.is_empty() {
        let mut s = state.write().await;
        if sessions.is_empty() {
            s.transcript.push_notice("no sessions to resume");
        } else {
            s.transcript
                .push_notice(format!("{} sessions:", sessions.len()));
            for line in session_lines(&sessions) {
                s.transcript.push_notice(line);
            }
            s.transcript
                .push_notice("resume one with /resume <number> or /resume <id>");
        }
        return Ok(());
    }

    match pick_session(&sessions, arg) {
        Some(session) => {
            let id = session.id.clone();
            switch_to(&id, &state, ws).await?;
        }
        None => {
            state
                .write()
                .await
                .transcript
                .push_notice(format!("⚠ no session matches \"{arg}\" — /resume lists them"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, activity: &str, pinned: bool) -> SessionInfo {
        SessionInfo {
            id: id.to_string(),
            name: Some(format!("name-{id}")),
            last_activity: Some(activity.to_string()),
            message_count: 2,
            pinned,
            ..Default::default()
        }
    }

    #[test]
    fn most_recent_picks_the_latest_timestamp() {
        let sessions = vec![
            session("a", "2026-09-14T10:00:00+00:00", false),
            session("b", "2026-09-15T09:00:00+00:00", false),
            session("c", "2026-09-13T23:00:00+00:00", false),
        ];
        assert_eq!(most_recent(&sessions).map(|s| s.id.as_str()), Some("b"));
    }

    #[test]
    fn most_recent_of_nothing_is_none() {
        assert!(most_recent(&[]).is_none());
    }

    #[test]
    fn pick_accepts_a_position_or_an_id() {
        let sessions = vec![
            session("a", "2026-09-14T10:00:00+00:00", false),
            session("b", "2026-09-15T09:00:00+00:00", true),
        ];
        assert_eq!(pick_session(&sessions, "1").map(|s| s.id.as_str()), Some("a"));
        assert_eq!(pick_session(&sessions, "2").map(|s| s.id.as_str()), Some("b"));
        assert_eq!(pick_session(&sessions, "b").map(|s| s.id.as_str()), Some("b"));
        assert!(pick_session(&sessions, "3").is_none(), "out of range");
        assert!(pick_session(&sessions, "0").is_none(), "positions start at 1");
        assert!(pick_session(&sessions, "").is_none());
    }

    #[test]
    fn session_lines_show_position_name_agent_and_pin() {
        let sessions = vec![session("b", "2026-09-15T09:00:00+00:00", true)];
        let lines = session_lines(&sessions);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("name-b"));
        assert!(lines[0].contains("📌"));
        assert!(lines[0].contains("2026-09-15"));
    }
}
