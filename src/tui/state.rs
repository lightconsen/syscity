//! Central application state for the TUI.
//!
//! The TUI keeps no message cache of its own: the transcript (scrollback plus
//! the live tail) *is* the record of the conversation, and switching sessions
//! re-reads history from the gateway. That removes the class of bug where the
//! local copy and the gateway disagree.
// INVARIANTS-NONE: presentation-layer state; owns no persistent data.

use std::collections::VecDeque;
use std::time::Instant;

use serde_json::Value;

use crate::tui::gateway_calls::{AgentInfo, ApprovalDetail, SessionInfo};
use crate::tui::transcript::Transcript;

/// Connection state of the TUI to the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// Not connected; a reconnect is either scheduled or in flight.
    #[default]
    Disconnected,
    /// Handshake in progress.
    Connecting,
    /// Connected and handshaken.
    Connected {
        /// Features advertised by the server.
        features: Vec<String>,
        /// Scopes granted to this connection.
        scopes_granted: Vec<String>,
        /// Server version string.
        server_version: String,
    },
    /// Connection lost; the payload is the last error, if any.
    Lost(String),
}

impl ConnectionState {
    /// Short, human-readable form for the status row.
    pub fn label(&self) -> String {
        match self {
            Self::Disconnected => "disconnected".to_string(),
            Self::Connecting => "connecting…".to_string(),
            Self::Connected { server_version, .. } => format!("v{server_version}"),
            Self::Lost(e) => format!("disconnected: {e}"),
        }
    }

    /// Whether the socket is usable right now.
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }
}

/// The three decisions an approval prompt offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalChoice {
    /// Run the tool once.
    #[default]
    Approve,
    /// Run it and remember a `[permissions].allow` rule for next time.
    ApproveRemember,
    /// Refuse the call.
    Deny,
}

/// Which prompt owns the input area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LiveMode {
    /// Normal typing.
    #[default]
    Composer,
    /// A tool approval is waiting; only the decision keys apply.
    Approval,
    /// The agent asked a question; options or a free-text answer apply.
    Ask,
}

/// What the in-flight turn is doing, shown as the status row's phase hint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RunPhase {
    /// Sent, waiting on the first token.
    #[default]
    Waiting,
    /// Reasoning deltas are arriving.
    Thinking,
    /// Answer deltas are arriving.
    Responding,
    /// A tool call is in flight.
    ToolCall(String),
}

/// One delegated task row, as learned from `delegation.updated` push events.
///
/// Kept in insertion order so the live rows don't jump around. The TUI renders
/// these while they run and retires them on a terminal status with a notice in
/// the transcript. Lost on reconnect/restart by design (v1): the events only
/// ever reach a live subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationRow {
    /// Task id (registry run id).
    pub task_id: String,
    /// Root of the delegation tree.
    pub root_id: String,
    /// Agent id responsible for the task.
    pub agent_id: String,
    /// Short human title (what the task is for).
    pub title: String,
    /// `running | waiting_handoff` while displayed here.
    pub status: String,
    /// Local start time: the first "running" sighting. Elapsed per row is
    /// computed from this, not from the payload's RFC3339 stamps.
    pub started: Option<Instant>,
    /// Total tokens the task's LLM rounds have reported so far.
    pub usage_tokens: u64,
}

impl DelegationRow {
    /// Parse a `delegation.updated` payload into a row. `None` when the
    /// payload lacks the fields every row needs.
    pub fn from_payload(payload: &Value) -> Option<Self> {
        Some(Self {
            task_id: payload["task_id"].as_str()?.to_string(),
            root_id: payload["root_id"].as_str().unwrap_or_default().to_string(),
            agent_id: payload["agent_id"].as_str().unwrap_or_default().to_string(),
            title: payload["title"].as_str().unwrap_or_default().to_string(),
            status: payload["status"].as_str()?.to_string(),
            started: None,
            usage_tokens: payload["usage_tokens"].as_u64().unwrap_or(0),
        })
    }

    /// The one-line transcript notice a terminal status graduates to.
    pub fn graduation_line(&self) -> String {
        let title = self.compact_title();
        let elapsed = self
            .started
            .map(|t| crate::tui::ui::live::format_elapsed(t.elapsed().as_secs()))
            .unwrap_or_else(|| "?".to_string());
        match self.status.as_str() {
            "completed" => {
                let tokens = if self.usage_tokens > 0 {
                    format!(
                        " · {} tokens",
                        crate::tui::ui::live::format_token_count(self.usage_tokens)
                    )
                } else {
                    String::new()
                };
                format!("✓ {agent}: {title} — {elapsed}{tokens}", agent = self.agent_id)
            }
            other => {
                format!("✘ {agent}: {title} — {other} after {elapsed}", agent = self.agent_id)
            }
        }
    }

    /// The one-line `/agents` listing entry for a row, whatever its status.
    pub fn listing_line(&self) -> String {
        let agent = &self.agent_id;
        let title = self.compact_title();
        let elapsed = self
            .started
            .map(|t| crate::tui::ui::live::format_elapsed(t.elapsed().as_secs()))
            .unwrap_or_else(|| "?".to_string());
        let tokens = if self.usage_tokens > 0 {
            format!(" · ↓{}", crate::tui::ui::live::format_token_count(self.usage_tokens))
        } else {
            String::new()
        };
        match self.status.as_str() {
            "completed" => format!("✓ {agent}: {title} — {elapsed}{tokens}"),
            "failed" => format!("✘ {agent}: {title} — failed after {elapsed}"),
            "waiting_handoff" => format!("⏸ {agent}: {title} · waiting for handoff"),
            _ => format!("⠋ {agent}: {title} · {elapsed}{tokens}"),
        }
    }

    /// The title, trimmed to a display length with an ellipsis when cut.
    fn compact_title(&self) -> String {
        let title = self.title.trim();
        if title.is_empty() {
            return "(no title)".to_string();
        }
        let mut out: String = title.chars().take(60).collect();
        if title.chars().count() > 60 {
            out.push('…');
        }
        out
    }

    /// Whether the row has reached a terminal status and must be retired.
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed")
    }
}

/// Whimsical verbs for the running status row. The word rotates slowly so the
/// row feels alive without flickering; the *phase* next to it carries the
/// real information.
const SPINNER_WORDS: &[&str] = &[
    "Puzzling",
    "Thinking",
    "Cooking",
    "Brewing",
    "Noodling",
    "Tinkering",
    "Mulling",
    "Scheming",
    "Churning",
    "Pondering",
];

/// How long one spinner word stays before the next rotates in.
const WORD_ROTATE_MS: u128 = 2_500;

/// Braille spinner frames — one per 50ms tick while a run is in flight, so a
/// full cycle is half a second.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The word for a given rotation offset and elapsed run time.
fn spinner_word_at(offset: usize, elapsed_ms: u128) -> &'static str {
    SPINNER_WORDS[(offset + (elapsed_ms / WORD_ROTATE_MS) as usize) % SPINNER_WORDS.len()]
}

/// What the connection dropped in the middle of.
///
/// Kept so a reconnect can say what actually *happened* to the work in flight,
/// rather than leaving "interrupted" as the last word on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interruption {
    /// Wall-clock milliseconds when the socket went away. Anything the gateway
    /// wrote after this is what the TUI never received.
    pub since_ms: i64,
    /// A turn was in flight at the time.
    pub run_in_flight: bool,
}

/// Milliseconds since the Unix epoch.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

/// A question from `ask_user` waiting for a human answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AskPrompt {
    /// Ask id, echoed back on the answer.
    pub ask_id: String,
    /// The question itself.
    pub question: String,
    /// Selectable answers, when the agent offered any.
    pub options: Vec<String>,
    /// Whether an answer is mandatory.
    pub required: bool,
    /// Answer the agent suggests.
    pub default: Option<String>,
}

/// Information about a gateway command, for `/help` and the completion hints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandInfo {
    /// Canonical key (e.g. "new").
    pub key: String,
    /// Display name.
    pub name: String,
    /// Short description.
    pub description: String,
    /// Usage pattern.
    pub usage: String,
    /// Category string.
    pub category: String,
    /// Tier.
    pub tier: String,
    /// Whether the command runs client-side.
    pub local: bool,
    /// Whether the command requires admin scope.
    pub requires_admin: bool,
}

impl CommandInfo {
    /// Match against a completion query (name prefix or description).
    pub fn matches(&self, query: &str) -> bool {
        let q = query.to_lowercase();
        self.name.to_lowercase().starts_with(&q) || self.description.to_lowercase().contains(&q)
    }
}

/// Central mutable application state.
#[derive(Debug)]
pub struct AppState {
    /// Connection state.
    pub connection: ConnectionState,
    /// Palette in use for the live region and scrollback, resolved at startup.
    pub active_theme: crate::tui::ui::Theme,
    /// What the OSC 11 query saw at startup, if the terminal answered. Kept so
    /// an `auto` setting can be re-resolved later without asking again.
    pub startup_bg: Option<crate::tui::ui::ThemeId>,
    /// Session the TUI is talking to.
    pub current_session: Option<String>,
    /// Agent bound to the current session, if any.
    pub current_agent: Option<String>,
    /// Known sessions (refreshed from the gateway).
    pub sessions: Vec<SessionInfo>,
    /// Known agents (refreshed from the gateway).
    pub agents: Vec<AgentInfo>,
    /// Scrollback + live tail.
    pub transcript: Transcript,
    /// Current input buffer.
    pub input_buffer: String,
    /// Cursor position in `input_buffer` (byte index).
    pub input_cursor: usize,
    /// Previously sent lines, oldest first.
    pub input_history: Vec<String>,
    /// Position while browsing history; `None` means "editing a fresh line".
    pub history_index: Option<usize>,
    /// The fresh line stashed when history browsing started, restored on the
    /// way back down.
    pub history_draft: String,
    /// Oldest message timestamp (ms) shown for the current session — the
    /// cursor `/history more` pages backwards from. `None` until a history
    /// load lands, and reset when the session changes.
    pub history_oldest_ms: Option<i64>,
    /// Which prompt owns the input area.
    pub live_mode: LiveMode,
    /// Approvals awaiting a decision, oldest first.
    pub approvals: VecDeque<ApprovalDetail>,
    /// This conversation's permission-mode override, as the gateway last
    /// reported it. `Default` means the gateway config decides.
    pub permission_mode: crate::tools::PermissionMode,
    /// Which decision is highlighted in the approval prompt.
    pub approval_selection: ApprovalChoice,
    /// An unanswered `ask_user` question.
    pub pending_ask: Option<AskPrompt>,
    /// Free-text answer being typed for the ask prompt.
    pub ask_input: String,
    /// Cached configuration payload + revision (for `/config`).
    pub config_cache: Option<Value>,
    /// Revision the cache was read at, for optimistic writes.
    pub config_revision: Option<String>,
    /// Command catalog (feeds `/help` and completion).
    pub command_list: Vec<CommandInfo>,
    /// Highlighted entry in the completion list.
    pub completion_index: usize,
    /// A response is streaming.
    pub is_running: bool,
    /// Messages submitted while a turn was running, oldest first. They go out
    /// when the current turn ends — one turn at a time, in order.
    pub queued: VecDeque<String>,
    /// The last finished assistant answer, verbatim — what `/copy` (Ctrl+Y)
    /// puts on the clipboard. `None` until a turn completes.
    pub last_assistant_text: Option<String>,
    /// The last plain-text prompt sent (a slash command is never recorded —
    /// only a message). What `/retry` resends. `None` until the first send.
    pub last_user_prompt: Option<String>,
    /// The most recent tool call, kept whole for `/expand`. Only the latest
    /// is held: a turn can call many tools, and every one after the first
    /// makes the previous expand pointless.
    pub last_tool_name: Option<String>,
    /// The last tool call's arguments, *untruncated*. `None` until a call.
    pub last_tool_args: Option<Value>,
    /// The last tool call's result, *untruncated*. `None` until a result.
    /// Cleared by the next `tool.calling`, which a result follows.
    pub last_tool_result: Option<Value>,
    /// When the current run started, for the elapsed counter.
    pub run_started: Option<Instant>,
    /// What the in-flight turn is doing right now.
    pub run_phase: RunPhase,
    /// Rotation offset for the spinner word, re-rolled on every run.
    pub word_offset: usize,
    /// Spinner frame counter.
    pub spinner: u8,
    /// Tokens accumulated from `agent.usage` events during the current run,
    /// shown next to the elapsed time. Reset when a run ends — an idle row
    /// never carries a token count.
    pub run_tokens: Option<u64>,
    /// Delegation task rows currently running (or handing off), oldest first.
    /// Fed by `delegation.updated` push events; retired on terminal status.
    pub delegation_tasks: Vec<DelegationRow>,
    /// Transient status text + when it was set (expires on its own).
    pub status: Option<(String, Instant)>,
    /// Set when a redraw is needed.
    pub dirty: bool,
    /// What the last lost connection interrupted, until a reconnect has
    /// reconciled it.
    pub interrupted: Option<Interruption>,
    /// Quit on the next loop iteration.
    pub should_quit: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            connection: ConnectionState::default(),
            active_theme: crate::tui::ui::Theme::dark(),
            startup_bg: None,
            current_session: None,
            current_agent: None,
            sessions: Vec::new(),
            agents: Vec::new(),
            transcript: Transcript::new(),
            input_buffer: String::new(),
            input_cursor: 0,
            input_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            history_oldest_ms: None,
            live_mode: LiveMode::default(),
            approvals: VecDeque::new(),
            permission_mode: crate::tools::PermissionMode::Default,
            approval_selection: ApprovalChoice::Approve,
            pending_ask: None,
            ask_input: String::new(),
            config_cache: None,
            config_revision: None,
            command_list: Vec::new(),
            completion_index: 0,
            is_running: false,
            queued: VecDeque::new(),
            last_assistant_text: None,
            last_user_prompt: None,
            last_tool_name: None,
            last_tool_args: None,
            last_tool_result: None,
            run_started: None,
            run_phase: RunPhase::default(),
            word_offset: 0,
            spinner: 0,
            run_tokens: None,
            delegation_tasks: Vec::new(),
            status: None,
            dirty: true,
            interrupted: None,
            should_quit: false,
        }
    }
}

impl AppState {
    /// Insert a character at the cursor.
    pub fn insert_char(&mut self, c: char) {
        self.input_buffer.insert(self.input_cursor, c);
        self.input_cursor += c.len_utf8();
    }

    /// Insert a newline at the cursor.
    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Insert pasted text at the cursor, one character at a time so the cursor
    /// lands after the whole run. A pasted `\n` is content — only the Enter
    /// key sends, never the inside of a paste.
    pub fn insert_paste(&mut self, text: &str) {
        for c in text.chars() {
            self.insert_char(c);
        }
    }

    /// Delete the character before the cursor.
    pub fn input_backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let mut prev = self.input_cursor - 1;
        while prev > 0 && !self.input_buffer.is_char_boundary(prev) {
            prev -= 1;
        }
        self.input_buffer.replace_range(prev..self.input_cursor, "");
        self.input_cursor = prev;
    }

    /// Move the cursor one character left.
    pub fn move_cursor_left(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let mut prev = self.input_cursor - 1;
        while prev > 0 && !self.input_buffer.is_char_boundary(prev) {
            prev -= 1;
        }
        self.input_cursor = prev;
    }

    /// Move the cursor one character right.
    pub fn move_cursor_right(&mut self) {
        if self.input_cursor >= self.input_buffer.len() {
            return;
        }
        let mut next = self.input_cursor + 1;
        while next < self.input_buffer.len() && !self.input_buffer.is_char_boundary(next) {
            next += 1;
        }
        self.input_cursor = next.min(self.input_buffer.len());
    }

    /// Byte offset of the start of the line the cursor is on.
    fn line_start(&self) -> usize {
        self.input_buffer[..self.input_cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    /// Byte offset just past the end of the line the cursor is on.
    fn line_end(&self) -> usize {
        self.input_buffer[self.input_cursor..]
            .find('\n')
            .map(|i| self.input_cursor + i)
            .unwrap_or(self.input_buffer.len())
    }

    /// Whether the cursor is on the first line of the input.
    pub fn cursor_on_first_line(&self) -> bool {
        self.line_start() == 0
    }

    /// Whether the cursor is on the last line of the input.
    pub fn cursor_on_last_line(&self) -> bool {
        self.line_end() == self.input_buffer.len()
    }

    /// Move the cursor up one visual line, falling back to history recall.
    ///
    /// Returns `true` when history was consulted instead of the cursor moved —
    /// the caller may need to redraw.
    pub fn cursor_up_or_history(&mut self) -> bool {
        if !self.cursor_on_first_line() {
            let col = self.input_cursor - self.line_start();
            let start = self.input_buffer[..self.line_start().saturating_sub(1)]
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0);
            let prev_line_end = self.line_start().saturating_sub(1);
            self.input_cursor = (start + col).min(prev_line_end);
            return false;
        }
        self.history_older();
        true
    }

    /// Move the cursor down one visual line, falling back to history recall.
    pub fn cursor_down_or_history(&mut self) -> bool {
        if !self.cursor_on_last_line() {
            let col = self.input_cursor - self.line_start();
            let next_start = self.line_end() + 1;
            let next_end = self.input_buffer[next_start..]
                .find('\n')
                .map(|i| next_start + i)
                .unwrap_or(self.input_buffer.len());
            self.input_cursor = (next_start + col).min(next_end);
            return false;
        }
        self.history_newer();
        true
    }

    /// Recall the previous submitted line.
    pub fn history_older(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let next = match self.history_index {
            None => {
                self.history_draft = self.input_buffer.clone();
                self.input_history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_index = Some(next);
        self.set_input(self.input_history[next].clone());
    }

    /// Come back down towards the line being edited.
    pub fn history_newer(&mut self) {
        match self.history_index {
            None => {}
            Some(i) if i + 1 < self.input_history.len() => {
                self.history_index = Some(i + 1);
                self.set_input(self.input_history[i + 1].clone());
            }
            Some(_) => {
                self.history_index = None;
                let draft = std::mem::take(&mut self.history_draft);
                self.set_input(draft);
            }
        }
    }

    /// Replace the input buffer and put the cursor at its end.
    ///
    /// Public because line mode has no composer to type into: it reads a whole
    /// line and has to place it in the buffer before submitting it.
    pub fn set_input(&mut self, text: String) {
        self.input_buffer = text;
        self.input_cursor = self.input_buffer.len();
    }

    /// Remember a submitted line (skipping a repeat of the previous one).
    pub fn remember_input(&mut self, line: &str) {
        if !line.trim().is_empty() && self.input_history.last().map(String::as_str) != Some(line) {
            self.input_history.push(line.to_string());
        }
        self.history_index = None;
        self.history_draft.clear();
    }

    /// Clear the input and the completion highlight.
    pub fn clear_input(&mut self) {
        self.input_buffer.clear();
        self.input_cursor = 0;
        self.completion_index = 0;
    }

    /// Candidates for completing the `/command` being typed.
    pub fn completions(&self) -> Vec<&CommandInfo> {
        let trimmed = self.input_buffer.trim_start();
        let Some(rest) = trimmed.strip_prefix('/') else {
            return Vec::new();
        };
        // Only the first word is being completed.
        if rest.contains(char::is_whitespace) {
            return Vec::new();
        }
        self.command_list
            .iter()
            .filter(|c| c.name.to_lowercase().starts_with(&rest.to_lowercase()))
            .collect()
    }

    /// Apply the highlighted completion to the input buffer.
    pub fn apply_completion(&mut self) {
        let candidates = self.completions();
        if candidates.is_empty() {
            return;
        }
        let idx = self.completion_index.min(candidates.len() - 1);
        let completed = format!("/{} ", candidates[idx].name);
        self.set_input(completed);
        self.completion_index = 0;
    }

    /// Move the completion highlight.
    pub fn move_completion(&mut self, forward: bool) {
        let len = self.completions().len();
        if len == 0 {
            return;
        }
        self.completion_index = if forward {
            (self.completion_index + 1) % len
        } else {
            (self.completion_index + len - 1) % len
        };
    }

    /// Set the transient status line.
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), Instant::now()));
    }

    /// Note that a run started.
    pub fn begin_run(&mut self) {
        self.is_running = true;
        self.run_started = Some(Instant::now());
        self.run_phase = RunPhase::Waiting;
        self.run_tokens = None;
        // Re-roll the word each run, without pulling in an RNG for it.
        self.word_offset = now_millis() as usize % SPINNER_WORDS.len();
    }

    /// Note that the current run ended.
    pub fn end_run(&mut self) {
        self.is_running = false;
        self.run_started = None;
        self.run_tokens = None;
    }

    /// The whimsical verb for the running status row, rotating slowly.
    pub fn spinner_word(&self) -> &'static str {
        let elapsed = self
            .run_started
            .map(|t| t.elapsed().as_millis())
            .unwrap_or(0);
        spinner_word_at(self.word_offset, elapsed)
    }

    /// The current spinner frame, advancing with every animation tick.
    pub fn spinner_frame(&self) -> &'static str {
        SPINNER_FRAMES[self.spinner as usize % SPINNER_FRAMES.len()]
    }

    /// Queue a message to send when the current turn ends.
    pub fn queue_message(&mut self, text: String) {
        self.queued.push_back(text);
    }

    /// Take the oldest queued message.
    pub fn pop_queued(&mut self) -> Option<String> {
        self.queued.pop_front()
    }

    /// Drop the queue, returning how many were waiting.
    pub fn clear_queue(&mut self) -> usize {
        let dropped = self.queued.len();
        self.queued.clear();
        dropped
    }

    /// Converge to "nothing is in flight" after the connection is lost.
    ///
    /// Neither a run nor a prompt can make progress without the gateway, and
    /// both lie if they stay set: the status row spins "running" forever, and
    /// a pending approval owns the keyboard, so the composer swallows every
    /// keystroke. Whatever is dropped is said out loud — silently discarding
    /// a prompt the user was looking at would be worse than the lock-up.
    pub fn connection_lost(&mut self, reason: impl Into<String>) {
        self.connection = ConnectionState::Lost(reason.into());
        let was_running = self.is_running;
        self.end_run();
        self.transcript.finish_open_streams();

        let dropped_approvals = self.approvals.len();
        self.approvals.clear();
        self.approval_selection = ApprovalChoice::Approve;
        let dropped_ask = self.pending_ask.take().is_some();
        self.ask_input.clear();
        self.live_mode = LiveMode::Composer;
        let dropped_queue = self.clear_queue();
        let dropped_tasks = self.delegation_tasks.len();
        self.delegation_tasks.clear();
        self.run_tokens = None;

        // Remember what we lost, so the reconnect can check what became of it
        // rather than leaving "interrupted" as the answer.
        self.interrupted = Some(Interruption {
            since_ms: now_millis(),
            run_in_flight: was_running,
        });

        if was_running {
            self.transcript.push_notice("── the run was interrupted ──");
        }
        if dropped_approvals > 0 {
            self.transcript.push_notice(format!(
                "⚠ {dropped_approvals} pending approval(s) dropped — the gateway times them out \
                 unless another client answers"
            ));
        }
        if dropped_ask {
            self.transcript.push_notice(
                "⚠ the pending question was dropped — ask the agent again once reconnected",
            );
        }
        if dropped_queue > 0 {
            self.transcript.push_notice(format!(
                "⚠ {dropped_queue} queued message(s) dropped — the gateway cannot take them yet"
            ));
        }
        if dropped_tasks > 0 {
            self.transcript.push_notice(format!(
                "⚠ {dropped_tasks} delegated task row(s) dropped — the gateway may have \
                 advanced them while offline"
            ));
        }
    }

    /// Seconds the current run has been going, if any.
    pub fn run_elapsed_secs(&self) -> Option<u64> {
        self.run_started.map(|t| t.elapsed().as_secs())
    }

    /// Advance time-based state. Returns `true` when something changed and the
    /// live region needs repainting.
    pub fn advance_animations(&mut self) -> bool {
        let mut changed = false;
        // Delegated tasks animate too: their elapsed counters tick even while
        // the parent turn is idle (the parent may have ended its reply while
        // children are still running, or a handoff is waiting).
        if self.is_running || !self.delegation_tasks.is_empty() {
            self.spinner = self.spinner.wrapping_add(1);
            changed = true;
        }
        if let Some((_, set_at)) = &self.status {
            if set_at.elapsed().as_secs() >= 6 {
                self.status = None;
                changed = true;
            }
        }
        changed
    }

    /// The approval at the front of the queue.
    pub fn current_approval(&self) -> Option<&ApprovalDetail> {
        self.approvals.front()
    }

    /// Retire the approval at the front of the queue and return to typing.
    pub fn pop_approval(&mut self) {
        self.approvals.pop_front();
        self.approval_selection = ApprovalChoice::Approve;
        if self.approvals.is_empty() {
            self.live_mode = if self.pending_ask.is_some() {
                LiveMode::Ask
            } else {
                LiveMode::Composer
            };
        }
    }

    /// The agent bound to the current session, if it is a known one.
    pub fn current_agent_info(&self) -> Option<&AgentInfo> {
        let id = self.current_agent.as_deref()?;
        self.agents.iter().find(|a| a.id == id)
    }

    /// Whether the granted scopes include `scope`.
    pub fn has_scope(&self, scope: &str) -> bool {
        matches!(
            &self.connection,
            ConnectionState::Connected { scopes_granted, .. }
                if scopes_granted.iter().any(|s| s == scope)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::transcript::LineKind;

    fn typing(state: &mut AppState, text: &str) {
        for c in text.chars() {
            state.insert_char(c);
        }
    }

    #[test]
    fn edits_multibyte_input_by_character() {
        let mut s = AppState::default();
        typing(&mut s, "中文ab");
        s.input_backspace();
        assert_eq!(s.input_buffer, "中文a");
        s.move_cursor_left();
        s.input_backspace();
        assert_eq!(s.input_buffer, "中a");
    }

    /// Pasting is insert-only: every pasted character (newlines included)
    /// lands in the buffer and the cursor ends after the run — a paste never
    /// sends anything, whatever it contains.
    #[test]
    fn paste_inserts_its_newlines_as_content() {
        let mut s = AppState::default();
        typing(&mut s, "ab");
        s.input_cursor = 1;
        s.insert_paste("X\nZ中文");
        assert_eq!(s.input_buffer, "aX\nZ中文b");
        // The paste landed at the cursor (between `a` and `b`), so the cursor
        // ends after the pasted run, with the trailing `b` still after it.
        assert_eq!(s.input_cursor, "aX\nZ中文".len());
    }

    #[test]
    fn history_recalls_on_the_first_line_and_restores_the_draft() {
        let mut s = AppState::default();
        s.remember_input("first");
        s.remember_input("second");
        typing(&mut s, "draft");

        assert!(s.cursor_up_or_history());
        assert_eq!(s.input_buffer, "second");
        assert!(s.cursor_up_or_history());
        assert_eq!(s.input_buffer, "first");
        // Already at the oldest entry: stays put.
        assert!(s.cursor_up_or_history());
        assert_eq!(s.input_buffer, "first");

        assert!(s.cursor_down_or_history());
        assert_eq!(s.input_buffer, "second");
        assert!(s.cursor_down_or_history());
        assert_eq!(s.input_buffer, "draft", "the typed draft comes back");
    }

    #[test]
    fn up_moves_within_a_multiline_buffer_before_touching_history() {
        let mut s = AppState::default();
        s.remember_input("older");
        typing(&mut s, "one\ntwo");
        // Cursor is at the end (line 2): up moves to line 1, not to history.
        assert!(!s.cursor_up_or_history());
        assert!(s.cursor_on_first_line());
        assert_eq!(s.input_buffer, "one\ntwo");
        // Now on the first line, up recalls history.
        assert!(s.cursor_up_or_history());
        assert_eq!(s.input_buffer, "older");
    }

    #[test]
    fn remembers_input_without_duplicating_consecutive_repeats() {
        let mut s = AppState::default();
        s.remember_input("same");
        s.remember_input("same");
        s.remember_input("   ");
        s.remember_input("different");
        assert_eq!(s.input_history, vec!["same", "different"]);
    }

    #[test]
    fn completes_a_slash_command_from_the_catalog() {
        let mut s = AppState::default();
        s.command_list = vec![
            CommandInfo {
                name: "status".into(),
                description: "Show status".into(),
                ..Default::default()
            },
            CommandInfo {
                name: "stop".into(),
                description: "Stop".into(),
                ..Default::default()
            },
            CommandInfo {
                name: "help".into(),
                description: "Help".into(),
                ..Default::default()
            },
        ];
        typing(&mut s, "/st");
        assert_eq!(s.completions().len(), 2);
        s.apply_completion();
        assert_eq!(s.input_buffer, "/status ");
        assert!(s.completions().is_empty(), "no completion once a space is typed");

        s.clear_input();
        typing(&mut s, "/st");
        s.move_completion(true);
        assert_eq!(s.completion_index, 1, "second candidate highlighted");
        s.apply_completion();
        assert_eq!(s.input_buffer, "/stop ");
    }

    #[test]
    fn approval_queue_returns_to_typing_only_when_empty() {
        let mut s = AppState::default();
        s.approvals.push_back(ApprovalDetail::default());
        s.approvals.push_back(ApprovalDetail::default());
        s.live_mode = LiveMode::Approval;
        s.pop_approval();
        assert_eq!(s.live_mode, LiveMode::Approval);
        s.pop_approval();
        assert_eq!(s.live_mode, LiveMode::Composer);
    }

    #[test]
    fn idle_state_does_not_repaint_forever() {
        let mut s = AppState::default();
        assert!(!s.advance_animations(), "nothing running, nothing set");
        s.begin_run();
        assert!(s.advance_animations(), "the spinner advances while running");
        s.end_run();
        s.set_status("hello");
        assert!(s.advance_animations() || s.status.is_some());
    }

    /// Delegated task rows animate on their own clock: the parent turn may
    /// have ended its reply while children still run, or a handoff may be
    /// waiting, and the row's elapsed counter must not freeze either way.
    #[test]
    fn task_rows_animate_even_when_the_parent_is_idle() {
        let mut s = AppState::default();
        assert!(!s.advance_animations(), "nothing at all: no repaint");
        s.delegation_tasks.push(DelegationRow {
            task_id: "run-1".to_string(),
            root_id: "r".to_string(),
            agent_id: "researcher".to_string(),
            title: "scan".to_string(),
            status: "running".to_string(),
            started: Some(Instant::now()),
            usage_tokens: 0,
        });
        assert!(s.advance_animations(), "the spinner advances while a task row is on the board");
    }

    /// The run token counter belongs to the run: it is empty when a run
    /// starts and gone when it ends, so an idle status row never carries a
    /// stale count.
    #[test]
    fn the_run_token_counter_lives_and_dies_with_the_run() {
        let mut s = AppState::default();
        s.begin_run();
        assert_eq!(s.run_tokens, None, "a fresh run starts with no count");
        s.run_tokens = Some(1_200);
        s.end_run();
        assert_eq!(s.run_tokens, None, "ending the run clears the count");
    }

    /// Losing the connection converges the run state, and says what it dropped.
    #[test]
    fn a_lost_connection_clears_the_run_and_the_prompts() {
        let mut s = AppState::default();
        s.begin_run();
        s.approvals.push_back(ApprovalDetail::default());
        s.pending_ask = Some(AskPrompt::default());
        s.live_mode = LiveMode::Approval;
        s.transcript
            .push_delta("assistant", LineKind::Assistant, "half an answer");

        s.connection_lost("gateway went away");

        assert!(!s.is_running, "no run survives the gateway");
        assert_eq!(s.run_started, None);
        assert!(s.approvals.is_empty(), "a prompt that owns the keyboard cannot be left set");
        assert!(s.pending_ask.is_none());
        assert_eq!(s.live_mode, LiveMode::Composer, "the composer takes the input back");
        assert!(matches!(s.connection, ConnectionState::Lost(_)));

        let flushed: Vec<String> = s
            .transcript
            .take_flushable()
            .into_iter()
            .map(|l| l.text)
            .collect();
        assert!(flushed.iter().any(|l| l.contains("half an answer")), "got {flushed:?}");
        assert!(flushed.iter().any(|l| l.contains("run was interrupted")), "got {flushed:?}");
        assert!(
            flushed.iter().any(|l| l.contains("approval")),
            "a dropped prompt is said out loud: {flushed:?}"
        );
        assert!(s.transcript.preview(10).is_empty(), "nothing is left live");
    }

    /// Task rows the TUI was holding go with the connection: the gateway may
    /// have advanced them while it was away, and a stale row lies.
    #[test]
    fn a_lost_connection_drops_the_task_rows_say_what_it_dropped() {
        let mut s = AppState::default();
        s.delegation_tasks.push(DelegationRow {
            task_id: "run-1".to_string(),
            root_id: "r".to_string(),
            agent_id: "researcher".to_string(),
            title: "scan".to_string(),
            status: "running".to_string(),
            started: Some(Instant::now()),
            usage_tokens: 0,
        });

        s.connection_lost("gateway gone");

        assert!(s.delegation_tasks.is_empty(), "the rows do not survive");
        let flushed: Vec<String> = s
            .transcript
            .take_flushable()
            .into_iter()
            .map(|l| l.text)
            .collect();
        assert!(
            flushed.iter().any(|l| l.contains("task row")),
            "the drop is said out loud: {flushed:?}"
        );
    }

    #[test]
    fn delegation_rows_parse_from_the_payload() {
        let row = DelegationRow::from_payload(&serde_json::json!({
            "session_id": "s1",
            "task_id": "run-1",
            "root_id": "root-1",
            "agent_id": "researcher",
            "title": "scan docs",
            "status": "running",
            "usage_tokens": 3_400,
        }))
        .expect("a row");
        assert_eq!(row.task_id, "run-1");
        assert_eq!(row.agent_id, "researcher");
        assert_eq!(row.usage_tokens, 3_400);
        assert!(!row.is_terminal());

        // A payload missing a required field parses to nothing.
        assert!(
            DelegationRow::from_payload(&serde_json::json!({ "task_id": "run-1" })).is_none(),
            "no status, no row"
        );
    }

    #[test]
    fn terminal_rows_graduate_to_a_notice() {
        let mut row = DelegationRow {
            task_id: "run-1".to_string(),
            root_id: "r".to_string(),
            agent_id: "researcher".to_string(),
            title: "scan docs".to_string(),
            status: "completed".to_string(),
            started: Some(Instant::now()),
            usage_tokens: 5_100,
        };
        assert!(row.is_terminal());
        let line = row.graduation_line();
        assert!(line.contains("✓ researcher: scan docs"), "{line}");
        assert!(line.contains("5.1k tokens"), "{line}");

        // A long title is cut to one line's worth.
        row.title = "x".repeat(200);
        assert!(row.graduation_line().chars().count() < 120);

        row.status = "failed".to_string();
        let line = row.graduation_line();
        assert!(line.contains("✘ researcher"), "the failure is named: {line}");
        assert!(line.contains("failed after"), "{line}");
    }

    #[test]
    fn listing_lines_read_the_row_status() {
        let base = |status: &str| DelegationRow {
            task_id: "run-1".to_string(),
            root_id: "r".to_string(),
            agent_id: "writer".to_string(),
            title: "draft email".to_string(),
            status: status.to_string(),
            started: Some(Instant::now()),
            usage_tokens: 0,
        };
        assert!(base("running")
            .listing_line()
            .starts_with("⠋ writer: draft email"));
        assert_eq!(
            base("waiting_handoff").listing_line(),
            "⏸ writer: draft email · waiting for handoff"
        );
        assert!(base("completed").listing_line().starts_with("✓ writer:"));
        assert!(base("failed").listing_line().starts_with("✘ writer:"));
    }

    /// The loss records what it interrupted, so a reconnect can check on it.
    #[test]
    fn a_lost_connection_records_what_it_interrupted() {
        let mut s = AppState::default();
        assert!(s.interrupted.is_none(), "nothing lost yet");

        s.begin_run();
        s.connection_lost("gone");

        let interrupted = s.interrupted.expect("recorded");
        assert!(interrupted.run_in_flight, "a turn was in flight");
        assert!(
            interrupted.since_ms > 1_600_000_000_000,
            "a wall clock reading, not a zero: {}",
            interrupted.since_ms
        );
    }

    /// With nothing running there is still a window, just no run to explain.
    #[test]
    fn a_loss_with_no_run_still_records_the_window() {
        let mut s = AppState::default();
        s.connection_lost("gone");

        let interrupted = s.interrupted.expect("recorded");
        assert!(!interrupted.run_in_flight);
    }

    /// Converging twice is harmless — a reconnect can drop again.
    #[test]
    fn a_second_loss_drops_nothing_and_says_nothing_new() {
        let mut s = AppState::default();
        s.connection_lost("gone");
        let first = s.transcript.take_flushable().len();
        s.connection_lost("gone again");
        assert_eq!(
            s.transcript.take_flushable().len(),
            0,
            "no run and no prompt means nothing to report (first loss queued {first} lines)"
        );
    }

    #[test]
    fn scope_check() {
        let mut s = AppState::default();
        assert!(!s.has_scope("write"));
        s.connection = ConnectionState::Connected {
            features: vec![],
            scopes_granted: vec!["chat".to_string(), "write".to_string()],
            server_version: "0.3.6".to_string(),
        };
        assert!(s.has_scope("write"));
    }

    #[test]
    fn the_spinner_word_holds_then_rotates() {
        let first = spinner_word_at(0, 0);
        assert_eq!(spinner_word_at(0, WORD_ROTATE_MS - 1), first, "no flicker mid-window");
        assert_ne!(spinner_word_at(0, WORD_ROTATE_MS), first, "rotates at the boundary");
        // The offset re-rolled per run changes where the rotation starts.
        assert_ne!(spinner_word_at(1, 0), first, "a different run, a different word");
    }

    #[test]
    fn begin_run_resets_the_phase_and_rolls_a_word() {
        let mut s = AppState {
            run_phase: RunPhase::ToolCall("shell".into()),
            ..AppState::default()
        };
        s.begin_run();
        assert_eq!(s.run_phase, RunPhase::Waiting);
        assert!(s.word_offset < SPINNER_WORDS.len());
        // A running turn always has a word to show.
        assert!(SPINNER_WORDS.contains(&s.spinner_word()));
    }
}
