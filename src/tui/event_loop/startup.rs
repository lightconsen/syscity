//! Everything that has to happen before the first paint.

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::tui::app::SessionChoice;
use crate::tui::gateway_calls as gw;
use crate::tui::osc11;
use crate::tui::resume;
use crate::tui::state::AppState;
use crate::tui::transcript::{LineKind, TranscriptLine};
use crate::tui::ws_client::WsClient;

/// Everything that has to happen before the first paint.
pub(super) async fn startup(
    state: &Arc<RwLock<AppState>>,
    client: &WsClient,
    session: &SessionChoice,
) {
    // The welcome banner is the first thing in scrollback. The version comes
    // from the handshake, which run_app records in state before this task
    // starts; the cwd is the TUI process's own — for the common local-daemon
    // case that is also where the gateway's workspace work happens.
    {
        let s = state.read().await;
        let version = match &s.connection {
            crate::tui::state::ConnectionState::Connected { server_version, .. } => {
                server_version.clone()
            }
            _ => String::new(),
        };
        let cwd = std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string());
        drop(s);
        state
            .write()
            .await
            .transcript
            .push(welcome_lines(&version, cwd));
    }

    // Resolve the theme before the first paint: `tui.theme` in the gateway
    // config, `auto` consulting the OSC 11 result from the setup window. A
    // silent failure anywhere just keeps the dark default — the theme is a
    // nicety, and the first frame must not wait on it.
    if let Ok(config) = gw::config_get(client).await {
        let setting = config
            .get("tui")
            .and_then(|t| t.get("theme"))
            .and_then(|v| serde_json::from_value::<crate::gateway::ThemeSetting>(v.clone()).ok())
            .unwrap_or_default();
        let detected = state.read().await.startup_bg;
        let id = osc11::resolve(setting, osc11::env_hint(), detected);
        // The palette is modeless; what the terminal can render is not, so
        // degrade the chosen dark/light colors at the last possible moment.
        state.write().await.active_theme =
            crate::tui::ui::Theme::from(id).for_mode(osc11::color_mode());
    }

    // The catalog feeds `/help` and Tab completion.
    if let Ok(catalog) = gw::commands_list(client).await {
        let mut s = state.write().await;
        if !catalog.is_empty() {
            s.command_list = catalog;
        }
    }

    match resume::resolve_startup_session(session, state, client).await {
        Ok(resume::StartupSession::Use(id)) => {
            if let Err(e) = resume::switch_to(&id, state, client).await {
                state
                    .write()
                    .await
                    .transcript
                    .push_notice(format!("⚠ could not resume {id}: {e}"));
            }
        }
        Ok(resume::StartupSession::ListAndWait) => {
            // `--resume` with no id: show the list, keep `/resume <n>` as the
            // way in.
            if let Ok(sessions) = resume::refresh_sessions(state, client).await {
                let mut lines = vec![TranscriptLine::new(
                    LineKind::Notice,
                    format!("{} sessions:", sessions.len()),
                )];
                for line in resume::session_lines(&sessions) {
                    lines.push(TranscriptLine::new(LineKind::Notice, line));
                }
                lines.push(TranscriptLine::new(
                    LineKind::Notice,
                    "resume one with /resume <number>",
                ));
                state.write().await.transcript.push(lines);
            }
        }
        Ok(resume::StartupSession::Fresh) => {}
        Err(e) => {
            state
                .write()
                .await
                .transcript
                .push_notice(format!("⚠ could not list sessions: {e}"));
        }
    }

    let greeting = {
        let s = state.read().await;
        match (&s.current_session, s.connection.label()) {
            (Some(id), label) => format!("── connected ({label}) · session {id} ──"),
            (None, label) => format!("── connected ({label}) · Ctrl+H for help ──"),
        }
    };
    state.write().await.transcript.push_notice(greeting);
}

/// The startup banner: branding, the three hints a first-time user needs, and
/// the working directory. Pushed as the first thing in scrollback, in the
/// Claude-Code manner — a quiet "who am I talking to" block rather than a
/// full-screen takeover, because this TUI lives inline and the transcript
/// below it is the product.
fn welcome_lines(version: &str, cwd: Option<String>) -> Vec<TranscriptLine> {
    use LineKind::{Heading, Notice};
    let mut lines = vec![
        TranscriptLine::separator(),
        TranscriptLine::new(Heading, format!("# ✻ Syscity v{version}")),
        TranscriptLine::new(
            Notice,
            "/help for commands · /resume <n> to switch sessions · /theme for the palette",
        ),
    ];
    if let Some(cwd) = cwd {
        lines.push(TranscriptLine::new(Notice, format!("cwd: {cwd}")));
    }
    lines.push(TranscriptLine::separator());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_carries_branding_hints_and_cwd() {
        let lines = welcome_lines("0.3.8", Some("/Users/x/work".to_string()));
        // blank / heading / tips / cwd / blank
        assert_eq!(lines.len(), 5);
        assert!(lines[0].text.is_empty());
        assert_eq!(lines[1].text, "# ✻ Syscity v0.3.8");
        assert_eq!(lines[1].kind, LineKind::Heading);
        assert!(lines[2].text.contains("/help for commands"));
        assert!(lines[2].text.contains("/resume <n>"));
        assert!(lines[2].text.contains("/theme"));
        assert_eq!(lines[3].text, "cwd: /Users/x/work");
        assert!(lines[4].text.is_empty());
    }

    #[test]
    fn welcome_omits_the_cwd_line_when_none() {
        let lines = welcome_lines("0.3.8", None);
        assert_eq!(lines.len(), 4);
        assert!(lines.iter().all(|l| !l.text.starts_with("cwd:")));
    }
}
