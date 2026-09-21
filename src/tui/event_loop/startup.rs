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
