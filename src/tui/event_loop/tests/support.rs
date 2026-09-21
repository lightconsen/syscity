//! Fixtures the event loop's tests share.

use std::io::{self, Cursor, Write};
use std::sync::Arc;
use std::time::Duration;

use ratatui::Terminal;
use serde_json::Value;
use tokio::sync::{mpsc, RwLock};

use crate::tui::actions::TuiAction;
use crate::tui::app::Endpoint;
use crate::tui::event_loop::plain::PlainIo;
use crate::tui::gateway_calls::ApprovalDetail;
use crate::tui::input::InputSource;
use crate::tui::state::{AppState, LiveMode};
use crate::tui::test_gateway::TestGateway;
use crate::tui::ui::live;
use crate::tui::ws_client::{ClientEvent, WsClient};

/// How long a test waits for a frame or an effect before calling it lost.
pub(super) const PATIENCE: Duration = Duration::from_secs(5);

/// An endpoint pointing at a test gateway.
pub(super) fn test_endpoint(port: u16) -> Endpoint {
    let auth = crate::tui::auth::AuthConfig::None;
    let url = auth.ws_url("127.0.0.1", port, None, "tui");
    Endpoint { url, auth, session: None }
}

/// Poll `check` until it holds, or fail.
pub(super) async fn eventually(check: impl Fn() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while tokio::time::Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The async form, for conditions that live behind a lock.
pub(super) async fn eventually_async<F, Fut>(mut check: F, what: &str)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while tokio::time::Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// An input source that never has anything — production without a tty
/// behaves the same, and the loop must cope.
pub(super) struct SilentInput;

impl InputSource for SilentInput {
    fn poll(&mut self) -> Option<TuiAction> {
        None
    }
}

/// Actions a test queues for the loop, one `poll` per action.
pub(super) struct ScriptedInput {
    rx: mpsc::UnboundedReceiver<TuiAction>,
}

impl ScriptedInput {
    /// The source and its half of the channel.
    pub(super) fn new() -> (Self, mpsc::UnboundedSender<TuiAction>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { rx }, tx)
    }
}

impl InputSource for ScriptedInput {
    fn poll(&mut self) -> Option<TuiAction> {
        // try_recv, not recv: like crossterm, `None` means "nothing right
        // now", and the loop keeps its own beat.
        self.rx.try_recv().ok()
    }
}

/// A terminal with an inline viewport and a real scrollback, as
/// `tests/tui_inline.rs` builds one.
pub(super) fn inline_terminal() -> Terminal<ratatui::backend::TestBackend> {
    use ratatui::TerminalOptions;
    use ratatui::Viewport;

    let mut terminal = Terminal::with_options(
        ratatui::backend::TestBackend::new(60, 12),
        TerminalOptions {
            viewport: Viewport::Inline(live::LIVE_HEIGHT),
        },
    )
    .expect("inline terminal");
    terminal
        .set_cursor_position(ratatui::layout::Position::new(0, 0))
        .expect("cursor home");
    terminal
}

/// A `Write` sink the test can read back.
#[derive(Clone, Default)]
pub(super) struct SharedOutput(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl SharedOutput {
    pub(super) fn text(&self) -> String {
        let buf = self.0.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&buf).into_owned()
    }
}

impl Write for SharedOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Line mode reading `input`, with the output captured.
///
/// `interactive` is false: this is a pipe, which is what line mode is for.
pub(super) fn plain_io(input: &str) -> (PlainIo, SharedOutput) {
    let out = SharedOutput::default();
    (
        PlainIo {
            input: Box::new(Cursor::new(input.as_bytes().to_vec())),
            output: Box::new(out.clone()),
            interactive: false,
        },
        out,
    )
}

/// The same, but with a terminal on stdin — `syscity tui > out.txt`.
pub(super) fn plain_io_interactive(input: &str) -> (PlainIo, SharedOutput) {
    let out = SharedOutput::default();
    (
        PlainIo {
            input: Box::new(Cursor::new(input.as_bytes().to_vec())),
            output: Box::new(out.clone()),
            interactive: true,
        },
        out,
    )
}

/// A fresh state and a connected client.
pub(super) async fn state_and_client(gateway: &TestGateway) -> (Arc<RwLock<AppState>>, WsClient) {
    let auth = crate::tui::auth::AuthConfig::None;
    let url = auth.ws_url("127.0.0.1", gateway.port, None, "tui");
    let (client, _hello) = WsClient::connect(&url, &auth, &["chat"])
        .await
        .expect("connect");
    let state = Arc::new(RwLock::new(AppState::default()));
    (state, client)
}

/// The same, with a pending approval waiting for a decision.
pub(super) async fn state_with_approval(
    gateway: &TestGateway,
) -> (Arc<RwLock<AppState>>, WsClient) {
    let (state, client) = state_and_client(gateway).await;
    {
        let mut s = state.write().await;
        s.approvals.push_back(ApprovalDetail {
            id: "ap1".to_string(),
            tool_name: "file_write".to_string(),
            risk_level: "High".to_string(),
            ..Default::default()
        });
        s.live_mode = LiveMode::Approval;
    }
    (state, client)
}

/// Everything currently painted or scrolled off, as one string.
pub(super) fn painted(terminal: &Terminal<ratatui::backend::TestBackend>) -> String {
    let mut text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    text.push_str(
        &terminal
            .backend()
            .scrollback()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>(),
    );
    text
}

/// An `approval.required` event with the arguments the prompt needs.
pub(super) fn approval_event() -> ClientEvent {
    event(
        "approval.required",
        serde_json::json!({
            "approval_id": "ap1",
            "tool_name": "file_write",
            "requested_by": "secretary",
            "risk_level": "High",
            "message": "writes outside the workspace",
        }),
    )
}

/// An `ask.required` event, optionally with a default.
pub(super) fn ask_event(default: Option<&str>) -> ClientEvent {
    let mut payload = serde_json::json!({
        "ask_id": "ask1",
        "question": "which branch?",
        "options": ["main", "dev"],
        "required": true,
    });
    if let Some(default) = default {
        payload["default"] = serde_json::json!(default);
    }
    event("ask.required", payload)
}

/// A server event, as it arrives off the wire.
pub(super) fn event(name: &str, payload: Value) -> ClientEvent {
    serde_json::from_value(serde_json::json!({
        "type": "event",
        "event": name,
        "payload": payload,
        "seq": 1,
    }))
    .expect("a client event")
}
