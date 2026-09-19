//! Terminal setup, panic recovery, and TUI orchestration.
//!
//! The TUI runs *inline*: it never takes over the alternate screen, so the
//! conversation lands in the terminal's own scrollback — native scrolling,
//! native text selection, and the transcript survives exit. Only the bottom
//! [`LIVE_HEIGHT`] rows are ours to redraw.
//!
//! Mouse capture is deliberately not enabled: it would suppress the terminal's
//! own selection, which is half the point of running inline.
// INVARIANTS-NONE: process-lifetime terminal setup; owns no shared state.

use std::io::{stdout, IsTerminal, Stdout, Write};
use std::sync::Arc;

use crossterm::cursor::Show;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::RwLock;

use crate::tui::auth::AuthConfig;
use crate::tui::error::TuiError;
use crate::tui::event_loop;
use crate::tui::state::AppState;
use crate::tui::ui::live::LIVE_HEIGHT;
use crate::tui::ws_client::WsClient;

/// Which conversation to open on startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionChoice {
    /// Start fresh.
    New,
    /// Resume the most recently active session.
    Continue,
    /// Resume a specific session.
    Resume(String),
}

/// Where to connect, kept for reconnects.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// WebSocket URL.
    pub url: String,
    /// Auth material.
    pub auth: AuthConfig,
    /// Session to subscribe to on connect, if any.
    pub session: Option<String>,
}

/// Run the TUI.
pub async fn run(
    host: &str,
    port: u16,
    token: Option<&str>,
    session: SessionChoice,
) -> crate::Result<()> {
    let preselected = match &session {
        SessionChoice::Resume(id) => Some(id.clone()),
        _ => None,
    };
    let auth = AuthConfig::from_token(token);
    let url = auth.ws_url(host, port, preselected.as_deref(), "tui");
    let endpoint = Endpoint {
        url,
        auth,
        session: preselected,
    };

    let result = if stdout().is_terminal() {
        run_inline(endpoint, session).await
    } else {
        // Piped output: no cursor addressing, no raw mode — just lines.
        event_loop::run_plain(endpoint, session).await
    };
    result.map_err(|e| crate::SyscityError::Validation(format!("TUI error: {e}")))
}

/// Run the interactive inline TUI.
async fn run_inline(endpoint: Endpoint, session: SessionChoice) -> Result<(), TuiError> {
    // An inline viewport is anchored to the cursor, which ratatui learns by
    // asking the terminal (`ESC[6n`). A terminal that does not answer — a
    // piped pty, an exotic emulator — makes that query time out. Degrade to
    // line mode rather than dying at startup.
    let mut terminal = match setup_terminal() {
        Ok(terminal) => terminal,
        Err(e) => {
            let _ = disable_raw_mode();
            eprintln!("inline rendering unavailable ({e}); falling back to line mode");
            return event_loop::run_plain(endpoint, session).await;
        }
    };

    // A panic must not leave the terminal in raw mode with a hidden cursor.
    // The hook is taken back when this function returns — see `PanicHookGuard`
    // — because it restores a terminal the process no longer owns.
    let original: PanicHook = Arc::from(std::panic::take_hook());
    std::panic::set_hook({
        let original = Arc::clone(&original);
        Box::new(move |info| {
            let _ = restore_terminal();
            original(info);
        })
    });
    let _hook_guard = PanicHookGuard(Some(original));

    // Drills for the two ways this can end badly, both only in debug builds.
    // The terminal is raw and the cursor hidden at this point, so it is the
    // worst moment for either. tests/tui_pty.rs uses them to prove the
    // terminal comes back.
    #[cfg(debug_assertions)]
    if std::env::var_os("SYSCITY_TUI_DEBUG_PANIC").is_some() {
        panic!("SYSCITY_TUI_DEBUG_PANIC drill");
    }
    // A fatal error returns rather than unwinding: it takes the `restore?`
    // path below, not the panic hook.
    #[cfg(debug_assertions)]
    let result: Result<(), TuiError> = if std::env::var_os("SYSCITY_TUI_DEBUG_FATAL").is_some() {
        Err(TuiError::Terminal(std::io::Error::other("SYSCITY_TUI_DEBUG_FATAL drill")))
    } else {
        run_app(&mut terminal, endpoint, session).await
    };
    #[cfg(not(debug_assertions))]
    let result = run_app(&mut terminal, endpoint, session).await;

    let restore = restore_terminal();
    if let Err(ref e) = result {
        // Safe to print now: raw mode is off and the cursor is shown.
        eprintln!("TUI error: {e}");
    }
    restore?;
    result
}

/// The process's panic hook, as `std::panic` stores it — held behind an `Arc`
/// because the installed closure and the guard need the same copy.
type PanicHook = Arc<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync>;

/// Holds the panic hook for exactly as long as the terminal is ours.
///
/// Dropping it puts the original back, on the ordinary return *and* on the
/// unwind — the installed hook calls `restore_terminal`, which is the right
/// thing while the TUI owns the terminal and pointless afterwards.
struct PanicHookGuard(Option<PanicHook>);

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        if let Some(original) = self.0.take() {
            std::panic::set_hook(Box::new(move |info| original(info)));
        }
    }
}

/// Initialize crossterm and ratatui for an inline viewport.
fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>, TuiError> {
    enable_raw_mode()?;
    let backend = CrosstermBackend::new(stdout());
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(LIVE_HEIGHT),
        },
    )
    .map_err(TuiError::Terminal)
}

/// Hand the terminal back the way we found it.
fn restore_terminal() -> Result<(), TuiError> {
    disable_raw_mode()?;
    let mut out = stdout();
    // Park the cursor on a fresh line so the shell prompt does not land on top
    // of the composer, and make sure it is visible again.
    let _ = out.write_all(b"\r\n");
    let _ = out.flush();
    crossterm::execute!(out, Show)?;
    Ok(())
}

/// Core application lifecycle.
async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    endpoint: Endpoint,
    session: SessionChoice,
) -> Result<(), TuiError> {
    let (ws_client, hello) =
        WsClient::connect(&endpoint.url, &endpoint.auth, &["chat", "read", "write"]).await?;

    let state = std::sync::Arc::new(RwLock::new(AppState::default()));
    {
        let mut s = state.write().await;
        s.connection = crate::tui::state::ConnectionState::Connected {
            features: hello.features,
            scopes_granted: hello.scopes_granted,
            server_version: hello.server.version,
        };
    }

    event_loop::run(
        terminal,
        state,
        ws_client,
        endpoint,
        session,
        &mut crate::tui::input::CrosstermInput,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Restoring the terminal must be safe even when it was never put into raw
    /// mode (e.g. after a failed setup).
    #[test]
    fn restore_terminal_does_not_panic() {
        let _ = restore_terminal();
    }

    #[test]
    fn session_choice_maps_to_the_url_parameter() {
        let auth = AuthConfig::None;
        assert_eq!(
            auth.ws_url("127.0.0.1", 18080, Some("s1"), "tui"),
            "ws://127.0.0.1:18080/ws?session_id=s1&client=tui"
        );
    }
}
