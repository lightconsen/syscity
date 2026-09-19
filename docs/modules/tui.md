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
│  ⠹ Cooking… (4s · responding) — esc stops · 🦊 Secretary · sess anon  │
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

  The status row above the composer doubles as the run indicator while a turn
  is in flight:

      ⠹ Scheming… (1m 23s · thinking) — esc stops  ·  secretary  ·  sess …

  The braille frame animates on the 50ms tick (a full cycle is half a second),
  and it and the word share one color — cyan and bold — while the facts beside
  them (time, phase, agent, session) stay in the plain status color. The word
  rotates every 2.5s (`state.rs`'s `SPINNER_WORDS`, re-rolled per run), and the
  parenthetical carries the *real* information: elapsed time and the phase,
  which is `thinking`, `responding`, or the name of the tool in flight
  (`RunPhase`, set from the event stream). A tool's name comes back down when
  its result lands — the wait that follows is not labelled with a call that
  already finished. Everything here is present only while a run is in flight;
  the idle row is the connection, agent and session as before.

  The block area also shows the slash-command candidates while a `/command`
  is being typed (windowed around the Tab selection), taking precedence over
  the stream preview — the typist's attention is on the command.

  The row carries the run and nothing else from it: when the turn ends the
  spinner, the word, the elapsed time and the phase all go, leaving the
  connection, agent and session as before. A token meter was tried here and
  removed — usage only rides on `chat.final` (no delta carries it), so it could
  only ever describe a turn that had already finished, and it read as a
  leftover of one.

  While running, the row drops the server version to stay inside 80 columns —
  the tail it would otherwise push off is the session id. A *disconnected*
  connection still speaks up mid-run; that is not noise.
- **`ui/blocks.rs`** — transcript lines and gateway history → styled lines.
- **`gateway_calls.rs`** — every WS call the TUI makes, once, with the payload
  shapes the gateway actually serves, plus parser tests written against real
  payload literals.
- **`commands.rs`** — slash commands; anything that used to open a popup now
  prints into the scrollback.
- **`resume.rs`** — `--continue` / `--resume` / `/resume`.
- **`retry.rs`** — reconnect backoff (500ms → 8s cap).
- **`input.rs`** — input polling behind the `InputSource` seam. Events are
  polled by the event loop rather than read by a background task: an inline
  viewport asks the terminal for its cursor position on every draw, and that
  reply arrives on the same stdin a concurrent reader would be draining.
  Polling from the loop means nothing else reads stdin while a frame is drawn.
  Production is `CrosstermInput`; tests script actions through the same loop —
  resize, key timing against a hung request, the approval keys — without a pty.

### Slash commands

| Command | Description |
|---------|-------------|
| `/new [agent]` | Start a new session, optionally bound to an agent |
| `/resume [n\|id]` | List sessions, or switch to one |
| `/sessions` | List sessions |
| `/history [n\|more]` | Reprint this conversation, oldest first (default 200, max 2000); `more` pages backwards |
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

Any *other* control chord does nothing. It used to type its bare letter —
`Ctrl+U` put a `u` in the draft — because the key map's catch-all arm saw a
`Char` and never looked at the modifier. `Ctrl+Alt` is deliberately exempt:
that is AltGr on several keyboard layouts, where it types real characters
(`@`, `\`, `|`), and swallowing those would be the worse bug.

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
purpose — cron notices, and approvals raised outside any conversation.

Approvals name the conversation that raised them: `PendingApproval` carries
the tool call's `conversation_id`, the gateway routes `approval.required` to
that session's subscribers (`audience_of`), and a client in the fail-open
window that sees one for *another* session prints a notice — someone's turn
is blocked — instead of prompting or dropping it silently. An approval with
no conversation behind it still broadcasts, which is also the behavior every
client saw before the field existed.

What this does **not** do is authorization: any connected client with the
`write` scope can still call `approvals.approve` on any pending id, including
another session's. That is deliberate for now — the deployment premise is a
single operator, every client is that operator, and scoping the *prompt* is
about putting it where the blocked turn is, not about keeping it from anyone.
Owner-checked approval decisions become worth their complexity when a second
identity (a paired device, a room member) can connect; until then the audit
log records who answered what.

Actions take one of two lanes. **Edits** (typing, cursor, completion, resize)
run inline even while a command is in flight — "input still editable while the
gateway is slow" is the whole point of the loop being non-blocking, and a
keystroke queued behind an RPC would fail it; with a prompt up the same keys
are its decision, also inline. **Commands** (`Enter`, a slash command, abort,
Esc) run as tasks off the loop, one at a time — the rest queue in order —
because two `/new`s racing would leave the state describing whichever finished
last. `run` `select!`s over the in-flight command alongside gateway events and
the animation tick; `WsClient` takes `&self` throughout, so the loop keeps its
connection while a command holds it. Startup is the first such task, so the
frame is painted while its two requests are still on the wire. Quit jumps both
lanes; offline, every action takes the path that existed before.

Gateway *events* are still handled inline. Their handlers can make requests
(`approvals.get`, `sessions.list`), which is what the loop is no longer
blocked by, but routing them through the queue would stall a streaming answer
behind whatever command is running — the worse trade of the two.

## Deliberate Limitations

- No markdown rendering: only fenced code blocks are styled; headings, tables
  and lists appear as their source text.
- Untested surface: real terminal resize events, exotic emulators, and the
  slow-terminal cases still sit below the seams. `tests/tui_pty.rs` runs the
  real binary under a real pty and pins the rest — the cursor-position query
  going out and being answered, the tty raw while running and cooked after
  `/quit` or `SIGTERM`, CJK text hitting the wire without padding — but it is
  a handful of scenarios on one platform (macOS/Linux pty), not a
  terminal-emulator matrix.
- Scrollback insertion uses DECSTBM scrolling regions (ratatui's
  `scrolling-regions` feature). Without it, ratatui's fallback draws every
  buffer cell — including the empty continuation cell after a wide character,
  which surfaces as a blank column after every CJK character. Any terminal
  with xterm-style scroll regions (iTerm2, Terminal.app, kitty, alacritty,
  Windows Terminal) renders this correctly.
- Tool output is truncated (6–8 lines with an ellipsis), not collapsible.
- Resuming reprints the last 100 messages. The scrollback is append-only and
  top-anchored, so older messages cannot be spliced in above what is already
  printed — history is paged, never spliced. `/history [n]` reprints a window
  (up to 2000) in reading order under a rule, and `/history more` asks the
  gateway for the page strictly older than the oldest message shown so far
  (its `before` cursor, a timestamp). The cursor lives in
  `AppState::history_oldest_ms`, is reset when the session changes, and a
  `more` with nothing loaded says so rather than guessing.
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

`src/tui/event_loop.rs` also holds loop-level tests: the real `run` and the
real `run_plain`, against `src/tui/test_gateway.rs` (an in-process stand-in
that speaks the protocol — handshake, scripted replies, events pushed at the
client, requests recorded for assertion) and actions injected through the
`InputSource` seam. They cover the pipe contract, print-once streaming,
disconnect convergence, the approval decision round-trip, resize repaint, and
typing while a request is on the wire. The PTY suite (`tests/tui_pty.rs`)
covers what those seams cannot: the cursor query, raw-mode entry and restore
on `/quit`, and a clean exit with a cooked terminal on `SIGTERM`.
