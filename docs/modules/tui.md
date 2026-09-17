# TUI Module

Interactive terminal UI client for Syscity (`syscity tui`). Always compiled —
there is no `tui` Cargo feature.

## Design

The client runs **inline**, the way a shell session does rather than the way a
full-screen application does. It never enters the alternate screen and never
captures the mouse, so:

- the conversation lands in the terminal's own **scrollback** — native scrolling,
  native text selection, and the transcript is still there after exit;
- only a small **live region** at the bottom (composer, status row, blocking
  prompts) is ours to redraw.

```
┌─ the terminal's scrollback: written once, never redrawn ─────────────┐
│  ── connected (v0.3.6) · Ctrl+H for help ──                          │
│  > what is in src/tui?                                               │
│  Let me look.                                                        │
│  thinking:                                                            │
│    the ask is about the tui module…                                  │
│  ⚙ file_read                                                          │
│    {"path":"src/tui/app.rs"}                                          │
│    ↳ file_read: //! Terminal setup, panic recovery…                   │
├─ live region: Viewport::Inline(8) ───────────────────────────────────┤
│  (approval / question prompt, or the tail of a streaming answer)      │
│  v0.3.6 · running 4s — esc to stop · 🦊 Secretary · sess anonymous    │
│  > ▊                                                                  │
└──────────────────────────────────────────────────────────────────────┘
```

One rule keeps both correct: **a line is emitted exactly once, either into the
live region or into scrollback, never both.** Scrollback is append-only from our
side, which is what makes the terminal's own scroll and selection work.

### Modules

- **`transcript.rs`** — the freeze policy, with no ratatui types in it.
  `StreamBuffer` accumulates `chat.delta` text and releases whole lines as soon
  as they are newline-terminated (that is what makes output *stream* into the
  scrollback rather than landing in one dump). Two things hold a line back: the
  trailing partial line, and a fenced code block, which is held until its
  closing fence so the block is styled as one unit (`HOLD_CAP` bounds that
  hold). `chat.final` reconciles the authoritative text against what was
  already printed — including the non-streaming provider case, where no deltas
  arrive at all.
- **`scrollback.rs`** — the only caller of `Terminal::insert_before`, because of
  two things that API does not do: it does **not** wrap (an over-wide line is
  silently truncated, so everything is wrapped first) and it **clears the
  viewport** (so a flush must be followed by a draw in the same iteration).
- **`ui/wrap.rs`** — display-width wrapping (`unicode-width`), so CJK and emoji
  wrap where they visually should. Styles survive: spans are split at the wrap
  points.
- **`ui/live.rs`** — the live region: a constant-height strip (`LIVE_HEIGHT`; an
  inline viewport's height cannot change after construction, so overflow is the
  transcript's problem, not the layout's). Bottom-up row allocation keeps it
  sane at any terminal size.
- **`ui/blocks.rs`** — transcript lines and gateway history → styled lines.
- **`gateway_calls.rs`** — every WS call the TUI makes, once, with the payload
  shapes the gateway actually serves, plus parser tests written against real
  payload literals.
- **`commands.rs`** — slash commands; anything that used to open a popup now
  prints into the scrollback.
- **`resume.rs`** — `--continue` / `--resume` / `/resume`.
- **`retry.rs`** — reconnect backoff (500ms → 8s cap).
- **`input.rs`** — input polling. Events are polled by the event loop rather
  than read by a background task: an inline viewport asks the terminal for its
  cursor position on every draw, and that reply arrives on the same stdin a
  concurrent reader would be draining. Polling from the loop means nothing else
  reads stdin while a frame is drawn.

### Slash commands

| Command | Description |
|---------|-------------|
| `/new [agent]` | Start a new session, optionally bound to an agent |
| `/resume [n\|id]` | List sessions, or switch to one |
| `/sessions` | List sessions |
| `/rename <name>` | Rename the current session |
| `/pin` | Pin or unpin the current session |
| `/agents` | List agents |
| `/agent <id>` | Start a session bound to an agent |
| `/clear` | Clear the conversation context (`sessions.reset`) |
| `/config [set <path> <value>]` | Show or change configuration |
| `/status` | Gateway status |
| `/tools` | List gateway commands |
| `/model <id>` | Set the default model |
| `/answer <text>` | Answer a pending question |
| `/help` · `/quit` | Help · leave |

Commands the TUI does not implement are forwarded to the gateway
(`commands.execute`).

### Keys

`Enter` send · `Shift+Enter` newline · `Up`/`Down` input history (or move within
a multiline input) · `Tab`/`Shift+Tab` cycle `/command` completions · `Esc`
dismiss a prompt, else stop the running turn · `Ctrl+C` abort, or quit when idle
· `Ctrl+H` help · `Ctrl+E` config · `Ctrl+R` resume · `Ctrl+Q` quit.

While an approval or question prompt is up, typing goes to the prompt rather
than the composer.

## Key Types

```rust
pub async fn run(host: &str, port: u16, token: Option<&str>, session: SessionChoice) -> Result<()>

pub enum SessionChoice { New, Continue, Resume(String) }

pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected { features: Vec<String>, scopes_granted: Vec<String>, server_version: String },
    Lost(String),
}

pub enum LiveMode { Composer, Approval, Ask }

pub struct Transcript { /* ready lines + in-flight streams */ }
```

## Data Flow

```
keyboard ──poll──▶ TuiAction ──▶ AppState ──▶ transcript
                                    │              │
gateway ◀── WS ──▶ chat.delta/final │              │ take_flushable()
                                    ▼              ▼
                                 live region   scrollback (insert_before)
```

Reconnecting: a dropped socket surfaces as `WsMessage::Disconnected`, which the
loop turns into a visible "lost the gateway" notice and a backoff retry. The
session is re-subscribed, and history is **not** reprinted — the transcript is
already above. Output produced while offline is lost, which the notice says.
The run state converges with it: a disconnected run cannot finish and a
disconnected prompt cannot be answered, so both are cleared and said so, rather
than left claiming to be in progress.

Subscription filtering: the gateway reads an *empty* subscription list as
"every session" (`ProtocolConnection::is_subscribed`), and a session created
from the TUI is unsubscribed until its id comes back — so there is a window in
which the connection legitimately receives other conversations' events. The
client therefore filters as well: an event whose payload names a session is
applied only if it names the current one. Events naming none are global on
purpose — cron notices, and `approval.required`, which the gateway scopes to a
tool call rather than to a conversation, so an approval raised anywhere is
offered here.

## Deliberate Limitations

- No markdown rendering: only fenced code blocks are styled; headings, tables
  and lists appear as their source text.
- Tool output is truncated (6–8 lines with an ellipsis), not collapsible.
- `/clear` resets the conversation context; it cannot unprint what the terminal
  has already scrolled past, and it says so.
- Frozen lines are wrapped at the width they were written with. Resizing the
  window afterwards reflows them the way any other scrollback text reflows.
- Non-tty stdout (a pipe or a file) degrades to line mode: no raw mode, no
  cursor addressing. Lines are read from stdin (a blank one is ignored, a `/`
  one is a command) and printed as the transcript graduates them, so a
  streamed answer arrives a line at a time rather than a token at a time —
  a partial line waits for its newline.

## Testing

The rendering path is testable without a terminal: `TestBackend` has a real
scrollback buffer and an inline viewport of its own. `tests/tui_inline.rs`
drives flush-then-draw and asserts what lands where; `src/tui/scrollback.rs`
covers the writer's contract (wrapping, styles, scrolling off the top);
`transcript.rs`, `wrap.rs`, `state.rs`, `actions.rs`, `retry.rs` and
`gateway_calls.rs` are pure unit tests.
