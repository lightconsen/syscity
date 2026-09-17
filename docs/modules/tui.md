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
| `/history [n]` | Reprint this conversation, oldest first (default 200, max 2000) |
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
dismiss a prompt, else stop the running turn (and drop its queue) · `Ctrl+C`
abort, quit when idle, or dismiss an approval prompt · `Ctrl+H` help · `Ctrl+E`
config · `Ctrl+R` resume · `Ctrl+Q` quit.

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
run state converges with it: a disconnected run cannot finish and a disconnected
prompt cannot be answered, so both are cleared and said so, rather than left
claiming to be in progress.

The reconnect then asks what became of it. `chat.history` is read for the
current session, and any message the gateway wrote *after* the socket went
away is printed under a "while offline" rule — those are exactly the ones the
TUI never received, so nothing already in the transcript is reprinted. With no
run in flight there is nothing to explain and nothing is printed. The window
comes from `AppState::interrupted`, which the disconnect records; it is spent
once a reconnect has answered it. If nothing arrived while a run was in
flight, the TUI says that instead — which is the answer to "what happened to
it", and better than leaving "interrupted" as the last word.

The watermark is our wall clock against the gateway's `created_at`. Those
agree when the gateway is local, which is the ordinary case; the reconnect
backoff is half a second and up, far more than the skew between two machines
kept in sync, and a skewed clock can only widen or narrow the window — it
cannot reprint what the TUI already showed, because nothing it showed carries
a timestamp after the disconnect.

Subscription filtering: the gateway reads an *empty* subscription list as
"every session" (`ProtocolConnection::is_subscribed`), and a session created
from the TUI is unsubscribed until its id comes back — so there is a window in
which the connection legitimately receives other conversations' events. The
client therefore filters as well: an event whose payload names a session is
applied only if it names the current one. Events naming none are global on
purpose — cron notices, and `approval.required`, which the gateway scopes to a
tool call rather than to a conversation, so an approval raised anywhere is
offered here.

Commands run off the loop: `run` spawns one task per command and `select!`s
over it alongside input, gateway events and the animation tick. `WsClient`
takes `&self` throughout, so the loop keeps its connection while a command
holds it. One command at a time — the rest queue in order — because two
`/new`s racing would leave the state describing whichever finished last.
Startup is the first such task, so the frame is painted while its two requests
are still on the wire.

Gateway *events* are still handled inline. Their handlers can make requests
(`approvals.get`, `sessions.list`), which is what the loop is no longer
blocked by, but routing them through the queue would stall a streaming answer
behind whatever command is running — the worse trade of the two.

## Deliberate Limitations

- No markdown rendering: only fenced code blocks are styled; headings, tables
  and lists appear as their source text.
- Tool output is truncated (6–8 lines with an ellipsis), not collapsible.
- Resuming reprints the last 100 messages. The scrollback is append-only and
  top-anchored, so older messages cannot be spliced in above what is already
  printed — `/history [n]` reprints a longer window (up to 2000) in reading
  order below a rule instead.
- `/clear` resets the conversation context; it cannot unprint what the terminal
  has already scrolled past, and it says so.
- Frozen lines are wrapped at the width they were written with. Resizing the
  window afterwards reflows them the way any other scrollback text reflows.
- Non-tty stdout (a pipe or a file) degrades to line mode: no raw mode, no
  cursor addressing. Lines are read from stdin (a blank one is ignored, a `/`
  one is a command) and printed as the transcript graduates them, so a
  streamed answer arrives a line at a time rather than a token at a time —
  a partial line waits for its newline. Lines are submitted one turn at a
  time, in order, however fast the pipe delivers them: a message arriving
  mid-turn queues behind it rather than opening a second turn on the session.
- One turn at a time is the rule everywhere: a message typed mid-response
  queues and goes out when the turn ends. Esc stops the turn *and* drops the
  queue, and a lost connection drops it too, saying so.
- Prompts in line mode depend on what stdin is. With a terminal on it
  (`syscity tui > out.txt`) a typed line answers: `y`/`n` for an approval, the
  text — or the number of an option — for a question. With a pipe
  (`echo … | syscity tui`) nobody can answer, so a prompt is settled as it
  arrives: an approval is denied, a question is answered with the agent's own
  default, and a question with no default stops the turn. Nothing parks on the
  gateway's five-minute timeout with the pipe behind it.

## Testing

The rendering path is testable without a terminal: `TestBackend` has a real
scrollback buffer and an inline viewport of its own. `tests/tui_inline.rs`
drives flush-then-draw and asserts what lands where; `src/tui/scrollback.rs`
covers the writer's contract (wrapping, styles, scrolling off the top);
`transcript.rs`, `wrap.rs`, `state.rs`, `actions.rs`, `retry.rs` and
`gateway_calls.rs` are pure unit tests.
