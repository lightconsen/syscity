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

/// The brand mark in block characters: three towers with a mast on the tallest,
/// over a horizon — the logo (`syscity.png`) as a skyline.
///
/// Same family of glyphs as Claude Code's mark, drawn from our own logo rather
/// than invented: the centre tower is tallest and carries the mast, the two
/// flanking towers are lower, and the horizon runs under them. `LineKind::Logo`
/// renders it in the theme accent, so the whole mark takes the active palette.
///
/// Rows are kept the same width on purpose — uneven rows still render, but the
/// silhouette only reads cleanly when the towers line up column for column.
const LOGO: [&str; 5] = [
    "       ▐▌",
    "  ▗▄▖  ▐█▌  ▗▄▖",
    " ▟███▖▐███▌▗████▖",
    "▐███████████████▌",
    "▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀",
];

/// The startup banner: the skyline mark, branding, the hints a first-time user
/// needs, and the working directory. Pushed as the first thing in scrollback,
/// in the Claude-Code manner — a quiet "who am I talking to" block rather than
/// a full-screen takeover, because this TUI lives inline and the transcript
/// below it is the product.
fn welcome_lines(version: &str, cwd: Option<String>) -> Vec<TranscriptLine> {
    use LineKind::{Heading, Logo, Notice};
    let mut lines = vec![
        TranscriptLine::separator(),
        TranscriptLine::new(Logo, LOGO[0].to_string()),
        TranscriptLine::new(Logo, LOGO[1].to_string()),
        TranscriptLine::new(Logo, LOGO[2].to_string()),
        TranscriptLine::new(Logo, LOGO[3].to_string()),
        TranscriptLine::new(Logo, LOGO[4].to_string()),
        TranscriptLine::new(Heading, format!("# ✻ Syscity v{version}")),
    ];
    if let Some(cwd) = cwd {
        lines.push(TranscriptLine::new(Notice, format!("cwd: {cwd}")));
    }
    lines.push(TranscriptLine::new(
        Notice,
        "/help for commands · /resume <n> to switch sessions · /theme for the palette",
    ));
    lines.push(TranscriptLine::separator());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_carries_logo_branding_hints_and_cwd() {
        let lines = welcome_lines("0.3.8", Some("/Users/x/work".to_string()));
        // blank / 5 logo rows / heading / cwd / hints / blank
        assert_eq!(lines.len(), 10);
        assert!(lines[0].text.is_empty());
        for row in &lines[1..6] {
            assert_eq!(row.kind, LineKind::Logo);
        }
        // The mark is the skyline: a mast, three towers, and the horizon.
        assert!(lines[1].text.contains('▐'), "mast");
        assert!(lines[3].text.contains('▗') && lines[3].text.contains('▖'), "towers");
        assert!(lines[5].text.starts_with('▀'), "horizon under the towers");
        assert_eq!(lines[6].text, "# ✻ Syscity v0.3.8");
        assert_eq!(lines[6].kind, LineKind::Heading);
        assert_eq!(lines[7].text, "cwd: /Users/x/work");
        assert!(lines[8].text.contains("/help for commands"));
        assert!(lines[8].text.contains("/resume <n>"));
        assert!(lines[8].text.contains("/theme"));
        assert!(lines[9].text.is_empty());
    }

    /// The mark is a skyline: the towers rise from the horizon row, so nothing
    /// is wider than it and it starts at the left edge. Rows differ in length —
    /// leading spaces are what align the towers — so this is the property worth
    /// pinning, not equal widths.
    #[test]
    fn the_logo_rises_from_its_left_aligned_base() {
        let base = LOGO[LOGO.len() - 1];
        assert!(base.starts_with('▀'), "the last row is the horizon: {base:?}");
        let base_width = base.chars().count();
        for row in LOGO {
            assert!(
                row.chars().count() <= base_width,
                "logo row {row:?} is wider than the horizon"
            );
            assert!(!row.ends_with(' '), "trailing space would add width: {row:?}");
        }
    }

    #[test]
    fn welcome_omits_the_cwd_line_when_none() {
        let lines = welcome_lines("0.3.8", None);
        assert_eq!(lines.len(), 9);
        assert!(lines.iter().all(|l| !l.text.starts_with("cwd:")));
        // The logo is still there.
        assert_eq!(lines[1..6].iter().map(|l| l.kind).collect::<Vec<_>>(), vec![LineKind::Logo; 5]);
    }
}
